//! Fullscreen `/models` hub: role assignments, retry fallback chains, and
//! quick-cycle configuration over the live catalog.
//!
//! Layout mirrors pi's model hub: a sidebar of scopes (role management, all
//! models, one entry per provider — locked providers included, dimmed) beside
//! a filterable model browser. The Roles view manages assignments directly:
//! pick a role, pick a model, adjust thinking in an inline chip strip, or
//! clear the role back to auto-selection. Locked providers forward to the
//! `/login` flow. Session-only switching lives in the compact alt+p picker
//! ([`crate::picker::ModelPicker`]).

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Icon, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent,
	assets::provider_logo, dom,
};

use crate::{
	HubScope, LockedProviderRow, ModelHubData, ModelRow,
	overlays::{OverlayPanel, panel_divider},
};

const FRAME_ROWS: u16 = 6;
const SIDEBAR_MIN_WIDTH: u16 = 18;
const SIDEBAR_MAX_WIDTH: u16 = 26;

/// Action emitted by the retained models hub.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelHubEvent {
	/// Input was consumed and the hub remains open.
	Consumed,
	/// Close the hub.
	Close,
	/// Persist one role assignment.
	AssignRole {
		/// Role identifier.
		role:     Str,
		/// Catalog model key.
		selector: Str,
		/// Explicit thinking annotation.
		thinking: Option<Str>,
		/// Explicit persistence scope; `None` follows configured storage.
		scope:    Option<HubScope>,
	},
	/// Clear one configured role back to auto-selection.
	UnassignRole {
		/// Role identifier.
		role:  Str,
		/// Explicit persistence scope; `None` follows configured storage.
		scope: Option<HubScope>,
	},
	/// Replace one retry fallback chain; an empty chain clears the key.
	SetFallbackChain {
		/// Role, model key, or `provider/*` chain key.
		key:   Str,
		/// Ordered fallback selectors.
		chain: Vec<Str>,
	},
	/// Replace the quick-cycle role order.
	SetCycleOrder {
		/// Role names in cycle order.
		order: Vec<Str>,
	},
	/// Start the login flow for a locked provider.
	Login(Str),
}

/// Sidebar scope entry.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Entry {
	Roles,
	All,
	Provider { id: Str, name: Str, count: usize },
	Locked(usize),
}

impl Entry {
	fn key(&self, locked: &[LockedProviderRow]) -> Str {
		match self {
			Self::Roles => Str::new_static("roles"),
			Self::All => Str::new_static("all"),
			Self::Provider { id, .. } => sf!("provider:{id}"),
			Self::Locked(index) => locked
				.get(*index)
				.map_or_else(|| sf!("locked:{index}"), |row| sf!("locked:{}", row.id)),
		}
	}
}

/// One row of the Roles view.
#[derive(Clone, Debug, Eq, PartialEq)]
enum RolesRow {
	/// Index into [`ModelHubData::roles`].
	Role(usize),
	/// Header for a model/wildcard fallback chain.
	ChainKey(Str),
	/// One entry of a fallback chain (`key`, position, selector).
	Fallback {
		key:      Str,
		index:    usize,
		selector: Str,
	},
	Separator,
	NewRole,
	NewFallback,
}

/// What the model browser is currently picking for.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Assign {
	Role(Str),
	Fallback { key: Str, index: Option<usize> },
	FallbackKey,
}

/// Arrow-key ownership: the sidebar or the active body list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
	Scope,
	List,
}

#[derive(Clone, Debug)]
enum ChipAction {
	Assign { role: Str, scope: Option<HubScope> },
	Unassign { role: Str, scope: Option<HubScope> },
	FallbackModel,
	FallbackProvider,
	Scope(HubScope),
	Thinking(Option<Str>),
}

#[derive(Clone, Debug)]
struct Chip {
	label:  Str,
	color:  Str,
	action: ChipAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StripKind {
	Role,
	Scope,
	Thinking,
	FallbackKey,
}

#[derive(Clone, Debug)]
enum Strip {
	Chips {
		kind:            StripKind,
		/// Catalog key of the model the strip acts on.
		item:            Str,
		role:            Option<Str>,
		/// Persistence scope carried from the scope strip into thinking.
		scope:           Option<HubScope>,
		chips:           Vec<Chip>,
		index:           usize,
		return_to_roles: bool,
	},
	RoleName {
		value: String,
	},
}

/// Retained fullscreen models hub overlay.
pub struct ModelHub {
	ui:          Ui,
	ctx:         UiContext,
	options:     OverlayOptions,
	data:        ModelHubData,
	entries:     Vec<Entry>,
	active:      usize,
	focus:       Focus,
	query:       Str,
	roles_view:  Vec<RolesRow>,
	role_index:  usize,
	role_start:  usize,
	assigning:   Option<Assign>,
	strip:       Option<Strip>,
	/// Roles-view row to land on after the next data refresh.
	pending_row: Option<(Str, Option<Str>)>,
	body_rows:   u16,
	width:       u16,
}

impl ModelHub {
	/// Opens the hub over one backend-projected snapshot.
	pub fn open(data: ModelHubData, ctx: &UiContext) -> Self {
		let mut hub = Self {
			ui: Ui::from_root(dom! { <text/> }, 1, ctx.clone()),
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Pct(100))
				.z(20),
			data,
			entries: Vec::new(),
			active: 0,
			focus: Focus::Scope,
			query: Str::default(),
			roles_view: Vec::new(),
			role_index: 0,
			role_start: 0,
			assigning: None,
			strip: None,
			pending_row: None,
			body_rows: 18,
			width: 100,
		};
		hub.sync_derived();
		hub.active = hub
			.entries
			.iter()
			.position(|entry| *entry == Entry::Roles)
			.unwrap_or(0);
		hub.rebuild();
		hub
	}

	/// Replaces backend state after a mutation while preserving navigation.
	pub fn update(&mut self, data: ModelHubData) {
		let active_key = self
			.entries
			.get(self.active)
			.map(|entry| entry.key(&self.data.locked));
		self.data = data;
		self.sync_derived();
		self.active = active_key
			.and_then(|key| {
				self
					.entries
					.iter()
					.position(|entry| entry.key(&self.data.locked) == key)
			})
			.unwrap_or(0);
		if let Some((key, selector)) = self.pending_row.take() {
			let target = self.roles_view.iter().position(|row| match row {
				RolesRow::Fallback { key: row_key, selector: row_selector, .. } => {
					*row_key == key && selector.as_ref() == Some(row_selector)
				},
				RolesRow::Role(index) => {
					selector.is_none()
						&& self
							.data
							.roles
							.get(*index)
							.is_some_and(|role| role.id == key)
				},
				_ => false,
			});
			if let Some(target) = target {
				self.role_index = target;
			}
		}
		self.role_index = self.role_index.min(self.roles_view.len().saturating_sub(1));
		if let Some(Strip::Chips { kind: StripKind::Role, item, chips, index, .. }) = &mut self.strip
		{
			let item = item.clone();
			let at = (*index).min(chips.len().saturating_sub(1));
			let rebuilt = role_chips(&self.data, &item);
			if let Some(Strip::Chips { chips, index, .. }) = &mut self.strip {
				*chips = rebuilt;
				*index = at.min(chips.len().saturating_sub(1));
			}
		}
		self.rebuild();
	}

	/// Routes one key; unhandled keys reach the embedded model browser.
	pub fn handle_key(&mut self, key: Key) -> ModelHubEvent {
		if self.strip.is_some() {
			return self.handle_strip_key(key);
		}
		match key {
			Key::Tab | Key::BackTab => {
				self.focus = match self.focus {
					Focus::Scope => Focus::List,
					Focus::List => Focus::Scope,
				};
				self.rebuild();
				return ModelHubEvent::Consumed;
			},
			Key::Left => {
				self.focus = Focus::Scope;
				self.rebuild();
				return ModelHubEvent::Consumed;
			},
			Key::Right => {
				if self.roles_view_active() || self.browser_view_active() {
					self.focus = Focus::List;
					self.rebuild();
				}
				return ModelHubEvent::Consumed;
			},
			Key::Up if self.focus == Focus::Scope => {
				self.step_sidebar(-1);
				return ModelHubEvent::Consumed;
			},
			Key::Down if self.focus == Focus::Scope => {
				self.step_sidebar(1);
				return ModelHubEvent::Consumed;
			},
			Key::Esc => {
				if self.assigning.is_some() {
					self.cancel_assign();
					return ModelHubEvent::Consumed;
				}
				if self.browser_view_active() && !self.query.is_empty() {
					// The select clears its own filter on Esc.
					let event = self.ui.handle_key(key);
					return self.route(event);
				}
				return ModelHubEvent::Close;
			},
			_ => {},
		}
		if self.roles_view_active() {
			return self.handle_roles_key(key);
		}
		if self.locked_view_active() {
			return self.handle_locked_key(key);
		}
		if self.focus == Focus::Scope && matches!(key, Key::Enter | Key::Space) {
			self.focus = Focus::List;
			self.rebuild();
			return ModelHubEvent::Consumed;
		}
		if matches!(key, Key::Char(_) | Key::Backspace) {
			self.focus = Focus::List;
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted text into the model filter.
	pub fn handle_paste(&mut self, text: &str) -> ModelHubEvent {
		if let Some(Strip::RoleName { value }) = &mut self.strip {
			value.push_str(text.trim());
			self.rebuild();
			return ModelHubEvent::Consumed;
		}
		if self.browser_view_active() {
			let event = self.ui.handle_paste(text);
			return self.route(event);
		}
		ModelHubEvent::Consumed
	}

	/// Routes a pointer event; clicking outside dismisses the hub.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> ModelHubEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => ModelHubEvent::Close,
			None => ModelHubEvent::Consumed,
		}
	}

