//! Interactive scoped extension-resource configuration.

use std::{collections::BTreeMap, path::Path};

use clap::Args;
use miette::{IntoDiagnostic as _, miette};
use omp_chat_ui::OverlayPanel;
use omp_core::{Str, sf};
use omp_ext::{
	config::{
		DeploymentManifest, ExtensionOverlay, PackageResourceFilter, ResourceFamily, Scope,
		ScopedOverlay, fold_extension,
	},
	lock::{InstalledExtension, InstalledRecord},
};
use omp_settings::io::{DocumentMutation, mutate_document, read_document};
use omp_tui::{
	AppEvent, AppOptions, Dim, Key, OverlayAnchor, OverlayMargin, OverlayOptions, Prop, Size, Ui,
	components::{Button, Col, Row, Select, SelectOption, Shader, TextLeaf},
	shader::Eclipse,
};
use strum::Display;

use super::Layer;

const RESOURCE_LIST: &str = "extension-config-resources";
const ACCEPT: &str = "extension-config-accept";
const CANCEL: &str = "extension-config-cancel";

/// Options for the interactive extension resource selector.
#[derive(Clone, Debug, Default, Args)]
pub struct ExtConfigArgs {}

#[derive(Clone, Copy, Debug, Default, Display, Eq, PartialEq)]
enum WriteScope {
	#[default]
	Client,
	Workspace,
}

impl WriteScope {
	const fn switched(self) -> Self {
		match self {
			Self::Client => Self::Workspace,
			Self::Workspace => Self::Client,
		}
	}
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum OverrideState {
	Inherit,
	Load,
	Unload,
}

impl OverrideState {
	const fn cycle(self, inherited_enabled: bool) -> Self {
		match (self, inherited_enabled) {
			(Self::Inherit, true) => Self::Unload,
			(Self::Inherit, false) => Self::Load,
			(Self::Unload, true) => Self::Load,
			(Self::Unload, false) => Self::Inherit,
			(Self::Load, true) => Self::Inherit,
			(Self::Load, false) => Self::Unload,
		}
	}
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ItemKey {
	Extension { id: Str },
	Resource { id: Str, family: ResourceFamily, path: Str },
}

#[derive(Clone, Debug)]
struct SelectorItem {
	key:              ItemKey,
	label:            Str,
	default_enabled:  bool,
	client_available: bool,
}

#[derive(Clone, Debug)]
struct SelectorModel {
	scope:           WriteScope,
	items:           Vec<SelectorItem>,
	selected:        usize,
	client:          ExtensionOverlay,
	workspace:       ExtensionOverlay,
	dirty_client:    bool,
	dirty_workspace: bool,
}

impl SelectorModel {
	fn visible_items(&self) -> impl Iterator<Item = (usize, &SelectorItem)> {
		self
			.items
			.iter()
			.enumerate()
			.filter(|(_, item)| self.scope == WriteScope::Workspace || item.client_available)
	}

	fn selected_item(&self) -> Option<&SelectorItem> {
		self
			.items
			.get(self.selected)
			.filter(|item| self.scope == WriteScope::Workspace || item.client_available)
	}

	fn switch_scope(&mut self) {
		self.scope = self.scope.switched();
		if self.selected_item().is_none() {
			let selected = {
				let mut visible = self.visible_items();
				visible.next().map_or(0, |(index, _)| index)
			};
			self.selected = selected;
		}
	}

	fn select(&mut self, index: usize) {
		if self
			.items
			.get(index)
			.is_some_and(|item| self.scope == WriteScope::Workspace || item.client_available)
		{
			self.selected = index;
		}
	}

	fn toggle_selected(&mut self) {
		let Some(item) = self.selected_item().cloned() else {
			return;
		};
		match item.key {
			ItemKey::Extension { id } => self.toggle_extension(&id, item.default_enabled),
			ItemKey::Resource { id, family, path } => {
				self.toggle_resource(&id, family, &path, item.default_enabled)
			},
		}
	}

