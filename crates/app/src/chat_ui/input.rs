use std::{
	borrow::Cow,
	collections::{BTreeMap, HashSet},
	error,
	fmt::{self, Display},
	iter, mem,
	sync::Arc,
};

use omp_agent::Budget;
use omp_core::{Str, sf};
use omp_proto::{
	inference::v1::TaskBudget,
	thread::v1::{Item, Message, Part, Role, item, part},
};
use omp_storage::index::{self, SessionIndex};
use omp_tui::{Command, Icon};
use parking_lot::RwLock;
use smallvec::SmallVec;

use super::now_ms;
const MANUAL_CONTINUE_PROMPT: &str =
	"<system-notice>\nContinue.\n\nMUST resume most recent intent; complete unfinished work.\nIf \
	 interrupted mid-step: resume where stopped.\nNEVER pause to summarize progress, re-confirm \
	 plan, or ask whether to proceed; continue.\n</system-notice>";

/// One declarative subcommand offered by completion and help.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubcommandSpec {
	/// Subcommand spelling.
	pub name:        &'static str,
	/// One-line explanation.
	pub description: &'static str,
	/// Positional usage shown after the spelling.
	pub usage:       &'static str,
}

/// Metadata shared by slash-command parsing, completion, help, and dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
	/// Command token without the leading slash.
	pub name:        &'static str,
	/// Alternate spellings, also without `/`.
	pub aliases:     &'static [&'static str],
	/// Human-readable completion and help text.
	pub description: &'static str,
	/// Optional argument hint appended by help and completion.
	pub usage:       &'static str,
	/// Declarative first-argument choices.
	pub subcommands: &'static [SubcommandSpec],
}

const TODO_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "show",
		description: "Show the current task list",
		usage:       "",
	},
	SubcommandSpec {
		name:        "append",
		description: "Append a task",
		usage:       "[phase] <task>",
	},
	SubcommandSpec { name: "start", description: "Start a task", usage: "<task>" },
	SubcommandSpec {
		name:        "done",
		description: "Complete a task or phase",
		usage:       "[task|phase]",
	},
	SubcommandSpec {
		name:        "drop",
		description: "Abandon a task or phase",
		usage:       "[task|phase]",
	},
];
const COMPACT_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "summary",
		description: "Summarize old context",
		usage:       "[focus]",
	},
	SubcommandSpec {
		name:        "prune",
		description: "Prune replaceable context",
		usage:       "[focus]",
	},
];
const PLAN_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec { name: "on", description: "Enter read-only planning", usage: "" },
	SubcommandSpec {
		name:        "yolo",
		description: "Plan until the first env-authorized mutation",
		usage:       "",
	},
	SubcommandSpec { name: "off", description: "Exit planning", usage: "" },
	SubcommandSpec {
		name:        "stop",
		description: "Stop a queued or active plan activation",
		usage:       "<activation>",
	},
	SubcommandSpec { name: "status", description: "Show active mode", usage: "" },
];
const GOAL_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "set",
		description: "Set an autonomous objective",
		usage:       "<objective> [token-budget]",
	},
	SubcommandSpec { name: "pause", description: "Pause goal continuation", usage: "" },
	SubcommandSpec {
		name:        "resume",
		description: "Resume goal continuation",
		usage:       "",
	},
	SubcommandSpec {
		name:        "complete",
		description: "Mark the objective complete",
		usage:       "",
	},
	SubcommandSpec { name: "drop", description: "Abandon the objective", usage: "" },
	SubcommandSpec {
		name:        "budget",
		description: "Replace the hard budget",
		usage:       "<tokens>",
	},
	SubcommandSpec {
		name:        "status",
		description: "Show goal status and spend",
		usage:       "",
	},
	SubcommandSpec {
		name:        "stop",
		description: "Stop a queued or active goal activation",
		usage:       "<activation>",
	},
];
const VIBE_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec { name: "on", description: "Enter director/worker mode", usage: "" },
	SubcommandSpec { name: "off", description: "Exit director/worker mode", usage: "" },
	SubcommandSpec {
		name:        "stop",
		description: "Stop a queued or active vibe activation",
		usage:       "<activation>",
	},
	SubcommandSpec { name: "status", description: "Show active mode", usage: "" },
];
const PREWALK_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "on",
		description: "Reason cheaply until first mutation",
		usage:       "",
	},
	SubcommandSpec { name: "off", description: "Disarm prewalk", usage: "" },
	SubcommandSpec { name: "status", description: "Show active mode", usage: "" },
];

/// Canonical builtin slash-command vocabulary.
///
/// This table is deliberately the sole builtin name authority: autocomplete,
/// help, reserved-name filtering, and parsing all consume it.
pub const COMMANDS: &[CommandSpec] = &[
	CommandSpec {
		name:        "help",
		aliases:     &["hotkeys"],
		description: "Show commands and keyboard controls",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "login",
		aliases:     &[],
		description: "Authenticate a provider",
		usage:       "[provider]",
		subcommands: &[],
	},
	CommandSpec {
		name:        "model",
		aliases:     &["models"],
		description: "Change the durable default model",
		usage:       "[model]",
		subcommands: &[],
	},
	CommandSpec {
		name:        "switch",
		aliases:     &[],
		description: "Temporarily change this session's model",
		usage:       "[model]",
		subcommands: &[],
	},
	CommandSpec {
		name:        "resume",
		aliases:     &[],
		description: "Open another project session",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "new",
		aliases:     &[],
		description: "Start a new session",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "clear",
		aliases:     &[],
		description: "Clear conversation context in place",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "fresh",
		aliases:     &[],
		description: "Reset provider account affinity for the next turn",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "compact",
		aliases:     &[],
		description: "Compact conversation context",
		usage:       "[summary|prune] [focus]",
		subcommands: COMPACT_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "todo",
		aliases:     &[],
		description: "Inspect or update session tasks",
		usage:       "[subcommand]",
		subcommands: TODO_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "plan",
		aliases:     &[],
		description: "Control read-only plan mode",
		usage:       "[on|yolo|off|status|stop <activation>]",
		subcommands: PLAN_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "goal",
		aliases:     &[],
		description: "Control an autonomous objective",
		usage:       "[set|pause|resume|complete|drop|budget|status|stop <activation>]",
		subcommands: GOAL_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "vibe",
		aliases:     &[],
		description: "Control director/worker vibe mode",
		usage:       "[on|off|status|stop <activation>]",
		subcommands: VIBE_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "prewalk",
		aliases:     &[],
		description: "Control reason-first prewalk",
		usage:       "[on|off|status]",
		subcommands: PREWALK_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "jobs",
		aliases:     &[],
		description: "List active background jobs",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "settings",
		aliases:     &[],
		description: "Open settings",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "theme",
		aliases:     &[],
		description: "Preview a JSON theme without committing settings",
		usage:       "<environment-path> [256]",
		subcommands: &[],
	},
	CommandSpec {
		name:        "agents",
		aliases:     &[],
		description: "Open the live agent hierarchy",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "pause",
		aliases:     &[],
		description: "Pause the interactive session",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "live",
		aliases:     &[],
		description: "Toggle the firehose activity waveform",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "quit",
		aliases:     &["exit", "q"],
		description: "Exit the application",
		usage:       "",
		subcommands: &[],
	},
];