	/// Returns the responsive full-width overlay layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let rows = viewport.height.saturating_sub(FRAME_ROWS).max(10);
		if rows != self.body_rows || viewport.width != self.width {
			self.body_rows = rows;
			self.width = viewport.width;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	// ── Derived state ────────────────────────────────────────────────────

	fn sync_derived(&mut self) {
		let mut providers: Vec<(Str, Str, usize)> = Vec::new();
		for row in &self.data.rows {
			match providers.iter_mut().find(|(id, ..)| id == &row.provider_id) {
				Some((.., count)) => *count += 1,
				None => providers.push((row.provider_id.clone(), row.provider.clone(), 1)),
			}
		}
		providers.sort_by(|left, right| left.0.cmp(&right.0));
		let mut entries = vec![Entry::Roles, Entry::All];
		entries.extend(
			providers
				.into_iter()
				.map(|(id, name, count)| Entry::Provider { id, name, count }),
		);
		entries.extend((0..self.data.locked.len()).map(Entry::Locked));
		self.entries = entries;
		self.roles_view = roles_rows(&self.data);
		self.role_index = self.role_index.min(self.roles_view.len().saturating_sub(1));
	}

	fn active_entry(&self) -> &Entry {
		self.entries.get(self.active).unwrap_or(&Entry::All)
	}

	fn roles_view_active(&self) -> bool {
		*self.active_entry() == Entry::Roles && self.assigning.is_none()
	}

	fn locked_view_active(&self) -> bool {
		matches!(self.active_entry(), Entry::Locked(_)) && self.assigning.is_none()
	}

	fn browser_view_active(&self) -> bool {
		self.assigning.is_some() || matches!(self.active_entry(), Entry::All | Entry::Provider { .. })
	}

	/// Row indices shown by the browser select for the active scope.
	fn scope_rows(&self) -> Vec<usize> {
		if self.assigning.is_some() {
			return (0..self.data.rows.len()).collect();
		}
		match self.active_entry() {
			Entry::Provider { id, .. } => self
				.data
				.rows
				.iter()
				.enumerate()
				.filter(|(_, row)| row.provider_id == *id)
				.map(|(index, _)| index)
				.collect(),
			_ => (0..self.data.rows.len()).collect(),
		}
	}

	fn step_sidebar(&mut self, delta: isize) {
		let count = self.entries.len();
		if count == 0 {
			return;
		}
		let searching = !self.query.is_empty();
		let mut index = self.active;
		for _ in 0..count {
			index = (index as isize + delta).rem_euclid(count as isize) as usize;
			let entry = &self.entries[index];
			// While searching, hop only across scopes that can show matches.
			let skipped = searching
				&& match entry {
					Entry::Roles | Entry::Locked(_) => true,
					Entry::Provider { id, .. } => self.provider_match_count(id) == 0,
					Entry::All => false,
				};
			if skipped {
				continue;
			}
			if *entry == Entry::Roles {
				self.assigning = None;
			}
			self.active = index;
			self.focus = Focus::Scope;
			self.rebuild();
			return;
		}
	}

	fn provider_match_count(&self, provider: &str) -> usize {
		self
			.data
			.rows
			.iter()
			.filter(|row| row.provider_id == provider)
			.filter(|row| {
				fuzzy_matches(&sf!("{} {} {}", row.provider, row.name, row.key), &self.query)
			})
			.count()
	}

	// ── Roles view input ─────────────────────────────────────────────────

	fn handle_roles_key(&mut self, key: Key) -> ModelHubEvent {
		if self.focus == Focus::Scope {
			if matches!(key, Key::Enter | Key::Space) {
				self.focus = Focus::List;
				self.rebuild();
			} else if let Key::Char(ch) = key {
				// Typing from the sidebar jumps into a full-catalog search.
				self.jump_to_search(ch);
			}
			return ModelHubEvent::Consumed;
		}
		match key {
			Key::Up => {
				self.role_index = step_rows(&self.roles_view, self.role_index, -1);
				self.rebuild();
				return ModelHubEvent::Consumed;
			},
			Key::Down => {
				self.role_index = step_rows(&self.roles_view, self.role_index, 1);
				self.rebuild();
				return ModelHubEvent::Consumed;
			},
			_ => {},
		}
		let row = self.roles_view.get(self.role_index).cloned();
		match (key, row) {
			(Key::Enter, Some(row)) => return self.activate_roles_row(row),
			(Key::Backspace | Key::Delete | Key::Char('x'), Some(row)) => {
				return self.clear_roles_row(row);
			},
			(Key::SelectUp | Key::Char('['), Some(row)) => return self.move_roles_row(row, -1),
			(Key::SelectDown | Key::Char(']'), Some(row)) => return self.move_roles_row(row, 1),
			(Key::Char('f'), Some(row)) => match row {
				RolesRow::NewFallback => {
					self.start_assign_fallback_key();
					return ModelHubEvent::Consumed;
				},
				RolesRow::Role(index) => {
					if let Some(role) = self.data.roles.get(index) {
						self.start_assign_fallback(role.id.clone(), None);
					}
					return ModelHubEvent::Consumed;
				},
				RolesRow::ChainKey(key) | RolesRow::Fallback { key, .. } => {
					self.start_assign_fallback(key, None);
					return ModelHubEvent::Consumed;
				},
				RolesRow::NewRole | RolesRow::Separator => return ModelHubEvent::Consumed,
			},
			(Key::Char('c'), Some(RolesRow::Role(index))) => {
				if let Some(role) = self.data.roles.get(index) {
					let mut order: Vec<Str> = self.data.cycle_order.clone();
					match order.iter().position(|entry| *entry == role.id) {
						Some(at) => {
							order.remove(at);
						},
						None => order.push(role.id.clone()),
					}
					return ModelHubEvent::SetCycleOrder { order };
				}
				return ModelHubEvent::Consumed;
			},
			(Key::Char('n'), _) => {
				self.strip = Some(Strip::RoleName { value: String::new() });
				self.rebuild();
				return ModelHubEvent::Consumed;
			},
			(Key::Char('t'), Some(RolesRow::Role(index))) => {
				if let Some(role) = self.data.roles.get(index)
					&& let Some(resolved) = role.resolved.clone()
				{
					let id = role.id.clone();
					self.open_thinking_strip(&resolved, id, true, None);
				}
				return ModelHubEvent::Consumed;
			},
			_ => {},
		}
		ModelHubEvent::Consumed
	}

	fn jump_to_search(&mut self, ch: char) {
		if let Some(all) = self.entries.iter().position(|entry| *entry == Entry::All) {
			self.active = all;
		}
		self.focus = Focus::List;
		self.rebuild();
		let event = self.ui.handle_key(Key::Char(ch));
		let _ = self.route(event);
	}

	fn activate_roles_row(&mut self, row: RolesRow) -> ModelHubEvent {
		match row {
			RolesRow::Role(index) => {
				if let Some(role) = self.data.roles.get(index) {
					self.start_assign(role.id.clone());
				}
			},
			RolesRow::ChainKey(key) => self.start_assign_fallback(key, None),
			RolesRow::Fallback { key, index, .. } => self.start_assign_fallback(key, Some(index)),
			RolesRow::NewFallback => self.start_assign_fallback_key(),
			RolesRow::NewRole => {
				self.strip = Some(Strip::RoleName { value: String::new() });
				self.rebuild();
			},
			RolesRow::Separator => {},
		}
		ModelHubEvent::Consumed
	}

	fn clear_roles_row(&self, row: RolesRow) -> ModelHubEvent {
		match row {
			RolesRow::Role(index) => {
				let Some(role) = self.data.roles.get(index) else {
					return ModelHubEvent::Consumed;
				};
				if role.selector.is_none() {
					return ModelHubEvent::Consumed;
				}
				ModelHubEvent::UnassignRole { role: role.id.clone(), scope: None }
			},
			RolesRow::Fallback { key, index, .. } => {
				let mut chain = self.chain(&key);
				if index >= chain.len() {
					return ModelHubEvent::Consumed;
				}
				chain.remove(index);
				ModelHubEvent::SetFallbackChain { key, chain }
			},
			RolesRow::ChainKey(key) => ModelHubEvent::SetFallbackChain { key, chain: Vec::new() },
			_ => ModelHubEvent::Consumed,
		}
	}

	fn move_roles_row(&mut self, row: RolesRow, delta: isize) -> ModelHubEvent {
		match row {
			RolesRow::Role(index) => {
				let Some(role) = self.data.roles.get(index) else {
					return ModelHubEvent::Consumed;
				};
				let mut order: Vec<Str> = self.data.cycle_order.clone();
				let Some(at) = order.iter().position(|entry| *entry == role.id) else {
					return ModelHubEvent::Consumed;
				};
				let target = at as isize + delta;
				if target < 0 || target as usize >= order.len() {
					return ModelHubEvent::Consumed;
				}
				order.swap(at, target as usize);
				ModelHubEvent::SetCycleOrder { order }
			},
			RolesRow::Fallback { key, index, selector } => {
				let mut chain = self.chain(&key);
				let target = index as isize + delta;
				if index >= chain.len() || target < 0 || target as usize >= chain.len() {
					return ModelHubEvent::Consumed;
				}
				chain.swap(index, target as usize);
				self.pending_row = Some((key.clone(), Some(selector)));
				ModelHubEvent::SetFallbackChain { key, chain }
			},
			_ => ModelHubEvent::Consumed,
		}
	}

	fn handle_locked_key(&mut self, key: Key) -> ModelHubEvent {
		match key {
			Key::Enter => {
				if let Entry::Locked(index) = self.active_entry()
					&& let Some(provider) = self.data.locked.get(*index)
					&& provider.oauth
				{
					return ModelHubEvent::Login(provider.id.clone());
				}
				ModelHubEvent::Consumed
			},
			Key::Char(ch) => {
				self.jump_to_search(ch);
				ModelHubEvent::Consumed
			},
			_ => ModelHubEvent::Consumed,
		}
	}

	// ── Assignment flow ──────────────────────────────────────────────────

	fn chain(&self, key: &str) -> Vec<Str> {
		self
			.data
			.chains
			.iter()
			.find(|(chain_key, _)| chain_key == key)
			.map(|(_, chain)| chain.clone())
			.unwrap_or_default()
	}

	fn start_assign(&mut self, role: Str) {
		self.assigning = Some(Assign::Role(role));
		self.focus = Focus::List;
		self.query = Str::default();
		self.rebuild();
	}

	fn start_assign_fallback(&mut self, key: Str, index: Option<usize>) {
		self.assigning = Some(Assign::Fallback { key, index });
		self.focus = Focus::List;
		self.query = Str::default();
		self.rebuild();
	}

	fn start_assign_fallback_key(&mut self) {
		self.assigning = Some(Assign::FallbackKey);
		self.focus = Focus::List;
		self.query = Str::default();
		self.rebuild();
	}

	fn cancel_assign(&mut self) {
		self.assigning = None;
		self.query = Str::default();
		if let Some(roles) = self.entries.iter().position(|entry| *entry == Entry::Roles) {
			self.active = roles;
		}
		self.focus = Focus::List;
		self.rebuild();
	}

	fn activate_item(&mut self, row_index: usize) -> ModelHubEvent {
		let Some(row) = self.data.rows.get(row_index) else {
			return ModelHubEvent::Consumed;
		};
		let key = row.key.clone();
		match self.assigning.take() {
			Some(Assign::Role(role)) => {
				if self.data.project_storage {
					self.open_scope_strip(&key, role);
					return ModelHubEvent::Consumed;
				}
				self.finish_assign(&key, role, false, None)
			},
			Some(Assign::Fallback { key: chain_key, index }) => {
				let mut chain = self.chain(&chain_key);
				if let Some(index) = index.filter(|index| *index < chain.len()) {
					chain[index] = key.clone();
					let mut position = chain.len();
					while position > 0 {
						position -= 1;
						if position != index && chain[position] == key {
							chain.remove(position);
						}
					}
				} else if !chain.contains(&key) {
					chain.push(key.clone());
				}
				self.query = Str::default();
				if let Some(roles) = self.entries.iter().position(|entry| *entry == Entry::Roles) {
					self.active = roles;
				}
				self.focus = Focus::List;
				self.pending_row = Some((chain_key.clone(), Some(key)));
				self.rebuild();
				ModelHubEvent::SetFallbackChain { key: chain_key, chain }
			},
			Some(Assign::FallbackKey) => {
				self.open_fallback_key_strip(&key);
				ModelHubEvent::Consumed
			},
			None => {
				self.strip = Some(Strip::Chips {
					kind:            StripKind::Role,
					item:            key.clone(),
					role:            None,
					scope:           None,
					chips:           role_chips(&self.data, &key),
					index:           0,
					return_to_roles: false,
				});
				self.rebuild();
				ModelHubEvent::Consumed
			},
		}
	}

	/// Emits the assignment and opens the follow-up thinking strip.
	fn finish_assign(
		&mut self,
		item: &Str,
		role: Str,
		return_to_roles: bool,
		scope: Option<HubScope>,
	) -> ModelHubEvent {
		let thinking = self
			.data
			.roles
			.iter()
			.find(|candidate| candidate.id == role)
			.and_then(|candidate| candidate.thinking.clone())
			.filter(|level| self.effort_supported(item, level));
		self.query = Str::default();
		self.pending_row = Some((role.clone(), None));
		self.open_thinking_strip(item, role.clone(), return_to_roles, scope);
		ModelHubEvent::AssignRole { role, selector: item.clone(), thinking, scope }
	}

	fn effort_supported(&self, item: &Str, level: &str) -> bool {
		level == "auto"
			|| self
				.data
				.rows
				.iter()
				.find(|row| row.key == *item)
				.is_some_and(|row| row.efforts.iter().any(|effort| effort == level))
	}

	// ── Strips ───────────────────────────────────────────────────────────

	fn open_scope_strip(&mut self, item: &Str, role: Str) {
		self.strip = Some(Strip::Chips {
			kind:            StripKind::Scope,
			item:            item.clone(),
			role:            Some(role),
			scope:           None,
			chips:           vec![
				Chip {
					label:  Str::new_static("project"),
					color:  Str::new_static("accent"),
					action: ChipAction::Scope(HubScope::Project),
				},
				Chip {
					label:  Str::new_static("global"),
					color:  Str::new_static("muted"),
					action: ChipAction::Scope(HubScope::Global),
				},
			],
			index:           0,
			return_to_roles: true,
		});
		self.rebuild();
	}

	fn open_thinking_strip(
		&mut self,
		item: &Str,
		role: Str,
		return_to_roles: bool,
		scope: Option<HubScope>,
	) {
		let current = self
			.data
			.roles
			.iter()
			.find(|candidate| candidate.id == role)
			.and_then(|candidate| candidate.thinking.clone());
		let mut chips = vec![
			Chip {
				label:  Str::new_static("inherit"),
				color:  Str::new_static("muted"),
				action: ChipAction::Thinking(None),
			},
			Chip {
				label:  Str::new_static("auto"),
				color:  Str::new_static("muted"),
				action: ChipAction::Thinking(Some(Str::new_static("auto"))),
			},
		];
		let efforts = self
			.data
			.rows
			.iter()
			.find(|row| row.key == *item)
			.map_or_else(|| std::sync::Arc::from([]), |row| row.efforts.clone());
		chips.extend(efforts.iter().map(|effort| Chip {
			label:  effort.clone(),
			color:  Str::new_static("accent"),
			action: ChipAction::Thinking(Some(effort.clone())),
		}));
		let index = chips
			.iter()
			.position(|chip| match &chip.action {
				ChipAction::Thinking(level) => level.as_deref() == current.as_deref(),
				_ => false,
			})
			.unwrap_or(0);
		self.strip = Some(Strip::Chips {
			kind: StripKind::Thinking,
			item: item.clone(),
			role: Some(role),
			scope,
			chips,
			index,
			return_to_roles,
		});
		self.rebuild();
	}

	fn open_fallback_key_strip(&mut self, item: &Str) {
		let provider = self
			.data
			.rows
			.iter()
			.find(|row| row.key == *item)
			.map(|row| row.provider_id.clone())
			.unwrap_or_default();
		self.strip = Some(Strip::Chips {
			kind:            StripKind::FallbackKey,
			item:            item.clone(),
			role:            None,
			scope:           None,
			chips:           vec![
				Chip {
					label:  sf!("for {item}"),
					color:  Str::new_static("muted"),
					action: ChipAction::FallbackModel,
				},
				Chip {
					label:  sf!("for {provider}/*"),
					color:  Str::new_static("muted"),
					action: ChipAction::FallbackProvider,
				},
			],
			index:           0,
			return_to_roles: false,
		});
		self.rebuild();
	}

	fn close_strip(&mut self) {
		let return_to_roles = matches!(
			&self.strip,
			Some(Strip::Chips {
				kind: StripKind::Scope | StripKind::Thinking,
				return_to_roles: true,
				..
			})
		);
		self.strip = None;
		if return_to_roles {
			if let Some(roles) = self.entries.iter().position(|entry| *entry == Entry::Roles) {
				self.active = roles;
			}
			self.focus = Focus::List;
		}
		self.rebuild();
	}

	fn handle_strip_key(&mut self, key: Key) -> ModelHubEvent {
		if key == Key::Esc {
			self.close_strip();
			return ModelHubEvent::Consumed;
		}
		match &mut self.strip {
			Some(Strip::RoleName { value }) => match key {
				Key::Enter => {
					let name = value.trim().to_owned();
					let valid = name
						.chars()
						.next()
						.is_some_and(|first| first.is_ascii_alphabetic())
						&& name
							.chars()
							.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
						&& !self.data.roles.iter().any(|role| role.id == name);
					if valid {
						self.strip = None;
						self.start_assign(Str::from(name));
					}
					ModelHubEvent::Consumed
				},
				Key::Backspace => {
					value.pop();
					self.rebuild();
					ModelHubEvent::Consumed
				},
				Key::Char(ch) if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') => {
					value.push(ch);
					self.rebuild();
					ModelHubEvent::Consumed
				},
				_ => ModelHubEvent::Consumed,
			},
			Some(Strip::Chips { chips, index, .. }) => match key {
				Key::Left | Key::Up | Key::BackTab => {
					*index = (*index + chips.len().max(1) - 1) % chips.len().max(1);
					self.rebuild();
					ModelHubEvent::Consumed
				},
				Key::Right | Key::Down | Key::Tab => {
					*index = (*index + 1) % chips.len().max(1);
					self.rebuild();
					ModelHubEvent::Consumed
				},
				Key::Enter => self.activate_chip(),
				_ => ModelHubEvent::Consumed,
			},
			None => ModelHubEvent::Consumed,
		}
	}

