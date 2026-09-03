//! Command-stream configuration migration and persistence contracts.

use std::fs;

use omp_app::{
	cli::ConfigScope,
	config_cmd::{migrate_settings, set_persisted},
};

#[test]
fn config_migrate_is_idempotent_and_maps_every_schema_key() {
	let registry = omp_con::Ctx::new();
	for &(legacy, name) in [
		omp_catalog::settings::LEGACY_CONVAR_MAPPINGS,
		omp_inference::settings::LEGACY_CONVAR_MAPPINGS,
		omp_tools::settings::LEGACY_CONVAR_MAPPINGS,
		omp_envd::LEGACY_CONVAR_MAPPINGS,
		omp_driver::settings::LEGACY_CONVAR_MAPPINGS,
		omp_app::voice::settings::LEGACY_CONVAR_MAPPINGS,
	]
	.into_iter()
	.flatten()
	{
		assert!(
			matches!(registry.find(name), Some(omp_con::RegItem::Var(_))),
			"legacy key {legacy} maps to missing convar {name}",
		);
	}
	let data = tempfile::tempdir().expect("data directory");
	let config = tempfile::tempdir().expect("config directory");
	// SAFETY: nextest runs each test in its own process; nothing else reads the
	// variable concurrently.
	unsafe { std::env::set_var("OMP_CONFIG_DIR", config.path()) };
	let project = tempfile::tempdir().expect("project directory");
	fs::write(data.path().join("config.toml"), "[stt]\nenabled = true\nmodelName = \"turbo\"\n")
		.expect("legacy TOML");

	let path = migrate_settings(data.path(), project.path()).expect("first migration");
	let first = fs::read(&path).expect("first config.cfg");
	migrate_settings(data.path(), project.path()).expect("second migration");
	let second = fs::read(&path).expect("second config.cfg");

	assert_eq!(second, first);
	let script = String::from_utf8(first).expect("UTF-8 cfg");
	assert!(script.contains("cl_voice_stt_enabled true"));
	assert!(script.contains("cl_stt_model turbo"));
}

#[test]
fn config_set_persists_and_get_reads_back() {
	let config = tempfile::tempdir().expect("config directory");
	// SAFETY: see above.
	unsafe { std::env::set_var("OMP_CONFIG_DIR", config.path()) };
	let project = tempfile::tempdir().expect("project directory");
	set_persisted(project.path(), ConfigScope::Global, "cl_showthinking", "false")
		.expect("set archived convar");

	let script = fs::read_to_string(config.path().join("config.cfg")).expect("config.cfg");
	assert!(script.contains("cl_showthinking false"));
	let ctx = omp_app::process_ctx(project.path()).expect("reload context");
	assert_eq!(ctx.get_typed::<bool>("cl_showthinking").expect("convar"), false);
}
