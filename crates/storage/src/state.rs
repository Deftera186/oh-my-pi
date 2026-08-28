//! Core-owned durable state for project, user, and organization authorities.
//!
//! [`StateScope::Session`] is routing vocabulary only: [`StateStore`] rejects
//! it so the canonical session journal remains the sole truth and sole source
//! of entry ids. The private authority-store layout is one append-only log; all
//! in-memory indexes are replay-derived and can never become a second source of
//! truth.

use std::{
	cmp,
	collections::{HashMap, HashSet},
	fmt::{self, Display},
	fs::{self, File, OpenOptions},
	io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
	mem::size_of,
	path::{Path, PathBuf},
	sync::{Arc, Weak},
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use flume::{Receiver, TryRecvError};
use omp_core::{CowBytes, Hash32, IntoStr, Principal, Provenance, Str, base64, sf};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use strum::{AsRefStr, Display, EnumString};
use thiserror::Error as ThisError;

use crate::blob::{BlobRef, BlobStore, Error as BlobError};

const CODEC: &str = "omp.state/1";
const LOG_FILE: &str = "state-v1.jsonl";
const LOCK_FILE: &str = "state-v1.lock";
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// A durable state's authority and retention boundary.
#[derive(
	Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString, AsRefStr,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum StateScope {
	/// State belonging to one session, delegated to the canonical session
	/// journal.
	Session,
	/// State shared by every session rooted at one normalized project.
	Project,
	/// State belonging to one authenticated principal on this daemon.
	User,
	/// State distributed by an organization authority.
	Organization,
}

/// The monotonically increasing revision of one scope instance.
#[derive(
	Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct StateRevision(u64);

impl StateRevision {
	/// Creates a revision from its persisted integer value.
	pub const fn new(value: u64) -> Self {
		Self(value)
	}

	/// Returns the persisted integer value.
	pub const fn get(self) -> u64 {
		self.0
	}
}

impl Display for StateRevision {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(formatter)
	}
}

/// The core-resolved identity of one scope instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopeKey {
	scope:     StateScope,
	authority: Str,
}

impl ScopeKey {
	/// Returns the scope class.
	pub const fn scope(&self) -> StateScope {
		self.scope
	}

	/// Returns the core-resolved authority identifier.
	pub fn authority(&self) -> &str {
		self.authority.as_str()
	}
}

/// An opaque, totally ordered entry identifier within one scope instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateEntryId {
	scope:    ScopeKey,
	revision: StateRevision,
}

impl StateEntryId {
	/// Returns the scope instance containing the entry.
	pub const fn scope(&self) -> &ScopeKey {
		&self.scope
	}

	/// Returns the entry's monotonic revision.
	pub const fn revision(&self) -> StateRevision {
		self.revision
	}
}

impl PartialOrd for StateEntryId {
	fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
		(self.scope == other.scope).then(|| self.revision.cmp(&other.revision))
	}
}

impl Display for StateEntryId {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{}:{}:{}", self.scope.scope, self.scope.authority, self.revision)
	}
}

/// The generation pair stamped on every durable request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GenerationFence {
	/// The authenticated extension host incarnation.
	pub host:    u64,
	/// The session epoch into which that host was spawned.
	pub session: u64,
}

/// Correlation and replay fields supplied with one durable request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableRequest {
	request_id:      Str,
	idempotency_key: Option<Str>,
	generation:      GenerationFence,
}

impl DurableRequest {
	/// Creates a request stamp. The request id is unique per attempt; an
	/// idempotency key is stable across attempts of one logical operation.
	///
	/// # Errors
	///
	/// Returns [`Error::InvalidKey`] for an empty or unsafe identifier.
	pub fn new(
		request_id: impl Into<Str>,
		idempotency_key: Option<Str>,
		generation: GenerationFence,
	) -> Result<Self, Error> {
		let request_id = request_id.into();
		validate_key("request id", request_id.as_str())?;
		if let Some(key) = &idempotency_key {
			validate_key("idempotency key", key.as_str())?;
		}
		Ok(Self { request_id, idempotency_key, generation })
	}

	/// Returns the correlation identifier for this attempt.
	pub fn request_id(&self) -> &str {
		self.request_id.as_str()
	}

	/// Returns the stable retry key, when supplied.
	pub fn idempotency_key(&self) -> Option<&str> {
		self.idempotency_key.as_deref()
	}

	/// Returns the generations carried by the request.
	pub const fn generation(&self) -> GenerationFence {
		self.generation
	}
}

/// Organization membership conferred by the authority layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationAccess {
	id:        Str,
	can_write: bool,
}

impl OrganizationAccess {
	/// Creates an organization membership grant.
	///
	/// # Errors
	///
	/// Returns [`Error::InvalidKey`] when `id` is empty or unsafe.
	pub fn new(id: impl Into<Str>, can_write: bool) -> Result<Self, Error> {
		let id = id.into();
		validate_key("organization id", id.as_str())?;
		Ok(Self { id, can_write })
	}
}

/// Core-authenticated authority used to resolve scope, namespace, authorship,
/// and current generation. Worker-authored frames never contain this value.
#[derive(Debug, Clone)]
pub struct StateAuthority {
	principal:           Principal,
	provenance:          Provenance,
	namespace:           Str,
	session:             Str,
	project:             Str,
	organization:        Option<OrganizationAccess>,
	readable_namespaces: SmallVec<Str, 4>,
	generation:          GenerationFence,
}

