//! Validation, findings-first rendering, and private artifact retention.

use std::{
	fmt::Write as _,
	io,
	path::{Component, Path, PathBuf},
};

use omp_core::{Str, sf};
use omp_storage::{blob, blob::BlobStore};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use super::model::ReviewOutput;

const RESULT_SCHEMA: &str = "omp.security-review/1";

/// Canonical workspace and optional bounded review target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewScope {
	root:   PathBuf,
	target: PathBuf,
}

impl ReviewScope {
	/// Resolves a workspace root and optional relative file or directory scope.
	pub fn resolve(root: &Path, relative: Option<&Path>) -> Result<Self, ReviewResultError> {
		let root = root
			.canonicalize()
			.map_err(|source| ReviewResultError::Canonicalize { path: root.to_path_buf(), source })?;
		let relative = relative.unwrap_or_else(|| Path::new("."));
		validate_relative(relative)?;
		let target_path = root.join(relative);
		let target = target_path
			.canonicalize()
			.map_err(|source| ReviewResultError::Canonicalize { path: target_path, source })?;
		if !target.starts_with(&root) {
			return Err(ReviewResultError::OutsideWorkspace { path: relative.to_path_buf() });
		}
		Ok(Self { root, target })
	}

	/// Canonical workspace root.
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// Canonical bounded target.
	pub fn target(&self) -> &Path {
		&self.target
	}

	/// Returns the target as a workspace-relative path for the child assignment.
	pub fn relative_target(&self) -> &Path {
		self
			.target
			.strip_prefix(&self.root)
			.unwrap_or_else(|_| Path::new("."))
	}

	fn contains(&self, path: &Path) -> bool {
		if self.target.is_dir() {
			path.starts_with(&self.target)
		} else {
			path == self.target
		}
	}
}

/// A validated result retained through existing child and blob authorities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedReview {
	/// Strict structured child output.
	pub output:       ReviewOutput,
	/// Findings-first human report.
	pub report:       Str,
	/// Ordinary child journal reference.
	pub agent_uri:    Str,
	/// Private content-addressed result artifact.
	pub artifact_uri: Str,
}

#[derive(Serialize)]
struct RetainedReview<'a> {
	schema: &'static str,
	output: &'a ReviewOutput,
	report: &'a str,
}