/// One command contributed by a live discovery provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandContribution {
	/// Primary spelling without `/`.
	pub name:        Str,
	/// Alternate spellings.
	pub aliases:     SmallVec<Str, 2>,
	/// One-line description.
	pub description: Str,
	/// Inline argument hint.
	pub hint:        Option<Str>,
	/// Human-readable discovery source label.
	pub origin:      Str,
	/// Optional prompt template dispatched when this command is submitted.
	pub template:    Option<Str>,
}
impl From<omp_driver::discovery::CommandContribution> for CommandContribution {
	fn from(value: omp_driver::discovery::CommandContribution) -> Self {
		Self {
			name:        value.name,
			aliases:     value.aliases.into_iter().collect(),
			description: value.description,
			hint:        value.hint,
			origin:      value.origin,
			template:    value.template,
		}
	}
}

const INIT_WORKFLOW_TEMPLATE: &str = r#"Use parallel `task` research agents for independent slices of the repository: core source, tests, configuration/build, and scripts/documentation. Synthesize their findings into one AGENTS.md.

The document MUST:
- be titled "Repository Guidelines" and use Markdown headings;
- concisely explain project purpose, architecture and data flow, key directories, development commands, code conventions, important files, runtime/tooling preferences, and testing/QA;
- include useful commands, paths, naming patterns, and architecture-specific guidance;
- omit facts that are obvious from the directory tree.

After analysis, write AGENTS.md to the project root."#;