	fn activate_chip(&mut self) -> ModelHubEvent {
		let Some(Strip::Chips {
			item, role, scope: strip_scope, chips, index, return_to_roles, ..
		}) = self.strip.clone()
		else {
			return ModelHubEvent::Consumed;
		};
		let Some(chip) = chips.get(index) else {
			return ModelHubEvent::Consumed;
		};
		match chip.action.clone() {
			ChipAction::Assign { role, scope } => {
				self.strip = None;
				self.finish_assign(&item, role, false, scope)
			},
			ChipAction::Unassign { role, scope } => {
				self.close_strip();
				ModelHubEvent::UnassignRole { role, scope }
			},
			ChipAction::FallbackModel => {
				self.strip = None;
				self.start_assign_fallback(item, None);
				ModelHubEvent::Consumed
			},
			ChipAction::FallbackProvider => {
				let provider = self
					.data
					.rows
					.iter()
					.find(|row| row.key == item)
					.map(|row| row.provider_id.clone())
					.unwrap_or_default();
				self.strip = None;
				self.start_assign_fallback(sf!("{provider}/*"), None);
				ModelHubEvent::Consumed
			},
			ChipAction::Scope(scope) => {
				let Some(role) = role else {
					self.close_strip();
					return ModelHubEvent::Consumed;
				};
				self.strip = None;
				self.finish_assign(&item, role, return_to_roles, Some(scope))
			},
			ChipAction::Thinking(level) => {
				let Some(role) = role else {
					self.close_strip();
					return ModelHubEvent::Consumed;
				};
				let scope = strip_scope;
				self.close_strip();
				ModelHubEvent::AssignRole { role, selector: item, thinking: level, scope }
			},
		}
	}

