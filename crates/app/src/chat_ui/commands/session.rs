//! Structural durable-session and workspace routes.

use omp_core::{Str, sf};
use omp_storage::index::SessionStatistics;

use super::{BranchRequest, SessionRequest, WorkspaceRequest, command};

command!(help, 10, "help", icon: Keyboard, ["hotkeys"], "Show commands and keyboard controls", [], true, none => |host| host.help());
command!(new_session, 20, "new", icon: Add, [], "Start a new session", [Session], false, none => |host| host.new_session());
command!(clear, 30, "clear", icon: Broom, [], "Clear context inside this session", [Session], false, none => |host| host.clear());
command!(fresh, 40, "fresh", icon: Refresh, [], "Reset provider affinity for the next turn", [Session], false, none => |host| host.fresh());
command!(drop_session, 45, "drop", icon: Trash, [], "Delete the current session and start a new one", [Session, Owner], false, none => |host| async move {
	host.session(SessionRequest::Delete { force: true }).await
});
command!(rename, 50, "rename", icon: Pencil, [], "Rename this session", [Session], false, required("<title>") => |host, title| host.rename(title));
command!(retry, 60, "retry", icon: Redo, [], "Retry the previous user turn", [Session, Execution], false, none => |host| host.retry());
command!(resume, 70, "resume", icon: History, [], "Resume a native session", [Session], false, selector("[session]") => |host, selector| host.resume(selector));
command!(pin, 79, "pin", icon: Pin, [], "Pin or unpin a session at the top of the resume list", [Session, Owner], false, optional("[session id]") => |host, selector| host.session(SessionRequest::Pin(selector)));
command!(session, 80, "session", icon: Session, [], "Inspect or mutate this session", [Session, Owner], false, typed("info|delete|pin [session id]", ["info", "delete", "pin"], parse_session) => |host, request| host.session(request));
command!(jobs, 81, "jobs", icon: Job, [], "List active background jobs", [Execution], true, none => |host| host.jobs());
command!(agents, 82, "agents", icon: Agents, [], "Open the live agent hierarchy", [Execution], false, none => |host| host.agents());
command!(pause, 83, "pause", icon: Pause, [], "Pause the interactive session", [Execution], false, none => |host| host.pause());
command!(move_root, 90, "move", icon: FolderMove, [], "Set the primary workspace root for the next resume", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Move(root)));
command!(add_dir, 100, "add-dir", icon: FolderPlus, [], "Add a directory to this session's workspace roots", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Add(root)));
command!(remove_dir, 110, "remove-dir", icon: FolderMinus, [], "Remove a directory from this session's workspace roots", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Remove(root)));
command!(dirs, 120, "dirs", icon: Folder, [], "List this session's effective workspace roots", [Workspace], true, none => |host| host.workspace(WorkspaceRequest::List));

command!(handoff, 121, "handoff", icon: Handoff, [], "Summarize the session into a handoff document and compact in place", [Session, Execution], false, optional("[focus instructions]") => |host, instructions| host.handoff(instructions));
command!(branch, 122, "branch", icon: Branch, [], "Create a new branch from a checkpoint", [Session, Execution], false, typed("[checkpoint]", [], parse_branch) => |host, request| host.branch(request));
command!(fork, 123, "fork", icon: Branch, [], "Create an independent fork of the live session", [Session, Execution], false, optional("[title]") => |host, title| host.fork(title));
command!(branch_tree, 124, "tree", icon: Worktree, [], "Show session branch lineage and descendants", [Session], false, none => |host| host.branch_tree());
command!(quit, 900, "quit", icon: Power, ["exit", "q"], "Exit the client", [], true, none => |host| host.quit());
fn parse_branch(args: &str) -> miette::Result<BranchRequest> {
	let checkpoint = args.trim();
	if checkpoint.split_whitespace().count() > 1 {
		return Err(miette::miette!("usage: /branch [checkpoint]"));
	}
	Ok(BranchRequest { checkpoint: (!checkpoint.is_empty()).then(|| Str::new(checkpoint)) })
}

