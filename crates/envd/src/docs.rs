//! Document-server connection and revision-pinned document operations.

use std::{
	collections::HashMap,
	fmt,
	future::Future,
	mem,
	path::Path,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use bytes::{Bytes, BytesMut};
use omp_core::{Str, sf};
use omp_docserver::{
	client::{TerminalEventReceiver, terminal_event_channel},
	connection::{PROTOCOL_MAJOR, PROTOCOL_MINOR},
	wire::{self, FrameConfig},
};
use omp_hashline::{Clipboard, NoopLoopGuard, SnapshotStore};
use omp_proto::document::v1::{
	self as pb, client_frame, commit_transaction_response, document_target, server_frame,
};
use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::{
	io,
	io::{AsyncRead, AsyncWrite},
	net::UnixStream,
};
use tokio_util::sync::CancellationToken;

use super::{ssh::SshService, vault::VaultService};
/// Editor-client document authority installed for an ACP session.
///
/// The boxed futures are confined to this cold dynamic RPC boundary; ordinary
/// document and tool calls remain statically dispatched.
pub trait AcpDocumentBackend: Send + Sync {
	/// Reads the editor's exact current UTF-8 buffer for an absolute path.
	fn read_text(
		&self,
		absolute_path: Str,
	) -> Pin<Box<dyn Future<Output = miette::Result<Str>> + Send + '_>>;

	/// Writes the editor buffer and returns its authoritative read-back after
	/// any client format-on-save hook.
	fn write_text(
		&self,
		absolute_path: Str,
		content: Str,
	) -> Pin<Box<dyn Future<Output = miette::Result<Str>> + Send + '_>>;
}

/// Late-bound ACP document capability shared by every tool adapter using one
/// document connection.
#[derive(Clone, Default)]
pub(crate) struct AcpDocumentSlot(Arc<RwLock<Option<Arc<dyn AcpDocumentBackend>>>>);

impl fmt::Debug for AcpDocumentSlot {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("AcpDocumentSlot")
			.field("bound", &self.0.read().is_some())
			.finish()
	}
}

impl AcpDocumentSlot {
	/// Replaces the active editor-client authority.
	pub(crate) fn bind(&self, backend: Option<Arc<dyn AcpDocumentBackend>>) {
		*self.0.write() = backend;
	}

	fn backend(&self) -> Option<Arc<dyn AcpDocumentBackend>> {
		self.0.read().clone()
	}
}

/// Metadata established by the document protocol hello exchange.
#[derive(Clone, Debug)]
pub struct DocumentHello {
	/// Negotiated protocol major version.
	pub protocol_major: u32,
	/// Negotiated protocol minor version.
	pub protocol_minor: u32,
	/// Stable identity of the connected document workspace.
	pub workspace_id:   Bytes,
	/// Canonical file URI of the connected workspace root.
	pub root_uri:       Str,
	/// Epoch scoping transaction idempotency keys.
	pub server_epoch:   Bytes,
	/// Executable-generation identity of the serving document authority.
	pub server_build:   Str,
}
/// A terminal loss of continuity in a document-server event stream.
#[derive(Clone, Debug, Error)]
#[error("document event stream ended ({failure:?}); skipped {skipped_events} events: {message}")]
pub struct EventStreamError {
	/// Stream family whose continuity was lost.
	pub stream:         pb::EventStreamKind,
	/// Terminal failure classification.
	pub failure:        pb::EventStreamFailure,
	/// Number of events overwritten before a lag failure.
	pub skipped_events: u64,
	/// Server-provided diagnostic.
	pub message:        Str,
}

/// One ordered DAP output or lifecycle event.
#[derive(Clone, Debug)]
pub enum DapRegistryEvent {
	/// Bounded adapter or debuggee output.
	Output(pb::DapOutput),
	/// Bounded adapter lifecycle or debugger event.
	Event(pb::DapEvent),
}

/// One connection-wide LSP registry event.
#[derive(Clone, Debug)]
pub enum LspRegistryEvent {
	/// Notification emitted by a bound language server.
	Event(pb::LspEvent),
	/// Binding lifecycle or synchronization-policy change.
	Binding(pb::LspBindingEvent),
}

/// The terminally contiguous event stream attached to an open document lease.
#[derive(Debug)]
pub struct DocumentEvents {
	receiver: TerminalEventReceiver<pb::DocumentEvent, EventStreamError>,
}

impl DocumentEvents {
	/// Waits for the next event, returning the terminal continuity error once.
	pub async fn next_event(&self) -> Result<pb::DocumentEvent, EventStreamError> {
		self
			.receiver
			.next_event()
			.await
			.unwrap_or_else(|| Err(closed_stream_error(pb::EventStreamKind::Document)))
	}
}

/// The terminally contiguous connection-wide LSP event stream.
#[derive(Debug)]
pub struct LspEvents {
	receiver: TerminalEventReceiver<LspRegistryEvent, EventStreamError>,
}

impl LspEvents {
	/// Waits for the next LSP or binding event.
	pub async fn next_event(&self) -> Result<LspRegistryEvent, EventStreamError> {
		self
			.receiver
			.next_event()
			.await
			.unwrap_or_else(|| Err(closed_stream_error(pb::EventStreamKind::LspRegistry)))
	}
}

type DocumentEventResult = Result<pb::DocumentEvent, EventStreamError>;
type DocumentEventSender = flume::Sender<DocumentEventResult>;
type DocumentEventSubscribers = HashMap<Bytes, (Bytes, DocumentEventSender)>;
type PendingDocumentEvents = HashMap<Bytes, Vec<DocumentEventResult>>;
type PendingDapEvents = HashMap<Bytes, Vec<DapRegistryEvent>>;

