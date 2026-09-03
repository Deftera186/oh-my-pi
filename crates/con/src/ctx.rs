//! The console context: registry, storage, dispatch, binds, aliases, and
//! host integration points.

use std::{
	any::{Any, TypeId},
	fmt,
	fmt::Write as _,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU32, Ordering},
	},
};

use omp_core::{AppendVec, FastHashMap, IntoStr, Str, StrMut};
use parking_lot::RwLock;
use strum::Display;

use crate::{
	Arg, ConError, ConResult, LayerId, Origin, ParseError, RegItem, Role, Seed, SetReport,
	Statement, Value, ValueKind, VarFlags, VarSpec,
	layers::Layers,
	script::{self, CoerceIssue},
};

/// Name of the built-in unsafe gate variable.
pub const UNSAFE_NAME: &str = "sv_cheats";

/// Maximum alias/`exec` nesting depth per context.
const MAX_DEPTH: u32 = 16;

struct DepthGuard<'a>(&'a AtomicU32);

impl Drop for DepthGuard<'_> {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::AcqRel);
	}
}

/// Reply channel severity.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum Severity {
	/// Normal command output.
	Info,
	/// Recoverable oddity (skipped replication patch, lenient exec failure).
	Warn,
	/// Failed statement.
	Error,
}

/// Provenance of a variable write, deciding which permission gates apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetSource {
	/// Trusted host code ([`CVar::set`](crate::CVar::set), [`Ctx::set`]);
	/// bypasses `READONLY`/`UNSAFE` gates.
	Code,
	/// Console input / cfg script; all gates apply.
	Script,
	/// Authority patch on a replica; bypasses gates and `validate`.
	Replication,
}

/// Origin of a command stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
	/// Interactive/local console input.
	Console,
	/// A named archive config.
	Config(Str),
	/// Automatic child config.
	Subagent,
	/// Automatic agent-class config.
	Agent(Str),
	/// Replay of a session-tree convar.
	Session,
}

impl Source {
	fn origin(&self) -> Origin {
		match self {
			Self::Config(_) => Origin::Archive,
			Self::Session => Origin::Session,
			Self::Console => Origin::Script(Str::new_static("console")),
			Self::Subagent => Origin::Script(Str::new_static("subagent.cfg")),
			Self::Agent(name) => Origin::Script(name.clone()),
		}
	}
}

/// One successfully executed command-stream statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
	/// One-based source line of the statement.
	pub line: u32,
}

/// Loads cfg script text by name.
pub trait CfgLoader: Send + Sync {
	/// Returns cfg text, or `None` when the file is absent.
	fn load(&self, name: &str) -> Option<Str>;
}

impl<F> CfgLoader for F
where
	F: Fn(&str) -> Option<Str> + Send + Sync,
{
	fn load(&self, name: &str) -> Option<Str> {
		self(name)
	}
}

/// Saves a generated cfg script.
pub trait CfgSaver: Send + Sync {
	/// Persists `contents` under `name`.
	fn save(&self, name: &str, contents: &str) -> ConResult<()>;
}

impl<F> CfgSaver for F
where
	F: Fn(&str, &str) -> ConResult<()> + Send + Sync,
{
	fn save(&self, name: &str, contents: &str) -> ConResult<()> {
		self(name, contents)
	}
}

/// Outcome of a lenient script execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecOutcome {
	/// Statements that ran successfully.
	pub ran:    usize,
	/// Statements that failed (reported through the sink).
	pub failed: usize,
}

/// Reply sink: receives all console output.
pub type SinkFn = dyn Fn(Severity, &str) + Send + Sync;
/// Config source resolver for `exec`: name → script text.
pub type LoaderFn = dyn Fn(&str) -> Option<Str> + Send + Sync;
/// Config writer for `writecfg`: `(name, contents)`.
pub type SaverFn = dyn Fn(&str, &str) -> ConResult<()> + Send + Sync;
/// Value observer, called after each committed non-no-op variable change.
pub type ObserverFn = dyn Fn(&str, &Value, &Value) + Send + Sync;
/// Handler for an owned dynamically registered command.
pub type DynamicCmdHandler = fn(&Ctx, &str, &[Arg]) -> ConResult<()>;

/// Owned descriptor for a dynamically registered variable.
pub struct DynamicVarSpec {
	/// Canonical console name.
	pub name:    Str,
	/// Human description.
	pub desc:    Str,
	/// Value type descriptor.
	pub ty:      &'static crate::TypeSpec,
	/// Behavior flags.
	pub flags:   VarFlags,
	/// Registration-time default.
	pub default: Value,
}

/// Owned descriptor for a dynamically registered command.
pub struct DynamicCmdSpec {
	/// Canonical console name.
	pub name:    Str,
	/// Human description.
	pub desc:    Str,
	/// Shared dynamic dispatch entry point.
	pub handler: DynamicCmdHandler,
}

/// Per-item mutable state.
pub struct ItemState {
	value:   RwLock<Value>,
	default: Value,
	dirty:   AtomicBool,
	presses: AtomicU32,
}

impl ItemState {
	fn new(value: Value) -> Self {
		Self {
			value:   RwLock::new(value.clone()),
			default: value,
			dirty:   AtomicBool::new(false),
			presses: AtomicU32::new(0),
		}
	}

	/// Registration-time default used as the D10 persistence baseline.
	pub(crate) const fn default_value(&self) -> &Value {
		&self.default
	}

	/// Snapshot of the current value.
	pub(crate) fn value(&self) -> Value {
		self.value.read().clone()
	}

	/// Consumes the dirty bit.
	pub(crate) fn take_dirty(&self) -> bool {
		self.dirty.swap(false, Ordering::AcqRel)
	}

