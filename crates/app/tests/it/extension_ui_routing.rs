#![cfg(unix)]

use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc};

use omp_app::chat_ui::{
	command_roster_response,
	commands::{
		CommandImplementation, CommandProvenance, CommandResult,
		CommandRoster as StructuralCommandRoster, CommandSourceKind, CommandSurface,
		ExtensionCommandHandler, ExtensionCommandInvocation,
	},
	lower_presentation_effect, lower_presentation_request, lower_presentation_response,
	presentation::{ControlPresentationCallbackDispatcher, PublishedUiRoster},
	presentation_authority::{
		PresentationEffect, PresentationIdentity, PresentationRequest, PresentationResponse,
	},
};
use omp_core::{ArtifactDigest, Provenance, Str, sf};
use omp_envd::{
	ProjectEnvironment, RegistryBridges,
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, ServiceManifest,
		lifecycle::HeadlessLifecycleKind,
	},
	worker::{ExtHostSpec, HostKey},
};
use omp_ext::config::{StaticDeclaration, StaticDeclarations, UiDeclarations};
use omp_proto::{
	inference::v1::{Value as ProtoValue, ValueMap, value as proto_value},
	omp::ui::v1::{DialogOutcome, UiResponse, ui_effect, ui_request, ui_response},
};
use serde_json::json;

const MODULE: &str = "extension_ui_fixture";

fn provenance(key: &HostKey) -> Provenance {
	Provenance::new(
		sf!("fixture"),
		key.extension().clone(),
		sf!("1.0.0"),
		ArtifactDigest::new([7; 32]),
		key.layer().clone(),
		key.tier().clone(),
		1,
	)
}

fn command_row() -> StaticDeclaration {
	StaticDeclaration {
		id: sf!("fixture.command"),
		kind: sf!("command"),
		module: sf!(MODULE),
		trigger: sf!("lazy"),
		key: sf!("inspect"),
		api: 1,
		failure: sf!("fault"),
		properties: BTreeMap::from([
			(sf!("aliases"), json!(["i"])),
			(sf!("description"), json!("Inspect typed arguments")),
			(sf!("hint"), json!("[arguments]")),
			(sf!("callback"), json!(format!("{MODULE}.inspect"))),
		]),
		..StaticDeclaration::default()
	}
}

fn shortcut_row() -> StaticDeclaration {
	StaticDeclaration {
		id: sf!("fixture.shortcut"),
		kind: sf!("shortcut"),
		module: sf!(MODULE),
		trigger: sf!("lazy"),
		key: sf!("ctrl+alt+k"),
		api: 1,
		failure: sf!("fail-open"),
		properties: BTreeMap::from([
			(sf!("action_id"), json!("fixture.inspect")),
			(sf!("description"), json!("Inspect shortcut")),
			(sf!("when"), json!([])),
			(sf!("callback"), json!(format!("{MODULE}.inspect_shortcut"))),
		]),
		..StaticDeclaration::default()
	}
}

fn fixture_extension(site: PathBuf, module_path: PathBuf) -> ExtHostSpec {
	let key = HostKey::new("workspace", "trusted", MODULE);
	let command = command_row();
	let shortcut = shortcut_row();
	let declarations = StaticDeclarations {
		ordered: vec![command.clone(), shortcut.clone()].into_boxed_slice(),
		ui: UiDeclarations {
			commands: vec![command].into_boxed_slice(),
			shortcuts: vec![shortcut].into_boxed_slice(),
			..UiDeclarations::default()
		},
		..StaticDeclarations::default()
	};
	let manifest = ExtensionManifest::new_with_static(
		provenance(&key),
		sf!(MODULE),
		[],
		DeclarationSet::default(),
		ServiceManifest::default(),
		declarations,
		[],
		[ActivationTrigger::FirstReach],
	);
	let mut extension = ExtHostSpec::new(key, manifest);
	extension.python_site = Some(site);
	extension.entry_path = Some(module_path);
	extension.host_executable = Some(PathBuf::from(env!("CARGO_BIN_EXE_omp")));
	extension
}

