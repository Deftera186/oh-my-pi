use std::{env, fs, sync::Arc};

use omp_agent::{
	AgentEvent, AgentRunSummary, ApprovalBook, ApprovalDecision, ApprovalRoute, ApprovalSource,
	ApprovalSpec, EventBus, EventProvenance, EventVisibility, Journal, RunSettlement, TicketState,
};
use omp_core::{ToolPath, sf};
use omp_proto::{
	inference::{
		v1,
		v1::{Outcome, StopReason},
	},
	thread::v1::{self as thread, Item, Message, Part, Role, item, part},
};
use omp_storage::transcript::{Header, SessionId};
use omp_tool::{Rev, ToolIdentity};

fn message(role: Role, text: &str) -> Item {
	Item {
		kind: Some(item::Kind::Message(Message {
			role:  role as i32,
			parts: vec![Part { kind: Some(part::Kind::Text(text.to_owned())) }],
		})),
		..Item::default()
	}
}

fn approval_spec() -> ApprovalSpec {
	ApprovalSpec {
		title:         sf!("Run tool"),
		body:          sf!("The scripted tool requires approval."),
		subject:       sf!("scripted"),
		kind:          sf!("exec"),
		scopes:        vec![sf!("once")],
		default:       None,
		route:         sf!("headless"),
		approver:      None,
		timeout_ms:    1_000,
		unreachable:   sf!("fail_closed"),
		require_human: true,
		pattern:       None,
		evidence:      Vec::new(),
	}
}

#[tokio::test]
async fn headless_runtime() {
	let path = env::temp_dir().join(format!(
		"omp-headless-runtime-{}-{}.jsonl",
		std::process::id(),
		omp_core::Ulid::generate()
	));
	let mut journal = Journal::create(&path, &Header {
		v:       4,
		id:      SessionId(sf!("headless-runtime")),
		created: 1,
		cwd:     env::temp_dir(),
	})
	.expect("create v4 journal");
	journal
		.append_optimistic(1, message(Role::User, "run scripted"), None)
		.expect("journal user input");
	journal
		.append_optimistic(2, message(Role::Assistant, "scripted complete"), None)
		.expect("journal assistant output");
	drop(journal);
	let reopened = Journal::open(&path).expect("reopen durable journal");
	assert_eq!(reopened.live_item_events().expect("live replay").len(), 2);

	let bus = EventBus::new();
	bus.set_session_generation(9);
	let events = bus.subscribe_lossless();
	let identity = ToolIdentity { name: sf!("scripted"), rev: Rev { family: sf!("test"), n: 1 } };
	bus.publish(AgentEvent::ToolObserved {
		call_id:            sf!("call-1"),
		identity:           identity.clone(),
		path:               Some(ToolPath::new(sf!("scripted")).expect("typed path")),
		visibility:         EventVisibility::User,
		provenance:         EventProvenance::Model,
		session_generation: bus.session_generation(),
	});
	bus.publish(AgentEvent::ToolOpened {
		call_id: sf!("call-1"),
		name:    identity.name.clone(),
		rev:     identity.rev.clone(),
	});
	bus.publish(AgentEvent::ToolFinished {
		call_id: sf!("call-1"),
		item:    Item {
			kind: Some(item::Kind::ToolResult(thread::ToolResult {
				call_id: "call-1".to_owned(),
				name: "scripted".to_owned(),
				parts: vec![Part { kind: Some(part::Kind::Text("ok".to_owned())) }],
				..thread::ToolResult::default()
			})),
			..Item::default()
		},
		usage:   v1::Usage::default(),
	});
	assert!(matches!(events.try_recv().unwrap().as_ref(), AgentEvent::ToolObserved {
		session_generation: 9,
		..
	}));
	assert!(matches!(events.try_recv().unwrap().as_ref(), AgentEvent::ToolOpened { .. }));
	assert!(matches!(events.try_recv().unwrap().as_ref(), AgentEvent::ToolFinished { .. }));

	let book = Arc::new(ApprovalBook::new());
	let (route, inbox) = ApprovalRoute::new(Arc::clone(&book), None);
	let waiting = route.request(Some(sf!("call-1")), vec![approval_spec()], 3);
	let answering = async {
		let request = inbox.recv().await.expect("approval dispatched");
		assert_eq!(request.ticket.state, TicketState::Pending);
		request
			.respond(ApprovalDecision {
				approved:   true,
				scope:      sf!("once"),
				source:     ApprovalSource::User,
				decided_by: Some(sf!("fixture")),
				reason:     None,
				audited:    false,
			})
			.expect("answer approval");
	};
	let (approved, ()) = tokio::join!(waiting, answering);
	assert!(approved.decision.expect("decision").approved);

	let (lost_route, lost_inbox) = ApprovalRoute::new(Arc::new(ApprovalBook::new()), None);
	drop(lost_inbox);
	let denied = lost_route
		.request(Some(sf!("call-lost")), vec![approval_spec()], 4)
		.await
		.decision
		.expect("host loss settles");
	assert!(!denied.approved);
	assert_eq!(denied.source, ApprovalSource::Unavailable);

	let summary = AgentRunSummary::settled(
		Outcome {
			output: vec![message(Role::Assistant, "scripted complete")],
			stop: StopReason::StopEndTurn as i32,
			..Outcome::default()
		},
		1,
		false,
	);
	assert_eq!(summary.settlement, RunSettlement::Success);
	assert_eq!(summary.final_assistant(), Some("scripted complete"));
	assert_eq!(AgentRunSummary::terminal_fault().settlement, RunSettlement::TerminalFault);

	fs::remove_file(path).expect("remove journal fixture");
}
