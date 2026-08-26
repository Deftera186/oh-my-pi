//! Structural context-control and execution-flow routes.

use super::command;

command!(compact, 410, "compact", icon: Compress, [], "Compact conversation context", [Context, Execution], false, typed("[soft|remote|snapcompact] [focus]", ["soft", "remote", "snapcompact"], parse_compact) => |host, request| host.compact(request));
command!(shake, 420, "shake", icon: Vibrate, [], "Reclaim replaceable context", [Context, Execution], false, raw("[elide|drop-media|thinking]", ["elide", "drop-media", "thinking"]) => |host, args| host.shake(args));
command!(usage, 430, "usage", icon: Throughput, [], "Show provider usage and limits", [Context], false, raw("[show|reset [account|active]]", ["show", "reset"]) => |host, args| host.usage(args));
command!(stats, 440, "stats", icon: ChartBar, [], "Launch the local stats dashboard", [Context], false, flags("[--host HOST] [--port PORT]", ["--host", "--port"]) => |host, flags| host.stats(flags));
command!(plan, 450, "plan", icon: Plan, [], "Control planning mode", [Execution], false, raw("[on|yolo|off|status|stop <activation>]", ["on", "yolo", "off", "status", "stop"]) => |host, args| host.plan(args));
command!(vibe, 451, "vibe", icon: Wave, [], "Control director/worker mode", [Execution], false, raw("[on|off|status|stop <activation>]", ["on", "off", "status", "stop"]) => |host, args| host.vibe(args));
command!(todo, 452, "todo", icon: Todo, [], "Inspect or update session tasks", [Session], false, typed("<subcommand>", ["edit", "copy", "expand", "collapse", "export [<path>]", "import [<path>]", "append [<phase>] <task...>", "start <task>", "done [<task|phase>]", "drop [<task|phase>]", "rm [<task|phase>]"], parse_raw) => |host, args| host.todo(args));
command!(plan_review, 460, "plan-review", icon: Plan, [], "Review the current plan", [Execution], false, raw("[args]", []) => |host, args| host.plan_review(args));
command!(goal, 470, "goal", icon: Goal, [], "Start or control a guided goal", [Execution], false, raw("[set|pause|resume|complete|drop|budget|status|stop]", ["set", "pause", "resume", "complete", "drop", "budget", "status", "stop"]) => |host, args| host.guided_goal(args));
command!(guided_goal, 471, "guided-goal", icon: Compass, [], "Have the agent interview you in chat, then set up goal mode", [Execution], false, raw("[rough objective]", []) => |host, args| host.guided_goal(args));
command!(loop_command, 480, "loop", icon: Loop, [], "Toggle bounded prompt repetition after each yield", [Execution], false, raw("[count|duration] [prompt]", []) => |host, args| host.loop_command(args));
command!(queue, 490, "queue", icon: Inbox, [], "Queue a message for after the agent yields", [Execution], false, required("<message>") => |host, prompt| host.queue(prompt));
command!(force, 500, "force", icon: Hammer, ["force:"], "Force next turn to use a specific tool", [Execution], false, raw("<tool-name> [prompt]", []) => |host, tool| host.force(tool));
command!(fast, 510, "fast", icon: Fast, [], "Toggle the priority service tier", [Model, Execution], false, raw("[on|off|status]", ["on", "off", "status"]) => |host, args| host.fast(args));
command!(prewalk, 520, "prewalk", icon: Prewalk, [], "Reason on a cheap model until the first mutation", [Model, Execution], false, raw("[on|off|status]", ["on", "off", "status"]) => |host, args| host.prewalk(args));
command!(btw, 530, "btw", icon: Question, [], "Ask an ephemeral side question using the current session context", [Execution], false, required("<question>") => |host, prompt| host.btw(prompt));
command!(tan, 540, "tan", icon: Rocket, [], "Run a full background agent on tangential work", [Execution], false, required("<work>") => |host, prompt| host.tan(prompt));
command!(omfg, 550, "omfg", icon: RuleExtension, [], "Forge a TTSR rule from a complaint to stop a recurring behavior", [Execution, Session], false, required("<complaint>") => |host, instruction| host.omfg(instruction));
command!(live, 560, "live", icon: Mic, [], "Toggle the live activity waveform", [Execution], false, raw("", []) => |host, args| host.live(args));

fn parse_compact(args: &str) -> miette::Result<omp_agent::ManualCompactionRequest> {
	omp_agent::ManualCompactionRequest::parse(args).map_err(|error| miette::miette!("{error}"))
}