	pub(crate) fn presses(&self) -> u32 {
		self.presses.load(Ordering::Acquire)
	}
}
struct Item {
	spec:  RegItem,
	state: ItemState,
}
struct DynamicVar {
	spec:  DynamicVarSpec,
	state: ItemState,
}

struct VarRef<'a> {
	name:      &'a str,
	ty:        &'static crate::TypeSpec,
	min:       Option<f64>,
	max:       Option<f64>,
	flags:     VarFlags,
	on_change: Option<crate::ChangeHook>,
	validate:  Option<crate::ValidateHook>,
	state:     &'a ItemState,
}

const DYNAMIC_VAR: u32 = 1 << 31;
const DYNAMIC_CMD: u32 = 1 << 30;
const DYNAMIC_INDEX: u32 = !(DYNAMIC_VAR | DYNAMIC_CMD);

/// Builder for [`Ctx`].
#[derive(Default)]
pub struct CtxBuilder {
	role:     Role,
	sink:     Option<Box<SinkFn>>,
	loader:   Option<Box<LoaderFn>>,
	saver:    Option<Box<SaverFn>>,
	isolated: bool,
	user:     Vec<(TypeId, Arc<dyn Any + Send + Sync>)>,
}

impl CtxBuilder {
	/// Sets the replication role (default [`Role::Standalone`]).
	#[must_use]
	pub const fn role(mut self, role: Role) -> Self {
		self.role = role;
		self
	}

	/// Installs the reply sink (default: output is dropped).
	#[must_use]
	pub fn sink(mut self, sink: impl Fn(Severity, &str) + Send + Sync + 'static) -> Self {
		self.sink = Some(Box::new(sink));
		self
	}

	/// Installs the `exec` config loader.
	#[must_use]
	pub fn loader(mut self, loader: impl Fn(&str) -> Option<Str> + Send + Sync + 'static) -> Self {
		self.loader = Some(Box::new(loader));
		self
	}

	/// Installs the `writecfg` config saver.
	#[must_use]
	pub fn saver(
		mut self,
		saver: impl Fn(&str, &str) -> ConResult<()> + Send + Sync + 'static,
	) -> Self {
		self.saver = Some(Box::new(saver));
		self
	}

	/// Stores a typed host object, retrievable via [`Ctx::user`].
	#[must_use]
	pub fn user<T: Send + Sync + 'static>(mut self, value: T) -> Self {
		self.user.push((TypeId::of::<T>(), Arc::new(value)));
		self
	}

	/// Skips folding the link-time [`REGISTRY`](crate::REGISTRY) (isolated
	/// contexts for tests/embedders; note the built-ins live in the registry
	/// too, so an isolated context starts truly empty).
	#[must_use]
	pub const fn isolated(mut self) -> Self {
		self.isolated = true;
		self
	}

	/// Builds the context, folding the link-time registry unless
	/// [`isolated`](Self::isolated).
	///
	/// # Panics
	/// On duplicate registered names — two crates claiming one name is a
	/// build misconfiguration, not a runtime condition.
	#[must_use]
	pub fn build(self) -> Ctx {
		let ctx = Ctx {
			items:         AppendVec::new(),
			dynamic_vars:  AppendVec::new(),
			dynamic_cmds:  AppendVec::new(),
			names:         RwLock::new(FastHashMap::default()),
			aliases:       RwLock::new(FastHashMap::default()),
			binds:         RwLock::new(FastHashMap::default()),
			bind_baseline: RwLock::new(None),
			completers:    RwLock::new(FastHashMap::default()),
			observers:     RwLock::new(Vec::new()),
			user:          RwLock::new(self.user.into_iter().collect()),
			layers:        RwLock::new(Layers::default()),
			depth:         AtomicU32::new(0),
			sink:          self.sink,
			loader:        self.loader,
			saver:         self.saver,
			role:          self.role,
		};
		if !self.isolated {
			for item in crate::REGISTRY {
				if let Err(err) = ctx.register(*item) {
					panic!("console registry conflict: {err}");
				}
			}
		}
		ctx
	}
}

/// The console: one registry of typed variables, commands, and actions,
/// plus aliases, key binds, completion providers, and host userdata.
///
/// `Ctx` is `Send + Sync`; commands receive `&Ctx` and mutate through
/// interior mutability.
pub struct Ctx {
	items:                 AppendVec<Item>,
	dynamic_vars:          AppendVec<DynamicVar>,
	dynamic_cmds:          AppendVec<DynamicCmdSpec>,
	names:                 RwLock<FastHashMap<Str, u32>>,
	aliases:               RwLock<FastHashMap<Str, Str>>,
	binds:                 RwLock<FastHashMap<Str, Str>>,
	/// Bind table as it stood after the default bind cfg ran; `dump` diffs
	/// against it instead of resetting the whole table.
	bind_baseline:         RwLock<Option<FastHashMap<Str, Str>>>,
	pub(crate) completers: RwLock<FastHashMap<Str, Box<crate::CompleterFn>>>,
	observers:             RwLock<Vec<Box<ObserverFn>>>,
	user:                  RwLock<FastHashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
	layers:                RwLock<Layers>,
	depth:                 AtomicU32,
	sink:                  Option<Box<SinkFn>>,
	loader:                Option<Box<LoaderFn>>,
	saver:                 Option<Box<SaverFn>>,
	role:                  Role,
}

impl Default for Ctx {
	fn default() -> Self {
		Self::new()
	}
}

impl Ctx {
	/// Context with defaults and the full link-time registry.
	#[must_use]
	pub fn new() -> Self {
		Self::builder().build()
	}

	/// Starts a builder.
	#[must_use]
	pub fn builder() -> CtxBuilder {
		CtxBuilder::default()
	}

	/// This context's replication role.
	#[must_use]
	pub const fn role(&self) -> Role {
		self.role
	}