fn parse_session(args: &str) -> miette::Result<SessionRequest> {
	let mut words = args.split_whitespace();
	match (words.next(), words.next(), words.next()) {
		(Some("info"), None, None) => Ok(SessionRequest::Info),
		(Some("delete"), None, None) => Ok(SessionRequest::Delete { force: false }),
		(Some("pin"), None, None) => Ok(SessionRequest::Pin(None)),
		(Some("pin"), Some(session), None) => Ok(SessionRequest::Pin(Some(Str::new(session)))),
		_ => Err(miette::miette!("usage: /session info|delete|pin [session id]")),
	}
}
/// Provider facts resolved from the catalog for the active model.
pub struct ProviderInfo {
	/// Human-readable provider name.
	pub name:         Str,
	/// Normalized model key.
	pub model:        Str,
	/// Catalog display name for the model.
	pub display_name: Str,
	/// Wire codec identifier of the preferred route.
	pub api:          Str,
	/// Route base endpoint URL.
	pub endpoint:     Str,
	/// Resolved authentication summary (mode and credential presence).
	pub auth:         Str,
}

/// One live MCP server row for the report.
pub struct McpServerInfo {
	/// Declared server name.
	pub name:   Str,
	/// Lifecycle health label.
	pub health: &'static str,
	/// Live tool catalog size.
	pub tools:  usize,
}

/// One language server row for the report.
pub struct LspServerInfo {
	/// Declared server name.
	pub name:       Str,
	/// Lowercase lifecycle stage label.
	pub stage:      Str,
	/// Accepted extensions or exact filenames.
	pub file_types: Vec<Str>,
	/// Failure detail, when the server could not start.
	pub detail:     Option<Str>,
}

/// Facts assembled by the live host for `/session info`.
pub struct SessionInfo {
	/// Durable session file path, absent until first save.
	pub file:           Option<Str>,
	/// Stable session identifier.
	pub id:             Str,
	/// Current session title, when one is set.
	pub title:          Option<Str>,
	/// Raw active model identifier, rendered when catalog resolution fails.
	pub model:          Str,
	/// Catalog-resolved provider facts, absent for unknown models.
	pub provider:       Option<ProviderInfo>,
	/// Durable message/token/cost aggregates, absent when the index query
	/// failed.
	pub stats:          Option<SessionStatistics>,
	/// Context tokens currently in use.
	pub context_tokens: u64,
	/// Model context window, when known.
	pub context_window: Option<u64>,
	/// Queued user submissions.
	pub queued:         usize,
	/// Live MCP servers in stable name order.
	pub mcp:            Vec<McpServerInfo>,
	/// Discovered language servers in stable name order.
	pub lsp:            Vec<LspServerInfo>,
}