/// Returns native workflow templates used only when no discovered command
/// claims the same name.
pub fn embedded_workflow_commands() -> [CommandContribution; 1] {
	[CommandContribution {
		name:        sf!("init"),
		aliases:     SmallVec::new(),
		description: sf!("Generate AGENTS.md for the current codebase"),
		hint:        None,
		origin:      sf!("Bundled OMP workflow"),
		template:    Some(sf!(INIT_WORKFLOW_TEMPLATE)),
	}]
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AvailableCommand {
	name:        Str,
	aliases:     SmallVec<Str, 2>,
	description: Str,
	hint:        Option<Str>,
	origin:      Str,
	template:    Option<Str>,
	builtin:     bool,
}

impl From<CommandContribution> for AvailableCommand {
	fn from(command: CommandContribution) -> Self {
		Self {
			name:        command.name,
			aliases:     command.aliases,
			description: command.description,
			hint:        command.hint,
			origin:      command.origin,
			template:    command.template,
			builtin:     false,
		}
	}
}

/// Live first-source-wins command roster shared by completion and dispatch.
pub struct CommandRoster {
	available: Vec<AvailableCommand>,
}
/// Process-local slash-command counts backed by the authoritative session
/// index.
pub struct CommandUsage {
	index:  Arc<SessionIndex>,
	counts: RwLock<BTreeMap<Str, u64>>,
}

impl CommandUsage {
	/// Loads persisted counts from `index` for synchronous completion ranking.
	pub fn load(index: Arc<SessionIndex>) -> Result<Self, index::Error> {
		let counts = index.command_usage()?;
		Ok(Self { index, counts: RwLock::new(counts) })
	}

	/// Returns the persisted invocation count for `name`.
	pub fn count(&self, name: &str) -> u64 {
		self.counts.read().get(name).copied().unwrap_or_default()
	}

	/// Persists one invocation and updates the process-local ranking snapshot.
	pub fn record(&self, name: &str, now_ms: u64) -> Result<(), index::Error> {
		self.index.record_command_usage(name, now_ms)?;
		let mut counts = self.counts.write();
		let count = counts.entry(Str::new(name)).or_default();
		*count = count.saturating_add(1);
		Ok(())
	}
}

impl CommandRoster {
	/// Aggregates builtins followed by provider feeds in precedence order.
	pub fn new(sources: Vec<Vec<CommandContribution>>) -> Self {
		let mut ordered = Vec::with_capacity(sources.len().saturating_add(1));
		ordered.push(builtin_available());
		ordered.extend(sources.into_iter().map(|source| {
			source
				.into_iter()
				.map(AvailableCommand::from)
				.collect::<Vec<_>>()
		}));
		Self { available: aggregate_commands(ordered) }
	}

	/// Slash commands offered by the chat composer's completion palette.
	pub fn completions(&self) -> Vec<Command> {
		self.available.iter().map(to_completion).collect()
	}

	/// Parses builtin and provider-contributed slash commands.
	pub fn parse_input(&self, text: &str) -> Result<ChatCommand, InputError> {
		parse_input(text, &self.available)
	}

	/// Resolves submitted slash input to the canonical name used for frequency
	/// ranking.
	pub fn command_usage_name(&self, text: &str) -> Option<Str> {
		let parsed = parse_slash(text)?;
		self
			.available
			.iter()
			.find(|command| {
				command.name == parsed.name || command.aliases.iter().any(|alias| alias == parsed.name)
			})
			.map(|command| command.name.clone())
	}

	/// Renders help from the same winning roster used by completion and
	/// dispatch.
	pub fn help_text(&self) -> String {
		render_help(&self.available)
	}
}

/// Aggregates command sources in caller order. The first spelling wins;
/// builtin names, aliases, and colon namespaces are always reserved.
fn aggregate_commands(
	sources: impl IntoIterator<Item = impl IntoIterator<Item = AvailableCommand>>,
) -> Vec<AvailableCommand> {
	let reserved = reserved_names();
	let mut claimed = HashSet::<Str>::new();
	let mut available = Vec::new();
	for source in sources {
		for mut command in source {
			let shadows = |candidate: &Str| {
				reserved.iter().any(|name| {
					candidate == name
						|| candidate
							.strip_prefix(name.as_str())
							.is_some_and(|rest| rest.starts_with(':'))
				})
			};
			let shadowed_builtin =
				!command.builtin && (shadows(&command.name) || command.aliases.iter().any(shadows));
			if shadowed_builtin || claimed.contains(&command.name) {
				continue;
			}
			command.aliases.retain(|alias| !claimed.contains(alias));
			claimed.insert(command.name.clone());
			for alias in &command.aliases {
				claimed.insert(alias.clone());
			}
			available.push(command);
		}
	}
	available
}

fn builtin_available() -> Vec<AvailableCommand> {
	COMMANDS
		.iter()
		.map(|spec| AvailableCommand {
			name:        sf!(spec.name),
			aliases:     spec.aliases.iter().copied().map(Str::new_static).collect(),
			description: sf!(spec.description),
			hint:        (!spec.usage.is_empty()).then(|| sf!(spec.usage)),
			origin:      sf!("builtin"),
			template:    None,
			builtin:     true,
		})
		.collect()
}

fn reserved_names() -> HashSet<Str> {
	COMMANDS
		.iter()
		.flat_map(|spec| iter::once(spec.name).chain(spec.aliases.iter().copied()))
		.map(Str::new)
		.collect()
}

fn to_completion(available: &AvailableCommand) -> Command {
	let aliases: SmallVec<&str, 2> = available.aliases.iter().map(Str::as_str).collect();
	let mut command =
		Command::new(available.name.as_str(), available.description.as_str(), &aliases)
			.with_icon(command_icon(available));
	if let Some(spec) = COMMANDS
		.iter()
		.find(|spec| spec.name == available.name.as_str())
		&& !spec.subcommands.is_empty()
	{
		let args: Vec<_> = spec
			.subcommands
			.iter()
			.map(|sub| (sub.name, sub.description, sub.usage))
			.collect();
		command = command.with_args(&args);
	}
	if let Some(hint) = &available.hint {
		command = command.with_hint(hint);
	}
	command
}

fn command_icon(command: &AvailableCommand) -> Icon {
	let origin = command.origin.to_ascii_lowercase();
	if command.name == "mcp" || origin.contains("mcp") {
		Icon::McpExtension
	} else if command.name.starts_with("skill:") {
		Icon::Skill
	} else if command.builtin && matches!(command.name.as_str(), "resume" | "new" | "clear") {
		Icon::Session
	} else if command.builtin {
		Icon::SlashCommand
	} else if origin.contains("extension") {
		Icon::ExtensionCommand
	} else {
		Icon::Prompt
	}
}

fn render_help(available: &[AvailableCommand]) -> String {
	let mut help = String::from("**Commands**\n");
	for command in available {
		help.push_str("- `/");
		help.push_str(command.name.as_str());
		if let Some(hint) = &command.hint {
			help.push(' ');
			help.push_str(hint.as_str());
		}
		help.push_str("` — ");
		help.push_str(command.description.as_str());
		if !command.aliases.is_empty() {
			help.push_str(" (aliases: ");
			for (index, alias) in command.aliases.iter().enumerate() {
				if index != 0 {
					help.push_str(", ");
				}
				help.push('/');
				help.push_str(alias.as_str());
			}
			help.push(')');
		}
		if !command.builtin {
			help.push_str(" via ");
			help.push_str(command.origin.as_str());
		}
		help.push('\n');
	}
	help.push_str("\n**Keys**\nesc interrupt · esc esc rewind · enter steer · alt+enter follow-up");
	help
}

/// Actions parsed from user input in the chat shell.
#[derive(Debug, PartialEq)]
pub enum ChatCommand {
	/// Ignore an empty composer submission.
	Nothing,
	/// Show the command and key reference.
	Help,
	/// Start provider authentication.
	Login(Option<Str>),
	/// Update the durable model preference.
	Model(Str),
	/// Apply a journaled session-only model override.
	Switch(Str),
	/// Open the catalog model picker.
	ModelPicker,
	/// Open the fullscreen models hub.
	ModelHub,
	/// Open the durable-session picker.
	Resume,
	/// Start a new durable session.
	NewSession,
	/// Append a same-journal context reset.
	Clear,
	/// Append a provider-account reset hint without changing journal identity.
	Fresh,
	/// List active background jobs.
	Jobs,
	/// Open settings.
	Settings,
	/// Preview a JSON theme read through the Environment.
	Theme(Str),
	/// Open the agent hierarchy.
	Agents,
	/// Pause the interactive host.
	Pause,
	/// Toggle the firehose-backed activity waveform.
	Live,
	/// Control plan mode with raw subcommand arguments.
	Plan(Str),
	/// Control goal mode with raw subcommand arguments.
	Goal(Str),
	/// Control vibe mode with raw subcommand arguments.
	Vibe(Str),
	/// Control prewalk mode with raw subcommand arguments.
	Prewalk(Str),
	/// Invoke one frozen session skill with surrounding user prose as arguments.
	Skill {
		/// Stable skill name.
		name:            Str,
		/// User prose outside the invocation token.
		args:            Str,
		/// Optional per-turn budget parsed from surrounding prose.
		budget:          Option<ParsedTurnBudget>,
		/// Original local submission time before command preprocessing.
		submitted_at_ms: u64,
	},
	/// A recognized command whose backend is not available yet.
	Unavailable { command: Str, reason: Str },
	/// Exit cleanly.
	Quit,
	/// A normal prompt, including unknown slash input which must pass through.
	Submit {
		/// Canonical user item with the budget prefix removed.
		item:   Box<Item>,
		/// Visible prompt text with the budget prefix removed.
		text:   Str,
		/// Optional per-turn advisory or hard token budget.
		budget: Option<ParsedTurnBudget>,
	},
}

/// Parsed `+Nk` or `+Nk!` turn-budget directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedTurnBudget {
	/// Agent-side output-token budget represented in the landed budget type.
	pub agent: Budget,
	/// Provider task-budget representation used by the turn parameters.
	pub task:  TaskBudget,
	/// `true` for the hard `!` form; advisory budgets remain visible but do not
	/// reject work.
	pub hard:  bool,
}

