//! lintx — omp's custom lint engine for path/style rules.
//!
//! Rules:
//! - `long-path`: paths with more than two `::`, plus explicit name policy.
//! - `std-path`: `std::module::item` paths that should start at `module`.
//! - `tokio-path`: `tokio::module::item` paths, except `tokio::fs`.
//! - `relative-path`: inline `crate::`, `super::`, or `self::` paths.
//! - `import-alias`: CamelCase `use … as Alias` bindings.
//! - `arc-struct`: structs where most fields are `Arc`-wrapped.
//! - `mutex-arc`: locks around swappable `Arc` handles.
//! - `model-gate`: model-name literals gating behavior in `crates/inference`.
//! - `model-table`: hardcoded model-id arrays in `crates/inference`.
//!
//! `--fix` rewrites paths according to explicit bare/qualified-name policy and
//! inserts a `use` into the nearest enclosing module scope, iterating each file
//! to a fixpoint. `--only <rule>` restricts detection and fixes to one or more
//! named rules. Ambiguous cases stay diagnostics. Run `cargo fmt` afterwards.

mod bindings;
mod fix;
mod lint;
mod lints;
mod scope;

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf};

use lint::FileContext;
use walkdir::WalkDir;

#[derive(Default)]
struct Options {
	fix:          bool,
	max_segments: usize,
	only:         Vec<String>,
	paths:        Vec<PathBuf>,
}

fn main() {
	let mut opts = Options { max_segments: 3, ..Options::default() };
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--fix" => opts.fix = true,
			"--max-segments" => {
				opts.max_segments = args
					.next()
					.and_then(|value| value.parse().ok())
					.expect("--max-segments <N>");
			},
			"--only" => opts.only.push(args.next().expect("--only <rule>")),
			_ => opts.paths.push(PathBuf::from(arg)),
		}
	}
	if opts.paths.is_empty() {
		opts.paths.push(PathBuf::from("."));
	}

	let mut rules = lints::all(opts.max_segments);
	for requested in &opts.only {
		assert!(rules.iter().any(|rule| rule.name() == requested), "unknown lint rule `{requested}`");
	}
	if !opts.only.is_empty() {
		rules.retain(|rule| opts.only.iter().any(|requested| requested == rule.name()));
	}

	let mut totals: BTreeMap<&'static str, usize> = BTreeMap::new();
	let mut fixed_files = 0usize;
	let mut fixes_applied = 0usize;
	for root in &opts.paths {
		for entry in WalkDir::new(root)
			.into_iter()
			.filter_entry(|entry| {
				let name = entry.file_name().to_string_lossy();
				name != "target" && name != "vendor" && !name.starts_with('.')
			})
			.filter_map(Result::ok)
			.filter(|entry| entry.file_type().is_file())
			.filter(|entry| {
				entry
					.path()
					.extension()
					.is_some_and(|extension| extension == "rs")
			}) {
			let path = entry.path();
			let Ok(mut text) = std::fs::read_to_string(path) else {
				continue;
			};
			let own_crate = crate_name_for(path);
			// Nested fixes (a linted path inside another's generic args) are
			// deferred to the next pass; iterate to a fixpoint.
			let mut changed = false;
			for pass in 0..5 {
				let ctx = FileContext::new(path, &text);
				let mut diags = Vec::new();
				for rule in &rules {
					rule.detect_erased(&ctx, &mut |diag| {
						if pass == 0 {
							*totals.entry(diag.rule).or_default() += 1;
							println!("{}", diag.render(&ctx));
						}
						diags.push(diag);
					});
				}
				if !opts.fix {
					break;
				}
				let (new_text, applied) = fix::apply(&ctx, diags, &own_crate);
				if applied == 0 {
					break;
				}
				fixes_applied += applied;
				changed = true;
				drop(ctx);
				text = new_text;
			}
			if changed {
				std::fs::write(path, text).expect("write fixed file");
				fixed_files += 1;
			}
		}
	}
	let mut summary = String::new();
	for (rule, count) in &totals {
		let _ = write!(summary, "{rule}: {count}  ");
	}
	eprintln!("\n== {summary}");
	if opts.fix {
		eprintln!("== applied {fixes_applied} fixes across {fixed_files} files");
	}
}

/// `lib` name (hyphens normalized) of the crate containing `file`, from the
/// nearest ancestor Cargo.toml. The fixer imports self-referencing paths via
/// `crate::` instead.
///
/// Only lib-module files under the crate's `src/` qualify: integration
/// tests, benches, examples, and bin targets (`src/main.rs`, `src/bin/*`)
/// are separate crates where the lib is named, not `crate`.
fn crate_name_for(file: &std::path::Path) -> String {
	let mut dir = file.parent();
	while let Some(directory) = dir {
		if let Ok(manifest) = std::fs::read_to_string(directory.join("Cargo.toml")) {
			let is_lib_module = file.strip_prefix(directory).is_ok_and(|relative| {
				relative.starts_with("src")
					&& relative != std::path::Path::new("src/main.rs")
					&& !relative.starts_with("src/bin")
			});
			if !is_lib_module {
				return String::new();
			}
			if let Some(name) = manifest.lines().find_map(|line| {
				line
					.trim()
					.strip_prefix("name")
					.and_then(|rest| rest.trim().strip_prefix('='))
			}) {
				return name.trim().trim_matches('"').replace('-', "_");
			}
		}
		dir = directory.parent();
	}
	String::new()
}