	// ── Event routing ────────────────────────────────────────────────────

	fn route(&mut self, event: UiEvent) -> ModelHubEvent {
		match event {
			UiEvent::Cancel => {
				if self.assigning.is_some() {
					self.cancel_assign();
					return ModelHubEvent::Consumed;
				}
				ModelHubEvent::Close
			},
			UiEvent::Changed { id, value } if id.as_str() == "hub-models" => value
				.as_str()
				.parse()
				.map_or(ModelHubEvent::Consumed, |index| self.activate_item(index)),
			UiEvent::Highlighted { id, value } if id.as_str() == "hub-models" => {
				self.show_detail(value.as_str().parse().ok());
				ModelHubEvent::Consumed
			},
			UiEvent::Filtered { id, query, value } if id.as_str() == "hub-models" => {
				let changed = self.query != query;
				self.query = query;
				self.show_detail(value.and_then(|value| value.as_str().parse().ok()));
				if changed {
					self.focus = Focus::List;
					// Sidebar match counts and provider fallback need a repaint,
					// but rebuilding would reset the select's live filter state;
					// counts refresh on the next structural rebuild instead.
					if let Entry::Provider { id, .. } = self.active_entry()
						&& self.assigning.is_none()
						&& !self.query.is_empty()
						&& self.provider_match_count(&id.clone()) == 0
						&& let Some(all) = self.entries.iter().position(|entry| *entry == Entry::All)
					{
						self.active = all;
						self.rebuild();
					}
				}
				ModelHubEvent::Consumed
			},
			_ => ModelHubEvent::Consumed,
		}
	}

	fn show_detail(&mut self, row: Option<usize>) {
		let facts = row
			.and_then(|index| self.data.rows.get(index))
			.map_or_else(|| sf!(" "), |row| model_facts(&self.data, row));
		self.ui.set_text("hub-facts", facts);
	}

	// ── Rendering ────────────────────────────────────────────────────────

	fn rebuild(&mut self) {
		self.ui = build(self);
		if self.browser_view_active() && self.strip.is_none() {
			self.ui.focus_id("hub-models");
		}
		let rows = self.scope_rows();
		let preselect = rows
			.iter()
			.position(|index| *index == self.data.current)
			.unwrap_or(0);
		self.show_detail(rows.get(preselect).copied());
	}
}

// ═══════════════════════════════════════════════════════════════════════════
// Free helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Case-insensitive subsequence match, used for sidebar match counts.
fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
	let mut chars = haystack.chars().flat_map(char::to_lowercase);
	needle
		.chars()
		.flat_map(char::to_lowercase)
		.filter(|ch| !ch.is_whitespace())
		.all(|needle_ch| chars.any(|hay_ch| hay_ch == needle_ch))
}

/// Builds the Roles view rows: each role followed by its chain entries, then
/// model-keyed chains as headed groups.
fn roles_rows(data: &ModelHubData) -> Vec<RolesRow> {
	let mut rows = Vec::new();
	for (index, role) in data.roles.iter().enumerate() {
		rows.push(RolesRow::Role(index));
		if let Some((_, chain)) = data.chains.iter().find(|(key, _)| *key == role.id) {
			for (at, selector) in chain.iter().enumerate() {
				rows.push(RolesRow::Fallback {
					key:      role.id.clone(),
					index:    at,
					selector: selector.clone(),
				});
			}
		}
	}
	rows.push(RolesRow::NewRole);
	rows.push(RolesRow::Separator);
	let mut model_keys: Vec<&Str> = data
		.chains
		.iter()
		.filter(|(key, _)| key.contains('/'))
		.map(|(key, _)| key)
		.collect();
	model_keys.sort();
	for key in model_keys {
		rows.push(RolesRow::ChainKey(key.clone()));
		if let Some((_, chain)) = data.chains.iter().find(|(chain_key, _)| chain_key == key) {
			for (at, selector) in chain.iter().enumerate() {
				rows.push(RolesRow::Fallback {
					key:      key.clone(),
					index:    at,
					selector: selector.clone(),
				});
			}
		}
	}
	rows.push(RolesRow::NewFallback);
	rows
}

