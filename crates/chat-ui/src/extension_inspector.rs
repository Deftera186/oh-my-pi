//! Retained `/extensions` catalog and inspector overlay.

use std::collections::{BTreeMap, BTreeSet};

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Icon, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};
use serde_json::Value;
use strum::{Display, EnumString, IntoStaticStr};
use xutf::IntoAnsiStripped as _;

use crate::{OverlayPanel, panel_divider};

const FRAME_ROWS: u16 = 6;
const MIN_INSPECTOR_ROWS: u16 = 5;
const INLINE_ARGUMENT_LIMIT: usize = 3;
const COLLAPSED_CATALOG_ITEMS: usize = 8;
const COLLAPSED_TEXT_LINES: usize = 3;

/// Stable extension family shown by the inspector.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, Ord, PartialEq, PartialOrd)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ExtensionKind {
	/// Static native extension manifest.
	Extension,
	/// Custom tool declaration.
	Tool,
	/// MCP server declaration.
	Mcp,
	/// Skill document.
	Skill,
	/// Rule document.
	Rule,
	/// Slash-command template.
	SlashCommand,
	/// Hook declaration.
	Hook,
	/// Reusable prompt.
	Prompt,
	/// Context document.
	ContextFile,
	/// File-targeted instruction.
	Instruction,
}

/// Effective registry disposition for one declaration row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionDisposition {
	/// Effective winning declaration.
	Winner,
	/// Winning declaration disabled by item or provider policy.
	Disabled {
		/// Short typed policy label.
		reason: Str,
	},
	/// Losing declaration retained for provenance inspection.
	Shadowed {
		/// Winning source label or path.
		by: Str,
	},
}

/// Source facts retained from discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionOrigin {
	/// Stable discovery provider ID.
	pub provider_id:   Str,
	/// Human-readable provider name.
	pub provider_name: Str,
	/// Canonical source file or manifest path.
	pub path:          Str,
	/// Source scope (`project`, `user`, `package`, `native`, or `built-in`).
	pub scope:         Str,
	/// Project label used in compact list hints.
	pub project:       Option<Str>,
	/// Whether the source authority forbids local mutation.
	pub read_only:     bool,
}

/// One live custom tool from a single session catalog snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveToolView {
	/// Runtime tool name.
	pub name:         Str,
	/// Optional runtime display label.
	pub label:        Option<Str>,
	/// Optional runtime description.
	pub description:  Option<Str>,
	/// JSON Schema input contract.
	pub input_schema: Value,
	/// Authoritative originating source file, when known.
	pub source_path:  Option<Str>,
	/// Whether the tool is hidden from default discovery.
	pub hidden:       bool,
	/// Runtime source class (`extension`, `builtin`, `mcp`, or `sdk`).
	pub source:       Str,
}

/// One MCP tool catalog entry.
#[derive(Clone, Debug, PartialEq)]
pub struct McpToolView {
	/// Protocol tool name.
	pub name:         Str,
	/// Optional protocol title.
	pub title:        Option<Str>,
	/// Optional protocol description.
	pub description:  Option<Str>,
	/// MCP `inputSchema`.
	pub input_schema: Value,
}

/// One MCP resource or prompt catalog entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpCatalogEntry {
	/// Protocol entry name.
	pub name:        Str,
	/// Optional protocol title.
	pub title:       Option<Str>,
	/// Optional protocol description.
	pub description: Option<Str>,
}

/// Live MCP lifecycle state.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum McpHealth {
	/// Transport or initialization is in progress.
	Connecting,
	/// Server and its tool catalog are ready.
	Connected,
	/// Declaration is enabled but no live connection exists.
	Disconnected,
	/// Declaration is not enabled.
	Inactive,
	/// Connection failed or its restart breaker opened.
	Failed,
}

/// Immutable live MCP catalog captured for one server update.
#[derive(Clone, Debug, PartialEq)]
pub struct McpLiveSnapshot {
	/// Declared server name used for the join.
	pub server:           Str,
	/// Current lifecycle health.
	pub health:           McpHealth,
	/// Monotone connection generation.
	pub generation:       u64,
	/// Monotone definition catalog epoch.
	pub definition_epoch: u64,
	/// Server implementation name.
	pub implementation:   Option<Str>,
	/// Server implementation version.
	pub version:          Option<Str>,
	/// Server-provided title.
	pub title:            Option<Str>,
	/// Server-provided description.
	pub description:      Option<Str>,
	/// Initialize instructions.
	pub instructions:     Option<Str>,
	/// Live tool catalog.
	pub tools:            Vec<McpToolView>,
	/// Live resource catalog.
	pub resources:        Vec<McpCatalogEntry>,
	/// Live prompt catalog.
	pub prompts:          Vec<McpCatalogEntry>,
}

/// Kind-specific declared inspector facts.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionDetail {
	/// No additional typed content.
	None,
	/// Declared custom-tool schema before the live join.
	Tool {
		/// Static description.
		description:  Option<Str>,
		/// Static input schema.
		input_schema: Value,
	},
	/// MCP connection plumbing.
	Mcp {
		/// Transport label.
		transport: Str,
		/// Command or URL.
		endpoint:  Option<Str>,
		/// Static command arguments.
		args:      Vec<Str>,
		/// Number of declared environment variables.
		env_count: usize,
	},
	/// Slash-command metadata and parsed body.
	SlashCommand {
		/// Parsed frontmatter description.
		description:   Option<Str>,
		/// Parsed argument hint.
		argument_hint: Option<Str>,
		/// Body with frontmatter removed, including an intentionally empty body.
		body:          Str,
	},
	/// Named preview section used by rules, skills, prompts, and instructions.
	Document {
		/// Preview heading.
		heading: Str,
		/// Parsed body.
		body:    Str,
		/// Compact applicability or discovery facts.
		facts:   Vec<(Str, Str)>,
	},
	/// Hook timing and target.
	Hook {
		/// Hook phase.
		phase: Str,
		/// Tool selector.
		tool:  Str,
	},
}