	// ── registration ────────────────────────────────────────────────────

	/// Registers one static item.
	pub fn register(&self, item: RegItem) -> ConResult<()> {
		let initial = match item {
			RegItem::Var(spec) => (spec.default)(),
			_ => Value::Bool(false),
		};
		let key = fold_name(item.name());
		let mut names = self.names.write();
		if names.contains_key(key.as_str()) {
			return Err(ConError::Duplicate { name: key });
		}
		let idx = self
			.items
			.push(Item { spec: item, state: ItemState::new(initial) });
		names.insert(key, idx as u32);
		Ok(())
	}

	/// D1/D2: registers an owned dynamic variable in the shared namespace.
	pub fn register_dynamic_var(&self, mut spec: DynamicVarSpec) -> ConResult<()> {
		let key = fold_name(&spec.name);
		spec.name = key.clone();
		let mut names = self.names.write();
		if names.contains_key(key.as_str()) {
			return Err(ConError::Duplicate { name: key });
		}
		if !spec.ty.conforms(&spec.default) {
			return Err(ConError::TypeMismatch {
				name:     key,
				expected: spec.ty.kind,
				got:      spec.default.to_str(),
			});
		}
		let initial = spec.default.clone();
		let idx = self
			.dynamic_vars
			.push(DynamicVar { spec, state: ItemState::new(initial) });
		let idx = u32::try_from(idx).expect("dynamic console variable index overflow");
		assert_eq!(idx & !DYNAMIC_INDEX, 0, "dynamic console variable index overflow");
		names.insert(key, DYNAMIC_VAR | idx);
		Ok(())
	}

	/// D1/D2: registers an owned dynamic command in the shared namespace.
	pub fn register_dynamic_cmd(&self, mut spec: DynamicCmdSpec) -> ConResult<()> {
		let key = fold_name(&spec.name);
		spec.name = key.clone();
		let mut names = self.names.write();
		if names.contains_key(key.as_str()) {
			return Err(ConError::Duplicate { name: key });
		}
		let idx = self.dynamic_cmds.push(spec);
		let idx = u32::try_from(idx).expect("dynamic console command index overflow");
		assert_eq!(idx & !DYNAMIC_INDEX, 0, "dynamic console command index overflow");
		names.insert(key, DYNAMIC_CMD | idx);
		Ok(())
	}

	/// Registers a host observer for committed variable changes.
	pub fn observe(&self, observer: impl Fn(&str, &Value, &Value) + Send + Sync + 'static) {
		self.observers.write().push(Box::new(observer));
	}

	/// Number of statically registered items.
	pub(crate) fn item_count(&self) -> usize {
		self.items.len()
	}