impl StateAuthority {
	/// Creates authority from facts authenticated and stamped by the core.
	///
	/// # Errors
	///
	/// Returns [`Error::InvalidAuthority`] when provenance and generation
	/// disagree, or [`Error::InvalidKey`] for an empty or unsafe identifier.
	pub fn new_core(
		principal: Principal,
		provenance: Provenance,
		namespace: impl Into<Str>,
		session: impl Into<Str>,
		project: impl Into<Str>,
		generation: GenerationFence,
	) -> Result<Self, Error> {
		let namespace = namespace.into();
		let session = session.into();
		let project = project.into();
		validate_namespace(namespace.as_str())?;
		validate_key("principal id", principal.id())?;
		validate_key("session id", session.as_str())?;
		validate_key("project id", project.as_str())?;
		if provenance.generation() != generation.host
			|| provenance.extension_id() != namespace.as_str()
		{
			return Err(Error::InvalidAuthority);
		}
		Ok(Self {
			principal,
			provenance,
			namespace,
			session,
			project,
			organization: None,
			readable_namespaces: SmallVec::new(),
			generation,
		})
	}

	/// Attaches authority-owned organization membership.
	pub fn with_organization(mut self, access: OrganizationAccess) -> Self {
		self.organization = Some(access);
		self
	}

	/// Grants read access to one foreign namespace.
	///
	/// # Errors
	///
	/// Returns [`Error::InvalidKey`] if the namespace is not canonical.
	pub fn grant_read_namespace(&mut self, namespace: impl Into<Str>) -> Result<(), Error> {
		let namespace = namespace.into();
		validate_namespace(namespace.as_str())?;
		if namespace != self.namespace && !self.readable_namespaces.contains(&namespace) {
			self.readable_namespaces.push(namespace);
		}
		Ok(())
	}

	/// Returns the authenticated principal.
	pub const fn principal(&self) -> &Principal {
		&self.principal
	}

	/// Returns the exact authenticated extension incarnation stamped on writes.
	pub const fn provenance(&self) -> &Provenance {
		&self.provenance
	}

	/// Returns the core's current host and session generations.
	pub const fn generation(&self) -> GenerationFence {
		self.generation
	}

	/// Returns the writing extension's own namespace.
	pub fn namespace(&self) -> &str {
		self.namespace.as_str()
	}

	/// Returns the authenticated session id for delegation to the session
	/// journal.
	pub fn session_id(&self) -> &str {
		self.session.as_str()
	}

	fn resolve(&self, scope: StateScope, write: bool) -> Result<ScopeKey, Error> {
		let authority = match scope {
			StateScope::Session => return Err(Error::WrongAuthority(StateScope::Session)),
			StateScope::Project => self.project.clone(),
			StateScope::User => Str::new(self.principal.id()),
			StateScope::Organization => {
				let access = self
					.organization
					.as_ref()
					.ok_or(Error::ScopeDenied(scope))?;
				if write && !access.can_write {
					return Err(Error::ScopeDenied(scope));
				}
				access.id.clone()
			},
		};
		Ok(ScopeKey { scope, authority })
	}

	/// Returns whether this authority may read `namespace`.
	pub fn may_read_namespace(&self, namespace: &str) -> bool {
		namespace == self.namespace
			|| self
				.readable_namespaces
				.iter()
				.any(|allowed| allowed == namespace)
	}
}

/// A typed append-log entry after strict storage-record decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEntry {
	/// Opaque entry identity and watermark.
	pub id:         StateEntryId,
	/// Declaring extension namespace.
	pub namespace:  Str,
	/// Globally declared entry-kind name.
	pub kind:       Str,
	/// Schema revision at which the bytes were encoded.
	pub schema_rev: Str,
	/// Epoch milliseconds assigned by the core.
	pub timestamp:  u64,
	/// Authenticated actor stamped by the core.
	pub principal:  Principal,
	/// Exact extension incarnation stamped by the core.
	pub provenance: Provenance,
	/// Verbatim canonical typed payload bytes.
	pub raw:        CowBytes<'static>,
}

/// The materialized result of an append-only compare-and-swap operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateValue {
	/// Scope revision which installed this value.
	pub revision: StateRevision,
	/// Namespaced key.
	pub key:      Str,
	/// Verbatim value bytes.
	pub value:    CowBytes<'static>,
}

/// A content-addressed value rooted by a scoped append-log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentRoot {
	/// Scope revision which established reachability.
	pub revision:  StateRevision,
	/// Immutable content reference.
	pub reference: BlobRef,
}

/// One committed change delivered to state watchers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateChange {
	/// A typed log entry was appended.
	Entry(StateEntry),
	/// A compare-and-swap installed a value.
	Value(StateValue),
	/// A content-addressed value became reachable in the scope.
	Content(ContentRoot),
}

const _: () = assert!(size_of::<StateChange>() <= 288, "StateChange must stay compact");

impl StateChange {
	/// Returns the scope revision assigned to the change.
	pub const fn revision(&self) -> StateRevision {
		match self {
			Self::Entry(entry) => entry.id.revision,
			Self::Value(value) => value.revision,
			Self::Content(root) => root.revision,
		}
	}
}