/// Steps a roles-view cursor by one row, skipping separators, wrapping.
fn step_rows(rows: &[RolesRow], from: usize, delta: isize) -> usize {
	let count = rows.len();
	if count == 0 {
		return 0;
	}
	let mut index = from as isize;
	for _ in 0..count {
		index = (index + delta).rem_euclid(count as isize);
		if rows[index as usize] != RolesRow::Separator {
			return index as usize;
		}
	}
	from
}

/// Role-assignment chips for one picked model.
fn role_chips(data: &ModelHubData, item: &Str) -> Vec<Chip> {
	let mut chips = Vec::new();
	let scopes: &[Option<HubScope>] = if data.project_storage {
		&[Some(HubScope::Project), Some(HubScope::Global)]
	} else {
		&[None]
	};
	for role in &data.roles {
		let assigned_here =
			role.selector.is_some() && role.resolved.as_deref() == Some(item.as_str());
		for scope in scopes {
			let label = match scope {
				Some(HubScope::Project) => sf!("project {}", role.name.to_lowercase()),
				Some(HubScope::Global) => sf!("global {}", role.name.to_lowercase()),
				None => Str::from(role.name.to_lowercase()),
			};
			chips.push(Chip {
				label,
				color: role
					.color
					.clone()
					.unwrap_or_else(|| Str::new_static("muted")),
				action: if assigned_here {
					ChipAction::Unassign { role: role.id.clone(), scope: *scope }
				} else {
					ChipAction::Assign { role: role.id.clone(), scope: *scope }
				},
			});
		}
	}
	let provider = data
		.rows
		.iter()
		.find(|row| row.key == *item)
		.map(|row| row.provider_id.clone())
		.unwrap_or_default();
	chips.push(Chip {
		label:  sf!("fallbacks:{item}"),
		color:  Str::new_static("muted"),
		action: ChipAction::FallbackModel,
	});
	chips.push(Chip {
		label:  sf!("fallbacks:{provider}/*"),
		color:  Str::new_static("muted"),
		action: ChipAction::FallbackProvider,
	});
	chips
}

fn compact_count(value: u64) -> Str {
	if value >= 1_000_000 {
		sf!("{:.1}m", value as f64 / 1_000_000.0)
	} else if value >= 1_000 {
		sf!("{:.0}k", value as f64 / 1_000.0)
	} else {
		sf!("{value}")
	}
}

fn model_facts(data: &ModelHubData, row: &ModelRow) -> Str {
	let mut facts = Vec::new();
	let name = if row.name.is_empty() {
		row.key.clone()
	} else {
		row.name.clone()
	};
	facts.push(name);
	facts.push(row.provider.clone());
	if let Some(context) = row.context {
		facts.push(sf!("{} context", compact_count(context)));
	}
	match (row.input_mtok, row.output_mtok) {
		(Some(input), Some(output)) => facts.push(sf!("${input}/${output} per Mtok")),
		(Some(input), None) => facts.push(sf!("${input} in per Mtok")),
		(None, Some(output)) => facts.push(sf!("${output} out per Mtok")),
		(None, None) => {},
	}
	if !row.efforts.is_empty() {
		facts.push(Str::new_static("reasoning"));
	}
	let mut assigned: Vec<Str> = Vec::new();
	for role in &data.roles {
		if role.resolved.as_deref() == Some(row.key.as_str()) {
			assigned.push(sf!(
				"{}{}",
				if role.selector.is_some() { "" } else { "~" },
				role.name.to_lowercase()
			));
		}
	}
	if !assigned.is_empty() {
		facts.push(sf!("roles: {}", assigned.join(", ")));
	}
	Str::from(
		facts
			.iter()
			.map(Str::as_str)
			.collect::<Vec<_>>()
			.join(" · "),
	)
}

// ═══════════════════════════════════════════════════════════════════════════
// Dom construction
// ═══════════════════════════════════════════════════════════════════════════

struct SidebarLine {
	cursor:      &'static str,
	icon:        Str,
	icon_color:  Str,
	label:       Str,
	label_color: Str,
	bold:        bool,
	annotation:  Str,
	separator:   bool,
}

struct RoleLine {
	cursor:      &'static str,
	dot:         Str,
	dot_color:   Str,
	label:       Str,
	label_color: Str,
	value:       Str,
	value_color: Str,
	right:       Str,
}

fn build(hub: &ModelHub) -> Ui {
	let width = hub.width.max(40);
	let sidebar_width = sidebar_width(hub);
	let body_width = width.saturating_sub(sidebar_width + 7);
	let body_rows = hub.body_rows;
	let searching = !hub.query.is_empty();

	let sidebar = sidebar_lines(hub, sidebar_width, searching);
	let status = status_line(hub);
	let footer_hint = footer_hint(hub);
	let strip_line = strip_segments(hub);

	let list_rows = body_rows.saturating_sub(5).max(3);
	let roles_capacity = usize::from(body_rows.saturating_sub(3).max(3));

	enum Body {
		Roles(Vec<RoleLine>, Str),
		Browser(Vec<BrowserOption>, Str, u16),
		Locked(Vec<(Str, Str)>),
	}

	let body = if hub.roles_view_active() {
		let (lines, start) = role_lines(hub, roles_capacity, body_width);
		let _ = start;
		Body::Roles(lines, cycle_preview(hub))
	} else if let Entry::Locked(index) = hub.active_entry() {
		Body::Locked(locked_lines(hub, *index))
	} else {
		Body::Browser(browser_options(hub), hub.query.clone(), list_rows)
	};

	let root = OverlayPanel::new("Models").child(dom! {
		<col>
			<text fg=muted truncate>{status}</text>
			<row gap=1>
				<col w={sidebar_width} h={body_rows}>
					for line in sidebar {
						if line.separator {
							<text fg=border truncate>{line.label}</text>
						} else {
							<row>
								<pre fg=accent>{line.cursor}</pre>
								<pre fg={line.icon_color}>{line.icon}</pre>
								<pre>{" "}</pre>
								<pre fg={line.label_color} bold={line.bold}>{line.label}</pre>
								<pre fg=muted>{line.annotation}</pre>
							</row>
						}
					}
				</col>
				<col grow h={body_rows}>
					match body {
						Body::Roles(lines, preview) => {
							<col grow>
								for line in lines {
									<row>
										<pre fg=accent>{line.cursor}</pre>
										<pre fg={line.dot_color}>{line.dot}</pre>
										<pre>{" "}</pre>
										<pre fg={line.label_color}>{line.label}</pre>
										<pre fg={line.value_color} truncate>{line.value}</pre>
										<pre fg=muted>{line.right}</pre>
									</row>
								}
								<spacer/>
								<text fg=muted truncate>{preview}</text>
							</col>
						},
						Body::Browser(options, seed, rows) => {
							<col>
								<select id="hub-models" filter={seed} h={rows.saturating_add(1)}>
									for option in options {
										<option value={option.value} label={option.label} recommended={option.current}>
											<td>
												if let Some(src) = option.logo_src.clone() { <img src={src} w=2 h=1/> }
											</td>
											<td truncate>
												<pre fg=fg bg=border>{" "}{option.provider}{" "}</pre>
											</td>
											<td truncate=start grow>
												<pre fg={option.color}>{option.name}</pre>
												if option.current { <pre fg=ok>{" current"}</pre> }
												if !option.roles.is_empty() { <pre fg=muted>{" "}{option.roles.clone()}</pre> }
											</td>
											if !option.context.is_empty() { <td align=end><pre fg=muted>{option.context}</pre></td> }
											if !option.price.is_empty() { <td align=end><pre fg=muted>{option.price}</pre></td> }
										</option>
									}
								</select>
								<text id="hub-facts" fg=muted truncate>{" "}</text>
							</col>
						},
						Body::Locked(lines) => {
							<col grow>
								for (color, text) in lines {
									<text fg={color} truncate>{text}</text>
								}
							</col>
						},
					}
				</col>
			</row>
			{panel_divider()}
			if let Some(segments) = strip_line {
				<row>
					for (color, text, selected) in segments {
						<pre fg={color} bold={selected}>{text}</pre>
					}
				</row>
			} else {
				<text fg=muted truncate>{footer_hint}</text>
			}
		</col>
	});
	Ui::from_root(root, width, hub.ctx.clone())
}

