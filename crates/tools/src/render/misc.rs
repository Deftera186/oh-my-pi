//! Native GitHub, browser, and computer renderers.

use std::fmt::Write as _;

use omp_core::{Str, sf};
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};
use serde_json::Value;

use super::{live_view, view::El};
use crate::{
	browser::{
		Action as BrowserAction, Fault as BrowserFault, Payload as BrowserPayload,
		Update as BrowserUpdate,
	},
	computer::{
		Action as ComputerAction, Fault as ComputerFault, Payload as ComputerPayload,
		Update as ComputerUpdate,
	},
	gallery::RendererGalleryFixture,
	github::{
		Fault as GithubFault, Operation as GithubOperation, Payload as GithubPayload,
		Update as GithubUpdate,
	},
	view,
};

#[derive(Default)]
pub(super) struct GithubState {
	op:    Option<GithubOperation>,
	repo:  Option<Str>,
	query: Option<Str>,
}

pub(super) struct GithubRenderer;

impl RenderFold for GithubRenderer {
	type Outcome = CallOutcome<GithubPayload, GithubFault>;
	type State = GithubState;
	type Update = GithubUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn fold_args(&self, state: &mut Self::State, args: &omp_slopjson::Value, _complete: bool) {
		state.op = args
			.get("op")
			.and_then(|value| value.deserialize_into::<GithubOperation>().ok());
		state.repo = args
			.get("repo")
			.and_then(omp_slopjson::Value::as_str)
			.map(Str::new);
		state.query = args
			.get("query")
			.and_then(omp_slopjson::Value::as_str)
			.map(Str::new);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(render_github_live(state).into()),
			Some(CallOutcome::Ok(payload)) => Some(render_github_payload(state, payload).into()),
			Some(CallOutcome::Faulted(fault)) => Some(render_github_fault(state, fault).into()),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn render_github_live(state: &GithubState) -> El {
	let action = state
		.op
		.map_or_else(|| Str::new_static("GitHub request"), |op| sf!("{op}"));
	view! {
		<row gap=1>
			<spinner/>
			<text bold>{action}</text>
			if let Some(repo) = state.repo.as_deref() {
				<text fg=accent>{"["}{repo}{"]"}</text>
			}
			if let Some(query) = state.query.as_deref() {
				<text fg=muted truncate>{query}</text>
			}
		</row>
	}
}

fn render_github_payload(state: &GithubState, payload: &GithubPayload) -> El {
	if matches!(
		payload.op,
		GithubOperation::SearchIssues
			| GithubOperation::SearchPrs
			| GithubOperation::SearchCode
			| GithubOperation::SearchCommits
			| GithubOperation::SearchRepos
	) {
		return render_github_search(state, payload);
	}

	let content = (payload.op == GithubOperation::FileRead)
		.then(|| payload.result.get("content").and_then(Value::as_str))
		.flatten();
	view! {
		<col gap=0>
			{github_header(state, payload)}
			if let Some(content) = content {
				<pre max-rows=40 overflow="lines">{content}</pre>
			} else {
				{json_view(&payload.result, 3, 10, 100)}
			}
			if let Some(remaining) = payload.rate_limit_remaining {
				<text fg=muted>{sf!("{remaining} API requests remaining")}</text>
			}
		</col>
	}
}

fn render_github_search(state: &GithubState, payload: &GithubPayload) -> El {
	let items = payload
		.result
		.get("items")
		.and_then(Value::as_array)
		.or_else(|| payload.result.as_array());
	let count = payload
		.result
		.get("total_count")
		.and_then(Value::as_u64)
		.unwrap_or_else(|| items.map_or(0, |items| items.len() as u64));
	let summary = payload
		.result
		.get("summary")
		.and_then(Value::as_str)
		.map_or_else(|| sf!("{count} results"), Str::new);
	view! {
		<col gap=0>
			{github_header(state, payload)}
			if let Some(items) = items {
				if items.is_empty() {
					<text fg=muted>{"No results"}</text>
				} else {
					<table max-rows=20 overflow="results">
						for item in items {
							{github_search_row(item)}
						}
					</table>
				}
			}
			<row gap=1><text fg=muted>{summary}</text></row>
			if let Some(remaining) = payload.rate_limit_remaining {
				<text fg=muted>{sf!("{remaining} API requests remaining")}</text>
			}
		</col>
	}
}

fn github_header(state: &GithubState, payload: &GithubPayload) -> El {
	let repo = state
		.repo
		.as_deref()
		.or_else(|| payload.result.get("repo").and_then(Value::as_str))
		.or_else(|| payload.result.get("full_name").and_then(Value::as_str));
	view! {
		<row gap=1>
			<text bold>{sf!("{}", payload.op)}</text>
			if let Some(repo) = repo {
				<text fg=accent>{"["}{repo}{"]"}</text>
			}
			if let Some(query) = state.query.as_deref() {
				<text fg=muted truncate>{query}</text>
			}
		</row>
	}
}

fn github_search_row(item: &Value) -> El {
	let number = item.get("number").and_then(Value::as_u64);
	let title = item
		.get("title")
		.and_then(Value::as_str)
		.or_else(|| item.get("name").and_then(Value::as_str))
		.unwrap_or("untitled");
	let author = item
		.get("author")
		.and_then(Value::as_str)
		.or_else(|| item.pointer("/user/login").and_then(Value::as_str))
		.or_else(|| item.pointer("/owner/login").and_then(Value::as_str))
		.unwrap_or("unknown");
	let age = item
		.get("age")
		.and_then(Value::as_str)
		.or_else(|| item.get("updated").and_then(Value::as_str))
		.or_else(|| item.get("updated_at").and_then(Value::as_str));
	let additions = item
		.get("additions")
		.and_then(Value::as_u64)
		.or_else(|| item.pointer("/diff/additions").and_then(Value::as_u64));
	let deletions = item
		.get("deletions")
		.and_then(Value::as_u64)
		.or_else(|| item.pointer("/diff/deletions").and_then(Value::as_u64));
	let number_text = number.map_or_else(|| Str::new_static("—"), |number| sf!("#{number}"));

	view! {
		<tr>
			<td w=7><text fg=info bold>{number_text}</text></td>
			<td grow truncate><text>{title}</text></td>
			<td w=24 truncate>
				<text fg=muted>
					{author}
					if let Some(age) = age {
						{" · "}{age}
					}
				</text>
			</td>
			<td w=15 align="right">
				if let Some(additions) = additions {
					<text fg=ok>{sf!("+{additions}")}</text>
				}
				if additions.is_some() && deletions.is_some() {
					{" "}
				}
				if let Some(deletions) = deletions {
					<text fg=err>{sf!("-{deletions}")}</text>
				}
			</td>
		</tr>
	}
}

fn render_github_fault(state: &GithubState, fault: &GithubFault) -> El {
	let action = state
		.op
		.map_or_else(|| Str::new_static("GitHub request"), |op| sf!("{op}"));
	view! {
		<col gap=0>
			<row gap=1>
				<text bold fg=err>{action}</text>
				if let Some(repo) = state.repo.as_deref() {
					<text fg=muted>{"["}{repo}{"]"}</text>
				}
			</row>
			<callout kind="error">{&fault.message}</callout>
		</col>
	}
}

#[derive(Default)]
pub(super) struct BrowserState {
	action: Option<BrowserAction>,
	name:   Option<Str>,
	url:    Option<Str>,
	code:   Option<Str>,
}

pub(super) struct BrowserRenderer;

impl RenderFold for BrowserRenderer {
	type Outcome = CallOutcome<BrowserPayload, BrowserFault>;
	type State = BrowserState;
	type Update = BrowserUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn fold_args(&self, state: &mut Self::State, args: &omp_slopjson::Value, _complete: bool) {
		state.action = args
			.get("action")
			.and_then(|value| value.deserialize_into::<BrowserAction>().ok());
		state.name = args
			.get("name")
			.and_then(omp_slopjson::Value::as_str)
			.map(Str::new);
		state.url = args
			.get("url")
			.and_then(omp_slopjson::Value::as_str)
			.map(Str::new);
		state.code = args
			.get("code")
			.and_then(omp_slopjson::Value::as_str)
			.map(Str::new);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(render_browser_live(state).into()),
			Some(CallOutcome::Ok(payload)) => Some(render_browser_payload(state, payload).into()),
			Some(CallOutcome::Faulted(fault)) => Some(render_browser_fault(state, fault).into()),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn render_browser_live(state: &BrowserState) -> El {
	let Some(action) = state.action else {
		return live_view("browser", "waiting for a browser action");
	};
	view! {
		<col gap=0>
			{browser_header(
				action,
				state.name.as_deref().unwrap_or("main"),
				state.url.as_deref(),
				None,
				true,
			)}
			if action == BrowserAction::Run {
				if let Some(code) = state.code.as_deref().filter(|code| !code.is_empty()) {
					<text bold>{"Code"}</text>
					<pre max-rows=16 overflow="lines">{code}</pre>
				}
			}
		</col>
	}
}

fn render_browser_payload(state: &BrowserState, payload: &BrowserPayload) -> El {
	view! {
		<col gap=0>
			{browser_header(
				payload.action,
				&payload.name,
				payload.url.as_deref().or(state.url.as_deref()),
				payload.title.as_deref(),
				false,
			)}
			if payload.action == BrowserAction::Run {
				if let Some(code) = state.code.as_deref().filter(|code| !code.is_empty()) {
					<text bold>{"Code"}</text>
					<pre max-rows=16 overflow="lines">{code}</pre>
				}
				if let Some(result) = payload.result.as_ref() {
					<text bold>{"Output"}</text>
					{browser_output(result)}
				}
			} else if let Some(result) = payload.result.as_ref() {
				{json_view(result, 3, 6, 100)}
			}
			if !payload.artifacts.is_empty() {
				<text bold>{"Artifacts"}</text>
				for artifact in &payload.artifacts {
					<fact label="Artifact"><text fg=accent>{artifact}</text></fact>
				}
			}
		</col>
	}
}

fn render_browser_fault(state: &BrowserState, fault: &BrowserFault) -> El {
	view! {
		<col gap=0>
			if let Some(action) = state.action {
				{browser_header(
					action,
					state.name.as_deref().unwrap_or("main"),
					state.url.as_deref(),
					None,
					false,
				)}
				if action == BrowserAction::Run {
					if let Some(code) = state.code.as_deref().filter(|code| !code.is_empty()) {
						<text bold>{"Code"}</text>
						<pre max-rows=16 overflow="lines">{code}</pre>
					}
				}
			} else {
				<text bold fg=err>{"Browser action failed"}</text>
			}
			<callout kind="error">{&fault.message}</callout>
		</col>
	}
}

fn browser_header(
	action: BrowserAction,
	name: &str,
	url: Option<&str>,
	title: Option<&str>,
	live: bool,
) -> El {
	view! {
		<row gap=1>
			if live {
				<spinner/>
			}
			<text bold>{sf!(r#"{action} tab "{name}""#)}</text>
			if let Some(title) = title {
				<text truncate>{title}</text>
			}
			if let Some(url) = url {
				<text fg=muted truncate>{url}</text>
			}
			<text fg=secondary>{"headless"}</text>
		</row>
	}
}

fn browser_output(result: &Value) -> El {
	let Some(object) = result.as_object() else {
		return view! { <col gap=0>{labeled_value("return", result)}</col> };
	};
	let displays = object
		.get("display_outputs")
		.or_else(|| object.get("display"));
	let returned = object.get("return_value").or_else(|| object.get("return"));
	if displays.is_none() && returned.is_none() {
		return view! { <col gap=0>{labeled_value("return", result)}</col> };
	}

	view! {
		<col gap=0>
			if let Some(displays) = displays {
				if let Some(values) = displays.as_array() {
					for value in values {
						{labeled_value("display", value)}
					}
				} else {
					{labeled_value("display", displays)}
				}
			}
			if let Some(returned) = returned {
				{labeled_value("return", returned)}
			}
		</col>
	}
}

fn labeled_value(label: &'static str, value: &Value) -> El {
	view! {
		<fact label={label}>{json_view(value, 4, 10, 100)}</fact>
	}
}

#[derive(Default)]
pub(super) struct ComputerState {
	action:  Option<ComputerAction>,
	summary: Str,
}

pub(super) struct ComputerRenderer;

impl RenderFold for ComputerRenderer {
	type Outcome = CallOutcome<ComputerPayload, ComputerFault>;
	type State = ComputerState;
	type Update = ComputerUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn fold_args(&self, state: &mut Self::State, args: &omp_slopjson::Value, _complete: bool) {
		state.action = args
			.get("action")
			.and_then(|value| value.deserialize_into::<ComputerAction>().ok());
		state.summary = computer_arg_summary(args);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(render_computer_live(state).into()),
			Some(CallOutcome::Ok(payload)) => Some(render_computer_payload(state, payload).into()),
			Some(CallOutcome::Faulted(fault)) => Some(render_computer_fault(state, fault).into()),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn computer_arg_summary(args: &omp_slopjson::Value) -> Str {
	let mut summary = String::new();
	if args
		.get("read_only")
		.and_then(omp_slopjson::Value::as_bool)
		.unwrap_or(false)
	{
		summary.push_str("read-only");
	}
	let window = args.get("window").and_then(omp_slopjson::Value::as_str);
	if let Some(window) = window {
		push_summary_field(&mut summary, "window", window);
	} else if matches!(
		args
			.get("action")
			.and_then(|value| value.deserialize_into::<ComputerAction>().ok()),
		Some(ComputerAction::Capture)
	) {
		push_summary_field(&mut summary, "", "primary display");
	}
	if let Some(reference) = args.get("reference").and_then(omp_slopjson::Value::as_str) {
		push_summary_field(&mut summary, "ref", reference);
	}
	if let Some(value) = args.get("value").and_then(omp_slopjson::Value::as_str) {
		push_summary_field(&mut summary, "value", value);
	}
	if let (Some(x), Some(y)) = (
		args.get("x").and_then(omp_slopjson::Value::as_f64),
		args.get("y").and_then(omp_slopjson::Value::as_f64),
	) {
		if !summary.is_empty() {
			summary.push_str(" · ");
		}
		write!(summary, "at {x:.0},{y:.0}").expect("writing to String cannot fail");
	}
	if let (Some(dx), Some(dy)) = (
		args.get("dx").and_then(omp_slopjson::Value::as_f64),
		args.get("dy").and_then(omp_slopjson::Value::as_f64),
	) {
		if !summary.is_empty() {
			summary.push_str(" · ");
		}
		write!(summary, "delta {dx:.0},{dy:.0}").expect("writing to String cannot fail");
	}
	if let Some(points) = args.get("points").and_then(omp_slopjson::Value::as_array) {
		if !summary.is_empty() {
			summary.push_str(" · ");
		}
		write!(summary, "{} points", points.len()).expect("writing to String cannot fail");
	}
	let width = args.get("max_width").and_then(omp_slopjson::Value::as_u64);
	let height = args.get("max_height").and_then(omp_slopjson::Value::as_u64);
	if width.is_some() || height.is_some() {
		if !summary.is_empty() {
			summary.push_str(" · ");
		}
		match (width, height) {
			(Some(width), Some(height)) => {
				write!(summary, "up to {width}×{height}").expect("writing to String cannot fail");
			},
			(Some(width), None) => {
				write!(summary, "up to {width}px wide").expect("writing to String cannot fail");
			},
			(None, Some(height)) => {
				write!(summary, "up to {height}px tall").expect("writing to String cannot fail");
			},
			(None, None) => {},
		}
	}
	if summary.is_empty() {
		summary.push_str("desktop session");
	}
	Str::new(summary)
}

fn push_summary_field(output: &mut String, label: &str, value: &str) {
	if !output.is_empty() {
		output.push_str(" · ");
	}
	if !label.is_empty() {
		output.push_str(label);
		output.push(' ');
	}
	output.push_str(value);
}

fn render_computer_live(state: &ComputerState) -> El {
	let Some(action) = state.action else {
		return live_view("computer", "waiting for desktop access");
	};
	view! {
		<row gap=1>
			<spinner/>
			<text bold>{sf!("{action}")}</text>
			<text fg=muted>{&state.summary}</text>
		</row>
	}
}

fn render_computer_payload(state: &ComputerState, payload: &ComputerPayload) -> El {
	view! {
		<col gap=0>
			<row gap=1>
				<text bold>{sf!("{}", payload.action)}</text>
				if !state.summary.is_empty() {
					<text fg=muted>{&state.summary}</text>
				}
			</row>
			if let Some(message) = payload.result.get("message").and_then(Value::as_str) {
				<text fg=ok>{message}</text>
			} else {
				{json_view(&payload.result, 3, 6, 100)}
			}
			for artifact in &payload.artifacts {
				<fact label="Screenshot"><text fg=accent>{artifact}</text></fact>
			}
		</col>
	}
}

fn render_computer_fault(state: &ComputerState, fault: &ComputerFault) -> El {
	let action = state
		.action
		.map_or_else(|| Str::new_static("Desktop action"), |action| sf!("{action}"));
	view! {
		<col gap=0>
			<row gap=1>
				<text bold fg=err>{action}</text>
				if state.action.is_some() {
					<text fg=muted>{&state.summary}</text>
				}
			</row>
			<callout kind="error">{&fault.message}</callout>
		</col>
	}
}

fn json_view(value: &Value, max_depth: usize, max_rows: usize, max_chars: usize) -> El {
	view! {
		<json max-depth={max_depth} max-rows={max_rows} max-chars={max_chars}>
			{value.to_string()}
		</json>
	}
}

/// Native GitHub, browser, and computer lifecycle fixtures for the visual QA
/// gallery.
pub(crate) fn gallery_fixtures(
	github: ToolIdentity,
	browser: ToolIdentity,
	computer: ToolIdentity,
) -> Vec<RendererGalleryFixture> {
	vec![
		RendererGalleryFixture {
			identity: github,
			streaming_args: r#"{"op":"search_prs","query":"is:open review-requested:@me sort:up"#,
			args: r#"{"op":"search_prs","query":"is:open review-requested:@me sort:updated","repo":"oh-my-pi/pi","limit":10}"#,
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"op":"search_prs","result":{"repo":"oh-my-pi/pi","total_count":4,"summary":"4 open pull requests requesting your review","items":[{"number":1842,"title":"feat(tui): virtualized scrollback for tool output","author":"openyou","age":"2h ago","additions":312,"deletions":47},{"number":1839,"title":"fix(agent): retry stream on transient 529","author":"dvir","age":"5h ago","additions":18,"deletions":4},{"number":1830,"title":"refactor(edit): unify hashline + ast_edit previews","author":"mira","age":"1d ago","additions":540,"deletions":210},{"number":1817,"title":"docs: document gallery fixtures contract","author":"leo","age":"2d ago","additions":96,"deletions":0}]},"rate_limit_remaining":4876,"rate_limit_reset":null}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"code":"github_http","message":"gh: Could not resolve to a Repository with the name 'oh-my-pi/pi'. (HTTP 404)"}}"#,
		},
		RendererGalleryFixture {
			identity: browser,
			streaming_args: r#"{"action":"run","name":"docs","url":"https://bun.sh/docs","code":"const obs = await tab.observe();\nconst heading = obs.elements.find(e => e.role === 'head"#,
			args: r#"{"action":"run","name":"docs","url":"https://bun.sh/docs","code":"const obs = await tab.observe();\nconst heading = obs.elements.find(e => e.role === 'heading');\ndisplay({ url: obs.url, title: obs.title, headings: obs.elements.filter(e => e.role === 'heading').length });\nreturn heading?.name ?? 'no heading found';"}"#,
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"action":"run","name":"docs","url":"https://bun.sh/docs","title":"Bun Documentation","result":{"display_outputs":[{"url":"https://bun.sh/docs","title":"Bun Documentation","headings":14}],"return_value":"Get started with Bun"},"artifacts":[]}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"code":"browser_timeout","message":"TimeoutError: waiting for selector `aria/Sign in` failed: timeout 30000ms exceeded\n    at Tab.waitFor (browser/tab.ts:212:13)\n    at run (eval:3:7)"}}"#,
		},
		RendererGalleryFixture {
			identity: computer,
			streaming_args: r#"{"action":"capture","read_only":true,"max_width":1440,"max_height":9"#,
			args: r#"{"action":"capture","read_only":true,"max_width":1440,"max_height":900}"#,
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"action":"capture","result":{"message":"Captured 1440\u00d7900 screenshot","width":1440,"height":900,"bytes":482193},"artifacts":["artifact://sha256/8f9b0dd1e9c0a05d4f1d6d2ae9742d7a"]}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"code":"desktop_permission","message":"Screen Recording permission is required to capture the primary display"}}"#,
		},
	]
}

