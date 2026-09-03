//! External editor resolution and safe temporary-draft round trips.

use std::{
	env,
	fs::{self, File, OpenOptions},
	io,
	io::{Read as _, Write as _},
	path::{Path, PathBuf},
	process::{Command, ExitStatus, Stdio},
};

use omp_tui::components::editor::{
	ExternalEditorCommand, ExternalEditorCommandError, ExternalEditorSuspension,
	ExternalEditorTerminal, parse_external_editor_command,
};
use thiserror::Error;

/// External editor launch options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorOptions<'a> {
	/// Temporary-file extension, including or excluding the leading dot.
	pub extension:             &'a str,
	/// Remove one terminal newline from the successful edited draft.
	pub trim_trailing_newline: bool,
}

impl Default for EditorOptions<'_> {
	fn default() -> Self {
		Self { extension: "md", trim_trailing_newline: true }
	}
}

/// Failure to resolve or run an external editor.
#[derive(Debug, Error)]
pub enum EditorError {
	/// Configured command could not be split into safe argv.
	#[error(transparent)]
	Command(#[from] ExternalEditorCommandError),
	/// Temporary extension contains a path separator or unsupported character.
	#[error("external editor temporary extension is invalid")]
	InvalidExtension,
	/// Temporary draft creation, child launch, or edited read failed.
	#[error("external editor {operation} failed for {path}")]
	Io {
		/// Operation being performed.
		operation: &'static str,
		/// Affected path or executable.
		path:      PathBuf,
		/// Underlying operating-system failure.
		#[source]
		source:    io::Error,
	},
}

/// Resolves `VISUAL`, then `EDITOR`, then the platform's baseline editor.
///
/// Environment values are trimmed but otherwise parsed later as shell words;
/// no shell is invoked.
pub fn resolve_editor_command() -> String {
	resolve_editor_command_from(
		env::var("VISUAL").ok().as_deref(),
		env::var("EDITOR").ok().as_deref(),
	)
}

/// Deterministic resolution helper used by settings and tests.
pub fn resolve_editor_command_from(visual: Option<&str>, editor: Option<&str>) -> String {
	visual
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.or_else(|| editor.map(str::trim).filter(|value| !value.is_empty()))
		.unwrap_or(platform_editor())
		.to_owned()
}

/// Opens `content` in the selected editor and returns a replacement only after
/// a successful child exit. Terminal restoration and temporary cleanup are
/// guaranteed on every path.
pub fn edit_draft<T: ExternalEditorTerminal + ?Sized>(
	terminal: &mut T,
	content: &str,
	options: EditorOptions<'_>,
) -> Result<Option<String>, EditorError> {
	let configured = resolve_editor_command();
	let command = parse_external_editor_command(&configured)?;
	edit_draft_with_command(terminal, &command, content, options)
}

/// Runs one already parsed command. This is useful when a settings owner has
/// frozen environment-derived editor configuration for the session.
pub fn edit_draft_with_command<T: ExternalEditorTerminal + ?Sized>(
	terminal: &mut T,
	command: &ExternalEditorCommand,
	content: &str,
	options: EditorOptions<'_>,
) -> Result<Option<String>, EditorError> {
	let mut draft = prepared_draft(content, options.extension)?;
	let suspension = ExternalEditorSuspension::new(terminal).map_err(|source| EditorError::Io {
		operation: "terminal suspend",
		path: PathBuf::from("<terminal>"),
		source,
	})?;
	let status = launch_editor(command, draft.path())?;
	suspension.restore().map_err(|source| EditorError::Io {
		operation: "terminal restore",
		path: PathBuf::from("<terminal>"),
		source,
	})?;
	finish_draft(&mut draft, status, options.trim_trailing_newline)
}

/// Opens a draft after the terminal host has already restored terminal modes.
///
/// GUI hosts and terminal lifecycle owners use this at a reconstruction
/// boundary, so no second suspension or raw-mode transition occurs.
pub fn edit_draft_detached(
	content: &str,
	options: EditorOptions<'_>,
) -> Result<Option<String>, EditorError> {
	let configured = resolve_editor_command();
	let command = parse_external_editor_command(&configured)?;
	let mut draft = prepared_draft(content, options.extension)?;
	let status = launch_editor(&command, draft.path())?;
	finish_draft(&mut draft, status, options.trim_trailing_newline)
}

fn prepared_draft(content: &str, extension: &str) -> Result<DraftFile, EditorError> {
	let mut draft = DraftFile::create(extension)?;
	draft.write_all(content.as_bytes())?;
	Ok(draft)
}

fn launch_editor(command: &ExternalEditorCommand, path: &Path) -> Result<ExitStatus, EditorError> {
	let mut child = Command::new(command.program.as_str());
	child
		.args(command.arguments.iter().map(|argument| argument.as_str()))
		.arg(path)
		.stdin(Stdio::inherit())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit());
	child.status().map_err(|source| EditorError::Io {
		operation: "launch",
		path: PathBuf::from(command.program.as_str()),
		source,
	})
}

