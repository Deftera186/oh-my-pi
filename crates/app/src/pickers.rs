//! Standalone alternate-screen pickers shared by CLI startup flows.

use std::{
	fs,
	io::{self, IsTerminal as _, Write as _},
	path::{Path, PathBuf},
};

use omp_chat_ui::{ListPicker, ListRow, PickerEvent};
use omp_core::Str;
use omp_driver::cleanse::{Checker, TargetChoice};
use omp_storage::index::{self, SessionFilter, SessionIndex, SessionInfo};
use omp_tui::{
	Frame, InputEvent, Renderer, Size, Terminal, TerminalEvent, TerminalOptions, TtyOut, UiContext,
};
use thiserror::Error;

/// A session selected before project-scoped authorities are started.
#[derive(Clone, Debug)]
pub(crate) struct SessionSelection {
	pub(crate) session:       SessionInfo,
	pub(crate) sessions_dir:  PathBuf,
	pub(crate) database_path: PathBuf,
}

/// Failure to discover or render a standalone picker.
#[derive(Debug, Error)]
pub(crate) enum PickerError {
	/// Session metadata could not be read from an authoritative index.
	#[error(transparent)]
	Storage(#[from] index::Error),
	/// Terminal entry, input, or rendering failed.
	#[error(transparent)]
	Terminal(#[from] io::Error),
}

/// Chooses all checkers, one checker, or a free-form discovery request.
///
/// Non-interactive invocations deterministically select every discovered
/// checker instead of attempting to enter the alternate screen.
pub(crate) async fn pick_cleanse_target(checkers: &[Checker]) -> Result<TargetChoice, PickerError> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Ok(TargetChoice::All);
	}
	let mut rows = Vec::with_capacity(checkers.len() + 2);
	rows.push(ListRow {
		key:    "all".into(),
		label:  Str::from(format!(
			"Run all {} discovered checker{}",
			checkers.len(),
			if checkers.len() == 1 { "" } else { "s" }
		)),
		detail: Str::new(""),
	});
	rows.extend(checkers.iter().map(|checker| ListRow {
		key:    checker.id.clone(),
		label:  checker.label.clone(),
		detail: Str::from(format!("{} — {}", checker.language, checker.binary.display())),
	}));
	rows.push(ListRow {
		key:    "request".into(),
		label:  Str::new("Describe what to fix…"),
		detail: Str::new("A discovery agent determines the checker command"),
	});
	let Some(index) = run_list("Select what to cleanse", &rows).await? else {
		return Ok(TargetChoice::Cancel);
	};
	if index == 0 {
		Ok(TargetChoice::All)
	} else if index == rows.len() - 1 {
		Ok(prompt_cleanse_request()?.map_or(TargetChoice::Cancel, TargetChoice::Request))
	} else {
		Ok(TargetChoice::Checker(checkers[index - 1].id.clone()))
	}
}

/// Reads a cleanse discovery request only when both standard streams are TTYs.
pub(crate) fn prompt_cleanse_request() -> Result<Option<Str>, PickerError> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Ok(None);
	}
	print!("Describe what to detect and fix (for example, \"ts errors\"): ");
	io::stdout().flush()?;
	let mut request = String::new();
	io::stdin().read_line(&mut request)?;
	let request = request.trim();
	Ok((!request.is_empty()).then(|| Str::from(request)))
}