	/// Dynamic variable snapshots for D10 persistence.
	pub(crate) fn dynamic_vars_for_dump(
		&self,
	) -> impl Iterator<Item = (&str, VarFlags, Value, Value)> + '_ {
		self.dynamic_vars.iter().map(|item| {
			(item.spec.name.as_str(), item.spec.flags, item.spec.default.clone(), item.state.value())
		})
	}

	/// Static item spec and state by registration index.
	pub(crate) fn item_at(&self, idx: usize) -> Option<(RegItem, &ItemState)> {
		self.items.get(idx).map(|item| (item.spec, &item.state))
	}

	/// All statically registered items in registration order.
	pub fn items(&self) -> impl Iterator<Item = RegItem> + '_ {
		self.items.iter().map(|item| item.spec)
	}

	/// Resolves a static name to its registration.
	#[must_use]
	pub fn find(&self, name: &str) -> Option<RegItem> {
		let idx = self.lookup(name)?;
		if idx & (DYNAMIC_VAR | DYNAMIC_CMD) != 0 {
			return None;
		}
		self.items.get(idx as usize).map(|item| item.spec)
	}

	fn lookup(&self, name: &str) -> Option<u32> {
		let names = self.names.read();
		if let Some(&idx) = names.get(name) {
			return Some(idx);
		}
		if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
			return names.get(name.to_ascii_lowercase().as_str()).copied();
		}
		None
	}

	fn var(&self, name: &str) -> ConResult<VarRef<'_>> {
		let idx = self
			.lookup(name)
			.ok_or_else(|| ConError::Unknown { name: name.to_str() })?;
		if idx & DYNAMIC_VAR != 0 {
			let item = self
				.dynamic_vars
				.get((idx & DYNAMIC_INDEX) as usize)
				.expect("index from name table");
			return Ok(VarRef {
				name:      item.spec.name.as_str(),
				ty:        item.spec.ty,
				min:       None,
				max:       None,
				flags:     item.spec.flags,
				on_change: None,
				validate:  None,
				state:     &item.state,
			});
		}
		if idx & DYNAMIC_CMD != 0 {
			return Err(ConError::NotAVar { name: name.to_str() });
		}
		let item = self.items.get(idx as usize).expect("index from name table");
		match item.spec {
			RegItem::Var(spec) => Ok(VarRef {
				name:      spec.name,
				ty:        spec.ty,
				min:       spec.min,
				max:       spec.max,
				flags:     spec.flags,
				on_change: spec.on_change,
				validate:  spec.validate,
				state:     &item.state,
			}),
			_ => Err(ConError::NotAVar { name: name.to_str() }),
		}
	}

	// ── values ──────────────────────────────────────────────────────────

	/// Current dynamic value of a variable.
	pub fn value(&self, name: &str) -> ConResult<Value> {
		Ok(self.var(name)?.state.value())
	}

	/// Current typed value of a variable.
	pub fn get_typed<T: crate::ConType>(&self, name: &str) -> ConResult<T> {
		let var = self.var(name)?;
		let value = var.state.value();
		T::from_value(&value).ok_or_else(|| ConError::TypeMismatch {
			name:     var.name.to_str(),
			expected: T::SPEC.kind,
			got:      value.to_str(),
		})
	}

	/// Returns a snapshot of the effective value.
	#[must_use]
	pub fn get(&self, name: &str) -> Option<Value> {
		self.value(name).ok()
	}

	/// Sets a variable from trusted host code.
	pub fn set_typed<T: crate::ConType>(&self, name: &str, value: T) -> ConResult<SetReport> {
		self.set(name, value.into_value(), Origin::Host)
	}

	/// Commits a dynamic value to its declared layer.
	pub fn set(&self, name: &str, value: Value, origin: Origin) -> ConResult<SetReport> {
		let var = self.var(name)?;
		self.commit_layer(var, value, origin)
	}

	/// Sets a variable from a dynamic value with explicit provenance.
	pub fn set_value(&self, name: &str, value: Value, source: SetSource) -> ConResult<()> {
		let origin = match source {
			SetSource::Code | SetSource::Replication => Origin::Host,
			SetSource::Script => Origin::Script(Str::new_static("console")),
		};
		let var = self.var(name)?;
		self
			.commit_layer_source(var, value, origin, source)
			.map(|_| ())
	}

	/// Restores a variable to its registration-time default.
	pub fn reset(&self, name: &str) -> ConResult<()> {
		let var = self.var(name)?;
		let default = var.state.default_value().clone();
		self.set(var.name, default, Origin::Host).map(|_| ())
	}

	/// Whether unsafe-gated writes are currently allowed.
	#[must_use]
	pub fn unsafe_enabled(&self) -> bool {
		self.get_typed::<bool>(UNSAFE_NAME).unwrap_or(false)
	}

	/// Pushes an engagement layer. Later layers have higher precedence.
	pub fn push_layer(&self, owner: Str, binds: &[(Str, Value)]) -> LayerId {
		for (name, value) in binds {
			let var = self
				.var(name.as_str())
				.expect("engagement bind names a registered variable");
			self
				.check_value(&var, value.clone(), SetSource::Code)
				.expect("engagement bind has the variable's declared type");
		}
		let id = self.layers.write().push(owner, binds);
		for (name, _) in binds {
			self.refresh(name.as_str(), SetSource::Code);
		}
		id
	}

	/// Removes one engagement layer without disturbing any other layer.
	pub fn pop_layer(&self, id: LayerId) {
		let Some(layer) = self.layers.write().pop(id) else {
			return;
		};
		for name in layer.values.keys() {
			self.refresh(name.as_str(), SetSource::Code);
		}
	}

	/// Captures effective values of variables declared for child inheritance.
	#[must_use]
	pub fn seed_child(&self) -> Seed {
		let mut values = FastHashMap::default();
		for item in self.items.iter() {
			if let RegItem::Var(spec) = item.spec
				&& spec.flags.contains(VarFlags::INHERIT)
			{
				values.insert(Str::new_static(spec.name), item.state.value());
			}
		}
		for item in self.dynamic_vars.iter() {
			if item.spec.flags.contains(VarFlags::INHERIT) {
				values.insert(item.spec.name.clone(), item.state.value());
			}
		}
		Seed::new(values)
	}

	/// Returns a snapshot iterator of writes owned by the session layer.
	pub fn session_writes(&self) -> impl Iterator<Item = (Str, Value)> {
		self
			.layers
			.read()
			.session
			.iter()
			.filter(|(name, _)| {
				self
					.var(name.as_str())
					.is_ok_and(|var| var.flags.contains(VarFlags::SESSION))
			})
			.map(|(name, value)| (name.clone(), value.clone()))
			.collect::<Vec<_>>()
			.into_iter()
	}

	fn refresh(&self, name: &str, source: SetSource) {
		let var = self
			.var(name)
			.expect("layer references a registered variable");
		let effective = {
			let layers = self.layers.read();
			layers
				.engagement_value(name)
				.or_else(|| layers.session.get(name))
				.or_else(|| layers.archive.get(name))
				.cloned()
				.unwrap_or_else(|| var.state.default_value().clone())
		};
		self.apply_effective(&var, effective, source);
	}

	fn commit_layer(&self, var: VarRef<'_>, value: Value, origin: Origin) -> ConResult<SetReport> {
		let source = if matches!(origin, Origin::Script(_)) {
			SetSource::Script
		} else {
			SetSource::Code
		};
		self.commit_layer_source(var, value, origin, source)
	}

	fn commit_layer_source(
		&self,
		var: VarRef<'_>,
		value: Value,
		origin: Origin,
		source: SetSource,
	) -> ConResult<SetReport> {
		let value = self.check_value(&var, value, source)?;
		let name = var.name.to_str();
		let (committed_to, shadowed_by, effective) = {
			let mut layers = self.layers.write();
			let committed_to = match origin {
				Origin::Archive => {
					layers.archive.insert(name.clone(), value);
					Origin::Archive
				},
				Origin::Default => {
					layers.archive.remove(name.as_str());
					layers.session.remove(name.as_str());
					Origin::Default
				},
				Origin::Engagement(_) => Origin::Session,
				Origin::Session | Origin::Script(_) | Origin::Host => {
					layers.session.insert(name.clone(), value);
					Origin::Session
				},
			};
			let shadowed_by = layers.shadow(name.as_str());
			let effective = layers
				.engagement_value(name.as_str())
				.or_else(|| layers.session.get(name.as_str()))
				.or_else(|| layers.archive.get(name.as_str()))
				.cloned()
				.unwrap_or_else(|| var.state.default_value().clone());
			(committed_to, shadowed_by, effective)
		};
		self.apply_effective(&var, effective, source);
		Ok(SetReport { committed_to, shadowed_by })
	}

	fn check_value(&self, var: &VarRef<'_>, value: Value, source: SetSource) -> ConResult<Value> {
		let name = || var.name.to_str();
		if !var.ty.conforms(&value) {
			return Err(match var.ty.kind {
				ValueKind::Enum => ConError::InvalidVariant { name: name(), got: value.to_str() },
				kind => {
					ConError::TypeMismatch { name: name(), expected: kind, got: value.to_str() }
				},
			});
		}
		let value = clamp(var.min, var.max, value);
		if var.flags.contains(VarFlags::REPLICATED)
			&& self.role == Role::Replica
			&& source != SetSource::Replication
		{
			return Err(ConError::ReplicatedWrite { name: name() });
		}
		if source == SetSource::Script {
			if var.flags.contains(VarFlags::READONLY) {
				return Err(ConError::ReadOnly { name: name() });
			}
			if var.flags.contains(VarFlags::UNSAFE) && !self.unsafe_enabled() {
				return Err(ConError::UnsafeGated { name: name() });
			}
		}
		if source != SetSource::Replication
			&& let Some(validate) = var.validate
		{
			validate(self, &value).map_err(|_| ConError::Invalid { name: name() })?;
		}
		Ok(value)
	}

	fn apply_effective(&self, var: &VarRef<'_>, value: Value, source: SetSource) {
		let old = {
			let mut slot = var.state.value.write();
			if *slot == value {
				return;
			}
			std::mem::replace(&mut *slot, value.clone())
		};
		if var.flags.contains(VarFlags::REPLICATED)
			&& self.role == Role::Authority
			&& source != SetSource::Replication
		{
			var.state.dirty.store(true, Ordering::Release);
		}
		if var.flags.contains(VarFlags::NOTIFY) {
			self.reply_fmt(Severity::Info, format_args!("{} = {value}", var.name));
		}
		if let Some(on_change) = var.on_change {
			on_change(self, &old, &value);
		}
		for observer in &*self.observers.read() {
			observer(var.name, &old, &value);
		}
	}

	// ── execution ───────────────────────────────────────────────────────

	/// Executes one interactive command stream strictly.
	pub fn run(&self, src: &str) -> ConResult<Output> {
		self
			.eval(&src.to_str(), false, &Origin::Script(Str::new_static("console")))
			.map(|_| Output { line: 1 })
	}

	/// Executes a command stream strictly with explicit provenance.
	pub fn exec(&self, src: &str, source: Source) -> ConResult<Vec<Output>> {
		let origin = source.origin();
		let outcome = self.eval(&src.to_str(), false, &origin)?;
		Ok((0..outcome.ran)
			.map(|index| Output { line: u32::try_from(index + 1).unwrap_or(u32::MAX) })
			.collect())
	}

	/// Executes a script leniently: failures are reported and execution
	/// continues.
	pub fn exec_lenient(&self, src: impl IntoStr) -> ExecOutcome {
		self
			.eval(&src.to_str(), true, &Origin::Script(Str::new_static("console")))
			.unwrap_or_else(|_| unreachable!("lenient eval never errors"))
	}

	/// Loads `config.cfg`, then child and agent-class cfgs in fixed order.
	///
	/// Cfg files are user data written by older builds: every statement runs
	/// leniently, so a stale or unknown name is reported through the sink and
	/// skipped instead of aborting startup. The aggregate outcome counts the
	/// skipped statements.
	pub fn exec_configs(&self, loader: &dyn CfgLoader, agent: Option<&str>) -> ExecOutcome {
		let mut total = ExecOutcome::default();
		let mut run = |src: Str, source: Source| {
			let outcome = self
				.eval(&src, true, &source.origin())
				.unwrap_or_else(|_| unreachable!("lenient eval never errors"));
			total.ran += outcome.ran;
			total.failed += outcome.failed;
		};
		if let Some(src) = loader.load("config.cfg") {
			run(src, Source::Config(Str::new_static("config.cfg")));
		}
		if let Some(agent) = agent {
			if let Some(src) = loader.load("subagent.cfg") {
				run(src, Source::Subagent);
			}
			let mut name = StrMut::new(agent);
			name.push_str(".cfg");
			let name = name.freeze();
			if let Some(src) = loader.load(name.as_str()) {
				run(src, Source::Agent(agent.to_str()));
			}
		}
		total
	}

	/// Restores one `<meta><con><var>` value into the session layer.
	pub fn restore_session_write(&self, name: &str, value: &str) -> ConResult<()> {
		self.var(name)?;
		let mut script = StrMut::new(name);
		script.push(' ');
		script.push_str(value);
		self.exec(script.as_str(), Source::Session).map(|_| ())
	}

	/// Writes the current replayable archive diff through `saver`.
	pub fn write_cfg(&self, saver: &dyn CfgSaver, name: &str) -> ConResult<()> {
		let contents = self.dump();
		saver.save(name, contents.as_str())
	}

	/// Loads and applies arbitrary named startup scripts in declaration order.
	pub fn exec_named_configs<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> ExecOutcome {
		let mut total = ExecOutcome::default();
		for name in names {
			let name = name.to_str();
			let Some(loader) = self.loader.as_deref() else {
				total.failed += 1;
				self.reply_fmt(
					Severity::Error,
					format_args!("no config loader installed; cannot exec `{name}`"),
				);
				continue;
			};
			let Some(src) = loader(name.as_str()) else {
				total.failed += 1;
				self.reply_fmt(Severity::Error, format_args!("config `{name}` not found"));
				continue;
			};
			match self.with_depth(&name, |ctx| ctx.eval(&src, true, &Origin::Script(name.clone()))) {
				Ok(outcome) => {
					total.ran += outcome.ran;
					total.failed += outcome.failed;
				},
				Err(error) => {
					total.failed += 1;
					self.reply_fmt(Severity::Error, format_args!("{error}"));
				},
			}
		}
		total
	}

	fn eval(&self, src: &Str, lenient: bool, origin: &Origin) -> ConResult<ExecOutcome> {
		let stmts = match script::parse(src) {
			Ok(stmts) => stmts,
			Err(err) if lenient => {
				self.reply_fmt(Severity::Error, format_args!("{err}"));
				return Ok(ExecOutcome { ran: 0, failed: 1 });
			},
			Err(err) => return Err(err.into()),
		};
		let mut outcome = ExecOutcome::default();
		for stmt in &stmts {
			match self.dispatch(stmt, lenient, origin) {
				Ok(()) => outcome.ran += 1,
				Err(err) if lenient => {
					outcome.failed += 1;
					self.reply_fmt(Severity::Error, format_args!("line {}: {err}", stmt.line));
				},
				Err(err) => return Err(err),
			}
		}
		Ok(outcome)
	}

	fn dispatch(&self, stmt: &Statement, lenient: bool, origin: &Origin) -> ConResult<()> {
		let Some(name) = stmt.args[0].as_atom() else {
			return Err(ParseError::BadName { line: stmt.line }.into());
		};
		let alias = self.aliases.read().get(name.as_str()).cloned();
		if let Some(body) = alias {
			return self.with_depth(name, |ctx| ctx.eval(&body, lenient, origin).map(|_| ()));
		}
		if let Some(base) = name.as_str().strip_prefix('+') {
			return self.dispatch_action(base, true);
		}
		if let Some(base) = name.as_str().strip_prefix('-') {
			return self.dispatch_action(base, false);
		}
		if let Some(idx) = self.lookup(name.as_str()) {
			if idx & DYNAMIC_VAR != 0 {
				let item = self
					.dynamic_vars
					.get((idx & DYNAMIC_INDEX) as usize)
					.expect("index from name table");
				return self.dispatch_dynamic_var(item, &stmt.args[1..], origin);
			}
			if idx & DYNAMIC_CMD != 0 {
				let item = self
					.dynamic_cmds
					.get((idx & DYNAMIC_INDEX) as usize)
					.expect("index from name table");
				return (item.handler)(self, item.name.as_str(), &stmt.args[1..]);
			}
			let item = self.items.get(idx as usize).expect("index from name table");
			return match item.spec {
				RegItem::Var(spec) => self.dispatch_var(spec, &item.state, &stmt.args[1..], origin),
				RegItem::Cmd(spec) => {
					let args = Args { cmd: spec.name, spec: spec.args, values: &stmt.args[1..] };
					for (i, arg) in spec.args.iter().enumerate() {
						if arg.required && i >= args.len() {
							return Err(ConError::MissingArg {
								cmd: Str::new_static(spec.name),
								arg: Str::new_static(arg.name),
							});
						}
					}
					(spec.handler)(self, &args)
				},
				RegItem::Action(spec) => {
					self.reply_fmt(
						Severity::Info,
						format_args!("`{0}` is an action; use `+{0}` / `-{0}`", spec.name),
					);
					Ok(())
				},
			};
		}
		Err(ConError::Unknown { name: name.clone() })
	}

	fn dispatch_dynamic_var(
		&self,
		item: &DynamicVar,
		args: &[Arg],
		origin: &Origin,
	) -> ConResult<()> {
		if args.is_empty() {
			let value = item.state.value();
			self.reply_fmt(
				Severity::Info,
				format_args!(
					"{} = {value} (default {}) — {}",
					item.spec.name, item.spec.default, item.spec.desc
				),
			);
			return Ok(());
		}
		let value = script::coerce_set_args(args, item.spec.ty)
			.map_err(|issue| issue_error_str(item.spec.name.clone(), issue))?;
		self
			.commit_layer(
				VarRef {
					name:      item.spec.name.as_str(),
					ty:        item.spec.ty,
					min:       None,
					max:       None,
					flags:     item.spec.flags,
					on_change: None,
					validate:  None,
					state:     &item.state,
				},
				value,
				origin.clone(),
			)
			.map(|_| ())
	}

	fn dispatch_var(
		&self,
		spec: &'static VarSpec,
		state: &ItemState,
		args: &[Arg],
		origin: &Origin,
	) -> ConResult<()> {
		if args.is_empty() {
			let value = state.value();
			let mut line = StrMut::new("");
			let _ = write!(line, "{} = {value} (default {})", spec.name, (spec.default)());
			if let Some(desc) = spec.desc.trim().lines().next()
				&& !desc.is_empty()
			{
				let _ = write!(line, " — {}", desc.trim());
			}
			self.reply(Severity::Info, line.as_str());
			return Ok(());
		}
		let value =
			script::coerce_set_args(args, spec.ty).map_err(|issue| issue_error(spec.name, issue))?;
		self
			.commit_layer(
				VarRef {
					name: spec.name,
					ty: spec.ty,
					min: spec.min,
					max: spec.max,
					flags: spec.flags,
					on_change: spec.on_change,
					validate: spec.validate,
					state,
				},
				value,
				origin.clone(),
			)
			.map(|_| ())
	}

	fn dispatch_action(&self, base: &str, pressed: bool) -> ConResult<()> {
		let idx = self
			.lookup(base)
			.ok_or_else(|| ConError::Unknown { name: base.to_str() })?;
		let item = self.items.get(idx as usize).expect("index from name table");
		let RegItem::Action(spec) = item.spec else {
			return Err(ConError::Unknown { name: base.to_str() });
		};
		let presses = &item.state.presses;
		if pressed {
			if presses.fetch_add(1, Ordering::AcqRel) == 0
				&& let Some(hook) = spec.on_press
			{
				hook(self);
			}
		} else {
			let prev = presses
				.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
				.unwrap_or(0);
			if prev == 1
				&& let Some(hook) = spec.on_release
			{
				hook(self);
			}
		}
		Ok(())
	}

	fn with_depth<R>(&self, name: &Str, f: impl FnOnce(&Self) -> ConResult<R>) -> ConResult<R> {
		let depth = self.depth.fetch_add(1, Ordering::AcqRel);
		let _guard = DepthGuard(&self.depth);
		if depth >= MAX_DEPTH {
			Err(ConError::Recursion { name: name.clone() })
		} else {
			f(self)
		}
	}

	/// Runs a named config through the installed loader (lenient).
	pub(crate) fn exec_cfg(&self, name: &Str) -> ConResult<()> {
		let loader = self
			.loader
			.as_deref()
			.ok_or_else(|| ConError::NoLoader { name: name.clone() })?;
		let src = loader(name.as_str()).ok_or_else(|| ConError::MissingCfg { name: name.clone() })?;
		self.with_depth(name, |ctx| {
			ctx.eval(&src, true, &Origin::Script(name.clone()))
				.map(|_| ())
		})
	}

	pub(crate) fn save_cfg(&self, name: &str, contents: &str) -> ConResult<()> {
		let saver = self.saver.as_deref().ok_or(ConError::NoSaver)?;
		saver(name, contents)
	}

	// ── aliases ─────────────────────────────────────────────────────────

	/// Defines an alias. The name must not shadow a registered item.
	pub fn set_alias(&self, name: impl IntoStr, body: impl IntoStr) -> ConResult<()> {
		let name = fold_name(&name.to_str());
		if self.lookup(name.as_str()).is_some() {
			return Err(ConError::Duplicate { name });
		}
		self.aliases.write().insert(name, body.to_str());
		Ok(())
	}

	/// Removes an alias; `true` when it existed.
	pub fn remove_alias(&self, name: &str) -> bool {
		self.aliases.write().remove(name).is_some()
	}

	/// Removes every alias.
	pub fn clear_aliases(&self) {
		self.aliases.write().clear();
	}

	/// Alias table snapshot, sorted by name.
	#[must_use]
	pub fn aliases(&self) -> Vec<(Str, Str)> {
		let mut out: Vec<_> = self
			.aliases
			.read()
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
		out
	}

	// ── binds ───────────────────────────────────────────────────────────

	/// Binds a key (case-insensitive, host-defined key names) to a script.
	pub fn bind(&self, key: impl IntoStr, script: impl IntoStr) -> ConResult<()> {
		let script = script.to_str();
		script::parse(&script)?;
		self.binds.write().insert(fold_name(&key.to_str()), script);
		Ok(())
	}

	/// Removes a bind; `true` when it existed.
	pub fn unbind(&self, key: &str) -> bool {
		self.binds.write().remove(key).is_some()
	}

	/// Removes every bind.
	pub fn unbind_all(&self) {
		self.binds.write().clear();
	}

	/// Marks the current bind table as the default baseline: the default
	/// bind cfg has run, and persistence ([`Ctx::dump`]) records only the
	/// user's divergence from it (`unbind` for removed defaults, `bind` for
	/// changed or added chords) instead of `unbindall` plus every bind.
	pub fn seal_bind_defaults(&self) {
		*self.bind_baseline.write() = Some(self.binds.read().clone());
	}

	/// The user's bind divergence from the sealed default baseline: removed
	/// default chords, then changed or added binds, each sorted by key.
	/// `None` when no baseline was sealed.
	#[must_use]
	pub fn bind_diff(&self) -> Option<(Vec<Str>, Vec<(Str, Str)>)> {
		let baseline = self.bind_baseline.read();
		let baseline = baseline.as_ref()?;
		let binds = self.binds.read();
		let mut removed: Vec<_> = baseline
			.keys()
			.filter(|key| !binds.contains_key(*key))
			.cloned()
			.collect();
		removed.sort_unstable();
		let mut changed: Vec<_> = binds
			.iter()
			.filter(|(key, script)| baseline.get(*key) != Some(*script))
			.map(|(key, script)| (key.clone(), script.clone()))
			.collect();
		changed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
		Some((removed, changed))
	}

	/// Bind table snapshot, sorted by key.
	#[must_use]
	pub fn binds(&self) -> Vec<(Str, Str)> {
		let mut out: Vec<_> = self
			.binds
			.read()
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
		out
	}

	/// Feeds a key edge. Press executes the bound script; release executes
	/// the `-` counterpart of every `+action` statement in it.
	pub fn key(&self, key: &str, pressed: bool) -> ConResult<Vec<Output>> {
		let script = {
			let binds = self.binds.read();
			match binds.get(key) {
				Some(script) => script.clone(),
				None if key.bytes().any(|byte| byte.is_ascii_uppercase()) => {
					let lowered = key.to_ascii_lowercase();
					match binds.get(lowered.as_str()) {
						Some(script) => script.clone(),
						None => return Ok(Vec::new()),
					}
				},
				None => return Ok(Vec::new()),
			}
		};
		if pressed {
			return self.exec(script.as_str(), Source::Console);
		}
		let stmts = script::parse(&script)?;
		let mut output = Vec::new();
		for stmt in &stmts {
			let Some(name) = stmt.args[0].as_atom() else {
				continue;
			};
			let Some(base) = name.as_str().strip_prefix('+') else {
				continue;
			};
			let mut release = StrMut::new("-");
			release.push_str(base);
			self.run(release.as_str())?;
			output.push(Output { line: stmt.line });
		}
		Ok(output)
	}

	// ── userdata ────────────────────────────────────────────────────────

	/// Stores a typed host object, replacing any previous `T`.
	pub fn insert_user<T: Send + Sync + 'static>(&self, value: T) {
		self.user.write().insert(TypeId::of::<T>(), Arc::new(value));
	}

	/// Fetches the host object of type `T`, if present.
	#[must_use]
	pub fn user<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
		let user = self.user.read();
		let any = user.get(&TypeId::of::<T>())?.clone();
		drop(user);
		Arc::downcast(any).ok()
	}

	// ── output ──────────────────────────────────────────────────────────

	/// Held-press count for an action (0 for unknown names).
	pub(crate) fn action_presses(&self, name: &str) -> u32 {
		self
			.lookup(name)
			.and_then(|idx| self.items.get(idx as usize))
			.map_or(0, |item| item.state.presses())
	}

	/// Emits a line through the reply sink.
	pub fn reply(&self, severity: Severity, text: &str) {
		if let Some(sink) = &self.sink {
			sink(severity, text);
		}
	}

	/// Formats and emits a line through the reply sink.
	pub(crate) fn reply_fmt(&self, severity: Severity, args: fmt::Arguments<'_>) {
		if let Some(sink) = &self.sink {
			let mut buf = StrMut::new("");
			let _ = buf.write_fmt(args);
			sink(severity, buf.as_str());
		}
	}
}

