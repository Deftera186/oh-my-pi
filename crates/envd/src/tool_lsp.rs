//! Production bridge from `lsp@1` to the project document authority.

use std::{
	collections::HashMap,
	future::Future,
	path::{Component, Path, PathBuf},
	str,
	sync::Arc,
	time::Duration,
};

use bytes::Bytes;
use omp_core::Str;
use omp_docserver::position::{PositionEncoding, TextEdit, apply_text_edits};
use omp_proto::{
	document::v1::{
		self as pb, commit_transaction_response, document_mutation, lsp_response, text_mutation,
	},
	env::v1::{self as env_pb, OutputChannel},
};
use omp_tools::lsp::{
	Action, Fault, LspControl, Params, Payload, actions,
	checkers::{self, CheckerExecutor, CheckerFault, CheckerOutput, CheckerRequest, Preset},
	diagnostics::{DiagnosticResult, render as render_diagnostics},
	navigation, refactor, render,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
	docs::{DocumentHost, lease_target},
	exec::{ExecEvent, ExecHost},
	tool_document::{read_whole, transaction_id},
};

#[derive(Clone)]
struct ExecChecker {
	host:     ExecHost,
	sessions: Arc<Mutex<HashMap<PathBuf, Bytes>>>,
}

impl CheckerExecutor for ExecChecker {
	fn run(
		&self,
		request: CheckerRequest,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<CheckerOutput, CheckerFault>> + Send + '_ {
		async move {
			let cwd_uri =
				Url::from_directory_path(&request.cwd).map_err(|()| CheckerFault::LaunchFailed)?;
			let cached_session = { self.sessions.lock().get(&request.cwd).cloned() };
			let session = if let Some(session) = cached_session {
				session
			} else {
				let opened = self
					.host
					.open_session(env_pb::OpenSessionRequest {
						cwd_uri: cwd_uri.to_string(),
						..Default::default()
					})
					.await
					.map_err(|_| CheckerFault::LaunchFailed)?;
				self
					.sessions
					.lock()
					.insert(request.cwd.clone(), opened.session.clone());
				opened.session
			};
			let command = checker_command(&request);
			let execution = self
				.host
				.exec(
					env_pb::ExecRequest {
						session: session.clone(),
						source: Some(env_pb::Script { text: command, ..Default::default() }),
						..Default::default()
					},
					Some(Duration::from_secs(300)),
				)
				.await;
			let (_, run) = match execution {
				Ok(execution) => execution,
				Err(_) => {
					self.sessions.lock().remove(&request.cwd);
					let _ = self.host.close_session(&session);
					return Err(CheckerFault::LaunchFailed);
				},
			};
			let mut stdout = Vec::new();
			let mut stderr = Vec::new();
			let mut cancelled = false;
			let status = loop {
				let event = if cancelled {
					run.next_event().await
				} else {
					tokio::select! {
						biased;
						() = cancel.cancelled() => {
							run.cancel();
							cancelled = true;
							continue;
						},
						event = run.next_event() => event,
					}
				};
				match event {
					Some(ExecEvent::Output(frame)) if frame.channel == OutputChannel::Stdout as i32 => {
						stdout.extend_from_slice(&frame.data);
					},
					Some(ExecEvent::Output(frame)) if frame.channel == OutputChannel::Stderr as i32 => {
						stderr.extend_from_slice(&frame.data);
					},
					Some(ExecEvent::Exit(exit)) => break exit.status,
					Some(ExecEvent::Started { .. } | ExecEvent::Output(_)) => {},
					None => break None,
				}
			};
			let status = status.ok_or(CheckerFault::LaunchFailed)?;
			if cancelled || status.outcome == env_pb::ExecOutcome::Cancelled as i32 {
				return Err(CheckerFault::Cancelled);
			}
			if status.outcome == env_pb::ExecOutcome::Timeout as i32 {
				return Err(CheckerFault::TimedOut);
			}
			Ok(CheckerOutput {
				status: status.exit_code,
				stdout: bounded_checker_output(&stdout, request.line_limit),
				stderr: bounded_checker_output(&stderr, request.line_limit),
			})
		}
	}
}