/// One declared extension row before live session catalogs are joined.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionRow {
	/// Stable row identity; source path distinguishes same-name losers.
	pub id:          Str,
	/// Capability family.
	pub kind:        ExtensionKind,
	/// Stable capability name.
	pub name:        Str,
	/// Optional human-readable description.
	pub description: Option<Str>,
	/// Discovery provenance.
	pub origin:      ExtensionOrigin,
	/// Winner, disabled winner, or shadowed loser.
	pub disposition: ExtensionDisposition,
	/// Kind-specific static facts.
	pub detail:      ExtensionDetail,
	/// Live tools joined from the same source file.
	pub live_tools:  Vec<LiveToolView>,
	/// Live MCP snapshot, joined only onto the effective winner.
	pub mcp:         Option<McpLiveSnapshot>,
}

impl ExtensionRow {
	/// Returns whether this row can request an enablement change.
	pub fn toggleable(&self) -> bool {
		!self.origin.read_only
			&& !matches!(self.disposition, ExtensionDisposition::Shadowed { .. })
			&& !matches!(
				self.kind,
				ExtensionKind::ContextFile | ExtensionKind::Prompt | ExtensionKind::Instruction
			)
	}

	/// Returns the enablement value requested by a toggle.
	pub fn toggled_enabled(&self) -> Option<bool> {
		self
			.toggleable()
			.then(|| matches!(self.disposition, ExtensionDisposition::Disabled { .. }))
	}
}

/// Complete immutable catalog used by one render/listing generation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionSnapshot {
	/// Declared rows in registry precedence order.
	pub rows:       Vec<ExtensionRow>,
	/// Generation shared by declared and live facts.
	pub generation: u64,
}

impl ExtensionSnapshot {
	/// Joins every live tool from one already-captured runtime catalog.
	pub fn join_live_tools(&mut self, live_tools: &[LiveToolView]) {
		let mut by_path = BTreeMap::<String, Vec<LiveToolView>>::new();
		let mut exact = BTreeMap::<Str, LiveToolView>::new();
		for tool in live_tools {
			if let Some(path) = tool.source_path.as_deref() {
				by_path
					.entry(normalize_source_path(path))
					.or_default()
					.push(tool.clone());
			} else if tool.source.as_str() == "extension" {
				exact.insert(tool.name.clone(), tool.clone());
			}
		}
		for row in &mut self.rows {
			row.live_tools.clear();
			if row.kind != ExtensionKind::Tool
				|| matches!(row.disposition, ExtensionDisposition::Shadowed { .. })
			{
				continue;
			}
			if let Some(tools) = by_path.get(&normalize_source_path(row.origin.path.as_str())) {
				row.live_tools.clone_from(tools);
			} else if let Some(tool) = exact.get(&row.name) {
				row.live_tools.push(tool.clone());
			}
		}
	}

	/// Replaces one server's live catalog without dropping unrelated connected
	/// servers.
	pub fn merge_mcp(&mut self, update: McpLiveSnapshot) {
		for row in &mut self.rows {
			if row.kind == ExtensionKind::Mcp
				&& row.name == update.server
				&& matches!(row.disposition, ExtensionDisposition::Winner)
			{
				row.mcp = Some(update.clone());
			}
		}
	}

	/// Drops live MCP catalogs owned by a provider after provider disable.
	pub fn drop_provider_mcp(&mut self, provider_id: &str) {
		for row in &mut self.rows {
			if row.kind == ExtensionKind::Mcp && row.origin.provider_id == provider_id {
				row.mcp = None;
			}
		}
	}
}

/// Snapshot authority used to guarantee one catalog read per refresh.
pub trait ExtensionCatalogSource {
	/// Captures declared rows and every live catalog once.
	fn snapshot(&self) -> ExtensionSnapshot;
}

/// Action emitted by the retained extension inspector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionInspectorEvent {
	/// Input was consumed and the inspector remains open.
	Consumed,
	/// Close the inspector.
	Close,
	/// Request an item-level enablement change.
	Toggle {
		/// Stable row identity.
		id:      Str,
		/// Requested enabled value.
		enabled: bool,
	},
}

/// Retained two-pane `/extensions` overlay.
pub struct ExtensionInspector {
	ui:             Ui,
	ctx:            UiContext,
	options:        OverlayOptions,
	snapshot:       ExtensionSnapshot,
	selected:       usize,
	expanded:       bool,
	inspector_top:  usize,
	inspector_rows: u16,
	width:          u16,
}

