use super::{CardFixture, FixtureState};

const BASH_ARGS: &str = r#"{"command":"git status --short && git log --oneline -5","cwd":"packages/coding-agent","timeout":30}"#;
const BASH_RESULT: &str = r#"{"transcript":[{"data":" M src/cli/gallery-cli.ts\n M src/tools/bash.ts\n?? src/cli/gallery-fixtures/shell.ts\na1b2c3d Wire gallery command into CLI dispatch\n9f8e7d6 Add ToolExecutionComponent lifecycle states\n4c5b6a7 Extract createShellRenderer from bashToolRenderer\n2d3e4f5 Strip LLM-facing notices before TUI render\n7a8b9c0 Cap preview lines in pending command block\n"}],"status":{"exit_code":0,"wall_clock_ms":184}}"#;
const BASH_FAULT: &str = r#""src/tools/bash.ts:1142:34 - error TS2339: Property 'requestedTimeoutSeconds' does not exist on\ntype 'BashToolDetails'.\n\n1142   const requestedTimeoutSeconds = details?.requestedTimeoutSeconds;\n                                            ~~~~~~~~~~~~~~~~~~~~~~~~\nFound 1 error in src/tools/bash.ts:1142""#;

const EVAL_ARGS: &str = r#"{"language":"py","title":"load config","code":"import json\nfrom pathlib import Path\n\ndata = json.loads(Path(\"package.json\").read_text())\ndeps = data.get(\"dependencies\", {})\nprint(f\"{data[\\\"name\\\"]} v{data[\\\"version\\\"]}\")\nprint(f\"{len(deps)} dependencies\")\ndisplay(sorted(deps)[:3])"}"#;
const EVAL_FAILED_ARGS: &str = r#"{"language":"py","title":"load config","code":"import json\nfrom pathlib import Path\n\ndata = json.loads(Path(\"package.json\").read_text())\ndeps = data.get(\"dependencies\", {})\nprint(f\"{data[\\\"name\\\"]} v{data[\\\"version\\\"]}\")"}"#;
const EVAL_RESULT: &str = r#"{"frames":[{"data":"@oh-my-pi/coding-agent v0.42.0\n37 dependencies\n"}],"display_outputs":[{"type":"json","data":"@ai-sdk/anthropic"},{"type":"json","data":"@oh-my-pi/pi-ai"},{"type":"json","data":"@oh-my-pi/pi-tui"}],"status":{"duration_ms":64}}"#;
const EVAL_FAULT: &str = r#""Traceback (most recent call last):\n  File \"<cell 0>\", line 4, in <module>\n    data = json.loads(Path(\"package.json\").read_text())\njson.decoder.JSONDecodeError: Expecting ',' delimiter: line 12 column 3 (char 318)""#;

const AST_GREP_ARGS: &str = r#"{"pat":"useState($A)","path":"packages/tui/src/components"}"#;
const AST_GREP_RESULT: &str = r#"{"matches":[{"path":"packages/tui/src/components/SearchBox.tsx","line":18,"text":"  const [query, setQuery] = useState(\"\");","bindings":{"A":"\"\""}},{"path":"packages/tui/src/components/StatusBar.tsx","line":27,"text":"  const [expanded, setExpanded] = useState(false);","bindings":{"A":"false"}}],"match_count":2,"file_count":2,"files_searched":14,"scope_path":"packages/tui/src/components"}"#;

const AST_EDIT_ARGS: &str = r#"{"ops":[{"pat":"countEditFiles($$$ARGS)","out":"countDistinctFiles($$$ARGS)"}],"paths":["packages/coding-agent/src/**/*.ts"]}"#;
const AST_EDIT_RESULT: &str = r#"{"applied":false,"total_replacements":3,"files_touched":2,"proposal_id":"ast-edit-1","scope_path":"packages/coding-agent/src","files":[{"path":"edit/renderer.ts","replacements":2,"diff":[{"kind":"del","line":468,"text":"\t\tfileCount = countEditFiles(editArgs.edits);"},{"kind":"add","line":468,"text":"\t\tfileCount = countDistinctFiles(editArgs.edits);"},{"kind":"del","line":488,"text":"\t\tconst totalFiles = args?.edits ? countEditFiles(args.edits) : 0;"},{"kind":"add","line":488,"text":"\t\tconst totalFiles = args?.edits ? countDistinctFiles(args.edits) : 0;"}]},{"path":"tools/tool-result.ts","replacements":1,"diff":[{"kind":"del","line":42,"text":"\treturn countEditFiles(files);"},{"kind":"add","line":42,"text":"\treturn countDistinctFiles(files);"}]}]}"#;

