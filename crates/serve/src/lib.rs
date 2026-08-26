//! gRPC transport projections serving OMP inference, auth, and blob services.

pub mod auth;
pub mod blob;
pub mod inference;

pub use auth::{AuthRpc, AuthenticatedRevealContext};
pub use blob::BlobRpc;
pub use inference::InferenceRpc;