	fn toggle_extension(&mut self, id: &Str, default_enabled: bool) {
		let inherited = extension_enabled(&self.client, id, default_enabled);
		let next = match self.scope {
			WriteScope::Client => {
				if inherited {
					OverrideState::Unload
				} else {
					OverrideState::Load
				}
			},
			WriteScope::Workspace => extension_override(&self.workspace, id).cycle(inherited),
		};
		let overlay = self.overlay_mut();
		set_extension_override(overlay, id, next);
		self.mark_dirty();
	}

	fn toggle_resource(
		&mut self,
		id: &Str,
		family: ResourceFamily,
		path: &Str,
		default_enabled: bool,
	) {
		let inherited = resource_enabled(&self.client, id, family, path, default_enabled);
		let scope = self.scope;
		let next = match self.scope {
			WriteScope::Client => {
				if inherited {
					OverrideState::Unload
				} else {
					OverrideState::Load
				}
			},
			WriteScope::Workspace => {
				resource_override(&self.workspace, id, family, path).cycle(inherited)
			},
		};
		let overlay = self.overlay_mut();
		set_resource_override(overlay, id, family, path, next, scope);
		self.mark_dirty();
	}

	fn overlay_mut(&mut self) -> &mut ExtensionOverlay {
		match self.scope {
			WriteScope::Client => &mut self.client,
			WriteScope::Workspace => &mut self.workspace,
		}
	}

	fn mark_dirty(&mut self) {
		match self.scope {
			WriteScope::Client => self.dirty_client = true,
			WriteScope::Workspace => self.dirty_workspace = true,
		}
	}

	fn state(&self, item: &SelectorItem) -> OverrideState {
		match (&item.key, self.scope) {
			(ItemKey::Extension { id }, WriteScope::Client) => {
				if extension_enabled(&self.client, id, item.default_enabled) {
					OverrideState::Load
				} else {
					OverrideState::Unload
				}
			},
			(ItemKey::Extension { id }, WriteScope::Workspace) => {
				extension_override(&self.workspace, id)
			},
			(ItemKey::Resource { id, family, path }, WriteScope::Client) => {
				if resource_enabled(&self.client, id, *family, path, item.default_enabled) {
					OverrideState::Load
				} else {
					OverrideState::Unload
				}
			},
			(ItemKey::Resource { id, family, path }, WriteScope::Workspace) => {
				resource_override(&self.workspace, id, *family, path)
			},
		}
	}

	fn checked(&self, item: &SelectorItem) -> bool {
		let state = self.state(item);
		match state {
			OverrideState::Load => true,
			OverrideState::Unload => false,
			OverrideState::Inherit => match &item.key {
				ItemKey::Extension { id } => extension_enabled(&self.client, id, item.default_enabled),
				ItemKey::Resource { id, family, path } => {
					resource_enabled(&self.client, id, *family, path, item.default_enabled)
				},
			},
		}
	}
}

/// Opens the alternate-buffer selector and persists staged changes only after
/// acceptance.
#[expect(clippy::future_not_send, reason = "the selector owns a thread-confined omp_tui::App")]
pub async fn run(
	project: &Path,
	data_dir: Option<&Path>,
	layer: Option<Layer>,
	_args: ExtConfigArgs,
) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(data_dir.map(Path::to_path_buf)).into_diagnostic()?;
	let client_path = data_dir.join("config.toml");
	let workspace_path = project.join(".omp/config.toml");
	let client = read_extension_overlay(&client_path, Scope::Client)?;
	let workspace = read_extension_overlay(&workspace_path, Scope::Workspace)?;
	let items = load_items(&data_dir, project)?;
	let scope = if layer == Some(Layer::Workspace) {
		WriteScope::Workspace
	} else {
		WriteScope::Client
	};
	let mut model = SelectorModel {
		scope,
		selected: items
			.iter()
			.position(|item| scope == WriteScope::Workspace || item.client_available)
			.unwrap_or(0),
		items,
		client,
		workspace,
		dirty_client: false,
		dirty_workspace: false,
	};

