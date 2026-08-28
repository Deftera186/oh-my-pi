//! Executable P7 proof for the real chat TUI, interruption, and terminal
//! restoration.

#![feature(impl_trait_in_assoc_type)]
#![cfg(unix)]

use std::{
	collections::VecDeque,
	ffi,
	fmt::Write as _,
	fs,
	io::{self, BufRead as _, BufReader, Read as _, Write as _},
	os::{
		fd::{self, AsFd as _, AsRawFd as _},
		unix::net::UnixStream,
	},
	path::Path,
	process::{self, Child, Command, Stdio},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	task::{Context, Poll},
	thread,
	time::{Duration, Instant},
};

use bytes::Bytes;
use flume::{Receiver, Sender};
use futures::StreamExt as _;
use nix::{
	errno::Errno,
	fcntl::{FcntlArg, OFlag, fcntl},
	pty::{Winsize, openpty},
	sys::termios::{Termios, cfgetispeed, cfgetospeed, tcgetattr},
	unistd::ttyname,
};
use omp_app::{
	daemon::{DaemonConfig, DaemonHandle},
	endpoint::LocalEndpoint,
};
use omp_catalog::{
	ManagementCapabilities, OperationBits, OperationKind,
	snapshot::{Catalog, SnapshotProvenance},
};
use omp_core::{Str, sf};
use omp_inference::{
	Answer, Error as InferenceError, Registry,
	answer::{AnswerBody, ChatStream},
	call::{Call, ContentPart, OpaqueJson, OperationCall},
	event::{BlockKind, ChatEvent, Completion, FinishReason, ToolCall, WorkflowResponse},
	id::ToolCallId,
	layer::{LayerCall, stack::RouteProviderService},
	provider::fake::{FakeProvider, FakeScript},
	receipt::{Cost, ExecutionReceipt, ReasonId, Usage, UsageSource},
	registry::RouteUnavailable,
	session::ConversationSessionPlanner,
};
use omp_storage::{
	index::{NewSession, SessionIndex, SessionKind},
	transcript::{Header, SessionId},
};
use omp_tool::{Claims, Constraint, Effects, Precedence, Presentation, Rev, ToolSpec};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::time;
use tower::Service;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(30);
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct GatedRoute {
	fake:            FakeProvider,
	gates:           Arc<Mutex<VecDeque<Receiver<()>>>>,
	captures:        Arc<Mutex<Vec<Call>>>,
	preview_reached: Sender<()>,
	preview_release: Receiver<()>,
}

impl Service<LayerCall<Call>> for GatedRoute {
	type Error = InferenceError;
	type Response = Answer;

	type Future = impl Future<Output = Result<Answer, InferenceError>> + Send;

	fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		<FakeProvider as Service<Call>>::poll_ready(&mut self.fake, context)
	}

	fn call(&mut self, request: LayerCall<Call>) -> Self::Future {
		let gate = self
			.gates
			.lock()
			.pop_front()
			.expect("every scripted provider call has a gate");
		let call_index = {
			let mut captures = self.captures.lock();
			let index = captures.len();
			captures.push(request.payload.clone());
			index
		};
		let response = <FakeProvider as Service<Call>>::call(&mut self.fake, request.payload);
		let preview_reached = self.preview_reached.clone();
		let preview_release = self.preview_release.clone();
		async move {
			gate
				.recv_async()
				.await
				.expect("scripted provider gate remains open");
			let Answer { meta, receipt, body } = response.await?;
			let body = if call_index == 1 {
				match body {
					AnswerBody::Chat(mut chat) => {
						let events = async_stream::stream! {
							let mut pause_pending = true;
							while let Some(event) = chat.next().await {
								let pause = pause_pending
									&& matches!(&event, Ok(ChatEvent::ToolArgumentsDelta { .. }));
								pause_pending &= !pause;
								yield event;
								if pause {
									preview_reached
										.send_async(())
										.await
										.expect("preview observer remains open");
									preview_release
										.recv_async()
										.await
										.expect("preview release remains open");
								}
							}
						};
						AnswerBody::Chat(ChatStream::ordinary(Box::pin(events)))
					},
					body => body,
				}
			} else {
				body
			};
			Ok(Answer { meta, receipt, body })
		}
	}
}

struct ScriptedGateway {
	_handle:         DaemonHandle,
	model:           String,
	permits:         Vec<Sender<()>>,
	captures:        Arc<Mutex<Vec<Call>>>,
	preview_reached: Receiver<()>,
	preview_release: Sender<()>,
	_responses:      Receiver<WorkflowResponse>,
}

impl ScriptedGateway {
	async fn start(scratch: &Path, socket: &Path, shell_release: &Path) -> Self {
		let scripts = scripts(shell_release);
		Self::start_with_scripts(scratch, socket, scripts).await
	}