/// A document-server lease pinned to the revision returned by `OpenDocument`.
///
/// Dropping the lease sends a best-effort close request, keeping lease release
/// resource-owned even when an executor future is cancelled.
#[derive(Debug)]
#[must_use]
pub struct DocumentLease {
	lease_id: Bytes,
	head:     pb::DocumentHead,
	host:     Arc<Inner>,
	events:   Option<DocumentEvents>,
	released: bool,
}

impl DocumentLease {
	/// Returns the opaque connection-owned lease identity.
	pub const fn id(&self) -> &Bytes {
		&self.lease_id
	}

	/// Returns the immutable head to which reads and edits are pinned.
	pub const fn head(&self) -> &pb::DocumentHead {
		&self.head
	}

	/// Takes the terminally contiguous event stream for this lease.
	///
	/// A lease has exactly one event consumer. Subsequent calls return `None`.
	pub const fn take_events(&mut self) -> Option<DocumentEvents> {
		self.events.take()
	}

	/// Advances this lease to a committed head returned for the same document.
	pub(crate) fn advance(&mut self, head: pb::DocumentHead) -> Result<(), DocumentError> {
		if head.revision.is_none() || head.document != self.head.document {
			return Err(unexpected("committed head for the leased document"));
		}
		self.head = head;
		Ok(())
	}

	fn revision(&self) -> Result<pb::Revision, DocumentError> {
		self
			.head
			.revision
			.clone()
			.ok_or(DocumentError::MalformedResponse(sf!("document head omitted its revision",)))
	}
}
/// Connection-owned exclusive workspace reservation.
#[derive(Debug)]
#[must_use]
pub struct WorkspaceLease {
	lease_id: Bytes,
	host:     Arc<Inner>,
	released: bool,
}

impl WorkspaceLease {
	/// Returns the opaque reservation identity.
	pub const fn id(&self) -> &Bytes {
		&self.lease_id
	}
}

impl Drop for WorkspaceLease {
	fn drop(&mut self) {
		if self.released || self.host.shutdown.is_cancelled() {
			return;
		}
		let request_id = self.host.next_request.fetch_add(1, Ordering::Relaxed);
		if request_id == 0 {
			return;
		}
		let _ = self.host.writer.try_send(pb::ClientFrame {
			request_id,
			body: Some(client_frame::Body::ReleaseWorkspaceLease(pb::ReleaseWorkspaceLeaseRequest {
				workspace_lease_id: self.lease_id.clone(),
			})),
		});
	}
}

impl Drop for DocumentLease {
	fn drop(&mut self) {
		self.host.document_events.lock().remove(&self.lease_id);
		if self.released || self.host.shutdown.is_cancelled() {
			return;
		}
		let request_id = self.host.next_request.fetch_add(1, Ordering::Relaxed);
		if request_id == 0 {
			return;
		}
		let _ = self.host.writer.try_send(pb::ClientFrame {
			request_id,
			body: Some(client_frame::Body::CloseDocument(pb::CloseDocumentRequest {
				lease_id: self.lease_id.clone(),
			})),
		});
	}
}

/// A document host connection, protocol, or server operation failed.
#[derive(Debug, Error)]
pub enum DocumentError {
	/// Transport framing or serialization error.
	#[error(transparent)]
	Wire(#[from] wire::WireError),
	/// Server connection was closed unexpectedly.
	#[error("document-server connection closed")]
	Disconnected,
	/// Document operation was cancelled before completion.
	#[error("document operation was cancelled")]
	Cancelled,
	/// Document server rejected the operation.
	#[error("document server rejected the operation ({code}): {message}")]
	Protocol {
		/// Server status code.
		code:    i32,
		/// Server error message.
		message: Str,
	},
	/// Server response frame was invalid or unexpected.
	#[error("malformed document-server response: {0}")]
	MalformedResponse(Str),
}

#[derive(Debug)]
struct Inner {
	hello:                    DocumentHello,
	resource_mutations:       RwLock<Option<ResourceMutationServices>>,
	acp_documents:            AcpDocumentSlot,
	writer:                   flume::Sender<pb::ClientFrame>,
	pending:                  Arc<Mutex<HashMap<u64, flume::Sender<pb::ServerFrame>>>>,
	document_events:          Arc<Mutex<DocumentEventSubscribers>>,
	pending_document_events:  Arc<Mutex<PendingDocumentEvents>>,
	pending_dap_events:       Arc<Mutex<PendingDapEvents>>,
	document_event_sequences: Arc<Mutex<HashMap<Bytes, u64>>>,
	lsp_event_sender:         flume::Sender<Result<LspRegistryEvent, EventStreamError>>,
	lsp_events:               Mutex<Option<LspEvents>>,
	next_request:             AtomicU64,
	shutdown:                 CancellationToken,
	snapshot_store:           Mutex<SnapshotStore>,
	clipboard:                Mutex<Clipboard>,
	noop_loop_guard:          Mutex<NoopLoopGuard>,
}

/// App-owned SSH and vault authorities used by document resource writes.
#[derive(Clone, Debug)]
pub(super) struct ResourceMutationServices {
	pub(super) ssh:   SshService,
	pub(super) vault: VaultService,
}

/// Client connection to the project document server.
#[derive(Clone, Debug)]
pub struct DocumentHost {
	inner: Arc<Inner>,
}

impl DocumentHost {
	/// Binds or clears the editor-owned document authority.
	pub(crate) fn bind_acp_documents(&self, backend: Option<Arc<dyn AcpDocumentBackend>>) {
		self.inner.acp_documents.bind(backend);
	}

	/// Reads the current editor buffer when an ACP document authority is live.
	pub(crate) async fn read_acp_text(&self, absolute_path: Str) -> Option<miette::Result<Str>> {
		let backend = self.inner.acp_documents.backend()?;
		Some(backend.read_text(absolute_path).await)
	}