fn checker_command(request: &CheckerRequest) -> String {
	let mut command = shell_quote(request.program.as_str());
	for argument in &request.args {
		command.push(' ');
		command.push_str(&shell_quote(argument.as_str()));
	}
	command
}

fn shell_quote(value: &str) -> String {
	format!("'{}'", value.replace('\'', "'\\''"))
}

fn bounded_checker_output(bytes: &[u8], limit: usize) -> Str {
	Str::from(
		String::from_utf8_lossy(bytes)
			.lines()
			.take(limit)
			.collect::<Vec<_>>()
			.join("\n"),
	)
}

fn merge_workspace_edits(edits: &[Value], renamed: Option<(&str, &str)>) -> Result<Value, Fault> {
	let mut changes = serde_json::Map::<String, Value>::new();
	for edit in edits {
		if let Some(entries) = edit.get("changes").and_then(Value::as_object) {
			for (uri, value) in entries {
				append_text_edits(&mut changes, rewrite_uri(uri, renamed), value)?;
			}
		}
		for change in edit
			.get("documentChanges")
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
		{
			if change.get("kind").is_some() {
				return Err(Fault::WorkspaceEdit);
			}
			let uri = change
				.pointer("/textDocument/uri")
				.and_then(Value::as_str)
				.ok_or(Fault::WorkspaceEdit)?;
			let value = change.get("edits").ok_or(Fault::WorkspaceEdit)?;
			append_text_edits(&mut changes, rewrite_uri(uri, renamed), value)?;
		}
	}
	let merged = json!({ "changes": changes });
	refactor::validate_workspace_edit(&merged).map_err(|_| Fault::WorkspaceEdit)?;
	Ok(merged)
}

fn append_text_edits(
	changes: &mut serde_json::Map<String, Value>,
	uri: String,
	edits: &Value,
) -> Result<(), Fault> {
	let incoming = edits.as_array().ok_or(Fault::WorkspaceEdit)?;
	let entry = changes
		.entry(uri)
		.or_insert_with(|| Value::Array(Vec::new()))
		.as_array_mut()
		.ok_or(Fault::WorkspaceEdit)?;
	entry.extend(incoming.iter().cloned());
	Ok(())
}

fn rewrite_uri(uri: &str, renamed: Option<(&str, &str)>) -> String {
	renamed
		.filter(|(source, _)| *source == uri)
		.map_or_else(|| uri.to_owned(), |(_, destination)| destination.to_owned())
}
fn reload_notification(binding: &pb::LspServerBinding) -> pb::LspNotificationRequest {
	pb::LspNotificationRequest {
		server_id:   binding.server_id.clone(),
		method:      "workspace/didChangeConfiguration".into(),
		params_json: binding.settings_json.clone(),
	}
}

/// Environment-owned implementation of the revisioned LSP tool.
#[derive(Clone)]
pub struct DocumentLspControl {
	documents: DocumentHost,
	checker:   ExecChecker,
}

impl DocumentLspControl {
	/// Binds the project document authority.
	pub fn new(documents: DocumentHost, exec: ExecHost) -> Self {
		Self {
			documents,
			checker: ExecChecker { host: exec, sessions: Arc::new(Mutex::new(HashMap::new())) },
		}
	}

	fn file_uri(&self, file: &str) -> Result<Url, Fault> {
		let mut root = Url::parse(self.documents.hello().root_uri.as_str())
			.map_err(|_| Fault::InvalidArguments)?;
		if file == "*" {
			return Ok(root);
		}
		let path = Path::new(file);
		if path.is_absolute()
			|| path.components().any(|component| {
				matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
			}) {
			return Err(Fault::InvalidArguments);
		}
		root
			.path_segments_mut()
			.map_err(|_| Fault::InvalidArguments)?
			.pop_if_empty()
			.extend(path.components().filter_map(|component| match component {
				Component::Normal(value) => value.to_str(),
				Component::CurDir => None,
				_ => None,
			}));
		Ok(root)
	}

