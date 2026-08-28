//! Proves Python agent controls preserve inbox messages and bind requests to
//! the exact chat parent.

use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{Stream, stream};
use omp_agent::{
	AgentKind, AgentSnapshot, AgentState, AgentTree, Broker, Budget, DeliveryMode, InvokeFrame,
	Journal, Mailbox, PeerMessage, PromptFacts, TurnClient, TurnInput, TurnOptions, TurnSession,
	control_channel,
};
use omp_catalog::{settings::ModelSettings, snapshot};
use omp_core::{InvocationPhase, LifecyclePhase, Principal, Str, sf};
use omp_driver::{
	chat::{AgentsControlAuthority, ChatParentHost, InteractiveSessionControl},
	hub::share_inbox,
};
use omp_envd::{
	exthost::control::{
		ControlAuthority, ControlAuthorityFactory, ControlConnectionIdentity, ControlEffect,
		ControlInvocationAuthority, ControlProtocolError, ControlRequestContext,
		EnvdControlAuthorities, ExternalControlAuthorities, FixedControlAuthorityFactory,
		HostControlAuthorityFactory, PersistenceControlAuthorities, PolicyControlAuthorities,
		PresentationControlAuthorities, ProviderControlAuthorities, RegistryControlAuthorities,
	},
	worker::{ExtHostConfig, ExtHostSupervisor},
};
use omp_inference::TurnId;
use omp_storage::{
	index::SessionIndex,
	transcript::{Header, SessionId},
};
use serde_json::{Value, json};

fn node(tree: &AgentTree, id: &str, name: &str) -> Arc<omp_agent::AgentNode> {
	tree
		.register(
			Str::from(id),
			Str::from(name),
			AgentKind::Subagent,
			None,
			sf!("session"),
			Budget::default(),
		)
		.expect("register agent")
}

fn message(id: &str, from: &str, to: &str, reply_to: Option<&str>) -> PeerMessage {
	PeerMessage {
		id:            Str::from(id),
		from:          Str::from(from),
		to:            Str::from(to),
		text:          sf!("payload"),
		mode:          DeliveryMode::Aside,
		reply_to:      reply_to.map(Str::from),
		sent_ms:       1,
		session_id:    sf!("session"),
		expects_reply: false,
	}
}

#[tokio::test]
async fn shared_control_inbox_preserves_unmatched_messages_during_reply_wait() {
	let broker = Broker::new(sf!("project"));
	let tree = AgentTree::standard(2);
	let owner = node(&tree, "owner", "Owner");
	let peer = node(&tree, "peer", "Peer");
	let owner_mailbox = Mailbox::new();
	let peer_mailbox = Mailbox::new();
	let inbox = share_inbox(
		broker
			.register(&owner, owner_mailbox.sender())
			.expect("owner inbox"),
	);
	broker
		.register(&peer, peer_mailbox.sender())
		.expect("peer inbox");

	broker
		.route(message("unmatched", "other", "owner", None))
		.expect("buffer unmatched");
	broker
		.route(message("reply", "peer", "owner", Some("request")))
		.expect("buffer reply");

	let reply = inbox
		.lock()
		.await
		.wait_for_timeout(Some("peer"), Some("request"), Some(Duration::from_secs(1)))
		.await
		.expect("wait")
		.expect("correlated reply");
	assert_eq!(reply.id.as_str(), "reply");
	let buffered = inbox.lock().await.inbox(true);
	assert_eq!(buffered.len(), 1);
	assert_eq!(buffered[0].id.as_str(), "unmatched");
}

#[derive(Clone)]
struct NeverTurnClient;

struct NeverTurnSession;

impl TurnClient for NeverTurnClient {
	type Session<'client> = NeverTurnSession;

	async fn turn<'client>(
		&'client self,
		_turn_id: TurnId,
		_input: TurnInput,
		_options: &'client TurnOptions,
	) -> Result<Self::Session<'client>, omp_agent::Error> {
		Err(omp_agent::Error::Closed)
	}
}

impl TurnSession for NeverTurnSession {
	fn events(
		&mut self,
	) -> impl Stream<Item = Result<omp_agent::TurnEvent, omp_agent::Error>> + Send + Unpin + '_ {
		stream::empty()
	}

	async fn submit(&mut self, _frame: InvokeFrame) -> Result<(), omp_agent::Error> {
		Ok(())
	}
}

struct InertAuthority;

#[async_trait]
impl ControlAuthority for InertAuthority {
	fn handles(&self, _operation: &str) -> bool {
		true
	}

	fn authorize(
		&self,
		_context: &ControlRequestContext,
		_operation: &str,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		Ok(())
	}

