//! AI-assisted selective staging for the Git workbench.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::Path,
};

use futures::{StreamExt as _, stream};
use omp_chat_ui::git::{GitChangeKind, GitFileRow};
use omp_core::{IntoStr as _, Str, sf};
use omp_driver::commit::{CommitError, CommitGenerator};
use omp_vcs::{
	git::GitRepo,
	types::{DiffOptions, HunkSelection, HunkSpec},
};
use serde::Deserialize;

const FILE_SYSTEM_PROMPT: &str = "Select changed files matching the user's staging instruction. \
                                  Return JSON only as {\"files\":[\"exact/path\"]}. Copy paths \
                                  exactly from the supplied list and omit non-matches.";
const HUNK_SYSTEM_PROMPT: &str = "Decide whether the changed lines match the user's staging \
                                  instruction. Return exactly yes or no.";
const HUNK_CHARS: usize = 2_400;
const HUNK_CONCURRENCY: usize = 8;

/// Counts reported after one AI-assisted staging operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AiStageOutcome {
	/// Files selected by the file-list pass.
	pub matched_files: usize,
	/// Files presented to the file-list pass.
	pub total_files:   usize,
	/// Individual hunks staged.
	pub staged_hunks:  usize,
	/// Individual hunks evaluated.
	pub total_hunks:   usize,
	/// Untracked, binary, or file-scoped matches staged whole.
	pub whole_files:   usize,
}

impl AiStageOutcome {
	/// Human-readable one-line result for the workbench footer.
	pub fn message(self) -> Str {
		if self.staged_hunks == 0 && self.whole_files == 0 {
			return Str::new_static("No changes matched the AI staging instruction");
		}
		sf!(
			"Staged {} hunk(s) and {} whole file(s) from {}/{} matching files",
			self.staged_hunks,
			self.whole_files,
			self.matched_files,
			self.total_files
		)
	}
}

/// Failure while evaluating or applying an AI staging instruction.
#[derive(Debug, thiserror::Error)]
pub enum AiStageError {
	/// No eligible unstaged changes exist.
	#[error("no unstaged changes are eligible for AI staging")]
	NoChanges,
	/// Inference failed while selecting files or hunks.
	#[error("AI staging inference failed")]
	Inference {
		/// Typed inference-layer failure.
		#[source]
		source: CommitError,
	},
	/// The repository operation failed.
	#[error("AI staging repository operation failed")]
	Repository {
		/// Typed VCS failure.
		#[source]
		source: omp_vcs::Error,
	},
	/// A blocking VCS task could not be joined.
	#[error("AI staging repository task failed")]
	Task {
		/// Typed task failure.
		#[source]
		source: tokio::task::JoinError,
	},
}

#[derive(Clone)]
struct Candidate {
	path:      Str,
	kind:      GitChangeKind,
	additions: Option<u64>,
	deletions: Option<u64>,
	untracked: bool,
}

#[derive(Clone)]
struct HunkJob {
	path:    Str,
	index:   u32,
	changed: Str,
}

#[derive(Deserialize)]
struct FileSelection {
	#[serde(default)]
	files: Vec<String>,
}

