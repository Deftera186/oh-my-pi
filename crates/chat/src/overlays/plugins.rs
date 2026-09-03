//! `/marketplace` (no argument, `install`, `uninstall`): pi's
//! `PluginSelectorComponent` — a centered `Plugins` list of every catalog
//! plugin with `@version`, `[installed]`, and `[scope]` tags plus the
//! marketplace as the hint. Enter installs (or, for an installed row,
//! uninstalls) through [`Services`]; the request settles asynchronously
//! and the panel polls it from [`Panel::tick`].

use std::{sync::Arc, time::Duration};

use omp_core::{Str, sf};
use omp_tui::{Frame, Key, Prop, Size, Ui, UiContext, UiEvent, dom};

use super::{
	Panel, PanelAnchor, PanelEvent,
	services::{Pending, PluginRow, PluginsReport, Services},
};

/// pi `SelectList` cap: `Math.min(items.length, 20)`.
const MAX_VISIBLE: usize = 20;
/// Border rows, divider, status row, and hint.
const CHROME_ROWS: u16 = 5;
/// Poll cadence while a request is in flight.
const POLL: Duration = Duration::from_millis(100);
const EMPTY_VALUE: &str = "__empty__";
const HINT_INSTALL: &str = "↑/↓ plugins · Enter install (uninstall when installed) · type to search · Esc close";
const HINT_UNINSTALL: &str = "↑/↓ plugins · Enter uninstall · type to search · Esc close";

/// Which pi selector opened: the install browser over every catalog
/// plugin or the uninstall picker over installed ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginMode {
	/// `/marketplace`, `/marketplace install`.
	Install,
	/// `/marketplace uninstall`.
	Uninstall,
}

struct InFlight {
	id:         Str,
	installing: bool,
	pending:    Pending<Str>,
}

/// Retained marketplace plugin selector.
pub struct PluginSelector {
	services:  Arc<dyn Services>,
	report:    PluginsReport,
	mode:      PluginMode,
	in_flight: Option<InFlight>,
	/// Settled line not yet handed to the host.
	notice:    Option<Str>,
	query:     Str,
	next_wake: Option<Duration>,
	ui:        Ui,
	ctx:       UiContext,
	width:     u16,
	rows:      u16,
}

impl PluginSelector {
	/// Opens the selector over the current marketplace report.
	pub fn open(
		services: &Arc<dyn Services>,
		mode: PluginMode,
		ctx: &UiContext,
	) -> Result<Self, Str> {
		let report = services.plugins().map_err(|error| sf!("{error}"))?;
		let mut panel = Self {
			services: Arc::clone(services),
			report,
			mode,
			in_flight: None,
			notice: None,
			query: Str::default(),
			next_wake: None,
			ui: Ui::from_root(dom! { <col/> }, 80, ctx.clone()),
			ctx: ctx.clone(),
			width: 80,
			rows: 0,
		};
		panel.rows = panel.visible_rows(20);
		panel.rebuild();
		Ok(panel)
	}

	/// Plugins the selector lists, in list order.
	#[must_use]
	pub fn plugins(&self) -> Vec<&PluginRow> {
		self
			.report
			.plugins
			.iter()
			.filter(|plugin| self.mode == PluginMode::Install || plugin.installed)
			.collect()
	}

	/// The request in flight, as `(plugin id, installing)`.
	#[must_use]
	pub fn in_flight(&self) -> Option<(&str, bool)> {
		self
			.in_flight
			.as_ref()
			.map(|request| (request.id.as_str(), request.installing))
	}

	fn visible_rows(&self, height: u16) -> u16 {
		let items = self.plugins().len().max(1).min(MAX_VISIBLE) as u16;
		items.min(height.saturating_sub(CHROME_ROWS)).max(1)
	}