	/// Writes through the current editor and returns its formatted read-back.
	pub(crate) async fn write_acp_text(
		&self,
		absolute_path: Str,
		content: Str,
	) -> Option<miette::Result<Str>> {
		let backend = self.inner.acp_documents.backend()?;
		Some(backend.write_text(absolute_path, content).await)
	}

	/// Installs the app-owned capability-checked internal resource writers.
	pub(super) fn set_resource_mutations(&self, services: ResourceMutationServices) {
		*self.inner.resource_mutations.write() = Some(services);
	}

	pub(super) fn resource_mutations(&self) -> Option<ResourceMutationServices> {
		self.inner.resource_mutations.read().clone()
	}

	/// Connects to an already-running document server and completes its hello.
	pub async fn connect<S>(stream: S) -> Result<Self, DocumentError>
	where
		S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
	{
		let config = FrameConfig::default();
		let (mut reader, mut writer) = io::split(stream);
		let mut write_scratch = BytesMut::new();
		wire::write_client_frame(
			&mut writer,
			&pb::ClientFrame {
				request_id: 0,
				body:       Some(client_frame::Body::Hello(pb::ClientHello {
					protocol_major: PROTOCOL_MAJOR,
					protocol_minor: PROTOCOL_MINOR,
					client_id:      Bytes::new(),
				})),
			},
			config,
			&mut write_scratch,
		)
		.await?;

		let mut read_scratch = BytesMut::new();
		let hello_frame = wire::read_server_frame(&mut reader, config, &mut read_scratch)
			.await?
			.ok_or(DocumentError::Disconnected)?;
		let hello = match hello_frame.body {
			Some(server_frame::Body::Hello(hello)) if hello_frame.request_id == 0 => hello,
			Some(server_frame::Body::Error(error)) => {
				return Err(DocumentError::Protocol {
					code:    error.code,
					message: Str::from(error.message),
				});
			},
			_ => {
				return Err(DocumentError::MalformedResponse(sf!(
					"expected ServerHello as the first server frame",
				)));
			},
		};
		if hello.protocol_major != PROTOCOL_MAJOR || hello.protocol_minor > PROTOCOL_MINOR {
			return Err(DocumentError::MalformedResponse(sf!(
				"document server negotiated an unsupported protocol version",
			)));
		}
		let hello = DocumentHello {
			protocol_major: hello.protocol_major,
			protocol_minor: hello.protocol_minor,
			workspace_id:   hello.workspace_id,
			root_uri:       Str::from(hello.root_uri),
			server_epoch:   hello.server_epoch,
			server_build:   Str::from(hello.server_build),
		};

		let (write_tx, write_rx) = flume::unbounded();
		let (lsp_event_sender, lsp_event_receiver) = terminal_event_channel();
		let inner = Arc::new(Inner {
			hello,
			resource_mutations: RwLock::new(None),
			acp_documents: AcpDocumentSlot::default(),
			writer: write_tx,
			pending: Arc::new(Mutex::new(HashMap::new())),
			document_events: Arc::new(Mutex::new(HashMap::new())),
			pending_document_events: Arc::new(Mutex::new(HashMap::new())),
			pending_dap_events: Arc::new(Mutex::new(HashMap::new())),
			document_event_sequences: Arc::new(Mutex::new(HashMap::new())),
			lsp_event_sender,
			lsp_events: Mutex::new(Some(LspEvents { receiver: lsp_event_receiver })),
			next_request: AtomicU64::new(1),
			shutdown: CancellationToken::new(),
			snapshot_store: Mutex::new(SnapshotStore::default()),
			clipboard: Mutex::new(Clipboard::default()),
			noop_loop_guard: Mutex::new(NoopLoopGuard::default()),
		});

		let writer_shutdown = inner.shutdown.clone();
		tokio::spawn(async move {
			let mut scratch = write_scratch;
			while let Ok(frame) = write_rx.recv_async().await {
				if wire::write_client_frame(&mut writer, &frame, config, &mut scratch)
					.await
					.is_err()
				{
					break;
				}
			}
			writer_shutdown.cancel();
		});

		let reader_pending = Arc::clone(&inner.pending);
		let reader_document_events = Arc::clone(&inner.document_events);
		let reader_document_event_sequences = Arc::clone(&inner.document_event_sequences);
		let reader_pending_document_events = Arc::clone(&inner.pending_document_events);
		let reader_pending_dap_events = Arc::clone(&inner.pending_dap_events);
		let reader_lsp_events = inner.lsp_event_sender.clone();
		let reader_shutdown = inner.shutdown.clone();
		tokio::spawn(async move {
			loop {
				let frame = tokio::select! {
					() = reader_shutdown.cancelled() => break,
					result = wire::read_server_frame(&mut reader, config, &mut read_scratch) => {
						match result {
							Ok(Some(frame)) => frame,
							Ok(None) | Err(_) => break,
						}
					},
				};
				if frame.request_id == 0 {
					if let Some(body) = frame.body {
						dispatch_event_frame(
							body,
							&reader_document_events,
							&reader_pending_document_events,
							&reader_document_event_sequences,
							&reader_pending_dap_events,
							&reader_lsp_events,
						);
					}
					continue;
				}
				let waiter = reader_pending.lock().remove(&frame.request_id);
				if let Some(waiter) = waiter {
					let _ = waiter.send(frame);
				}
			}
			reader_shutdown.cancel();
			let waiters = mem::take(&mut *reader_pending.lock());
			for (request_id, waiter) in waiters {
				let _ = waiter.send(disconnected_frame(request_id));
			}
			let closed = closed_stream_error(pb::EventStreamKind::Document);
			let document_events = mem::take(&mut *reader_document_events.lock());
			for (_, sender) in document_events.into_values() {
				let _ = sender.send(Err(closed.clone()));
			}
			let _ = reader_lsp_events.send(Err(closed_stream_error(pb::EventStreamKind::LspRegistry)));
		});

		Ok(Self { inner })
	}