impl ExtensionInspector {
	/// Opens the inspector over one immutable catalog snapshot.
	pub fn open(snapshot: ExtensionSnapshot, ctx: &UiContext) -> Self {
		let width = 100;
		let inspector_rows = 12;
		let ui = build(&snapshot, 0, false, 0, inspector_rows, width, ctx);
		let mut inspector = Self {
			ui,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Pct(100))
				.z(20),
			snapshot,
			selected: 0,
			expanded: false,
			inspector_top: 0,
			inspector_rows,
			width,
		};
		inspector.ui.focus_first();
		inspector
	}

	/// Opens from exactly one source snapshot.
	pub fn open_from(source: &dyn ExtensionCatalogSource, ctx: &UiContext) -> Self {
		Self::open(source.snapshot(), ctx)
	}

	/// Replaces declared and live facts while preserving selection by stable ID.
	pub fn update_snapshot(&mut self, snapshot: ExtensionSnapshot) {
		let selected = self
			.snapshot
			.rows
			.get(self.selected)
			.map(|row| row.id.clone());
		self.snapshot = snapshot;
		self.selected = selected
			.as_ref()
			.and_then(|id| self.snapshot.rows.iter().position(|row| row.id == *id))
			.unwrap_or(0)
			.min(self.snapshot.rows.len().saturating_sub(1));
		let next = self.snapshot.rows.get(self.selected).map(|row| &row.id);
		if selected.as_ref() != next {
			self.expanded = false;
			self.inspector_top = 0;
		}
		self.clamp_inspector();
		self.rebuild();
	}

	/// Captures and installs one new source snapshot.
	pub fn refresh_from(&mut self, source: &dyn ExtensionCatalogSource) {
		self.update_snapshot(source.snapshot());
	}

	/// Merges a connection/catalog event and repaints without replacing other
	/// servers.
	pub fn update_mcp(&mut self, update: McpLiveSnapshot) {
		self.snapshot.merge_mcp(update);
		self.rebuild();
	}

	/// Clears live tools for a provider-disable transition and repaints.
	pub fn provider_disabled(&mut self, provider_id: &str) {
		self.snapshot.drop_provider_mcp(provider_id);
		self.rebuild();
	}

	/// Routes keyboard selection, toggle, expansion, paging, and close.
	pub fn handle_key(&mut self, key: Key) -> ExtensionInspectorEvent {
		match key {
			Key::Esc => return ExtensionInspectorEvent::Close,
			Key::PageUp => {
				self.page_inspector(-1);
				return ExtensionInspectorEvent::Consumed;
			},
			Key::PageDown => {
				self.page_inspector(1);
				return ExtensionInspectorEvent::Consumed;
			},
			Key::Ctrl('o') => {
				self.expanded = !self.expanded;
				self.inspector_top = 0;
				self.rebuild();
				return ExtensionInspectorEvent::Consumed;
			},
			Key::Char('j') => {
				let event = self.ui.handle_key(Key::Down);
				return self.route(event);
			},
			Key::Char('k') => {
				let event = self.ui.handle_key(Key::Up);
				return self.route(event);
			},
			Key::Enter | Key::Space => {
				if let Some(row) = self.snapshot.rows.get(self.selected)
					&& let Some(enabled) = row.toggled_enabled()
				{
					return ExtensionInspectorEvent::Toggle { id: row.id.clone(), enabled };
				}
				return ExtensionInspectorEvent::Consumed;
			},
			_ => {},
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pointer selection and outside-click dismissal.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> ExtensionInspectorEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => ExtensionInspectorEvent::Close,
			None => ExtensionInspectorEvent::Consumed,
		}
	}

	/// Returns the responsive full-width overlay layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let rows = viewport
			.height
			.saturating_sub(FRAME_ROWS)
			.max(MIN_INSPECTOR_ROWS);
		if rows != self.inspector_rows || viewport.width != self.width {
			self.inspector_rows = rows;
			self.width = viewport.width;
			self.clamp_inspector();
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&mut self, event: UiEvent) -> ExtensionInspectorEvent {
		match event {
			UiEvent::Cancel => ExtensionInspectorEvent::Close,
			UiEvent::Changed { id, value } | UiEvent::Highlighted { id, value }
				if id.as_str() == "extensions-list" =>
			{
				if let Ok(index) = value.as_str().parse::<usize>()
					&& index != self.selected
				{
					self.selected = index.min(self.snapshot.rows.len().saturating_sub(1));
					self.expanded = false;
					self.inspector_top = 0;
					self.rebuild();
				}
				ExtensionInspectorEvent::Consumed
			},
			_ => ExtensionInspectorEvent::Consumed,
		}
	}

	fn page_inspector(&mut self, direction: isize) {
		let total = detail_lines(self.snapshot.rows.get(self.selected), self.expanded).len();
		let page = usize::from(self.inspector_rows.saturating_sub(1).max(1));
		let max = total.saturating_sub(usize::from(self.inspector_rows));
		self.inspector_top = if direction < 0 {
			self.inspector_top.saturating_sub(page)
		} else {
			self.inspector_top.saturating_add(page).min(max)
		};
		self.rebuild();
	}

	fn clamp_inspector(&mut self) {
		let total = detail_lines(self.snapshot.rows.get(self.selected), self.expanded).len();
		self.inspector_top = self
			.inspector_top
			.min(total.saturating_sub(usize::from(self.inspector_rows)));
	}

	fn rebuild(&mut self) {
		self.clamp_inspector();
		self.ui = build(
			&self.snapshot,
			self.selected,
			self.expanded,
			self.inspector_top,
			self.inspector_rows,
			self.width,
			&self.ctx,
		);
		self.ui.focus_first();
	}
}

#[derive(Clone)]
struct InspectorLine {
	text:  Str,
	color: &'static str,
	bold:  bool,
}

impl InspectorLine {
	fn plain(text: impl AsRef<str>) -> Self {
		Self { text: sanitize_text(text.as_ref()), color: "fg", bold: false }
	}

	fn muted(text: impl AsRef<str>) -> Self {
		Self { text: sanitize_text(text.as_ref()), color: "muted", bold: false }
	}

	fn accent(text: impl AsRef<str>) -> Self {
		Self { text: sanitize_text(text.as_ref()), color: "accent", bold: true }
	}

	fn status(text: impl AsRef<str>, color: &'static str) -> Self {
		Self { text: sanitize_line(text.as_ref()), color, bold: true }
	}
}

