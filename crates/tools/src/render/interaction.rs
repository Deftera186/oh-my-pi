//! Native ask, todo, and think renderers.

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};

use super::view::El;
use crate::{
	ask::{
		Answer as AskAnswer, Fault as AskFault, Payload as AskPayload, Question as AskQuestion,
		Update as AskUpdate,
	},
	gallery::RendererGalleryFixture,
	think::{Fault as ThinkFault, Payload as ThinkPayload, Update as ThinkUpdate},
	todo::{
		Fault as TodoFault, Item as TodoItem, Payload as TodoPayload, Phase as TodoPhase,
		Status as TodoStatus, Update as TodoUpdate,
	},
	view,
};

#[derive(Default)]
pub(super) struct AskState {
	questions: Vec<AskQuestion>,
}

pub(super) struct AskRenderer;

impl RenderFold for AskRenderer {
	type Outcome = CallOutcome<AskPayload, AskFault>;
	type State = AskState;
	type Update = AskUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn fold_args(&self, state: &mut Self::State, args: &omp_slopjson::Value, _complete: bool) {
		let Some(questions) = args.get("questions").and_then(|value| value.as_array()) else {
			return;
		};
		state.questions.clear();
		state.questions.extend(
			questions
				.iter()
				.filter_map(|question| question.deserialize_into::<AskQuestion>().ok()),
		);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None if state.questions.is_empty() => {
				Some(view! { <spinner color=accent label="Waiting for answers"/> }.into())
			},
			None => Some(render_ask(&state.questions, None).into()),
			Some(CallOutcome::Ok(payload)) => {
				Some(render_ask(&state.questions, Some(&payload.answers)).into())
			},
			Some(CallOutcome::Faulted(fault)) => {
				let message = match fault {
					AskFault::Invalid { message } | AskFault::Presenter { message } => message,
				};
				Some(render_fault(message).into())
			},
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

#[derive(Default)]
pub(super) struct TodoState {
	op:     Option<Str>,
	phases: Vec<TodoPhase>,
}

pub(super) struct TodoRenderer;

impl RenderFold for TodoRenderer {
	type Outcome = CallOutcome<TodoPayload, TodoFault>;
	type State = TodoState;
	type Update = TodoUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn fold_args(&self, state: &mut Self::State, args: &omp_slopjson::Value, _complete: bool) {
		if let Some(op) = args.get("op").and_then(|value| value.as_str()) {
			state.op = Some(Str::new(op));
		}
		if let Some(phases) = args.get("list").and_then(|value| value.as_array()) {
			state.phases.clear();
			state.phases.extend(
				phases
					.iter()
					.filter_map(|phase| phase.deserialize_into::<TodoPhase>().ok()),
			);
		} else if let Some(items) = args.get("items").and_then(|value| value.as_array()) {
			let phase = args
				.get("phase")
				.and_then(|value| value.as_str())
				.unwrap_or("Todos");
			state.phases.clear();
			state.phases.push(TodoPhase {
				phase: Str::new(phase),
				items: items
					.iter()
					.filter_map(|item| item.as_str())
					.map(|text| TodoItem {
						text:   Str::new(text),
						status: TodoStatus::Pending,
						reason: None,
					})
					.collect(),
			});
		}
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None if !state.phases.is_empty() => Some(render_todo_phases(&state.phases).into()),
			None => Some(render_todo_live(state.op.as_deref()).into()),
			Some(CallOutcome::Ok(payload)) => Some(render_todo(payload).into()),
			Some(CallOutcome::Faulted(fault)) => Some(render_todo_fault(fault).into()),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

#[derive(Default)]
pub(super) struct ThinkState {
	thoughts: Option<Str>,
}

pub(super) struct ThinkRenderer;

impl RenderFold for ThinkRenderer {
	type Outcome = CallOutcome<ThinkPayload, ThinkFault>;
	type State = ThinkState;
	type Update = ThinkUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn fold_args(&self, state: &mut Self::State, args: &omp_slopjson::Value, _complete: bool) {
		if let Some(thoughts) = args.get("thoughts").and_then(|value| value.as_str()) {
			state.thoughts = Some(Str::new(thoughts));
		}
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => state
				.thoughts
				.as_deref()
				.map(|thoughts| render_thought(thoughts).into())
				.or_else(|| Some(view! { <spinner color=muted label="Recording thought"/> }.into())),
			Some(CallOutcome::Ok(_)) => state
				.thoughts
				.as_deref()
				.map(|thoughts| render_thought(thoughts).into()),
			Some(CallOutcome::Faulted(fault)) => Some(render_fault(fault.message()).into()),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}
fn render_ask(questions: &[AskQuestion], answers: Option<&[AskAnswer]>) -> El {
	view! {
		<col gap=1>
			for question in questions {
				{render_question(question, answers)}
			}
		</col>
	}
}

fn render_question(question: &AskQuestion, answers: Option<&[AskAnswer]>) -> El {
	let answer = answers.and_then(|items| items.iter().find(|item| item.id == question.id));
	view! {
		<col gap=0>
			<row sep=" · ">
				<fact label="ID">{&question.id}</fact>
				<fact label="Options"><num value={question.options.len()} compact/></fact>
				if question.multi { <fact label="Mode">{"multiple"}</fact> }
			</row>
			<text bold wrap="word">{&question.question}</text>
			for option in &question.options {
				<choice
					multi={question.multi}
					selected={answer.is_some_and(|item| item.selected.iter().any(|label| label == &option.label))}
				>
					{&option.label}
				</choice>
				if let Some(description) = option.description.as_deref()
					&& !description.trim().is_empty()
				{
					<text fg=muted wrap="word">{description}</text>
				}
			}
			if let Some(answer) = answer {
				for selected in &answer.selected {
					if !question.options.iter().any(|option| option.label == *selected) {
						<choice multi={question.multi} selected>{selected}</choice>
					}
				}
			}
		</col>
	}
}

fn render_todo(payload: &TodoPayload) -> El {
	render_todo_phases(&payload.phases)
}

fn render_todo_phases(phases: &[TodoPhase]) -> El {
	view! {
		<todo guides="round" numbering="roman">
			for phase in phases {
				<task label={&phase.phase}>
					for item in &phase.items {
						{render_todo_item(item)}
					}
				</task>
			}
		</todo>
	}
}

fn render_todo_item(item: &TodoItem) -> El {
	if item.status == TodoStatus::Blocked
		&& let Some(reason) = item.reason.as_deref()
	{
		view! {
			<task status={item.status.as_ref()} desc={reason}>{&item.text}</task>
		}
	} else {
		view! {
			<task status={item.status.as_ref()}>{&item.text}</task>
		}
	}
}

fn render_todo_live(op: Option<&str>) -> El {
	let Some(op) = op else {
		return view! { <spinner color=accent label="Updating task list"/> };
	};
	view! {
		<row sep=" · ">
			<spinner color=accent label="Updating task list"/>
			<fact label="Operation">{op}</fact>
		</row>
	}
}

fn render_todo_fault(fault: &TodoFault) -> El {
	render_fault(fault.message())
}

fn render_fault(message: &str) -> El {
	view! { <callout kind="error">{message}</callout> }
}

fn render_thought(thoughts: &str) -> El {
	view! { <text fg=muted dim italic wrap="word">{thoughts}</text> }
}
/// Native ask, todo, and think renderer lifecycle fixtures for the visual QA
/// gallery.
pub(crate) fn gallery_fixtures(
	ask: ToolIdentity,
	todo: ToolIdentity,
	think: ToolIdentity,
) -> Vec<RendererGalleryFixture> {
	vec![
		RendererGalleryFixture {
			identity: ask,
			title: "choose a database and v1 auth flows",
			streaming_args: r#"{"questions":[{"id":"db","question":"Which database should the new service use?","options":[{"label":"Postgres","description":"Relational, strong consistency, JSONB support"},{"label":"SQLite","description":"Embedded, zero-ops, great for single-node"},{"label":"MongoDB","description":"Document store, flexible schema"}],"recommended":0},{"id":"features","question":"Which auth flows should sh"#,
			args: r#"{"questions":[{"id":"db","question":"Which database should the new service use?","options":[{"label":"Postgres","description":"Relational, strong consistency, JSONB support"},{"label":"SQLite","description":"Embedded, zero-ops, great for single-node"},{"label":"MongoDB","description":"Document store, flexible schema"}],"recommended":0},{"id":"features","question":"Which auth flows should ship in v1?","options":[{"label":"Email + password"},{"label":"OAuth (Google, GitHub)"},{"label":"Magic links"},{"label":"SAML SSO","description":"Enterprise; can be deferred"}],"multi":true}]}"#,
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"answers":[{"id":"db","selected":["Postgres"],"timed_out":false},{"id":"features","selected":["Email + password","OAuth (Google, GitHub)","Custom <flow>"],"timed_out":false}],"headless":false}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"presenter","message":"Prompt cancelled by user before any answer was given"}}"#,
		},
		RendererGalleryFixture {
			identity: todo,
			title: "initialize the Foundation and Auth plan",
			streaming_args: r#"{"op":"init","list":[{"phase":"Foundation","items":[{"text":"Scaffold crate"},{"text":"Wire workspace"}]},{"phase":"Au"#,
			args: r#"{"op":"init","list":[{"phase":"Foundation","items":[{"text":"Scaffold crate"},{"text":"Wire workspace"}]},{"phase":"Auth","items":[{"text":"Port credential store"},{"text":"Wire OAuth providers"}]}]}"#,
			progress_update: None,
			success_outcome: br##"{"kind":"ok","value":{"phases":[{"phase":"Foundation","items":[{"text":"Scaffold crate","status":"completed","reason":null},{"text":"Wire workspace","status":"in_progress","reason":null}]},{"phase":"Auth","items":[{"text":"Port credential store","status":"pending","reason":null},{"text":"Wire OAuth providers","status":"pending","reason":null}]}],"rendered":"# Foundation\n- [x] Scaffold crate\n- [/] Wire workspace\n\n# Auth\n- [ ] Port credential store\n- [ ] Wire OAuth providers\n"}}"##,
			error_outcome: br#"{"kind":"faulted","value":{"kind":"missing","message":"Unknown phase 'Auth' - initialize the list first"}}"#,
		},
		RendererGalleryFixture {
			identity: think,
			title: "reflect on retry-loop latency",
			streaming_args: r#"{"thoughts":"The retry loop re-reads the config after every failure, which explains the doubled lat"#,
			args: r#"{"thoughts":"The retry loop re-reads the config after every failure, which explains the doubled latency. Cache the parsed config outside the loop, then re-check the invalidation path."}"#,
			progress_update: None,
			success_outcome: br#"{"kind":"ok","value":{"recorded":true}}"#,
			error_outcome: br#"{"kind":"faulted","value":{"message":"thoughts must not be empty"}}"#,
		},
	]
}
#[cfg(test)]
mod tests {
	use omp_tool::Rev;

	use super::*;

	fn identity(name: &'static str) -> ToolIdentity {
		ToolIdentity { name: Str::new_static(name), rev: Rev { family: Str::new_static(""), n: 1 } }
	}

	#[test]
	fn fixtures_decode_and_render_rich_lifecycle_states() {
		let fixtures = gallery_fixtures(identity("ask"), identity("todo"), identity("think"));

		let ask_outcome =
			serde_json::from_slice::<CallOutcome<AskPayload, AskFault>>(fixtures[0].success_outcome)
				.expect("ask outcome decodes");
		let ask_fault =
			serde_json::from_slice::<CallOutcome<AskPayload, AskFault>>(fixtures[0].error_outcome)
				.expect("ask fault decodes");
		let mut ask_state = AskState::default();
		let streaming = omp_slopjson::parse_streaming(fixtures[0].streaming_args);
		AskRenderer.fold_args(&mut ask_state, &streaming, false);
		let live = AskRenderer
			.view(&ask_state, None)
			.expect("ask live view renders");
		assert!(live.contains("<row sep=\" · \"><fact label=ID>db</fact>"));
		assert!(live.contains("<fact label=Options><num value=3 compact/></fact>"));
		assert!(live.contains("<choice>Postgres</choice>"));
		assert!(live.contains("Relational, strong consistency, JSONB support"));
		assert!(!live.contains('↳'));
		let committed = omp_slopjson::parse(fixtures[0].args).expect("ask args decode");
		AskRenderer.fold_args(&mut ask_state, &committed, true);
		let success = AskRenderer
			.view(&ask_state, Some(&ask_outcome))
			.expect("ask success renders");
		assert!(success.contains("<choice selected>Postgres</choice>"));
		assert!(success.contains("<choice multi selected>Email + password</choice>"));
		assert!(success.contains("<choice multi selected>Custom &lt;flow&gt;</choice>"));
		assert!(success.contains("Relational, strong consistency, JSONB support"));
		assert!(!success.contains('●'));
		assert!(!success.contains('○'));
		assert!(
			AskRenderer
				.view(&ask_state, Some(&ask_fault))
				.expect("ask fault renders")
				.as_str()
				== "<callout kind=error>Prompt cancelled by user before any answer was given</callout>"
		);

		let todo_outcome =
			serde_json::from_slice::<CallOutcome<TodoPayload, TodoFault>>(fixtures[1].success_outcome)
				.expect("todo outcome decodes");
		let todo_fault =
			serde_json::from_slice::<CallOutcome<TodoPayload, TodoFault>>(fixtures[1].error_outcome)
				.expect("todo fault decodes");
		let mut todo_state = TodoState::default();
		let streaming = omp_slopjson::parse_streaming(fixtures[1].streaming_args);
		TodoRenderer.fold_args(&mut todo_state, &streaming, false);
		let live = TodoRenderer
			.view(&todo_state, None)
			.expect("todo live view renders");
		assert!(live.contains("label=Foundation"));
		assert!(live.contains("<todo guides=round numbering=roman>"));
		let committed = omp_slopjson::parse(fixtures[1].args).expect("todo args decode");
		TodoRenderer.fold_args(&mut todo_state, &committed, true);
		let todo = TodoRenderer
			.view(&todo_state, Some(&todo_outcome))
			.expect("todo success renders");
		assert!(todo.contains("label=Foundation"));
		assert!(todo.contains("status=completed"));
		assert!(todo.contains("label=Auth"));
		assert_eq!(
			TodoRenderer
				.view(&todo_state, Some(&todo_fault))
				.expect("todo fault renders")
				.as_str(),
			"<callout kind=error>Unknown phase 'Auth' - initialize the list first</callout>",
		);

		let think_outcome = serde_json::from_slice::<CallOutcome<ThinkPayload, ThinkFault>>(
			fixtures[2].success_outcome,
		)
		.expect("think outcome decodes");
		let think_fault =
			serde_json::from_slice::<CallOutcome<ThinkPayload, ThinkFault>>(fixtures[2].error_outcome)
				.expect("think fault decodes");
		let mut think_state = ThinkState::default();
		let streaming = omp_slopjson::parse_streaming(fixtures[2].streaming_args);
		ThinkRenderer.fold_args(&mut think_state, &streaming, false);
		let live = ThinkRenderer
			.view(&think_state, None)
			.expect("think live view renders");
		assert!(live.starts_with("<text fg=muted dim italic"));
		assert!(live.contains("doubled lat"));
		let committed = omp_slopjson::parse(fixtures[2].args).expect("think args decode");
		ThinkRenderer.fold_args(&mut think_state, &committed, true);
		let success = ThinkRenderer
			.view(&think_state, Some(&think_outcome))
			.expect("think success renders");
		assert!(success.contains("Cache the parsed config outside the loop"));
		assert_eq!(
			ThinkRenderer
				.view(&think_state, Some(&think_fault))
				.expect("think fault renders")
				.as_str(),
			"<callout kind=error>thoughts must not be empty</callout>",
		);
	}

	#[test]
	fn user_text_is_escaped_in_every_renderer() {
		let thought = render_thought("<retry & inspect>");
		assert_eq!(
			thought.to_tml().as_str(),
			"<text fg=muted dim italic wrap=word>&lt;retry &amp; inspect&gt;</text>",
		);

		let todo = TodoPayload {
			phases:   vec![crate::todo::Phase {
				phase: Str::new("Build \"core\""),
				items: vec![crate::todo::Item {
					text:   Str::new("<compile>"),
					status: TodoStatus::Blocked,
					reason: Some(Str::new("CI & review")),
				}],
			}],
			rendered: Str::new("unused"),
		};
		let rendered = render_todo(&todo).to_tml();
		assert!(rendered.contains("label=\"Build &quot;core&quot;\""));
		assert!(rendered.contains("<todo guides=round numbering=roman>"));
		assert!(rendered.contains("desc=\"CI &amp; review\""));
		assert!(rendered.contains("&lt;compile&gt;"));
		assert!(!rendered.contains("Ⅰ"));
		assert!(!rendered.contains("Ⅱ"));

		let live = render_todo_live(Some("<replace>"));
		assert_eq!(
			live.to_tml().as_str(),
			"<row sep=\" · \"><spinner color=accent label=\"Updating task list\"/><fact \
			 label=Operation>&lt;replace&gt;</fact></row>",
		);
	}

	#[test]
	fn fault_callouts_escape_tool_messages() {
		let fault = AskFault::Invalid { message: Str::new("<invalid & cancelled>") };
		let rendered = AskRenderer
			.view(&AskState::default(), Some(&CallOutcome::Faulted(fault)))
			.expect("ask fault renders");
		assert_eq!(
			rendered.as_str(),
			"<callout kind=error>&lt;invalid &amp; cancelled&gt;</callout>",
		);
	}
}