	fn status_line(&self) -> Str {
		match &self.in_flight {
			Some(request) if request.installing => sf!("Installing {}…", request.id),
			Some(request) => sf!("Uninstalling {}…", request.id),
			None => match self.mode {
				PluginMode::Install => {
					let count = self.report.marketplaces.len();
					if count == 1 {
						sf!("1 marketplace")
					} else {
						sf!("{count} marketplaces")
					}
				},
				PluginMode::Uninstall => {
					let count = self.plugins().len();
					if count == 1 {
						sf!("1 installed plugin")
					} else {
						sf!("{count} installed plugins")
					}
				},
			},
		}
	}

	fn rebuild(&mut self) {
		let plugins = self.plugins();
		let options: Vec<(Str, Str, Str, Str)> = plugins
			.iter()
			.map(|plugin| {
				let version = plugin
					.version
					.as_ref()
					.map_or_else(Str::default, |version| sf!("@{version}"));
				let status = if plugin.installed {
					" [installed]"
				} else {
					""
				};
				let scope = if plugin.scope.is_empty() {
					Str::default()
				} else {
					sf!(" [{}]", plugin.scope)
				};
				(
					plugin.id.clone(),
					sf!("{}{version}{status}{scope}", plugin.name),
					plugin.description.clone(),
					plugin.marketplace.clone(),
				)
			})
			.collect();
		let empty = options.is_empty();
		let empty_reason = if self.report.marketplaces.is_empty() {
			"Add a marketplace first: /marketplace add <source>"
		} else if self.mode == PluginMode::Uninstall {
			"No marketplace plugins installed"
		} else {
			"Configured marketplaces have no plugins"
		};
		let hint = match self.mode {
			PluginMode::Install => HINT_INSTALL,
			PluginMode::Uninstall => HINT_UNINSTALL,
		};
		let status = self.status_line();
		let status_fg = if self.in_flight.is_some() {
			"accent"
		} else {
			"muted"
		};
		let seed = self.query.clone();
		let height = self.rows.saturating_add(1);
		let tree = dom! {
			<box border=round title="Plugins" pad-x=1>
				<col>
					<select id="plugins" filter={seed} h={height}>
						if empty {
							<option value={EMPTY_VALUE} label="No plugins available">
								<td><pre>{"No plugins available"}</pre></td>
								<td truncate grow><pre fg=muted>{empty_reason}</pre></td>
							</option>
						}
						for (value, label, desc, marketplace) in options {
							<option value={value} label={label.clone()}>
								<td><pre>{label}</pre></td>
								<td truncate grow><pre fg=muted>{desc}</pre></td>
								<td align=end><pre fg=muted>{marketplace}</pre></td>
							</option>
						}
					</select>
					<hr border=round/>
					<text id="plugin-status" fg={status_fg} truncate>{status}</text>
					<text fg=muted truncate>{hint}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, self.width, self.ctx.clone());
	}

	/// pi `onSelect`: install the chosen plugin, or uninstall an installed
	/// one (the uninstall picker only lists those).
	fn choose(&mut self, id: &str) -> PanelEvent {
		if id == EMPTY_VALUE {
			return PanelEvent::Consumed;
		}
		if let Some(request) = &self.in_flight {
			return PanelEvent::Notice(sf!(
				"{} {} first",
				if request.installing {
					"Installing"
				} else {
					"Uninstalling"
				},
				request.id
			));
		}
		let Some(plugin) = self.report.plugins.iter().find(|plugin| plugin.id == id) else {
			return PanelEvent::Consumed;
		};
		let installing = !plugin.installed;
		let started = if installing {
			self.services.install_plugin(id)
		} else {
			self.services.uninstall_plugin(id)
		};
		match started {
			Ok(pending) => {
				self.in_flight = Some(InFlight { id: Str::new(id), installing, pending });
				self.next_wake = Some(Duration::ZERO);
				self.rebuild();
				PanelEvent::Consumed
			},
			Err(error) => PanelEvent::Notice(sf!("Marketplace error: {error}")),
		}
	}
}

