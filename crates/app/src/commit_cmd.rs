//! Agentic commit command over the production durable headless loop.

use omp_core::Str;

use crate::{
	cli::{ChatArgs, CommitCliArgs, PrintArgs},
	print_mode,
};

/// Runs the canonical agentic commit workflow with explicit mutation approval.
pub(crate) async fn run(args: CommitCliArgs) -> miette::Result<()> {
	let mut launch = ChatArgs::default_interactive();
	launch.model = args.model;
	launch.yolo = !args.dry_run;
	launch.no_title = true;
	let mut request = String::from(
		"Inspect the complete Git worktree and produce one coherent conventional commit. Stage only \
		 intentional changes. ",
	);
	if args.dry_run {
		request.push_str(
			"Dry run: do not mutate files, the index, refs, or remotes; print the proposed commit \
			 and changelog edits. ",
		);
	} else {
		request.push_str("Update applicable changelogs, then create exactly one commit. ");
	}
	if args.no_changelog {
		request.push_str("Do not edit changelog files. ");
	}
	if args.legacy {
		request.push_str(
			"Use the conservative deterministic legacy classification and formatting policy. ",
		);
	}
	if args.push {
		request.push_str("After the commit succeeds, push the current branch. ");
	}
	if let Some(context) = args.context {
		request.push_str("Additional operator context: ");
		request.push_str(&context);
	}
	launch.prompt = vec![Str::from(request)];
	print_mode::run(PrintArgs {
		launch,
		mode: "text".into(),
		print_thoughts: false,
		follow_ups: Vec::new(),
		shape_transcript: false,
	})
	.await
}
