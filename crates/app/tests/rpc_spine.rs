//! RPC transport proof over a scripted journal-first kernel.

use std::{collections::VecDeque, future::ready, sync::Arc, time::SystemTime};

use omp_agent::{DispatchPolicy, Inference, Kernel, StaticPrompt};
use omp_core::Str;
use omp_inference::{
	BlockKind, ChatEvent, ChatRequest, ChatStream, Completion, ExecutionReceipt, FinishReason,
	ProviderId, RequestId, ResponseMeta, RouteId, Usage,
};
use omp_journal::blob::BlobStore;
use omp_session::{ComponentRegistry, Session};
use omp_tool::Registry;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

struct ScriptedInference {
	scripts: VecDeque<Vec<ChatEvent>>,
}

impl Inference for ScriptedInference {
	fn chat(
		&mut self,
		_request: ChatRequest,
	) -> impl Future<Output = Result<ChatStream, omp_inference::Error>> + Send {
		let events = self.scripts.pop_front().expect("one scripted turn");
		ready(Ok(streaming(events)))
	}
}

fn streaming(events: Vec<ChatEvent>) -> ChatStream {
	let events = std::iter::once(ChatEvent::Started(ResponseMeta {
		request_id:          RequestId::from("rpc-script"),
		provider:            ProviderId::from("scripted"),
		route:               RouteId::from("scripted/test"),
		model:               None,
		provider_request_id: None,
		created_at:          SystemTime::UNIX_EPOCH,
	}))
	.chain(events)
	.map(Ok);
	ChatStream::ordinary(Box::pin(futures::stream::iter(events)))
}

#[tokio::test]
async fn rpc_transport_round_trip_streams_patches_from_scripted_kernel() {
	let temp = tempfile::tempdir().expect("tempdir");
	let client = ScriptedInference {
		scripts: VecDeque::from([vec![
			ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text },
			ChatEvent::TextDelta { index: 0, text: Str::new_static("pong") },
			ChatEvent::Completed(Completion {
				reason:  FinishReason::Stop,
				blocks:  1,
				usage:   Usage::default(),
				receipt: ExecutionReceipt::default().into(),
			}),
		]]),
	};
	let spill = BlobStore::open(temp.path().join("blobs")).expect("blob store");
	let kernel = Kernel::new(
		client,
		Arc::new(Registry::new()),
		DispatchPolicy::new(spill),
		StaticPrompt(Str::new_static("system")),
	);
	let session =
		Session::create(temp.path().join("rpc.oms"), ComponentRegistry::standard()).expect("session");
	let (client_io, server_io) = tokio::io::duplex(64 * 1024);
	let (server_read, server_write) = tokio::io::split(server_io);
	let server = omp_app::rpc_mode::serve_rpc(kernel, session, server_read, server_write);
	let client = async move {
		let (client_read, mut client_write) = tokio::io::split(client_io);
		client_write
			.write_all(b"{\"id\":\"1\",\"type\":\"prompt\",\"message\":\"ping\"}\n{\"id\":\"2\",\"type\":\"quit\"}\n")
			.await
			.expect("requests");
		client_write.shutdown().await.expect("shutdown");
		let mut lines = BufReader::new(client_read).lines();
		let mut frames = Vec::<Value>::new();
		while let Some(line) = lines.next_line().await.expect("response") {
			frames.push(serde_json::from_str(&line).expect("json response"));
		}
		frames
	};
	let (server, frames) = tokio::join!(server, client);
	server.expect("server");
	assert!(frames.iter().any(|frame| frame["event"] == "patch@1"));
	let response = frames
		.iter()
		.find(|frame| frame["type"] == "response" && frame["id"] == "1")
		.expect("prompt response");
	assert_eq!(response["data"]["text"], "pong");
}