/// Parsed `/name args` token. The delimiter is the earliest whitespace or `:`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParsedSlash<'a> {
	/// Command name without `/`.
	pub name: &'a str,
	/// Trimmed raw arguments after the delimiter.
	pub args: &'a str,
}

/// Dynamic slash-command argument query projected from text up to the cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgCompletionQuery {
	/// Token currently being completed.
	pub prefix: Str,
	/// Fully entered arguments preceding `prefix`.
	pub argv:   Vec<Str>,
}

/// Parses a syntactically command-shaped line. Paths containing another `/`
/// and ordinary prompt text return `None` for model passthrough.
pub fn parse_slash(text: &str) -> Option<ParsedSlash<'_>> {
	let text = text.trim();
	let body = text.strip_prefix('/')?;
	let delimiter = body
		.char_indices()
		.find(|(_, ch)| ch.is_whitespace() || *ch == ':')
		.map_or(body.len(), |(at, _)| at);
	let name = &body[..delimiter];
	if name.is_empty() || name.contains('/') {
		return None;
	}
	let args = body[delimiter..].trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':');
	Some(ParsedSlash { name, args })
}

/// Structured parsing failure for quote-aware command arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
	/// A quoted argument was not terminated.
	UnterminatedQuote,
	/// A built-in command requires a non-empty argument.
	MissingArgument {
		/// Command missing its required argument.
		command: Str,
	},
	/// A `+Nk` budget prefix used zero, overflowed, or was malformed.
	InvalidBudget,
}

impl Display for InputError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::UnterminatedQuote => f.write_str("unterminated quoted command argument"),
			Self::MissingArgument { command } => write!(f, "/{command} requires an argument"),
			Self::InvalidBudget => {
				f.write_str("turn budget must use a positive +N, +Nk, or +Nm directive")
			},
		}
	}
}
impl error::Error for InputError {}

/// Parses composer text against the same aggregated roster used for completion.
/// Unknown slash names intentionally pass through as normal prompt text.
fn parse_input(text: &str, available: &[AvailableCommand]) -> Result<ChatCommand, InputError> {
	let submitted_at_ms = now_ms();
	let (text, budget) = parse_budget_prefix(text)?;
	if text.trim().is_empty() {
		return if budget.is_some() {
			Err(InputError::InvalidBudget)
		} else {
			Ok(ChatCommand::Nothing)
		};
	}
	let submit = |text: &str| ChatCommand::Submit {
		item:   Box::new(user_message_at(text, submitted_at_ms)),
		text:   Str::from(text),
		budget: budget.clone(),
	};
	if matches!(&*text, "." | "c") {
		return Ok(ChatCommand::Submit {
			item: Box::new(manual_continue_message_at(submitted_at_ms)),
			text: Str::from(text),
			budget,
		});
	}
	if let Some(skill) = omp_driver::skills::parse_invocation(&text) {
		return Ok(ChatCommand::Skill {
			name: skill.name,
			args: skill.args,
			budget,
			submitted_at_ms,
		});
	}
	let Some(parsed) = parse_slash(&text) else {
		return Ok(submit(&text));
	};
	let Some(available) = available.iter().find(|command| {
		command.name == parsed.name || command.aliases.iter().any(|alias| alias == parsed.name)
	}) else {
		return Ok(submit(&text));
	};
	if !available.builtin {
		let Some(template) = &available.template else {
			return Ok(submit(&text));
		};
		let args = tokenize_args(parsed.args)?;
		let expanded = expand_arguments_with_fallback(template, &args, parsed.args);
		return Ok(ChatCommand::Submit {
			item: Box::new(user_message_at(&expanded, submitted_at_ms)),
			text: Str::from(expanded),
			budget,
		});
	}
	let spec = COMMANDS
		.iter()
		.find(|spec| spec.name == available.name.as_str())
		.expect("aggregated builtin must retain its declaration");
	let command = match spec.name {
		"help" => ChatCommand::Help,
		"login" => ChatCommand::Login((!parsed.args.is_empty()).then(|| Str::from(parsed.args))),
		"model" if parsed.args.is_empty() => ChatCommand::ModelHub,
		"model" => ChatCommand::Model(Str::from(parsed.args)),
		"switch" if !parsed.args.is_empty() => ChatCommand::Switch(Str::from(parsed.args)),
		"switch" => ChatCommand::ModelPicker,
		"resume" => ChatCommand::Resume,
		"new" => ChatCommand::NewSession,
		"clear" => ChatCommand::Clear,
		"fresh" => ChatCommand::Fresh,
		"jobs" => ChatCommand::Jobs,
		"settings" => ChatCommand::Settings,
		"theme" if !parsed.args.is_empty() => ChatCommand::Theme(Str::from(parsed.args)),
		"theme" => return Err(InputError::MissingArgument { command: sf!("theme") }),
		"agents" => ChatCommand::Agents,
		"pause" => ChatCommand::Pause,
		"live" => ChatCommand::Live,
		"plan" => ChatCommand::Plan(Str::from(parsed.args)),
		"goal" => ChatCommand::Goal(Str::from(parsed.args)),
		"vibe" => ChatCommand::Vibe(Str::from(parsed.args)),
		"prewalk" => ChatCommand::Prewalk(Str::from(parsed.args)),
		"quit" => ChatCommand::Quit,
		"compact" => unavailable("compact", "manual compaction is not exposed by the agent backend"),
		"todo" => unavailable("todo", "interactive todo storage is not attached to this session"),
		_ => unreachable!("every builtin has a dispatch arm"),
	};
	Ok(command)
}

