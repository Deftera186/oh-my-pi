use std::sync::{
	Arc, Mutex,
	atomic::{AtomicBool, AtomicUsize, Ordering},
};

use omp_app::{
	chat_ui::commands::{
		CommandProvenance, CommandResult, CommandSourceKind, CommandSurface, ConsumedResult,
		ExtensionCommandHandler, ExtensionCommandInvocation,
	},
	keybindings::{ExtensionShortcutRoster, KeyPlatform, config::ResolvedKeybindings},
};
use omp_core::{Str, sf};
use omp_envd::exthost::{HostKey, UiRoster, VerifiedUiRoster};
use omp_proto::ui::v1::{CommandDecl, ShortcutDecl};

fn command(id: &str, name: &str, callback: &str) -> CommandDecl {
	CommandDecl {
		name: name.to_owned(),
		description: format!("Run {name}"),
		declaration_id: id.to_owned(),
		callback: callback.to_owned(),
		module: "fixture.commands".to_owned(),
		activation_trigger: "lazy".to_owned(),
		..Default::default()
	}
}

fn shortcut(id: &str, chord: &str, action_id: &str) -> ShortcutDecl {
	ShortcutDecl {
		chord: chord.to_owned(),
		action_id: action_id.to_owned(),
		declaration_id: id.to_owned(),
		callback: format!("fixture.commands.{action_id}"),
		module: "fixture.commands".to_owned(),
		activation_trigger: "lazy".to_owned(),
		..Default::default()
	}
}

#[tokio::test]
async fn fixture_command_shortcut_reload_retires_old_generation() {
	let host = HostKey::new("project", "native", "fixture");
	let first = VerifiedUiRoster {
		generation: 11,
		extension:  sf!("fixture"),
		commands:   vec![command("review", "review", "fixture.commands.review")].into_boxed_slice(),
		shortcuts:  vec![shortcut("history", "ctrl+alt+h", "history")].into_boxed_slice(),
	};
	let mut host_roster = UiRoster::default();
	host_roster
		.install(host.clone(), &first)
		.expect("first roster");
	assert_eq!(host_roster.command("review").unwrap().owner.generation, 11);

	let shortcuts = ExtensionShortcutRoster::install(
		&first.shortcuts,
		11,
		&ResolvedKeybindings::default(),
		KeyPlatform::Unix,
	)
	.expect("fixture shortcut");
	assert_eq!(
		shortcuts
			.match_chord("alt+ctrl+h", "open")
			.expect("normalized chord")
			.expect("local shortcut")
			.generation,
		11
	);

	let activated = Arc::new(AtomicBool::new(false));
	let activation_count = Arc::new(AtomicUsize::new(0));
	let received = Arc::new(Mutex::new(None));
	let activated_for_handler = activated.clone();
	let activation_count_for_handler = activation_count.clone();
	let received_for_handler = received.clone();
	let handler: Arc<dyn ExtensionCommandHandler> =
		Arc::new(move |invocation: ExtensionCommandInvocation, _provenance: CommandProvenance| {
			if activated_for_handler
				.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
				.is_ok()
			{
				activation_count_for_handler.fetch_add(1, Ordering::Relaxed);
			}
			*received_for_handler.lock().expect("argument capture") = Some(invocation);
			async { Ok(CommandResult::Consumed(ConsumedResult::silent())) }
		});
	let provenance = CommandProvenance {
		source:     sf!("fixture"),
		label:      sf!("Fixture"),
		kind:       CommandSourceKind::Extension,
		generation: 11,
	};
	handler
		.call(
			ExtensionCommandInvocation {
				name:    sf!("review"),
				argv:    Arc::from([sf!("one"), sf!("two")]),
				raw:     sf!("one two"),
				surface: CommandSurface::Tui,
			},
			provenance,
		)
		.await
		.expect("fixture command");
	assert_eq!(activation_count.load(Ordering::Relaxed), 1);
	let invocation = received.lock().expect("argument capture").clone().unwrap();
	assert_eq!(invocation.argv.as_ref(), [Str::new("one"), Str::new("two")]);
	assert_eq!(invocation.raw, "one two");

	let replacement = VerifiedUiRoster {
		generation: 12,
		extension:  sf!("fixture"),
		commands:   vec![command("inspect", "inspect", "fixture.commands.inspect")]
			.into_boxed_slice(),
		shortcuts:  vec![shortcut("refresh", "f5", "refresh")].into_boxed_slice(),
	};
	host_roster
		.install(host, &replacement)
		.expect("replacement roster");
	assert!(host_roster.command("review").is_none());
	assert_eq!(host_roster.command("inspect").unwrap().owner.generation, 12);
	let shortcuts = ExtensionShortcutRoster::install(
		&replacement.shortcuts,
		12,
		&ResolvedKeybindings::default(),
		KeyPlatform::Unix,
	)
	.expect("replacement shortcuts");
	assert!(
		shortcuts
			.match_chord("ctrl+alt+h", "open")
			.unwrap()
			.is_none()
	);
	assert_eq!(
		shortcuts
			.match_chord("f5", "open")
			.unwrap()
			.unwrap()
			.generation,
		12
	);
}
