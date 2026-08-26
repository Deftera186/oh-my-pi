use std::fmt;

use omp_core::Str;
use omp_tool::{
	Part, PromptCaps, ToolIdentity,
	render::{RenderRegistry, RenderRegistryError},
};

use self::{
	edit::EditRenderer,
	exec::{EvalRenderer, ShellRenderer},
	fs::{ReadRenderer, WriteRenderer},
	hub::HubRenderer,
	search::{GlobRenderer, GrepRenderer},
	web::WebSearchRenderer,
};

/// Native edit renderer views.
pub(crate) mod edit;
/// Native shell and eval renderer views.
pub(crate) mod exec;
/// Native read and write renderer views.
pub(crate) mod fs;
/// Native hub renderer views.
pub(crate) mod hub;
/// Bounded JSON-tree previews shared by structured tool views.
pub mod json_tree;
/// Grouped path and directory-tree rendering.
pub mod paths;
/// Native grep and glob renderer views.
pub(crate) mod search;
/// Shared line, byte, and column truncation.
pub mod truncate;
/// Native web search renderer views.
pub(crate) mod web;

/// Exact production identities associated with enabled native renderer
/// implementations.
///
/// Composition supplies identities only for tools that were actually
/// registered. Renderers therefore auto-follow tool inclusion independently:
/// disabling one tool cannot suppress every unrelated renderer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuiltinRendererIdentities {
	/// Identity of the native edit dialect, when enabled.
	pub edit:       Option<ToolIdentity>,
	/// Identity of the native regex search tool, when enabled.
	pub grep:       Option<ToolIdentity>,
	/// Identity of canonical web search, when enabled.
	pub web_search: Option<ToolIdentity>,
	/// Identity of the native path matching tool, when enabled.
	pub glob:       Option<ToolIdentity>,
	/// Identity of the native persistent shell, when enabled.
	pub shell:      Option<ToolIdentity>,
	/// Identity of the native coordination hub, when enabled.
	pub hub:        Option<ToolIdentity>,
	/// Identity of the native whole-file writer, when enabled.
	pub write:      Option<ToolIdentity>,
	/// Identity of the native resource reader, when enabled.
	pub read:       Option<ToolIdentity>,
	/// Identity of the native persistent evaluator, when enabled.
	pub eval:       Option<ToolIdentity>,
}

/// Registers every native renderer under the exact identities supplied by
/// production composition.
///
/// # Errors
///
/// Returns the first duplicate-identity error reported by `registry`.
pub fn register_builtin_renderers(
	registry: &mut RenderRegistry,
	identities: BuiltinRendererIdentities,
) -> Result<(), RenderRegistryError> {
	if let Some(identity) = identities.edit {
		registry.register(identity, EditRenderer)?;
	}
	if let Some(identity) = identities.grep {
		registry.register(identity, GrepRenderer)?;
	}
	if let Some(identity) = identities.web_search {
		registry.register(identity, WebSearchRenderer)?;
	}
	if let Some(identity) = identities.glob {
		registry.register(identity, GlobRenderer)?;
	}
	if let Some(identity) = identities.shell {
		registry.register(identity, ShellRenderer)?;
	}
	if let Some(identity) = identities.hub {
		registry.register(identity, HubRenderer)?;
	}
	if let Some(identity) = identities.write {
		registry.register(identity, WriteRenderer)?;
	}
	if let Some(identity) = identities.read {
		registry.register(identity, ReadRenderer)?;
	}
	if let Some(identity) = identities.eval {
		registry.register(identity, EvalRenderer)?;
	}
	Ok(())
}

/// Writes a compact human duration (`12ms`, `1.4s`, `2m36s`, `1h04m`).
fn push_duration_ms(output: &mut String, ms: u64) {
	use std::fmt::Write as _;
	if ms < 1_000 {
		write!(output, "{ms}ms").expect("writing to String cannot fail");
	} else if ms < 60_000 {
		let tenths = ms / 100;
		write!(output, "{}.{}s", tenths / 10, tenths % 10).expect("writing to String cannot fail");
	} else if ms < 3_600_000 {
		let seconds = ms / 1_000;
		write!(output, "{}m{:02}s", seconds / 60, seconds % 60)
			.expect("writing to String cannot fail");
	} else {
		let minutes = ms / 60_000;
		write!(output, "{}h{:02}m", minutes / 60, minutes % 60)
			.expect("writing to String cannot fail");
	}
}

/// Writes a compact human byte count (`8B`, `2.4K`, `103K`, `1.2M`).
fn push_bytes(output: &mut String, bytes: u64) {
	use std::fmt::Write as _;
	const UNITS: [&str; 4] = ["K", "M", "G", "T"];
	if bytes < 1_000 {
		write!(output, "{bytes}B").expect("writing to String cannot fail");
		return;
	}
	let mut scaled = bytes as f64;
	let mut unit = 0usize;
	while scaled >= 1_000.0 && unit + 1 < UNITS.len() {
		scaled /= 1_000.0;
		unit += 1;
	}
	if scaled >= 1_000.0 || scaled.fract() < 0.05 || scaled >= 100.0 {
		write!(output, "{}{}", scaled.round() as u64, UNITS[unit])
			.expect("writing to String cannot fail");
	} else {
		write!(output, "{scaled:.1}{}", UNITS[unit]).expect("writing to String cannot fail");
	}
}

fn live_view(name: &str, status: &str) -> Str {
	let mut output = String::from("<row gap=1><text bold>");
	push_text(&mut output, name);
	output.push_str("</text><text fg=muted>");
	push_text(&mut output, status);
	output.push_str("</text></row>");
	Str::new(output)
}