/// Finds and removes one standalone `+N`, `+Nk`, or `+Nm` (`!`) directive.
///
/// The directive may occur anywhere at whitespace token boundaries. Fractional
/// values are rounded after applying the suffix multiplier; all surrounding
/// prose is retained.
pub fn parse_budget_prefix(
	text: &str,
) -> Result<(Cow<'_, str>, Option<ParsedTurnBudget>), InputError> {
	let mut start = 0;
	for token in text.split_inclusive(char::is_whitespace) {
		let bare = token.trim_end_matches(char::is_whitespace);
		if let Some((tokens, hard)) = parse_budget_token(bare)? {
			let end = start + bare.len();
			let mut prompt = String::with_capacity(text.len().saturating_sub(bare.len()));
			prompt.push_str(&text[..start]);
			let suffix = &text[end..];
			if prompt.ends_with(char::is_whitespace) && suffix.starts_with(char::is_whitespace) {
				prompt.push_str(suffix.trim_start_matches(char::is_whitespace));
			} else {
				prompt.push_str(suffix);
			}
			let prompt = prompt.trim().to_owned();
			return Ok((
				Cow::Owned(prompt),
				Some(ParsedTurnBudget {
					agent: Budget { max_output_tokens: Some(tokens), ..Budget::default() },
					task: TaskBudget {
						total_tokens:     tokens,
						remaining_tokens: hard.then_some(tokens),
					},
					hard,
				}),
			));
		}
		start += token.len();
	}
	Ok((Cow::Borrowed(text), None))
}

fn parse_budget_token(token: &str) -> Result<Option<(u64, bool)>, InputError> {
	let Some(body) = token.strip_prefix('+') else {
		return Ok(None);
	};
	let (body, hard) = body
		.strip_suffix('!')
		.map_or((body, false), |body| (body, true));
	let (number, multiplier) = match body.as_bytes().last().copied() {
		Some(b'k' | b'K') => (&body[..body.len() - 1], 1_000_f64),
		Some(b'm' | b'M') => (&body[..body.len() - 1], 1_000_000_f64),
		_ => (body, 1_f64),
	};
	if number.is_empty()
		|| number.bytes().filter(|byte| *byte == b'.').count() > 1
		|| !number
			.bytes()
			.all(|byte| byte.is_ascii_digit() || byte == b'.')
	{
		return Ok(None);
	}
	let value = number
		.parse::<f64>()
		.map_err(|_| InputError::InvalidBudget)?;
	let scaled = value * multiplier;
	if !scaled.is_finite() || scaled <= 0.0 || scaled > u64::MAX as f64 {
		return Err(InputError::InvalidBudget);
	}
	let tokens = scaled.round() as u64;
	if tokens == 0 {
		return Err(InputError::InvalidBudget);
	}
	Ok(Some((tokens, hard)))
}

const fn unavailable(command: &'static str, reason: &'static str) -> ChatCommand {
	ChatCommand::Unavailable { command: sf!(command), reason: sf!(reason) }
}

/// Splits arguments on unquoted whitespace. Single/double quotes group values;
/// backslash quotes the following scalar. Quote characters are not retained.
pub fn tokenize_args(raw: &str) -> Result<Vec<Str>, InputError> {
	let mut args = Vec::new();
	let mut current = String::new();
	let mut quote = None;
	let mut escaped = false;
	for ch in raw.chars() {
		if escaped {
			current.push(ch);
			escaped = false;
			continue;
		}
		if ch == '\\' {
			escaped = true;
			continue;
		}
		if let Some(open) = quote {
			if ch == open {
				quote = None;
			} else {
				current.push(ch);
			}
			continue;
		}
		if ch == '\'' || ch == '"' {
			quote = Some(ch);
		} else if ch.is_whitespace() {
			if !current.is_empty() {
				args.push(Str::from(mem::take(&mut current)));
			}
		} else {
			current.push(ch);
		}
	}
	if escaped {
		current.push('\\');
	}
	if quote.is_some() {
		return Err(InputError::UnterminatedQuote);
	}
	if !current.is_empty() {
		args.push(Str::from(current));
	}
	Ok(args)
}
/// Splits a partial argument line into completed arguments and the token under
/// the cursor. Unterminated quotes are accepted because completion runs before
/// submission validation.
pub fn completion_arg_query(raw: &str) -> ArgCompletionQuery {
	let mut argv = Vec::new();
	let mut current = String::new();
	let mut quote = None;
	let mut escaped = false;
	let mut ended_on_whitespace = false;
	for ch in raw.chars() {
		if escaped {
			current.push(ch);
			escaped = false;
			ended_on_whitespace = false;
			continue;
		}
		if ch == '\\' {
			escaped = true;
			ended_on_whitespace = false;
			continue;
		}
		if let Some(open) = quote {
			if ch == open {
				quote = None;
			} else {
				current.push(ch);
			}
			ended_on_whitespace = false;
			continue;
		}
		if ch == '\'' || ch == '"' {
			quote = Some(ch);
			ended_on_whitespace = false;
		} else if ch.is_whitespace() {
			if !current.is_empty() {
				argv.push(Str::from(mem::take(&mut current)));
			}
			ended_on_whitespace = true;
		} else {
			current.push(ch);
			ended_on_whitespace = false;
		}
	}
	if escaped {
		current.push('\\');
		ended_on_whitespace = false;
	}
	if ended_on_whitespace {
		ArgCompletionQuery { prefix: Str::default(), argv }
	} else {
		ArgCompletionQuery { prefix: Str::from(current), argv }
	}
}

