//! Immutable structural command router shared by every presentation surface.

use std::{collections::HashMap, fmt, future::Future, pin::Pin, sync::Arc};

use omp_core::{Str, sf};

use super::{
	super::{
		input::{expand_arguments_with_fallback, tokenize_args},
		template::{TemplateArguments, compile, references_arguments, render_compiled},
	},
	CommandHandler, CommandHost,
	result::{CommandResult, DispatchResult, PromptResult},
};

/// Runtime authority required before a command may be advertised or invoked.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandCapability {
	/// Durable session journal access.
	Session,
	/// Workspace authority access.
	Workspace,
	/// Model/catalog configuration access.
	Model,
	/// Provider credential authority access.
	Credentials,
	/// Context projection and accounting access.
	Context,
	/// Active turn execution control.
	Execution,
	/// Owner-only mutation authority.
	Owner,
}

/// Presentation surface on which a command may appear.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandSurface {
	/// Interactive terminal UI.
	Tui,
	/// Agent Client Protocol client.
	Acp,
	/// Plain text or RPC client.
	Text,
}

/// Collaboration role used only for presentation filtering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandRole {
	/// Local owner or collaboration host.
	Owner,
	/// Invited collaboration guest.
	Guest,
}

/// Stable category of a command contribution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandSourceKind {
	/// Command compiled into OMP.
	Builtin,
	/// OMP skill invocation.
	Skill,
	/// Signed native extension.
	Extension,
	/// Project or user native custom command.
	Custom,
	/// OMP Markdown command asset.
	Markdown,
}

/// Source identity retained through completion, help, and dispatch.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommandProvenance {
	/// Stable source identifier.
	pub source:     Str,
	/// Human-readable origin label.
	pub label:      Str,
	/// Source category.
	pub kind:       CommandSourceKind,
	/// Atomic discovery generation supplying this declaration.
	pub generation: u64,
}

impl CommandProvenance {
	/// Provenance shared by compiled commands.
	pub fn builtin() -> Self {
		Self {
			source:     sf!("builtin"),
			label:      sf!("OMP"),
			kind:       CommandSourceKind::Builtin,
			generation: 0,
		}
	}
}

/// One immutable argument suggestion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgumentHint {
	/// Text inserted by completion.
	pub value:       Str,
	/// Explanation displayed beside the value.
	pub description: Str,
}

/// Executable implementation attached to one declaration.
#[derive(Clone)]
pub enum CommandImplementation {
	/// Structural handler whose generated wrapper parses the declared grammar.
	Handler(CommandHandler),
	/// Prompt template. `$ARGUMENTS` is replaced exactly once.
	Prompt(Str),
	/// Lazy extension callback owned by one exact verified generation.
	Extension(Arc<dyn ExtensionCommandHandler>),
}
/// Typed slash invocation delivered to an extension callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionCommandInvocation {
	/// Spelling entered by the user, including aliases.
	pub name:    Str,
	/// Tokenized arguments with quote grouping already applied.
	pub argv:    Arc<[Str]>,
	/// Untokenized text after the command name.
	pub raw:     Str,
	/// Presentation mode in which the command was reached.
	pub surface: CommandSurface,
}

/// Future returned by an exact-generation extension command callback.
pub type ExtensionCommandFuture =
	Pin<Box<dyn Future<Output = miette::Result<CommandResult>> + Send + 'static>>;

/// Lazy activation and dispatch seam for one verified extension command.
pub trait ExtensionCommandHandler: Send + Sync + 'static {
	/// Activates only the owning extension and dispatches to its exact
	/// generation.
	fn call(
		&self,
		invocation: ExtensionCommandInvocation,
		provenance: CommandProvenance,
	) -> ExtensionCommandFuture;
}

impl<F, Fut> ExtensionCommandHandler for F
where
	F: Fn(ExtensionCommandInvocation, CommandProvenance) -> Fut + Send + Sync + 'static,
	Fut: Future<Output = miette::Result<CommandResult>> + Send + 'static,
{
	fn call(
		&self,
		invocation: ExtensionCommandInvocation,
		provenance: CommandProvenance,
	) -> ExtensionCommandFuture {
		Box::pin(self(invocation, provenance))
	}
}

impl fmt::Debug for CommandImplementation {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Handler(_) => formatter.write_str("Handler(..)"),
			Self::Prompt(template) => formatter.debug_tuple("Prompt").field(template).finish(),
			Self::Extension(_) => formatter.write_str("Extension(..)"),
		}
	}
}

