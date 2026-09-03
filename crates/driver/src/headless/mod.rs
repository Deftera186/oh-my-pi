//! Journal-first non-interactive session assembly shared by app modes.

pub mod gateway;
pub mod kernel;

pub use kernel::{KernelOptions, compose_kernel};
use omp_core::Str;

/// Typed failure while composing a journal-first headless session.
#[derive(Debug, thiserror::Error)]
pub enum HeadlessError {
	/// Filesystem state could not be prepared or inspected.
	#[error("headless session filesystem operation failed")]
	Io(#[from] std::io::Error),
	/// The project environment could not attach.
	#[error("project environment attachment failed")]
	Environment(#[from] omp_envd::EnvdError),
	/// Frozen Python extension registrations were invalid.
	#[error("Python extension registration failed")]
	PythonExtension(#[from] omp_envd::exthost::PyExtensionError),
	/// Catalog, inference, or credential composition failed.
	#[error("production inference composition failed")]
	Registry(#[from] crate::registry::RegistryError),
	/// Journal-backed session creation or replay failed.
	#[error("session journal operation failed")]
	Session(#[from] omp_session::SessionError),
	/// Artifact storage could not be opened.
	#[error("artifact storage operation failed")]
	Blob(#[from] omp_journal::blob::Error),
	/// The requested model selector was not present in the catalog.
	#[error("unknown model `{selector}`")]
	UnknownModel {
		/// Requested selector.
		selector: Str,
	},
	/// Continue mode found no durable session for the project.
	#[error("no durable headless session exists for this project")]
	NoSession,
}