/// Fail-closed durable state errors.
#[derive(Debug, ThisError)]
pub enum Error {
	/// A filesystem operation failed.
	#[error(transparent)]
	Io(#[from] io::Error),
	/// Content-addressed blob storage failed.
	#[error(transparent)]
	Blob(#[from] BlobError),
	/// A durable record failed strict canonical encoding.
	#[error("state codec failed: {0}")]
	Codec(#[from] serde_json::Error),
	/// A persisted line was malformed, non-canonical, reordered, or otherwise
	/// corrupt.
	#[error("state record {line} is corrupt: {reason}")]
	CorruptRecord {
		/// One-based physical line number.
		line:   u64,
		/// Stable corruption reason.
		reason: Str,
	},
	/// A frame carried generations other than the core's current generations.
	#[error("durable request came from a stale generation")]
	StaleGeneration,
	/// The requested scope was not granted by the authority.
	#[error("state scope {0} is denied")]
	ScopeDenied(StateScope),
	/// The scope is owned by another durable authority.
	#[error("state scope {0} belongs to a different authority")]
	WrongAuthority(StateScope),
	/// The requested namespace was neither owned nor granted.
	#[error("state namespace is denied")]
	NamespaceDenied,
	/// A compare-and-swap observed a different revision.
	#[error("compare-and-swap conflict: expected {expected:?}, actual {actual:?}")]
	CasConflict {
		/// Revision supplied by the caller.
		expected: Option<StateRevision>,
		/// Revision currently installed.
		actual:   Option<StateRevision>,
	},
	/// One idempotency key was reused for a different logical operation.
	#[error("idempotency key was reused with different content")]
	IdempotencyConflict,
	/// An operation requiring exactly-once replay omitted its idempotency key.
	#[error("operation requires an idempotency key")]
	MissingIdempotencyKey,
	/// A name or identifier is empty, oversized, or contains control characters.
	#[error("invalid {0}")]
	InvalidKey(&'static str),
	/// Namespace is not a canonical dotted extension namespace.
	#[error("invalid state namespace")]
	InvalidNamespace,
	/// Core provenance and the authenticated generation disagree.
	#[error("invalid core state authority")]
	InvalidAuthority,
	/// A record may have reached durable storage but rollback could not be
	/// proven.
	#[error("state durability is indeterminate; the owning session must halt")]
	Indeterminate,
	/// A content reference is not rooted in the requested scope and namespace.
	#[error("content reference is not rooted in this state scope")]
	ContentNotRooted,
	/// Rooted content no longer matches its content-derived digest.
	#[error("content-addressed state value failed digest verification")]
	ContentDigestMismatch,
	/// The scope revision counter was exhausted.
	#[error("state revision exhausted")]
	RevisionExhausted,
}

/// A core-owned append log, replay index, compare-and-swap store, and scoped
/// CAS.
#[derive(Clone)]
pub struct StateStore {
	inner: Arc<StateInner>,
}

impl fmt::Debug for StateStore {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("StateStore")
			.field("path", &self.inner.path)
			.finish_non_exhaustive()
	}
}

struct StateInner {
	path:  PathBuf,
	blobs: BlobStore,
	state: Mutex<StoreState>,
	lock:  File,
}

struct StoreState {
	file:          File,
	durable_len:   u64,
	changes:       Vec<StoredChange>,
	next_revision: HashMap<ScopeKey, u64>,
	values:        HashMap<ValueKey, StateValue>,
	roots:         HashSet<RootKey>,
	idempotency:   HashMap<IdempotencyKey, IdempotentResult>,
	last_checksum: Option<Str>,
	watchers:      HashMap<u64, WatchRegistration>,
	next_watcher:  u64,
}

#[derive(Clone)]
struct StoredChange {
	scope:     ScopeKey,
	namespace: Str,
	change:    StateChange,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ValueKey {
	scope:     ScopeKey,
	namespace: Str,
	key:       Str,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RootKey {
	scope:     ScopeKey,
	namespace: Str,
	reference: BlobRef,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct IdempotencyKey {
	scope:     ScopeKey,
	namespace: Str,
	principal: Str,
	key:       Str,
}

#[derive(Clone)]
struct IdempotentResult {
	fingerprint: [u8; 32],
	change:      StateChange,
}

struct WatchRegistration {
	scope:     ScopeKey,
	namespace: Str,
	sender:    flume::Sender<StateChange>,
}

struct FileLease<'a>(&'a File);

impl<'a> FileLease<'a> {
	fn acquire(file: &'a File) -> Result<Self, Error> {
		file.lock()?;
		Ok(Self(file))
	}
}

impl Drop for FileLease<'_> {
	fn drop(&mut self) {
		let _ = self.0.unlock();
	}
}

impl StateStore {
	/// Opens the authority-owned store at `root`, replaying the complete private
	/// log before serving any operation.
	///
	/// # Errors
	///
	/// Returns a fail-closed error if any physical record is missing, corrupt,
	/// non-canonical, or cannot be read.
	#[tracing::instrument(
		name = "state_store_open",
		level = "debug",
		skip_all,
		fields(root = %root.as_ref().display())
	)]
	pub fn open(root: impl AsRef<Path>) -> Result<Self, Error> {
		let root = root.as_ref();
		fs::create_dir_all(root)?;
		let lock = OpenOptions::new()
			.create(true)
			.truncate(false)
			.read(true)
			.write(true)
			.open(root.join(LOCK_FILE))?;
		let path = root.join(LOG_FILE);
		let blobs = BlobStore::open(root.join("content"))?;
		let mut replay = ReplayState::default();
		let file;
		let durable_len;
		{
			let _lease = FileLease::acquire(&lock)?;
			if path.exists() {
				replay_file(&path, &mut replay)?;
			}
			file = OpenOptions::new()
				.create(true)
				.read(true)
				.append(true)
				.open(&path)?;
			file.sync_data()?;
			durable_len = file.metadata()?.len();
			lock.sync_data()?;
			#[cfg(unix)]
			File::open(root)?.sync_all()?;
		}
		tracing::debug!(
			event_count = replay.changes.len(),
			durable_bytes = durable_len,
			"state journal replay completed"
		);
		Ok(Self {
			inner: Arc::new(StateInner {
				path,
				blobs,
				state: Mutex::new(StoreState {
					file,
					durable_len,
					changes: replay.changes,
					next_revision: replay.next_revision,
					values: replay.values,
					roots: replay.roots,
					idempotency: replay.idempotency,
					last_checksum: replay.last_checksum,
					watchers: HashMap::new(),
					next_watcher: 0,
				}),
				lock,
			}),
		})
	}

	/// Appends one typed entry and returns its durable identity. A retry under
	/// the same idempotency key returns the first identity without appending.
	///
	/// # Errors
	///
	/// Fails closed on invalid scope, generation, codec, or durability.
	pub fn append(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		kind: impl Into<Str>,
		schema_rev: impl Into<Str>,
		data: &[u8],
		request: &DurableRequest,
	) -> Result<StateEntryId, Error> {
		check_generation(authority, request)?;
		let scope = authority.resolve(scope, true)?;
		let kind = kind.into();
		let schema_rev = schema_rev.into();
		validate_key("entry kind", kind.as_str())?;
		validate_key("schema revision", schema_rev.as_str())?;
		let op = RequestedOp::Append { kind: &kind, schema_rev: &schema_rev, data };
		let fingerprint = fingerprint(&op)?;
		let mut state = self.inner.state.lock();
		let _lease = self.refresh_locked(&mut state)?;
		if let Some(change) = replay_idempotent(&state, authority, &scope, request, fingerprint)? {
			return match change {
				StateChange::Entry(entry) => Ok(entry.id),
				_ => Err(Error::IdempotencyConflict),
			};
		}
		let revision = allocate_revision(&state, &scope)?;
		let timestamp = epoch_millis()?;
		let body = RecordBody {
			scope: scope.clone(),
			namespace: authority.namespace.clone(),
			revision,
			timestamp,
			principal: authority.principal.clone(),
			provenance: authority.provenance.clone(),
			request_id: request.request_id.clone(),
			idempotency_key: request.idempotency_key.clone(),
			host_generation: request.generation.host,
			session_generation: request.generation.session,
			previous_checksum: state.last_checksum.clone(),
			op: RecordOp::Append {
				kind:       kind.clone(),
				schema_rev: schema_rev.clone(),
				data:       Str::from(base64::encode(data).into_string()),
			},
		};
		let checksum = persist(&mut state, &body)?;
		let entry = StateEntry {
			id: StateEntryId { scope: scope.clone(), revision },
			namespace: authority.namespace.clone(),
			kind,
			schema_rev,
			timestamp,
			principal: authority.principal.clone(),
			provenance: authority.provenance.clone(),
			raw: CowBytes::copy_from_slice(data).into_owned(),
		};
		commit_change(
			&mut state,
			authority,
			scope,
			request,
			fingerprint,
			StateChange::Entry(entry.clone()),
			checksum,
		);
		Ok(entry.id)
	}

	/// Installs a namespaced value only when the current revision equals
	/// `expected`. The mutation itself is an append-only durable record.
	///
	/// # Errors
	///
	/// Returns [`Error::CasConflict`] on a lost race. An idempotency key is
	/// required so an indeterminate retry cannot install twice.
	pub fn compare_exchange(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		key: impl Into<Str>,
		expected: Option<StateRevision>,
		value: &[u8],
		request: &DurableRequest,
	) -> Result<StateValue, Error> {
		check_generation(authority, request)?;
		let idempotency_key = request
			.idempotency_key
			.as_ref()
			.ok_or(Error::MissingIdempotencyKey)?;
		let scope = authority.resolve(scope, true)?;
		let key = key.into();
		validate_key("state key", key.as_str())?;
		let op = RequestedOp::CompareExchange { key: &key, expected, value };
		let fingerprint = fingerprint(&op)?;
		let mut state = self.inner.state.lock();
		let _lease = self.refresh_locked(&mut state)?;
		if let Some(change) = replay_idempotent(&state, authority, &scope, request, fingerprint)? {
			return match change {
				StateChange::Value(value) => Ok(value),
				_ => Err(Error::IdempotencyConflict),
			};
		}
		let value_key = ValueKey {
			scope:     scope.clone(),
			namespace: authority.namespace.clone(),
			key:       key.clone(),
		};
		let actual = state.values.get(&value_key).map(|value| value.revision);
		if actual != expected {
			return Err(Error::CasConflict { expected, actual });
		}
		let revision = allocate_revision(&state, &scope)?;
		let timestamp = epoch_millis()?;
		let body = RecordBody {
			scope: scope.clone(),
			namespace: authority.namespace.clone(),
			revision,
			timestamp,
			principal: authority.principal.clone(),
			provenance: authority.provenance.clone(),
			request_id: request.request_id.clone(),
			idempotency_key: Some(idempotency_key.clone()),
			host_generation: request.generation.host,
			session_generation: request.generation.session,
			previous_checksum: state.last_checksum.clone(),
			op: RecordOp::CompareExchange {
				key: key.clone(),
				expected,
				value: Str::from(base64::encode(value).into_string()),
			},
		};
		let checksum = persist(&mut state, &body)?;
		let installed =
			StateValue { revision, key, value: CowBytes::copy_from_slice(value).into_owned() };
		state.values.insert(value_key, installed.clone());
		commit_change(
			&mut state,
			authority,
			scope,
			request,
			fingerprint,
			StateChange::Value(installed.clone()),
			checksum,
		);
		Ok(installed)
	}

	/// Returns the current compare-and-swap value for a readable namespace.
	///
	/// # Errors
	///
	/// Returns an access error when scope or namespace is not granted.
	pub fn value(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		namespace: &str,
		key: &str,
	) -> Result<Option<StateValue>, Error> {
		if !authority.may_read_namespace(namespace) {
			return Err(Error::NamespaceDenied);
		}
		let scope = authority.resolve(scope, false)?;
		let mut state = self.inner.state.lock();
		let _lease = self.refresh_locked(&mut state)?;
		Ok(state
			.values
			.get(&ValueKey { scope, namespace: Str::new(namespace), key: Str::new(key) })
			.cloned())
	}

	/// Returns a stable snapshot iterator of matching typed entries in ascending
	/// scope revision order.
	///
	/// # Errors
	///
	/// Returns an access error when scope or namespace is not granted.
	pub fn entries(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		namespace: &str,
		kind: &str,
		since: Option<StateRevision>,
		limit: Option<usize>,
	) -> Result<impl Iterator<Item = StateEntry>, Error> {
		if !authority.may_read_namespace(namespace) {
			return Err(Error::NamespaceDenied);
		}
		let scope = authority.resolve(scope, false)?;
		let mut state = self.inner.state.lock();
		let _lease = self.refresh_locked(&mut state)?;
		let mut entries = SmallVec::<StateEntry, 8>::new();
		if limit == Some(0) {
			return Ok(entries.into_iter());
		}
		for stored in &state.changes {
			if stored.scope == scope
				&& stored.namespace == namespace
				&& stored.change.revision() > since.unwrap_or_default()
				&& let StateChange::Entry(entry) = &stored.change
				&& entry.kind == kind
			{
				entries.push(entry.clone());
				if limit.is_some_and(|limit| entries.len() == limit) {
					break;
				}
			}
		}
		Ok(entries.into_iter())
	}

	/// Returns the latest matching typed entry in a readable namespace.
	///
	/// # Errors
	///
	/// Returns an access error when scope or namespace is not granted.
	pub fn latest(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		namespace: &str,
		kind: &str,
	) -> Result<Option<StateEntry>, Error> {
		Ok(self
			.entries(authority, scope, namespace, kind, None, None)?
			.last())
	}

	/// Folds matching entries in ascending revision order and returns the last
	/// applied entry id as a durable watermark.
	///
	/// # Errors
	///
	/// Returns an access error when scope or namespace is not granted.
	pub fn fold<T>(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		namespace: &str,
		kind: &str,
		since: Option<StateRevision>,
		mut value: T,
		mut reducer: impl FnMut(T, &StateEntry) -> T,
	) -> Result<(T, Option<StateEntryId>), Error> {
		let mut watermark = None;
		for entry in self.entries(authority, scope, namespace, kind, since, None)? {
			watermark = Some(entry.id.clone());
			value = reducer(value, &entry);
		}
		Ok((value, watermark))
	}

	/// Stores immutable bytes, then durably roots their reference in the scoped
	/// log. A failed root append leaves only an unreachable, sweepable blob.
	///
	/// # Errors
	///
	/// Fails closed on blob, scope, generation, or durability errors. An
	/// idempotency key is required.
	pub fn put_content(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		data: &[u8],
		request: &DurableRequest,
	) -> Result<ContentRoot, Error> {
		check_generation(authority, request)?;
		request
			.idempotency_key
			.as_ref()
			.ok_or(Error::MissingIdempotencyKey)?;
		let scope = authority.resolve(scope, true)?;
		let reference = self.inner.blobs.put(data)?;
		let op = RequestedOp::Content { reference };
		let fingerprint = fingerprint(&op)?;
		let mut state = self.inner.state.lock();
		let _lease = self.refresh_locked(&mut state)?;
		if let Some(change) = replay_idempotent(&state, authority, &scope, request, fingerprint)? {
			return match change {
				StateChange::Content(root) => Ok(root),
				_ => Err(Error::IdempotencyConflict),
			};
		}
		let revision = allocate_revision(&state, &scope)?;
		let body = RecordBody {
			scope: scope.clone(),
			namespace: authority.namespace.clone(),
			revision,
			timestamp: epoch_millis()?,
			principal: authority.principal.clone(),
			provenance: authority.provenance.clone(),
			request_id: request.request_id.clone(),
			idempotency_key: request.idempotency_key.clone(),
			host_generation: request.generation.host,
			session_generation: request.generation.session,
			previous_checksum: state.last_checksum.clone(),
			op: RecordOp::Content { reference },
		};
		let checksum = persist(&mut state, &body)?;
		let root = ContentRoot { revision, reference };
		state.roots.insert(RootKey {
			scope: scope.clone(),
			namespace: authority.namespace.clone(),
			reference,
		});
		commit_change(
			&mut state,
			authority,
			scope,
			request,
			fingerprint,
			StateChange::Content(root.clone()),
			checksum,
		);
		Ok(root)
	}

	/// Gets immutable content only if a log record roots it in the requested
	/// scope and readable namespace.
	///
	/// # Errors
	///
	/// Returns [`Error::ContentNotRooted`] instead of exposing a blob rooted by
	/// a different scope, principal, or namespace.
	pub fn get_content(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		namespace: &str,
		reference: &BlobRef,
	) -> Result<Bytes, Error> {
		if !authority.may_read_namespace(namespace) {
			return Err(Error::NamespaceDenied);
		}
		let scope = authority.resolve(scope, false)?;
		let mut state = self.inner.state.lock();
		let lease = self.refresh_locked(&mut state)?;
		let rooted = state.roots.contains(&RootKey {
			scope,
			namespace: Str::new(namespace),
			reference: *reference,
		});
		if !rooted {
			return Err(Error::ContentNotRooted);
		}
		drop(lease);
		drop(state);
		if !self.inner.blobs.verify(reference)? {
			return Err(Error::ContentDigestMismatch);
		}
		Ok(self.inner.blobs.get(reference)?)
	}

	/// Subscribes to a readable scope and namespace. Existing changes after
	/// `since` are queued before live changes under the writer lock, so no race
	/// exists between catch-up and subscription.
	///
	/// # Errors
	///
	/// Returns an access error when scope or namespace is not granted.
	pub fn watch(
		&self,
		authority: &StateAuthority,
		scope: StateScope,
		namespace: &str,
		since: Option<StateRevision>,
	) -> Result<StateWatcher, Error> {
		if !authority.may_read_namespace(namespace) {
			return Err(Error::NamespaceDenied);
		}
		let scope = authority.resolve(scope, false)?;
		let namespace = Str::new(namespace);
		let (sender, receiver) = flume::unbounded();
		let mut state = self.inner.state.lock();
		let _lease = self.refresh_locked(&mut state)?;
		for stored in &state.changes {
			if stored.scope == scope
				&& stored.namespace == namespace
				&& stored.change.revision() > since.unwrap_or_default()
			{
				let _ = sender.send(stored.change.clone());
			}
		}
		let id = state.next_watcher;
		state.next_watcher = state
			.next_watcher
			.checked_add(1)
			.ok_or(Error::RevisionExhausted)?;
		state
			.watchers
			.insert(id, WatchRegistration { scope, namespace, sender });
		Ok(StateWatcher { receiver, owner: Arc::downgrade(&self.inner), id })
	}

	fn refresh_locked<'a>(&'a self, state: &mut StoreState) -> Result<FileLease<'a>, Error> {
		let lease = FileLease::acquire(&self.inner.lock)?;
		refresh_state(&self.inner.path, state)?;
		Ok(lease)
	}
}