/// One complete declaration consumed by parsing, help, completion, and
/// dispatch.
#[derive(Clone, Debug)]
pub struct CommandDeclaration {
	/// Stable ordering among compiled declarations.
	pub order:           u16,
	/// Canonical spelling without `/`.
	pub name:            Str,
	/// Semantic completion icon for compiled commands.
	pub icon:            omp_tui::Icon,
	/// Alternate spellings without `/`.
	pub aliases:         Arc<[Str]>,
	/// One-line help and completion text.
	pub description:     Str,
	/// Argument grammar rendered by help and completion.
	pub argument_hint:   Option<Str>,
	/// Immutable argument candidates.
	pub hints:           Arc<[ArgumentHint]>,
	/// Required runtime capabilities.
	pub capabilities:    Arc<[CommandCapability]>,
	/// Supported presentation surfaces.
	pub surfaces:        Arc<[CommandSurface]>,
	/// Whether collaboration guests may see this command.
	pub guest_visible:   bool,
	/// ACP-specific description override.
	pub acp_description: Option<Str>,
	/// Source identity.
	pub provenance:      CommandProvenance,
	/// Executable implementation.
	pub implementation:  CommandImplementation,
}
impl CommandDeclaration {
	/// Builds an executable command exclusively from a manifest-verified typed
	/// UI declaration and its exact-generation callback.
	pub fn verified_extension(
		declaration: &omp_proto::ui::v1::CommandDecl,
		provenance: CommandProvenance,
		handler: Arc<dyn ExtensionCommandHandler>,
	) -> Self {
		Self {
			order: 0,
			name: Str::from(declaration.name.as_str()),
			icon: omp_tui::Icon::ExtensionCommand,
			aliases: declaration
				.aliases
				.iter()
				.map(Str::from)
				.collect::<Vec<_>>()
				.into(),
			description: Str::from(declaration.description.as_str()),
			argument_hint: declaration.hint.as_deref().map(Str::from),
			hints: declaration
				.args
				.iter()
				.map(|arg| ArgumentHint {
					value:       Str::from(arg.name.as_str()),
					description: Str::from(arg.description.as_str()),
				})
				.collect::<Vec<_>>()
				.into(),
			capabilities: Arc::from([]),
			surfaces: Arc::from([CommandSurface::Tui, CommandSurface::Acp, CommandSurface::Text]),
			guest_visible: false,
			acp_description: None,
			provenance,
			implementation: CommandImplementation::Extension(handler),
		}
	}
}

/// One compiled declaration factory submitted by [`inventory`].
pub struct BuiltinRegistration {
	/// Builds the declaration without retaining mutable global state.
	pub declaration: fn() -> CommandDeclaration,
}

inventory::collect!(BuiltinRegistration);

/// Explicit source-qualified permission to replace one builtin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowRule {
	/// Builtin canonical name being replaced.
	pub builtin: Str,
	/// Exact contribution source identifier.
	pub source:  Str,
	/// Exact contribution canonical name.
	pub command: Str,
}

/// Roster construction policy. Builtins win unless a rule matches exactly.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShadowPolicy {
	/// Explicit builtin replacement rules.
	pub rules: Arc<[ShadowRule]>,
}

impl ShadowPolicy {
	fn permits(&self, builtin: &str, declaration: &CommandDeclaration) -> bool {
		self.rules.iter().any(|rule| {
			rule.builtin == builtin
				&& rule.source == declaration.provenance.source
				&& rule.command == declaration.name
		})
	}
}

/// A source generation published atomically by discovery.
#[derive(Clone, Debug)]
pub struct CommandGeneration {
	/// Source generation identity.
	pub provenance:   CommandProvenance,
	/// All declarations in the generation.
	pub declarations: Arc<[CommandDeclaration]>,
}

/// Lightweight declaration copied to completion, help, or ACP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvertisedCommand {
	/// Canonical slash spelling.
	pub name:          Str,
	/// Description appropriate to the requested surface.
	pub description:   Str,
	/// Inline argument placeholder.
	pub argument_hint: Option<Str>,
	/// Immutable argument candidates.
	pub hints:         Arc<[ArgumentHint]>,
	/// Source identity shown to the user.
	pub provenance:    CommandProvenance,
}

/// Immutable winning command generation.
#[derive(Clone)]
pub struct CommandRoster {
	commands:  Arc<[CommandDeclaration]>,
	spellings: Arc<HashMap<Str, usize>>,
}

fn completion_icon(declaration: &CommandDeclaration) -> omp_tui::Icon {
	match declaration.provenance.kind {
		CommandSourceKind::Builtin => declaration.icon,
		CommandSourceKind::Skill => omp_tui::Icon::Skill,
		CommandSourceKind::Extension => omp_tui::Icon::ExtensionCommand,
		CommandSourceKind::Custom | CommandSourceKind::Markdown => omp_tui::Icon::Prompt,
	}
}