/// Selects and stages files or hunks matching `instruction`.
pub async fn stage(
	cwd: &Path,
	instruction: &str,
	files: &[GitFileRow],
	generator: &CommitGenerator,
) -> Result<AiStageOutcome, AiStageError> {
	let candidates = files
		.iter()
		.filter(|file| file.kind != GitChangeKind::Conflicted)
		.map(|file| Candidate {
			path:      file.path.clone(),
			kind:      file.kind,
			additions: file.additions,
			deletions: file.deletions,
			untracked: file.kind == GitChangeKind::Untracked,
		})
		.collect::<Vec<_>>();
	if candidates.is_empty() {
		return Err(AiStageError::NoChanges);
	}

	let file_list = candidates
		.iter()
		.map(|candidate| {
			sf!(
				"- {} ({:?}, +{} -{})",
				candidate.path,
				candidate.kind,
				candidate.additions.unwrap_or(0),
				candidate.deletions.unwrap_or(0)
			)
		})
		.collect::<Vec<_>>()
		.join("\n");
	let file_prompt = sf!("Changed files:\n{file_list}\n\nStage only: {instruction}");
	let reply = generator
		.complete_auxiliary(FILE_SYSTEM_PROMPT, file_prompt.as_str())
		.await
		.map_err(|source| AiStageError::Inference { source })?;
	let paths = candidates
		.iter()
		.map(|candidate| candidate.path.clone())
		.collect::<Vec<_>>();
	let picked = parse_file_selection(reply.as_str(), &paths);
	let authoritative = !picked.is_empty();
	let picked = picked.into_iter().collect::<BTreeSet<_>>();
	let matched = candidates
		.iter()
		.filter(|candidate| !authoritative || picked.contains(&candidate.path))
		.cloned()
		.collect::<Vec<_>>();

	let tracked_paths = candidates
		.iter()
		.filter(|candidate| !candidate.untracked)
		.map(|candidate| candidate.path.to_string())
		.collect::<Vec<_>>();
	let cwd_owned = cwd.to_path_buf();
	let raw_diff = tokio::task::spawn_blocking(move || {
		GitRepo::require(&cwd_owned)?.diff_text(&DiffOptions {
			files: tracked_paths,
			binary: true,
			..DiffOptions::default()
		})
	})
	.await
	.map_err(|source| AiStageError::Task { source })?
	.map_err(|source| AiStageError::Repository { source })?;

	let mut whole = BTreeSet::new();
	let mut jobs = Vec::new();
	for candidate in &matched {
		if candidate.untracked {
			if authoritative {
				whole.insert(candidate.path.clone());
			}
			continue;
		}
		let hunks = diff_hunks(raw_diff.as_str(), candidate.path.as_str());
		if hunks.is_empty() {
			if authoritative {
				whole.insert(candidate.path.clone());
			}
			continue;
		}
		jobs.extend(hunks.into_iter().map(|(index, changed)| HunkJob {
			path: candidate.path.clone(),
			index,
			changed: changed.to_str(),
		}));
	}

	let verdicts = stream::iter(jobs.iter().cloned().enumerate())
		.map(|(ordinal, job)| {
			let generator = generator.clone();
			async move {
				let changed = bound(job.changed.as_str(), HUNK_CHARS);
				let prompt = sf!(
					"File: {}\nChanged lines:\n{}\n\nStage only: {}",
					job.path,
					changed,
					instruction
				);
				let result = generator
					.complete_auxiliary(HUNK_SYSTEM_PROMPT, prompt.as_str())
					.await
					.map(|reply| parse_verdict(reply.as_str()));
				(ordinal, result)
			}
		})
		.buffer_unordered(HUNK_CONCURRENCY)
		.collect::<Vec<_>>()
		.await;
	let mut accepted = vec![false; jobs.len()];
	let mut failures = 0;
	let mut first_error = None;
	for (ordinal, verdict) in verdicts {
		match verdict {
			Ok(value) => accepted[ordinal] = value,
			Err(error) => {
				failures += 1;
				first_error.get_or_insert(error);
			},
		}
	}
	if !jobs.is_empty() && failures == jobs.len() {
		return Err(AiStageError::Inference {
			source: first_error.expect("every failed hunk records its typed error"),
		});
	}

	let staged_hunks = accepted.iter().filter(|accepted| **accepted).count();
	let whole_file_scope = authoritative && failures == 0 && !jobs.is_empty() && staged_hunks == 0;
	if whole_file_scope {
		whole.extend(
			matched
				.iter()
				.filter(|candidate| !candidate.untracked)
				.map(|candidate| candidate.path.clone()),
		);
	}
	let mut selections = whole
		.iter()
		.map(|path| HunkSelection { path: path.to_string(), hunks: HunkSpec::All })
		.collect::<Vec<_>>();
	if !whole_file_scope {
		let mut indices = BTreeMap::<Str, Vec<u32>>::new();
		for (job, accepted) in jobs.iter().zip(&accepted) {
			if *accepted {
				indices.entry(job.path.clone()).or_default().push(job.index);
			}
		}
		selections.extend(indices.into_iter().map(|(path, indices)| HunkSelection {
			path:  path.to_string(),
			hunks: HunkSpec::Indices(indices),
		}));
	}
	let untracked = whole
		.iter()
		.filter(|path| {
			matched
				.iter()
				.any(|candidate| candidate.untracked && &candidate.path == *path)
		})
		.map(ToString::to_string)
		.collect::<Vec<_>>();
	selections.retain(|selection| !untracked.iter().any(|path| path == &selection.path));

	let cwd_owned = cwd.to_path_buf();
	let raw_for_apply = raw_diff.clone();
	tokio::task::spawn_blocking(move || {
		let repo = GitRepo::require(&cwd_owned)?;
		if !selections.is_empty() {
			repo.stage_hunks(&selections, Some(raw_for_apply.as_str()))?;
		}
		if !untracked.is_empty() {
			repo.stage_files(&untracked)?;
		}
		Ok::<_, omp_vcs::Error>(())
	})
	.await
	.map_err(|source| AiStageError::Task { source })?
	.map_err(|source| AiStageError::Repository { source })?;

	Ok(AiStageOutcome {
		matched_files: matched.len(),
		total_files: candidates.len(),
		staged_hunks,
		total_hunks: jobs.len(),
		whole_files: whole.len(),
	})
}

