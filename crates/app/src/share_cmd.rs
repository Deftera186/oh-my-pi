//! Standalone encrypted transcript sharing over the production HTTP store.

use std::{fs, iter, sync::Arc};

use miette::{IntoDiagnostic as _, miette};
use omp_driver::{
	export::SessionTree,
	secrets::session::SecretSessionSnapshot,
	settings::{ExportSettings, ShareStore},
	share::{
		DirectShareStore, HTTP_MAX_SEALED_BYTES, ShareProjection, ShareStoreKind, seal, upload,
	},
};
use omp_envd::github_url::GithubCredentialBridge;
use omp_storage::transcript::reader;

use crate::{cli::ShareArgs, pickers};

/// Selects a live journal projection, irreversibly redacts it, seals it, and
/// uploads only ciphertext to the configured share store.
pub async fn run(args: ShareArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let journal = match args.journal {
		Some(path) => path,
		None => {
			let selection = pickers::pick_session(&data_dir, None)
				.await
				.map_err(|error| miette!("{error}"))?
				.ok_or_else(|| miette!("no session selected"))?;
			selection
				.sessions_dir
				.join(format!("{}.jsonl", selection.session.id.0))
		},
	};
	let tree = SessionTree::load(&journal).map_err(|error| miette!("{error}"))?;
	let value = serde_json::to_value(tree).into_diagnostic()?;
	let project = session_project(&journal)?;
	let configured = omp_driver::settings::current_for_project(&data_dir, &project)
		.map_err(|error| miette!("{error}"))?;
	let secrets = SecretSessionSnapshot::build(
		0,
		&data_dir.join("secrets.toml"),
		&project.join(".omp/secrets.toml"),
		iter::empty(),
	)
	.map_err(|error| miette!("{error}"))?;
	let projection = ShareProjection::materialize_bounded(
		value,
		ExportSettings {
			share_redact_secrets: configured.export.share_redact_secrets && !args.no_redact,
		},
		&secrets,
		HTTP_MAX_SEALED_BYTES.saturating_sub(64 * 1024),
	);
	let sealed = seal(&projection).map_err(|error| miette!("{error}"))?;
	let server = args.server.as_ref().unwrap_or(&configured.share.server_url);
	let credentials = Arc::new(GithubCredentialBridge::new());
	let authority = Arc::new(omp_driver::auth_backend::github_authority(
		omp_driver::registry::open_credential_store(data_dir.join("credentials.db"))
			.into_diagnostic()?,
	));
	credentials
		.bind(authority)
		.map_err(|_| miette!("GitHub credential authority is already bound"))?;
	let store =
		DirectShareStore::new(server.as_str(), credentials).map_err(|error| miette!("{error}"))?;
	let result = upload(
		&store,
		match configured.share.store {
			ShareStore::Http => ShareStoreKind::Http,
			ShareStore::Gist => ShareStoreKind::Gist,
		},
		&sealed,
		args.viewer.as_str(),
	)
	.await
	.map_err(|error| miette!("{error}"))?;
	println!("{}", result.url);
	Ok(())
}
fn session_project(journal: &std::path::Path) -> miette::Result<std::path::PathBuf> {
	let journal = fs::canonicalize(journal).into_diagnostic()?;
	let log = reader::load(&journal).map_err(|error| miette!("{error}"))?;
	let project = fs::canonicalize(&log.header().cwd).map_err(|error| {
		miette!("cannot resolve shared session project `{}`: {error}", log.header().cwd.display())
	})?;
	if !project.is_dir() {
		return Err(miette!("shared session project `{}` is not a directory", project.display()));
	}
	Ok(project)
}
#[cfg(test)]
mod tests {
	use omp_core::sf;
	use omp_storage::transcript::{SessionId, codec};

	use super::*;

	#[test]
	fn selected_journal_owns_share_project_policy() {
		let directory = tempfile::tempdir().expect("project");
		let journal = directory.path().join("session.jsonl");
		let mut bytes = Vec::new();
		codec::write_header(
			&codec::Header {
				v:       4,
				id:      SessionId(sf!("01ARZ3NDEKTSV4RRFFQ69G5FAV")),
				created: 1,
				cwd:     directory.path().to_path_buf(),
			},
			&mut bytes,
		)
		.expect("header");
		bytes.push(b'\n');
		fs::write(&journal, bytes).expect("journal");
		assert_eq!(
			session_project(&journal).expect("session project"),
			directory.path().canonicalize().expect("canonical project")
		);
	}
}