struct BrowserOption {
	value:    Str,
	label:    Str,
	logo_src: Option<Str>,
	provider: Str,
	name:     Str,
	color:    Str,
	current:  bool,
	roles:    Str,
	context:  Str,
	price:    Str,
}

fn browser_options(hub: &ModelHub) -> Vec<BrowserOption> {
	let show_context = hub.width >= 72;
	let show_price = hub.width >= 88;
	hub.scope_rows()
		.into_iter()
		.map(|index| {
			let row = &hub.data.rows[index];
			let mut role_marks: Vec<Str> = Vec::new();
			for role in &hub.data.roles {
				if role.selector.is_some() && role.resolved.as_deref() == Some(row.key.as_str()) {
					role_marks.push(Str::from(role.name.to_lowercase()));
				}
			}
			BrowserOption {
				value:    sf!("{index}"),
				label:    sf!("{} {} {}", row.provider, row.name, row.key),
				logo_src: provider_logo(row.provider_id.as_str())
					.is_some()
					.then(|| sf!("asset://login/{}", row.provider_id)),
				provider: if row.provider.is_empty() {
					row.provider_id.clone()
				} else {
					row.provider.clone()
				},
				name:     if row.name.is_empty() {
					row.key.clone()
				} else {
					row.name.clone()
				},
				color:    row.color.clone().unwrap_or_else(|| sf!("fg")),
				current:  index == hub.data.current,
				roles:    if role_marks.is_empty() {
					Str::default()
				} else {
					sf!("[{}]", role_marks.join(" "))
				},
				context:  if show_context {
					row.context
						.map_or_else(Str::default, |tokens| sf!("{} ctx", compact_count(tokens)))
				} else {
					Str::default()
				},
				price:    if show_price {
					match (row.input_mtok, row.output_mtok) {
						(Some(input), Some(output)) => sf!("${input}/${output}"),
						(Some(input), None) => sf!("${input} in"),
						(None, Some(output)) => sf!("${output} out"),
						(None, None) => Str::default(),
					}
				} else {
					Str::default()
				},
			}
		})
		.collect()
}