#[cfg(test)]
mod tests {
	use omp_core::Str;
	use omp_tool::{CallOutcome, Rev, ToolIdentity, render::RenderFold};

	use super::*;

	fn identity(name: &'static str) -> ToolIdentity {
		ToolIdentity { name: Str::new_static(name), rev: Rev { family: Str::default(), n: 1 } }
	}

	#[test]
	fn fixture_wire_shapes_decode_and_streaming_args_render() {
		let fixtures =
			gallery_fixtures(identity("github"), identity("browser"), identity("computer"));
		for fixture in &fixtures {
			assert!(omp_slopjson::parse(fixture.streaming_args).is_err());
			omp_slopjson::parse(fixture.args).expect("committed args decode");
			assert!(fixture.progress_update.is_none());
		}

		let mut github_state = GithubState::default();
		GithubRenderer.fold_args(
			&mut github_state,
			&omp_slopjson::parse_streaming(fixtures[0].streaming_args),
			false,
		);
		assert!(
			GithubRenderer
				.view(&github_state, None)
				.expect("streaming GitHub args render")
				.contains("sort:up")
		);
		let mut browser_state = BrowserState::default();
		BrowserRenderer.fold_args(
			&mut browser_state,
			&omp_slopjson::parse_streaming(fixtures[1].streaming_args),
			false,
		);
		assert!(
			BrowserRenderer
				.view(&browser_state, None)
				.expect("streaming browser args render")
				.contains("role === 'head")
		);
		let mut computer_state = ComputerState::default();
		ComputerRenderer.fold_args(
			&mut computer_state,
			&omp_slopjson::parse_streaming(fixtures[2].streaming_args),
			false,
		);
		assert!(
			ComputerRenderer
				.view(&computer_state, None)
				.expect("streaming computer args render")
				.contains("up to 1440")
		);

		serde_json::from_slice::<CallOutcome<GithubPayload, GithubFault>>(
			fixtures[0].success_outcome,
		)
		.expect("GitHub success outcome decodes");
		serde_json::from_slice::<CallOutcome<GithubPayload, GithubFault>>(fixtures[0].error_outcome)
			.expect("GitHub error outcome decodes");
		serde_json::from_slice::<CallOutcome<BrowserPayload, BrowserFault>>(
			fixtures[1].success_outcome,
		)
		.expect("browser success outcome decodes");
		serde_json::from_slice::<CallOutcome<BrowserPayload, BrowserFault>>(
			fixtures[1].error_outcome,
		)
		.expect("browser error outcome decodes");
		serde_json::from_slice::<CallOutcome<ComputerPayload, ComputerFault>>(
			fixtures[2].success_outcome,
		)
		.expect("computer success outcome decodes");
		serde_json::from_slice::<CallOutcome<ComputerPayload, ComputerFault>>(
			fixtures[2].error_outcome,
		)
		.expect("computer error outcome decodes");
	}