	async fn apply_workspace_edit(
		&self,
		edit: &Value,
		encoding: PositionEncoding,
		cancel: &CancellationToken,
	) -> Result<usize, Fault> {
		let changes = edit
			.get("changes")
			.and_then(Value::as_object)
			.ok_or(Fault::WorkspaceEdit)?;
		let mut operations = Vec::with_capacity(changes.len());
		for (uri, edits) in changes {
			let lease = self
				.documents
				.open(Str::from(uri.as_str()), None, cancel)
				.await
				.map_err(|_| Fault::WorkspaceEdit)?;
			let content = read_whole(&self.documents, &lease)
				.await
				.map_err(|_| Fault::WorkspaceEdit)?;
			let text = str::from_utf8(&content).map_err(|_| Fault::WorkspaceEdit)?;
			let edits = serde_json::from_value::<Vec<TextEdit>>(edits.clone())
				.map_err(|_| Fault::WorkspaceEdit)?;
			let content =
				apply_text_edits(text, &edits, encoding).map_err(|_| Fault::WorkspaceEdit)?;
			operations.push(pb::DocumentMutation {
				document:  Some(lease_target(&lease)),
				operation: Some(document_mutation::Operation::Text(pb::TextMutation {
					base_revision: lease.head().revision.clone(),
					change:        Some(text_mutation::Change::ProposedContent(content)),
					stale_policy:  pb::StalePolicy::Fail as i32,
					format_policy: pb::FormatPolicy::BestEffort as i32,
				})),
			});
		}
		let response = self
			.documents
			.commit_transaction(
				transaction_id(self.documents.hello().server_epoch.as_ref()),
				operations,
				cancel,
			)
			.await
			.map_err(|_| Fault::WorkspaceEdit)?;
		match response.outcome {
			Some(commit_transaction_response::Outcome::Committed(committed)) => {
				Ok(committed.operations.len())
			},
			_ => Err(Fault::WorkspaceEdit),
		}
	}

	async fn workspace_diagnostics(&self, cancel: &CancellationToken) -> Result<Payload, Fault> {
		let root = Url::parse(self.documents.hello().root_uri.as_str())
			.map_err(|_| Fault::Unavailable)?
			.to_file_path()
			.map_err(|()| Fault::Unavailable)?;
		let candidates: &[(Preset, &[&str])] = &[
			(Preset::Cargo, &["Cargo.toml"]),
			(Preset::TypeScript, &["tsconfig.json", "tsconfig.base.json"]),
			(Preset::Go, &["go.work", "go.mod"]),
			(Preset::Pyright, &["pyproject.toml", "pyrightconfig.json"]),
			(Preset::Biome, &["biome.json", "biome.jsonc"]),
			(Preset::SwiftLint, &[".swiftlint.yml", "Package.swift"]),
		];
		let mut diagnostics = Vec::new();
		let mut servers = Vec::new();
		let mut failures = Vec::new();
		for (preset, markers) in candidates {
			if !markers.iter().any(|marker| root.join(marker).is_file()) {
				continue;
			}
			servers.push(Str::from(preset.to_string()));
			let request = checkers::request(*preset, &root, None);
			match self.checker.run(request, cancel.child_token()).await {
				Ok(output) => match checkers::parse_output(*preset, &output) {
					Ok(mut findings) => diagnostics.append(&mut findings),
					Err(error) => failures.push(Str::from(error.to_string())),
				},
				Err(error) => failures.push(Str::from(error.to_string())),
			}
		}
		let result = DiagnosticResult::new(diagnostics, failures.is_empty());
		let output = render_diagnostics(&result);
		let data = json!({
			"diagnostics": result.diagnostics,
			"omitted": result.omitted,
			"complete": result.complete,
			"failures": failures,
		});
		Ok(Payload { action: Action::Diagnostics, servers, output, data })
	}
}