impl Panel for PluginSelector {
	fn id(&self) -> &'static str {
		"plugins"
	}

	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Center
	}

	fn key(&mut self, key: Key) -> PanelEvent {
		match self.ui.handle_key(key) {
			UiEvent::Cancel => PanelEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "plugins" => self.choose(value.as_str()),
			UiEvent::Filtered { id, query, .. } if id.as_str() == "plugins" => {
				self.query = query;
				PanelEvent::Consumed
			},
			_ => PanelEvent::Consumed,
		}
	}

	fn paste(&mut self, text: &str) -> PanelEvent {
		match self.ui.handle_paste(text) {
			UiEvent::Cancel => PanelEvent::Close,
			UiEvent::Filtered { id, query, .. } if id.as_str() == "plugins" => {
				self.query = query;
				PanelEvent::Consumed
			},
			_ => PanelEvent::Consumed,
		}
	}

	fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = self.visible_rows(viewport.height);
		if rows != self.rows {
			self.rows = rows;
			self.ui.set_prop("plugins", Prop::H, rows.saturating_add(1));
		}
		if viewport.width != self.width {
			self.width = viewport.width;
			self.rebuild();
		}
		self.ui.frame()
	}

	fn tick(&mut self, now: Duration) -> bool {
		let Some(request) = &self.in_flight else {
			return false;
		};
		let line = match request.pending.try_recv() {
			Ok(Ok(line)) => line,
			Ok(Err(error)) => sf!("Marketplace error: {error}"),
			Err(flume::TryRecvError::Disconnected) => {
				Str::new_static("Marketplace error: the request was dropped before settling")
			},
			Err(flume::TryRecvError::Empty) => {
				self.next_wake = Some(now + POLL);
				return false;
			},
		};
		self.in_flight = None;
		self.next_wake = None;
		self.notice = Some(line);
		if let Ok(report) = self.services.plugins() {
			self.report = report;
		}
		self.rebuild();
		true
	}

	fn next_wake(&self) -> Option<Duration> {
		self.next_wake
	}

	fn settled(&mut self) -> Option<PanelEvent> {
		self.notice.take().map(PanelEvent::Notice)
	}
}

#[cfg(test)]
mod tests {
	use parking_lot::Mutex;

	use super::*;
	use crate::overlays::services::{MarketplaceSource, ServiceResult};

	struct Feed {
		report:   Mutex<PluginsReport>,
		installs: Mutex<Vec<Str>>,
		tx:       Mutex<Option<flume::Sender<ServiceResult<Str>>>>,
	}

	impl Services for Feed {
		fn plugins(&self) -> ServiceResult<PluginsReport> {
			Ok(self.report.lock().clone())
		}

		fn install_plugin(&self, id: &str) -> ServiceResult<Pending<Str>> {
			self.installs.lock().push(Str::new(id));
			let (tx, rx) = flume::bounded(1);
			*self.tx.lock() = Some(tx);
			Ok(rx)
		}
	}

	fn plugin(name: &str, installed: bool) -> PluginRow {
		PluginRow {
			id:          sf!("{name}@official"),
			name:        Str::new(name),
			version:     Some(Str::new_static("1.0.0")),
			description: sf!("The {name} plugin"),
			marketplace: Str::new_static("official"),
			installed,
			enabled:     installed,
			scope:       if installed {
				Str::new_static("user")
			} else {
				Str::default()
			},
			shadowed:    false,
		}
	}

	fn feed(plugins: Vec<PluginRow>, marketplaces: usize) -> Arc<Feed> {
		let sources = (0..marketplaces)
			.map(|index| MarketplaceSource { name: sf!("market{index}"), uri: sf!("org/repo{index}") })
			.collect::<Vec<_>>();
		Arc::new(Feed {
			report:   Mutex::new(PluginsReport {
				marketplaces: sources.iter().map(|source| source.name.clone()).collect(),
				plugins,
				sources,
			}),
			installs: Mutex::new(Vec::new()),
			tx:       Mutex::new(None),
		})
	}

	fn open(feed: &Arc<Feed>, mode: PluginMode) -> PluginSelector {
		let services: Arc<dyn Services> = Arc::clone(feed) as Arc<dyn Services>;
		PluginSelector::open(&services, mode, &UiContext::default()).expect("selector opens")
	}

