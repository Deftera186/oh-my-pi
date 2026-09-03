//! `/` slash-command roster projected from the console registry: every
//! registered command is a palette entry whose description is its doc
//! comment, whose usage ghost lists its declared arguments, and whose first
//! argument completes through the console's own completer groups. A
//! submitted `/name args` line runs as the console statement `name args`.

use std::sync::Arc;

use omp_con::{Ctx, RegItem};
use omp_core::{Str, StrMut};
use omp_tui::{Command, CommandArgument, Icon};

/// Palette icons for the host's product commands; anything else shows no
/// type indicator, like pi's extension-registered commands.
const ICONS: [(&str, Icon); 14] = [
	("cl_model_select", Icon::Model),
	("cl_model_cycle", Icon::Model),
	("cl_thinking_cycle", Icon::EyeSpeechThought),
	("cl_history_search", Icon::History),
	("cl_plan_toggle", Icon::Plan),
	("cl_editor_external", Icon::External),
	("cl_clear", Icon::Trash),
	("cl_exit", Icon::Exit),
	("cl_interrupt", Icon::Stop),
	("cl_retry", Icon::Refresh),
	("cl_display_reset", Icon::Refresh),
	("help", Icon::Help),
	("find", Icon::Search),
	("exec", Icon::Config),
];

/// Builds the slash palette from `con`'s statically registered commands.
#[must_use]
pub fn roster(con: &Arc<Ctx>) -> Vec<Command> {
	con
		.items()
		.filter_map(|item| match item {
			RegItem::Cmd(spec) => Some(spec),
			RegItem::Var(_) | RegItem::Action(_) => None,
		})
		.map(|spec| {
			let mut command = Command::new(spec.name, first_line(spec.desc), &[]);
			if let Some((_, icon)) = ICONS.iter().find(|(name, _)| *name == spec.name) {
				command = command.with_icon(*icon);
			}
			let usage = usage(spec.args);
			if !usage.is_empty() {
				command = command.with_hint(&usage);
			}
			if !spec.args.is_empty() {
				let con = Arc::clone(con);
				let name = spec.name;
				command = command.with_dynamic_args(move |partial| {
					let mut line = StrMut::new(name);
					line.push(' ');
					line.push_str(partial);
					let cursor = line.len();
					con
						.complete(line.as_str(), cursor)
						.into_iter()
						.map(|suggestion| CommandArgument {
							value:       suggestion.text,
							description: suggestion.help,
							usage:       None,
						})
						.collect()
				});
			}
			command
		})
		.collect()
}

/// First line of a doc-comment description, without its leading space.
fn first_line(desc: &str) -> &str {
	desc.lines().next().unwrap_or_default().trim()
}

/// `<required> [optional]` usage text from declared arguments.
fn usage(args: &[omp_con::ArgSpec]) -> Str {
	let mut out = StrMut::new("");
	for (index, arg) in args.iter().enumerate() {
		if index > 0 {
			out.push(' ');
		}
		let (open, close) = if arg.required { ('<', '>') } else { ('[', ']') };
		out.push(open);
		out.push_str(arg.name);
		out.push(close);
	}
	out.freeze()
}

#[cfg(test)]
mod tests {
	use omp_con::CtxBuilder;
	use omp_tui::{EditorCompletion, SlashCommands, SuggestionDisplay};

	use super::*;

	fn labels(suggestions: &omp_tui::Suggestions) -> Vec<&str> {
		suggestions
			.items
			.iter()
			.map(|item| match item.display() {
				SuggestionDisplay::Text(label) => label.as_str(),
				SuggestionDisplay::Emoji { .. } => unreachable!(),
			})
			.collect()
	}

	#[test]
	fn console_builtins_become_palette_rows_with_usage_and_icons() {
		let con = Arc::new(CtxBuilder::default().build());
		let roster = roster(&con);
		let help = roster
			.iter()
			.find(|command| command.name() == "help")
			.expect("`help` is a console builtin");
		assert_eq!(help.icon(), Some(Icon::Help));
		assert!(help.description().starts_with("Shows a name"), "{}", help.description());
		let mut slash = SlashCommands::new(roster);
		let rows = slash.suggest("/he", 3).expect("slash rows");
		assert!(labels(&rows).iter().any(|label| *label == "help"), "{:?}", labels(&rows));
		// The usage ghost lists the declared optional argument.
		assert_eq!(slash.hint("/help ", 6).as_deref(), Some("[name]"));
	}

	#[test]
	fn first_argument_completes_through_the_console_completer() {
		let con = Arc::new(CtxBuilder::default().build());
		let mut slash = SlashCommands::new(roster(&con));
		let rows = slash.suggest("/help fi", 8).expect("argument rows");
		assert!(labels(&rows).iter().any(|label| *label == "find"), "{:?}", labels(&rows));
	}
}