/// Expands `$1`, `$2`, `$@`, `$@[start:length]`, and `$ARGUMENTS` once.
///
/// Positional substitutions use tokenized arguments while aggregate forms
/// preserve their canonical joined spelling.
/// Slice starts are one-based. An omitted length (including a trailing colon)
/// selects through the final argument. Non-positive and out-of-range slices
/// expand to empty text. Values are never scanned again, preventing recursive
/// substitution.
pub fn expand_arguments(template: &str, args: &[Str]) -> String {
	let joined = args.iter().map(Str::as_str).collect::<Vec<_>>().join(" ");
	expand_arguments_with_fallback(template, args, &joined)
}

pub(crate) fn expand_arguments_with_fallback(
	template: &str,
	args: &[Str],
	fallback: &str,
) -> String {
	let joined = args.iter().map(Str::as_str).collect::<Vec<_>>().join(" ");
	let mut expanded = String::with_capacity(template.len().saturating_add(joined.len()));
	let bytes = template.as_bytes();
	let mut at = 0;
	let mut substituted = false;
	while at < bytes.len() {
		if bytes[at] != b'$' {
			let ch = template[at..]
				.chars()
				.next()
				.expect("valid scalar boundary");
			expanded.push(ch);
			at += ch.len_utf8();
			continue;
		}
		if template[at..].starts_with("$ARGUMENTS") {
			substituted = true;
			expanded.push_str(&joined);
			at += "$ARGUMENTS".len();
			continue;
		}
		if let Some((end, start, length)) = argument_slice(&template[at..]) {
			substituted = true;
			if start > 0 {
				let start = start - 1;
				if start < args.len() {
					let end =
						length.map_or(args.len(), |length| start.saturating_add(length).min(args.len()));
					for (index, value) in args[start..end].iter().enumerate() {
						if index > 0 {
							expanded.push(' ');
						}
						expanded.push_str(value);
					}
				}
			}
			at += end;
			continue;
		}
		if template[at..].starts_with("$@") {
			substituted = true;
			expanded.push_str(&joined);
			at += 2;
			continue;
		}
		let mut end = at + 1;
		while end < bytes.len() && bytes[end].is_ascii_digit() {
			end += 1;
		}
		if end > at + 1 {
			substituted = true;
			let index = template[at + 1..end].parse::<usize>().unwrap_or(0);
			if index > 0
				&& let Some(value) = args.get(index - 1)
			{
				expanded.push_str(value);
			}
			at = end;
			continue;
		}
		expanded.push('$');
		at += 1;
	}
	if !substituted && !fallback.is_empty() {
		if !expanded.is_empty() && !expanded.ends_with(char::is_whitespace) {
			expanded.push(' ');
		}
		expanded.push_str(fallback);
	}
	expanded
}

fn argument_slice(template: &str) -> Option<(usize, usize, Option<usize>)> {
	let body = template.strip_prefix("$@[")?;
	let close = body.find(']')?;
	let selector = &body[..close];
	let (start, length) = match selector.split_once(':') {
		Some((start, "")) => (start, None),
		Some((start, length)) => (start, Some(length.parse().ok()?)),
		None => (selector, None),
	};
	if start.is_empty() || !start.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	Some((3 + close + 1, start.parse().ok()?, length))
}

/// Builds the canonical user-message item used by submissions and steering.
pub(super) fn user_message(text: impl Into<String>) -> Item {
	user_message_at(text, now_ms())
}

/// Builds a canonical user message with a caller-captured submission time.
pub(super) fn user_message_at(text: impl Into<String>, submitted_at_ms: u64) -> Item {
	Item {
		seq:           0,
		created_at_ms: submitted_at_ms,
		kind:          Some(item::Kind::Message(Message {
			role: i32::from(Role::User),
			parts: vec![Part { kind: Some(part::Kind::Text(text.into())) }],
			..Message::default()
		})),
		props:         None,
	}
}

