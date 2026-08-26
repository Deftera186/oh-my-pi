//! Native renderer lifecycle fixtures used by the visual QA gallery.

use omp_core::Str;
use omp_tool::{Rev, ToolIdentity};

use crate::BuiltinRendererIdentities;

/// One native renderer's synthetic lifecycle inputs for visual QA.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RendererGalleryFixture {
	/// Exact production-compatible renderer identity.
	pub identity:        ToolIdentity,
	/// Human-readable card title.
	pub title:           &'static str,
	/// Serialized typed progress update, when this renderer streams updates.
	pub progress_update: Option<&'static [u8]>,
	/// Serialized typed successful tool outcome.
	pub success_outcome: &'static [u8],
	/// Serialized typed faulted tool outcome.
	pub error_outcome:   &'static [u8],
}

/// Complete native renderer gallery fixture set and its registration keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinRendererGallery {
	/// Exact identities passed through production renderer registration.
	pub identities: BuiltinRendererIdentities,
	/// One four-state fixture contract for each registered renderer.
	pub fixtures:   Vec<RendererGalleryFixture>,
}

/// Builds the complete native renderer fixture contract.
///
/// Serialized values use the production update and durable outcome wire
/// formats. Dispatching them through the native render registry validates every
/// fixture against its renderer's typed fold.
pub fn builtin_renderer_gallery() -> BuiltinRendererGallery {
	fn identity(name: &'static str, family: &'static str) -> ToolIdentity {
		ToolIdentity {
			name: Str::new_static(name),
			rev:  Rev { family: Str::new_static(family), n: 1 },
		}
	}

	let edit = identity("edit", "hl");
	let grep = identity("grep", "");
	let web_search = identity("web_search", "");
	let glob = identity("glob", "");
	let shell = identity("shell", "");
	let hub = identity("hub", "");
	let write = identity("write", "");
	let read = identity("read", "");
	let eval = identity("eval", "");
	let identities = BuiltinRendererIdentities {
		edit:       Some(edit.clone()),
		grep:       Some(grep.clone()),
		web_search: Some(web_search.clone()),
		glob:       Some(glob.clone()),
		shell:      Some(shell.clone()),
		hub:        Some(hub.clone()),
		write:      Some(write.clone()),
		read:       Some(read.clone()),
		eval:       Some(eval.clone()),
	};
	let fixtures = [
		crate::render::edit::gallery_fixtures(edit.clone()),
		crate::render::fs::gallery_fixtures(write.clone(), read.clone()),
		crate::render::search::gallery_fixtures(grep.clone(), glob.clone()),
		crate::render::exec::gallery_fixtures(shell.clone(), eval.clone()),
		crate::render::web::gallery_fixtures(web_search.clone()),
		crate::render::hub::gallery_fixtures(hub.clone()),
	]
	.concat();
	BuiltinRendererGallery { identities, fixtures }
}
#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use bytes::Bytes;
	use omp_tool::render::{RenderRegistry, ViewState};

	use super::builtin_renderer_gallery;
	use crate::register_builtin_renderers;

	#[test]
	fn fixtures_cover_every_registered_renderer_and_decode_all_states() {
		let gallery = builtin_renderer_gallery();
		let fixture_identities = gallery
			.fixtures
			.iter()
			.map(|fixture| fixture.identity.clone())
			.collect::<BTreeSet<_>>();
		let mut registry = RenderRegistry::new();
		register_builtin_renderers(&mut registry, gallery.identities).unwrap();
		let registry_identities = registry.identities().cloned().collect::<BTreeSet<_>>();
		assert_eq!(fixture_identities, registry_identities);

		for fixture in &gallery.fixtures {
			let mut fold = ViewState::new();
			registry.view(&fixture.identity, &fold, None).unwrap();
			if let Some(update) = fixture.progress_update {
				registry
					.fold(&fixture.identity, &mut fold, Bytes::from_static(update))
					.unwrap();
			}
			registry.view(&fixture.identity, &fold, None).unwrap();
			registry
				.view(&fixture.identity, &fold, Some(fixture.success_outcome))
				.unwrap();
			registry
				.view(&fixture.identity, &fold, Some(fixture.error_outcome))
				.unwrap();
		}
	}
}