fn finish_draft(
	draft: &mut DraftFile,
	status: ExitStatus,
	trim_trailing_newline: bool,
) -> Result<Option<String>, EditorError> {
	if !status.success() {
		return Ok(None);
	}
	let mut edited = draft.read_to_string()?;
	if trim_trailing_newline && edited.ends_with('\n') {
		edited.pop();
	}
	Ok(Some(edited))
}

const fn platform_editor() -> &'static str {
	if cfg!(windows) { "notepad" } else { "vi" }
}

#[must_use]
struct DraftFile {
	path: PathBuf,
	file: File,
}

impl DraftFile {
	fn create(extension: &str) -> Result<Self, EditorError> {
		let extension = extension.trim().trim_start_matches('.');
		if extension.is_empty()
			|| !extension
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
		{
			return Err(EditorError::InvalidExtension);
		}
		let directory = env::temp_dir();
		for _ in 0..16 {
			let path =
				directory.join(format!("omp-editor-{}.{}", omp_core::Ulid::generate(), extension));
			let mut options = OpenOptions::new();
			options.write(true).read(true).create_new(true);
			#[cfg(unix)]
			{
				use std::os::unix::fs::OpenOptionsExt as _;
				options.mode(0o600);
			}
			match options.open(&path) {
				Ok(file) => return Ok(Self { path, file }),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
				Err(source) => return Err(io_error("temporary creation", path, source)),
			}
		}
		Err(io_error(
			"temporary creation",
			directory,
			io::Error::new(io::ErrorKind::AlreadyExists, "temporary name collision"),
		))
	}

	fn path(&self) -> &Path {
		&self.path
	}

	fn write_all(&mut self, bytes: &[u8]) -> Result<(), EditorError> {
		self
			.file
			.write_all(bytes)
			.map_err(|source| io_error("draft write", self.path.clone(), source))?;
		self
			.file
			.sync_all()
			.map_err(|source| io_error("draft sync", self.path.clone(), source))
	}

	fn read_to_string(&mut self) -> Result<String, EditorError> {
		self.file = File::open(&self.path)
			.map_err(|source| io_error("draft reopen", self.path.clone(), source))?;
		let mut output = String::new();
		self
			.file
			.read_to_string(&mut output)
			.map_err(|source| io_error("draft read", self.path.clone(), source))?;
		Ok(output)
	}
}

impl Drop for DraftFile {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.path);
	}
}

fn io_error(operation: &'static str, path: PathBuf, source: io::Error) -> EditorError {
	EditorError::Io { operation, path, source }
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use omp_core::Str;

	use super::*;

	struct TerminalProbe {
		suspended: AtomicUsize,
		restored:  AtomicUsize,
	}

	impl ExternalEditorTerminal for TerminalProbe {
		fn suspend_for_external_editor(&mut self) -> io::Result<()> {
			self.suspended.fetch_add(1, Ordering::Relaxed);
			Ok(())
		}

		fn restore_after_external_editor(&mut self) -> io::Result<()> {
			self.restored.fetch_add(1, Ordering::Relaxed);
			Ok(())
		}
	}

	#[test]
	fn resolution_prefers_visual_then_editor_then_platform() {
		assert_eq!(resolve_editor_command_from(Some(" code --wait "), Some("vim")), "code --wait");
		assert_eq!(resolve_editor_command_from(Some(" "), Some("vim")), "vim");
		assert_eq!(resolve_editor_command_from(None, None), platform_editor());
	}

	#[cfg(unix)]
	#[test]
	fn successful_round_trip_restores_terminal_and_replaces_draft() {
		use std::os::unix::fs::PermissionsExt as _;
		let directory = tempfile::tempdir().unwrap();
		let executable = directory.path().join("editor");
		fs::write(&executable, "#!/bin/sh\nprintf 'edited\\n' > \"$1\"\n").unwrap();
		fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
		let command = ExternalEditorCommand {
			program:   Str::from(executable.to_string_lossy().into_owned()),
			arguments: Box::new([]),
		};
		let mut terminal =
			TerminalProbe { suspended: AtomicUsize::new(0), restored: AtomicUsize::new(0) };
		let result =
			edit_draft_with_command(&mut terminal, &command, "initial", EditorOptions::default())
				.unwrap();
		assert_eq!(result.as_deref(), Some("edited"));
		assert_eq!(terminal.suspended.load(Ordering::Relaxed), 1);
		assert_eq!(terminal.restored.load(Ordering::Relaxed), 1);
	}
}