fn command_handler(
	roster: &PublishedUiRoster,
	session: &Str,
) -> (Arc<dyn ExtensionCommandHandler>, CommandProvenance) {
	let generation = roster
		.command_generations(session)
		.into_iter()
		.next()
		.expect("fixture command generation");
	let declaration = generation
		.declarations
		.first()
		.expect("fixture command declaration");
	let CommandImplementation::Extension(handler) = &declaration.implementation else {
		panic!("fixture command was not installed as an extension callback");
	};
	(Arc::clone(handler), generation.provenance)
}

fn invocation() -> ExtensionCommandInvocation {
	ExtensionCommandInvocation {
		name:    sf!("i"),
		argv:    Arc::from([sf!("one"), sf!("two words")]),
		raw:     sf!("one \"two words\""),
		surface: CommandSurface::Tui,
	}
}

#[tokio::test]
async fn spawned_python_command_shortcut_reload_retires_old_generation() {
	let scratch = tempfile::tempdir().expect("UI routing fixture scratch");
	let root = scratch.path().join("workspace");
	let state = scratch.path().join("state");
	let site = scratch.path().join("site");
	fs::create_dir_all(&root).expect("fixture workspace");
	fs::create_dir_all(&state).expect("fixture state");
	fs::create_dir_all(&site).expect("fixture site");
	let source = r#"from __future__ import annotations
import os
from omp.ui import Prompt, command, shortcut

@command("inspect", aliases=("i",), description="Inspect typed arguments", hint="[arguments]")
def inspect(invocation, ctx):
    return Prompt(
        os.environ["OMP_EXT_HOST_GENERATION"]
        + ":" + invocation.name
        + ":" + "|".join(invocation.argv)
        + ":" + invocation.raw
    )

@shortcut("ctrl+alt+k", action_id="fixture.inspect", description="Inspect shortcut")
def inspect_shortcut(action, ctx):
    return None
"#;
	let module_path = site.join(format!("{MODULE}.py"));
	fs::write(&module_path, source).expect("write Python UI fixture");
	let extension = fixture_extension(site, module_path);
	let environment = ProjectEnvironment::connect_or_start(
		&root,
		&state,
		&state.join("env.sock"),
		&state.join("docs.sock"),
		false,
		None,
		&[extension],
		&[],
		omp_tool::DEFAULT_INTERRUPT_GRACE,
		RegistryBridges::default(),
	)
	.await
	.expect("start spawned Python UI extension");
	let callbacks = environment.extension_callback_dispatcher();
	let roster = PublishedUiRoster::default();
	let invalidations = roster.subscribe();
	let evidence = environment.extension_registry_evidences();
	assert_eq!(evidence.len(), 1, "fixture registry was not sealed");
	let session = evidence[0]
		.session
		.clone()
		.expect("fixture session identity");
	roster
		.replace(evidence, Arc::clone(&callbacks))
		.expect("install first UI roster");
	assert!(matches!(
		invalidations.recv().expect("initial command invalidation"),
		HeadlessLifecycleKind::CommandRosterInvalidated
	));

	let (old_handler, provenance) = command_handler(&roster, &session);
	let result = old_handler
		.call(invocation(), provenance)
		.await
		.expect("dispatch spawned Python command");
	let CommandResult::Prompt(result) = result else {
		panic!("spawned command returned no prompt");
	};
	assert_eq!(result.text, "1:i:one|two words:one \"two words\"");
	let (shortcut, identity, dispatcher) = roster
		.shortcut("ctrl+alt+k")
		.expect("fixture shortcut route");
	assert!(
		ControlPresentationCallbackDispatcher::new(identity, dispatcher)
			.dispatch_shortcut(&shortcut, session.clone(), sf!("ctrl+alt+k"), sf!("idle"))
			.await,
		"spawned Python shortcut failed open",
	);

	let reload = environment.extension_reload_handle();
	reload
		.reload()
		.await
		.expect("reload spawned Python fixture");
	roster
		.replace(reload.registry_evidences(), Arc::clone(&callbacks))
		.expect("atomically replace UI roster");
	assert!(matches!(
		invalidations.recv().expect("reload command invalidation"),
		HeadlessLifecycleKind::CommandRosterInvalidated
	));
	assert!(
		old_handler
			.call(invocation(), CommandProvenance {
				source:     sf!("old"),
				label:      sf!("old"),
				kind:       CommandSourceKind::Extension,
				generation: 1,
			})
			.await
			.is_err(),
		"old callback generation remained reachable after reload",
	);
	let (new_handler, provenance) = command_handler(&roster, &session);
	let result = new_handler
		.call(invocation(), provenance)
		.await
		.expect("dispatch replacement Python command");
	let CommandResult::Prompt(result) = result else {
		panic!("replacement command returned no prompt");
	};
	assert_eq!(result.text, "2:i:one|two words:one \"two words\"");
}
fn presentation_identity() -> PresentationIdentity {
	PresentationIdentity {
		principal:          sf!("fixture"),
		extension:          sf!("extension"),
		artifact_digest:    sf!("sha256:fixture"),
		host_generation:    1,
		session_generation: 1,
		capabilities:       Arc::new(Default::default()),
	}
}