	#[test]
	fn selector_lists_plugins_with_pi_tags_and_marketplace_hint() {
		let feed = feed(vec![plugin("linter", true), plugin("docs", false)], 1);
		let mut panel = open(&feed, PluginMode::Install);
		let text = omp_tui::frame_text(panel.frame(Size { width: 70, height: 20 }));
		assert!(text.contains("Plugins"), "title missing:\n{text}");
		assert!(text.contains("linter@1.0.0 [installed] [user]"), "installed row missing:\n{text}");
		assert!(text.contains("docs@1.0.0"), "available row missing:\n{text}");
		assert!(text.contains("official"), "marketplace hint missing:\n{text}");
		assert!(text.contains("Enter install"), "hint missing:\n{text}");
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
	}

	#[test]
	fn enter_installs_then_settles_through_tick_and_settled() {
		let feed = feed(vec![plugin("docs", false), plugin("linter", true)], 1);
		let mut panel = open(&feed, PluginMode::Install);
		panel.frame(Size { width: 70, height: 20 });
		assert_eq!(panel.key(Key::Enter), PanelEvent::Consumed);
		assert_eq!(feed.installs.lock().as_slice(), &[Str::new_static("docs@official")]);
		assert_eq!(panel.in_flight(), Some(("docs@official", true)));
		let text = omp_tui::frame_text(panel.frame(Size { width: 70, height: 20 }));
		assert!(text.contains("Installing docs@official…"), "pending row missing:\n{text}");
		assert!(!panel.tick(Duration::ZERO), "nothing settled yet");
		assert_eq!(panel.next_wake(), Some(POLL));
		assert!(
			matches!(panel.key(Key::Enter), PanelEvent::Notice(text) if text.contains("Installing")),
			"a second Enter waits for the request"
		);
		feed.report.lock().plugins[0].installed = true;
		let tx = feed.tx.lock().take().expect("install started");
		tx.send(Ok(Str::new_static("Installed docs from official"))).unwrap();
		assert!(panel.tick(POLL), "settling repaints");
		assert_eq!(
			panel.settled(),
			Some(PanelEvent::Notice(Str::new_static("Installed docs from official")))
		);
		assert_eq!(panel.settled(), None, "notice delivered once");
		assert_eq!(panel.in_flight(), None);
		assert_eq!(panel.next_wake(), None);
		let text = omp_tui::frame_text(panel.frame(Size { width: 70, height: 20 }));
		assert!(text.contains("docs@1.0.0 [installed]"), "list refreshed from services:\n{text}");
	}

	#[test]
	fn empty_catalog_explains_the_missing_marketplace() {
		let feed = feed(Vec::new(), 0);
		let mut panel = open(&feed, PluginMode::Install);
		let text = omp_tui::frame_text(panel.frame(Size { width: 70, height: 20 }));
		assert!(text.contains("No plugins available"), "empty row missing:\n{text}");
		assert!(
			text.contains("Add a marketplace first: /marketplace add <source>"),
			"empty reason missing:\n{text}"
		);
		assert_eq!(panel.key(Key::Enter), PanelEvent::Consumed);
		assert!(feed.installs.lock().is_empty());
		let feed = feed_with_market();
		let mut panel = open(&feed, PluginMode::Install);
		let text = omp_tui::frame_text(panel.frame(Size { width: 70, height: 20 }));
		assert!(text.contains("Configured marketplaces have no plugins"), "reason missing:\n{text}");
	}

	fn feed_with_market() -> Arc<Feed> {
		feed(Vec::new(), 1)
	}

	#[test]
	fn uninstall_mode_lists_only_installed_plugins() {
		let feed = feed(vec![plugin("docs", false), plugin("linter", true)], 1);
		let mut panel = open(&feed, PluginMode::Uninstall);
		assert_eq!(panel.plugins().len(), 1);
		let text = omp_tui::frame_text(panel.frame(Size { width: 70, height: 20 }));
		assert!(text.contains("linter@1.0.0 [installed]"), "installed row missing:\n{text}");
		assert!(!text.contains("docs@"), "available rows hidden:\n{text}");
		assert!(text.contains("Enter uninstall"), "hint missing:\n{text}");
	}
}
