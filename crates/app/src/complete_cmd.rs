//! Dynamic model and session candidates used by generated shell completions.

use std::{fs, path::Path};

use miette::IntoDiagnostic as _;
use omp_storage::index::{SessionFilter, SessionIndex};

/// Dynamic completion candidate class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum CompletionKind {
	/// Embedded and configured model identifiers.
	Models,
	/// Sessions in the current project's authoritative index.
	Sessions,
}

/// Emits tab-separated completion candidates.
pub fn run(kind: CompletionKind, prefix: &str) -> miette::Result<()> {
	match kind {
		CompletionKind::Models => models(prefix),
		CompletionKind::Sessions => sessions(prefix),
	}
}

fn models(prefix: &str) -> miette::Result<()> {
	let data = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let catalog =
		omp_driver::registry::production_catalog(&data).map_err(|error| miette::miette!(error))?;
	let needle = prefix.to_ascii_lowercase();
	let mut rows = Vec::new();
	for model in catalog.models() {
		let key = clean(model.key.as_str());
		if needle.is_empty() || key.to_ascii_lowercase().contains(&needle) {
			rows.push(format!("{}\t{}", key, clean(model.display_name.as_str())));
		}
	}
	rows.sort_unstable();
	rows.dedup();
	print_rows(&rows);
	Ok(())
}

fn sessions(prefix: &str) -> miette::Result<()> {
	let data = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let project = fs::canonicalize(".").into_diagnostic()?;
	let state = omp_env::project_state::directory(&data, &project).into_diagnostic()?;
	let path = state.join("sessions.sqlite3");
	if !Path::new(&path).is_file() {
		return Ok(());
	}
	let index = SessionIndex::open_authoritative_reader(path).into_diagnostic()?;
	let page = index
		.list(&SessionFilter { limit: 200, ..SessionFilter::default() })
		.into_diagnostic()?;
	let mut rows = page
		.sessions
		.into_iter()
		.filter(|session| prefix.is_empty() || session.id.0.as_str().starts_with(prefix))
		.map(|session| {
			let description = session.title.as_deref().unwrap_or(session.cwd.as_str());
			format!("{}\t{}", clean(session.id.0.as_str()), truncate(&clean(description), 72))
		})
		.collect::<Vec<_>>();
	rows.sort_unstable();
	print_rows(&rows);
	Ok(())
}

fn clean(value: &str) -> String {
	value
		.chars()
		.map(|character| {
			if matches!(character, '\t' | '\r' | '\n' | '\0') {
				' '
			} else {
				character
			}
		})
		.collect::<String>()
		.trim()
		.to_owned()
}

fn truncate(value: &str, max: usize) -> &str {
	if value.len() <= max {
		return value;
	}
	let mut end = max;
	while !value.is_char_boundary(end) {
		end -= 1;
	}
	&value[..end]
}

fn print_rows(rows: &[String]) {
	if !rows.is_empty() {
		println!("{}", rows.join("\n"));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn line_protocol_is_control_character_safe() {
		assert_eq!(clean("title\twith\ncontrols\0"), "title with controls");
		assert_eq!(truncate("abcdefgh", 4), "abcd");
	}
}