fn build(
	snapshot: &ExtensionSnapshot,
	selected: usize,
	expanded: bool,
	inspector_top: usize,
	inspector_rows: u16,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let list_width = if width >= 78 {
		width.saturating_mul(2) / 5
	} else {
		width.saturating_sub(4)
	};
	let list_height = inspector_rows;
	let list = snapshot
		.rows
		.iter()
		.enumerate()
		.map(|(index, row)| {
			let status = disposition_glyph(row, ctx);
			let hint = list_hint(row);
			(index, sanitize_line(row.name.as_str()), status, hint)
		})
		.collect::<Vec<_>>();
	let details = detail_lines(snapshot.rows.get(selected), expanded);
	let visible = details
		.into_iter()
		.skip(inspector_top)
		.take(usize::from(inspector_rows))
		.collect::<Vec<_>>();
	let summary = sf!("{} extensions · generation {}", snapshot.rows.len(), snapshot.generation);
	let root = if width >= 78 {
		OverlayPanel::new("Extensions").child(dom! {
			<col>
				<text dim truncate>{summary}</text>
				<row gap=2>
					<select id="extensions-list" w={list_width} h={list_height}>
						for (index, name, status, hint) in list {
							<option value={sf!("{index}")} label={name.clone()} recommended={index == selected}>
								<td><pre fg={status.1}>{status.0}</pre></td>
								<td truncate grow><pre>{name}</pre></td>
								<td truncate><pre fg=muted>{hint}</pre></td>
							</option>
						}
					</select>
					<col h={inspector_rows} grow>
						for line in visible {
							<text fg={line.color} bold={line.bold} wrap>{line.text}</text>
						}
					</col>
				</row>
				{panel_divider()}
				<text dim truncate>{"↑/↓/j/k select · Space toggle · PgUp/PgDn inspect · Ctrl+O expand · Esc close"}</text>
			</col>
		})
	} else {
		OverlayPanel::new("Extensions").child(dom! {
			<col>
				<text dim truncate>{summary}</text>
				<select id="extensions-list" h={list_height}>
					for (index, name, status, hint) in list {
						<option value={sf!("{index}")} label={name.clone()} recommended={index == selected}>
							<td><pre fg={status.1}>{status.0}</pre></td>
							<td truncate grow><pre>{name}</pre></td>
							<td truncate><pre fg=muted>{hint}</pre></td>
						</option>
					}
				</select>
				{panel_divider()}
				<text dim truncate>{"↑/↓/j/k select · Space toggle · PgUp/PgDn inspect · Ctrl+O expand · Esc"}</text>
			</col>
		})
	};
	Ui::from_root(root, width, ctx.clone())
}

fn disposition_glyph(row: &ExtensionRow, ctx: &UiContext) -> (Str, &'static str) {
	match &row.disposition {
		ExtensionDisposition::Winner => (Str::new(ctx.charset.icon(Icon::Enabled)), "success"),
		ExtensionDisposition::Disabled { .. } => {
			(Str::new(ctx.charset.icon(Icon::Disabled)), "muted")
		},
		ExtensionDisposition::Shadowed { .. } => {
			(Str::new(ctx.charset.icon(Icon::Shadowed)), "warning")
		},
	}
}

fn list_hint(row: &ExtensionRow) -> Str {
	let detail = match row.kind {
		ExtensionKind::Tool if row.live_tools.len() > 1 => {
			Some(sf!("{} tools", row.live_tools.len()))
		},
		ExtensionKind::Tool if row.live_tools.iter().any(|tool| tool.hidden) => Some(sf!("hidden")),
		ExtensionKind::Mcp => row.mcp.as_ref().map(mcp_hint),
		ExtensionKind::Skill if matches!(&row.detail, ExtensionDetail::Document { facts, .. } if facts.iter().any(|(key, value)| key.as_str() == "discovery" && value.as_str() == "hidden")) => {
			Some(sf!("hidden"))
		},
		ExtensionKind::SlashCommand => Some(sf!("/{}", row.name)),
		ExtensionKind::ContextFile => None,
		_ => None,
	};
	let project = row
		.origin
		.project
		.as_ref()
		.map(|value| sanitize_line(value.as_str()));
	match (detail, project) {
		(Some(detail), Some(project)) if !detail.eq_ignore_ascii_case(project.as_str()) => {
			sanitize_line(sf!("{detail} · {project}").as_str())
		},
		(Some(detail), _) => sanitize_line(detail.as_str()),
		(None, Some(project)) => project,
		(None, None) => Str::default(),
	}
}

fn mcp_hint(snapshot: &McpLiveSnapshot) -> Str {
	match snapshot.health {
		McpHealth::Connected => {
			let mut parts = vec![sf!("{} tool{}", snapshot.tools.len(), plural(snapshot.tools.len()))];
			if !snapshot.resources.is_empty() {
				parts.push(sf!(
					"{} resource{}",
					snapshot.resources.len(),
					plural(snapshot.resources.len())
				));
			}
			if !snapshot.prompts.is_empty() {
				parts.push(sf!("{} prompt{}", snapshot.prompts.len(), plural(snapshot.prompts.len())));
			}
			Str::from(
				parts
					.iter()
					.map(Str::as_str)
					.collect::<Vec<_>>()
					.join(" · "),
			)
		},
		McpHealth::Connecting => Str::new_static("connecting"),
		McpHealth::Disconnected => Str::new_static("unavailable"),
		McpHealth::Inactive => Str::new_static("inactive"),
		McpHealth::Failed => Str::new_static("failed"),
	}
}