	async fn request(
		&self,
		_context: ControlRequestContext,
		_operation: Str,
		_arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		Ok(Value::Null)
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		Ok(())
	}
}

fn inert_factory() -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(FixedControlAuthorityFactory::new(Arc::new(InertAuthority)))
}

fn host_factory() -> Arc<HostControlAuthorityFactory> {
	let envd = EnvdControlAuthorities::new(
		RegistryControlAuthorities::new(inert_factory(), inert_factory(), inert_factory()),
		PersistenceControlAuthorities::new(
			inert_factory(),
			inert_factory(),
			inert_factory(),
			inert_factory(),
			inert_factory(),
			inert_factory(),
		),
		PolicyControlAuthorities::new(inert_factory(), inert_factory()),
		PresentationControlAuthorities::new(inert_factory(), inert_factory(), inert_factory()),
		ProviderControlAuthorities::new(inert_factory(), inert_factory(), inert_factory()),
		inert_factory(),
		inert_factory(),
	);
	Arc::new(HostControlAuthorityFactory::new(
		envd,
		ExternalControlAuthorities::new(inert_factory(), inert_factory()),
	))
}

fn control_identity() -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension:          sf!("test.agents"),
		principal:          Principal::new(sf!("test"), sf!("Test")),
		artifact_digest:    sf!("sha256:test"),
		layer:              sf!("project"),
		tier:               sf!("trusted"),
		trust:              sf!("trusted"),
		host_generation:    3,
		session_generation: 11,
		capabilities:       Arc::new(BTreeSet::new()),
	})
}

fn parent(
	scratch: &tempfile::TempDir,
	session: &str,
	marker: &str,
) -> Arc<ChatParentHost<NeverTurnClient>> {
	let root = scratch.path().join(session);
	let sessions = root.join("sessions");
	fs::create_dir_all(&sessions).expect("session directory");
	let state = AgentState::new(AgentSnapshot::new(
		TurnOptions::default(),
		PromptFacts::new(&root, Arc::from([]))
			.props()
			.expect("prompt props"),
		Arc::new(omp_tool::Registry::new()),
	));
	let (env, _transport) = omp_env::EnvClient::in_process(1);
	let parent = Arc::new(ChatParentHost::new(
		NeverTurnClient,
		env,
		state,
		sf!("{session}"),
		sessions,
		root,
		Arc::new(
			SessionIndex::open(scratch.path().join(format!("{session}.sqlite3")))
				.expect("session index"),
		),
		false,
	));
	let root_node = parent
		.tree()
		.register(
			sf!("{session}"),
			sf!("Main"),
			AgentKind::Main,
			None,
			sf!("{session}"),
			Budget::default(),
		)
		.expect("main node");
	parent
		.broker()
		.register(&root_node, Mailbox::new().sender())
		.expect("main broker record");
	let marker_node = parent
		.tree()
		.register(
			sf!("{marker}"),
			sf!("{marker}"),
			AgentKind::Subagent,
			Some(sf!("{session}")),
			sf!("{session}"),
			Budget::default(),
		)
		.expect("marker node");
	parent
		.broker()
		.register(&marker_node, Mailbox::new().sender())
		.expect("marker broker record");
	parent
}

fn request_context(identity: Arc<ControlConnectionIdentity>) -> ControlRequestContext {
	ControlRequestContext { connection: identity, request_id: 1, invocation: None }
}

