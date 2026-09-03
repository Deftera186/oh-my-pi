//! Console error taxonomy.

use omp_core::Str;

use crate::{Role, ValueKind};

/// Result alias used across the console.
pub type ConResult<T> = Result<T, ConError>;

/// Script-source parse failures.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ParseError {
	/// A `"` string never closed before end of input.
	#[error("unterminated string starting on line {line}")]
	UnterminatedString {
		/// 1-based source line of the opening quote.
		line: u32,
	},
	/// A `[` list literal never closed.
	#[error("unterminated list starting on line {line}")]
	UnterminatedList {
		/// 1-based source line of the opening bracket.
		line: u32,
	},
	/// A `{` key/value block never closed.
	#[error("unterminated kv block starting on line {line}")]
	UnterminatedKv {
		/// 1-based source line of the opening brace.
		line: u32,
	},
	/// A `]` or `}` with no matching opener.
	#[error("unexpected `{token}` on line {line}")]
	UnexpectedClose {
		/// 1-based source line of the stray token.
		line:  u32,
		/// The offending closer.
		token: char,
	},
	/// A kv block key position held a nested list/block instead of an atom.
	#[error("kv key on line {line} must be a word or quoted string")]
	KvKey {
		/// 1-based source line of the bad key.
		line: u32,
	},
	/// A statement began with a list/block instead of a name.
	#[error("statement on line {line} does not start with a name")]
	BadName {
		/// 1-based source line of the statement.
		line: u32,
	},
}

/// Any console-facing failure: dispatch, permission, typing, or parsing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConError {
	/// Name resolves to no var, command, action, or alias.
	#[error("unknown console name `{name}`")]
	Unknown {
		/// The name as written.
		name: Str,
	},
	/// Operation requires a variable but the name is a command/action.
	#[error("`{name}` is not a variable")]
	NotAVar {
		/// The resolved item name.
		name: Str,
	},
	/// Var carries `READONLY` and the write came from a script.
	#[error("`{name}` is read-only")]
	ReadOnly {
		/// The variable name.
		name: Str,
	},
	/// Var carries `UNSAFE` and the `sv_cheats` gate is disabled.
	#[error("`{name}` is unsafe-gated; requires `sv_cheats true`")]
	UnsafeGated {
		/// The variable name.
		name: Str,
	},
	/// Var is replicated from the authority; replicas cannot write it.
	#[error("`{name}` is replicated from the authority and cannot be set here")]
	ReplicatedWrite {
		/// The variable name.
		name: Str,
	},
	/// Supplied value does not parse/conform to the target's type.
	#[error("`{name}` expects {expected}, got `{got}`")]
	TypeMismatch {
		/// Target var or argument name.
		name:     Str,
		/// Expected shape.
		expected: ValueKind,
		/// Offending token, rendered.
		got:      Str,
	},
	/// Supplied value is not a declared enum variant.
	#[error("`{got}` is not a variant of `{name}`")]
	InvalidVariant {
		/// Target var or argument name.
		name: Str,
		/// Offending token.
		got:  Str,
	},
	/// A `validate` hook vetoed the write.
	#[error("`{name}` rejected the supplied value")]
	Invalid {
		/// The variable name.
		name: Str,
	},
	/// Required command argument absent.
	#[error("`{cmd}` missing required argument `{arg}`")]
	MissingArg {
		/// The command name.
		cmd: Str,
		/// Declared argument name (or index when undeclared).
		arg: Str,
	},
	/// Alias/`exec` nesting exceeded the recursion cap.
	#[error("recursion limit reached while expanding `{name}`")]
	Recursion {
		/// The alias/config that overflowed.
		name: Str,
	},
	/// `exec` invoked with no loader installed on the context.
	#[error("no config loader installed; cannot exec `{name}`")]
	NoLoader {
		/// Requested config name.
		name: Str,
	},
	/// `writecfg` invoked with no saver installed on the context.
	#[error("no config saver installed; cannot writecfg")]
	NoSaver,
	/// Loader had no config under this name.
	#[error("config `{name}` not found")]
	MissingCfg {
		/// Requested config name.
		name: Str,
	},
	/// Name registered twice (item or alias colliding with an item).
	#[error("`{name}` is already registered")]
	Duplicate {
		/// The colliding name.
		name: Str,
	},
	/// Replication API called on a context with the wrong [`Role`].
	#[error("operation not valid for replication role {role}")]
	RoleMismatch {
		/// The context's actual role.
		role: Role,
	},
	/// Script source failed to parse.
	#[error(transparent)]
	Parse(#[from] ParseError),
}