/// Command arguments as passed to a [`CmdHandler`](crate::CmdHandler).
pub struct Args<'a> {
	cmd:    &'static str,
	spec:   &'static [crate::ArgSpec],
	values: &'a [Arg],
}

impl<'a> Args<'a> {
	/// Canonical name of the dispatched static command.
	#[must_use]
	pub const fn command(&self) -> &'static str {
		self.cmd
	}

	/// Number of supplied arguments.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.values.len()
	}

	/// Whether no arguments were supplied.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.values.is_empty()
	}

	/// Raw parsed arguments.
	#[must_use]
	pub const fn raw(&self) -> &'a [Arg] {
		self.values
	}

	/// Atom text at `index`.
	pub fn atom(&self, index: usize) -> ConResult<Str> {
		match self.values.get(index) {
			Some(Arg::Atom(s)) => Ok(s.clone()),
			Some(arg) => Err(ConError::TypeMismatch {
				name:     self.label(index),
				expected: ValueKind::Str,
				got:      arg.to_script(),
			}),
			None => {
				Err(ConError::MissingArg { cmd: Str::new_static(self.cmd), arg: self.label(index) })
			},
		}
	}

	/// Typed argument at `index`; errors when absent.
	pub fn get<T: crate::ConType>(&self, index: usize) -> ConResult<T> {
		match self.try_index::<T>(index)? {
			Some(v) => Ok(v),
			None => {
				Err(ConError::MissingArg { cmd: Str::new_static(self.cmd), arg: self.label(index) })
			},
		}
	}

	/// Typed argument at `index`; `None` when absent.
	pub fn opt<T: crate::ConType>(&self, index: usize) -> ConResult<Option<T>> {
		self.try_index::<T>(index)
	}

	fn try_index<T: crate::ConType>(&self, index: usize) -> ConResult<Option<T>> {
		let Some(arg) = self.values.get(index) else {
			return Ok(None);
		};
		let value = script::coerce_one(arg, T::SPEC)
			.map_err(|issue| issue_error_str(self.label(index), issue))?;
		Ok(Some(T::from_value(&value).expect("coerced value conforms")))
	}

	/// Script-rendered join of arguments starting at `from` (for `echo`,
	/// bind/alias bodies).
	#[must_use]
	pub fn join(&self, from: usize) -> Str {
		let mut out = StrMut::new("");
		for (i, arg) in self.values.iter().skip(from).enumerate() {
			if i > 0 {
				out.push(' ');
			}
			match arg {
				// Bare join keeps atoms verbatim so `echo hello world` echoes cleanly.
				Arg::Atom(s) => out.push_str(s.as_str()),
				other => {
					let _ = write!(out, "{other}");
				},
			}
		}
		out.freeze()
	}

	fn label(&self, index: usize) -> Str {
		if let Some(arg) = self.spec.get(index) {
			Str::new_static(arg.name)
		} else {
			let mut out = StrMut::new("");
			let _ = write!(out, "#{index}");
			out.freeze()
		}
	}
}