fn manual_continue_message_at(submitted_at_ms: u64) -> Item {
	Item {
		seq:           0,
		created_at_ms: submitted_at_ms,
		kind:          Some(item::Kind::Message(Message {
			role: i32::from(Role::System),
			parts: vec![Part { kind: Some(part::Kind::Text(MANUAL_CONTINUE_PROMPT.to_owned())) }],
			synthetic: Some(true),
			user_initiated: Some(true),
			..Message::default()
		})),
		props:         None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn submit_text(command: ChatCommand) -> String {
		let ChatCommand::Submit { item, .. } = command else {
			panic!("expected passthrough submit")
		};
		let Some(item::Kind::Message(message)) = item.kind else {
			panic!("missing message")
		};
		let Some(part::Kind::Text(text)) = message.parts[0].kind.clone() else {
			panic!("missing text")
		};
		text
	}

	fn builtins() -> CommandRoster {
		CommandRoster::new(Vec::new())
	}

	fn contribution(
		name: &'static str,
		description: &'static str,
		origin: &'static str,
		template: &'static str,
	) -> CommandContribution {
		CommandContribution {
			name:        sf!(name),
			aliases:     SmallVec::new(),
			description: sf!(description),
			hint:        None,
			origin:      sf!(origin),
			template:    Some(sf!(template)),
		}
	}

	#[test]
	fn parses_whitespace_colon_aliases_and_passthrough() {
		let commands = builtins();
		assert_eq!(commands.parse_input("/live"), Ok(ChatCommand::Live));
		assert_eq!(parse_slash("/model: smol"), Some(ParsedSlash { name: "model", args: "smol" }));
		assert_eq!(commands.parse_input("/model:smol"), Ok(ChatCommand::Model(sf!("smol"))));
		assert_eq!(commands.parse_input("/model"), Ok(ChatCommand::ModelHub));
		assert_eq!(commands.parse_input("/switch"), Ok(ChatCommand::ModelPicker));
		assert_eq!(
			commands.parse_input("/switch anthropic/opus"),
			Ok(ChatCommand::Switch(sf!("anthropic/opus")))
		);
		assert_eq!(commands.parse_input("/q"), Ok(ChatCommand::Quit));
		assert_eq!(submit_text(commands.parse_input("/unknown arg").unwrap()), "/unknown arg");
		assert_eq!(
			submit_text(commands.parse_input("/tmp/pic.png describe").unwrap()),
			"/tmp/pic.png describe"
		);
	}

	#[test]
	fn continue_shortcuts_build_user_initiated_synthetic_developer_items() {
		let commands = builtins();
		for shortcut in [".", "c"] {
			let ChatCommand::Submit { item, text, .. } =
				commands.parse_input(shortcut).expect("continue shortcut")
			else {
				panic!("continue shortcut must submit");
			};
			let Some(item::Kind::Message(message)) = item.kind else {
				panic!("continue shortcut message");
			};
			assert_eq!(text, shortcut);
			assert!(item.created_at_ms > 0);
			assert_eq!(message.role(), Role::System);
			assert_eq!(message.synthetic, Some(true));
			assert_eq!(message.user_initiated, Some(true));
			assert!(matches!(
				message.parts.as_slice(),
				[Part { kind: Some(part::Kind::Text(body)) }] if body.contains("Continue.")
			));
		}
	}

	#[test]
	fn parses_execution_mode_commands_without_submitting_text() {
		let commands = builtins();
		assert_eq!(commands.parse_input("/plan yolo"), Ok(ChatCommand::Plan(sf!("yolo"))));
		assert_eq!(
			commands.parse_input("/plan stop activation-1"),
			Ok(ChatCommand::Plan(sf!("stop activation-1")))
		);
		assert_eq!(
			commands.parse_input("/goal set finish migration 12000"),
			Ok(ChatCommand::Goal(sf!("set finish migration 12000")))
		);
		assert_eq!(commands.parse_input("/vibe on"), Ok(ChatCommand::Vibe(sf!("on"))));
		assert_eq!(
			commands.parse_input("/goal stop activation-2"),
			Ok(ChatCommand::Goal(sf!("stop activation-2")))
		);
		assert_eq!(
			commands.parse_input("/vibe stop activation-3"),
			Ok(ChatCommand::Vibe(sf!("stop activation-3")))
		);
		assert_eq!(commands.parse_input("/prewalk status"), Ok(ChatCommand::Prewalk(sf!("status"))));
	}
	#[test]
	fn usage_names_canonicalize_aliases_and_reject_unknown_commands() {
		let commands = CommandRoster::new(vec![vec![contribution(
			"review",
			"Review changes",
			"extension",
			"Review $ARGUMENTS",
		)]]);
		assert_eq!(commands.command_usage_name("/q"), Some(sf!("quit")));
		assert_eq!(commands.command_usage_name("/review focused"), Some(sf!("review")));
		assert_eq!(commands.command_usage_name("/unknown"), None);
	}
	#[test]
	fn command_usage_ranking_state_survives_restart() {
		let directory = tempfile::tempdir().expect("temporary directory");
		let path = directory.path().join("sessions.sqlite3");
		{
			let index = Arc::new(SessionIndex::open(&path).expect("open session index"));
			let usage = CommandUsage::load(index).expect("load command usage");
			usage.record("model", 1_000).expect("record model");
			usage.record("model", 2_000).expect("record model again");
			usage.record("skill:rust", 3_000).expect("record skill");
			assert_eq!(usage.count("model"), 2);
		}

		let index = Arc::new(SessionIndex::open(&path).expect("reopen session index"));
		let restarted = CommandUsage::load(index).expect("reload command usage");
		assert_eq!(restarted.count("model"), 2);
		assert_eq!(restarted.count("skill:rust"), 1);
	}

	#[test]
	fn completion_types_use_semantic_catalog_icons() {
		let builtin = AvailableCommand {
			name:        sf!("help"),
			aliases:     SmallVec::new(),
			description: sf!("help"),
			hint:        None,
			origin:      sf!("builtin"),
			template:    None,
			builtin:     true,
		};
		let session = AvailableCommand { name: sf!("resume"), ..builtin.clone() };
		let mcp = AvailableCommand { name: sf!("mcp"), ..builtin.clone() };
		let prompt = AvailableCommand {
			name: sf!("prompt"),
			origin: sf!("project"),
			builtin: false,
			..builtin.clone()
		};
		let extension = AvailableCommand {
			name: sf!("extension"),
			origin: sf!("extension"),
			builtin: false,
			..builtin.clone()
		};
		let skill = AvailableCommand {
			name: sf!("skill:rust"),
			origin: sf!("skill"),
			builtin: false,
			..builtin.clone()
		};
		assert_eq!(command_icon(&builtin), Icon::SlashCommand);
		assert_eq!(command_icon(&session), Icon::Session);
		assert_eq!(command_icon(&mcp), Icon::McpExtension);
		assert_eq!(command_icon(&prompt), Icon::Prompt);
		assert_eq!(command_icon(&extension), Icon::ExtensionCommand);
		assert_eq!(command_icon(&skill), Icon::Skill);
	}

	#[test]
	fn completion_help_and_reserved_names_share_one_live_roster() {
		let commands = CommandRoster::new(vec![
			vec![
				contribution("model", "shadow", "extension", "Shadow $1"),
				contribution("model:secret", "shadow namespace", "extension", "Shadow $1"),
				contribution("review", "first", "extension", "Review $1"),
			],
			vec![contribution("review", "second", "file", "Second $1")],
		]);
		let completed = commands.completions();
		let help = commands.help_text();
		for spec in COMMANDS {
			assert!(completed.iter().any(|command| command.name() == spec.name));
			assert!(help.contains(&format!("/{}", spec.name)));
		}
		assert_eq!(
			commands
				.available
				.iter()
				.filter(|entry| entry.name == "review")
				.count(),
			1
		);
		assert!(
			!commands
				.available
				.iter()
				.any(|entry| entry.description == "shadow")
		);
		assert!(
			!commands
				.available
				.iter()
				.any(|entry| entry.name == "model:secret")
		);
		assert_eq!(
			submit_text(commands.parse_input("/review 'two words'").unwrap()),
			"Review two words"
		);
	}

	#[test]
	fn turn_budget_prefixes_map_to_agent_and_provider_budgets() {
		let commands = builtins();
		let ChatCommand::Submit { item, text, budget: Some(advisory) } =
			commands.parse_input("+12k investigate").unwrap()
		else {
			panic!("expected budgeted submit")
		};
		assert_eq!(text, "investigate");
		assert_eq!(advisory.agent.max_output_tokens, Some(12_000));
		assert_eq!(advisory.task.total_tokens, 12_000);
		assert_eq!(advisory.task.remaining_tokens, None);
		assert!(!advisory.hard);
		let Some(item::Kind::Message(message)) = item.kind else {
			panic!("missing user message")
		};
		assert!(matches!(
			message.parts[0].kind.as_ref(),
			Some(part::Kind::Text(text)) if text == "investigate"
		));

		let (_, hard) = parse_budget_prefix("+2k! build").unwrap();
		let hard = hard.unwrap();
		assert_eq!(hard.task.remaining_tokens, Some(2_000));
		assert!(hard.hard);
		for invalid in ["+0k no", "+2k!"] {
			assert_eq!(commands.parse_input(invalid), Err(InputError::InvalidBudget));
		}
		assert_eq!(submit_text(commands.parse_input("+2x no").unwrap()), "+2x no");
		assert_eq!(submit_text(commands.parse_input("+context please").unwrap()), "+context please");

		let ChatCommand::Submit { text, budget: Some(fractional), .. } =
			commands.parse_input("please +1.5k investigate").unwrap()
		else {
			panic!("fractional non-leading budget")
		};
		assert_eq!(text, "please investigate");
		assert_eq!(fractional.task.total_tokens, 1_500);

		let ChatCommand::Submit { text, budget: Some(millions), .. } =
			commands.parse_input("ship this +0.25m! safely").unwrap()
		else {
			panic!("million hard budget")
		};
		assert_eq!(text, "ship this safely");
		assert_eq!(millions.task.total_tokens, 250_000);
		assert_eq!(millions.task.remaining_tokens, Some(250_000));
	}

	#[test]
	fn tokenizer_and_substitution_are_quote_aware_and_non_recursive() {
		let args = tokenize_args("one 'two words' \"three words\" four\\ five $1").unwrap();
		assert_eq!(args, ["one", "two words", "three words", "four five", "$1"]);
		assert_eq!(
			expand_arguments("a=$1 all=$@ raw=$ARGUMENTS fifth=$5", &args),
			"a=one all=one two words three words four five $1 raw=one two words three words four \
			 five $1 fifth=$1"
		);
		assert_eq!(tokenize_args("'open"), Err(InputError::UnterminatedQuote));
	}
	#[test]
	fn partial_argument_completion_retains_completed_argv_and_open_token() {
		assert_eq!(completion_arg_query("one 'two words' thr"), ArgCompletionQuery {
			prefix: sf!("thr"),
			argv:   vec![sf!("one"), sf!("two words")],
		});
		assert_eq!(completion_arg_query("one "), ArgCompletionQuery {
			prefix: Str::default(),
			argv:   vec![sf!("one")],
		});
		assert_eq!(completion_arg_query("'open token"), ArgCompletionQuery {
			prefix: sf!("open token"),
			argv:   Vec::new(),
		});
	}
	#[test]
	fn all_argument_slices_use_pi_one_based_bounds() {
		let args = [sf!("a"), sf!("b"), sf!("c"), sf!("d")];
		assert_eq!(expand_arguments("$@[2]", &args), "b c d");
		assert_eq!(expand_arguments("$@[2:2]", &args), "b c");
		assert_eq!(expand_arguments("$@[3:]", &args), "c d");
		assert_eq!(expand_arguments("$@[5:] $@[0:] $@[2:0]", &args), "  ");
		assert_eq!(
			expand_arguments("$@[2] literal=$@[x] recursive=$1", &[sf!("$@"), sf!("b")]),
			"b literal=$@ b[x] recursive=$@"
		);
	}

	#[test]
	fn shell_arguments_expand_once_and_suppress_fallback() {
		let args = [sf!("one"), sf!("$1")];
		assert_eq!(expand_arguments("$ARGUMENTS | $2", &args), "one $1 | $1");
		assert_eq!(expand_arguments("no placeholder", &args), "no placeholder one $1");
		assert_eq!(expand_arguments("missing=$9", &args), "missing=");
	}
	#[test]
	fn embedded_init_is_a_native_prompt_workflow() {
		let roster = CommandRoster::new(vec![embedded_workflow_commands().into()]);
		let ChatCommand::Submit { text, .. } = roster
			.parse_input("/init focus on release tooling")
			.unwrap()
		else {
			panic!("embedded workflow did not submit a prompt");
		};
		assert!(text.starts_with("Use parallel `task` research agents"));
		assert!(text.ends_with("focus on release tooling"));
	}

	#[test]
	fn unavailable_commands_have_named_errors() {
		assert_eq!(
			builtins().parse_input("/compact summary"),
			Ok(ChatCommand::Unavailable {
				command: sf!("compact"),
				reason:  sf!("manual compaction is not exposed by the agent backend"),
			})
		);
	}
}