	/// Connects to an already-running document server over a Unix-domain socket.
	#[cfg(unix)]
	pub async fn connect_uds(path: impl AsRef<Path>) -> Result<Self, DocumentError> {
		Self::connect(
			UnixStream::connect(path)
				.await
				.map_err(wire::WireError::from)?,
		)
		.await
	}

	/// Returns the negotiated server and workspace identity.
	pub fn hello(&self) -> &DocumentHello {
		&self.inner.hello
	}

	/// Returns the session-shared hashline snapshot store.
	///
	/// Callers should hold the lock only for synchronous snapshot operations,
	/// never across an await point.
	pub(crate) fn snapshot_store(&self) -> &Mutex<SnapshotStore> {
		&self.inner.snapshot_store
	}

	/// Returns the session-shared hashline clipboard.
	///
	/// Named registers persist for the lifetime of this document connection.
	/// Callers should not hold the lock across an await point.
	pub(crate) fn clipboard(&self) -> &Mutex<Clipboard> {
		&self.inner.clipboard
	}

	/// Returns the session-shared repeated no-op edit guard.
	pub(crate) fn noop_loop_guard(&self) -> &Mutex<NoopLoopGuard> {
		&self.inner.noop_loop_guard
	}

	/// Takes the connection-wide LSP registry event stream.
	///
	/// A protocol connection has exactly one ordered LSP event consumer.
	pub fn take_lsp_events(&self) -> Option<LspEvents> {
		self.inner.lsp_events.lock().take()
	}

	/// Acquires a document lease and pins it to the returned immutable revision.
	pub async fn open(
		&self,
		uri: Str,
		language_id: Option<Str>,
		cancel: &CancellationToken,
	) -> Result<DocumentLease, DocumentError> {
		let (lease, _) = self
			.open_request(
				pb::OpenDocumentRequest {
					uri:         uri.into(),
					language_id: language_id.unwrap_or_default().into(),
				},
				cancel,
			)
			.await?;
		Ok(lease)
	}

	/// Forwards one canonical open request and returns both its owned lease and
	/// unmodified protocol response.
	pub(crate) async fn open_request(
		&self,
		request: pb::OpenDocumentRequest,
		cancel: &CancellationToken,
	) -> Result<(DocumentLease, pb::OpenDocumentResponse), DocumentError> {
		let body = self
			.request(client_frame::Body::OpenDocument(request), cancel)
			.await?;
		let server_frame::Body::DocumentOpened(opened) = body else {
			return Err(unexpected("OpenDocumentResponse"));
		};
		let head = opened
			.head
			.clone()
			.ok_or_else(|| unexpected("OpenDocumentResponse.head"))?;
		let document_id = head
			.document
			.as_ref()
			.map(|document| document.id.clone())
			.filter(|id| !id.is_empty())
			.ok_or_else(|| unexpected("OpenDocumentResponse.head.document.id"))?;
		if opened.lease_id.len() != 16 || head.revision.is_none() {
			return Err(unexpected("valid lease id and pinned revision"));
		}
		let (event_sender, event_receiver) = terminal_event_channel();
		self
			.inner
			.document_events
			.lock()
			.insert(opened.lease_id.clone(), (document_id.clone(), event_sender.clone()));
		let pending_events = self
			.inner
			.pending_document_events
			.lock()
			.remove(&document_id);
		if let Some(events) = pending_events {
			for event in events {
				let _ = event_sender.send(event);
			}
		}
		let pending_events = self
			.inner
			.pending_document_events
			.lock()
			.remove(&opened.lease_id);
		if let Some(events) = pending_events {
			for event in events {
				let _ = event_sender.send(event);
			}
		}
		let lease = DocumentLease {
			lease_id: opened.lease_id.clone(),
			head,
			host: Arc::clone(&self.inner),
			events: Some(DocumentEvents { receiver: event_receiver }),
			released: false,
		};
		Ok((lease, opened))
	}

	/// Reads ranges from the exact revision pinned by `lease`.
	pub async fn read(
		&self,
		lease: &DocumentLease,
		selection: pb::ReadSelection,
		cancel: &CancellationToken,
	) -> Result<pb::ReadDocumentResponse, DocumentError> {
		self
			.read_request(
				lease,
				pb::ReadDocumentRequest {
					document:  Some(lease_target(lease)),
					revision:  Some(lease.revision()?),
					selection: Some(selection),
				},
				cancel,
			)
			.await
	}

	/// Forwards one canonical read request after validating its connection-owned
	/// lease. The protocol permits omitting the revision to read the current
	/// head and permits an explicit retained revision.
	pub(crate) async fn read_request(
		&self,
		lease: &DocumentLease,
		request: pb::ReadDocumentRequest,
		cancel: &CancellationToken,
	) -> Result<pb::ReadDocumentResponse, DocumentError> {
		self.ensure_request_lease(lease, request.document.as_ref())?;
		if request.selection.is_none() {
			return Err(unexpected("ReadDocumentRequest.selection"));
		}
		let requested_revision = request.revision.clone();
		let body = self
			.request(client_frame::Body::ReadDocument(request), cancel)
			.await?;
		let server_frame::Body::DocumentRead(response) = body else {
			return Err(unexpected("ReadDocumentResponse"));
		};
		ensure_requested_head(response.head.as_ref(), requested_revision.as_ref())?;
		Ok(response)
	}

