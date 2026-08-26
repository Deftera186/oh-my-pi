//! Canonical project-bank identity and conservative legacy-bank adoption.

use std::{
	collections::HashSet,
	fmt::{self, Display},
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::{Hash32, Str};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, config::BankScoping};

const DEFAULT_SHARED_BANK: &str = "default";
const MAX_BANK_BYTES: usize = 64;
const LEGACY_SCAN_LIMIT: usize = 64;

/// Validated Mnemopi bank identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BankId(Str);

impl BankId {
	/// Validates and normalizes a user-configured bank name.
	pub fn configured(value: &str) -> Result<Self> {
		sanitize(value).map(Self).ok_or(Error::InvalidIdentifier)
	}

	/// Borrows the identifier.
	pub fn as_str(&self) -> &str {
		self.0.as_str()
	}
}

impl Display for BankId {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}

impl AsRef<str> for BankId {
	fn as_ref(&self) -> &str {
		self.as_str()
	}
}

/// Environment-owned identity facts consumed by bank selection.
pub struct BankScopeInput<'a> {
	/// Canonical primary Git root from the Environment repository snapshot.
	/// Linked worktrees must receive the same value.
	pub canonical_primary_root: Option<&'a Path>,
	/// Canonical selected workspace root, used only outside a Git repository.
	pub workspace_root:         &'a Path,
	/// Optional configured shared bank base.
	pub configured_bank:        Option<&'a str>,
	/// Requested write/recall scoping.
	pub scoping:                BankScoping,
}

/// Resolved durable write bank and ordered recall-bank fallback set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BankScope {
	/// Stable root identity that was hashed. Memory code never probes Git
	/// itself.
	pub identity_root: PathBuf,
	/// Shared bank.
	pub global:        BankId,
	/// Bank receiving new durable rows.
	pub retain:        BankId,
	/// Ordered banks searched by recall.
	pub recall:        Vec<BankId>,
	/// Applied scoping mode.
	pub scoping:       BankScoping,
}

impl BankScope {
	/// Resolves bank identity exclusively from Environment-supplied roots.
	pub fn resolve(input: BankScopeInput<'_>) -> Result<Self> {
		let identity_root = input
			.canonical_primary_root
			.unwrap_or(input.workspace_root)
			.to_path_buf();
		let global = match input.configured_bank {
			Some(value) => BankId::configured(value)?,
			None => BankId(Str::new_static(DEFAULT_SHARED_BANK)),
		};
		let project = project_bank(&identity_root, input.configured_bank)?;
		let (retain, recall) = match input.scoping {
			BankScoping::Global => (global.clone(), vec![global.clone()]),
			BankScoping::PerProject => (project.clone(), vec![project]),
			BankScoping::PerProjectTagged if project == global => (project.clone(), vec![project]),
			BankScoping::PerProjectTagged => (project.clone(), vec![project, global.clone()]),
		};
		Ok(Self { identity_root, global, retain, recall, scoping: input.scoping })
	}

	/// Adds already-validated adopted legacy banks after the normal recall
	/// order.
	pub fn append_adopted(&mut self, adopted: impl IntoIterator<Item = BankId>) {
		let mut seen = self.recall.iter().cloned().collect::<HashSet<_>>();
		for bank in adopted {
			if seen.insert(bank.clone()) {
				self.recall.push(bank);
			}
		}
	}
}

/// Returns the isolated SQLite file for `bank`; the shared bank alone owns the
/// primary DB.
pub fn database_path(db_dir: &Path, global: &BankId, bank: &BankId) -> PathBuf {
	if bank == global {
		db_dir.join("mnemopi.db")
	} else {
		db_dir.join("banks").join(bank.as_str()).join("mnemopi.db")
	}
}