#[tokio::test]
async fn live_host_agents_requests_follow_the_exact_bound_chat_parent() {
	let mut config = ExtHostConfig::new(
		PathBuf::from("unused"),
		Principal::new(sf!("test"), sf!("Test")),
		sf!("environment-session"),
		11,
	);
	config.bind_control_authorities(host_factory());
	let supervisor = ExtHostSupervisor::spawn(config)
		.await
		.expect("empty live extension supervisor");
	let scratch = tempfile::tempdir().expect("scratch");
	let first = parent(&scratch, "session-one", "only-first");
	let first_lease =
		supervisor.bind_agents_control_authority(AgentsControlAuthority::factory(Arc::clone(&first)));
	let identity = control_identity();
	let first_authority = supervisor
		.control_authority(Arc::clone(&identity))
		.expect("first live host authority");
	let first_context = request_context(Arc::clone(&identity));
	first_authority
		.authorize(&first_context, "omp.agents.list", &serde_json::Map::new())
		.expect("first request authorized");
	let first_roster = first_authority
		.request(first_context.clone(), sf!("omp.agents.list"), serde_json::Map::new())
		.await
		.expect("first live host request");
	assert!(
		first_roster
			.as_array()
			.expect("first roster")
			.iter()
			.any(|row| row["id"] == json!("only-first"))
	);

	let second = parent(&scratch, "session-two", "only-second");
	let second_lease = supervisor
		.bind_agents_control_authority(AgentsControlAuthority::factory(Arc::clone(&second)));
	drop(first_lease);
	let stale = first_authority
		.request(first_context, sf!("omp.agents.list"), serde_json::Map::new())
		.await
		.expect_err("replaced host authority must be revoked");
	assert_eq!(stale.code.as_str(), "AgentsOwnerUnavailable");

	let second_authority = supervisor
		.control_authority(Arc::clone(&identity))
		.expect("replacement live host authority");
	let second_context = request_context(Arc::clone(&identity));
	second_authority
		.authorize(&second_context, "omp.agents.list", &serde_json::Map::new())
		.expect("replacement request authorized");
	let second_roster = second_authority
		.request(second_context, sf!("omp.agents.list"), serde_json::Map::new())
		.await
		.expect("replacement live host request");
	let second_roster = second_roster.as_array().expect("second roster");
	assert!(
		second_roster
			.iter()
			.any(|row| row["id"] == json!("only-second"))
	);
	assert!(
		!second_roster
			.iter()
			.any(|row| row["id"] == json!("only-first"))
	);

	drop(second_lease);
	let revoked = supervisor
		.control_authority(control_identity())
		.expect("dynamic authority remains generation-bound");
	let error = revoked
		.authorize(&request_context(identity), "omp.agents.list", &serde_json::Map::new())
		.expect_err("released owner must reject new requests");
	assert_eq!(error.code.as_str(), "AgentsOwnerUnavailable");
}
#[tokio::test]
async fn set_model_control_request_commits_before_switching_the_live_snapshot() {
	let scratch = tempfile::tempdir().expect("scratch");
	let parent = parent(&scratch, "session-model", "marker");
	let root = scratch.path().join("session-model");
	let catalog = Arc::new(snapshot::Catalog::embedded().clone());
	let selected = catalog
		.models()
		.iter()
		.find(|model| !model.routes.is_empty())
		.expect("routed embedded model")
		.key
		.as_str()
		.to_owned();
	let state = AgentState::new(AgentSnapshot::default());
	let path = root.join("model-control.jsonl");
	let journal = Journal::create(&path, &Header {
		v:       4,
		id:      SessionId(sf!("session-model")),
		created: 1,
		cwd:     root.clone(),
	})
	.expect("journal");
	let (control, mailbox) = control_channel();
	let owner = tokio::spawn(async move {
		let mut journal = journal;
		assert!(matches!(
			mailbox.handle_next(&mut journal).await,
			omp_agent::ControlMailboxEvent::JournalHandled
		));
		journal
			.effective_model_override()
			.expect("read model override")
			.expect("model override")
	});
	let session_control = Arc::new(InteractiveSessionControl::new(
		root.clone(),
		root.join("sessions"),
		Arc::new(SessionIndex::open(root.join("model-index.sqlite3")).expect("session index")),
		Arc::clone(&catalog),
		ModelSettings::default(),
		state.clone(),
		control,
	));
	let authority = AgentsControlAuthority::with_session_control(parent, session_control);
	let identity = control_identity();
	let context = ControlRequestContext {
		connection: Arc::clone(&identity),
		request_id: 9,
		invocation: Some(ControlInvocationAuthority {
			invocation:        sf!("command"),
			phase:             InvocationPhase::EffectsAuthorized,
			session:           sf!("session-model"),
			turn:              None,
			event:             None,
			call:              None,
			device:            None,
			effects:           Box::new([]),
			place_kind:        sf!("host"),
			lifecycle:         LifecyclePhase::Active,
			roots:             Box::new([]),
			remote:            false,
			has_ui:            true,
			headless:          false,
			settings:          serde_json::Map::new(),
			secret_settings:   Box::new([]),
			data:              None,
			direct_filesystem: None,
		}),
	};
	let arguments = serde_json::Map::from_iter([
		("model".to_owned(), Value::String(selected.clone())),
		("thinking".to_owned(), Value::String("high".to_owned())),
	]);
	authority
		.authorize(&context, "omp.agents.set_model", &arguments)
		.expect("set_model authorized");
	let response = authority
		.request(context, sf!("omp.agents.set_model"), arguments)
		.await
		.expect("set_model request");
	let durable = owner.await.expect("journal owner");
	assert_eq!(response["model"], json!(selected));
	assert_eq!(durable.model.model.0.as_str(), selected);
	assert_eq!(state.snapshot().turn.params.model, selected);
}
