use std::{
	borrow::Cow,
	ffi::{OsStr, OsString},
	fmt,
	path::Path,
};

use globset::{Glob, GlobMatcher};
use omp_core::Str;

use crate::SandboxError;

/// Source environment used before allow and deny filtering.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum EnvironmentSource {
	/// Inherit the caller's environment at preparation time.
	#[default]
	Inherit,
	/// Use exactly these `NAME=VALUE` entries, including an explicitly empty
	/// list.
	Exact(Vec<OsString>),
}

/// Ordered environment source and name filters for a sandboxed process.
#[derive(Clone, Default)]
pub struct EnvironmentPolicy {
	source: EnvironmentSource,
	allow:  Vec<EnvironmentPattern>,
	deny:   Vec<EnvironmentPattern>,
}

impl EnvironmentPolicy {
	/// Creates a policy that inherits the caller's complete environment.
	#[must_use]
	pub const fn inherit() -> Self {
		Self { source: EnvironmentSource::Inherit, allow: Vec::new(), deny: Vec::new() }
	}

	/// Creates a policy from exact `NAME=VALUE` entries.
	#[must_use]
	pub const fn exact(entries: Vec<OsString>) -> Self {
		Self { source: EnvironmentSource::Exact(entries), allow: Vec::new(), deny: Vec::new() }
	}

	/// Returns the source evaluated before filtering.
	#[must_use]
	pub const fn source(&self) -> &EnvironmentSource {
		&self.source
	}

	/// Iterates over allow patterns in deterministic order.
	pub fn allow_patterns(&self) -> impl ExactSizeIterator<Item = &str> {
		self.allow.iter().map(|pattern| pattern.text.as_str())
	}

	/// Iterates over deny patterns in deterministic order.
	pub fn deny_patterns(&self) -> impl ExactSizeIterator<Item = &str> {
		self.deny.iter().map(|pattern| pattern.text.as_str())
	}

	pub(crate) fn set_source(&mut self, source: EnvironmentSource) {
		self.source = source;
	}

	pub(crate) fn add_allow(&mut self, pattern: impl AsRef<str>) -> Result<(), SandboxError> {
		insert_pattern(&mut self.allow, pattern.as_ref())
	}

	pub(crate) fn add_deny(&mut self, pattern: impl AsRef<str>) -> Result<(), SandboxError> {
		insert_pattern(&mut self.deny, pattern.as_ref())
	}

	pub(crate) const fn scrubs(&self) -> bool {
		!self.allow.is_empty() || !self.deny.is_empty()
	}

	pub(crate) fn resolve(&self) -> Option<Vec<OsString>> {
		let entries = match &self.source {
			EnvironmentSource::Inherit if !self.scrubs() => return None,
			EnvironmentSource::Inherit => std::env::vars_os()
				.map(|(name, value)| {
					let mut entry = name;
					entry.push("=");
					entry.push(value);
					entry
				})
				.collect(),
			EnvironmentSource::Exact(entries) => entries.clone(),
		};
		Some(
			entries
				.into_iter()
				.filter(|entry| {
					let name = env_name(entry);
					(self.allow.is_empty() || matches_any(name.as_ref(), &self.allow))
						&& !matches_any(name.as_ref(), &self.deny)
				})
				.collect(),
		)
	}
}

impl fmt::Debug for EnvironmentPolicy {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("EnvironmentPolicy")
			.field("source", &self.source)
			.field(
				"allow",
				&self
					.allow
					.iter()
					.map(|pattern| &pattern.text)
					.collect::<Vec<_>>(),
			)
			.field(
				"deny",
				&self
					.deny
					.iter()
					.map(|pattern| &pattern.text)
					.collect::<Vec<_>>(),
			)
			.finish()
	}
}

#[derive(Clone)]
struct EnvironmentPattern {
	text:    Str,
	matcher: GlobMatcher,
}

fn insert_pattern(
	patterns: &mut Vec<EnvironmentPattern>,
	pattern: &str,
) -> Result<(), SandboxError> {
	if pattern.trim().is_empty() {
		return Err(SandboxError::EmptyEnvironmentPattern);
	}
	let text = Str::from(pattern);
	let matcher = Glob::new(pattern)
		.map_err(|source| SandboxError::InvalidEnvironmentPattern { pattern: text.clone(), source })?
		.compile_matcher();
	match patterns.binary_search_by(|existing| existing.text.cmp(&text)) {
		Ok(_) => {},
		Err(index) => patterns.insert(index, EnvironmentPattern { text, matcher }),
	}
	Ok(())
}

fn matches_any(name: &OsStr, patterns: &[EnvironmentPattern]) -> bool {
	patterns
		.iter()
		.any(|pattern| pattern.matcher.is_match(Path::new(name)))
}

fn env_name(entry: &OsStr) -> Cow<'_, OsStr> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt as _;

		let bytes = entry.as_bytes();
		let end = bytes
			.iter()
			.position(|byte| *byte == b'=')
			.unwrap_or(bytes.len());
		return Cow::Borrowed(OsStr::from_bytes(&bytes[..end]));
	}
	#[cfg(not(unix))]
	{
		let text = entry.to_string_lossy();
		let end = text.find('=').unwrap_or(text.len());
		Cow::Owned(OsString::from(&text[..end]))
	}
}

pub(crate) fn split_entry(entry: &OsStr) -> (OsString, OsString) {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

		let bytes = entry.as_bytes();
		let split = bytes
			.iter()
			.position(|byte| *byte == b'=')
			.unwrap_or(bytes.len());
		let name = OsString::from_vec(bytes[..split].to_vec());
		let value = if split == bytes.len() {
			OsString::new()
		} else {
			OsString::from_vec(bytes[split + 1..].to_vec())
		};
		return (name, value);
	}
	#[cfg(windows)]
	{
		use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

		let wide = entry.encode_wide().collect::<Vec<_>>();
		let split = wide
			.iter()
			.position(|unit| *unit == b'=' as u16)
			.unwrap_or(wide.len());
		let name = OsString::from_wide(&wide[..split]);
		let value = if split == wide.len() {
			OsString::new()
		} else {
			OsString::from_wide(&wide[split + 1..])
		};
		return (name, value);
	}
	#[cfg(not(any(unix, windows)))]
	{
		let text = entry.to_string_lossy();
		let (name, value) = text.split_once('=').unwrap_or((&text, ""));
		(OsString::from(name), OsString::from(value))
	}
}
