//! Structural durable-session and workspace routes.

use omp_core::Str;

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