	async fn start_with_scripts(scratch: &Path, socket: &Path, scripts: Vec<FakeScript>) -> Self {
		let mut senders = Vec::with_capacity(scripts.len());
		let mut receivers = VecDeque::with_capacity(scripts.len());
		for _ in 0..scripts.len() {
			let (sender, receiver) = flume::bounded(1);
			senders.push(sender);
			receivers.push_back(receiver);
		}
		let captures = Arc::new(Mutex::new(Vec::with_capacity(scripts.len())));
		let (preview_reached_tx, preview_reached) = flume::bounded(1);
		let (preview_release, preview_release_rx) = flume::bounded(1);
		let (registry, sessions, fake, model) = scripted_registry(
			scratch,
			receivers,
			Arc::clone(&captures),
			preview_reached_tx,
			preview_release_rx,
		);
		fake.extend(scripts);

		let mut tools = omp_tool::Registry::new();
		for name in [
			"checkpoint",
			"rewind",
			"ask",
			"ast_edit",
			"ast_grep",
			"bash",
			"debug",
			"edit",
			"eval",
			"glob",
			"grep",
			"hub",
			"lsp",
			"think",
			"todo",
			"web_search",
			"write",
			"read",
		] {
			tools
				.register_worker(
					ToolSpec {
						name:            sf!(name),
						rev:             Rev {
							family: if name == "edit" {
								sf!("hl")
							} else {
								Str::default()
							},
							n:      1,
						},
						description:     sf!("P7 gateway executor declaration"),
						schema:          Bytes::from_static(br#"{"type":"object"}"#),
						constraint:      Constraint::None,
						effects:         Effects::empty(),
						projection_code: [0; 32],
					},
					Presentation::Device,
					Claims {
						precedence: Precedence::DEFAULT,
						claimant:   sf!("test/worker"),
						replaces:   None,
					},
				)
				.expect("proof tool registers");
		}
		let (responses, incoming) = flume::bounded(32);
		let handle = time::timeout(
			READY_TIMEOUT,
			DaemonHandle::start_for_test(
				DaemonConfig::local(LocalEndpoint::from(socket.to_path_buf()))
					.with_data_dir(scratch.join("gateway-state")),
				registry,
				sessions,
				Arc::new(tools),
				responses,
			),
		)
		.await
		.expect("gateway startup timed out")
		.expect("scripted gateway starts");
		Self {
			_handle: handle,
			model,
			permits: senders,
			captures,
			preview_reached,
			preview_release,
			_responses: incoming,
		}
	}

	fn release(&self, call: usize) {
		self.permits[call]
			.send(())
			.expect("scripted call gate remains open");
	}

	fn captured_text(&self, call: usize, expected: &str) -> bool {
		let captures = self.captures.lock();
		let Some(call) = captures.get(call) else {
			return false;
		};
		let OperationCall::Chat(request) = &call.operation else {
			return false;
		};
		request.messages.iter().any(|message| {
			message
				.content
				.iter()
				.any(|part| matches!(part, ContentPart::Text { text, .. } if text.contains(expected)))
		})
	}

	async fn await_preview(&self) {
		match time::timeout(CHECKPOINT_TIMEOUT, self.preview_reached.recv_async()).await {
			Ok(Ok(())) => {},
			Ok(Err(error)) => panic!("edit preview stream observer closed: {error}"),
			Err(_) => panic!(
				"edit preview stream pause timed out after {} captured provider calls",
				self.captures.lock().len()
			),
		}
	}

	fn release_preview(&self) {
		self
			.preview_release
			.send(())
			.expect("edit preview stream remains paused");
	}
}

fn scripted_registry(
	scratch: &Path,
	gates: VecDeque<Receiver<()>>,
	captures: Arc<Mutex<Vec<Call>>>,
	preview_reached: Sender<()>,
	preview_release: Receiver<()>,
) -> (Registry, ConversationSessionPlanner, FakeProvider, String) {
	let mut compiled = Catalog::embedded().compiled().clone();
	for provider in &mut compiled.providers {
		provider.management = ManagementCapabilities {
			operations:        OperationBits::empty(),
			multiple_accounts: false,
			refresh:           false,
			principal_quota:   false,
		};
	}
	let artifacts = Catalog::encode(compiled, SnapshotProvenance { source_digest: [0; 32] })
		.expect("catalog snapshot");
	let catalog = Arc::new(Catalog::decode(&artifacts.postcard).expect("catalog decode"));
	let model = catalog
		.models()
		.iter()
		.find(|candidate| {
			candidate
				.capabilities
				.operations
				.contains_kind(OperationKind::Chat)
		})
		.expect("chat model");
	let model_key = model.key.as_str().to_owned();
	let route_id = model.routes.first().expect("chat route").clone();
	let route = catalog.route(&route_id).expect("selected route");
	let fake = FakeProvider::new(route.provider.clone(), route_id.clone());
	let route_service = RouteProviderService::new(GatedRoute {
		fake: fake.clone(),
		gates: Arc::new(Mutex::new(gates)),
		captures,
		preview_reached,
		preview_release,
	});
	let mut builder = Registry::builder(catalog.clone());
	for candidate in catalog.routes() {
		builder = if candidate.id == route_id {
			builder
				.register_route(candidate.id.clone(), route_service.clone())
				.expect("scripted route registers")
		} else {
			builder
				.register_unavailable(RouteUnavailable {
					route:     candidate.id.clone(),
					reason:    ReasonId(sf!("p7-scripted-route-only")),
					operation: None,
				})
				.expect("unavailable route registers")
		};
	}
	let sessions = ConversationSessionPlanner::open(scratch.join("sessions.db"), catalog)
		.expect("conversation store opens");
	(builder.build().expect("base registry"), sessions, fake, model_key)
}

fn tool_script(calls: &[(&str, &str, Value)]) -> FakeScript {
	let mut events = Vec::with_capacity(calls.len() * 3 + 1);
	for (index, (id, name, arguments)) in calls.iter().enumerate() {
		let index = u32::try_from(index).expect("small scripted batch");
		let id = ToolCallId::from(*id);
		events.push(Ok(ChatEvent::ToolCallStarted { index, id: id.clone(), name: Str::from(*name) }));
		events.push(Ok(ChatEvent::ToolArgumentsDelta {
			index,
			bytes: Bytes::from(serde_json::to_vec(arguments).expect("tool args encode")),
		}));
		events.push(Ok(ChatEvent::ToolCallReady {
			index,
			call: ToolCall {
				id,
				name: Str::from(*name),
				arguments: OpaqueJson::new(arguments.clone()),
			},
		}));
	}
	events.push(Ok(completed(FinishReason::ToolCalls, calls.len())));
	FakeScript::chat(events)
}

fn text_script(text: &'static str) -> FakeScript {
	FakeScript::chat(vec![
		Ok(ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text }),
		Ok(ChatEvent::TextDelta { index: 0, text: Str::from(text) }),
		Ok(completed(FinishReason::Stop, 1)),
	])
}
/// A provider stream whose thinking block is closed implicitly by the
/// following text block, mirroring reasoning-capable providers.
fn thinking_text_script(thinking: &'static str, answer: &'static str) -> FakeScript {
	FakeScript::chat(vec![
		Ok(ChatEvent::BlockStarted { index: 0, kind: BlockKind::Thinking }),
		Ok(ChatEvent::ThinkingDelta { index: 0, text: Str::from(thinking) }),
		Ok(ChatEvent::BlockStarted { index: 1, kind: BlockKind::Text }),
		Ok(ChatEvent::TextDelta { index: 1, text: Str::from(answer) }),
		Ok(completed(FinishReason::Stop, 2)),
	])
}

fn metered_text_script(text: &'static str) -> FakeScript {
	let usage = Usage {
		input_tokens: 4_096,
		output_tokens: 128,
		source: UsageSource::Provider,
		..Usage::default()
	};
	let receipt = ExecutionReceipt {
		usage,
		cost: Cost::from_micro_usd(1_500_000),
		..ExecutionReceipt::default()
	};
	FakeScript::chat(vec![
		Ok(ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text }),
		Ok(ChatEvent::TextDelta { index: 0, text: Str::from(text) }),
		Ok(ChatEvent::Completed(Completion {
			reason: FinishReason::Stop,
			blocks: 1,
			usage,
			receipt: receipt.into(),
		})),
	])
}