fn detail_lines(row: Option<&ExtensionRow>, expanded: bool) -> Vec<InspectorLine> {
	let Some(row) = row else {
		return vec![InspectorLine::muted("No extensions discovered")];
	};
	let mut out = Vec::new();
	out.push(InspectorLine::accent(row.name.as_str()));
	match &row.disposition {
		ExtensionDisposition::Winner => out.push(InspectorLine::status("Active", "success")),
		ExtensionDisposition::Disabled { reason } => {
			out.push(InspectorLine::status(sf!("Disabled · {reason}"), "muted"));
		},
		ExtensionDisposition::Shadowed { by } => {
			out.push(InspectorLine::status(sf!("Shadowed by {by}"), "warning"));
		},
	}
	if let Some(description) = &row.description {
		push_collapsible_text(&mut out, description.as_str(), expanded);
	}
	out.push(InspectorLine::muted(sf!("via {} ({})", row.origin.provider_name, row.origin.scope)));
	out.push(InspectorLine::muted(row.origin.path.as_str()));
	out.push(InspectorLine::plain(""));
	match &row.detail {
		ExtensionDetail::None => {},
		ExtensionDetail::Tool { description, input_schema } => {
			if let Some(description) = description {
				push_collapsible_text(&mut out, description.as_str(), expanded);
			}
			let tools = if row.live_tools.is_empty() {
				vec![LiveToolView {
					name:         row.name.clone(),
					label:        None,
					description:  None,
					input_schema: input_schema.clone(),
					source_path:  Some(row.origin.path.clone()),
					hidden:       false,
					source:       Str::new_static("extension"),
				}]
			} else {
				row.live_tools.clone()
			};
			push_tools(&mut out, &tools, expanded);
		},
		ExtensionDetail::Mcp { transport, endpoint, args, env_count } => {
			push_mcp(&mut out, row, expanded);
			out.push(InspectorLine::muted("Connection"));
			out.push(InspectorLine::plain(sf!("  transport  {transport}")));
			if let Some(endpoint) = endpoint {
				out.push(InspectorLine::plain(sf!("  endpoint   {endpoint}")));
			}
			if !args.is_empty() {
				out.push(InspectorLine::muted(sf!(
					"  args       {}",
					args.iter().map(Str::as_str).collect::<Vec<_>>().join(" ")
				)));
			}
			if *env_count > 0 {
				out.push(InspectorLine::muted(sf!("  env        {env_count} defined")));
			}
		},
		ExtensionDetail::SlashCommand { description, argument_hint, body } => {
			if let Some(description) = description {
				push_collapsible_text(&mut out, description.as_str(), expanded);
			}
			out.push(InspectorLine::muted("Invocation"));
			out.push(InspectorLine::accent(sf!("  /{}", row.name)));
			if let Some(hint) = argument_hint {
				out.push(InspectorLine::muted(sf!("  hint       {hint}")));
			}
			if body.contains("$ARGUMENTS") {
				out.push(InspectorLine::muted("  accepts $ARGUMENTS"));
			}
			out.push(InspectorLine::muted("Template"));
			push_preview(&mut out, body.as_str(), expanded);
		},
		ExtensionDetail::Document { heading, body, facts } => {
			for (label, value) in facts {
				out.push(InspectorLine::plain(sf!("  {label:<10} {value}")));
			}
			if !facts.is_empty() {
				out.push(InspectorLine::plain(""));
			}
			out.push(InspectorLine::muted(heading.as_str()));
			push_preview(&mut out, body.as_str(), expanded);
		},
		ExtensionDetail::Hook { phase, tool } => {
			out.push(InspectorLine::muted("Hook"));
			out.push(InspectorLine::plain(sf!("  phase      {phase}")));
			out.push(InspectorLine::plain(sf!("  tool       {tool}")));
		},
	}
	out
}

fn push_tools(out: &mut Vec<InspectorLine>, tools: &[LiveToolView], expanded: bool) {
	out.push(InspectorLine::muted(if tools.len() == 1 {
		"Arguments"
	} else {
		"Tools"
	}));
	for tool in tools {
		if tools.len() > 1 {
			out.push(InspectorLine::accent(sf!("  {}", tool.name)));
			if let Some(label) = &tool.label
				&& label != &tool.name
			{
				out.push(InspectorLine::muted(sf!("    {label}")));
			}
			if let Some(description) = &tool.description {
				out.push(InspectorLine::plain(sf!("    {description}")));
			}
		}
		let params = schema_params(&tool.input_schema);
		if params.is_empty() && tools.len() == 1 {
			out.push(InspectorLine::muted("  (no arguments)"));
		} else if expanded || params.len() <= INLINE_ARGUMENT_LIMIT {
			for param in params {
				out.push(InspectorLine::plain(sf!(
					"  {:<12} {:<12} {}",
					param.name,
					param.kind,
					param.requirement
				)));
				if let Some(description) = param.description {
					out.push(InspectorLine::muted(sf!("    {description}")));
				}
			}
		} else {
			out.push(InspectorLine::muted(sf!("    {} args · Ctrl+O to expand", params.len())));
		}
	}
}