/// Strict result, scope, or retention failure.
#[derive(Debug, Error)]
pub enum ReviewResultError {
	/// A scope or finding path is absolute, traversing, or otherwise malformed.
	#[error("security review path must be a bounded relative workspace path: {path}")]
	InvalidPath {
		/// Refused path.
		path: PathBuf,
	},
	/// An existing path could not be resolved.
	#[error("security review path could not be resolved: {path}")]
	Canonicalize {
		/// Unresolved path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// A canonical path escapes the workspace or requested scope.
	#[error("security review location is outside the requested workspace scope: {path}")]
	OutsideWorkspace {
		/// Refused workspace-relative path.
		path: PathBuf,
	},
	/// A finding location did not identify an existing regular source file.
	#[error("security review location is not a source file: {path}")]
	NotFile {
		/// Refused workspace-relative path.
		path: PathBuf,
	},
	/// Structured child output did not match the strict data model.
	#[error("security reviewer returned malformed structured output")]
	Malformed(#[source] serde_json::Error),
	/// A required finding string was empty after trimming.
	#[error("security finding field `{field}` must not be empty")]
	EmptyField {
		/// Empty field name.
		field: &'static str,
	},
	/// A one-based inclusive source range was reversed or empty.
	#[error("security finding source range is malformed")]
	MalformedRange,
	/// The child handle was not an ordinary agent reference.
	#[error("security review did not return an ordinary agent reference")]
	InvalidAgentReference,
	/// Validated output could not be serialized for retention.
	#[error("security review result could not be serialized")]
	Serialize(#[source] serde_json::Error),
	/// The existing private blob authority could not retain the result.
	#[error("security review result artifact could not be retained")]
	Retain(#[source] blob::Error),
}

/// Validates final structured output, renders findings first, and retains one
/// private result artifact without introducing another store.
pub fn validate_and_retain(
	raw: Value,
	scope: &ReviewScope,
	agent_uri: impl Into<Str>,
	blobs: &BlobStore,
) -> Result<ValidatedReview, ReviewResultError> {
	let mut output =
		serde_json::from_value::<ReviewOutput>(raw).map_err(ReviewResultError::Malformed)?;
	validate_output(&output, scope)?;
	output
		.findings
		.sort_unstable_by_key(|finding| finding.severity);
	let report = render_findings_first(&output);
	let agent_uri = agent_uri.into();
	if !agent_uri.starts_with("agent://") {
		return Err(ReviewResultError::InvalidAgentReference);
	}
	let retained = serde_json::to_vec(&RetainedReview {
		schema: RESULT_SCHEMA,
		output: &output,
		report: report.as_str(),
	})
	.map_err(ReviewResultError::Serialize)?;
	let reference = blobs.put(&retained).map_err(ReviewResultError::Retain)?;
	Ok(ValidatedReview {
		output,
		report,
		agent_uri,
		artifact_uri: sf!("artifact://sha256/{}", reference.to_hex()),
	})
}

fn validate_output(output: &ReviewOutput, scope: &ReviewScope) -> Result<(), ReviewResultError> {
	non_empty("summary", &output.summary)?;
	for finding in &output.findings {
		non_empty("title", &finding.title)?;
		non_empty("evidence", &finding.evidence)?;
		non_empty("impact", &finding.impact)?;
		non_empty("remediation", &finding.remediation)?;
		if finding.range.start_line == 0
			|| finding.range.end_line == 0
			|| finding.range.end_line < finding.range.start_line
		{
			return Err(ReviewResultError::MalformedRange);
		}
		let relative = Path::new(finding.path.as_str());
		validate_relative(relative)?;
		let joined = scope.root.join(relative);
		let canonical = joined
			.canonicalize()
			.map_err(|source| ReviewResultError::Canonicalize { path: joined, source })?;
		if !scope.contains(&canonical) {
			return Err(ReviewResultError::OutsideWorkspace { path: relative.to_path_buf() });
		}
		if !canonical.is_file() {
			return Err(ReviewResultError::NotFile { path: relative.to_path_buf() });
		}
	}
	Ok(())
}

fn non_empty(field: &'static str, value: &str) -> Result<(), ReviewResultError> {
	if value.trim().is_empty() {
		Err(ReviewResultError::EmptyField { field })
	} else {
		Ok(())
	}
}

fn validate_relative(path: &Path) -> Result<(), ReviewResultError> {
	if path.as_os_str().is_empty()
		|| path.is_absolute()
		|| path.components().any(|component| {
			matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
		}) || path.to_string_lossy().contains('\0')
	{
		return Err(ReviewResultError::InvalidPath { path: path.to_path_buf() });
	}
	Ok(())
}

fn render_findings_first(output: &ReviewOutput) -> Str {
	let mut report = String::new();
	if output.findings.is_empty() {
		report.push_str("No actionable security findings.\n");
	} else {
		for finding in &output.findings {
			let _ = writeln!(
				report,
				"[{}] {}\n  {}:{}-{}\n  Evidence: {}\n  Impact: {}\n  Remediation: {}\n",
				finding.severity,
				finding.title,
				finding.path,
				finding.range.start_line,
				finding.range.end_line,
				finding.evidence,
				finding.impact,
				finding.remediation,
			);
		}
	}
	let _ = writeln!(report, "Summary: {}", output.summary);
	Str::from(report)
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::{super::model::Severity, *};

	#[test]
	fn rejects_reversed_ranges_and_scope_escape() {
		let workspace = tempfile::tempdir().unwrap();
		fs::create_dir_all(workspace.path().join("src")).unwrap();
		fs::write(workspace.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
		fs::write(workspace.path().join("outside.rs"), "secret\n").unwrap();
		let scope = ReviewScope::resolve(workspace.path(), Some(Path::new("src"))).unwrap();
		let malformed = serde_json::json!({
			"findings": [{
				"severity": "high", "title": "unsafe", "path": "src/lib.rs",
				"range": { "startLine": 4, "endLine": 2 }, "evidence": "source to sink",
				"impact": "impact", "remediation": "fix the control"
			}],
			"summary": "reviewed src"
		});
		let output = serde_json::from_value::<ReviewOutput>(malformed).unwrap();
		assert!(matches!(validate_output(&output, &scope), Err(ReviewResultError::MalformedRange)));

		let outside = ReviewOutput {
			findings: vec![super::super::model::Finding {
				severity:    Severity::High,
				title:       "unsafe".into(),
				path:        "outside.rs".into(),
				range:       super::super::model::SourceRange { start_line: 1, end_line: 1 },
				evidence:    "source to sink".into(),
				impact:      "impact".into(),
				remediation: "fix".into(),
			}],
			summary:  "reviewed".into(),
		};
		assert!(matches!(
			validate_output(&outside, &scope),
			Err(ReviewResultError::OutsideWorkspace { .. })
		));
	}
}