impl LspControl for DocumentLspControl {
	fn execute(
		&self,
		params: Params,
		_timeout: Duration,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<Payload, Fault>> + Send + '_ {
		async move {
			let file = params.file.as_deref().unwrap_or("*");
			let uri = self.file_uri(file)?;
			if file == "*" {
				if params.action == Action::Diagnostics {
					return self.workspace_diagnostics(&cancel).await;
				}
				if params.action == Action::Status {
					return Ok(Payload {
						action:  params.action,
						servers: Vec::new(),
						output:  Str::from(
							"Native project document daemon is connected; provide a file for binding \
							 status",
						),
						data:    json!({ "daemon": "connected", "workspace": self.documents.hello().root_uri }),
					});
				}
				return Err(Fault::InvalidArguments);
			}
			if params.action == Action::RenameFile
				&& uri.to_file_path().is_ok_and(|path| path.is_dir())
			{
				let destination =
					self.file_uri(params.query.as_deref().ok_or(Fault::InvalidArguments)?)?;
				let data = json!({
					"oldUri": uri.as_str(),
					"newUri": destination.as_str(),
					"directory": true,
				});
				if params.apply == Some(false) {
					return Ok(Payload {
						action: params.action,
						servers: Vec::new(),
						output: Str::from(format!(
							"Would rename directory {} to {}",
							uri.path(),
							destination.path(),
						)),
						data,
					});
				}
				self
					.documents
					.rename(
						pb::RenamePathRequest {
							source_uri:           uri.to_string(),
							destination_uri:      destination.to_string(),
							overwrite:            pb::DestinationOverwritePolicy::FailIfExists as i32,
							source_revision:      None,
							destination_revision: None,
						},
						&cancel,
					)
					.await
					.map_err(|_| Fault::WorkspaceEdit)?;
				return Ok(Payload {
					action: params.action,
					servers: Vec::new(),
					output: Str::from(format!(
						"Renamed directory {} to {}",
						uri.path(),
						destination.path(),
					)),
					data,
				});
			}
			let lease = self
				.documents
				.open(Str::from(uri.as_str()), None, &cancel)
				.await
				.map_err(|_| Fault::Unavailable)?;
			let bindings = self
				.documents
				.get_lsp_bindings(
					pb::GetLspBindingsRequest { document: Some(lease_target(&lease)) },
					&cancel,
				)
				.await
				.map_err(|_| Fault::Server)?
				.bindings;
			let selected = bindings
				.into_iter()
				.filter(|binding| {
					params
						.server
						.as_ref()
						.is_none_or(|name| name.as_str() == binding.name)
				})
				.collect::<Vec<_>>();
			if selected.is_empty() {
				return Err(Fault::Unavailable);
			}
			let servers = selected
				.iter()
				.map(|binding| Str::from(binding.name.as_str()))
				.collect::<Vec<_>>();
			if params.action == Action::Reload {
				for binding in &selected {
					self
						.documents
						.lsp_notification(reload_notification(binding), &cancel)
						.await
						.map_err(|_| Fault::Server)?;
					if binding.name.contains("rust-analyzer") {
						let _ = self
							.documents
							.lsp_request(
								pb::LspRequest {
									server_id:    binding.server_id.clone(),
									method:       "rust-analyzer/reloadWorkspace".into(),
									params_json:  Bytes::from_static(b"{}"),
									document:     Some(lease_target(&lease)),
									revision:     lease.head().revision.clone(),
									stale_policy: pb::LspStalePolicy::Fail as i32,
								},
								&cancel,
							)
							.await;
					}
				}
				return Ok(Payload {
					action: params.action,
					servers,
					output: Str::new_static("Reloaded selected language-server configuration"),
					data: json!({ "reloaded": true }),
				});
			}
			if params.action == Action::RenameFile {
				let destination =
					self.file_uri(params.query.as_deref().ok_or(Fault::InvalidArguments)?)?;
				let rename_params = json!({
					"files": [{ "oldUri": uri.as_str(), "newUri": destination.as_str() }],
				});
				let mut edits = Vec::new();
				for binding in &selected {
					let response = self
						.documents
						.lsp_request(
							pb::LspRequest {
								server_id:    binding.server_id.clone(),
								method:       "workspace/willRenameFiles".into(),
								params_json:  Bytes::from(
									serde_json::to_vec(&rename_params)
										.map_err(|_| Fault::InvalidArguments)?,
								),
								document:     Some(lease_target(&lease)),
								revision:     lease.head().revision.clone(),
								stale_policy: pb::LspStalePolicy::Fail as i32,
							},
							&cancel,
						)
						.await
						.map_err(|_| Fault::Server)?;
					if let Some(lsp_response::Outcome::ResultJson(bytes)) = response.outcome {
						let edit: Value = serde_json::from_slice(&bytes).map_err(|_| Fault::Server)?;
						if !edit.is_null() {
							edits.push(edit);
						}
					}
				}
				let preview_edit = merge_workspace_edits(&edits, None)?;
				let data = json!({
					"oldUri": uri.as_str(),
					"newUri": destination.as_str(),
					"workspaceEdit": preview_edit,
				});
				if params.apply == Some(false) {
					return Ok(Payload {
						action: params.action,
						servers,
						output: refactor::preview(&preview_edit),
						data,
					});
				}
				let source_revision = lease.head().revision.clone();
				self
					.documents
					.rename(
						pb::RenamePathRequest {
							source_uri: uri.to_string(),
							destination_uri: destination.to_string(),
							overwrite: pb::DestinationOverwritePolicy::FailIfExists as i32,
							source_revision,
							destination_revision: None,
						},
						&cancel,
					)
					.await
					.map_err(|_| Fault::WorkspaceEdit)?;
				let committed_edit =
					merge_workspace_edits(&edits, Some((uri.as_str(), destination.as_str())))?;
				let encoding = selected
					.first()
					.and_then(|binding| binding.sync_policy.as_ref())
					.map(|policy| PositionEncoding::from_lsp_name(Some(&policy.position_encoding)))
					.unwrap_or_default();
				if !edits.is_empty()
					&& self
						.apply_workspace_edit(&committed_edit, encoding, &cancel)
						.await
						.is_err()
				{
					let rollback = self
						.documents
						.open(Str::from(destination.as_str()), None, &cancel)
						.await
						.map_err(|_| Fault::WorkspaceEdit)?;
					self
						.documents
						.rename(
							pb::RenamePathRequest {
								source_uri:           destination.to_string(),
								destination_uri:      uri.to_string(),
								overwrite:            pb::DestinationOverwritePolicy::FailIfExists as i32,
								source_revision:      rollback.head().revision.clone(),
								destination_revision: None,
							},
							&cancel,
						)
						.await
						.map_err(|_| Fault::WorkspaceEdit)?;
					return Err(Fault::WorkspaceEdit);
				}
				for binding in &selected {
					let _ = self
						.documents
						.lsp_notification(
							pb::LspNotificationRequest {
								server_id:   binding.server_id.clone(),
								method:      "workspace/didRenameFiles".into(),
								params_json: Bytes::from(
									serde_json::to_vec(&rename_params)
										.map_err(|_| Fault::InvalidArguments)?,
								),
							},
							&cancel,
						)
						.await;
				}
				return Ok(Payload {
					action: params.action,
					servers,
					output: Str::from(format!(
						"Renamed {} to {} with import updates",
						uri.path(),
						destination.path(),
					)),
					data,
				});
			}
			if params.action == Action::Status {
				let data = Value::Array(selected.iter().map(|binding| json!({ "name": binding.name, "state": "ready", "position_encoding": binding.sync_policy.as_ref().map(|policy| policy.position_encoding.as_str()) })).collect());
				return Ok(Payload {
					action: params.action,
					servers,
					output: render::structured(&data, 50),
					data,
				});
			}
			if params.action == Action::Capabilities {
				let data = Value::Array(
					selected
						.iter()
						.map(|binding| {
							serde_json::from_slice(&binding.capabilities_json).unwrap_or(Value::Null)
						})
						.collect(),
				);
				return Ok(Payload {
					action: params.action,
					servers,
					output: Str::from(serde_json::to_string_pretty(&data).unwrap_or_default()),
					data,
				});
			}
			let content = read_whole(&self.documents, &lease)
				.await
				.map_err(|_| Fault::Server)?;
			let line = params.line.unwrap_or(1);
			let symbol_target = params
				.symbol
				.as_ref()
				.map(|symbol| navigation::parse_symbol_target(symbol))
				.transpose()
				.map_err(|_| Fault::InvalidArguments)?;
			let source_line = if symbol_target.is_some() {
				let text = str::from_utf8(&content).map_err(|_| Fault::InvalidArguments)?;
				Some(
					text
						.lines()
						.nth(line.saturating_sub(1) as usize)
						.ok_or(Fault::InvalidArguments)?,
				)
			} else {
				None
			};
			let method = if params.action == Action::Request {
				params.method.as_deref().ok_or(Fault::InvalidArguments)?
			} else if params.action == Action::Reload {
				"rust-analyzer/reloadWorkspace"
			} else {
				actions::method(params.action).ok_or(Fault::InvalidArguments)?
			};
			let mut results = Vec::new();
			for binding in &selected {
				let encoding = binding
					.sync_policy
					.as_ref()
					.map(|policy| PositionEncoding::from_lsp_name(Some(&policy.position_encoding)))
					.unwrap_or_default();
				let character = match (source_line, symbol_target.as_ref()) {
					(Some(source_line), Some(target)) => {
						navigation::resolve_symbol_column(source_line, target, encoding)
							.ok_or(Fault::InvalidArguments)?
					},
					_ => 0,
				};
				let mut request_params = actions::auto_parameters(
					params.params.clone(),
					Some(uri.as_str()),
					params.line,
					Some(character),
				);
				if params.action == Action::References {
					request_params["context"] = json!({ "includeDeclaration": true });
				}
				if params.action == Action::Rename {
					request_params["newName"] = json!(params.new_name);
				}
				if params.action == Action::Symbols {
					request_params = json!({ "textDocument": { "uri": uri.as_str() } });
				}
				if params.action == Action::CodeActions {
					request_params["range"] = json!({ "start": { "line": line - 1, "character": character }, "end": { "line": line - 1, "character": character } });
					request_params["context"] = json!({ "diagnostics": [], "triggerKind": 1 });
				}
				let response = self
					.documents
					.lsp_request(
						pb::LspRequest {
							server_id:    binding.server_id.clone(),
							method:       method.into(),
							params_json:  Bytes::from(
								serde_json::to_vec(&request_params).map_err(|_| Fault::InvalidArguments)?,
							),
							document:     Some(lease_target(&lease)),
							revision:     lease.head().revision.clone(),
							stale_policy: pb::LspStalePolicy::Fail as i32,
						},
						&cancel,
					)
					.await
					.map_err(|_| Fault::Server)?;
				match response.outcome {
					Some(lsp_response::Outcome::ResultJson(bytes)) => {
						results.push(serde_json::from_slice(&bytes).map_err(|_| Fault::Server)?)
					},
					Some(lsp_response::Outcome::Error(_)) | None => return Err(Fault::Server),
				}
			}
			let mut data = if results.len() == 1 {
				results.remove(0)
			} else {
				Value::Array(results)
			};
			if params.action == Action::Rename {
				let edits = data
					.as_array()
					.map_or_else(|| vec![data.clone()], Clone::clone);
				data = merge_workspace_edits(&edits, None)?;
			}
			if matches!(
				params.action,
				Action::Definition
					| Action::TypeDefinition
					| Action::Implementation
					| Action::References
			) {
				data = Value::Array(navigation::normalize_locations(
					&data,
					if params.action == Action::References {
						200
					} else {
						50
					},
				));
			}
			if params.action == Action::Rename {
				refactor::validate_workspace_edit(&data).map_err(|_| Fault::WorkspaceEdit)?;
				if params.apply.unwrap_or(true) {
					let encoding = selected
						.first()
						.and_then(|binding| binding.sync_policy.as_ref())
						.map(|policy| PositionEncoding::from_lsp_name(Some(&policy.position_encoding)))
						.unwrap_or_default();
					let applied = self.apply_workspace_edit(&data, encoding, &cancel).await?;
					let output =
						Str::from(format!("Applied rename transaction across {applied} document(s)"));
					return Ok(Payload { action: params.action, servers, output, data });
				}
			}
			if params.action == Action::CodeActions {
				let actions = data
					.as_array()
					.into_iter()
					.flatten()
					.flat_map(|value| {
						value
							.as_array()
							.map_or_else(|| vec![value.clone()], Clone::clone)
					})
					.collect::<Vec<_>>();
				if params.apply == Some(true) {
					let selector = params.query.as_deref().ok_or(Fault::InvalidArguments)?;
					let index = selector
						.parse::<usize>()
						.ok()
						.and_then(|index| index.checked_sub(1))
						.or_else(|| {
							actions.iter().position(|action| {
								action
									.get("title")
									.and_then(Value::as_str)
									.is_some_and(|title| title.eq_ignore_ascii_case(selector))
							})
						})
						.ok_or(Fault::InvalidArguments)?;
					let mut action = actions.get(index).cloned().ok_or(Fault::InvalidArguments)?;
					let binding = selected.first().ok_or(Fault::Unavailable)?;
					if action.get("data").is_some() {
						let response = self
							.documents
							.lsp_request(
								pb::LspRequest {
									server_id:    binding.server_id.clone(),
									method:       "codeAction/resolve".into(),
									params_json:  Bytes::from(
										serde_json::to_vec(&action).map_err(|_| Fault::InvalidArguments)?,
									),
									document:     Some(lease_target(&lease)),
									revision:     lease.head().revision.clone(),
									stale_policy: pb::LspStalePolicy::Fail as i32,
								},
								&cancel,
							)
							.await
							.map_err(|_| Fault::Server)?;
						if let Some(lsp_response::Outcome::ResultJson(bytes)) = response.outcome {
							action = serde_json::from_slice(&bytes).map_err(|_| Fault::Server)?;
						}
					}
					let mut applied = 0;
					if let Some(edit) = action.get("edit") {
						let edit = merge_workspace_edits(&[edit.clone()], None)?;
						let encoding = binding
							.sync_policy
							.as_ref()
							.map(|policy| PositionEncoding::from_lsp_name(Some(&policy.position_encoding)))
							.unwrap_or_default();
						applied = self.apply_workspace_edit(&edit, encoding, &cancel).await?;
					}
					if let Some(command) = action.get("command") {
						let (name, arguments) = if let Some(name) = command.as_str() {
							(name, Value::Array(Vec::new()))
						} else {
							(
								command
									.get("command")
									.and_then(Value::as_str)
									.ok_or(Fault::InvalidArguments)?,
								command
									.get("arguments")
									.cloned()
									.unwrap_or_else(|| Value::Array(Vec::new())),
							)
						};
						let response = self
							.documents
							.lsp_request(
								pb::LspRequest {
									server_id:    binding.server_id.clone(),
									method:       "workspace/executeCommand".into(),
									params_json:  Bytes::from(
										serde_json::to_vec(&json!({
											"command": name,
											"arguments": arguments,
										}))
										.map_err(|_| Fault::InvalidArguments)?,
									),
									document:     Some(lease_target(&lease)),
									revision:     lease.head().revision.clone(),
									stale_policy: pb::LspStalePolicy::Fail as i32,
								},
								&cancel,
							)
							.await
							.map_err(|_| Fault::Server)?;
						if !matches!(response.outcome, Some(lsp_response::Outcome::ResultJson(_))) {
							return Err(Fault::Server);
						}
					}
					let output = Str::from(format!(
						"Applied code action {} across {applied} document(s)",
						action
							.get("title")
							.and_then(Value::as_str)
							.unwrap_or(selector),
					));
					return Ok(Payload { action: params.action, servers, output, data: action });
				}
				data = Value::Array(actions);
			}
			let output = match params.action {
				Action::Hover => data
					.get("contents")
					.map_or_else(|| Str::from("No hover information"), navigation::hover_text),
				Action::Rename => refactor::preview(&data),
				_ => render::structured(&data, 200),
			};
			Ok(Payload { action: params.action, servers, output, data })
		}
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reload_notification_preserves_configured_and_empty_settings() {
		for settings_json in [
			Bytes::from_static(br#"{"settings":{"rust-analyzer":{"cargo":{"features":"all"}}}}"#),
			Bytes::from_static(br#"{"settings":{}}"#),
		] {
			let binding = pb::LspServerBinding {
				server_id: Bytes::from_static(b"active-server"),
				settings_json: settings_json.clone(),
				..Default::default()
			};
			let notification = reload_notification(&binding);
			assert_eq!(notification.server_id, binding.server_id);
			assert_eq!(notification.method, "workspace/didChangeConfiguration");
			assert_eq!(notification.params_json, settings_json);
		}
	}
}
