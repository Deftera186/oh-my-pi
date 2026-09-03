//! Joined proof for live-session hub routing.

use std::{future::ready, sync::Arc, time::SystemTime};

use futures::stream;
use omp_agent::{Inference, Kernel, RunControl, StaticPrompt, TurnInput};
use omp_catalog::{ProviderId, RouteId};
use omp_core::Str;
use omp_driver::{
	sessions::{KernelHandle, SessionId, SessionRegistry},
	subagent::hub::SessionHub,
};
use omp_inference::{
	BlockKind, ChatEvent, ChatRequest, ChatStream, Completion, ExecutionReceipt, FinishReason,
	RequestId, ResponseMeta, Usage,
};
use omp_session::{ComponentRegistry, Session};
use omp_tool::Registry;
use parking_lot::RwLock;

struct OneTurn;

impl Inference for OneTurn {
	fn chat(
		&mut self,
		_request: ChatRequest,
	) -> impl Future<Output = Result<ChatStream, omp_inference::Error>> + Send {
		let events = [
			ChatEvent::Started(ResponseMeta {
				request_id:          RequestId::from("hub-test"),
				provider:            ProviderId::from("test"),
				route:               RouteId::from("test/route"),
				model:               None,
				provider_request_id: None,
				created_at:          SystemTime::UNIX_EPOCH,
			}),
			ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text },
			ChatEvent::TextDelta { index: 0, text: Str::new_static("done") },
			ChatEvent::Completed(Completion {
				reason:  FinishReason::Stop,
				blocks:  1,
				usage:   Usage::default(),
				receipt: ExecutionReceipt::default().into(),
			}),
		]
		.into_iter()
		.map(Ok);
		ready(Ok(ChatStream::ordinary(Box::pin(stream::iter(events)))))
	}
}

#[tokio::test]
async fn send_lands_in_child_steering_and_inbox_reads_it() {
	let temp = tempfile::tempdir().expect("temporary directory");
	let mut child = Session::create(temp.path().join("child.oms"), ComponentRegistry::standard())
		.expect("child session");
	let spill =
		omp_journal::blob::BlobStore::open(temp.path().join("artifacts")).expect("artifact store");
	let mut kernel = Kernel::new(
		OneTurn,
		Arc::new(Registry::new()),
		omp_agent::DispatchPolicy::new(spill),
		StaticPrompt(Str::new_static("test")),
	);
	let sessions = SessionRegistry::new();
	sessions.register(Str::new_static("Child"), KernelHandle {
		id:       SessionId::new("child"),
		name:     Str::new_static("Child"),
		up:       kernel.mailbox(),
		snapshot: Arc::new(RwLock::new(child.dom().snapshot())),
	});

	SessionHub::send(&sessions, "child", Str::new_static("please adjust")).expect("hub send");
	kernel
		.run_turn(
			&mut child,
			TurnInput { text: Str::new_static("work"), attachments: Vec::new() },
			RunControl::default(),
		)
		.await
		.expect("child turn");

	let response = SessionHub::inbox(&mut child, true).expect("hub inbox");
	assert!(response.text.as_str().contains("please adjust"));
	let drained = SessionHub::inbox(&mut child, false).expect("hub drain");
	assert!(drained.text.as_str().contains("please adjust"));
	assert!(
		SessionHub::inbox(&mut child, true)
			.expect("empty inbox")
			.useless
	);
}