#[test]
fn dialog_round_trip_preserves_fields_and_fixed_outcome_shape() {
	let fields = json!({
		"title": "Configure",
		"fields": [{"id": "name", "kind": "text", "prefill": "omp"}],
		"options": {"timeout": 5000, "countdown": true},
	})
	.as_object()
	.expect("dialog object")
	.clone();
	let request = PresentationRequest::Dialog { kind: sf!("form"), fields };
	let lowered = lower_presentation_request(7, &request).expect("dialog lowers");
	let Some(ui_request::Kind::Dialog(dialog)) = lowered.kind else {
		panic!("dialog request kind");
	};
	let props = dialog.props.expect("lossless dialog props");
	assert!(props.fields.contains_key("fields"));
	assert!(props.fields.contains_key("options"));

	let response = UiResponse {
		kind:  Some(ui_response::Kind::DialogOutcome(DialogOutcome {
			accepted:  true,
			cancelled: false,
			value:     None,
			values:    Vec::new(),
			answers:   Some(ValueMap {
				fields: BTreeMap::from([("name".to_owned(), ProtoValue {
					kind: Some(proto_value::Kind::String("omp".to_owned())),
				})]),
			}),
			reason:    None,
		})),
		props: None,
	};
	let PresentationResponse::Dialog(outcome) =
		lower_presentation_response(request, response).expect("dialog response lowers")
	else {
		panic!("dialog response kind");
	};
	assert_eq!(
		outcome,
		json!({
			"accepted": true,
			"value": null,
			"values": null,
			"answers": {"name": "omp"},
			"reason": null,
		}),
	);
}

#[test]
fn set_editor_text_lowers_to_full_composer_replacement() {
	let effect = PresentationEffect {
		kind: sf!("set_editor_text"),
		body: json!({"text": "replacement"})
			.as_object()
			.expect("effect object")
			.clone(),
	};
	let lowered =
		lower_presentation_effect(&presentation_identity(), &effect).expect("effect lowers");
	let Some(ui_effect::Kind::ComposerEdit(edit)) = lowered.kind else {
		panic!("composer edit effect");
	};
	assert_eq!(edit.start, 0);
	assert_eq!(edit.end, u32::MAX);
	assert_eq!(edit.text, "replacement");
}

#[test]
fn commands_query_projects_live_structural_registry() {
	let response = command_roster_response(&StructuralCommandRoster::builtins());
	let commands = response["commands"].as_array().expect("commands array");
	assert!(!commands.is_empty());
	assert!(commands.iter().all(|command| {
		command["name"].is_string()
			&& command["aliases"].is_array()
			&& command["description"].is_string()
			&& command["source"].is_string()
	}));
	assert!(
		commands
			.iter()
			.any(|command| command["source"] == "builtin")
	);
}
