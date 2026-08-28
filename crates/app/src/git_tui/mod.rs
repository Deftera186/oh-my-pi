//! Shared backend for the standalone and chat-hosted Git workbench.

pub mod ai_stage;
pub mod avatar;
pub mod model;

use std::{
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use omp_chat_ui::git::{GitIntent, GitUpdate};
use omp_core::{IntoStr as _, Str};
use omp_driver::commit::{CommitError, CommitGenerator, CommitRequest};
use omp_vcs::{git::GitRepo, types::DiffOptions};
use parking_lot::Mutex as SyncMutex;
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;

use self::{
	ai_stage::AiStageError,
	avatar::AvatarLoader,
	model::{GitModel, GitModelError},
};

/// Result of applying one workbench intent.
#[derive(Debug, Default)]
pub struct GitIntentResult {
	/// Backend updates to deliver to the workbench in order.
	pub updates: Vec<GitUpdate>,
	/// Whether the workbench requested closure.
	pub close:   bool,
}

/// Cloneable controller shared by refresh and interaction tasks.
#[derive(Clone)]
pub struct GitSession {
	model:       Arc<Mutex<GitModel>>,
	avatar:      Option<AvatarLoader>,
	busy:        Arc<AtomicBool>,
	cancel:      CancellationToken,
	load_cancel: Arc<SyncMutex<CancellationToken>>,
	generator:   Arc<OnceCell<CommitGenerator>>,
}

#[derive(Debug, thiserror::Error)]
enum GeneratorError {
	#[error("cannot resolve the OMP data directory")]
	DataDir {
		#[source]
		source: omp_core::dirs::DataDirError,
	},
	#[error(transparent)]
	Commit(#[from] CommitError),
}

#[derive(Debug, thiserror::Error)]
enum GenerateError {
	#[error(transparent)]
	Generator(#[from] GeneratorError),
	#[error(transparent)]
	Model(#[from] GitModelError),
	#[error("cannot read the staged diff")]
	Repository {
		#[source]
		source: omp_vcs::Error,
	},
	#[error("staged diff task failed")]
	Task {
		#[source]
		source: tokio::task::JoinError,
	},
	#[error(transparent)]
	Commit(#[from] CommitError),
}

impl GitSession {
	/// Opens a repository session and resolves an optional pinned revision.
	pub async fn open(
		cwd: &Path,
		revision: Option<&str>,
		cancel: CancellationToken,
	) -> Result<Self, GitModelError> {
		let model = GitModel::open(cwd, revision, &cancel).await?;
		Ok(Self {
			model: Arc::new(Mutex::new(model)),
			avatar: AvatarLoader::new(),
			busy: Arc::new(AtomicBool::new(false)),
			load_cancel: Arc::new(SyncMutex::new(cancel.child_token())),
			generator: Arc::new(OnceCell::new()),
			cancel,
		})
	}

	/// Returns the initial repository snapshot.
	pub async fn initial_snapshot(&self) -> Result<omp_chat_ui::git::GitSnapshot, GitModelError> {
		self.model.lock().await.force_refresh(&self.cancel).await
	}

	/// Polls for a changed repository snapshot.
	pub async fn poll_refresh(
		&self,
	) -> Result<Option<omp_chat_ui::git::GitSnapshot>, GitModelError> {
		self.model.lock().await.refresh(&self.cancel).await
	}

	/// Loads line counts for the current fast snapshot, returning the
	/// stats-enriched follow-up snapshot once.
	pub async fn deferred_stats(
		&self,
	) -> Result<Option<omp_chat_ui::git::GitSnapshot>, GitModelError> {
		self
			.model
			.lock()
			.await
			.load_deferred_stats(&self.cancel)
			.await
	}

	/// Applies one UI intent through the same path used by both workbench hosts.
	pub async fn handle(&self, intent: GitIntent) -> GitIntentResult {
		self.handle_with_progress(intent, |_| {}).await
	}

	/// Applies an intent while forwarding progressive file-content chunks.
	pub async fn handle_with_progress(
		&self,
		intent: GitIntent,
		mut on_update: impl FnMut(GitUpdate) + Send,
	) -> GitIntentResult {
		match intent {
			GitIntent::Close => {
				self.cancel.cancel();
				GitIntentResult { close: true, updates: Vec::new() }
			},
			GitIntent::Refresh => match self.model.lock().await.force_refresh(&self.cancel).await {
				Ok(snapshot) => one(GitUpdate::Snapshot(snapshot)),
				Err(error) => failed(error),
			},
			GitIntent::Load { area, path, orig_path, seq } => {
				let load_cancel = self.cancel.child_token();
				{
					let mut active = self.load_cancel.lock();
					active.cancel();
					*active = load_cancel.clone();
				}
				let result = self
					.model
					.lock()
					.await
					.contents_stream(
						area,
						path.as_str(),
						orig_path.as_deref(),
						&load_cancel,
						|old_lines, new_lines| {
							on_update(GitUpdate::ContentsChunk { seq, old_lines, new_lines });
						},
					)
					.await;
				{
					let active = self.load_cancel.lock();
					if load_cancel.is_cancelled() || active.is_cancelled() {
						return GitIntentResult::default();
					}
				}
				match result {
					Ok(contents) => one(GitUpdate::Contents { seq, contents }),
					Err(error) => failed(error),
				}
			},
			GitIntent::Avatar { email } => {
				let png = if let Some(loader) = &self.avatar {
					let cwd = self.model.lock().await.cwd().to_path_buf();
					loader.load(email.as_str(), &cwd, &self.cancel).await
				} else {
					None
				};
				one(GitUpdate::Avatar { email, png })
			},
			intent => self.handle_action(intent).await,
		}
	}

	async fn handle_action(&self, intent: GitIntent) -> GitIntentResult {
		let Ok(_busy) = BusyGuard::enter(&self.busy) else {
			return GitIntentResult::default();
		};
		match intent {
			GitIntent::GenerateCommit { amend } => return self.generate_commit(amend).await,
			GitIntent::AiStage { instruction } => {
				return self.ai_stage(instruction.as_str()).await;
			},
			_ => {},
		}
		let mut model = self.model.lock().await;
		let action = match intent {
			GitIntent::StageFiles(paths) => model.stage(paths.as_deref(), &self.cancel).await,
			GitIntent::UnstageFiles(paths) => model.unstage(paths.as_deref(), &self.cancel).await,
			GitIntent::ApplyLines { op, path, old, new } => {
				model
					.apply_lines(op, path.as_str(), old, new, &self.cancel)
					.await
			},
			GitIntent::Commit { message, amend, stage_all } => {
				model
					.commit(message.as_str(), amend, stage_all, &self.cancel)
					.await
			},
			GitIntent::Refresh
			| GitIntent::Load { .. }
			| GitIntent::Avatar { .. }
			| GitIntent::GenerateCommit { .. }
			| GitIntent::AiStage { .. }
			| GitIntent::Close => {
				unreachable!("non-action intent routed to action handler")
			},
		};
		let mut updates = Vec::with_capacity(2);
		match action {
			Ok(message) => updates.push(GitUpdate::ActionDone { message }),
			Err(error) => updates.push(GitUpdate::ActionFailed { message: render_error(&error) }),
		}
		match model.force_refresh(&self.cancel).await {
			Ok(snapshot) => updates.push(GitUpdate::Snapshot(snapshot)),
			Err(error) => updates.push(GitUpdate::ActionFailed { message: render_error(&error) }),
		}
		GitIntentResult { updates, close: false }
	}

	async fn commit_generator(&self, project: &Path) -> Result<CommitGenerator, GeneratorError> {
		let data_dir =
			omp_core::dirs::data_dir(None).map_err(|source| GeneratorError::DataDir { source })?;
		let generator = self
			.generator
			.get_or_try_init(|| CommitGenerator::production(&data_dir, project, None))
			.await?;
		Ok(generator.clone())
	}

	async fn generate_commit(&self, amend: bool) -> GitIntentResult {
		match self.generate_commit_inner(amend).await {
			Ok((commit, snapshot)) => GitIntentResult {
				updates: vec![
					GitUpdate::CommitGenerated { summary: commit.summary, body: commit.body },
					GitUpdate::Snapshot(snapshot),
				],
				close:   false,
			},
			Err(error) => {
				let mut updates = vec![GitUpdate::ActionFailed { message: error.to_string().to_str() }];
				if let Ok(snapshot) = self.model.lock().await.force_refresh(&self.cancel).await {
					updates.push(GitUpdate::Snapshot(snapshot));
				}
				GitIntentResult { updates, close: false }
			},
		}
	}

	async fn generate_commit_inner(
		&self,
		amend: bool,
	) -> Result<(omp_driver::commit::ConventionalCommit, omp_chat_ui::git::GitSnapshot), GenerateError>
	{
		let (cwd, snapshot) = {
			let mut model = self.model.lock().await;
			let mut snapshot = model.force_refresh(&self.cancel).await?;
			if snapshot.staged.is_empty() && !snapshot.unstaged.is_empty() {
				let _ = model.stage(None, &self.cancel).await?;
				snapshot = model.force_refresh(&self.cancel).await?;
			}
			(model.cwd().to_path_buf(), snapshot)
		};
		let diff_cwd = cwd.clone();
		let has_head = snapshot.head.is_some();
		let (staged_diff, recent_subjects) = tokio::task::spawn_blocking(move || {
			let repo = GitRepo::require(&diff_cwd)?;
			let diff = repo.diff_text(&DiffOptions {
				cached: true,
				binary: true,
				..DiffOptions::default()
			})?;
			let subjects = if has_head {
				repo.log_subjects(12)?
			} else {
				Vec::new()
			};
			Ok::<_, omp_vcs::Error>((diff, subjects))
		})
		.await
		.map_err(|source| GenerateError::Task { source })?
		.map_err(|source| GenerateError::Repository { source })?;
		let recent_subjects = recent_subjects
			.into_iter()
			.map(Str::from)
			.collect::<Vec<_>>();
		let amend_base = if amend { snapshot.head.as_ref() } else { None }.map(|head| {
			if head.body.is_empty() {
				head.subject.clone()
			} else {
				omp_core::sf!("{}\n\n{}", head.subject, head.body)
			}
		});
		let generator = self.commit_generator(&cwd).await?;
		let commit = generator
			.generate(CommitRequest {
				staged_diff:     staged_diff.as_str(),
				recent_subjects: &recent_subjects,
				amend_base:      amend_base.as_deref(),
			})
			.await?;
		Ok((commit, snapshot))
	}

	async fn ai_stage(&self, instruction: &str) -> GitIntentResult {
		let (cwd, snapshot) = {
			let mut model = self.model.lock().await;
			let snapshot = match model.force_refresh(&self.cancel).await {
				Ok(snapshot) => snapshot,
				Err(error) => return failed(error),
			};
			(model.cwd().to_path_buf(), snapshot)
		};
		let generator = match self.commit_generator(&cwd).await {
			Ok(generator) => generator,
			Err(error) => {
				return one(GitUpdate::ActionFailed { message: error.to_string().to_str() });
			},
		};
		let outcome = match ai_stage::stage(&cwd, instruction, &snapshot.unstaged, &generator).await {
			Ok(outcome) => outcome,
			Err(error) => {
				return one(GitUpdate::ActionFailed { message: render_ai_stage_error(&error) });
			},
		};
		let refreshed = match self.model.lock().await.force_refresh(&self.cancel).await {
			Ok(snapshot) => snapshot,
			Err(error) => return failed(error),
		};
		GitIntentResult {
			updates: vec![
				GitUpdate::ActionDone { message: outcome.message() },
				GitUpdate::Snapshot(refreshed),
			],
			close:   false,
		}
	}
}

struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
	fn enter(busy: &'a AtomicBool) -> Result<Self, ()> {
		busy
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.map(|_| Self(busy))
			.map_err(|_| ())
	}
}

impl Drop for BusyGuard<'_> {
	fn drop(&mut self) {
		self.0.store(false, Ordering::Release);
	}
}

fn one(update: GitUpdate) -> GitIntentResult {
	GitIntentResult { updates: vec![update], close: false }
}

fn failed(error: GitModelError) -> GitIntentResult {
	one(GitUpdate::ActionFailed { message: render_error(&error) })
}

fn render_error(error: &GitModelError) -> Str {
	error.to_string().to_str()
}
fn render_ai_stage_error(error: &AiStageError) -> Str {
	error.to_string().to_str()
}
#[cfg(test)]
mod tests {
	use std::{fs, process::Command};

	use omp_chat_ui::git::{GitArea, GitIntent, GitUpdate};

	use super::*;

	fn fixture_git(cwd: &Path, arguments: &[&str]) {
		let output = Command::new("git")
			.current_dir(cwd)
			.args(arguments)
			.output()
			.expect("fixture git should launch");
		assert!(output.status.success(), "fixture git {arguments:?} failed");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn newer_load_cancels_stale_sequence_before_terminal_delivery() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("large.txt"), "seed\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "large.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);
		let changed = (0..70_000)
			.map(|line| format!("{line:05} {}\n", "stale-stream".repeat(3)))
			.collect::<String>();
		assert!(changed.len() < model::MAX_FILE_BYTES as usize);
		fs::write(fixture.path().join("large.txt"), changed).expect("changed file");

		let session = GitSession::open(fixture.path(), None, CancellationToken::new())
			.await
			.unwrap();
		let first_session = session.clone();
		let (progress_tx, progress_rx) = flume::bounded(1);
		let (release_tx, release_rx) = flume::bounded(1);
		let first = tokio::spawn(async move {
			let mut paused = false;
			first_session
				.handle_with_progress(
					GitIntent::Load {
						area:      GitArea::Unstaged,
						path:      Str::new_static("large.txt"),
						orig_path: None,
						seq:       1,
					},
					|_| {
						if !paused {
							paused = true;
							progress_tx
								.send(())
								.expect("test should observe first chunk");
							release_rx.recv().expect("test should release first load");
						}
					},
				)
				.await
		});
		progress_rx.recv_async().await.expect("first load streamed");
		let first_cancel = session.load_cancel.lock().clone();
		let second_session = session.clone();
		let second = tokio::spawn(async move {
			second_session
				.handle(GitIntent::Load {
					area:      GitArea::Unstaged,
					path:      Str::new_static("large.txt"),
					orig_path: None,
					seq:       2,
				})
				.await
		});
		first_cancel.cancelled().await;
		release_tx
			.send(())
			.expect("first load should still be paused");
		assert!(first.await.unwrap().updates.is_empty());
		assert!(matches!(second.await.unwrap().updates.as_slice(), [GitUpdate::Contents {
			seq: 2,
			..
		}]));
	}
}