impl CommandRoster {
	/// Builds the inventory roster with no dynamic contributions.
	pub fn builtins() -> Self {
		Self::with_contributions([], &ShadowPolicy::default())
	}

	/// Builds inventory declarations plus atomically published contributions.
	pub fn with_contributions(
		generations: impl IntoIterator<Item = CommandGeneration>,
		policy: &ShadowPolicy,
	) -> Self {
		Self::with_contributions_filtered(generations, policy, |declaration| {
			declaration.name != "security"
		})
	}

	/// Builds inventory declarations while omitting disabled builtins before
	/// help, completion, advertisement, and dispatch indexes are derived.
	pub fn with_contributions_filtered(
		generations: impl IntoIterator<Item = CommandGeneration>,
		policy: &ShadowPolicy,
		include_builtin: impl Fn(&CommandDeclaration) -> bool,
	) -> Self {
		let mut builtins: Vec<_> = inventory::iter::<BuiltinRegistration>
			.into_iter()
			.map(|registration| (registration.declaration)())
			.filter(include_builtin)
			.collect();
		builtins.sort_unstable_by_key(|declaration| declaration.order);
		Self::build(builtins, generations, policy)
	}

	/// Builds one atomic roster from compiled declarations and source
	/// generations.
	pub fn build(
		builtins: impl IntoIterator<Item = CommandDeclaration>,
		generations: impl IntoIterator<Item = CommandGeneration>,
		policy: &ShadowPolicy,
	) -> Self {
		let mut commands: Vec<CommandDeclaration> = builtins.into_iter().collect();
		let builtin_count = commands.len();
		for generation in generations {
			for mut declaration in generation.declarations.iter().cloned() {
				declaration.provenance = generation.provenance.clone();
				if let Some((index, builtin)) = commands
					.iter()
					.take(builtin_count)
					.enumerate()
					.find(|(_, builtin)| builtin.name == declaration.name)
				{
					if policy.permits(builtin.name.as_str(), &declaration) {
						commands[index] = declaration;
					}
					continue;
				}
				commands.push(declaration);
			}
		}

		let mut winners = Vec::<CommandDeclaration>::with_capacity(commands.len());
		let mut spellings = HashMap::<Str, usize>::new();
		for mut declaration in commands {
			declaration.guest_visible =
				omp_collab::guest::guest_command_allowed(declaration.name.as_str())
					|| declaration
						.aliases
						.iter()
						.any(|alias| omp_collab::guest::guest_command_allowed(alias.as_str()));
			if spellings.contains_key(&declaration.name) {
				continue;
			}
			let index = winners.len();
			spellings.insert(declaration.name.clone(), index);
			for alias in declaration.aliases.iter() {
				spellings.entry(alias.clone()).or_insert(index);
			}
			winners.push(declaration);
		}
		Self { commands: winners.into(), spellings: Arc::new(spellings) }
	}

	/// Slash completion entries derived from the winning roster.
	pub fn completions(&self) -> Vec<omp_tui::Command> {
		self.completions_for(CommandRole::Owner)
	}

	/// Resolves submitted slash input to its winning canonical declaration name.
	pub fn command_usage_name(&self, text: &str) -> Option<Str> {
		let token = text.trim().strip_prefix('/')?.split_whitespace().next()?;
		let index = self.spellings.get(token)?;
		self
			.commands
			.get(*index)
			.map(|command| command.name.clone())
	}

	/// Slash completion entries filtered for the collaboration role.
	pub fn completions_for(&self, role: CommandRole) -> Vec<omp_tui::Command> {
		self.completions_for_described(role, |_| None)
	}

	/// Slash completion entries with a caller-projected live-state description.
	pub fn completions_for_described(
		&self,
		role: CommandRole,
		describe: impl Fn(&CommandDeclaration) -> Option<Str>,
	) -> Vec<omp_tui::Command> {
		use smallvec::SmallVec;
		self
			.commands
			.iter()
			.filter(|declaration| role == CommandRole::Owner || declaration.guest_visible)
			.map(|declaration| {
				let aliases: SmallVec<&str, 2> = declaration.aliases.iter().map(Str::as_str).collect();
				let description =
					describe(declaration).unwrap_or_else(|| declaration.description.clone());
				let mut command =
					omp_tui::Command::new(declaration.name.as_str(), description.as_str(), &aliases)
						.with_icon(completion_icon(declaration));
				if !declaration.hints.is_empty() {
					let hints: Vec<_> = declaration
						.hints
						.iter()
						.map(|hint| (hint.value.as_str(), hint.description.as_str(), ""))
						.collect();
					command = command.with_args(&hints);
				}
				if let Some(hint) = &declaration.argument_hint {
					command = command.with_hint(hint);
				}
				command
			})
			.collect()
	}