fn sidebar_width(hub: &ModelHub) -> u16 {
	let mut longest = 0usize;
	for entry in &hub.entries {
		let len = match entry {
			Entry::Roles => "Roles".len() + 8,
			Entry::All => "All models".len() + 8,
			Entry::Provider { id, count, .. } => id.len() + count.to_string().len() + 6,
			Entry::Locked(index) => {
				hub.data
					.locked
					.get(*index)
					.map_or(8, |provider| provider.id.len())
					+ 8
			},
		};
		longest = longest.max(len);
	}
	(longest as u16).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

fn sidebar_lines(hub: &ModelHub, width: u16, searching: bool) -> Vec<SidebarLine> {
	let charset = hub.ctx.charset;
	let mut lines = Vec::with_capacity(hub.entries.len() + 2);
	let push_separator = |lines: &mut Vec<SidebarLine>| {
		lines.push(SidebarLine {
			cursor:      " ",
			icon:        Str::default(),
			icon_color:  Str::new_static("border"),
			label:       Str::from("─".repeat(usize::from(width))),
			label_color: Str::new_static("border"),
			bold:        false,
			annotation:  Str::default(),
			separator:   true,
		});
	};
	let mut prev_kind = 0u8;
	for (index, entry) in hub.entries.iter().enumerate() {
		let kind = match entry {
			Entry::Roles | Entry::All => 0,
			Entry::Provider { .. } => 1,
			Entry::Locked(_) => 2,
		};
		if kind != prev_kind {
			push_separator(&mut lines);
			prev_kind = kind;
		}
		let active = index == hub.active;
		let cursor = if active && hub.focus == Focus::Scope {
			">"
		} else {
			" "
		};
		let (icon, icon_color, label, annotation, muted) = match entry {
			Entry::Roles => {
				let assigned = hub
					.data
					.roles
					.iter()
					.filter(|role| role.selector.is_some())
					.count();
				(
					Str::new(charset.icon(Icon::Skill)),
					Str::new_static("accent"),
					Str::new_static("Roles"),
					sf!(" {assigned}/{}", hub.data.roles.len()),
					searching,
				)
			},
			Entry::All => (
				Str::new(charset.icon(Icon::Model)),
				Str::new_static("accent"),
				Str::new_static("All models"),
				sf!(" {}", hub.data.rows.len()),
				false,
			),
			Entry::Provider { id, count, .. } => {
				let matches = if searching {
					hub.provider_match_count(id)
				} else {
					*count
				};
				(
					Str::new(charset.icon(Icon::Enabled)),
					Str::new_static("ok"),
					id.clone(),
					sf!(" {matches}"),
					searching && matches == 0,
				)
			},
			Entry::Locked(locked_index) => {
				let provider = hub.data.locked.get(*locked_index);
				(
					Str::new(charset.icon(Icon::Shadowed)),
					Str::new_static("muted"),
					provider.map_or_else(Str::default, |provider| provider.id.clone()),
					Str::new_static(" login"),
					true,
				)
			},
		};
		lines.push(SidebarLine {
			cursor,
			icon,
			icon_color: if muted {
				Str::new_static("muted")
			} else {
				icon_color
			},
			label,
			label_color: if muted {
				Str::new_static("muted")
			} else if active {
				Str::new_static("accent")
			} else {
				Str::new_static("fg")
			},
			bold: active,
			annotation,
			separator: false,
		});
	}
	lines
}

fn status_line(hub: &ModelHub) -> Str {
	if let Some(assign) = &hub.assigning {
		return match assign {
			Assign::Role(role) => sf!(" Assigning {role} — Enter assigns, Esc cancels"),
			Assign::Fallback { key, index: None } => {
				sf!(" Adding fallback for {key} — Enter picks the fallback model, Esc cancels")
			},
			Assign::Fallback { key, index: Some(_) } => {
				sf!(" Replacing fallback of {key} — Enter picks the fallback model, Esc cancels")
			},
			Assign::FallbackKey => {
				sf!(" New fallback chain — Enter picks the model it protects, Esc cancels")
			},
		};
	}
	match hub.active_entry() {
		Entry::Roles => {
			sf!(" Model roles — f adds a retry fallback, cleared roles fall back to auto-selection")
		},
		Entry::All => sf!(" All available models"),
		Entry::Provider { name, count, .. } => sf!(" {name} · {count} models"),
		Entry::Locked(index) => {
			let name = hub
				.data
				.locked
				.get(*index)
				.map_or("provider", |provider| provider.name.as_str());
			sf!(" {name} · not configured")
		},
	}
}

fn role_lines(hub: &ModelHub, capacity: usize, width: u16) -> (Vec<RoleLine>, usize) {
	let charset = hub.ctx.charset;
	let total = hub.roles_view.len();
	let view = capacity.min(total.max(1));
	let mut start = hub.role_start.min(total.saturating_sub(view));
	if hub.role_index < start {
		start = hub.role_index;
	} else if hub.role_index >= start + view {
		start = hub.role_index + 1 - view;
	}
	let listed = hub.focus == Focus::List;
	let tag_width = hub
		.data
		.roles
		.iter()
		.map(|role| role.name.len())
		.max()
		.unwrap_or(4)
		.max(4);
	let mut lines = Vec::with_capacity(view);
	for (index, row) in hub.roles_view.iter().enumerate().skip(start).take(view) {
		let selected = index == hub.role_index;
		let cursor = if selected && listed { ">" } else { " " };
		let line = match row {
			RolesRow::Separator => RoleLine {
				cursor:      " ",
				dot:         Str::default(),
				dot_color:   Str::new_static("fg"),
				label:       Str::default(),
				label_color: Str::new_static("fg"),
				value:       Str::from("─".repeat(usize::from(width.saturating_sub(4)).max(1))),
				value_color: Str::new_static("border"),
				right:       Str::default(),
			},
			RolesRow::NewRole | RolesRow::NewFallback => RoleLine {
				cursor,
				dot: Str::new_static(" "),
				dot_color: Str::new_static("fg"),
				label: Str::default(),
				label_color: Str::new_static("fg"),
				value: if *row == RolesRow::NewRole {
					Str::new_static("+ New role…")
				} else {
					Str::new_static("+ New fallback…")
				},
				value_color: if selected {
					Str::new_static("accent")
				} else {
					Str::new_static("muted")
				},
				right: Str::default(),
			},
			RolesRow::ChainKey(key) => RoleLine {
				cursor,
				dot: Str::new(charset.icon(Icon::Shadowed)),
				dot_color: Str::new_static("muted"),
				label: Str::new_static(" "),
				label_color: Str::new_static("fg"),
				value: key.clone(),
				value_color: if selected {
					Str::new_static("accent")
				} else {
					Str::new_static("fg")
				},
				right: Str::default(),
			},
			RolesRow::Fallback { selector, .. } => RoleLine {
				cursor,
				dot: Str::default(),
				dot_color: Str::new_static("fg"),
				label: sf!("{:width$} ↳ ", "", width = tag_width + 2),
				label_color: Str::new_static("muted"),
				value: selector.clone(),
				value_color: if selected {
					Str::new_static("accent")
				} else {
					Str::new_static("muted")
				},
				right: Str::default(),
			},
			RolesRow::Role(role_index) => {
				let Some(role) = hub.data.roles.get(*role_index) else {
					continue;
				};
				let configured = role.selector.is_some();
				let color = role
					.color
					.clone()
					.unwrap_or_else(|| Str::new_static("muted"));
				let value = match (&role.resolved, configured) {
					(Some(resolved), true) => resolved.clone(),
					(Some(resolved), false) => sf!("auto → {resolved}"),
					(None, _) => Str::new_static("—"),
				};
				let mut right = String::new();
				if let Some(level) = &role.thinking {
					right.push_str(level);
				}
				if let Some(position) = hub
					.data
					.cycle_order
					.iter()
					.position(|entry| *entry == role.id)
				{
					if !right.is_empty() {
						right.push_str("  ");
					}
					right.push_str(&sf!("{} {}", charset.icon(Icon::Loop), position + 1));
				}
				if !right.is_empty() {
					right.insert_str(0, "  ");
				}
				RoleLine {
					cursor,
					dot: Str::new(charset.icon(if configured {
						Icon::Enabled
					} else {
						Icon::Shadowed
					})),
					dot_color: if configured {
						color.clone()
					} else {
						Str::new_static("muted")
					},
					label: sf!("{:width$}  ", role.name.to_lowercase(), width = tag_width),
					label_color: if configured {
						color
					} else {
						Str::new_static("muted")
					},
					value,
					value_color: if selected {
						Str::new_static("accent")
					} else if configured {
						Str::new_static("fg")
					} else {
						Str::new_static("muted")
					},
					right: Str::from(right),
				}
			},
		};
		lines.push(line);
	}
	(lines, start)
}

fn cycle_preview(hub: &ModelHub) -> Str {
	if hub.data.cycle_order.is_empty() {
		return Str::new_static("  ctrl+p cycle is empty — press c on a role to add it");
	}
	let selected_role = match hub.roles_view.get(hub.role_index) {
		Some(RolesRow::Role(index)) => hub.data.roles.get(*index).map(|role| role.id.as_str()),
		Some(RolesRow::Fallback { key, .. }) => Some(key.as_str()),
		_ => None,
	};
	let track = hub
		.data
		.cycle_order
		.iter()
		.map(|role| {
			if Some(role.as_str()) == selected_role {
				sf!("[{role}]")
			} else {
				role.clone()
			}
		})
		.collect::<Vec<_>>()
		.join(" → ");
	sf!("  ctrl+p cycle: {track}")
}

fn locked_lines(hub: &ModelHub, index: usize) -> Vec<(Str, Str)> {
	let Some(provider) = hub.data.locked.get(index) else {
		return Vec::new();
	};
	let mut lines = vec![
		(Str::new_static("muted"), Str::default()),
		(Str::new_static("warning"), sf!("  {} has no credentials configured", provider.name)),
		(Str::new_static("muted"), Str::default()),
	];
	if provider.env_vars.is_empty() {
		lines.push((
			Str::new_static("muted"),
			Str::new_static("  Add an API key for this provider in config."),
		));
	} else {
		lines.push((
			Str::new_static("muted"),
			sf!(
				"  Set {} in your environment, or add a key in config.",
				provider
					.env_vars
					.iter()
					.map(Str::as_str)
					.collect::<Vec<_>>()
					.join(" or ")
			),
		));
	}
	if provider.oauth {
		lines.push((Str::new_static("muted"), Str::default()));
		lines.push((Str::new_static("accent"), Str::new_static("  > Log in with OAuth (Enter)")));
	}
	if provider.models > 0 {
		lines.push((Str::new_static("muted"), Str::default()));
		lines.push((Str::new_static("muted"), sf!("  {} models in catalog", provider.models)));
	}
	lines
}

fn footer_hint(hub: &ModelHub) -> Str {
	if let Some(strip) = &hub.strip {
		return match strip {
			Strip::RoleName { .. } => Str::new_static("Enter create + pick model · Esc cancel"),
			Strip::Chips { kind: StripKind::Role | StripKind::FallbackKey, .. } => {
				Str::new_static("←/→ choose · Enter assign/clear · Esc cancel")
			},
			Strip::Chips { kind: StripKind::Scope, .. } => {
				Str::new_static("←/→ save scope · Enter choose · Esc cancel")
			},
			Strip::Chips { kind: StripKind::Thinking, .. } => {
				Str::new_static("←/→ thinking level · Enter apply · Esc keep")
			},
		};
	}
	if let Some(assign) = &hub.assigning {
		return match assign {
			Assign::Fallback { .. } => {
				Str::new_static("Enter pick fallback · ↑/↓ providers · type to search · Esc cancel")
			},
			Assign::FallbackKey => Str::new_static(
				"Enter pick the protected model · ↑/↓ providers · type to search · Esc cancel",
			),
			Assign::Role(_) => {
				Str::new_static("Enter assign · ↑/↓ providers · type to search · Esc cancel")
			},
		};
	}
	match hub.active_entry() {
		Entry::Roles if hub.focus == Focus::Scope => {
			Str::new_static("↑/↓ providers · → roles · Esc close")
		},
		Entry::Roles => match hub.roles_view.get(hub.role_index) {
			Some(RolesRow::Fallback { .. }) => Str::new_static(
				"↑/↓ rows · Enter replace · f add another · x remove · [/] reorder · ← providers",
			),
			Some(RolesRow::ChainKey(_)) => {
				Str::new_static("↑/↓ rows · Enter/f add fallback · x clear chain · ← providers")
			},
			Some(RolesRow::NewFallback) => {
				Str::new_static("↑/↓ rows · Enter new model/provider fallback chain · ← providers")
			},
			_ => Str::new_static(
				"↑/↓ rows · Enter pick · f fallback · x clear · t thinking · c cycle · [/] reorder · \
				 n new",
			),
		},
		Entry::Locked(index) => {
			if hub
				.data
				.locked
				.get(*index)
				.is_some_and(|provider| provider.oauth)
			{
				Str::new_static("Enter log in · ↑/↓ providers · Esc close")
			} else {
				Str::new_static("↑/↓ providers · Esc close")
			}
		},
		_ if hub.focus == Focus::Scope => Str::new_static(
			"Enter assign roles · ↑/↓ providers · → models · type to search · Esc close",
		),
		_ => Str::new_static(
			"Enter assign roles · ↑/↓ models · ← providers · type to search · Esc close",
		),
	}
}

/// Footer chip segments: `(color, text, selected)` triples, or `None` when no
/// strip is active.
fn strip_segments(hub: &ModelHub) -> Option<Vec<(Str, Str, bool)>> {
	match &hub.strip {
		None => None,
		Some(Strip::RoleName { value }) => Some(vec![
			(Str::new_static("accent"), Str::new_static("New role name: "), false),
			(Str::new_static("fg"), sf!("{value}▏"), true),
			(Str::new_static("muted"), Str::new_static("  (letters, digits, - and _)"), false),
		]),
		Some(Strip::Chips { item, chips, index, .. }) => {
			let mut segments = vec![
				(Str::new_static("accent"), item.clone(), false),
				(Str::new_static("muted"), Str::new_static(" → "), false),
			];
			for (at, chip) in chips.iter().enumerate() {
				let selected = at == *index;
				if selected {
					segments.push((Str::new_static("accent"), sf!("[{}]", chip.label), true));
				} else {
					segments.push((chip.color.clone(), chip.label.clone(), false));
				}
				segments.push((Str::new_static("fg"), Str::new_static(" "), false));
			}
			Some(segments)
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::HubRole;
	fn frame_rows(hub: &mut ModelHub, viewport: Size) -> Vec<String> {
		let layer = hub.layer(viewport);
		let frame = layer.frame;
		(0..frame.size().height)
			.map(|row| omp_tui::test_support::frame_row_text(frame, row))
			.collect()
	}

	fn row(provider: &'static str, name: &'static str) -> ModelRow {
		ModelRow {
			key:         sf!("{provider}/{name}"),
			name:        sf!(name),
			color:       None,
			provider_id: sf!(provider),
			provider:    sf!(provider),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
			efforts:     std::sync::Arc::from([Str::new_static("low"), Str::new_static("high")]),
		}
	}

	fn data() -> ModelHubData {
		ModelHubData {
			rows:            vec![row("anthropic", "claude"), row("openai", "gpt")],
			current:         0,
			roles:           vec![
				HubRole {
					id:       Str::new_static("default"),
					name:     Str::new_static("Default"),
					color:    Some(Str::new_static("ok")),
					selector: Some(Str::new_static("anthropic/claude")),
					resolved: Some(Str::new_static("anthropic/claude")),
					thinking: None,
				},
				HubRole {
					id:       Str::new_static("smol"),
					name:     Str::new_static("Fast"),
					color:    None,
					selector: None,
					resolved: Some(Str::new_static("openai/gpt")),
					thinking: None,
				},
			],
			cycle_order:     vec![Str::new_static("default")],
			chains:          vec![(Str::new_static("default"), vec![Str::new_static("openai/gpt")])],
			project_storage: false,
			locked:          vec![LockedProviderRow {
				id:       Str::new_static("mistral"),
				name:     Str::new_static("Mistral"),
				models:   3,
				oauth:    true,
				env_vars: vec![Str::new_static("MISTRAL_API_KEY")],
			}],
		}
	}

	fn hub() -> ModelHub {
		ModelHub::open(data(), &UiContext::default())
	}

	#[test]
	fn roles_rows_interleave_chains_and_trailing_actions() {
		let rows = roles_rows(&data());
		assert_eq!(rows[0], RolesRow::Role(0));
		assert_eq!(rows[1], RolesRow::Fallback {
			key:      Str::new_static("default"),
			index:    0,
			selector: Str::new_static("openai/gpt"),
		});
		assert_eq!(rows[2], RolesRow::Role(1));
		assert_eq!(rows[3], RolesRow::NewRole);
		assert_eq!(rows[4], RolesRow::Separator);
		assert_eq!(rows.last(), Some(&RolesRow::NewFallback));
	}

	#[test]
	fn cycle_toggle_emits_new_order() {
		let mut hub = hub();
		hub.focus = Focus::List;
		// Cursor starts on the `default` role row.
		let event = hub.handle_key(Key::Char('c'));
		assert_eq!(event, ModelHubEvent::SetCycleOrder { order: vec![] });
		// Down twice lands on the `smol` role row (skipping the fallback row).
		hub.handle_key(Key::Down);
		hub.handle_key(Key::Down);
		let event = hub.handle_key(Key::Char('c'));
		assert_eq!(event, ModelHubEvent::SetCycleOrder {
			order: vec![Str::new_static("default"), Str::new_static("smol")],
		});
	}

	#[test]
	fn fallback_removal_and_chain_clear() {
		let mut hub = hub();
		hub.focus = Focus::List;
		hub.handle_key(Key::Down);
		let event = hub.handle_key(Key::Char('x'));
		assert_eq!(event, ModelHubEvent::SetFallbackChain {
			key:   Str::new_static("default"),
			chain: vec![],
		});
	}

	#[test]
	fn unassign_requires_configured_role() {
		let mut hub = hub();
		hub.focus = Focus::List;
		let event = hub.handle_key(Key::Char('x'));
		assert_eq!(event, ModelHubEvent::UnassignRole {
			role:  Str::new_static("default"),
			scope: None,
		});
		hub.handle_key(Key::Down);
		hub.handle_key(Key::Down);
		let event = hub.handle_key(Key::Char('x'));
		assert_eq!(event, ModelHubEvent::Consumed);
	}

	#[test]
	fn assignment_flow_emits_role_and_opens_thinking_strip() {
		let mut hub = hub();
		hub.focus = Focus::List;
		let event = hub.handle_key(Key::Enter);
		assert_eq!(event, ModelHubEvent::Consumed);
		assert_eq!(hub.assigning, Some(Assign::Role(Str::new_static("default"))));
		let event = hub.activate_item(1);
		assert_eq!(event, ModelHubEvent::AssignRole {
			role:     Str::new_static("default"),
			selector: Str::new_static("openai/gpt"),
			thinking: None,
			scope:    None,
		});
		assert!(matches!(hub.strip, Some(Strip::Chips { kind: StripKind::Thinking, .. })));
		// Selecting the `high` effort re-emits the assignment with thinking.
		hub.handle_key(Key::Right);
		hub.handle_key(Key::Right);
		hub.handle_key(Key::Right);
		let event = hub.handle_key(Key::Enter);
		assert_eq!(event, ModelHubEvent::AssignRole {
			role:     Str::new_static("default"),
			selector: Str::new_static("openai/gpt"),
			thinking: Some(Str::new_static("high")),
			scope:    None,
		});
	}

	#[test]
	fn locked_provider_enter_requests_login() {
		let mut hub = hub();
		// Sidebar: Roles → All → anthropic → openai → mistral(locked).
		for _ in 0..4 {
			hub.handle_key(Key::Down);
		}
		assert!(matches!(hub.active_entry(), Entry::Locked(0)));
		let event = hub.handle_key(Key::Enter);
		assert_eq!(event, ModelHubEvent::Login(Str::new_static("mistral")));
	}

	#[test]
	fn roles_view_renders_assignments_chains_and_cycle_preview() {
		let mut hub = hub();
		hub.focus = Focus::List;
		let rows = frame_rows(&mut hub, Size::new(110, 30));
		let all = rows.join("\n");
		assert!(all.contains("Models"), "panel title present:\n{all}");
		assert!(all.contains("Roles"), "sidebar lists the roles scope:\n{all}");
		assert!(all.contains("All models"), "sidebar lists the catalog scope:\n{all}");
		assert!(all.contains("anthropic/claude"), "assigned default renders its model:\n{all}");
		assert!(all.contains("↳ openai/gpt"), "fallback row renders under its role:\n{all}");
		assert!(all.contains("+ New role…"), "trailing new-role action renders:\n{all}");
		assert!(all.contains("ctrl+p cycle:"), "cycle preview renders:\n{all}");
		assert!(all.contains("mistral"), "locked provider appears in the sidebar:\n{all}");
	}

	#[test]
	fn browser_view_renders_search_rows_and_facts() {
		let mut hub = hub();
		// Hop from Roles to All models.
		hub.handle_key(Key::Down);
		let rows = frame_rows(&mut hub, Size::new(110, 30));
		let all = rows.join("\n");
		assert!(all.contains("All available models"), "status row names the scope:\n{all}");
		assert!(all.contains("claude"), "catalog rows render:\n{all}");
		assert!(all.contains("current"), "current model is marked:\n{all}");
	}

	#[test]
	fn locked_view_renders_credential_guidance() {
		let mut hub = hub();
		for _ in 0..4 {
			hub.handle_key(Key::Down);
		}
		let rows = frame_rows(&mut hub, Size::new(110, 30));
		let all = rows.join("\n");
		assert!(all.contains("no credentials configured"), "locked banner renders:\n{all}");
		assert!(all.contains("MISTRAL_API_KEY"), "env var guidance renders:\n{all}");
		assert!(all.contains("Log in with OAuth"), "oauth action renders:\n{all}");
	}

	#[test]
	fn thinking_strip_renders_chips_in_footer() {
		let mut hub = hub();
		hub.focus = Focus::List;
		hub.handle_key(Key::Enter);
		let _ = hub.activate_item(1);
		let rows = frame_rows(&mut hub, Size::new(110, 30));
		let all = rows.join("\n");
		assert!(all.contains("[inherit]"), "selected thinking chip renders bracketed:\n{all}");
		assert!(all.contains("high"), "supported efforts render as chips:\n{all}");
	}

	#[test]
	fn update_preserves_active_entry_and_lands_pending_row() {
		let mut hub = hub();
		hub.focus = Focus::List;
		hub.handle_key(Key::Enter);
		let _ = hub.activate_item(1);
		// Simulate the backend refresh after the mutation.
		let mut refreshed = data();
		refreshed.roles[0].selector = Some(Str::new_static("openai/gpt"));
		refreshed.roles[0].resolved = Some(Str::new_static("openai/gpt"));
		hub.update(refreshed);
		assert!(matches!(hub.active_entry(), Entry::Roles));
		assert_eq!(hub.roles_view.get(hub.role_index), Some(&RolesRow::Role(0)));
	}
}
