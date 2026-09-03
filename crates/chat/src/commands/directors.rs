//! Director-shaped mode commands (pi `builtin-modes.ts:259-336`,
//! `builtin-control.ts:7-73`): each engages or exits one ADR 0015 Director
//! frame under `<meta><directors>` through the controller.

use omp_con::ConError;
use omp_core::Str;
use omp_tui::Icon;

use super::{CommandAction, GoalOp, PaletteEntry, post, rest};

/// Palette icons for this module's commands.
pub const PALETTE: &[PaletteEntry] = &[
	PaletteEntry { name: "vibe", icon: Icon::Sparkle },
	PaletteEntry { name: "goal", icon: Icon::Goal },
	PaletteEntry { name: "guided-goal", icon: Icon::Goal },
	PaletteEntry { name: "loop", icon: Icon::Loop },
	PaletteEntry { name: "force", icon: Icon::Bolt },
	PaletteEntry { name: "pause", icon: Icon::Pause },
];

/// pi `loop-limit.ts` usage error.
pub const LOOP_USAGE: &str = "Usage: /loop [count|duration]. Examples: /loop 10, /loop 10m, /loop 10min.";

/// Splits `/loop [count] [prompt]`: a leading positive integer is the cap,
/// the rest is the prompt re-sent each iteration. A leading duration
/// (`10m`) is pi's time limit; the Director counts iterations, so it is
/// reported as unsupported rather than silently treated as a prompt.
pub fn loop_args(words: Option<Str>) -> Result<(Option<u32>, Option<Str>), ConError> {
	let Some(words) = words else {
		return Ok((None, None));
	};
	let text = words.as_str().trim();
	let (first, remainder) = text
		.split_once(char::is_whitespace)
		.map_or((text, ""), |(first, remainder)| (first, remainder.trim_start()));
	let prompt = |rest: &str| (!rest.is_empty()).then(|| Str::new(rest));
	if let Ok(limit) = first.parse::<u32>() {
		if limit == 0 {
			return Err(usage("Loop count must be a positive integer."));
		}
		return Ok((Some(limit), prompt(remainder)));
	}
	let looks_like_duration = first
		.chars()
		.next()
		.is_some_and(|ch| ch.is_ascii_digit())
		&& first.chars().all(|ch| ch.is_ascii_alphanumeric());
	if looks_like_duration {
		return Err(usage(LOOP_USAGE));
	}
	Ok((None, prompt(text)))
}

/// Parses `/goal …` words into one [`GoalOp`].
pub fn goal_op(words: Option<Str>) -> Result<GoalOp, ConError> {
	let Some(words) = words else {
		return Ok(GoalOp::Menu);
	};
	let text = words.as_str().trim();
	let (verb, rest) = text
		.split_once(char::is_whitespace)
		.map_or((text, ""), |(verb, rest)| (verb, rest.trim()));
	Ok(match verb {
		"set" => {
			if rest.is_empty() {
				GoalOp::Menu
			} else {
				GoalOp::Set(Str::new(rest))
			}
		},
		"show" => GoalOp::Show,
		"pause" => GoalOp::Pause,
		"resume" => GoalOp::Resume,
		"drop" => GoalOp::Drop,
		"budget" => match rest {
			"" | "off" => GoalOp::Budget(None),
			number => GoalOp::Budget(Some(
				number
					.parse::<u64>()
					.ok()
					.filter(|value| *value > 0)
					.ok_or_else(|| usage("Goal budget must be a positive integer or `off`."))?,
			)),
		},
		_ => GoalOp::Set(Str::new(text)),
	})
}

fn usage(message: &'static str) -> ConError {
	ConError::Usage(Str::new_static(message))
}

omp_con::cmd! {
	/// Toggles vibe mode: you direct worker sessions; a prompt is submitted once on.
	vibe(?prompt: Str) = |ctx, args| post(ctx, CommandAction::Vibe { prompt: rest(args, 0) });

	/// Manages the goal: `set <objective>`, `show`, `pause`, `resume`, `drop`, `budget <N|off>`.
	goal(?op: Str, ?args: Str) = |ctx, args| {
		post(ctx, CommandAction::Goal(goal_op(rest(args, 0))?))
	};

	/// Interviews you step by step, then creates the goal.
	"guided-goal"(?objective: Str) = |ctx, args| {
		post(ctx, CommandAction::GuidedGoal { initial: rest(args, 0) })
	};

	/// Repeats the prompt after each turn: `/loop [count] [prompt]`; again to disable.
	"loop"(?count: Str, ?prompt: Str) = |ctx, args| {
		let (limit, prompt) = loop_args(rest(args, 0))?;
		post(ctx, CommandAction::Loop { limit, prompt })
	};

	/// Forces the next turn to call the named tool: `/force <tool> [prompt]`.
	force(tool @ "sv::tool": Str, ?prompt: Str) = |ctx, args| {
		post(ctx, CommandAction::Force { tool: args.get::<Str>(0)?, prompt: rest(args, 1) })
	};

	/// Pauses every agent at its next step until you resume.
	pause() = |ctx, _args| post(ctx, CommandAction::Pause);

	/// Releases the pause gate (posted by the pause screen with the hold length).
	pause_resume(?held_ms: i64) = |ctx, args| {
		let held_ms = args.opt::<i64>(0)?.unwrap_or(0).max(0).unsigned_abs();
		post(ctx, CommandAction::PauseResume { held_ms })
	};
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn loop_arguments_split_a_leading_count_from_the_prompt() {
		assert_eq!(loop_args(None).unwrap(), (None, None));
		assert_eq!(loop_args(Some(Str::new_static("5"))).unwrap(), (Some(5), None));
		assert_eq!(
			loop_args(Some(Str::new_static("5 run the tests"))).unwrap(),
			(Some(5), Some(Str::new_static("run the tests")))
		);
		assert_eq!(
			loop_args(Some(Str::new_static("run the tests"))).unwrap(),
			(None, Some(Str::new_static("run the tests")))
		);
		assert!(loop_args(Some(Str::new_static("0"))).is_err());
		assert!(loop_args(Some(Str::new_static("10m fix"))).is_err());
	}

	#[test]
	fn goal_words_dispatch_to_subcommands_with_a_bare_objective_fallback() {
		assert_eq!(goal_op(None).unwrap(), GoalOp::Menu);
		assert_eq!(goal_op(Some(Str::new_static("show"))).unwrap(), GoalOp::Show);
		assert_eq!(
			goal_op(Some(Str::new_static("set ship it"))).unwrap(),
			GoalOp::Set(Str::new_static("ship it"))
		);
		assert_eq!(
			goal_op(Some(Str::new_static("ship it"))).unwrap(),
			GoalOp::Set(Str::new_static("ship it"))
		);
		assert_eq!(goal_op(Some(Str::new_static("budget off"))).unwrap(), GoalOp::Budget(None));
		assert_eq!(goal_op(Some(Str::new_static("budget 5000"))).unwrap(), GoalOp::Budget(Some(5000)));
		assert!(goal_op(Some(Str::new_static("budget -1"))).is_err());
	}
}