/// Conservatively discovers legacy single-root banks.
///
/// Every durable row must name exactly the active canonical identity or
/// selected workspace root. Missing, mixed, unreadable, and corrupt banks are
/// ignored. Discovery is bounded to 64 bank directories and never adopts based
/// on a basename.
pub fn discover_legacy_banks(
	db_dir: &Path,
	resolved: &[BankId],
	identity_root: &Path,
	workspace_root: &Path,
) -> Result<Vec<BankId>> {
	let banks_dir = db_dir.join("banks");
	let entries = match fs::read_dir(banks_dir) {
		Ok(entries) => entries,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(error) => return Err(error.into()),
	};
	let have = resolved.iter().map(BankId::as_str).collect::<HashSet<_>>();
	let identity = identity_root.to_string_lossy();
	let workspace = workspace_root.to_string_lossy();
	let mut adopted = Vec::new();
	let mut scanned = 0usize;
	for entry in entries {
		let entry = entry?;
		if scanned == LEGACY_SCAN_LIMIT {
			break;
		}
		if !entry.file_type()?.is_dir() {
			continue;
		}
		let name = entry.file_name();
		let Some(name) = name.to_str() else { continue };
		if have.contains(name) {
			continue;
		}
		scanned += 1;
		let Ok(bank) = BankId::configured(name) else {
			continue;
		};
		let path = entry.path().join("mnemopi.db");
		if legacy_bank_matches(&path, identity.as_ref(), workspace.as_ref()) {
			adopted.push(bank);
		}
	}
	adopted.sort_unstable();
	Ok(adopted)
}

fn legacy_bank_matches(path: &Path, identity: &str, workspace: &str) -> bool {
	let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
	let Ok(connection) = Connection::open_with_flags(path, flags) else {
		return false;
	};
	let recorded = connection
		.query_row("SELECT identity_root FROM bank_scope WHERE singleton = 1", [], |row| {
			row.get::<_, String>(0)
		})
		.optional();
	if let Ok(Some(recorded)) = recorded {
		return recorded == identity;
	}
	let query = "SELECT COUNT(*) AS total,\nSUM(CASE WHEN json_extract(metadata_json, \
	             '$.primary_root') = ?1\nOR json_extract(metadata_json, '$.cwd') = ?2 THEN 1 ELSE \
	             0 END) AS matching\nFROM working_memory";
	connection
		.query_row(query, (identity, workspace), |row| {
			let total = row.get::<_, i64>(0)?;
			let matching = row.get::<_, i64>(1)?;
			Ok(total > 0 && matching == total)
		})
		.unwrap_or(false)
}

fn project_bank(root: &Path, configured: Option<&str>) -> Result<BankId> {
	let project = root
		.file_name()
		.and_then(|name| name.to_str())
		.and_then(sanitize)
		.unwrap_or_else(|| Str::new_static("default"));
	let digest = Hash32::sum(root.to_string_lossy().as_bytes());
	let hex = digest.to_hex();
	let hash = &hex.as_str()[..12];
	let raw = match configured.and_then(sanitize) {
		Some(base) => format!("{}-{}-{hash}", base.as_str(), project.as_str()),
		None => format!("{}-{hash}", project.as_str()),
	};
	Ok(BankId(limit(&raw)))
}

fn sanitize(value: &str) -> Option<Str> {
	let value = value.trim();
	if value.is_empty() {
		return None;
	}
	let mut output = String::with_capacity(value.len().min(MAX_BANK_BYTES));
	let mut last_dash = false;
	for byte in value.bytes() {
		let valid = byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
		if valid {
			output.push(char::from(byte));
			last_dash = byte == b'-';
		} else if !last_dash && !output.is_empty() {
			output.push('-');
			last_dash = true;
		}
	}
	while output.ends_with('-') {
		output.pop();
	}
	if output.is_empty() {
		None
	} else {
		Some(limit(&output))
	}
}

fn limit(name: &str) -> Str {
	if name.len() <= MAX_BANK_BYTES {
		return Str::new(name);
	}
	let digest = Hash32::sum(name.as_bytes());
	let hex = digest.to_hex();
	let suffix = &hex.as_str()[..12];
	let prefix_bytes = MAX_BANK_BYTES - suffix.len() - 1;
	let mut prefix = name[..prefix_bytes].trim_end_matches('-').to_owned();
	if prefix.is_empty() {
		prefix.push_str("bank");
	}
	Str::new(format!("{prefix}-{suffix}"))
}