	let mut app = AppOptions::new()
		.hold_alt()
		.keep_on_cancel()
		.mouse()
		.hotkeys([Key::Tab, Key::Space])
		.start(|env: omp_tui::AppEnv| {
			Ui::from_root(
				Shader::new(Eclipse::default()).size(env.viewport.width, env.viewport.height),
				env.viewport.width,
				env.ctx,
			)
		})
		.await
		.into_diagnostic()?;
	show_selector(app.ui_mut(), &model, false);
	let accepted = loop {
		match app.next().await.into_diagnostic()? {
			Some(AppEvent::Highlighted { id, value }) if id.as_str() == RESOURCE_LIST => {
				if let Ok(index) = value.parse::<usize>() {
					model.select(index);
				}
			},
			Some(AppEvent::Changed { id, value }) if id.as_str() == RESOURCE_LIST => {
				if let Ok(index) = value.parse::<usize>() {
					model.select(index);
					model.toggle_selected();
					show_selector(app.ui_mut(), &model, true);
				}
			},
			Some(AppEvent::Key(Key::Space)) => {
				model.toggle_selected();
				show_selector(app.ui_mut(), &model, true);
			},
			Some(AppEvent::Key(Key::Tab)) => {
				model.switch_scope();
				show_selector(app.ui_mut(), &model, true);
			},
			Some(AppEvent::Submitted) => break true,
			Some(AppEvent::Pressed(id)) if id.as_str() == ACCEPT => break true,
			Some(AppEvent::Pressed(id)) if id.as_str() == CANCEL => break false,
			Some(AppEvent::OverlayClosed(_)) | None => break false,
			_ => {},
		}
	};
	if accepted {
		if model.dirty_client {
			write_extension_overlay(&client_path, Scope::Client, &model.client)?;
		}
		if model.dirty_workspace {
			write_extension_overlay(&workspace_path, Scope::Workspace, &model.workspace)?;
		}
	}
	Ok(())
}

fn show_selector(ui: &mut Ui, model: &SelectorModel, replace: bool) {
	if replace {
		let _ = ui.close_top_overlay();
	}
	let mut select = Select::new()
		.with(Prop::Id, RESOURCE_LIST)
		.with(Prop::Multi, true)
		.with(Prop::Filter, true)
		.with(Prop::MaxRows, 18_u16);
	for (index, item) in model.visible_items() {
		let state = model.state(item);
		let detail = if state == OverrideState::Inherit {
			if model.checked(item) {
				sf!("inherit (enabled by client scope)")
			} else {
				sf!("inherit (disabled by client scope)")
			}
		} else {
			let state: &'static str = state.into();
			Str::new_static(state)
		};
		select = select.option(
			SelectOption::new()
				.label(item.label.clone())
				.with(Prop::Value, sf!("{index}"))
				.with(Prop::Desc, detail)
				.with(Prop::Selected, model.checked(item))
				.with(Prop::Active, index == model.selected),
		);
	}
	let actions = Row::new()
		.with(Prop::Gap, 1_u16)
		.child(Button::new().with(Prop::Id, ACCEPT).child("Apply"))
		.child(Button::new().with(Prop::Id, CANCEL).child("Cancel"));
	let content = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(TextLeaf::new().text(sf!("{} scope", model.scope)))
		.child(select)
		.child(actions)
		.child(
			TextLeaf::new()
				.with(Prop::Dim, true)
				.text("Tab switches scope | Space cycles state | Enter applies | Esc cancels"),
		);
	ui.show_overlay(
		OverlayPanel::new("Extension resources").child(content),
		OverlayOptions::default()
			.anchor(OverlayAnchor::Center)
			.width(Dim::Pct(72))
			.min_width(52)
			.max_height(Dim::Pct(86))
			.margin(OverlayMargin::uniform(1))
			.min_viewport(Size::new(30, 10)),
	);
	ui.focus_first();
}

fn read_extension_overlay(path: &Path, scope: Scope) -> miette::Result<ExtensionOverlay> {
	let document = read_document(path).into_diagnostic()?;
	let overlay: ExtensionOverlay = document
		.get("extensions")
		.cloned()
		.map(toml::Value::try_into)
		.transpose()
		.into_diagnostic()?
		.unwrap_or_default();
	overlay
		.validate(scope)
		.map_err(|error| miette!("{error}"))?;
	Ok(overlay)
}

fn write_extension_overlay(
	path: &Path,
	scope: Scope,
	overlay: &ExtensionOverlay,
) -> miette::Result<()> {
	overlay
		.validate(scope)
		.map_err(|error| miette!("{error}"))?;
	let value = toml::Value::try_from(overlay).into_diagnostic()?;
	mutate_document(path, &[DocumentMutation::Set { path: "extensions", value }])
		.into_diagnostic()?;
	Ok(())
}

fn load_items(data_dir: &Path, project: &Path) -> miette::Result<Vec<SelectorItem>> {
	let client = InstalledRecord::read(&data_dir.join("ext/installed.toml"))
		.map_err(|error| miette!("{error}"))?;
	let workspace = InstalledRecord::read(&project.join(".omp/installed.toml"))
		.map_err(|error| miette!("{error}"))?;
	let mut items = BTreeMap::<ItemKey, SelectorItem>::new();
	for extension in &client.extensions {
		collect_extension_items(&mut items, extension, true)?;
	}
	for extension in &workspace.extensions {
		collect_extension_items(&mut items, extension, false)?;
	}
	Ok(items.into_values().collect())
}

fn collect_extension_items(
	items: &mut BTreeMap<ItemKey, SelectorItem>,
	extension: &InstalledExtension,
	client_available: bool,
) -> miette::Result<()> {
	let key = ItemKey::Extension { id: extension.id.clone() };
	items
		.entry(key.clone())
		.and_modify(|item| {
			item.client_available |= client_available;
			if client_available {
				item.default_enabled = extension.enabled;
			}
		})
		.or_insert_with(|| SelectorItem {
			key,
			label: extension.id.clone(),
			default_enabled: extension.enabled,
			client_available,
		});
	let Some(manifest_path) = super::installed_manifest_path(extension) else {
		return Ok(());
	};
	let source = std::fs::read_to_string(&manifest_path).into_diagnostic()?;
	let manifest = DeploymentManifest::parse(&source).map_err(|error| miette!("{error}"))?;
	manifest.validate().map_err(|error| miette!("{error}"))?;
	for declaration in manifest.declarations {
		let Some((family, path)) = resource_declaration(&declaration.kind, declaration.path) else {
			continue;
		};
		let key = ItemKey::Resource { id: extension.id.clone(), family, path: path.clone() };
		items
			.entry(key.clone())
			.and_modify(|item| item.client_available |= client_available)
			.or_insert_with(|| SelectorItem {
				key,
				label: sf!("  {family}: {path}"),
				default_enabled: true,
				client_available,
			});
	}
	Ok(())
}

fn resource_declaration(kind: &Str, path: Option<Str>) -> Option<(ResourceFamily, Str)> {
	let family = kind.parse::<ResourceFamily>().ok()?;
	Some((family, path?))
}

fn extension_enabled(overlay: &ExtensionOverlay, id: &Str, default_enabled: bool) -> bool {
	if overlay.disabled.contains(id) {
		false
	} else if overlay.enabled.contains(id) {
		true
	} else {
		default_enabled
	}
}

fn extension_override(overlay: &ExtensionOverlay, id: &Str) -> OverrideState {
	if overlay.disabled.contains(id) {
		OverrideState::Unload
	} else if overlay.enabled.contains(id) {
		OverrideState::Load
	} else {
		OverrideState::Inherit
	}
}

fn set_extension_override(overlay: &mut ExtensionOverlay, id: &Str, state: OverrideState) {
	overlay.enabled.remove(id);
	overlay.disabled.remove(id);
	match state {
		OverrideState::Inherit => {},
		OverrideState::Load => {
			overlay.enabled.insert(id.clone());
		},
		OverrideState::Unload => {
			overlay.disabled.insert(id.clone());
		},
	}
}

fn resource_enabled(
	overlay: &ExtensionOverlay,
	id: &Str,
	family: ResourceFamily,
	path: &Str,
	default_enabled: bool,
) -> bool {
	fold_extension(&[ScopedOverlay { scope: Scope::Client, overlay: overlay.clone() }], id)
		.resource_enabled(family, path, default_enabled)
}

fn resource_override(
	overlay: &ExtensionOverlay,
	id: &Str,
	family: ResourceFamily,
	path: &Str,
) -> OverrideState {
	let Some(patterns) = overlay
		.resources
		.get(id)
		.and_then(|filter| filter.patterns(family))
	else {
		return OverrideState::Inherit;
	};
	let mut load = false;
	let mut unload = false;
	for pattern in patterns {
		match pattern.as_str().as_bytes().first().copied() {
			Some(b'+') if &pattern.as_str()[1..] == path.as_str() => load = true,
			Some(b'-') if &pattern.as_str()[1..] == path.as_str() => unload = true,
			_ => {},
		}
	}
	if unload {
		OverrideState::Unload
	} else if load {
		OverrideState::Load
	} else {
		OverrideState::Inherit
	}
}

fn set_resource_override(
	overlay: &mut ExtensionOverlay,
	id: &Str,
	family: ResourceFamily,
	path: &Str,
	state: OverrideState,
	scope: WriteScope,
) {
	let filter = overlay
		.resources
		.entry(id.clone())
		.or_insert_with(|| PackageResourceFilter {
			autoload: scope == WriteScope::Client,
			..PackageResourceFilter::default()
		});
	let patterns = resource_patterns_mut(filter, family);
	let entries = patterns.get_or_insert_with(Vec::new);
	entries.retain(|pattern| {
		let text = pattern.as_str();
		!matches!(text.as_bytes().first(), Some(b'+') | Some(b'-')) || &text[1..] != path.as_str()
	});
	match state {
		OverrideState::Inherit => {},
		OverrideState::Load => entries.push(sf!("+{path}")),
		OverrideState::Unload => entries.push(sf!("-{path}")),
	}
	if entries.is_empty() {
		*patterns = None;
	}
	let remove = overlay.resources.get(id).is_some_and(|filter| {
		!filter.autoload
			&& filter.extensions.is_none()
			&& filter.skills.is_none()
			&& filter.prompts.is_none()
			&& filter.themes.is_none()
	});
	if remove {
		overlay.resources.remove(id);
	}
}

fn resource_patterns_mut(
	filter: &mut PackageResourceFilter,
	family: ResourceFamily,
) -> &mut Option<Vec<Str>> {
	match family {
		ResourceFamily::Extensions => &mut filter.extensions,
		ResourceFamily::Skills => &mut filter.skills,
		ResourceFamily::Prompts => &mut filter.prompts,
		ResourceFamily::Themes => &mut filter.themes,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn id() -> Str {
		sf!("acme.review")
	}

	#[test]
	fn workspace_cycle_depends_on_inherited_state() {
		assert_eq!(OverrideState::Inherit.cycle(true), OverrideState::Unload);
		assert_eq!(OverrideState::Unload.cycle(true), OverrideState::Load);
		assert_eq!(OverrideState::Load.cycle(true), OverrideState::Inherit);
		assert_eq!(OverrideState::Inherit.cycle(false), OverrideState::Load);
		assert_eq!(OverrideState::Load.cycle(false), OverrideState::Unload);
		assert_eq!(OverrideState::Unload.cycle(false), OverrideState::Inherit);
	}
	#[test]
	fn workspace_extension_toggle_materializes_then_clears_scoped_delta() {
		let extension = id();
		let mut model = SelectorModel {
			scope:           WriteScope::Workspace,
			items:           vec![SelectorItem {
				key:              ItemKey::Extension { id: extension.clone() },
				label:            extension.clone(),
				default_enabled:  true,
				client_available: true,
			}],
			selected:        0,
			client:          ExtensionOverlay::default(),
			workspace:       ExtensionOverlay::default(),
			dirty_client:    false,
			dirty_workspace: false,
		};

		model.toggle_selected();
		assert_eq!(extension_override(&model.workspace, &extension), OverrideState::Unload);
		model.toggle_selected();
		assert_eq!(extension_override(&model.workspace, &extension), OverrideState::Load);
		model.toggle_selected();
		assert_eq!(extension_override(&model.workspace, &extension), OverrideState::Inherit);
		assert!(!model.workspace.enabled.contains(&extension));
		assert!(!model.workspace.disabled.contains(&extension));
		assert!(model.dirty_workspace);
	}

	#[test]
	fn scope_switch_keeps_a_visible_selection() {
		let mut model = SelectorModel {
			scope:           WriteScope::Workspace,
			items:           vec![
				SelectorItem {
					key:              ItemKey::Extension { id: sf!("project.only") },
					label:            sf!("project.only"),
					default_enabled:  true,
					client_available: false,
				},
				SelectorItem {
					key:              ItemKey::Extension { id: id() },
					label:            id(),
					default_enabled:  true,
					client_available: true,
				},
			],
			selected:        0,
			client:          ExtensionOverlay::default(),
			workspace:       ExtensionOverlay::default(),
			dirty_client:    false,
			dirty_workspace: false,
		};
		model.switch_scope();
		assert_eq!(model.scope, WriteScope::Client);
		assert_eq!(model.selected, 1);
		assert!(model.selected_item().is_some());
		model.switch_scope();
		assert_eq!(model.selected, 1);
		assert!(model.selected_item().is_some());
	}

	#[test]
	fn resource_delta_replaces_only_the_exact_forced_entry() {
		let mut overlay = ExtensionOverlay::default();
		overlay.resources.insert(id(), PackageResourceFilter {
			autoload: false,
			skills: Some(vec![sf!("skills/**"), sf!("+skills/a/SKILL.md"), sf!("-skills/b/SKILL.md")]),
			..PackageResourceFilter::default()
		});
		set_resource_override(
			&mut overlay,
			&id(),
			ResourceFamily::Skills,
			&sf!("skills/a/SKILL.md"),
			OverrideState::Unload,
			WriteScope::Workspace,
		);
		let patterns = overlay.resources[&id()]
			.patterns(ResourceFamily::Skills)
			.expect("skills filter");
		assert_eq!(patterns, [
			sf!("skills/**"),
			sf!("-skills/b/SKILL.md"),
			sf!("-skills/a/SKILL.md")
		]);
	}
	#[test]
	fn workspace_resource_toggle_uses_delta_only_filter_and_returns_to_inherit() {
		let extension = id();
		let path = sf!("skills/review/SKILL.md");
		let mut model = SelectorModel {
			scope:           WriteScope::Workspace,
			items:           vec![SelectorItem {
				key:              ItemKey::Resource {
					id:     extension.clone(),
					family: ResourceFamily::Skills,
					path:   path.clone(),
				},
				label:            sf!("review"),
				default_enabled:  true,
				client_available: true,
			}],
			selected:        0,
			client:          ExtensionOverlay::default(),
			workspace:       ExtensionOverlay::default(),
			dirty_client:    false,
			dirty_workspace: false,
		};

		model.toggle_selected();
		let filter = &model.workspace.resources[&extension];
		assert!(!filter.autoload);
		assert_eq!(filter.patterns(ResourceFamily::Skills), Some([sf!("-{path}")].as_slice()));
		model.toggle_selected();
		assert_eq!(
			model.workspace.resources[&extension].patterns(ResourceFamily::Skills),
			Some([sf!("+{path}")].as_slice()),
		);
		model.toggle_selected();
		assert!(!model.workspace.resources.contains_key(&extension));
	}

	#[test]
	fn persistence_writes_one_scoped_extensions_delta_and_preserves_other_tables() {
		let temp = tempfile::tempdir().expect("temporary config");
		let path = temp.path().join("config.toml");
		std::fs::write(&path, "[appearance]\ntheme = \"night\"\n").expect("seed config");
		let mut overlay = ExtensionOverlay::default();
		set_extension_override(&mut overlay, &id(), OverrideState::Unload);
		set_resource_override(
			&mut overlay,
			&id(),
			ResourceFamily::Prompts,
			&sf!("prompts/review.md"),
			OverrideState::Load,
			WriteScope::Workspace,
		);
		write_extension_overlay(&path, Scope::Workspace, &overlay).expect("persist overlay");
		let document = read_document(&path).expect("read config");
		assert_eq!(document["appearance"]["theme"].as_str(), Some("night"));
		let loaded = read_extension_overlay(&path, Scope::Workspace).expect("read overlay");
		assert!(loaded.disabled.contains(&id()));
		assert_eq!(
			loaded.resources[&id()].patterns(ResourceFamily::Prompts),
			Some([sf!("+prompts/review.md")].as_slice()),
		);
	}
}
