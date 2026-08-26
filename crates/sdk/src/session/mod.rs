//! Owned session options and callback-capable construction.

mod builder;
mod diagnostics;
mod handle;
mod options;

pub use builder::{
	ProductionCallbackBoundary, ProductionSessionComposition, ProductionSessionError,
	SessionBlueprint, SessionBuildError, SessionBuilder, SessionCreateError,
	WorkspaceRootDescriptor,
};
pub use diagnostics::{
	LaunchDiagnostic, LspSessionBinding, LspWarmupStatus, ModelCandidateState,
	ModelFallbackDiagnostic, ServiceTierDiagnostic, SessionDiagnostics, ThinkingDiagnostic,
};
pub use handle::{
	SessionHandle, SessionHandleError, SessionIdentity, SessionLifecycle,
	SessionLifecycleSubscription, SessionRevivalError, SessionRevivalFactory, SessionRevivalFuture,
	SessionRevivalRequest, SessionRuntime,
};
pub use options::{
	AgentIdentity, DiscoveryPolicy, SessionOptions, SessionPolicies, SubsystemToggles,
	ThinkingCeiling,
};