/// A cancellation-safe scoped state watcher. Dropping it unregisters the sender
/// immediately; a slow watcher never blocks the active authority writer.
#[must_use]
pub struct StateWatcher {
	receiver: Receiver<StateChange>,
	owner:    Weak<StateInner>,
	id:       u64,
}

impl fmt::Debug for StateWatcher {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("StateWatcher")
			.field("id", &self.id)
			.finish_non_exhaustive()
	}
}

impl StateWatcher {
	/// Blocks until the next matching change arrives or the store disconnects.
	///
	/// # Errors
	///
	/// Returns `RecvError` if the store has been dropped.
	pub fn recv(&self) -> Result<StateChange, flume::RecvError> {
		self.receiver.recv()
	}

	/// Asynchronously waits for the next matching change.
	///
	/// # Errors
	///
	/// Returns `RecvError` if the store has been dropped.
	pub async fn recv_async(&self) -> Result<StateChange, flume::RecvError> {
		self.receiver.recv_async().await
	}

	/// Attempts to receive one queued change without blocking.
	///
	/// # Errors
	///
	/// Returns `Empty` when no change is queued and `Disconnected` when the
	/// store has been dropped.
	pub fn try_recv(&self) -> Result<StateChange, TryRecvError> {
		self.receiver.try_recv()
	}
}