	/// Produces a structural summary from the exact revision pinned by `lease`.
	pub async fn summarize(
		&self,
		lease: &DocumentLease,
		options: pb::CodeSummaryOptions,
		cancel: &CancellationToken,
	) -> Result<pb::SummarizeDocumentResponse, DocumentError> {
		self
			.summarize_request(
				lease,
				pb::SummarizeDocumentRequest {
					document: Some(lease_target(lease)),
					revision: Some(lease.revision()?),
					options:  Some(options),
				},
				cancel,
			)
			.await
	}

	/// Forwards one canonical summary request after validating its
	/// connection-owned lease and optional requested revision.
	pub(crate) async fn summarize_request(
		&self,
		lease: &DocumentLease,
		request: pb::SummarizeDocumentRequest,
		cancel: &CancellationToken,
	) -> Result<pb::SummarizeDocumentResponse, DocumentError> {
		self.ensure_request_lease(lease, request.document.as_ref())?;
		if request.options.is_none() {
			return Err(unexpected("SummarizeDocumentRequest.options"));
		}
		let requested_revision = request.revision.clone();
		let body = self
			.request(client_frame::Body::SummarizeDocument(request), cancel)
			.await?;
		let server_frame::Body::DocumentSummarized(response) = body else {
			return Err(unexpected("SummarizeDocumentResponse"));
		};
		ensure_requested_head(response.head.as_ref(), requested_revision.as_ref())?;
		Ok(response)
	}

	/// Commits one text mutation against the lease's pinned base revision.
	///
	/// The lease advances only after a committed operation; rejected and partial
	/// outcomes retain the old pin so callers cannot accidentally write from an
	/// unobserved head.
	pub async fn commit(
		&self,
		lease: &mut DocumentLease,
		transaction_id: Bytes,
		mut mutation: pb::TextMutation,
		cancel: &CancellationToken,
	) -> Result<pb::CommitTransactionResponse, DocumentError> {
		self.ensure_owned(lease)?;
		mutation.base_revision = Some(lease.revision()?);
		let body = self
			.request(
				client_frame::Body::CommitTransaction(pb::CommitTransactionRequest {
					transaction_id,
					operations: vec![pb::DocumentMutation {
						document:  Some(lease_target(lease)),
						operation: Some(pb::document_mutation::Operation::Text(mutation)),
					}],
				}),
				cancel,
			)
			.await?;
		let server_frame::Body::TransactionResult(response) = body else {
			return Err(unexpected("CommitTransactionResponse"));
		};
		if let Some(commit_transaction_response::Outcome::Committed(committed)) = &response.outcome {
			let Some(head) = (committed.operations.len() == 1)
				.then(|| committed.operations[0].head.clone())
				.flatten()
			else {
				return Err(unexpected("one committed operation head"));
			};
			if head.revision.is_none() {
				return Err(unexpected("committed operation revision"));
			}
			lease.head = head;
		}
		Ok(response)
	}

	/// Commits several already revision-bound mutations as one document-server
	/// transaction. Operations are sent in declared order against the server's
	/// transaction-local overlay.
	pub async fn commit_transaction(
		&self,
		transaction_id: Bytes,
		operations: Vec<pb::DocumentMutation>,
		cancel: &CancellationToken,
	) -> Result<pb::CommitTransactionResponse, DocumentError> {
		self
			.commit_transaction_request(
				pb::CommitTransactionRequest { transaction_id, operations },
				cancel,
			)
			.await
	}

	/// Forwards one canonical document transaction request.
	pub(crate) async fn commit_transaction_request(
		&self,
		request: pb::CommitTransactionRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CommitTransactionResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::CommitTransaction(request), cancel)
			.await?;
		let server_frame::Body::TransactionResult(response) = body else {
			return Err(unexpected("CommitTransactionResponse"));
		};
		Ok(response)
	}

