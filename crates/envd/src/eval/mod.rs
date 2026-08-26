//! Eval helper prelude and authenticated host bridge.

mod bridge;
mod process;
pub mod spawn;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub use bridge::{
	BridgeCapabilities, BridgeDispatcher, BridgeHost, install_python_bridge, install_python_prelude,
};
pub use bridge::{
	BridgeHostError, BridgeProgressSink, EvalSessionConfig, NoopBridgeProgress, PRELUDE_PREFIX,
	ParentBindingLease, ParentSessionHost, SessionBridgeHost,
};
pub(crate) use bridge::{
	PRELUDE_PYTHON_KEYWORDS, PRELUDE_RESERVED_NAMES, PreludeHelper, PreludeInvoker,
	PreludeParamStub, PreludeTable,
};
pub use process::{EVAL_CHILD_ARG, ProcessEvalExec, run_eval_child_entry};

/// Python helpers installed once in every persistent eval namespace.
pub const PYTHON_PRELUDE: &str = include_str!("python_prelude.py");