fn streaming_edit_script() -> FakeScript {
	let arguments = json!({ "input": "[scratch.txt#5C9F]\nPUT 1.=1:\n+new" });
	let call = ToolCall {
		id:        ToolCallId::from("edit-1"),
		name:      sf!("edit"),
		arguments: OpaqueJson::new(arguments),
	};
	FakeScript::chat(vec![
		Ok(ChatEvent::ToolCallStarted { index: 0, id: call.id.clone(), name: call.name.clone() }),
		Ok(ChatEvent::ToolArgumentsDelta {
			index: 0,
			bytes: Bytes::from_static(br#"{"input":"[scratch.txt#5C9F]\nPUT 1.=1:\n+new""#),
		}),
		Ok(ChatEvent::ToolArgumentsDelta { index: 0, bytes: Bytes::from_static(br"}") }),
		Ok(ChatEvent::ToolCallReady { index: 0, call }),
		Ok(completed(FinishReason::ToolCalls, 1)),
	])
}

fn completed(reason: FinishReason, blocks: usize) -> ChatEvent {
	ChatEvent::Completed(Completion {
		reason,
		blocks: blocks.try_into().unwrap(),
		usage: Usage::default(),
		receipt: ExecutionReceipt::default().into(),
	})
}

fn scripts(shell_release: &Path) -> Vec<FakeScript> {
	let release = shell_quote(shell_release);
	let fixture_root = shell_release.parent().expect("shell fixture has parent");
	let batch_one_marker = shell_quote(&fixture_root.join("p7-b1-side-effect"));
	let batch_two_marker = shell_quote(&fixture_root.join("p7-b2-side-effect"));
	let batch_three_marker = shell_quote(&fixture_root.join("p7-b3-side-effect"));
	let queue_marker = shell_quote(&fixture_root.join("p7-queue-side-effect"));
	vec![
		tool_script(&[("read-1", "read", json!({ "path": "scratch.txt" }))]),
		streaming_edit_script(),
		tool_script(&[(
			"shell-1",
			"bash",
			json!({
				"command": format!(
					"printf '\\154\\151\\166\\145\\055\\164\\141\\151\\154\\n'; while [ ! -f {release} ]; do sleep 0.05; done; printf 'live-error\\n' >&2; exit $((3 + 4))"
				)
			}),
		)]),
		tool_script(&[("unknown-1", "think", json!({ "thoughts": "P7 generic card proof" }))]),
		metered_text_script("The deterministic tool sequence is complete."),
		tool_script(&[
			(
				"batch-1",
				"bash",
				json!({ "command": format!("touch {batch_one_marker}; printf '\\142\\141\\164\\143\\150\\055\\157\\156\\145\\055\\163\\164\\141\\162\\164\\145\\144\\n'; sleep 30") }),
			),
			(
				"batch-2",
				"bash",
				json!({ "command": format!("touch {batch_two_marker}; printf '\\142\\141\\164\\143\\150\\055\\164\\167\\157\\055\\162\\141\\156\\n'") }),
			),
			(
				"batch-3",
				"bash",
				json!({ "command": format!("touch {batch_three_marker}; printf '\\142\\141\\164\\143\\150\\055\\164\\150\\162\\145\\145\\055\\162\\141\\156\\n'") }),
			),
		]),
		tool_script(&[(
			"queue-batch",
			"bash",
			json!({ "command": format!("touch {queue_marker}; printf '\\161\\165\\145\\165\\145\\055\\142\\141\\164\\143\\150\\055\\154\\151\\166\\145\\n'; sleep 30") }),
		)]),
		text_script("The plain Enter steering ran before the queued follow-up."),
		text_script("The queued follow-up ran after all prior work."),
		text_script("Second session retained content."),
	]
}

fn shell_quote(path: &Path) -> String {
	format!("'{}'", path.display().to_string().replace(['’', '\''], "'\\''"))
}

#[derive(Clone, Debug)]
struct Snapshot {
	text:  String,
	frame: String,
}

impl Snapshot {
	fn combined(&self) -> String {
		format!("{}\n{}", self.text, self.frame)
	}
}

struct DebugClient {
	reader: BufReader<UnixStream>,
	writer: UnixStream,
}

impl DebugClient {
	fn connect(path: &Path, deadline: Instant, process: &mut PtyChild) -> Self {
		loop {
			let problem = match UnixStream::connect(path) {
				Ok(stream) => {
					stream
						.set_read_timeout(Some(IO_TIMEOUT))
						.expect("debug read timeout");
					stream
						.set_write_timeout(Some(IO_TIMEOUT))
						.expect("debug write timeout");
					let writer = stream.try_clone().expect("clone debug socket");
					let mut client = Self { reader: BufReader::new(stream), writer };
					match client.op("info") {
						Ok(_) => return client,
						Err(error) => error,
					}
				},
				Err(error) => error.to_string(),
			};
			if let Some(status) = process
				.child
				.try_wait()
				.expect("poll chat during debug startup")
			{
				let mut stdout = String::new();
				let mut stderr = String::new();
				if let Some(mut pipe) = process.child.stdout.take() {
					pipe.read_to_string(&mut stdout).expect("read early stdout");
				}
				if let Some(mut pipe) = process.child.stderr.take() {
					pipe.read_to_string(&mut stderr).expect("read early stderr");
				}
				panic!(
					"chat exited before debug socket: {status}\nconnect: {problem}\nstdout: \
					 {stdout}\nstderr: {stderr}\nraw PTY:\n{}",
					visible(&process.raw()),
				);
			}
			assert!(
				Instant::now() < deadline,
				"debug socket did not become ready: {problem}\nraw PTY:\n{}",
				visible(&process.raw()),
			);
			thread::sleep(Duration::from_millis(20));
		}
	}

	fn request(&mut self, request: Value) -> Result<Value, String> {
		serde_json::to_writer(&mut self.writer, &request).map_err(|error| error.to_string())?;
		self
			.writer
			.write_all(b"\n")
			.map_err(|error| error.to_string())?;
		self.writer.flush().map_err(|error| error.to_string())?;
		let mut line = String::new();
		self
			.reader
			.read_line(&mut line)
			.map_err(|error| error.to_string())?;
		if line.is_empty() {
			return Err("debug socket closed".to_owned());
		}
		let response: Value = serde_json::from_str(&line).map_err(|error| error.to_string())?;
		if response.get("ok").and_then(Value::as_bool) != Some(true) {
			return Err(format!("debug request {request} failed: {response}"));
		}
		Ok(response)
	}

	fn op(&mut self, op: &'static str) -> Result<Value, String> {
		self.request(json!({ "op": op }))
	}

	fn keys(&mut self, keys: &str) {
		self
			.request(json!({ "op": "keys", "keys": keys }))
			.unwrap_or_else(|error| panic!("key injection failed: {error}"));
	}

	fn snapshot(&mut self) -> Result<Snapshot, String> {
		let text = lines(&self.op("text")?);
		Ok(Snapshot { frame: text.clone(), text })
	}
}

fn lines(response: &Value) -> String {
	response
		.get("lines")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.collect::<Vec<_>>()
		.join("\n")
}

struct PtyChild {
	child:      Child,
	master:     fd::OwnedFd,
	slave:      fd::OwnedFd,
	before:     Termios,
	raw:        Arc<Mutex<Vec<u8>>>,
	reader_end: Arc<AtomicBool>,
	reader:     Option<thread::JoinHandle<()>>,
}

impl PtyChild {
	fn spawn(binary: &Path, args: &[String], project: &Path, debug: &Path) -> Self {
		let window = Winsize { ws_row: 48, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
		let pty = openpty(Some(&window), None).expect("open PTY");
		let device = ttyname(&pty.slave).expect("PTY slave path");
		let before = tcgetattr(&pty.slave).expect("initial PTY termios");
		fcntl(&pty.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("nonblocking PTY master");
		let reader_fd = pty.master.try_clone().expect("clone PTY master");
		let raw = Arc::new(Mutex::new(Vec::new()));
		let reader_raw = raw.clone();
		let reader_end = Arc::new(AtomicBool::new(false));
		let reader_stop = reader_end.clone();
		let reader = thread::spawn(move || {
			let mut buffer = [0_u8; 16 * 1024];
			loop {
				match nix::unistd::read(&reader_fd, &mut buffer) {
					Ok(0) if reader_stop.load(Ordering::Acquire) => break,
					Ok(0) => thread::sleep(Duration::from_millis(5)),
					Ok(count) => reader_raw.lock().extend_from_slice(&buffer[..count]),
					Err(Errno::EAGAIN) if reader_stop.load(Ordering::Acquire) => break,
					Err(Errno::EAGAIN) => thread::sleep(Duration::from_millis(5)),
					Err(Errno::EIO) => break,
					Err(error) => panic!("PTY read failed: {error}"),
				}
			}
		});

		let home = project.parent().expect("project has parent").join("home");
		fs::create_dir_all(&home).expect("create isolated home");
		let child = Command::new(binary)
			.args(args)
			.current_dir(project)
			.env("TERM", "xterm-256color")
			.env("HOME", &home)
			.env("OMP_DATA_DIR", home.join("data"))
			.env("OMP_TTY", &device)
			.env("OMP_TUI_DEBUG", debug)
			.env("NO_COLOR", "1")
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.expect("spawn omp chat");
		Self {
			child,
			master: pty.master,
			slave: pty.slave,
			before,
			raw,
			reader_end,
			reader: Some(reader),
		}
	}

	fn resize(&self, rows: u16, cols: u16) {
		let window = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
		// SAFETY: master is a live PTY and window is a valid winsize value.
		let result =
			unsafe { libc::ioctl(self.master.as_fd().as_raw_fd(), libc::TIOCSWINSZ, &window) };
		assert_eq!(result, 0, "TIOCSWINSZ failed: {}", io::Error::last_os_error());
	}

	fn raw(&self) -> Vec<u8> {
		self.raw.lock().clone()
	}

	fn wait(mut self, timeout: Duration) -> (process::ExitStatus, Vec<u8>, String, String, Termios) {
		let deadline = Instant::now() + timeout;
		let status = loop {
			match self.child.try_wait().expect("poll omp chat") {
				Some(status) => break status,
				None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
				None => {
					let raw = visible(&self.raw());
					let _ = self.child.kill();
					panic!("omp chat did not exit in {timeout:?}; raw PTY:\n{raw}");
				},
			}
		};
		self.reader_end.store(true, Ordering::Release);
		if let Some(reader) = self.reader.take() {
			reader.join().expect("PTY reader joins");
		}
		let mut stdout = String::new();
		let mut stderr = String::new();
		if let Some(mut pipe) = self.child.stdout.take() {
			pipe.read_to_string(&mut stdout).expect("read child stdout");
		}
		if let Some(mut pipe) = self.child.stderr.take() {
			pipe.read_to_string(&mut stderr).expect("read child stderr");
		}
		let after = tcgetattr(&self.slave).expect("final PTY termios");
		(status, self.raw(), stdout, stderr, after)
	}
}

fn wait_snapshot(
	debug: &mut DebugClient,
	raw: &Arc<Mutex<Vec<u8>>>,
	label: &str,
	mut ready: impl FnMut(&Snapshot) -> bool,
) -> Snapshot {
	let deadline = Instant::now() + CHECKPOINT_TIMEOUT;
	let mut last = None;
	let mut error = None;
	loop {
		match debug.snapshot() {
			Ok(snapshot) if ready(&snapshot) => return snapshot,
			Ok(snapshot) => last = Some(snapshot),
			Err(problem) => error = Some(problem),
		}
		if Instant::now() >= deadline {
			let snapshot = last.map_or_else(|| "<none>".to_owned(), |value| format!("{value:#?}"));
			panic!(
				"checkpoint {label:?} timed out\nlast error: {error:?}\nlast \
				 snapshot:\n{snapshot}\nraw PTY:\n{}",
				visible(&raw.lock()),
			);
		}
		thread::sleep(Duration::from_millis(15));
	}
}

fn wait_info(debug: &mut DebugClient, label: &str, mut ready: impl FnMut(&Value) -> bool) -> Value {
	let deadline = Instant::now() + CHECKPOINT_TIMEOUT;
	loop {
		let info = debug
			.op("info")
			.unwrap_or_else(|error| panic!("{label}: {error}"));
		if ready(&info) {
			return info;
		}
		assert!(Instant::now() < deadline, "checkpoint {label:?} timed out: {info}");
		thread::sleep(Duration::from_millis(15));
	}
}

fn assert_surface(snapshot: &Snapshot, label: &str) {
	assert!(!snapshot.text.trim().is_empty(), "{label}: published terminal surface is empty");
}

fn visible(bytes: &[u8]) -> String {
	let mut out = String::new();
	for &byte in &bytes[bytes.len().saturating_sub(96 * 1024)..] {
		match byte {
			b'\n' => out.push('\n'),
			b'\r' => out.push_str("\\r"),
			b'\t' => out.push_str("\\t"),
			0x20..=0x7e => out.push(char::from(byte)),
			_ => write!(out, "\\x{byte:02x}").expect("writing to String cannot fail"),
		}
	}
	out
}

/// Seeds one resumable interactive session journal and its index row.
fn seed_session(state_dir: &Path, project: &Path, id: &str) {
	let sessions = state_dir.join("sessions");
	fs::create_dir_all(&sessions).expect("create chat session directory");
	let session_id = SessionId(Str::from(id));
	let session_index =
		SessionIndex::open(sessions.join("sessions.sqlite3")).expect("open session index");
	let project_text = project.to_string_lossy();
	let journal = sessions.join(format!("{id}.jsonl"));
	session_index
		.create_session(
			&NewSession {
				id:         &session_id,
				cwd:        project_text.as_ref(),
				project:    project_text.as_ref(),
				created_ms: 1,
				kind:       SessionKind::Interactive,
				parent:     None,
				remote:     false,
			},
			|| {
				let mut header = serde_json::to_vec(&Header {
					v:       4,
					id:      session_id.clone(),
					created: 1,
					cwd:     project.to_path_buf(),
				})
				.map_err(io::Error::other)?;
				header.push(b'\n');
				fs::write(&journal, header)?;
				Ok::<_, io::Error>(((), 0))
			},
		)
		.expect("create resumable TUI session");
}

fn assert_restored(raw: &[u8], before: &Termios, after: &Termios, diagnostics: &str) {
	let alt_enter = raw.windows(8).rposition(|window| window == b"\x1b[?1049h");
	let alt_exit = raw.windows(8).rposition(|window| window == b"\x1b[?1049l");
	assert!(
		alt_enter.is_none() || alt_exit.is_some_and(|exit| Some(exit) > alt_enter),
		"alternate buffer was not restored; enter={alt_enter:?} exit={alt_exit:?}\n{diagnostics}"
	);
	for sequence in ["\x1b[?1047h", "\x1b[?47h"] {
		assert!(
			!raw
				.windows(sequence.len())
				.any(|window| window == sequence.as_bytes()),
			"legacy alternate-buffer entry {sequence:?} observed\n{diagnostics}"
		);
	}
	for mode in [1000, 1002, 1003, 1006] {
		let enable = format!("\x1b[?{mode}h");
		let disable = format!("\x1b[?{mode}l");
		let enabled = raw
			.windows(enable.len())
			.rposition(|window| window == enable.as_bytes());
		let disabled = raw
			.windows(disable.len())
			.rposition(|window| window == disable.as_bytes());
		assert!(
			enabled.is_none() || disabled.is_some_and(|exit| Some(exit) > enabled),
			"mouse tracking mode {mode} was not restored; enable={enabled:?} \
			 disable={disabled:?}\n{diagnostics}"
		);
	}
	let hide = raw.windows(6).rposition(|window| window == b"\x1b[?25l");
	let show = raw.windows(6).rposition(|window| window == b"\x1b[?25h");
	assert!(
		show.is_some() && hide.is_none_or(|hidden| show > Some(hidden)),
		"cursor was not restored; hide={hide:?} show={show:?}\n{diagnostics}"
	);
	assert_eq!(after.input_flags, before.input_flags, "input flags not restored\n{diagnostics}");
	assert_eq!(after.output_flags, before.output_flags, "output flags not restored\n{diagnostics}");
	assert_eq!(
		after.control_flags, before.control_flags,
		"control flags not restored\n{diagnostics}"
	);
	assert_eq!(after.local_flags, before.local_flags, "local flags not restored\n{diagnostics}");
	assert_eq!(
		after.control_chars, before.control_chars,
		"control characters not restored\n{diagnostics}"
	);
	assert_eq!(
		cfgetispeed(after),
		cfgetispeed(before),
		"input baud rate not restored\n{diagnostics}"
	);
	assert_eq!(
		cfgetospeed(after),
		cfgetospeed(before),
		"output baud rate not restored\n{diagnostics}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_tui_drives_real_pty_tools_interrupt_resize_and_clean_quit() {
	use std::os::unix::fs::PermissionsExt;
	omp_e2e::support::install_omp_binary_env().expect("install Cargo-built omp binary");
	let scratch = tempfile::tempdir().expect("scratch root");
	fs::set_permissions(scratch.path(), <fs::Permissions>::from_mode(0o700))
		.expect("secure scratch root");
	let project = scratch.path().join("project");
	fs::create_dir(&project).expect("project directory");
	// The product canonicalizes `--project`; macOS tempdirs live behind the
	// `/var` symlink, so the fixture must hash the same canonical root.
	let project = fs::canonicalize(&project).expect("canonical project root");
	fs::write(project.join("scratch.txt"), "old\n").expect("write read/edit fixture");
	let metadata_dir = project.join(".omp");
	fs::create_dir(&metadata_dir).expect("project metadata directory");
	fs::set_permissions(&metadata_dir, <fs::Permissions>::from_mode(0o755))
		.expect("use standard project metadata permissions");
	let data_dir = project.parent().expect("project parent").join("home/data");
	let state_dir =
		omp_env::project_state::directory(&data_dir, &project).expect("project state directory");
	fs::create_dir_all(&state_dir).expect("create project state directory");
	let shell_release = scratch.path().join("release-shell");
	let gateway_socket = scratch.path().join("gateway.sock");
	let debug_socket = scratch.path().join("tui-debug.sock");
	let gateway = ScriptedGateway::start(scratch.path(), &gateway_socket, &shell_release).await;
	gateway.release(0);

	let binary = omp_e2e::support::omp_binary().expect("locate omp binary");
	let base_args = vec![
		"chat".to_owned(),
		"--model".to_owned(),
		gateway.model.clone(),
		"--project".to_owned(),
		project.display().to_string(),
		"--gateway".to_owned(),
		gateway_socket.display().to_string(),
		"--envd-idle-timeout".to_owned(),
		"2".to_owned(),
	];
	let initial_session_id = "01ARZ3NDEKTSV4RRFFQ69G5FB1".to_owned();
	seed_session(&state_dir, &project, &initial_session_id);

	let mut args = base_args.clone();
	args.extend(["--resume".to_owned(), initial_session_id.clone()]);
	let mut process = PtyChild::spawn(&binary, &args, &project, &debug_socket);
	let raw_capture = process.raw.clone();
	let mut debug =
		DebugClient::connect(&debug_socket, Instant::now() + READY_TIMEOUT, &mut process);
	let ready = wait_snapshot(&mut debug, &raw_capture, "chat shell ready", |snapshot| {
		snapshot.text.contains("session") && snapshot.text.contains("idle")
	});
	assert_surface(&ready, "ready");

	debug.keys("'exercise deterministic tools' enter");
	gateway.release(1);
	gateway.await_preview().await;
	let preview = wait_snapshot(&mut debug, &raw_capture, "edit preview", |snapshot| {
		let surface = snapshot.combined();
		surface.contains("read scratch.txt · Lines 1 · Size 24B")
			&& surface.contains("edit")
			&& !surface.contains("edit · scratch.txt")
			&& surface.contains("scratch.txt +1 -1 1 op")
			&& surface.contains("+ new")
			&& surface.contains("streaming arguments")
			&& fs::read_to_string(project.join("scratch.txt")).is_ok_and(|text| text == "old\n")
	});
	assert_surface(&preview, "edit preview");
	gateway.release_preview();
	let final_edit = wait_snapshot(&mut debug, &raw_capture, "edit final", |snapshot| {
		let surface = snapshot.combined();
		surface.contains("edit")
			&& !surface.contains("edit@hl.1")
			&& !surface.contains("edit · scratch.txt")
			&& surface.contains("scratch.txt +1 -1 2 ops")
			&& surface.contains("- 1|old")
			&& surface.contains("+ 1|new")
			&& fs::read_to_string(project.join("scratch.txt")).is_ok_and(|text| text == "new\n")
	});
	assert_surface(&final_edit, "edit final");

	gateway.release(2);
	let shell_live = wait_snapshot(&mut debug, &raw_capture, "shell live tail", |snapshot| {
		let surface = snapshot.combined();
		surface.contains("bash")
			&& !surface.contains("bash bash ·")
			&& !surface.contains("bash@1")
			&& surface.contains("$ printf")
			&& surface.contains("10B")
			&& surface.contains("live-tail")
	});
	assert_surface(&shell_live, "shell live");
	assert!(
		shell_live
			.frame
			.contains("read scratch.txt · Lines 1 · Size 24B")
			&& shell_live.frame.contains("edit")
			&& !shell_live.frame.contains("edit@hl.1")
			&& !shell_live.frame.contains("edit · scratch.txt"),
		"prior transcript vanished during shell stream: {}",
		shell_live.frame
	);
	fs::write(&shell_release, b"release").expect("release shell fixture");
	let shell_final = wait_snapshot(&mut debug, &raw_capture, "shell exit badge", |snapshot| {
		let surface = snapshot.combined();
		surface.contains("bash")
			&& !surface.contains("bash@1")
			&& !surface.contains("bash ·")
			&& surface.contains("$ printf")
			&& surface.contains("live-error")
			&& surface.contains("Exit 7")
	});
	assert_surface(&shell_final, "shell final");

	gateway.release(3);
	let unknown = wait_snapshot(&mut debug, &raw_capture, "unknown generic card", |snapshot| {
		let surface = snapshot.combined();
		surface.contains("think")
			&& !surface.contains("think@1")
			&& !surface.contains("think ·")
			&& surface.contains("P7 generic card proof")
	});
	assert_surface(&unknown, "unknown");
	gateway.release(4);
	let summary =
		wait_snapshot(&mut debug, &raw_capture, "first turn metrics complete", |snapshot| {
			snapshot
				.frame
				.contains("deterministic tool sequence is complete")
				&& snapshot.frame.contains("context  4096")
				&& snapshot.frame.contains("cost     $1.50")
		});
	assert_surface(&summary, "summary");

	gateway.release(5);
	let batch_one_marker = scratch.path().join("p7-b1-side-effect");
	let batch_two_marker = scratch.path().join("p7-b2-side-effect");
	let batch_three_marker = scratch.path().join("p7-b3-side-effect");
	debug.keys("'interrupt' shift-enter 'the batch'");
	let multiline =
		wait_snapshot(&mut debug, &raw_capture, "Shift+Enter multiline input", |snapshot| {
			snapshot.text.contains("interrupt") && snapshot.text.contains("the batch")
		});
	assert_surface(&multiline, "Shift+Enter multiline input");
	debug.keys("enter");
	let batch_live = wait_snapshot(&mut debug, &raw_capture, "batch running", |snapshot| {
		snapshot.combined().contains("bash")
			&& !snapshot.combined().contains("bash bash ·")
			&& !snapshot.combined().contains("bash@1")
			&& snapshot.combined().contains("18B")
			&& gateway.captured_text(5, "interrupt\nthe batch")
			&& batch_one_marker.is_file()
	});
	assert_surface(&batch_live, "batch live");

	process.resize(32, 92);
	debug
		.op("resize")
		.unwrap_or_else(|error| panic!("resize injection failed: {error}"));
	let resized = wait_snapshot(&mut debug, &raw_capture, "streaming resize", |snapshot| {
		snapshot.frame.contains("bash")
			&& !snapshot.frame.contains("bash bash ·")
			&& !snapshot.frame.contains("bash@1")
			&& snapshot.frame.contains("18B")
			&& snapshot.frame.contains("Working")
	});
	assert_surface(&resized, "resized");
	let info = wait_info(&mut debug, "settled streaming resize", |info| {
		info.get("rows").and_then(Value::as_u64) == Some(32)
			&& info.get("cols").and_then(Value::as_u64) == Some(92)
			&& info.get("alt_screen").and_then(Value::as_bool) == Some(false)
	});
	assert_eq!(info.get("rows").and_then(Value::as_u64), Some(32), "resize rows: {info}");
	assert_eq!(info.get("cols").and_then(Value::as_u64), Some(92), "resize cols: {info}");
	assert_eq!(
		info.get("alt_screen").and_then(Value::as_bool),
		Some(false),
		"chat entered alt screen: {info}"
	);

	debug.keys("esc");
	let interrupted = wait_snapshot(&mut debug, &raw_capture, "batch interrupted", |snapshot| {
		let surface = snapshot.combined();
		surface.contains("\"kind\":\"aborted\"")
			&& surface.contains("\"kind\":\"interrupted\"")
			&& !snapshot.frame.contains("Working")
	});
	assert_surface(&interrupted, "interrupt");
	assert!(batch_two_marker.exists(), "concurrent batch-2 side-effect marker missing");
	assert!(batch_three_marker.exists(), "concurrent batch-3 side-effect marker missing");

	debug.keys("'/new' enter");
	drop(debug);
	thread::sleep(Duration::from_millis(100));
	let mut debug =
		DebugClient::connect(&debug_socket, Instant::now() + READY_TIMEOUT, &mut process);
	let fresh_host_deadline = Instant::now() + READY_TIMEOUT;
	while debug.op("slots").is_err() {
		assert!(Instant::now() < fresh_host_deadline, "fresh chat host did not attach");
		thread::sleep(Duration::from_millis(15));
	}
	let fresh = wait_snapshot(&mut debug, &raw_capture, "fresh session ready", |snapshot| {
		snapshot.frame.contains("DeepSeek V4 Flash")
			&& !snapshot.frame.contains("Working")
			&& !snapshot.frame.contains("\"kind\":\"interrupted\"")
	});
	assert_surface(&fresh, "fresh session");
	debug.keys("'continue' enter");
	gateway.release(6);
	let queue_marker = scratch.path().join("p7-queue-side-effect");
	let queue_live =
		wait_snapshot(&mut debug, &raw_capture, "next batch after interrupt", |snapshot| {
			snapshot.frame.contains("bash")
				&& !snapshot.frame.contains("bash bash ·")
				&& !snapshot.frame.contains("bash@1")
				&& snapshot.frame.contains("17B")
				&& queue_marker.is_file()
				&& gateway.captured_text(6, "continue")
				&& !gateway.captured_text(6, "steer now")
				&& !gateway.captured_text(6, "after all work")
		});
	assert_surface(&queue_live, "next batch after interrupt");
	debug.keys("'steer now' enter");
	let immediate_steering =
		wait_snapshot(&mut debug, &raw_capture, "plain Enter immediate steering", |snapshot| {
			snapshot.frame.contains("steer now")
		});
	assert_surface(&immediate_steering, "plain Enter immediate steering");
	debug.keys("'after all work' alt-enter");
	let queued_follow_up =
		wait_snapshot(&mut debug, &raw_capture, "Alt+Enter queued follow-up", |snapshot| {
			snapshot.frame.contains("after all work")
		});
	assert_surface(&queued_follow_up, "Alt+Enter queued follow-up");
	gateway.release(7);
	let entered = wait_snapshot(
		&mut debug,
		&raw_capture,
		"plain Enter steering precedes follow-up",
		|snapshot| {
			snapshot.frame.contains("plain Enter steering ran before")
				&& gateway.captured_text(7, "steer now")
				&& !gateway.captured_text(7, "after all work")
		},
	);
	assert_surface(&entered, "plain Enter steering precedes follow-up");
	gateway.release(8);
	let follow_up =
		wait_snapshot(&mut debug, &raw_capture, "Alt+Enter follows all active work", |snapshot| {
			snapshot
				.frame
				.contains("queued follow-up ran after all prior work")
				&& gateway.captured_text(8, "after all work")
		});
	assert_surface(&follow_up, "Alt+Enter follows all active work");

	debug.keys("'/quit' enter");
	drop(debug);
	let before = process.before.clone();
	let (status, raw, stdout, stderr, after) = process.wait(READY_TIMEOUT);
	let diagnostics = format!(
		"status={status}\nstdout={stdout}\nstderr={stderr}\nlast frame={}\nraw={}",
		follow_up.frame,
		visible(&raw),
	);
	assert!(status.success(), "omp chat did not exit cleanly\n{diagnostics}");
	assert_restored(&raw, &before, &after, &diagnostics);
	let journals: Vec<_> = fs::read_dir(state_dir.join("sessions"))
		.expect("read session directory")
		.map(|entry| entry.expect("read session entry").path())
		.filter(|path| {
			path
				.extension()
				.is_some_and(|extension| extension == "jsonl")
		})
		.collect();
	assert_eq!(journals.len(), 2, "expected original and steering journals: {journals:?}");
	assert!(
		journals.iter().any(|path| {
			path.file_stem().and_then(ffi::OsStr::to_str) == Some(initial_session_id.as_str())
		}),
		"original bootstrap journal missing: {journals:?}"
	);
	let steering_session_id = journals
		.iter()
		.filter_map(|path| path.file_stem().and_then(ffi::OsStr::to_str))
		.find(|id| *id != initial_session_id.as_str())
		.expect("steering journal has a distinct ULID")
		.to_owned();
	let resume_debug_socket = scratch.path().join("resume-tui-debug.sock");
	let mut resumed = PtyChild::spawn(&binary, &base_args, &project, &resume_debug_socket);
	let resumed_raw = resumed.raw.clone();
	let mut resume_debug =
		DebugClient::connect(&resume_debug_socket, Instant::now() + READY_TIMEOUT, &mut resumed);
	let fresh = wait_snapshot(&mut resume_debug, &resumed_raw, "fresh second session", |snapshot| {
		let frame = &snapshot.frame;
		frame.contains("session")
			&& frame.contains("idle")
			&& !frame.contains("exercise deterministic tools")
	});
	assert_surface(&fresh, "fresh second session");

	gateway.release(9);
	resume_debug.keys("'second-session retained content' enter");
	let second_session = wait_snapshot(
		&mut resume_debug,
		&resumed_raw,
		"second session retained content",
		|snapshot| {
			snapshot.frame.contains("second-session retained content")
				&& snapshot.frame.contains("Second session retained content.")
		},
	);
	assert_surface(&second_session, "second session retained content");
	let second_idle =
		wait_snapshot(&mut resume_debug, &resumed_raw, "second session idle", |snapshot| {
			snapshot.frame.contains("Second session retained content.")
				&& snapshot.frame.contains("state    idle")
		});
	assert_surface(&second_idle, "second session idle");

	resume_debug.keys("'/resume'");
	let resume_draft =
		wait_snapshot(&mut resume_debug, &resumed_raw, "resume command draft", |snapshot| {
			snapshot.frame.contains("╰─ /resume")
		});
	assert_surface(&resume_draft, "resume command draft");
	resume_debug.keys("enter enter");
	let picker =
		wait_snapshot(&mut resume_debug, &resumed_raw, "resume session picker", |snapshot| {
			snapshot.text.contains("Resume session")
		});
	assert_surface(&picker, "resume session picker");
	resume_debug.keys("esc");
	let direct_resume = format!("'/resume {steering_session_id}'");
	resume_debug.keys(direct_resume.as_str());
	resume_debug.keys("enter enter");
	let direct_picker =
		wait_snapshot(&mut resume_debug, &resumed_raw, "selected resume session", |snapshot| {
			snapshot.text.contains("Resume session")
				&& snapshot.text.contains(steering_session_id.as_str())
		});
	assert_surface(&direct_picker, "selected resume session");
	resume_debug.keys("enter");
	drop(resume_debug);
	thread::sleep(Duration::from_millis(100));
	let mut resume_debug =
		DebugClient::connect(&resume_debug_socket, Instant::now() + READY_TIMEOUT, &mut resumed);
	let rehydrated = wait_snapshot(
		&mut resume_debug,
		&resumed_raw,
		"same-process transcript rehydrated",
		|snapshot| {
			let all = snapshot.combined();
			all.contains("continue")
				&& all.contains("\"kind\":\"interrupted\"")
				&& all.contains("steer now")
				&& all.contains("The queued follow-up ran after all prior work.")
				&& !snapshot.frame.contains("second-session retained content")
				&& !snapshot.frame.contains("Second session retained content.")
		},
	);
	assert_surface(&rehydrated, "same-process resumed transcript");
	resume_debug.keys("'/quit' enter enter");
	drop(resume_debug);
	let resumed_before = resumed.before.clone();
	let (resumed_status, resumed_bytes, resumed_stdout, resumed_stderr, resumed_after) =
		resumed.wait(READY_TIMEOUT);
	let resumed_diagnostics = format!(
		"status={resumed_status}\nstdout={resumed_stdout}\nstderr={resumed_stderr}\nrehydrated \
		 frame={}\nraw={}",
		rehydrated.frame,
		visible(&resumed_bytes),
	);
	assert!(
		resumed_status.success(),
		"resumed omp chat did not exit cleanly\n{resumed_diagnostics}"
	);
	assert_restored(&resumed_bytes, &resumed_before, &resumed_after, &resumed_diagnostics);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_tui_persists_thinking_blocks_across_turns_and_resume() {
	use std::os::unix::fs::PermissionsExt;
	omp_e2e::support::install_omp_binary_env().expect("install Cargo-built omp binary");
	let scratch = tempfile::tempdir().expect("scratch root");
	fs::set_permissions(scratch.path(), <fs::Permissions>::from_mode(0o700))
		.expect("secure scratch root");
	let project = scratch.path().join("project");
	fs::create_dir(&project).expect("project directory");
	let project = fs::canonicalize(&project).expect("canonical project root");
	let metadata_dir = project.join(".omp");
	fs::create_dir(&metadata_dir).expect("project metadata directory");
	fs::set_permissions(&metadata_dir, <fs::Permissions>::from_mode(0o755))
		.expect("use standard project metadata permissions");
	let data_dir = project.parent().expect("project parent").join("home/data");
	let state_dir =
		omp_env::project_state::directory(&data_dir, &project).expect("project state directory");
	fs::create_dir_all(&state_dir).expect("create project state directory");
	let gateway_socket = scratch.path().join("gateway.sock");
	let debug_socket = scratch.path().join("tui-debug.sock");
	let gateway = ScriptedGateway::start_with_scripts(scratch.path(), &gateway_socket, vec![
		thinking_text_script(
			"Weighing the first request.\nThe deterministic option is safest.",
			"First answer settled.",
		),
		thinking_text_script("Second deliberation paragraph.", "Second answer settled."),
	])
	.await;
	gateway.release(0);
	gateway.release(1);

	let session_id = "01ARZ3NDEKTSV4RRFFQ69G5FB2".to_owned();
	seed_session(&state_dir, &project, &session_id);
	let binary = omp_e2e::support::omp_binary().expect("locate omp binary");
	let base_args = vec![
		"chat".to_owned(),
		"--model".to_owned(),
		gateway.model.clone(),
		"--project".to_owned(),
		project.display().to_string(),
		"--gateway".to_owned(),
		gateway_socket.display().to_string(),
		"--envd-idle-timeout".to_owned(),
		"2".to_owned(),
	];
	let mut args = base_args.clone();
	args.extend(["--resume".to_owned(), session_id.clone()]);
	let mut process = PtyChild::spawn(&binary, &args, &project, &debug_socket);
	let raw_capture = process.raw.clone();
	let mut debug =
		DebugClient::connect(&debug_socket, Instant::now() + READY_TIMEOUT, &mut process);
	let ready = wait_snapshot(&mut debug, &raw_capture, "chat shell ready", |snapshot| {
		snapshot.text.contains("session") && snapshot.text.contains("idle")
	});
	assert_surface(&ready, "ready");

	debug.keys("'first prompt' enter");
	let first = wait_snapshot(&mut debug, &raw_capture, "first turn keeps thinking", |snapshot| {
		snapshot.frame.contains("First answer settled.")
			&& snapshot.frame.contains("Weighing the first request")
	});
	assert_surface(&first, "first turn");

	// Ctrl+T is a scene-wide visibility toggle: it hides every unretired
	// thinking block and applies to future ones until toggled back.
	debug.keys("ctrl+t");
	let hidden = wait_snapshot(&mut debug, &raw_capture, "ctrl+t hides thinking", |snapshot| {
		snapshot.frame.contains("First answer settled.")
			&& !snapshot.frame.contains("Weighing the first request")
	});
	assert_surface(&hidden, "hidden thinking");
	debug.keys("ctrl+t");
	wait_snapshot(&mut debug, &raw_capture, "ctrl+t restores thinking", |snapshot| {
		snapshot.frame.contains("Weighing the first request")
	});

	debug.keys("'second prompt' enter");
	let second = wait_snapshot(&mut debug, &raw_capture, "second turn keeps history", |snapshot| {
		snapshot.frame.contains("Second answer settled.")
			&& snapshot.frame.contains("Second deliberation paragraph.")
	});
	assert_surface(&second, "second turn");
	assert!(
		second.frame.contains("First answer settled."),
		"prior transcript vanished during the second turn: {}",
		second.frame
	);

	debug.keys("'/quit' enter");
	drop(debug);
	let (status, raw, stdout, stderr, _) = process.wait(READY_TIMEOUT);
	assert!(
		status.success(),
		"omp chat did not exit cleanly\nstatus={status}\nstdout={stdout}\nstderr={stderr}\nraw={}",
		visible(&raw)
	);

	// Durable thinking parts replay with their bodies visible.
	let resume_socket = scratch.path().join("resume-tui-debug.sock");
	let mut args = base_args.clone();
	args.extend(["--resume".to_owned(), session_id.clone()]);
	let mut resumed = PtyChild::spawn(&binary, &args, &project, &resume_socket);
	let resumed_raw = resumed.raw.clone();
	let mut resume_debug =
		DebugClient::connect(&resume_socket, Instant::now() + READY_TIMEOUT, &mut resumed);
	let rehydrated = wait_snapshot(
		&mut resume_debug,
		&resumed_raw,
		"resumed transcript keeps thinking bodies",
		|snapshot| {
			let all = snapshot.combined();
			all.contains("First answer settled.")
				&& all.contains("Second answer settled.")
				&& all.contains("Weighing the first request")
		},
	);
	assert!(
		rehydrated
			.combined()
			.contains("Second deliberation paragraph."),
		"replayed thinking dropped a body: {}",
		rehydrated.combined()
	);
	resume_debug.keys("'/quit' enter enter");
	drop(resume_debug);
	let (resumed_status, resumed_bytes, resumed_stdout, resumed_stderr, _) =
		resumed.wait(READY_TIMEOUT);
	assert!(
		resumed_status.success(),
		"resumed omp chat did not exit \
		 cleanly\nstatus={resumed_status}\nstdout={resumed_stdout}\nstderr={resumed_stderr}\nraw={}",
		visible(&resumed_bytes)
	);
}
