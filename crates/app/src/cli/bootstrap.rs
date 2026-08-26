//! Two-pass extension CLI bootstrap and prompt-safe contributed value parsing.

use std::{
	collections::BTreeMap,
	ffi::OsString,
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::Str;
use omp_ext::config::{
	CliCollision, CliContribution, CliContributionSet, CliValueKind, ContributedCliValue,
	ContributedValue,
};
use serde::Deserialize;
use thiserror::Error;
use toml::de;

/// Final bootstrap output passed to clap exactly once.
#[derive(Clone, Debug, Default)]
pub struct BootstrapOutput {
	/// Args with contributed flags and values removed.
	pub arguments: Vec<OsString>,
	/// Typed values grouped by qualified owner and declaration sink.
	pub values:    Vec<ContributedCliValue>,
}

/// Extension CLI bootstrap failure.
#[derive(Debug, Error)]
pub enum BootstrapError {
	/// Static contribution collision or malformed declaration.
	#[error(transparent)]
	Collision(#[from] CliCollision),
	/// A manifest could not be read.
	#[error("cannot read extension CLI manifest `{path}`")]
	Read {
		/// Manifest path.
		path:   PathBuf,
		/// Filesystem source.
		#[source]
		source: io::Error,
	},
	/// A manifest could not be decoded.
	#[error("cannot parse extension CLI manifest `{path}`")]
	Parse {
		/// Manifest path.
		path:   PathBuf,
		/// TOML source.
		#[source]
		source: de::Error,
	},
	/// A required extension value is absent.
	#[error("extension CLI flag `--{0}` requires a value")]
	MissingValue(Str),
}

#[derive(Deserialize)]
struct StaticManifest {
	#[serde(default)]
	cli: Vec<CliContribution>,
}

/// Discovers static contributions from explicit local extension roots and
/// reparses only their values, preventing prompt leakage.
pub fn run(
	arguments: Vec<OsString>,
	builtin_names: impl IntoIterator<Item = Str>,
) -> Result<BootstrapOutput, BootstrapError> {
	let manifests = explicit_manifest_paths(&arguments);
	let mut contributions = Vec::new();
	for path in manifests {
		let bytes = fs::read_to_string(&path)
			.map_err(|source| BootstrapError::Read { path: path.clone(), source })?;
		let manifest: StaticManifest =
			toml::from_str(&bytes).map_err(|source| BootstrapError::Parse { path, source })?;
		contributions.extend(manifest.cli);
	}
	let declarations = CliContributionSet::build(contributions, builtin_names)?;
	parse_contributed(arguments, &declarations)
}

fn explicit_manifest_paths(arguments: &[OsString]) -> Vec<PathBuf> {
	let mut paths = Vec::new();
	let mut index = 1;
	while index < arguments.len() {
		let text = arguments[index].to_string_lossy();
		let inline = [
			"--extension=",
			"--ext=",
			"--hook=",
			"--plugin-dir=",
			"--ext-only=",
			"--trusted-extension=",
		]
		.into_iter()
		.find_map(|prefix| text.strip_prefix(prefix));
		let value = if let Some(value) = inline {
			Some(PathBuf::from(value.strip_prefix("path:").unwrap_or(value)))
		} else if matches!(
			text.as_ref(),
			"--extension"
				| "--ext"
				| "--hook"
				| "-e" | "--plugin-dir"
				| "--ext-only"
				| "--trusted-extension"
		) {
			index += 1;
			arguments.get(index).map(|value| {
				let value = value.to_string_lossy();
				PathBuf::from(value.strip_prefix("path:").unwrap_or(value.as_ref()))
			})
		} else {
			None
		};
		if let Some(value) = value
			&& let Some(path) = manifest_path(&value)
		{
			paths.push(path);
		}
		index += 1;
	}
	paths.sort_unstable();
	paths.dedup();
	paths
}

fn manifest_path(root: &Path) -> Option<PathBuf> {
	if root.is_file() && root.extension().and_then(|value| value.to_str()) == Some("toml") {
		return Some(root.to_path_buf());
	}
	["omp-extension.toml", "extension.toml", "manifest.toml"]
		.into_iter()
		.map(|name| root.join(name))
		.find(|path| path.is_file())
}

fn parse_contributed(
	arguments: Vec<OsString>,
	declarations: &CliContributionSet,
) -> Result<BootstrapOutput, BootstrapError> {
	let mut output = Vec::with_capacity(arguments.len());
	let mut parsed = BTreeMap::<Str, ContributedCliValue>::new();
	if let Some(program) = arguments.first() {
		output.push(program.clone());
	}
	let mut index = 1;
	let mut positional = false;
	while index < arguments.len() {
		let argument = &arguments[index];
		let text = argument.to_string_lossy();
		if positional || text == "--" {
			positional = true;
			output.push(argument.clone());
			index += 1;
			continue;
		}
		let Some(flag) = text.strip_prefix("--") else {
			output.push(argument.clone());
			index += 1;
			continue;
		};
		let (name, inline) = flag
			.split_once('=')
			.map_or((flag, None), |(name, value)| (name, Some(value)));
		let Some(declaration) = declarations.get(name) else {
			output.push(argument.clone());
			index += 1;
			continue;
		};
		let value = match declaration.kind {
			CliValueKind::Boolean => ContributedValue::Boolean(true),
			CliValueKind::String => {
				let value = if let Some(value) = inline {
					value
				} else {
					index += 1;
					arguments
						.get(index)
						.and_then(|value| value.to_str())
						.ok_or_else(|| BootstrapError::MissingValue(declaration.name.clone()))?
				};
				ContributedValue::String(Str::new(value))
			},
			CliValueKind::OptionalString => {
				if let Some(value) = inline {
					ContributedValue::String(Str::new(value))
				} else if arguments
					.get(index + 1)
					.is_some_and(|value| !value.to_string_lossy().starts_with('-'))
				{
					index += 1;
					ContributedValue::String(Str::new(arguments[index].to_string_lossy().as_ref()))
				} else {
					ContributedValue::Boolean(true)
				}
			},
		};
		parsed.insert(declaration.qualified_name(), ContributedCliValue {
			owner: declaration.qualified_name(),
			sink: declaration.sink.key.clone(),
			value,
		});
		index += 1;
	}
	Ok(BootstrapOutput { arguments: output, values: parsed.into_values().collect() })
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn all_three_kinds_are_typed_and_removed_from_prompt() {
		let declarations = CliContributionSet::build(
			[
				contribution("verbose", CliValueKind::Boolean),
				contribution("target", CliValueKind::String),
				contribution("label", CliValueKind::OptionalString),
			],
			Vec::<Str>::new(),
		)
		.expect("declarations");
		let parsed = parse_contributed(
			["omp", "--verbose", "--target=x", "--label", "prompt"]
				.map(OsString::from)
				.to_vec(),
			&declarations,
		)
		.expect("parse");
		assert_eq!(parsed.values.len(), 3);
		assert_eq!(parsed.arguments, ["omp"].map(OsString::from));
	}

	#[test]
	fn every_local_invocation_spelling_participates_in_manifest_bootstrap() {
		let directory = tempfile::tempdir().expect("extension root");
		let manifest = directory.path().join("omp-extension.toml");
		fs::write(&manifest, "").expect("manifest");
		for flag in ["--extension", "--ext", "--hook", "-e", "--plugin-dir", "--ext-only"] {
			let arguments =
				[OsString::from("omp"), OsString::from(flag), directory.path().as_os_str().to_owned()];
			assert_eq!(explicit_manifest_paths(&arguments), vec![manifest.clone()], "{flag}");
		}
		let inline = OsString::from(format!("--extension=path:{}", directory.path().display()));
		assert_eq!(explicit_manifest_paths(&[OsString::from("omp"), inline]), vec![manifest]);
	}

	#[test]
	fn explicit_extension_values_reach_the_typed_launch_output() {
		let directory = tempfile::tempdir().expect("extension root");
		fs::write(
			directory.path().join("omp-extension.toml"),
			r#"
[[cli]]
publisher = "publisher"
extension = "review"
name = "spawn-peer"
description = "Select peer"
kind = "string"
[cli.sink]
key = "peer"
"#,
		)
		.expect("manifest");
		let parsed = run(
			[
				OsString::from("omp"),
				OsString::from("--extension"),
				directory.path().as_os_str().to_owned(),
				OsString::from("--spawn-peer"),
				OsString::from("reviewer"),
				OsString::from("prompt"),
			]
			.to_vec(),
			Vec::<Str>::new(),
		)
		.expect("bootstrap");
		assert_eq!(parsed.values.len(), 1);
		assert_eq!(parsed.values[0].sink, "peer");
		assert_eq!(parsed.values[0].value, ContributedValue::String(sf!("reviewer")));
		assert_eq!(parsed.arguments, [
			OsString::from("omp"),
			OsString::from("--extension"),
			directory.path().as_os_str().to_owned(),
			OsString::from("prompt"),
		]);
	}

	fn contribution(name: &str, kind: CliValueKind) -> CliContribution {
		CliContribution {
			publisher: sf!("publisher"),
			extension: sf!("example"),
			name: Str::new(name),
			description: sf!("test"),
			kind,
			default: None,
			shadow_builtin: false,
			sink: omp_ext::config::CliValueSink { key: Str::new(name) },
		}
	}
}