fn push_mcp(out: &mut Vec<InspectorLine>, row: &ExtensionRow, expanded: bool) {
	let Some(snapshot) = row.mcp.as_ref() else {
		out.push(InspectorLine::status(
			if matches!(row.disposition, ExtensionDisposition::Disabled { .. }) {
				"Inactive"
			} else {
				"Not connected"
			},
			"muted",
		));
		return;
	};
	out.push(InspectorLine::status(snapshot.health.to_string(), match snapshot.health {
		McpHealth::Connected => "success",
		McpHealth::Connecting => "accent",
		McpHealth::Failed => "error",
		McpHealth::Disconnected | McpHealth::Inactive => "muted",
	}));
	if let Some(title) = &snapshot.title {
		out.push(InspectorLine::accent(title.as_str()));
	}
	if let Some(implementation) = &snapshot.implementation {
		out.push(InspectorLine::muted(match &snapshot.version {
			Some(version) => sf!("{implementation} {version}"),
			None => implementation.clone(),
		}));
	}
	if let Some(description) = &snapshot.description {
		push_collapsible_text(out, description.as_str(), expanded);
	}
	if let Some(instructions) = &snapshot.instructions {
		push_collapsible_text(out, instructions.as_str(), expanded);
	}
	let tools = snapshot
		.tools
		.iter()
		.map(|tool| LiveToolView {
			name:         tool.name.clone(),
			label:        tool.title.clone(),
			description:  tool.description.clone(),
			input_schema: tool.input_schema.clone(),
			source_path:  None,
			hidden:       false,
			source:       Str::new_static("mcp"),
		})
		.collect::<Vec<_>>();
	if !tools.is_empty() {
		let shown = if expanded {
			tools.len()
		} else {
			tools.len().min(COLLAPSED_CATALOG_ITEMS)
		};
		push_tools(out, &tools[..shown], expanded);
		if shown < tools.len() {
			out.push(InspectorLine::muted(sf!("  … {} more · Ctrl+O to expand", tools.len() - shown)));
		}
	}
	push_catalog(out, "Resources", &snapshot.resources, expanded);
	push_catalog(out, "Prompts", &snapshot.prompts, expanded);
}

fn push_catalog(
	out: &mut Vec<InspectorLine>,
	heading: &str,
	entries: &[McpCatalogEntry],
	expanded: bool,
) {
	if entries.is_empty() {
		return;
	}
	out.push(InspectorLine::muted(heading));
	let shown = if expanded {
		entries.len()
	} else {
		entries.len().min(COLLAPSED_CATALOG_ITEMS)
	};
	for entry in &entries[..shown] {
		out.push(InspectorLine::accent(sf!("  {}", entry.name)));
		if let Some(title) = &entry.title
			&& title != &entry.name
		{
			out.push(InspectorLine::muted(sf!("    {title}")));
		}
		if let Some(description) = &entry.description {
			out.push(InspectorLine::muted(sf!("    {description}")));
		}
	}
	if shown < entries.len() {
		out.push(InspectorLine::muted(sf!("  … {} more · Ctrl+O to expand", entries.len() - shown)));
	}
}

fn push_collapsible_text(out: &mut Vec<InspectorLine>, text: &str, expanded: bool) {
	let lines = sanitize_text(text)
		.lines()
		.map(Str::new)
		.collect::<Vec<_>>();
	let shown = if expanded {
		lines.len()
	} else {
		lines.len().min(COLLAPSED_TEXT_LINES)
	};
	out.extend(
		lines[..shown]
			.iter()
			.map(|line| InspectorLine::plain(line.as_str())),
	);
	if shown < lines.len() {
		out.push(InspectorLine::muted(sf!("… {} more · Ctrl+O to expand", lines.len() - shown)));
	}
}

fn push_preview(out: &mut Vec<InspectorLine>, body: &str, expanded: bool) {
	if body.is_empty() {
		out.push(InspectorLine::muted("  (empty)"));
		return;
	}
	let lines = sanitize_text(body)
		.lines()
		.map(Str::new)
		.collect::<Vec<_>>();
	let limit = if expanded {
		lines.len()
	} else {
		lines.len().min(COLLAPSED_CATALOG_ITEMS)
	};
	out.extend(
		lines[..limit]
			.iter()
			.map(|line| InspectorLine::plain(line.as_str())),
	);
	if limit < lines.len() {
		out.push(InspectorLine::muted(sf!("… {} more · Ctrl+O to expand", lines.len() - limit)));
	}
}

struct SchemaParam {
	name:        Str,
	kind:        Str,
	requirement: Str,
	description: Option<Str>,
}

fn schema_params(schema: &Value) -> Vec<SchemaParam> {
	let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
		return Vec::new();
	};
	let required = schema
		.get("required")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.collect::<BTreeSet<_>>();
	properties
		.iter()
		.map(|(name, value)| {
			let requirement = if required.contains(name.as_str()) {
				Str::new_static("Required")
			} else if let Some(default) = value.get("default") {
				sf!("Default: {}", compact_default(default))
			} else {
				Str::new_static("Optional")
			};
			SchemaParam {
				name:        sanitize_line(name),
				kind:        schema_type(value),
				requirement: sanitize_line(requirement.as_str()),
				description: value
					.get("description")
					.and_then(Value::as_str)
					.map(sanitize_text),
			}
		})
		.collect()
}

fn schema_type(value: &Value) -> Str {
	if let Some(kind) = value.get("type").and_then(Value::as_str) {
		if kind == "array" {
			return sf!(
				"array<{}>",
				value
					.get("items")
					.map(schema_type)
					.unwrap_or_else(|| Str::new_static("any"))
			);
		}
		return sanitize_line(kind);
	}
	if value.get("enum").is_some() {
		return Str::new_static("enum");
	}
	if value.get("oneOf").is_some() || value.get("anyOf").is_some() {
		return Str::new_static("union");
	}
	Str::new_static("any")
}

fn compact_default(value: &Value) -> Str {
	match value {
		Value::String(value) => sanitize_line(value),
		_ => serde_json::to_string(value)
			.map_or_else(|_| Str::new_static("?"), |value| sanitize_line(&value)),
	}
}

fn sanitize_text(text: &str) -> Str {
	let stripped = text.to_owned().into_ansi_stripped();
	let mut output = String::with_capacity(stripped.len());
	for grapheme in xutf::graphemes_str(&stripped) {
		if grapheme == "\t" {
			output.push_str("   ");
		} else if grapheme == "\n" || grapheme == "\r\n" {
			output.push('\n');
		} else if grapheme.chars().all(|character| !character.is_control()) {
			output.push_str(grapheme);
		}
	}
	Str::from(output)
}