const LSP_ARGS: &str =
	r#"{"action":"references","file":"src/server/auth.ts","line":42,"symbol":"validateToken"}"#;
const LSP_RESULT: &str = r#"{"action":"references","references":[{"path":"src/server/auth.ts","locations":[{"line":42,"col":14},{"line":118,"col":21}]},{"path":"src/server/middleware/session.ts","locations":[{"line":57,"col":18}]},{"path":"src/server/router.ts","locations":[{"line":153,"col":20}]},{"path":"test/auth.test.ts","locations":[{"line":24,"col":9},{"line":41,"col":9}]}]}"#;

const BROWSER_ARGS: &str = r#"{"action":"run","name":"docs","code":"const obs = await tab.observe();\nconst heading = obs.elements.find(e => e.role === 'heading');\ndisplay({\n    url: obs.url,\n    title: obs.title,\n    headings: obs.elements.filter(e => e.role === 'heading').length\n});\nreturn heading?.name ?? 'no heading found';"}"#;
const BROWSER_RESULT: &str = r#"{"action":"run","name":"docs","url":"https://bun.sh/docs","browser":"headless","display":[{"url":"https://bun.sh/docs","title":"Bun Documentation","headings":14}],"result":"Get started with Bun"}"#;
const BROWSER_FAULT: &str = r#""TimeoutError: waiting for selector `aria/Sign in` failed: timeout 30000ms exceeded\n    at Tab.waitFor (browser/tab.ts:212:13)\n    at run (eval:3:7)""#;

const INSPECT_ARGS: &str = r#"{"path":"docs/assets/dashboard-mock.png","question":"What chart types are shown and roughly what layout does the dashboard use?"}"#;
const INSPECT_RESULT: &str = r#"{"answer":"The dashboard uses a two-column layout on a dark background.\nTop row: four KPI cards (Revenue, Active Users, Churn, MRR) with sparklines.\nLeft column: a stacked area chart of weekly sessions over ~3 months.\nRight column: a horizontal bar chart ranking the top 6 referrers.\nBottom: a paginated table of recent transactions with status pills.","model":"claude-opus-4","image_path":"docs/assets/dashboard-mock.png","mime_type":"image/png"}"#;