/// Renders the `/session info` markdown report from collected facts.
pub fn render_info(info: &SessionInfo) -> Str {
	use std::fmt::Write as _;

	let mut out = String::with_capacity(1024);
	out.push_str("**Session**\n");
	match &info.file {
		Some(file) => {
			let _ = writeln!(out, "File: `{file}`");
		},
		None => out.push_str("File: in-memory (not saved yet)\n"),
	}
	let _ = writeln!(out, "ID: `{}`", info.id);
	if let Some(title) = &info.title {
		let _ = writeln!(out, "Title: {title}");
	}

	out.push_str("\n**Provider**\n");
	match &info.provider {
		Some(provider) => {
			let _ = writeln!(out, "Name: {}", provider.name);
			if provider.display_name.as_str() == provider.model.as_str() {
				let _ = writeln!(out, "Model: `{}`", provider.model);
			} else {
				let _ = writeln!(out, "Model: `{}` ({})", provider.model, provider.display_name);
			}
			let _ = writeln!(out, "API: `{}`", provider.api);
			let _ = writeln!(out, "Endpoint: {}", provider.endpoint);
			let _ = writeln!(out, "Auth: {}", provider.auth);
		},
		None => {
			let _ = writeln!(out, "Model: `{}` (no catalog entry)", info.model);
		},
	}

	if let Some(stats) = &info.stats {
		out.push_str("\n**Messages**\n");
		let _ = writeln!(out, "User: {}", stats.user_messages);
		let _ = writeln!(out, "Assistant: {}", stats.assistant_messages);
		let _ = writeln!(out, "Tool calls: {}", stats.tool_calls);
		if stats.tool_errors > 0 {
			let _ =
				writeln!(out, "Tool results: {} ({} errors)", stats.tool_results, stats.tool_errors);
		} else {
			let _ = writeln!(out, "Tool results: {}", stats.tool_results);
		}
		let total_messages = stats.user_messages
			+ stats.assistant_messages
			+ stats.system_messages
			+ stats.tool_calls
			+ stats.tool_results;
		let _ = writeln!(out, "Total: {total_messages}");

		out.push_str("\n**Tokens**\n");
		let _ = writeln!(out, "Input: {}", stats.usage.input_tokens);
		let _ = writeln!(out, "Output: {}", stats.usage.output_tokens);
		if stats.usage.cache_read_tokens > 0 {
			let _ = writeln!(out, "Cache read: {}", stats.usage.cache_read_tokens);
		}
		if stats.usage.cache_write_tokens > 0 {
			let _ = writeln!(out, "Cache write: {}", stats.usage.cache_write_tokens);
		}
		let total_tokens = stats.usage.total_tokens.unwrap_or_else(|| {
			stats.usage.input_tokens
				+ stats.usage.output_tokens
				+ stats.usage.cache_read_tokens
				+ stats.usage.cache_write_tokens
		});
		let _ = writeln!(out, "Total: {total_tokens}");

		out.push_str("\n**Cost**\n");
		let dollars = stats.cost.nanos_usd as f64 / 1_000_000_000.0;
		if stats.cost.estimated {
			let _ = writeln!(out, "Total: ${dollars:.4} (estimated)");
		} else {
			let _ = writeln!(out, "Total: ${dollars:.4}");
		}
		if stats.request_errors > 0 {
			let _ = writeln!(out, "Requests: {} ({} failed)", stats.requests, stats.request_errors);
		} else {
			let _ = writeln!(out, "Requests: {}", stats.requests);
		}
		if let Some(premium) = stats.usage.premium_requests
			&& premium > 0
		{
			let _ = writeln!(out, "Premium requests: {premium}");
		}
		if stats.sessions > 1 {
			let _ = writeln!(out, "Sessions: {} (including subagents)", stats.sessions);
		}
	}

	out.push_str("\n**Context**\n");
	match info.context_window {
		Some(window) if window > 0 => {
			let percent = info.context_tokens.saturating_mul(100) / window;
			let _ = writeln!(
				out,
				"{} / {window} tokens ({percent}%) · {} queued",
				info.context_tokens, info.queued
			);
		},
		_ => {
			let _ = writeln!(out, "{} tokens · {} queued", info.context_tokens, info.queued);
		},
	}

	if !info.mcp.is_empty() {
		out.push_str("\n**MCP servers**\n");
		for server in &info.mcp {
			let tools = if server.tools == 1 { "tool" } else { "tools" };
			let _ = writeln!(out, "{}: {} ({} {tools})", server.name, server.health, server.tools);
		}
	}

	if !info.lsp.is_empty() {
		out.push_str("\n**LSP servers**\n");
		for server in &info.lsp {
			let file_types = server
				.file_types
				.iter()
				.map(Str::as_str)
				.collect::<Vec<_>>()
				.join(", ");
			let stage = if server.stage.as_str() == "failed" {
				server
					.detail
					.as_ref()
					.map_or_else(|| server.stage.clone(), |detail| sf!("failed: {detail}"))
			} else {
				server.stage.clone()
			};
			let _ = writeln!(out, "{}: {stage} ({file_types})", server.name);
		}
	}

	Str::from(out)
}
#[cfg(test)]
mod tests {
	use omp_proto::inference::v1 as pb;

	use super::*;