	#[test]
	fn github_search_uses_aligned_rows_and_escapes_payload_text() {
		let payload = GithubPayload {
			op:                   GithubOperation::SearchPrs,
			result:               serde_json::json!({
				"items": [{
					"number": 7,
					"title": "Fix <unsafe> & parsing",
					"author": "mira",
					"age": "2h ago",
					"additions": 12,
					"deletions": 3
				}],
				"total_count": 1
			}),
			rate_limit_remaining: None,
			rate_limit_reset:     None,
		};
		let rendered = GithubRenderer
			.view(&GithubState::default(), Some(&CallOutcome::Ok(payload)))
			.expect("GitHub search renders");
		assert!(rendered.contains("<table max-rows=20 overflow=results>"));
		assert!(rendered.contains("#7"));
		assert!(rendered.contains("Fix &lt;unsafe&gt; &amp; parsing"));
		assert!(rendered.contains("<text fg=ok>+12</text>"));
		assert!(rendered.contains("<text fg=err>-3</text>"));
		assert!(!rendered.contains("ctrl+o"));
	}

	#[test]
	fn github_file_and_structured_results_delegate_bounding_to_primitives() {
		let file = GithubPayload {
			op:                   GithubOperation::FileRead,
			result:               serde_json::json!({"content": "first\nsecond <third>"}),
			rate_limit_remaining: Some(42),
			rate_limit_reset:     None,
		};
		let rendered = GithubRenderer
			.view(&GithubState::default(), Some(&CallOutcome::Ok(file)))
			.expect("GitHub file renders");
		assert!(rendered.contains("<pre max-rows=40 overflow=lines>"));
		assert!(rendered.contains("second &lt;third&gt;"));
		assert!(rendered.contains("42 API requests remaining"));
		assert!(!rendered.contains("ctrl+o"));

		let repo = GithubPayload {
			op:                   GithubOperation::RepoView,
			result:               serde_json::json!({"full_name": "oh-my-pi/pi", "private": false}),
			rate_limit_remaining: None,
			rate_limit_reset:     None,
		};
		let rendered = GithubRenderer
			.view(&GithubState::default(), Some(&CallOutcome::Ok(repo)))
			.expect("GitHub structured result renders");
		assert!(
			rendered.contains("<json max-depth=3 max-rows=10 max-chars=100>"),
			"rendered: {rendered}"
		);
		assert!(rendered.contains("\"full_name\":\"oh-my-pi/pi\""), "rendered: {rendered}");
		assert!(rendered.contains("\"private\":false"), "rendered: {rendered}");
	}