fn issue_error(name: &'static str, issue: CoerceIssue) -> ConError {
	issue_error_str(Str::new_static(name), issue)
}

fn issue_error_str(name: Str, issue: CoerceIssue) -> ConError {
	match issue {
		CoerceIssue::Kind { expected, got } => ConError::TypeMismatch { name, expected, got },
		CoerceIssue::Variant { got } => ConError::InvalidVariant { name, got },
	}
}

/// Canonical (ASCII-lowercase) form of a name or key.
fn fold_name(name: &str) -> Str {
	if name.bytes().any(|b| b.is_ascii_uppercase()) {
		Str::from(name.to_ascii_lowercase().as_str())
	} else {
		name.to_str()
	}
}

/// Clamps numeric values into the supplied `[min, max]`.
fn clamp(min: Option<f64>, max: Option<f64>, value: Value) -> Value {
	match value {
		Value::Int(i) => {
			let mut v = i;
			if let Some(min) = min {
				v = v.max(min as i64);
			}
			if let Some(max) = max {
				v = v.min(max as i64);
			}
			Value::Int(v)
		},
		Value::Float(f) => {
			let mut v = f;
			if let Some(min) = min {
				v = v.max(min);
			}
			if let Some(max) = max {
				v = v.min(max);
			}
			Value::Float(v)
		},
		other => other,
	}
}

/// Renders a value for error payloads.
trait ValueToStr {
	fn to_str(&self) -> Str;
}

impl ValueToStr for Value {
	fn to_str(&self) -> Str {
		let mut out = StrMut::new("");
		let _ = write!(out, "{self}");
		out.freeze()
	}
}