/// Parses a typed file-selection reply while tolerating fences, partial JSON,
/// bullets, and indices.
pub fn parse_file_selection(text: &str, paths: &[Str]) -> Vec<Str> {
	let mut picked = BTreeSet::new();
	if let Some(start) = text.find('{') {
		let object = &text[start..text.rfind('}').map_or(text.len(), |end| end + 1)];
		if let Ok(selection) = omp_slopjson::from_str::<FileSelection>(object) {
			for path in selection.files {
				if paths.iter().any(|candidate| candidate.as_str() == path) {
					picked.insert(path.to_str());
				}
			}
		}
	}
	for line in text.lines() {
		let line = line
			.trim()
			.trim_start_matches(['-', '*', '•'])
			.trim()
			.trim_matches(['`', '"', '\'']);
		if let Ok(index) = line.parse::<usize>()
			&& let Some(path) = index.checked_sub(1).and_then(|index| paths.get(index))
		{
			picked.insert(path.clone());
		}
		for path in paths {
			if line == path.as_str()
				|| line
					.strip_prefix(path.as_str())
					.is_some_and(|suffix| suffix.starts_with(" (") || suffix.starts_with(','))
			{
				picked.insert(path.clone());
			}
		}
	}
	for path in paths {
		let Some(offset) = text.find(path.as_str()) else {
			continue;
		};
		let bytes = text.as_bytes();
		let before = offset
			.checked_sub(1)
			.and_then(|index| bytes.get(index))
			.copied();
		let after = bytes.get(offset + path.len()).copied();
		let is_path =
			|byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b'-');
		if before.is_none_or(|byte| !is_path(byte)) && after.is_none_or(|byte| !is_path(byte)) {
			picked.insert(path.clone());
		}
	}
	paths
		.iter()
		.filter(|path| picked.contains(*path))
		.cloned()
		.collect()
}

fn parse_verdict(text: &str) -> bool {
	let mut words = text
		.split(|character: char| !character.is_ascii_alphabetic())
		.filter(|word| !word.is_empty());
	matches!(words.next(), Some(word) if word.eq_ignore_ascii_case("yes"))
}

fn bound(text: &str, limit: usize) -> &str {
	if text.len() <= limit {
		return text;
	}
	let mut end = limit;
	while !text.is_char_boundary(end) {
		end -= 1;
	}
	&text[..end]
}

fn diff_hunks(raw_diff: &str, path: &str) -> Vec<(u32, String)> {
	let marker = sf!("+++ b/{path}");
	let Some(file_start) = raw_diff
		.split_inclusive('\n')
		.scan(0, |offset, line| {
			let start = *offset;
			*offset += line.len();
			Some((start, line))
		})
		.find_map(|(offset, line)| (line.trim_end() == marker.as_str()).then_some(offset))
	else {
		return Vec::new();
	};
	let block_start = raw_diff[..file_start]
		.rfind("diff --git ")
		.unwrap_or(file_start);
	let rest = &raw_diff[block_start..];
	let block_end = rest[1..]
		.find("\ndiff --git ")
		.map_or(rest.len(), |offset| offset + 1);
	let block = &rest[..block_end];
	let mut hunks = Vec::new();
	let mut changed = String::new();
	let mut index = 0_u32;
	for line in block.lines() {
		if line.starts_with("@@") {
			if index > 0 && !changed.is_empty() {
				hunks.push((index, std::mem::take(&mut changed)));
			}
			index += 1;
			continue;
		}
		if index > 0 && (line.starts_with('+') || line.starts_with('-')) {
			changed.push_str(line);
			changed.push('\n');
		}
	}
	if index > 0 && !changed.is_empty() {
		hunks.push((index, changed));
	}
	hunks
}

#[cfg(test)]
mod tests {
	use omp_core::Str;

	use super::{diff_hunks, parse_file_selection};

	#[test]
	fn file_selection_parses_typed_partial_and_decorated_responses() {
		let paths = [Str::new_static("src/a.rs"), Str::new_static("docs/note.md")];
		assert_eq!(parse_file_selection("```json\n{\"files\":[\"src/a.rs\"]}\n```", &paths), vec![
			Str::new_static("src/a.rs")
		]);
		assert_eq!(parse_file_selection("- docs/note.md (modified)\n1", &paths), paths);
		assert_eq!(parse_file_selection("I would stage src/a.rs and nothing else", &paths), vec![
			Str::new_static("src/a.rs")
		]);
		assert!(parse_file_selection("none\nvendored/src/a.rs", &paths).is_empty());
	}

	#[test]
	fn hunk_parser_batches_changed_lines_by_one_based_index() {
		let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 \
		            @@\n-old\n+new\n@@ -8 +8 @@\n-before\n+after\ndiff --git a/b b/b\n";
		let hunks = diff_hunks(diff, "src/a.rs");
		assert_eq!(hunks.len(), 2);
		assert_eq!(hunks[0], (1, "-old\n+new\n".to_owned()));
		assert_eq!(hunks[1].0, 2);
	}
}