/// Lists sessions from every native project index and asks the user to choose
/// one.
pub(crate) async fn pick_session(
	data_dir: &Path,
	explicit_session_dir: Option<&Path>,
) -> Result<Option<SessionSelection>, PickerError> {
	let mut sessions = Vec::new();
	if let Some(directory) = explicit_session_dir {
		read_index(directory, &directory.join("sessions.sqlite3"), &mut sessions)?;
	} else {
		let projects = data_dir.join("projects");
		match fs::read_dir(&projects) {
			Ok(entries) => {
				for entry in entries.flatten() {
					let path = entry.path();
					if path.is_dir() {
						read_index(
							&path.join("sessions"),
							&path.join("sessions.sqlite3"),
							&mut sessions,
						)?;
					}
				}
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(error) => return Err(error.into()),
		}
	}
	sessions.sort_unstable_by(|left, right| {
		right
			.session
			.updated_ms
			.cmp(&left.session.updated_ms)
			.then_with(|| left.session.id.0.cmp(&right.session.id.0))
	});
	let rows: Vec<ListRow> = sessions
		.iter()
		.map(|selection| ListRow {
			key:    selection.session.id.0.clone(),
			label:  selection
				.session
				.title
				.clone()
				.unwrap_or_else(|| selection.session.id.0.clone()),
			detail: selection.session.cwd.clone(),
		})
		.collect();
	let picked = run_list("Resume session", &rows).await?;
	let candidate_count = rows.len();
	let selection = picked.and_then(|index| sessions.into_iter().nth(index));
	if let Some(selection) = &selection {
		tracing::debug!(
			session_id = %selection.session.id.0,
			candidate_count,
			"session selected from resume picker"
		);
	} else {
		tracing::debug!(candidate_count, "resume session picker closed without selection");
	}
	Ok(selection)
}

fn read_index(
	sessions_dir: &Path,
	database: &Path,
	output: &mut Vec<SessionSelection>,
) -> Result<(), PickerError> {
	if !database.is_file() {
		return Ok(());
	}
	let index = SessionIndex::open_authoritative_reader(database)?;
	let page = index.list(&SessionFilter { limit: 200, ..SessionFilter::default() })?;
	output.extend(page.sessions.into_iter().map(|session| SessionSelection {
		session,
		sessions_dir: sessions_dir.to_owned(),
		database_path: database.to_owned(),
	}));
	Ok(())
}

/// Runs the shared fuzzy picker on an alternate terminal screen.
pub(crate) async fn run_list(title: &str, rows: &[ListRow]) -> Result<Option<usize>, PickerError> {
	if rows.is_empty() {
		return Ok(None);
	}
	let mut terminal = Terminal::enter(TerminalOptions::default())?;
	let mut renderer = Renderer::new(TtyOut::new()?);
	renderer.apply_caps(&terminal.caps())?;
	terminal.enter_alt()?;
	let mut viewport = terminal.size()?;
	let context = UiContext::default().with_terminal_caps(&terminal.caps());
	let mut picker = ListPicker::open(title, rows, 0, &context);
	loop {
		paint(&mut picker, &mut renderer, viewport)?;
		match terminal.next().await? {
			TerminalEvent::Resize => {
				viewport = terminal.take_resize()?.unwrap_or(terminal.size()?);
			},
			TerminalEvent::Input(InputEvent::Key(key)) => match picker.handle_key(key) {
				PickerEvent::Consumed => {},
				PickerEvent::Close => return Ok(None),
				PickerEvent::Pick(index) => return Ok(Some(index)),
			},
			TerminalEvent::Input(InputEvent::Paste(text)) => match picker.handle_paste(&text) {
				PickerEvent::Consumed => {},
				PickerEvent::Close => return Ok(None),
				PickerEvent::Pick(index) => return Ok(Some(index)),
			},
			TerminalEvent::Input(InputEvent::Mouse(report)) => {
				match picker.handle_mouse(report.col, report.row, report.kind, viewport) {
					PickerEvent::Consumed => {},
					PickerEvent::Close => return Ok(None),
					PickerEvent::Pick(index) => return Ok(Some(index)),
				}
			},
			TerminalEvent::Input(event) => {
				let _ = terminal.handle_input_event(&event, &mut renderer)?;
			},
			TerminalEvent::Debug(_) | TerminalEvent::Effect(_) => {},
			TerminalEvent::Closed => return Ok(None),
		}
	}
}

fn paint(
	picker: &mut ListPicker,
	renderer: &mut Renderer<TtyOut>,
	viewport: Size,
) -> io::Result<()> {
	let base = Frame::new(Size::new(viewport.width, 1));
	renderer.repaint("", base, viewport.height, &[picker.layer(viewport)])?;
	Ok(())
}