impl Drop for StateWatcher {
	fn drop(&mut self) {
		if let Some(owner) = self.owner.upgrade() {
			owner.state.lock().watchers.remove(&self.id);
		}
	}
}

#[derive(Default)]
struct ReplayState {
	changes:       Vec<StoredChange>,
	next_revision: HashMap<ScopeKey, u64>,
	values:        HashMap<ValueKey, StateValue>,
	roots:         HashSet<RootKey>,
	idempotency:   HashMap<IdempotencyKey, IdempotentResult>,
	last_checksum: Option<Str>,
}

fn refresh_state(path: &Path, state: &mut StoreState) -> Result<(), Error> {
	let durable_len = fs::metadata(path)?.len();
	if durable_len == state.durable_len {
		return Ok(());
	}
	if durable_len < state.durable_len {
		return corrupt(0, "state log was truncated");
	}
	let old_count = state.changes.len();
	let mut replay = ReplayState::default();
	replay_file(path, &mut replay)?;
	if replay.changes.len() < old_count {
		return corrupt(0, "state log lost committed records");
	}
	for stored in replay.changes.iter().skip(old_count) {
		state.watchers.retain(|_, watcher| {
			watcher.scope != stored.scope
				|| watcher.namespace != stored.namespace
				|| watcher.sender.send(stored.change.clone()).is_ok()
		});
	}
	state.file = OpenOptions::new().read(true).append(true).open(path)?;
	state.durable_len = durable_len;
	state.changes = replay.changes;
	state.next_revision = replay.next_revision;
	state.values = replay.values;
	state.roots = replay.roots;
	state.idempotency = replay.idempotency;
	state.last_checksum = replay.last_checksum;
	Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordEnvelope {
	codec:    Str,
	body:     RecordBody,
	checksum: Str,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordBody {
	scope:              ScopeKey,
	namespace:          Str,
	revision:           StateRevision,
	timestamp:          u64,
	principal:          Principal,
	provenance:         Provenance,
	request_id:         Str,
	idempotency_key:    Option<Str>,
	host_generation:    u64,
	session_generation: u64,
	previous_checksum:  Option<Str>,
	op:                 RecordOp,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum RecordOp {
	Append { kind: Str, schema_rev: Str, data: Str },
	CompareExchange { key: Str, expected: Option<StateRevision>, value: Str },
	Content { reference: BlobRef },
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum RequestedOp<'a> {
	Append {
		kind:       &'a Str,
		schema_rev: &'a Str,
		#[serde(with = "serde_bytes_as_base64")]
		data:       &'a [u8],
	},
	CompareExchange {
		key:      &'a Str,
		expected: Option<StateRevision>,
		#[serde(with = "serde_bytes_as_base64")]
		value:    &'a [u8],
	},
	Content {
		reference: BlobRef,
	},
}

mod serde_bytes_as_base64 {
	use serde::Serializer;

	pub(super) fn serialize<S>(value: &&[u8], serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.serialize_str(&omp_core::base64::encode(value).into_string())
	}
}

fn replay_file(path: &Path, replay: &mut ReplayState) -> Result<(), Error> {
	let file = File::open(path)?;
	let mut reader = BufReader::new(file);
	let mut line = Vec::new();
	let mut number = 0_u64;
	loop {
		line.clear();
		let read = reader.read_until(b'\n', &mut line)?;
		if read == 0 {
			break;
		}
		number = number.checked_add(1).ok_or(Error::RevisionExhausted)?;
		if read > MAX_RECORD_BYTES || line.last() != Some(&b'\n') {
			return corrupt(number, "record is oversized or unterminated");
		}
		line.pop();
		let envelope: RecordEnvelope =
			serde_json::from_slice(&line).map_err(|error| corrupt_error(number, error.to_string()))?;
		if envelope.codec != CODEC {
			return corrupt(number, "unknown state codec");
		}
		if envelope.body.previous_checksum != replay.last_checksum {
			return corrupt(number, "checksum chain mismatch");
		}
		let canonical_body = serde_json::to_vec(&envelope.body)?;
		let expected_checksum = checksum(&canonical_body);
		if envelope.checksum != expected_checksum {
			return corrupt(number, "record checksum mismatch");
		}
		let canonical = serde_json::to_vec(&RecordEnvelope {
			codec:    sf!(CODEC),
			body:     envelope.body,
			checksum: envelope.checksum.clone(),
		})?;
		if canonical != line {
			return corrupt(number, "record encoding is not canonical");
		}
		let envelope: RecordEnvelope = serde_json::from_slice(&canonical)?;
		apply_replay(replay, envelope.body, envelope.checksum, number)?;
	}
	Ok(())
}

fn apply_replay(
	replay: &mut ReplayState,
	body: RecordBody,
	checksum: Str,
	line: u64,
) -> Result<(), Error> {
	validate_namespace(body.namespace.as_str())
		.map_err(|error| corrupt_error(line, error.to_string()))?;
	validate_key("request id", body.request_id.as_str())
		.map_err(|error| corrupt_error(line, error.to_string()))?;
	if body.provenance.generation() != body.host_generation {
		return corrupt(line, "provenance generation mismatch");
	}
	if body.provenance.extension_id() != body.namespace.as_str() {
		return corrupt(line, "provenance namespace mismatch");
	}
	if body.scope.scope == StateScope::Session {
		return corrupt(line, "session state belongs to the session journal");
	}
	if body.scope.scope == StateScope::User && body.scope.authority.as_str() != body.principal.id() {
		return corrupt(line, "user scope principal mismatch");
	}
	let expected = replay.next_revision.get(&body.scope).copied().unwrap_or(1);
	if body.revision.get() != expected {
		return corrupt(line, "scope revision is not monotonic");
	}
	let next = expected.checked_add(1).ok_or(Error::RevisionExhausted)?;
	let principal_key = Str::new(body.principal.id());
	let (change, fingerprint) = match body.op {
		RecordOp::Append { kind, schema_rev, data } => {
			validate_key("entry kind", kind.as_str())
				.map_err(|error| corrupt_error(line, error.to_string()))?;
			validate_key("schema revision", schema_rev.as_str())
				.map_err(|error| corrupt_error(line, error.to_string()))?;
			let raw = decode_canonical_base64(&data, line)?;
			let fingerprint = fingerprint(&RequestedOp::Append {
				kind:       &kind,
				schema_rev: &schema_rev,
				data:       &raw,
			})?;
			let entry = StateEntry {
				id: StateEntryId { scope: body.scope.clone(), revision: body.revision },
				namespace: body.namespace.clone(),
				kind,
				schema_rev,
				timestamp: body.timestamp,
				principal: body.principal.clone(),
				provenance: body.provenance.clone(),
				raw: CowBytes::owned(Bytes::from(raw)),
			};
			(StateChange::Entry(entry), fingerprint)
		},
		RecordOp::CompareExchange { key, expected, value } => {
			validate_key("state key", key.as_str())
				.map_err(|error| corrupt_error(line, error.to_string()))?;
			let raw = decode_canonical_base64(&value, line)?;
			let value_key = ValueKey {
				scope:     body.scope.clone(),
				namespace: body.namespace.clone(),
				key:       key.clone(),
			};
			let actual = replay.values.get(&value_key).map(|value| value.revision);
			if actual != expected {
				return corrupt(line, "persisted compare-and-swap precondition is false");
			}
			let fingerprint =
				fingerprint(&RequestedOp::CompareExchange { key: &key, expected, value: &raw })?;
			let installed =
				StateValue { revision: body.revision, key, value: CowBytes::owned(Bytes::from(raw)) };
			replay.values.insert(value_key, installed.clone());
			(StateChange::Value(installed), fingerprint)
		},
		RecordOp::Content { reference } => {
			let fingerprint = fingerprint(&RequestedOp::Content { reference })?;
			replay.roots.insert(RootKey {
				scope: body.scope.clone(),
				namespace: body.namespace.clone(),
				reference,
			});
			(StateChange::Content(ContentRoot { revision: body.revision, reference }), fingerprint)
		},
	};
	if let Some(key) = body.idempotency_key {
		let map_key = IdempotencyKey {
			scope: body.scope.clone(),
			namespace: body.namespace.clone(),
			principal: principal_key,
			key,
		};
		if replay
			.idempotency
			.insert(map_key, IdempotentResult { fingerprint, change: change.clone() })
			.is_some()
		{
			return corrupt(line, "duplicate idempotency key in physical log");
		}
	}
	replay.next_revision.insert(body.scope.clone(), next);
	replay
		.changes
		.push(StoredChange { scope: body.scope, namespace: body.namespace, change });
	replay.last_checksum = Some(checksum);
	Ok(())
}

fn decode_canonical_base64(encoded: &Str, line: u64) -> Result<Vec<u8>, Error> {
	let decoded = base64::decode(encoded.as_bytes())
		.into_vec()
		.map_err(|_| corrupt_error(line, "invalid base64 payload"))?;
	if base64::encode(&decoded).into_string() != encoded.as_str() {
		return corrupt(line, "non-canonical base64 payload");
	}
	Ok(decoded)
}

fn persist(state: &mut StoreState, body: &RecordBody) -> Result<Str, Error> {
	let encoded_body = serde_json::to_vec(body)?;
	let checksum = checksum(&encoded_body);
	let envelope = RecordEnvelope {
		codec:    sf!(CODEC),
		body:     RecordBody {
			scope:              body.scope.clone(),
			namespace:          body.namespace.clone(),
			revision:           body.revision,
			timestamp:          body.timestamp,
			principal:          body.principal.clone(),
			provenance:         body.provenance.clone(),
			request_id:         body.request_id.clone(),
			idempotency_key:    body.idempotency_key.clone(),
			host_generation:    body.host_generation,
			session_generation: body.session_generation,
			previous_checksum:  body.previous_checksum.clone(),
			op:                 clone_record_op(&body.op),
		},
		checksum: checksum.clone(),
	};
	let mut line = serde_json::to_vec(&envelope)?;
	if line.len() >= MAX_RECORD_BYTES {
		return Err(Error::InvalidKey("record size"));
	}
	line.push(b'\n');
	let original = state.file.seek(SeekFrom::End(0))?;
	if original != state.durable_len {
		return Err(Error::Indeterminate);
	}
	if let Err(error) = state
		.file
		.write_all(&line)
		.and_then(|()| state.file.sync_data())
	{
		if state
			.file
			.set_len(original)
			.and_then(|()| state.file.sync_data())
			.is_err()
		{
			return Err(Error::Indeterminate);
		}
		return Err(Error::Io(error));
	}
	let written =
		u64::try_from(line.len()).map_err(|_| io::Error::other("state record length exceeds u64"))?;
	state.durable_len = original
		.checked_add(written)
		.ok_or(Error::RevisionExhausted)?;
	Ok(checksum)
}

fn clone_record_op(op: &RecordOp) -> RecordOp {
	match op {
		RecordOp::Append { kind, schema_rev, data } => RecordOp::Append {
			kind:       kind.clone(),
			schema_rev: schema_rev.clone(),
			data:       data.clone(),
		},
		RecordOp::CompareExchange { key, expected, value } => RecordOp::CompareExchange {
			key:      key.clone(),
			expected: *expected,
			value:    value.clone(),
		},
		RecordOp::Content { reference } => RecordOp::Content { reference: *reference },
	}
}

fn commit_change(
	state: &mut StoreState,
	authority: &StateAuthority,
	scope: ScopeKey,
	request: &DurableRequest,
	fingerprint: [u8; 32],
	change: StateChange,
	checksum: Str,
) {
	let next = change.revision().get() + 1;
	state.next_revision.insert(scope.clone(), next);
	if let Some(key) = &request.idempotency_key {
		state.idempotency.insert(
			IdempotencyKey {
				scope:     scope.clone(),
				namespace: authority.namespace.clone(),
				principal: Str::new(authority.principal.id()),
				key:       key.clone(),
			},
			IdempotentResult { fingerprint, change: change.clone() },
		);
	}
	state.changes.push(StoredChange {
		scope:     scope.clone(),
		namespace: authority.namespace.clone(),
		change:    change.clone(),
	});
	state.last_checksum = Some(checksum);
	state.watchers.retain(|_, watcher| {
		watcher.scope != scope
			|| watcher.namespace != authority.namespace
			|| watcher.sender.send(change.clone()).is_ok()
	});
}

fn replay_idempotent(
	state: &StoreState,
	authority: &StateAuthority,
	scope: &ScopeKey,
	request: &DurableRequest,
	fingerprint: [u8; 32],
) -> Result<Option<StateChange>, Error> {
	let Some(key) = &request.idempotency_key else {
		return Ok(None);
	};
	let map_key = IdempotencyKey {
		scope:     scope.clone(),
		namespace: authority.namespace.clone(),
		principal: Str::new(authority.principal.id()),
		key:       key.clone(),
	};
	let Some(recorded) = state.idempotency.get(&map_key) else {
		return Ok(None);
	};
	if recorded.fingerprint != fingerprint {
		return Err(Error::IdempotencyConflict);
	}
	Ok(Some(recorded.change.clone()))
}

fn allocate_revision(state: &StoreState, scope: &ScopeKey) -> Result<StateRevision, Error> {
	let next = state.next_revision.get(scope).copied().unwrap_or(1);
	if next == u64::MAX {
		return Err(Error::RevisionExhausted);
	}
	Ok(StateRevision(next))
}

fn fingerprint(operation: &RequestedOp<'_>) -> Result<[u8; 32], Error> {
	Ok(Hash32::sum(serde_json::to_vec(operation)?).into_bytes())
}

fn checksum(encoded_body: &[u8]) -> Str {
	Str::new(Hash32::sum(encoded_body).to_hex().as_str())
}

fn check_generation(authority: &StateAuthority, request: &DurableRequest) -> Result<(), Error> {
	if authority.generation != request.generation {
		return Err(Error::StaleGeneration);
	}
	Ok(())
}

fn epoch_millis() -> Result<u64, Error> {
	let duration = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_err(|_| io::Error::other("system clock precedes Unix epoch"))?;
	u64::try_from(duration.as_millis())
		.map_err(|_| io::Error::other("epoch millisecond timestamp exceeds u64").into())
}

fn validate_namespace(namespace: &str) -> Result<(), Error> {
	validate_key("namespace", namespace)?;
	if !namespace.contains('.')
		|| namespace.split('.').any(|part| {
			part.is_empty()
				|| !part
					.bytes()
					.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
		}) {
		return Err(Error::InvalidNamespace);
	}
	Ok(())
}

fn validate_key(label: &'static str, value: &str) -> Result<(), Error> {
	if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
		return Err(Error::InvalidKey(label));
	}
	Ok(())
}

const fn corrupt<T>(line: u64, reason: &'static str) -> Result<T, Error> {
	Err(Error::CorruptRecord { line, reason: sf!(reason) })
}

fn corrupt_error(line: u64, reason: impl IntoStr) -> Error {
	Error::CorruptRecord { line, reason: reason.into_str() }
}