fn sanitize_line(text: &str) -> Str {
	let text = sanitize_text(text);
	Str::from(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn normalize_source_path(path: &str) -> String {
	let replaced = path.replace('\\', "/");
	let unc = replaced.starts_with("//");
	let mut parts = Vec::new();
	for part in replaced.split('/') {
		match part {
			"" | "." => {},
			".." => {
				let _ = parts.pop();
			},
			_ => parts.push(part),
		}
	}
	let prefix = if unc {
		"//"
	} else if replaced.starts_with('/') {
		"/"
	} else {
		""
	};
	format!("{prefix}{}", parts.join("/"))
}

const fn plural(count: usize) -> &'static str {
	if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
	use std::cell::Cell;

	use omp_tui::{Size, UiContext, frame_text};
	use serde_json::json;

	use super::*;

	fn origin(path: &str) -> ExtensionOrigin {
		ExtensionOrigin {
			provider_id:   Str::new_static("native"),
			provider_name: Str::new_static("OMP project"),
			path:          Str::new(path),
			scope:         Str::new_static("project"),
			project:       Some(Str::new_static("demo")),
			read_only:     false,
		}
	}

	fn tool_row(id: &str, path: &str) -> ExtensionRow {
		ExtensionRow {
			id:          Str::new(id),
			kind:        ExtensionKind::Tool,
			name:        Str::new(id),
			description: None,
			origin:      origin(path),
			disposition: ExtensionDisposition::Winner,
			detail:      ExtensionDetail::Tool {
				description:  None,
				input_schema: json!({"type":"object","properties":{}}),
			},
			live_tools:  Vec::new(),
			mcp:         None,
		}
	}

	fn mcp_row(name: &str) -> ExtensionRow {
		ExtensionRow {
			id:          Str::new(name),
			kind:        ExtensionKind::Mcp,
			name:        Str::new(name),
			description: None,
			origin:      origin("/repo/.omp/mcp.json"),
			disposition: ExtensionDisposition::Winner,
			detail:      ExtensionDetail::Mcp {
				transport: Str::new_static("stdio"),
				endpoint:  Some(Str::new_static("server")),
				args:      Vec::new(),
				env_count: 0,
			},
			live_tools:  Vec::new(),
			mcp:         None,
		}
	}

	fn live_mcp(name: &str, tool: &str) -> McpLiveSnapshot {
		McpLiveSnapshot {
			server:           Str::new(name),
			health:           McpHealth::Connected,
			generation:       1,
			definition_epoch: 1,
			implementation:   Some(Str::new_static("fixture")),
			version:          Some(Str::new_static("1")),
			title:            None,
			description:      None,
			instructions:     None,
			tools:            vec![McpToolView {
				name:         Str::new(tool),
				title:        None,
				description:  None,
				input_schema: json!({"type":"object","properties":{}}),
			}],
			resources:        Vec::new(),
			prompts:          Vec::new(),
		}
	}

	#[test]
	fn joins_tools_by_file_including_unc_and_isolates_shadowed_rows() {
		let unc = r"\\server\share\.omp\tools\bundle.ts";
		let mut shadowed = tool_row("bundle-shadow", unc);
		shadowed.disposition = ExtensionDisposition::Shadowed { by: Str::new_static("bundle") };
		let mut snapshot = ExtensionSnapshot {
			rows:       vec![tool_row("bundle", unc), tool_row("other", "/tmp/other.ts"), shadowed],
			generation: 1,
		};
		snapshot.join_live_tools(&[
			LiveToolView {
				name:         Str::new_static("inspect"),
				label:        None,
				description:  None,
				input_schema: json!({}),
				source_path:  Some(Str::new_static("//server/share/.omp/tools/bundle.ts")),
				hidden:       false,
				source:       Str::new_static("extension"),
			},
			LiveToolView {
				name:         Str::new_static("builtin_collision"),
				label:        None,
				description:  None,
				input_schema: json!({}),
				source_path:  None,
				hidden:       false,
				source:       Str::new_static("builtin"),
			},
		]);
		assert_eq!(snapshot.rows[0].live_tools.len(), 1);
		assert!(snapshot.rows[1].live_tools.is_empty());
		assert!(snapshot.rows[2].live_tools.is_empty());
	}

	#[test]
	fn incremental_mcp_updates_keep_other_servers_and_provider_disable_drops_live_state() {
		let mut beta = mcp_row("beta");
		beta.origin.provider_id = Str::new_static("foreign");
		let mut snapshot =
			ExtensionSnapshot { rows: vec![mcp_row("alpha"), beta], generation: 1 };
		snapshot.merge_mcp(live_mcp("alpha", "a"));
		snapshot.merge_mcp(live_mcp("beta", "b"));
		assert_eq!(snapshot.rows[0].mcp.as_ref().unwrap().tools[0].name, "a");
		assert_eq!(snapshot.rows[1].mcp.as_ref().unwrap().tools[0].name, "b");
		snapshot.drop_provider_mcp("foreign");
		assert!(snapshot.rows[0].mcp.is_some());
		assert!(snapshot.rows[1].mcp.is_none());
	}

	#[test]
	fn disabled_shadowed_loser_is_not_toggleable() {
		let mut loser = mcp_row("same");
		loser.disposition = ExtensionDisposition::Shadowed { by: Str::new_static("winner") };
		assert!(!loser.toggleable());
		assert_eq!(loser.toggled_enabled(), None);
		let mut disabled = mcp_row("disabled");
		disabled.disposition = ExtensionDisposition::Disabled { reason: Str::new_static("item") };
		assert_eq!(disabled.toggled_enabled(), Some(true));
		disabled.origin.read_only = true;
		assert_eq!(disabled.toggled_enabled(), None);
	}

	#[test]
	fn schema_formatting_is_compact_and_single_line() {
		let params = schema_params(&json!({
			"type":"object",
			"required":["query"],
			"properties":{
				"query":{"type":"string\ninjected","description":"Search query"},
				"limit":{"type":"integer","default":"10\n20"},
				"flags":{"type":"array","items":{"type":"string"},"default":[]}
			}
		}));
		assert_eq!(params[0].kind, "string injected");
		assert_eq!(params[0].requirement, "Required");
		assert_eq!(params[1].requirement, "Default: 10 20");
		assert_eq!(params[2].kind, "array<string>");
		assert_eq!(params[2].requirement, "Default: []");
	}

	#[test]
	fn command_keeps_frontmatter_fields_and_empty_parsed_body() {
		let row = ExtensionRow {
			id:          Str::new_static("deploy"),
			kind:        ExtensionKind::SlashCommand,
			name:        Str::new_static("deploy"),
			description: None,
			origin:      origin("/repo/.omp/commands/deploy.md"),
			disposition: ExtensionDisposition::Winner,
			detail:      ExtensionDetail::SlashCommand {
				description:   Some(Str::new_static("Deploy a service")),
				argument_hint: Some(Str::new_static("<service>")),
				body:          Str::default(),
			},
			live_tools:  Vec::new(),
			mcp:         None,
		};
		let text = detail_lines(Some(&row), false)
			.into_iter()
			.map(|line| line.text)
			.collect::<Vec<_>>();
		assert!(text.iter().any(|line| line.contains("Deploy a service")));
		assert!(text.iter().any(|line| line.contains("<service>")));
		assert!(text.iter().any(|line| line.contains("(empty)")));
		assert!(!text.iter().any(|line| line.contains("description:")));
	}

	#[test]
	fn mcp_omits_empty_catalogs_and_shows_tool_arguments() {
		let mut row = mcp_row("alpha");
		let mut live = live_mcp("alpha", "search");
		live.tools[0].input_schema = json!({
			"type":"object",
			"required":["query"],
			"properties":{"query":{"type":"string"}}
		});
		row.mcp = Some(live);
		let text = detail_lines(Some(&row), false)
			.into_iter()
			.map(|line| line.text)
			.collect::<Vec<_>>()
			.join("\n");
		assert!(text.contains("query"));
		assert!(text.contains("Required"));
		assert!(!text.contains("Resources"));
		assert!(!text.contains("Prompts"));
	}

	struct CountingSource {
		calls:    Cell<usize>,
		snapshot: ExtensionSnapshot,
	}

	impl ExtensionCatalogSource for CountingSource {
		fn snapshot(&self) -> ExtensionSnapshot {
			self.calls.set(self.calls.get() + 1);
			self.snapshot.clone()
		}
	}

	#[test]
	fn listing_uses_one_snapshot_and_keyboard_pages_overflow() {
		let body = (0..60)
			.map(|index| sf!("line-{index}"))
			.collect::<Vec<_>>()
			.join("\n");
		let row = ExtensionRow {
			id:          Str::new_static("long"),
			kind:        ExtensionKind::Rule,
			name:        Str::new_static("long"),
			description: None,
			origin:      origin("/repo/.omp/rules/long.md"),
			disposition: ExtensionDisposition::Winner,
			detail:      ExtensionDetail::Document {
				heading: Str::new_static("Rule"),
				body:    Str::from(body),
				facts:   Vec::new(),
			},
			live_tools:  Vec::new(),
			mcp:         None,
		};
		let source = CountingSource {
			calls:    Cell::new(0),
			snapshot: ExtensionSnapshot { rows: vec![row], generation: 1 },
		};
		let mut inspector = ExtensionInspector::open_from(&source, &UiContext::default());
		assert_eq!(source.calls.get(), 1);
		let _ = inspector.layer(Size { width: 100, height: 18 });
		assert_eq!(inspector.handle_key(Key::Ctrl('o')), ExtensionInspectorEvent::Consumed);
		let before = frame_text(inspector.layer(Size { width: 100, height: 18 }).frame);
		assert!(before.contains("line-0"));
		assert_eq!(inspector.handle_key(Key::PageDown), ExtensionInspectorEvent::Consumed);
		let after = frame_text(inspector.layer(Size { width: 100, height: 18 }).frame);
		assert_ne!(before, after);
		assert!(inspector.inspector_top > 0);
	}

	#[test]
	fn flat_list_jk_aliases_directional_navigation() {
		let snapshot = ExtensionSnapshot {
			rows:       vec![
				tool_row("first", "/repo/first.ts"),
				tool_row("second", "/repo/second.ts"),
			],
			generation: 1,
		};
		let mut inspector = ExtensionInspector::open(snapshot, &UiContext::default());

		assert_eq!(inspector.handle_key(Key::Char('j')), ExtensionInspectorEvent::Consumed);
		assert_eq!(inspector.selected, 1);
		assert_eq!(inspector.handle_key(Key::Char('k')), ExtensionInspectorEvent::Consumed);
		assert_eq!(inspector.selected, 0);
	}

	#[test]
	fn context_hint_does_not_repeat_project() {
		let mut row = tool_row("AGENTS.md", "/repo/AGENTS.md");
		row.kind = ExtensionKind::ContextFile;
		row.origin.project = Some(Str::new_static("project"));
		assert_eq!(list_hint(&row), "project");
	}
}