fn fault_view(name: &str, message: &str) -> Str {
	let mut output = String::from("<row gap=1><text bold fg=error>");
	push_text(&mut output, name);
	output.push_str("</text><text fg=error>");
	push_text(&mut output, message);
	output.push_str("</text></row>");
	Str::new(output)
}

fn debug_label(value: impl fmt::Debug) -> String {
	format!("{value:?}").to_ascii_lowercase()
}

fn push_attr(output: &mut String, text: &str) {
	for character in text.chars() {
		match character {
			'&' => output.push_str("&amp;"),
			'<' => output.push_str("&lt;"),
			'>' => output.push_str("&gt;"),
			'"' => output.push_str("&quot;"),
			'\'' => output.push_str("&#39;"),
			character if character.is_control() => output.push('\u{fffd}'),
			character => output.push(character),
		}
	}
}

fn push_text(output: &mut String, text: &str) {
	for character in text.chars() {
		match character {
			'&' => output.push_str("&amp;"),
			'<' => output.push_str("&lt;"),
			'>' => output.push_str("&gt;"),
			'\t' | '\n' | '\r' => output.push(character),
			character if character.is_control() => output.push('\u{fffd}'),
			character => output.push(character),
		}
	}
}

/// Accumulates whole UTF-8 fragments without splitting a caller-owned unit.
pub struct TextProjection {
	text:      String,
	max_bytes: usize,
	truncated: bool,
}

impl TextProjection {
	pub(crate) fn new(caps: PromptCaps) -> Option<Self> {
		(caps.maximum_parts != 0 && caps.maximum_text_bytes != 0).then(|| Self {
			text:      String::new(),
			max_bytes: usize::try_from(caps.maximum_text_bytes).unwrap_or(usize::MAX),
			truncated: false,
		})
	}

	pub(crate) fn push(&mut self, fragment: &str) -> bool {
		if self.text.len().saturating_add(fragment.len()) > self.max_bytes {
			self.truncated = true;
			return false;
		}
		self.text.push_str(fragment);
		true
	}

	pub(crate) fn finish(mut self) -> Vec<Part> {
		const MARKER: &str = "\n[truncated]";
		if self.truncated && self.text.len().saturating_add(MARKER.len()) <= self.max_bytes {
			self.text.push_str(MARKER);
		}
		if self.text.is_empty() {
			Vec::new()
		} else {
			vec![Part::Text { text: Str::new(self.text) }]
		}
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	//! Shared registry construction helpers for renderer tests.

	use omp_core::{Str, sf};
	use omp_tool::{Rev, ToolIdentity, render::RenderRegistry};

	use super::{BuiltinRendererIdentities, register_builtin_renderers};

	/// Mints a test identity under the shared `test` revision family.
	pub(crate) fn identity(name: &str, revision: u16) -> ToolIdentity {
		ToolIdentity { name: Str::new(name), rev: Rev { family: sf!("test"), n: revision } }
	}

	/// Full identity set covering every built-in renderer.
	pub(crate) fn identities() -> BuiltinRendererIdentities {
		BuiltinRendererIdentities {
			edit:       Some(identity("edit", 41)),
			grep:       Some(identity("grep", 42)),
			web_search: Some(identity("web_search", 48)),
			glob:       Some(identity("glob", 43)),
			shell:      Some(identity("shell", 44)),
			hub:        Some(identity("hub", 45)),
			write:      Some(identity("write", 45)),
			read:       Some(identity("read", 46)),
			eval:       Some(identity("eval", 47)),
		}
	}

	/// Registers every built-in renderer and echoes the identity set.
	pub(crate) fn registry(
		identities: BuiltinRendererIdentities,
	) -> (RenderRegistry, BuiltinRendererIdentities) {
		let mut registry = RenderRegistry::new();
		register_builtin_renderers(&mut registry, identities.clone())
			.expect("unique built-in identities register");
		(registry, identities)
	}
}

#[cfg(test)]
mod tests {
	use omp_tool::render::{RenderRegistry, ViewState};

	use super::{
		BuiltinRendererIdentities, register_builtin_renderers,
		test_support::{identities, identity, registry},
	};

	#[test]
	fn registers_every_builtin_at_only_its_exact_revision() {
		let (registry, identities) = registry(identities());
		for identity in [
			identities.edit.as_ref().unwrap(),
			identities.grep.as_ref().unwrap(),
			identities.web_search.as_ref().unwrap(),
			identities.glob.as_ref().unwrap(),
			identities.shell.as_ref().unwrap(),
			identities.hub.as_ref().unwrap(),
			identities.write.as_ref().unwrap(),
			identities.read.as_ref().unwrap(),
			identities.eval.as_ref().unwrap(),
		] {
			assert!(
				registry
					.get(identity)
					.is_some_and(|entry| entry.identity() == identity)
			);
		}

		let wrong_revision = identity("edit", identities.edit.as_ref().unwrap().rev.n + 1);
		assert!(registry.get(&wrong_revision).is_none());
		let raw = br#"{"kind":"ok","value":{"foreign":true}}"#;
		assert_eq!(
			registry
				.view(&wrong_revision, &ViewState::new(), Some(raw))
				.expect("unknown exact revision uses generic facts")
				.as_str(),
			std::str::from_utf8(raw).expect("fixture is UTF-8"),
		);
	}

	#[test]
	fn disabled_tool_does_not_suppress_enabled_renderers() {
		let read = identity("read", 9);
		let mut registry = RenderRegistry::new();
		register_builtin_renderers(&mut registry, BuiltinRendererIdentities {
			read: Some(read.clone()),
			..Default::default()
		})
		.unwrap();
		assert!(registry.get(&read).is_some());
		assert!(registry.get(&identity("edit", 9)).is_none());
	}
}