	/// Advertises the same winning roster used by dispatch.
	pub fn advertised(
		&self,
		surface: CommandSurface,
		role: CommandRole,
		skill_commands: bool,
		available: impl Fn(CommandCapability) -> bool,
	) -> Vec<AdvertisedCommand> {
		self
			.commands
			.iter()
			.filter(|command| command.surfaces.contains(&surface))
			.filter(|command| role == CommandRole::Owner || command.guest_visible)
			.filter(|command| skill_commands || command.provenance.kind != CommandSourceKind::Skill)
			.filter(|command| command.capabilities.iter().copied().all(&available))
			.map(|command| AdvertisedCommand {
				name:          command.name.clone(),
				description:   if surface == CommandSurface::Acp {
					command
						.acp_description
						.clone()
						.unwrap_or_else(|| command.description.clone())
				} else {
					command.description.clone()
				},
				argument_hint: command.argument_hint.clone(),
				hints:         command.hints.clone(),
				provenance:    command.provenance.clone(),
			})
			.collect()
	}

	/// Renders help from the same filtered declarations used by completion.
	pub fn help_text(
		&self,
		surface: CommandSurface,
		role: CommandRole,
		skill_commands: bool,
		available: impl Fn(CommandCapability) -> bool,
	) -> String {
		use std::fmt::Write as _;
		let advertised = self.advertised(surface, role, skill_commands, available);
		let mut output = String::with_capacity(advertised.len().saturating_mul(56));
		for command in advertised {
			let _ = write!(output, "/{}", command.name);
			if let Some(hint) = command.argument_hint {
				let _ = write!(output, " {hint}");
			}
			let _ = writeln!(output, " — {} [{}]", command.description, command.provenance.label);
		}
		output
	}