	/// Resolves an existing path to its host-canonical file URI.
	pub async fn canonicalize(
		&self,
		request: pb::CanonicalizePathRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CanonicalizePathResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::CanonicalizePath(request), cancel)
			.await?;
		let server_frame::Body::PathCanonicalized(response) = body else {
			return Err(unexpected("CanonicalizePathResponse"));
		};
		Ok(response)
	}

	/// Reads stat or lstat metadata through the document authority.
	pub async fn stat(
		&self,
		request: pb::StatPathRequest,
		cancel: &CancellationToken,
	) -> Result<pb::StatPathResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::StatPath(request), cancel)
			.await?;
		let server_frame::Body::PathStat(response) = body else {
			return Err(unexpected("StatPathResponse"));
		};
		Ok(response)
	}

	/// Enumerates one directory through the document authority.
	pub async fn list_directory(
		&self,
		request: pb::ListDirectoryRequest,
		cancel: &CancellationToken,
	) -> Result<pb::ListDirectoryResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::ListDirectory(request), cancel)
			.await?;
		let server_frame::Body::DirectoryListed(response) = body else {
			return Err(unexpected("ListDirectoryResponse"));
		};
		Ok(response)
	}

	/// Creates a directory through the document authority.
	pub async fn create_directory(
		&self,
		request: pb::CreateDirectoryRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CreateDirectoryResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::CreateDirectory(request), cancel)
			.await?;
		let server_frame::Body::DirectoryCreated(response) = body else {
			return Err(unexpected("CreateDirectoryResponse"));
		};
		Ok(response)
	}

	/// Removes a path under the authority's active-document revision checks.
	pub async fn remove(
		&self,
		request: pb::RemovePathRequest,
		cancel: &CancellationToken,
	) -> Result<pb::RemovePathResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::RemovePath(request), cancel)
			.await?;
		let server_frame::Body::PathRemoved(response) = body else {
			return Err(unexpected("RemovePathResponse"));
		};
		Ok(response)
	}

	/// Renames a path under exact source and destination revision checks.
	pub async fn rename(
		&self,
		request: pb::RenamePathRequest,
		cancel: &CancellationToken,
	) -> Result<pb::RenamePathResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::RenamePath(request), cancel)
			.await?;
		let server_frame::Body::PathRenamed(response) = body else {
			return Err(unexpected("RenamePathResponse"));
		};
		Ok(response)
	}

	/// Copies a regular file or symbolic link without bypassing the authority.
	pub async fn copy(
		&self,
		request: pb::CopyPathRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CopyPathResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::CopyPath(request), cancel)
			.await?;
		let server_frame::Body::PathCopied(response) = body else {
			return Err(unexpected("CopyPathResponse"));
		};
		Ok(response)
	}

	/// Reads a symbolic-link target without dereferencing the final entry.
	pub async fn read_link(
		&self,
		request: pb::ReadLinkRequest,
		cancel: &CancellationToken,
	) -> Result<pb::ReadLinkResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::ReadLink(request), cancel)
			.await?;
		let server_frame::Body::LinkRead(response) = body else {
			return Err(unexpected("ReadLinkResponse"));
		};
		Ok(response)
	}

	/// Creates a symbolic link through the document authority.
	pub async fn create_symlink(
		&self,
		request: pb::CreateSymlinkRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CreateSymlinkResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::CreateSymlink(request), cancel)
			.await?;
		let server_frame::Body::SymlinkCreated(response) = body else {
			return Err(unexpected("CreateSymlinkResponse"));
		};
		Ok(response)
	}

	/// Creates a hard link through the document authority.
	pub async fn create_hard_link(
		&self,
		request: pb::CreateHardLinkRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CreateHardLinkResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::CreateHardLink(request), cancel)
			.await?;
		let server_frame::Body::HardLinkCreated(response) = body else {
			return Err(unexpected("CreateHardLinkResponse"));
		};
		Ok(response)
	}

	/// Applies a portable permission transition under revision checks.
	pub async fn set_permissions(
		&self,
		request: pb::SetPermissionsRequest,
		cancel: &CancellationToken,
	) -> Result<pb::SetPermissionsResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::SetPermissions(request), cancel)
			.await?;
		let server_frame::Body::PermissionsSet(response) = body else {
			return Err(unexpected("SetPermissionsResponse"));
		};
		Ok(response)
	}

	/// Launches a DAP session and returns lifecycle/output events emitted before
	/// the launch response.
	pub async fn dap_launch(
		&self,
		request: pb::DapLaunchRequest,
		cancel: &CancellationToken,
	) -> Result<(pb::DapSessionResponse, Vec<DapRegistryEvent>), DocumentError> {
		let body = self
			.request(client_frame::Body::DapLaunch(request), cancel)
			.await?;
		let server_frame::Body::DapSession(response) = body else {
			return Err(unexpected("DAP session response"));
		};
		let events = self.take_dap_events(response.session.as_ref())?;
		Ok((response, events))
	}

	/// Attaches a DAP session and returns lifecycle/output events emitted before
	/// the attach response.
	pub async fn dap_attach(
		&self,
		request: pb::DapAttachRequest,
		cancel: &CancellationToken,
	) -> Result<(pb::DapSessionResponse, Vec<DapRegistryEvent>), DocumentError> {
		let body = self
			.request(client_frame::Body::DapAttach(request), cancel)
			.await?;
		let server_frame::Body::DapSession(response) = body else {
			return Err(unexpected("DAP session response"));
		};
		let events = self.take_dap_events(response.session.as_ref())?;
		Ok((response, events))
	}

	/// Executes one revision-fenced DAP action and returns the ordered events
	/// emitted before its terminal response.
	pub async fn dap_action(
		&self,
		request: pb::DapActionRequest,
		cancel: &CancellationToken,
	) -> Result<(pb::DapActionResponse, Vec<DapRegistryEvent>), DocumentError> {
		let body = self
			.request(client_frame::Body::DapAction(request), cancel)
			.await?;
		let server_frame::Body::DapAction(response) = body else {
			return Err(unexpected("DAP action response"));
		};
		let events = self.take_dap_events(response.session.as_ref())?;
		Ok((response, events))
	}

	fn take_dap_events(
		&self,
		session: Option<&pb::DapSessionRef>,
	) -> Result<Vec<DapRegistryEvent>, DocumentError> {
		let session = session.ok_or_else(|| unexpected("DAP response session identity"))?;
		if session.session_id.is_empty() {
			return Err(unexpected("non-empty DAP response session identity"));
		}
		Ok(self
			.inner
			.pending_dap_events
			.lock()
			.remove(&session.session_id)
			.unwrap_or_default())
	}

	/// Returns the authority-resolved LSP bindings for a document.
	pub async fn get_lsp_bindings(
		&self,
		request: pb::GetLspBindingsRequest,
		cancel: &CancellationToken,
	) -> Result<pb::GetLspBindingsResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::GetLspBindings(request), cancel)
			.await?;
		let server_frame::Body::LspBindings(response) = body else {
			return Err(unexpected("GetLspBindingsResponse"));
		};
		Ok(response)
	}

	/// Returns the native language-server roster and lifecycle stages.
	pub async fn lsp_status(
		&self,
		request: pb::LspStatusRequest,
		cancel: &CancellationToken,
	) -> Result<pb::LspStatusResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::LspStatus(request), cancel)
			.await?;
		let server_frame::Body::LspStatus(response) = body else {
			return Err(unexpected("LspStatusResponse"));
		};
		Ok(response)
	}

	/// Forwards an arbitrary non-lifecycle LSP request through the authority.
	pub async fn lsp_request(
		&self,
		request: pb::LspRequest,
		cancel: &CancellationToken,
	) -> Result<pb::LspResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::LspRequest(request), cancel)
			.await?;
		let server_frame::Body::LspResponse(response) = body else {
			return Err(unexpected("LspResponse"));
		};
		Ok(response)
	}

	/// Enqueues a non-lifecycle LSP notification on the selected server lane.
	pub async fn lsp_notification(
		&self,
		request: pb::LspNotificationRequest,
		cancel: &CancellationToken,
	) -> Result<pb::LspNotificationResponse, DocumentError> {
		let body = self
			.request(client_frame::Body::LspNotification(request), cancel)
			.await?;
		let server_frame::Body::LspNotificationAccepted(response) = body else {
			return Err(unexpected("LspNotificationResponse"));
		};
		Ok(response)
	}

	/// Atomically acquires or dry-runs an exclusive workspace path reservation.
	pub async fn acquire_workspace_lease(
		&self,
		request: pb::AcquireWorkspaceLeaseRequest,
		cancel: &CancellationToken,
	) -> Result<(Option<WorkspaceLease>, pb::AcquireWorkspaceLeaseResponse), DocumentError> {
		let body = self
			.request(client_frame::Body::AcquireWorkspaceLease(request), cancel)
			.await?;
		let server_frame::Body::WorkspaceLeaseAcquired(response) = body else {
			return Err(unexpected("AcquireWorkspaceLeaseResponse"));
		};
		if response
			.workspace_lease_id
			.as_ref()
			.is_some_and(|lease_id| lease_id.len() != 16)
		{
			return Err(unexpected("16-byte workspace lease id"));
		}
		let lease = response
			.workspace_lease_id
			.as_ref()
			.map(|lease_id| WorkspaceLease {
				lease_id: lease_id.clone(),
				host:     Arc::clone(&self.inner),
				released: false,
			});
		Ok((lease, response))
	}

	/// Explicitly releases an exclusive workspace reservation.
	pub async fn release_workspace_lease(
		&self,
		mut lease: WorkspaceLease,
		cancel: &CancellationToken,
	) -> Result<pb::ReleaseWorkspaceLeaseResponse, DocumentError> {
		if !Arc::ptr_eq(&self.inner, &lease.host) {
			return Err(unexpected("connection-owned workspace lease"));
		}
		let body = self
			.request(
				client_frame::Body::ReleaseWorkspaceLease(pb::ReleaseWorkspaceLeaseRequest {
					workspace_lease_id: lease.lease_id.clone(),
				}),
				cancel,
			)
			.await?;
		let server_frame::Body::WorkspaceLeaseReleased(response) = body else {
			return Err(unexpected("ReleaseWorkspaceLeaseResponse"));
		};
		lease.released = true;
		Ok(response)
	}

	/// Releases a connection-owned document lease.
	pub async fn close(
		&self,
		mut lease: DocumentLease,
		cancel: &CancellationToken,
	) -> Result<(), DocumentError> {
		let request = pb::CloseDocumentRequest { lease_id: lease.lease_id.clone() };
		self.close_request(&mut lease, request, cancel).await?;
		Ok(())
	}

	/// Forwards one canonical close request for a connection-owned lease.
	pub(crate) async fn close_request(
		&self,
		lease: &mut DocumentLease,
		request: pb::CloseDocumentRequest,
		cancel: &CancellationToken,
	) -> Result<pb::CloseDocumentResponse, DocumentError> {
		self.ensure_owned(lease)?;
		if request.lease_id != lease.lease_id {
			return Err(unexpected("connection-owned CloseDocumentRequest.lease_id"));
		}
		let body = self
			.request(client_frame::Body::CloseDocument(request), cancel)
			.await?;
		match body {
			server_frame::Body::DocumentClosed(response) => {
				lease.released = true;
				Ok(response)
			},
			_ => Err(unexpected("CloseDocumentResponse")),
		}
	}

	fn ensure_request_lease(
		&self,
		lease: &DocumentLease,
		target: Option<&pb::DocumentTarget>,
	) -> Result<(), DocumentError> {
		self.ensure_owned(lease)?;
		let lease_target = matches!(
			target.and_then(|target| target.target.as_ref()),
			Some(omp_proto::document::v1::document_target::Target::LeaseId(id)) if id == lease.id()
		);
		if !lease_target {
			return Err(unexpected("connection-owned document lease"));
		}
		Ok(())
	}

	fn ensure_owned(&self, lease: &DocumentLease) -> Result<(), DocumentError> {
		if Arc::ptr_eq(&self.inner, &lease.host) {
			Ok(())
		} else {
			Err(DocumentError::MalformedResponse(sf!(
				"document lease belongs to another document connection",
			)))
		}
	}

	async fn request(
		&self,
		body: client_frame::Body,
		cancel: &CancellationToken,
	) -> Result<server_frame::Body, DocumentError> {
		if self.inner.shutdown.is_cancelled() {
			return Err(DocumentError::Disconnected);
		}
		let request_id = self.inner.next_request.fetch_add(1, Ordering::Relaxed);
		if request_id == 0 {
			return Err(DocumentError::Disconnected);
		}
		let (response_tx, response_rx) = flume::bounded(1);
		self.inner.pending.lock().insert(request_id, response_tx);
		let mut pending = PendingRequest { inner: Arc::clone(&self.inner), request_id, armed: true };
		self
			.inner
			.writer
			.send_async(pb::ClientFrame { request_id, body: Some(body) })
			.await
			.map_err(|_| DocumentError::Disconnected)?;
		let frame = tokio::select! {
			() = cancel.cancelled() => return Err(DocumentError::Cancelled),
			() = self.inner.shutdown.cancelled() => return Err(DocumentError::Disconnected),
			result = response_rx.recv_async() => result.map_err(|_| DocumentError::Disconnected)?,
		};
		pending.armed = false;
		match frame.body {
			Some(server_frame::Body::Error(error)) => {
				Err(DocumentError::Protocol { code: error.code, message: Str::from(error.message) })
			},
			Some(body) => Ok(body),
			None => Err(unexpected("non-empty server frame")),
		}
	}
}