	#[test]
	fn browser_failure_retains_code_context_and_trace() {
		let mut state = BrowserState::default();
		BrowserRenderer.fold_args(
			&mut state,
			&omp_slopjson::parse(
				r#"{"action":"run","name":"docs","url":"https://bun.sh/docs","code":"await tab.waitFor('aria/Sign in')"}"#,
			)
			.expect("browser args decode"),
			true,
		);
		let fault = BrowserFault {
			code:    Str::new_static("browser_timeout"),
			message: Str::new_static("TimeoutError: selector <missing>"),
		};
		let rendered = BrowserRenderer
			.view(&state, Some(&CallOutcome::Faulted(fault)))
			.expect("browser failure renders");
		assert!(rendered.contains("await tab.waitFor"));
		assert!(rendered.contains("<pre max-rows=16 overflow=lines>"));
		assert!(rendered.contains("<callout kind=error>"));
		assert!(rendered.contains("TimeoutError: selector &lt;missing&gt;"));
		assert!(rendered.contains("headless"));
	}

	#[test]
	fn browser_results_use_json_facts_and_artifact_facts() {
		let mut state = BrowserState::default();
		state.code = Some(Str::new_static("return { ok: true };"));
		let payload = BrowserPayload {
			action:    BrowserAction::Run,
			name:      Str::new_static("main"),
			url:       Some(Str::new_static("https://example.test")),
			title:     Some(Str::new_static("Example")),
			result:    Some(serde_json::json!({
				"display_outputs": [{"value": "<shown>"}],
				"return_value": true
			})),
			artifacts: vec![Str::new_static("artifact://browser/result")],
		};
		let rendered = BrowserRenderer
			.view(&state, Some(&CallOutcome::Ok(payload)))
			.expect("browser result renders");
		assert!(rendered.contains("<fact label=display><json"));
		assert!(rendered.contains("<fact label=return><json"));
		assert!(rendered.contains("&lt;shown&gt;"));
		assert!(rendered.contains("<fact label=Artifact>"));
		assert!(!rendered.contains("ctrl+o"));

		let open = BrowserPayload {
			action:    BrowserAction::Open,
			name:      Str::new_static("docs"),
			url:       Some(Str::new_static("https://example.test/docs")),
			title:     Some(Str::new_static("Documentation")),
			result:    Some(serde_json::json!({"ready": true, "status": "<loaded>"})),
			artifacts: Vec::new(),
		};
		let rendered = BrowserRenderer
			.view(&BrowserState::default(), Some(&CallOutcome::Ok(open)))
			.expect("browser open renders");
		assert!(rendered.contains(r#"open tab "docs""#));
		assert!(rendered.contains("https://example.test/docs"));
		assert!(rendered.contains("Documentation"));
		assert!(rendered.contains("<json max-depth=3 max-rows=6 max-chars=100>"));
		assert!(rendered.contains("&lt;loaded&gt;"));
	}

	#[test]
	fn computer_structured_result_artifact_and_fault_are_semantic() {
		let payload = ComputerPayload {
			action:    ComputerAction::Capture,
			result:    serde_json::json!({"width": 1440, "height": 900}),
			artifacts: vec![Str::new_static("artifact://computer/screenshot")],
		};
		let rendered = ComputerRenderer
			.view(&ComputerState::default(), Some(&CallOutcome::Ok(payload)))
			.expect("computer result renders");
		assert!(rendered.contains("<json max-depth=3 max-rows=6 max-chars=100>"));
		assert!(rendered.contains("<fact label=Screenshot>"));
		assert!(rendered.contains("artifact://computer/screenshot"));

		let fault = ComputerFault {
			code:    Str::new_static("desktop_permission"),
			message: Str::new_static("Screen Recording <required>"),
		};
		let rendered = ComputerRenderer
			.view(&ComputerState::default(), Some(&CallOutcome::Faulted(fault)))
			.expect("computer fault renders");
		assert!(rendered.contains("<callout kind=error>"));
		assert!(rendered.contains("Screen Recording &lt;required&gt;"));
	}
}