	/// Parses and dispatches recognized input; unknown input remains a prompt.
	pub fn dispatch<'a>(
		&'a self,
		text: &'a str,
		surface: CommandSurface,
		host: &'a mut dyn CommandHost,
	) -> Pin<Box<dyn Future<Output = miette::Result<DispatchResult>> + Send + 'a>> {
		Box::pin(async move {
			let Some(body) = text.strip_prefix('/') else {
				return Ok(DispatchResult::Passthrough(Str::new(text)));
			};
			if body.is_empty() || body.starts_with('/') {
				return Ok(DispatchResult::Passthrough(Str::new(text)));
			}
			let split = body.find(char::is_whitespace).unwrap_or(body.len());
			let token = &body[..split];
			let trailing = body[split..].trim();
			let mut candidate = token;
			let mut colon_args = "";
			let (index, args) = loop {
				if let Some(index) = self.spellings.get(candidate).copied() {
					let args = match (colon_args.is_empty(), trailing.is_empty()) {
						(true, _) => trailing,
						(false, true) => colon_args,
						(false, false) => return Ok(DispatchResult::Passthrough(Str::new(text))),
					};
					break (index, args);
				}
				let Some((prefix, suffix)) = candidate.rsplit_once(':') else {
					return Ok(DispatchResult::Passthrough(Str::new(text)));
				};
				candidate = prefix;
				colon_args = suffix;
			};
			let declaration = &self.commands[index];
			if !declaration.surfaces.contains(&surface) {
				return Ok(DispatchResult::Passthrough(Str::new(text)));
			}
			let result = match &declaration.implementation {
				CommandImplementation::Handler(handler) => {
					handler(host, args, &declaration.provenance).await?
				},
				CommandImplementation::Prompt(template) => {
					let words = tokenize_args(args).map_err(|error| miette::miette!("{error}"))?;
					let compiled = compile(template)?;
					let fallback = if references_arguments(&compiled) {
						""
					} else {
						args
					};
					let rendered =
						render_compiled(&compiled, TemplateArguments { raw: args, words: &words })?;
					CommandResult::Prompt(PromptResult {
						text:       Str::from(expand_arguments_with_fallback(
							rendered.as_str(),
							&words,
							fallback,
						)),
						provenance: declaration.provenance.clone(),
					})
				},
				CommandImplementation::Extension(handler) => {
					let argv = tokenize_args(args).map_err(|error| miette::miette!("{error}"))?;
					handler
						.call(
							ExtensionCommandInvocation {
								name: Str::new(candidate),
								argv: argv.into(),
								raw: Str::new(args),
								surface,
							},
							declaration.provenance.clone(),
						)
						.await?
				},
			};
			Ok(DispatchResult::Handled(result))
		})
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn completion_items_use_builtin_icons_and_source_kind_overrides() {
		let provenance = CommandProvenance {
			source:     sf!("test:notes"),
			label:      sf!("Test Markdown"),
			kind:       CommandSourceKind::Markdown,
			generation: 1,
		};
		let markdown = CommandDeclaration {
			order:           0,
			name:            sf!("notes"),
			icon:            omp_tui::Icon::Model,
			aliases:         Arc::from([]),
			description:     sf!("Run the Markdown prompt"),
			argument_hint:   None,
			hints:           Arc::from([]),
			capabilities:    Arc::from([]),
			surfaces:        Arc::from([CommandSurface::Tui]),
			guest_visible:   false,
			acp_description: None,
			provenance:      provenance.clone(),
			implementation:  CommandImplementation::Prompt(sf!("Review the notes")),
		};
		let roster = CommandRoster::with_contributions(
			[CommandGeneration { provenance, declarations: Arc::from([markdown]) }],
			&ShadowPolicy::default(),
		);
		let completions = roster.completions();
		let icon = |name: &str| {
			completions
				.iter()
				.find(|command| command.name() == name)
				.and_then(omp_tui::Command::icon)
				.unwrap_or_else(|| panic!("missing completion icon for /{name}"))
		};

		assert_eq!(icon("model"), omp_tui::Icon::Model);
		assert_eq!(icon("quit"), omp_tui::Icon::Power);
		assert_eq!(icon("mcp"), omp_tui::Icon::Mcp);
		assert_eq!(icon("notes"), omp_tui::Icon::Prompt);

		for command in &completions {
			let icon = command
				.icon()
				.unwrap_or_else(|| panic!("missing completion icon for /{}", command.name()));
			assert_ne!(
				icon,
				omp_tui::Icon::SlashCommand,
				"/{} retained the generic slash-command icon",
				command.name()
			);
			for charset in
				[omp_tui::Charset::Ascii, omp_tui::Charset::Unicode, omp_tui::Charset::NerdFont]
			{
				assert!(!charset.icon(icon).is_empty(), "/{} has an empty icon", command.name());
			}
		}

		for (icon, glyphs) in [
			(omp_tui::Icon::Model, ["[M]", "⬢", ""]),
			(omp_tui::Icon::Power, ["PWR", "⏻", ""]),
			(omp_tui::Icon::Mcp, ["<>", "🔌", ""]),
			(omp_tui::Icon::Prompt, ["PR", "✎", ""]),
		] {
			assert_eq!(omp_tui::Charset::Ascii.icon(icon), glyphs[0]);
			assert_eq!(omp_tui::Charset::Unicode.icon(icon), glyphs[1]);
			assert_eq!(omp_tui::Charset::NerdFont.icon(icon), glyphs[2]);
		}
	}
	#[test]
	fn verified_extension_metadata_is_static_and_cannot_shadow_core() {
		let provenance = CommandProvenance {
			source:     sf!("extension:fixture"),
			label:      sf!("Fixture"),
			kind:       CommandSourceKind::Extension,
			generation: 7,
		};
		let handler: Arc<dyn ExtensionCommandHandler> =
			Arc::new(|_, _| async { Ok(CommandResult::Consumed(Default::default())) });
		let declaration = |name: &str, alias: &str| {
			CommandDeclaration::verified_extension(
				&omp_proto::ui::v1::CommandDecl {
					name: name.to_owned(),
					description: "Fixture command".to_owned(),
					aliases: vec![alias.to_owned()],
					declaration_id: name.to_owned(),
					..Default::default()
				},
				provenance.clone(),
				handler.clone(),
			)
		};
		let declarations =
			Arc::from([declaration("model", "steal-model"), declaration("fixture", "fx")]);
		let roster = CommandRoster::with_contributions(
			[CommandGeneration { provenance, declarations }],
			&ShadowPolicy::default(),
		);
		let completions = roster.completions();
		assert_eq!(
			completions
				.iter()
				.find(|command| command.name() == "model")
				.unwrap()
				.icon(),
			Some(omp_tui::Icon::Model)
		);
		assert_eq!(
			completions
				.iter()
				.find(|command| command.name() == "fixture")
				.unwrap()
				.description(),
			"Fixture command"
		);
		assert_eq!(roster.command_usage_name("/fx one"), Some(sf!("fixture")));
	}
}