pub(super) const FIXTURES: &[CardFixture] = &[
	CardFixture {
		tool:   "bash",
		title:  "Bash",
		states: [
			FixtureState {
				args:   r#"{"command":"git status --short && git log --on"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: BASH_ARGS, update: None, result: None, fault: None },
			FixtureState { args: BASH_ARGS, update: None, result: Some(BASH_RESULT), fault: None },
			FixtureState {
				args:   BASH_ARGS,
				update: Some(r#"{"wall_ms":5120,"exit":2}"#),
				result: None,
				fault:  Some(BASH_FAULT),
			},
		],
	},
	CardFixture {
		tool:   "eval",
		title:  "Eval",
		states: [
			FixtureState {
				args:   r#"{"language":"py","title":"load config","code":"import json\nfrom pathlib import Path\n\ndata = json.loads(Path(\"package.js"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: EVAL_ARGS, update: None, result: None, fault: None },
			FixtureState { args: EVAL_ARGS, update: None, result: Some(EVAL_RESULT), fault: None },
			FixtureState {
				args:   EVAL_FAILED_ARGS,
				update: Some(r#"{"wall_ms":41}"#),
				result: None,
				fault:  Some(EVAL_FAULT),
			},
		],
	},
	CardFixture {
		tool:   "ast_grep",
		title:  "AST Grep",
		states: [
			FixtureState {
				args:   r#"{"pat":"useState("}"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: AST_GREP_ARGS, update: None, result: None, fault: None },
			FixtureState {
				args:   AST_GREP_ARGS,
				update: None,
				result: Some(AST_GREP_RESULT),
				fault:  None,
			},
			FixtureState {
				args:   AST_GREP_ARGS,
				update: None,
				result: None,
				fault:  Some(
					r#""Pattern parse error: incomplete node `useState(` — expected a closing `)`""#,
				),
			},
		],
	},
	CardFixture {
		tool:   "ast_edit",
		title:  "AST Edit",
		states: [
			FixtureState {
				args:   r#"{"ops":[{"pat":"countEditFiles($$$ARGS)"}],"paths":["packages/coding-agent/src/**/*.ts"]}"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: AST_EDIT_ARGS, update: None, result: None, fault: None },
			FixtureState {
				args:   AST_EDIT_ARGS,
				update: None,
				result: Some(AST_EDIT_RESULT),
				fault:  None,
			},
			FixtureState {
				args:   r#"{"ops":[{"pat":"countEditFiles($$$ARGS","out":"countDistinctFiles($$$ARGS)"}],"paths":["packages/coding-agent/src/**/*.ts"]}"#,
				update: None,
				result: None,
				fault:  Some(
					r#""Pattern parse error in ops[0].pat: unbalanced parenthesis in `countEditFiles($$$ARGS`""#,
				),
			},
		],
	},
	CardFixture {
		tool:   "lsp",
		title:  "LSP",
		states: [
			FixtureState {
				args:   r#"{"action":"references","file":"src/server/auth.ts"}"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: LSP_ARGS, update: None, result: None, fault: None },
			FixtureState { args: LSP_ARGS, update: None, result: Some(LSP_RESULT), fault: None },
			FixtureState {
				args:   LSP_ARGS,
				update: None,
				result: None,
				fault:  Some(r#""No language server found for this file""#),
			},
		],
	},
	CardFixture {
		tool:   "computer",
		title:  "",
		states: [
			FixtureState { args: "{}", update: None, result: None, fault: None },
			FixtureState { args: "{}", update: None, result: None, fault: None },
			FixtureState { args: "{}", update: None, result: Some("{}"), fault: None },
			FixtureState { args: "{}", update: None, result: None, fault: Some(r#""error""#) },
		],
	},
	CardFixture {
		tool:   "browser",
		title:  "Browser",
		states: [
			FixtureState {
				args:   r#"{"action":"run","name":"docs","code":"const obs = await tab.observe();\n"}"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: BROWSER_ARGS, update: None, result: None, fault: None },
			FixtureState {
				args:   BROWSER_ARGS,
				update: None,
				result: Some(BROWSER_RESULT),
				fault:  None,
			},
			FixtureState {
				args:   BROWSER_ARGS,
				update: Some(
					r#"{"action":"run","name":"docs","url":"https://bun.sh/docs","browser":"headless"}"#,
				),
				result: None,
				fault:  Some(BROWSER_FAULT),
			},
		],
	},
	CardFixture {
		tool:   "inspect_image",
		title:  "Inspect Image",
		states: [
			FixtureState {
				args:   r#"{"path":"docs/assets/dashboard-mock.png"}"#,
				update: None,
				result: None,
				fault:  None,
			},
			FixtureState { args: INSPECT_ARGS, update: None, result: None, fault: None },
			FixtureState {
				args:   INSPECT_ARGS,
				update: None,
				result: Some(INSPECT_RESULT),
				fault:  None,
			},
			FixtureState {
				args:   INSPECT_ARGS,
				update: None,
				result: None,
				fault:  Some(r#""Image not found: docs/assets/dashboard-mock.png""#),
			},
		],
	},
];

#[cfg(test)]
mod tests {
	use crate::gallery::{GalleryState, render_sections};

	#[test]
	fn execution_cards_materialize_every_lifecycle() {
		for tool in
			["bash", "eval", "ast_grep", "ast_edit", "lsp", "computer", "browser", "inspect_image"]
		{
			let sections = render_sections(Some(tool), &GalleryState::ALL, 100, false)
				.unwrap_or_else(|error| panic!("{tool}: {error}"));
			assert_eq!(sections.len(), GalleryState::ALL.len());
		}
	}
}