impl Drop for Inner {
	fn drop(&mut self) {
		self.shutdown.cancel();
	}
}

#[must_use]
struct PendingRequest {
	inner:      Arc<Inner>,
	request_id: u64,
	armed:      bool,
}

impl Drop for PendingRequest {
	fn drop(&mut self) {
		if !self.armed || self.inner.pending.lock().remove(&self.request_id).is_none() {
			return;
		}
		let _ = self.inner.writer.try_send(pb::ClientFrame {
			request_id: 0,
			body:       Some(client_frame::Body::Cancel(pb::CancelRequest {
				target_request_id: self.request_id,
			})),
		});
	}
}
fn ensure_requested_head(
	head: Option<&pb::DocumentHead>,
	requested_revision: Option<&pb::Revision>,
) -> Result<(), DocumentError> {
	let revision = head
		.and_then(|head| head.revision.as_ref())
		.ok_or_else(|| unexpected("revision-pinned response head"))?;
	if requested_revision.is_some_and(|requested| requested != revision) {
		return Err(DocumentError::MalformedResponse(sf!(
			"document server returned a revision other than the requested revision",
		)));
	}
	Ok(())
}

pub(crate) fn lease_target(lease: &DocumentLease) -> pb::DocumentTarget {
	pb::DocumentTarget { target: Some(document_target::Target::LeaseId(lease.lease_id.clone())) }
}
fn dispatch_event_frame(
	body: server_frame::Body,
	document_events: &Mutex<DocumentEventSubscribers>,
	pending_document_events: &Mutex<PendingDocumentEvents>,
	document_event_sequences: &Mutex<HashMap<Bytes, u64>>,
	dap_events: &Mutex<PendingDapEvents>,
	lsp_events: &flume::Sender<Result<LspRegistryEvent, EventStreamError>>,
) {
	match body {
		server_frame::Body::DocumentEvent(event) => {
			let Some(document_id) = event
				.head
				.as_ref()
				.and_then(|head| head.document.as_ref())
				.map(|document| document.id.clone())
				.filter(|id| !id.is_empty())
			else {
				return;
			};
			let mut sequences = document_event_sequences.lock();
			if sequences
				.get(&document_id)
				.is_some_and(|sequence| *sequence >= event.event_sequence)
			{
				return;
			}
			sequences.insert(document_id.clone(), event.event_sequence);
			drop(sequences);
			let mut delivered = false;
			document_events.lock().retain(|_, (subscribed_id, sender)| {
				if subscribed_id != &document_id {
					return true;
				}
				let alive = sender.send(Ok(event.clone())).is_ok();
				delivered |= alive;
				alive
			});
			if !delivered {
				pending_document_events
					.lock()
					.entry(document_id)
					.or_default()
					.push(Ok(event));
			}
		},
		server_frame::Body::DapOutput(output) => {
			if let Some(session) = output
				.session
				.as_ref()
				.filter(|session| !session.session_id.is_empty())
			{
				dap_events
					.lock()
					.entry(session.session_id.clone())
					.or_default()
					.push(DapRegistryEvent::Output(output));
			}
		},
		server_frame::Body::DapEvent(event) => {
			if let Some(session) = event
				.session
				.as_ref()
				.filter(|session| !session.session_id.is_empty())
			{
				dap_events
					.lock()
					.entry(session.session_id.clone())
					.or_default()
					.push(DapRegistryEvent::Event(event));
			}
		},
		server_frame::Body::LspEvent(event) => {
			let _ = lsp_events.send(Ok(LspRegistryEvent::Event(event)));
		},
		server_frame::Body::LspBindingEvent(event) => {
			let _ = lsp_events.send(Ok(LspRegistryEvent::Binding(event)));
		},
		server_frame::Body::EventStreamError(error) => {
			let terminal = EventStreamError {
				stream:         pb::EventStreamKind::try_from(error.stream)
					.unwrap_or(pb::EventStreamKind::Unspecified),
				failure:        pb::EventStreamFailure::try_from(error.failure)
					.unwrap_or(pb::EventStreamFailure::Unspecified),
				skipped_events: error.skipped_events,
				message:        Str::from(error.message),
			};
			match terminal.stream {
				pb::EventStreamKind::Document => {
					let subscriber = document_events.lock().remove(&error.lease_id);
					if let Some((_, sender)) = subscriber {
						let _ = sender.send(Err(terminal));
					} else {
						pending_document_events
							.lock()
							.entry(error.lease_id)
							.or_default()
							.push(Err(terminal));
					}
				},
				pb::EventStreamKind::LspRegistry | pb::EventStreamKind::Unspecified => {
					let _ = lsp_events.send(Err(terminal));
				},
			}
		},
		_ => {},
	}
}

const fn closed_stream_error(stream: pb::EventStreamKind) -> EventStreamError {
	EventStreamError {
		stream,
		failure: pb::EventStreamFailure::Closed,
		skipped_events: 0,
		message: sf!("document-server connection closed"),
	}
}

fn unexpected(expected: &'static str) -> DocumentError {
	DocumentError::MalformedResponse(Str::new(expected))
}

fn disconnected_frame(request_id: u64) -> pb::ServerFrame {
	pb::ServerFrame {
		request_id,
		body: Some(server_frame::Body::Error(pb::ProtocolError {
			code:    pb::ProtocolErrorCode::Internal.into(),
			message: "document-server connection closed".to_owned(),
		})),
	}
}