	fn full_info() -> SessionInfo {
		SessionInfo {
			file:           Some(Str::new_static("/tmp/sessions/abc.jsonl")),
			model:          Str::new_static("claude-sonnet-4-5"),
			id:             Str::new_static("abc123"),
			title:          Some(Str::new_static("Port the parser")),
			provider:       Some(ProviderInfo {
				name:         Str::new_static("Anthropic"),
				model:        Str::new_static("claude-sonnet-4-5"),
				display_name: Str::new_static("Claude Sonnet 4.5"),
				api:          Str::new_static("anthropic-messages"),
				endpoint:     Str::new_static("https://api.anthropic.com/v1/messages"),
				auth:         Str::new_static("oauth (1 account)"),
			}),
			stats:          Some(SessionStatistics {
				user_messages:      4,
				assistant_messages: 5,
				system_messages:    1,
				tool_calls:         7,
				tool_results:       7,
				tool_errors:        2,
				usage:              pb::Usage {
					input_tokens: 1_000,
					output_tokens: 200,
					cache_read_tokens: 5_000,
					cache_write_tokens: 300,
					premium_requests: Some(3),
					..pb::Usage::default()
				},
				cost:               pb::Cost {
					nanos_usd: 12_345_000_000,
					estimated: true,
					..pb::Cost::default()
				},
				requests:           9,
				request_errors:     1,
				sessions:           3,
			}),
			context_tokens: 50_000,
			context_window: Some(200_000),
			queued:         2,
			mcp:            vec![McpServerInfo {
				name:   Str::new_static("linear"),
				health: "connected",
				tools:  12,
			}],
			lsp:            Vec::new(),
		}
	}

	#[test]
	fn full_report_renders_every_section_and_conditional_lines() {
		let rendered = render_info(&full_info());
		let text = rendered.as_str();
		assert!(text.contains("File: `/tmp/sessions/abc.jsonl`"));
		assert!(text.contains("Title: Port the parser"));
		assert!(text.contains("Model: `claude-sonnet-4-5` (Claude Sonnet 4.5)"));
		assert!(text.contains("Auth: oauth (1 account)"));
		assert!(text.contains("Tool results: 7 (2 errors)"));
		assert!(text.contains("Total: 24"), "message total sums every item kind");
		assert!(text.contains("Cache read: 5000"));
		assert!(text.contains("Total: 6500"), "token total sums all four counters");
		assert!(text.contains("Total: $12.3450 (estimated)"));
		assert!(text.contains("Requests: 9 (1 failed)"));
		assert!(text.contains("Premium requests: 3"));
		assert!(text.contains("Sessions: 3 (including subagents)"));
		assert!(text.contains("50000 / 200000 tokens (25%) · 2 queued"));
		assert!(text.contains("linear: connected (12 tools)"));
	}

	#[test]
	fn minimal_report_degrades_without_saved_file_stats_or_mcp() {
		let info = SessionInfo {
			file:           None,
			model:          Str::new_static("mystery-model"),
			id:             Str::new_static("fresh"),
			title:          None,
			provider:       None,
			stats:          None,
			context_tokens: 0,
			context_window: None,
			queued:         0,
			mcp:            Vec::new(),
			lsp:            Vec::new(),
		};
		let rendered = render_info(&info);
		let text = rendered.as_str();
		assert!(text.contains("File: in-memory (not saved yet)"));
		assert!(text.contains("Model: `mystery-model` (no catalog entry)"));
		assert!(!text.contains("Title:"));
		assert!(!text.contains("**Messages**"));
		assert!(!text.contains("**Tokens**"));
		assert!(!text.contains("**Cost**"));
		assert!(!text.contains("**MCP servers**"));
		assert!(!text.contains("**LSP servers**"));
		assert!(text.contains("0 tokens · 0 queued"));
	}

	#[test]
	fn lsp_report_renders_stages_file_types_and_failure_detail() {
		let mut info = full_info();
		info.lsp = vec![
			LspServerInfo {
				name:       Str::new_static("rust-analyzer"),
				stage:      Str::new_static("ready"),
				file_types: vec![Str::new_static("rs")],
				detail:     None,
			},
			LspServerInfo {
				name:       Str::new_static("typescript-language-server"),
				stage:      Str::new_static("failed"),
				file_types: vec![Str::new_static("ts"), Str::new_static("tsx")],
				detail:     Some(Str::new_static("binary not found")),
			},
		];

		let rendered = render_info(&info);
		assert!(rendered.contains("**LSP servers**"));
		assert!(rendered.contains("rust-analyzer: ready (rs)"));
		assert!(rendered.contains("typescript-language-server: failed: binary not found (ts, tsx)"));
	}

	#[test]
	fn lsp_report_is_omitted_when_roster_is_empty() {
		let rendered = render_info(&full_info());
		assert!(!rendered.contains("**LSP servers**"));
	}
}
