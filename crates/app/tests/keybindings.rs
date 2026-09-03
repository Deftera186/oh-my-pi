//! pi keybinding parity: every pi default chord that omp implements is bound
//! by the default cfg to the console command in the migration table.

use omp_app::keybindings::{DEFAULT_BINDS, PI_ACTIONS, config::ConsoleKeybindings};

/// pi `KEYBINDINGS` defaults (packages/coding-agent/src/config/keybindings.ts),
/// action id → default chords.
const PI_DEFAULTS: &[(&str, &[&str])] = &[
	("app.interrupt", &["escape"]),
	("app.clear", &["ctrl+c"]),
	("app.exit", &["ctrl+d"]),
	("app.suspend", &["ctrl+z"]),
	("app.display.reset", &["alt+l"]),
	("app.thinking.cycle", &["shift+tab"]),
	("app.thinking.toggle", &["ctrl+t"]),
	("app.model.cycleForward", &["ctrl+p"]),
	("app.model.cycleBackward", &["shift+ctrl+p"]),
	("app.model.select", &["alt+m"]),
	("app.model.selectTemporary", &["alt+p"]),
	("app.tools.expand", &["ctrl+o"]),
	("app.tools.toggleVisibility", &["ctrl+shift+o"]),
	("app.editor.external", &["ctrl+g"]),
	("app.message.followUp", &["ctrl+q", "ctrl+enter"]),
	("app.retry", &["f5", "alt+r"]),
	("app.message.dequeue", &["alt+up", "shift+up"]),
	("app.clipboard.pasteImage", &["ctrl+v"]),
	("app.clipboard.pasteTextRaw", &["ctrl+shift+v", "alt+shift+v"]),
	("app.clipboard.copyLine", &["alt+shift+l"]),
	("app.clipboard.copyPrompt", &["alt+shift+c"]),
	("app.agents.hub", &["alt+a"]),
	("app.session.observe", &["ctrl+s"]),
	("app.plan.toggle", &["alt+shift+p"]),
	("app.history.search", &["ctrl+r"]),
	("app.live.toggle", &["ctrl+l"]),
];

/// pi defaults deliberately not bound by omp, each with its reason.
const NOT_BOUND: &[(&str, &str)] = &[
	// The editor keymap in crates/tui owns Ctrl+Enter as a newline chord
	// (`submit_remap_on_ctrl_enter_wins_over_newline_default`); alt+enter is
	// bound instead.
	("ctrl+enter", "app.message.followUp"),
	// Steering is journaled at the kernel's safe point (ADR 0003
	// `<queues><steering>`); there is no local queue to dequeue from.
	("alt+up", "app.message.dequeue"),
	("shift+up", "app.message.dequeue"),
	// Editor-level chords: the decoder lowers them to Key::Paste/PasteRaw/
	// CopyLine/CopyPrompt and the composer handles them directly.
	("ctrl+v", "app.clipboard.pasteImage"),
	("ctrl+shift+v", "app.clipboard.pasteTextRaw"),
	("alt+shift+v", "app.clipboard.pasteTextRaw"),
	("alt+shift+l", "app.clipboard.copyLine"),
	("alt+shift+c", "app.clipboard.copyPrompt"),
	// Not ported yet: agent hub / session observer / live voice.
	("alt+a", "app.agents.hub"),
	("ctrl+s", "app.session.observe"),
	("ctrl+l", "app.live.toggle"),
];

fn default_bindings() -> ConsoleKeybindings {
	let ctx = omp_con::Ctx::new();
	ctx.exec(DEFAULT_BINDS, omp_con::Source::Config("default-binds.cfg".into()))
		.expect("default bind cfg executes");
	ConsoleKeybindings::from_ctx(&ctx).expect("default chords normalize")
}

#[test]
fn every_pi_default_is_bound_or_explicitly_excluded() {
	let bindings = default_bindings();
	let mut table = Vec::new();
	for (action, chords) in PI_DEFAULTS {
		let expected = PI_ACTIONS
			.iter()
			.find_map(|(id, command)| (id == action).then_some(*command));
		for chord in *chords {
			let bound = bindings.command_for(chord);
			let excluded = NOT_BOUND
				.iter()
				.any(|(excluded, id)| excluded == chord && id == action);
			table.push(format!("{action:32} {chord:14} -> {}", bound.unwrap_or("(unbound)")));
			match (expected, bound, excluded) {
				(Some(command), Some(bound), false) => {
					assert_eq!(bound, command, "{action} {chord} binds the wrong command");
				},
				(_, None, true) => {},
				(None, None, false) => {
					panic!("{action} {chord}: no omp command and not in NOT_BOUND")
				},
				(_, Some(bound), true) => {
					panic!("{action} {chord} is listed as excluded but bound to {bound}")
				},
				(Some(command), None, false) => {
					panic!("{action} {chord} should bind {command} but is unbound")
				},
				(None, Some(bound), false) => {
					panic!("{action} {chord} bound to {bound} without a PI_ACTIONS entry")
				},
			}
		}
	}
	eprintln!("{}", table.join("\n"));
}

#[test]
fn every_default_bind_names_a_registered_console_command() {
	let ctx = omp_con::Ctx::new();
	for (_, command) in default_bindings().bindings {
		let name = command.split_whitespace().next().expect("non-empty bind");
		assert!(ctx.find(name).is_some(), "bind runs unknown console name `{name}`");
	}
}

#[test]
fn process_ctx_seeds_defaults_then_lets_config_cfg_override() {
	let config = tempfile::tempdir().expect("config directory");
	// SAFETY: nextest runs each test in its own process; nothing else reads the
	// variable concurrently.
	unsafe { std::env::set_var("OMP_CONFIG_DIR", config.path()) };
	std::fs::write(config.path().join("config.cfg"), "unbind alt+p\nbind alt+x cl_model_select\n")
		.expect("user cfg");
	let project = tempfile::tempdir().expect("project directory");
	let ctx = omp_app::process_ctx(project.path()).expect("process ctx");
	let bindings = ConsoleKeybindings::from_ctx(&ctx).expect("bindings");
	assert_eq!(bindings.command_for("alt+p"), None, "config.cfg unbinds a default");
	assert_eq!(bindings.command_for("alt+x"), Some("cl_model_select"));
	assert_eq!(bindings.command_for("shift+tab"), Some("cl_thinking_cycle"));
	assert_eq!(bindings.command_for("ctrl+t"), Some("toggle cl_showthinking"));
}
