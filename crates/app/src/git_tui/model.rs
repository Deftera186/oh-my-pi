//! Environment-backed repository model for the interactive Git workbench.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use bytes::{Bytes, BytesMut};
use omp_chat_ui::git::{
	GitArea, GitChangeKind, GitCommitInfo, GitFileContents, GitFileRow, GitPatchOp, GitSnapshot,
};
use omp_core::{Hash32, IntoStr as _, Str};
use omp_envd::vcs::git::{
	commands::{CommandError, GitCommands},
	diff::{self, ChangeKind, DiffOptions, GitDiff, LineCount, NumstatEntry, StatusEntry},
	mutation::{
		CommitOptions, DiffLineSelection, GitMutation, GitMutationConsumer, LineRange, MutationError,
	},
	query::GitQuery,
	refs::{self, RefError},
	repo::{self, Repository, RepositoryError},
};
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;
use xutf::TextBuf as _;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
pub(super) const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 512;
const STREAM_BUFFER_BYTES: usize = 256 * 1024;
const STREAM_READ_BYTES: usize = 64 * 1024;
const LFS_POINTER_MAX_BYTES: usize = 1024;
const LFS_VERSION: &str = "version https://git-lfs.github.com/spec/v1";

/// Failures produced while reading or mutating an interactive Git repository.
#[derive(Debug, thiserror::Error)]
pub enum GitModelError {
	/// No repository contains the selected working directory.
	#[error("Not a git repository")]
	NotRepository,
	/// Repository discovery failed.
	#[error(transparent)]
	Repository(#[from] RepositoryError),
	/// A Git read command failed.
	#[error(transparent)]
	Command(#[from] CommandError),
	/// Direct HEAD resolution failed.
	#[error(transparent)]
	Reference(#[from] RefError),
	/// A Git mutation failed.
	#[error(transparent)]
	Mutation(#[from] MutationError),
	/// The requested revision does not resolve to a commit.
	#[error("Cannot resolve revision: {revision}")]
	RevisionMissing {
		/// User-supplied revision.
		revision: Str,
	},
	/// A worktree file could not be inspected or read.
	#[error("failed to read worktree file {path:?}")]
	WorktreeIo {
		/// File that could not be read.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// A local Git LFS object could not be inspected or read.
	#[error("failed to read Git LFS object {path:?}")]
	LfsIo {
		/// Object that could not be read.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: std::io::Error,
	},
}

/// Environment-backed state for one Git workbench.
pub struct GitModel {
	cwd:               PathBuf,
	repository:        Repository,
	diff:              GitDiff,
	query:             GitQuery,
	commands:          GitCommands,
	mutation:          GitMutation,
	pinned_sha:        Option<Str>,
	fingerprint:       Option<[u8; 32]>,
	stats_fingerprint: Option<[u8; 32]>,
	snapshot:          Option<GitSnapshot>,
	head:              Option<GitCommitInfo>,
}

impl GitModel {
	/// Discovers a repository and optionally resolves one pinned revision.
	pub async fn open(
		cwd: &Path,
		revision: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<Self, GitModelError> {
		let repository = repo::discover(cwd)
			.await?
			.ok_or(GitModelError::NotRepository)?;
		let cwd = repository.worktree_root.clone();
		let commands = GitCommands::new();
		let pinned_sha = match revision {
			Some(revision) => {
				let commit_revision = format!("{revision}^{{commit}}");
				Some(
					commands
						.resolve_ref(&cwd, &commit_revision, cancel)
						.await?
						.ok_or_else(|| GitModelError::RevisionMissing { revision: revision.to_str() })?,
				)
			},
			None => None,
		};
		Ok(Self {
			cwd,
			repository: repository.clone(),
			diff: GitDiff::new(),
			query: GitQuery::new(),
			commands,
			mutation: GitMutation::new(repository, GitMutationConsumer::InteractiveGit),
			pinned_sha,
			fingerprint: None,
			stats_fingerprint: None,
			snapshot: None,
			head: None,
		})
	}

	/// Returns the canonical worktree root.
	pub fn cwd(&self) -> &Path {
		&self.cwd
	}

	/// Re-reads repository state, returning `None` when its fingerprint is
	/// unchanged.
	pub async fn refresh(
		&mut self,
		cancel: &CancellationToken,
	) -> Result<Option<GitSnapshot>, GitModelError> {
		if let Some(sha) = self.pinned_sha.clone() {
			let fingerprint = fingerprint(sha.as_bytes(), &[]);
			if self.fingerprint == Some(fingerprint) {
				return Ok(None);
			}
			let head = self.load_commit(sha.as_str(), cancel).await?;
			self.fingerprint = Some(fingerprint);
			self.head = Some(head.clone());
			let snapshot = GitSnapshot {
				branch:   None,
				unstaged: Vec::new(),
				staged:   Vec::new(),
				head:     Some(head),
				pinned:   true,
			};
			self.snapshot = Some(snapshot.clone());
			return Ok(Some(snapshot));
		}

		let status_output = self.diff.status_raw(&self.cwd, cancel).await?;
		let entries = diff::parse_status_entries(&status_output);
		let branch = self.commands.current_branch(&self.cwd, cancel).await?;
		let head_state = refs::resolve_head(&self.repository, cancel).await?;
		let head_sha = head_state.commit().map(str::to_owned);
		let fingerprint =
			fingerprint(head_sha.as_deref().unwrap_or_default().as_bytes(), &status_output);
		if self.fingerprint == Some(fingerprint) {
			return Ok(None);
		}

		let (unstaged, staged) = rows_from_status(&entries, &[], &[]);
		let clean = unstaged.is_empty() && staged.is_empty();
		let previous_clean = self
			.snapshot
			.as_ref()
			.is_some_and(|snapshot| snapshot.unstaged.is_empty() && snapshot.staged.is_empty());
		if self.head.as_ref().map(|head| head.sha.as_str()) != head_sha.as_deref()
			|| (clean && !previous_clean)
		{
			self.head = match head_sha.as_deref() {
				Some(sha) if clean => Some(self.load_commit(sha, cancel).await?),
				Some(sha) => Some(self.load_commit_metadata(sha, cancel).await?),
				None => None,
			};
		}
		self.fingerprint = Some(fingerprint);
		self.stats_fingerprint = None;
		let snapshot =
			GitSnapshot { branch, unstaged, staged, head: self.head.clone(), pinned: false };
		self.snapshot = Some(snapshot.clone());
		Ok(Some(snapshot))
	}

	/// Populates changed-line counts after the fast status snapshot has been
	/// delivered, returning a second snapshot when counts were loaded.
	pub async fn load_deferred_stats(
		&mut self,
		cancel: &CancellationToken,
	) -> Result<Option<GitSnapshot>, GitModelError> {
		let Some(fingerprint) = self.fingerprint else {
			return Ok(None);
		};
		let Some(snapshot) = self.snapshot.as_ref() else {
			return Ok(None);
		};
		if snapshot.pinned
			|| (snapshot.unstaged.is_empty() && snapshot.staged.is_empty())
			|| self.stats_fingerprint == Some(fingerprint)
		{
			return Ok(None);
		}
		let (worktree_stats, staged_stats) =
			tokio::try_join!(self.numstat(false, cancel), self.numstat(true, cancel),)?;
		if self.fingerprint != Some(fingerprint) {
			return Ok(None);
		}
		let mut snapshot = self
			.snapshot
			.clone()
			.expect("snapshot remains present while its fingerprint is current");
		apply_counts(&mut snapshot.unstaged, &worktree_stats);
		apply_counts(&mut snapshot.staged, &staged_stats);
		self.stats_fingerprint = Some(fingerprint);
		self.snapshot = Some(snapshot.clone());
		Ok(Some(snapshot))
	}

	/// Invalidates fingerprint deduplication and returns a fresh snapshot.
	pub async fn force_refresh(
		&mut self,
		cancel: &CancellationToken,
	) -> Result<GitSnapshot, GitModelError> {
		self.fingerprint = None;
		Ok(self
			.refresh(cancel)
			.await?
			.expect("cleared fingerprint must produce a snapshot"))
	}

	/// Resolves both sides of one selected file.
	pub async fn contents(
		&self,
		area: GitArea,
		path: &str,
		orig_path: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<GitFileContents, GitModelError> {
		self
			.contents_stream(area, path, orig_path, cancel, |_, _| {})
			.await
	}

	/// Resolves both sides while delivering batches of newly complete text
	/// lines after the bounded fast-path buffer is exceeded.
	pub async fn contents_stream(
		&self,
		area: GitArea,
		path: &str,
		orig_path: Option<&str>,
		cancel: &CancellationToken,
		mut on_chunk: impl FnMut(Vec<Str>, Vec<Str>) + Send,
	) -> Result<GitFileContents, GitModelError> {
		let (events_tx, events_rx) = flume::unbounded();
		let old_spec;
		let new_spec;
		let old_source = match area {
			GitArea::Unstaged => {
				old_spec = format!(":0:{path}");
				StreamSource::Git(old_spec.as_str())
			},
			GitArea::Staged => {
				old_spec = format!("HEAD:{}", orig_path.unwrap_or(path));
				StreamSource::Git(old_spec.as_str())
			},
			GitArea::Commit => {
				old_spec = self
					.head
					.as_ref()
					.and_then(|head| head.parents.first())
					.map(|parent| format!("{}:{}", parent, orig_path.unwrap_or(path)))
					.unwrap_or_default();
				if old_spec.is_empty() {
					StreamSource::Empty
				} else {
					StreamSource::Git(old_spec.as_str())
				}
			},
		};
		let new_source = match area {
			GitArea::Unstaged => StreamSource::Worktree(path),
			GitArea::Staged => {
				new_spec = format!(":0:{path}");
				StreamSource::Git(new_spec.as_str())
			},
			GitArea::Commit => {
				new_spec = self
					.head
					.as_ref()
					.map(|head| format!("{}:{path}", head.sha))
					.unwrap_or_default();
				if new_spec.is_empty() {
					StreamSource::Empty
				} else {
					StreamSource::Git(new_spec.as_str())
				}
			},
		};
		let old = self.stream_side(DiffSide::Old, old_source, cancel, events_tx.clone());
		let new = self.stream_side(DiffSide::New, new_source, cancel, events_tx);
		let collect = collect_stream_events(path, events_rx, &mut on_chunk);
		let (old, new, ()) = tokio::try_join!(old, new, collect)?;
		self.finish_contents(path, old, new).await
	}

	async fn finish_contents(
		&self,
		path: &str,
		old: StreamedSide,
		new: StreamedSide,
	) -> Result<GitFileContents, GitModelError> {
		let too_large = old.too_large || new.too_large;
		let old = self.resolve_lfs(old.bytes).await?;
		let new = self.resolve_lfs(new.bytes).await?;
		let media = media_format(path, &old, &new);
		let (old, new) = if media.as_deref() == Some("svg") {
			tokio::join!(rasterize_svg_side(path, old), rasterize_svg_side(path, new))
		} else {
			(old, new)
		};
		let binary = is_binary(&old.bytes) || is_binary(&new.bytes);
		let binary_asset = media.as_deref() == Some("binary");
		let old_placeholder = old
			.unavailable
			.clone()
			.or_else(|| (binary_asset && !old.bytes.is_empty()).then(binary_placeholder));
		let new_placeholder = new
			.unavailable
			.clone()
			.or_else(|| (binary_asset && !new.bytes.is_empty()).then(binary_placeholder));
		let (old_text, new_text, old_bytes, new_bytes) = if media.is_some() {
			(
				Str::new_static(""),
				Str::new_static(""),
				(!binary_asset && !old.bytes.is_empty() && old.unavailable.is_none())
					.then_some(old.bytes),
				(!binary_asset && !new.bytes.is_empty() && new.unavailable.is_none())
					.then_some(new.bytes),
			)
		} else if binary {
			(Str::new_static(""), Str::new_static(""), None, None)
		} else {
			(decode_utf8(&old.bytes).to_str(), decode_utf8(&new.bytes).to_str(), None, None)
		};
		Ok(GitFileContents {
			old_text,
			new_text,
			binary,
			too_large,
			old_bytes,
			new_bytes,
			media,
			old_placeholder,
			new_placeholder,
		})
	}

	async fn stream_side(
		&self,
		side: DiffSide,
		source: StreamSource<'_>,
		cancel: &CancellationToken,
		events: flume::Sender<StreamEvent>,
	) -> Result<StreamedSide, GitModelError> {
		let result = match source {
			StreamSource::Empty => Ok(StreamedSide::default()),
			StreamSource::Git(spec) => self.stream_git_side(side, spec, cancel, &events).await,
			StreamSource::Worktree(path) => {
				self.stream_worktree_side(side, path, cancel, &events).await
			},
		};
		let _ = events.send(StreamEvent::Finished(side));
		result
	}

	async fn stream_git_side(
		&self,
		side: DiffSide,
		spec: &str,
		cancel: &CancellationToken,
		events: &flume::Sender<StreamEvent>,
	) -> Result<StreamedSide, GitModelError> {
		let mut streamed = 0_usize;
		let mut emit = |bytes: Bytes| {
			let remaining = (MAX_FILE_BYTES as usize + 1).saturating_sub(streamed);
			let take = remaining.min(bytes.len());
			if take > 0 {
				streamed += take;
				let _ = events.send(StreamEvent::Chunk(side, bytes.slice(..take)));
			}
		};
		match self
			.query
			.show_path_stream(&self.cwd, spec, cancel, &mut emit)
			.await
		{
			Ok(bytes) if bytes.len() > MAX_FILE_BYTES as usize => {
				Ok(StreamedSide { bytes: Bytes::new(), too_large: true })
			},
			Ok(bytes) => Ok(StreamedSide { bytes, too_large: false }),
			Err(CommandError::Vcs(omp_vcs::Error::ObjectNotFound { .. })) => {
				Ok(StreamedSide::default())
			},
			Err(error) => Err(error.into()),
		}
	}

	async fn stream_worktree_side(
		&self,
		side: DiffSide,
		path: &str,
		cancel: &CancellationToken,
		events: &flume::Sender<StreamEvent>,
	) -> Result<StreamedSide, GitModelError> {
		let full_path = self.cwd.join(path);
		let mut file = match tokio::fs::File::open(&full_path).await {
			Ok(file) => file,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Ok(StreamedSide::default());
			},
			Err(source) => return Err(GitModelError::WorktreeIo { path: full_path, source }),
		};
		let length = file
			.metadata()
			.await
			.map_err(|source| GitModelError::WorktreeIo { path: full_path.clone(), source })?
			.len();
		if length > MAX_FILE_BYTES {
			return Ok(StreamedSide { bytes: Bytes::new(), too_large: true });
		}
		let mut chunks = Vec::new();
		let mut buffer = vec![0_u8; STREAM_READ_BYTES];
		let mut total = 0;
		loop {
			if cancel.is_cancelled() {
				return Err(CommandError::Vcs(omp_vcs::Error::Canceled).into());
			}
			let read = file
				.read(&mut buffer)
				.await
				.map_err(|source| GitModelError::WorktreeIo { path: full_path.clone(), source })?;
			if read == 0 {
				break;
			}
			total += read;
			if total > MAX_FILE_BYTES as usize {
				return Ok(StreamedSide { bytes: Bytes::new(), too_large: true });
			}
			let chunk = Bytes::copy_from_slice(&buffer[..read]);
			let _ = events.send(StreamEvent::Chunk(side, chunk.clone()));
			chunks.push(chunk);
		}
		let mut bytes = BytesMut::with_capacity(total);
		for chunk in chunks {
			bytes.extend_from_slice(&chunk);
		}
		Ok(StreamedSide { bytes: bytes.freeze(), too_large: false })
	}

	/// Stages exact files, or every change when no path list is supplied.
	pub async fn stage(
		&self,
		paths: Option<&[Str]>,
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		match paths {
			Some(paths) => {
				let path_refs = paths.iter().map(Str::as_str).collect::<Vec<_>>();
				self.mutation.stage_files(&path_refs, cancel).await?;
				Ok(if let [path] = paths {
					omp_core::sf!("Staged {path}")
				} else {
					omp_core::sf!("Staged {} files", paths.len())
				})
			},
			None => {
				self.mutation.stage_all(cancel).await?;
				Ok(Str::new_static("Staged all changes"))
			},
		}
	}

	/// Unstages exact files, or the complete index when no path list is
	/// supplied.
	pub async fn unstage(
		&self,
		paths: Option<&[Str]>,
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		match paths {
			Some(paths) => {
				let path_refs = paths.iter().map(Str::as_str).collect::<Vec<_>>();
				self
					.mutation
					.reset_index_entries(&path_refs, cancel)
					.await?;
				Ok(if let [path] = paths {
					omp_core::sf!("Unstaged {path}")
				} else {
					omp_core::sf!("Unstaged {} files", paths.len())
				})
			},
			None => {
				self.mutation.unstage_all(cancel).await?;
				Ok(Str::new_static("Unstaged all changes"))
			},
		}
	}

	/// Applies one inclusive diff-line selection.
	pub async fn apply_lines(
		&self,
		op: GitPatchOp,
		path: &str,
		old: (u32, u32),
		new: (u32, u32),
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		let selection = DiffLineSelection { old: line_range(old), new: line_range(new) };
		match op {
			GitPatchOp::Stage => {
				self.mutation.stage_lines(path, selection, cancel).await?;
				Ok(Str::new_static("Staged selection"))
			},
			GitPatchOp::Unstage => {
				self.mutation.unstage_lines(path, selection, cancel).await?;
				Ok(Str::new_static("Unstaged selection"))
			},
			GitPatchOp::Discard => {
				self.mutation.discard_lines(path, selection, cancel).await?;
				Ok(Str::new_static("Discarded selection"))
			},
		}
	}

	/// Creates or amends one commit, optionally staging every change first.
	pub async fn commit(
		&self,
		message: &str,
		amend: bool,
		stage_all: bool,
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		if stage_all {
			self.mutation.stage_all(cancel).await?;
		}
		self
			.mutation
			.create_commit(message.as_bytes(), CommitOptions { amend, ..Default::default() }, cancel)
			.await?;
		Ok(Str::new_static(if amend {
			"Amended commit"
		} else {
			"Created commit"
		}))
	}

	async fn numstat(
		&self,
		cached: bool,
		cancel: &CancellationToken,
	) -> Result<Vec<NumstatEntry>, GitModelError> {
		let raw = self
			.diff
			.raw(&self.cwd, DiffOptions { cached, numstat: true, ..Default::default() }, &[], cancel)
			.await?;
		Ok(diff::parse_numstat(raw)?)
	}

	async fn load_commit(
		&self,
		sha: &str,
		cancel: &CancellationToken,
	) -> Result<GitCommitInfo, GitModelError> {
		let mut commit = self.load_commit_metadata(sha, cancel).await?;
		let base = commit.parents.first().map_or(EMPTY_TREE, Str::as_str);
		let output = self
			.diff
			.numstat_tree(&self.cwd, base, commit.sha.as_str(), cancel)
			.await?;
		commit.files = output.into_iter().map(commit_row).collect();
		Ok(commit)
	}

	async fn load_commit_metadata(
		&self,
		sha: &str,
		cancel: &CancellationToken,
	) -> Result<GitCommitInfo, GitModelError> {
		let metadata = self.query.commit_metadata(&self.cwd, sha, cancel).await?;
		let (subject, body) = metadata
			.body
			.as_str()
			.split_once('\n')
			.unwrap_or((metadata.body.as_str(), ""));
		Ok(GitCommitInfo {
			sha:          metadata.hash,
			subject:      subject.to_str(),
			body:         body.trim().to_str(),
			author_name:  metadata.author_name,
			author_email: metadata.author_email,
			author_date:  metadata.author_date,
			parents:      metadata.parents,
			files:        Vec::new(),
		})
	}

	async fn resolve_lfs(&self, bytes: Bytes) -> Result<LoadedSide, GitModelError> {
		let Some(pointer) = parse_lfs_pointer(&bytes) else {
			return Ok(LoadedSide { bytes, unavailable: None });
		};
		if pointer.size > MAX_FILE_BYTES {
			return Ok(LoadedSide { bytes, unavailable: Some(lfs_placeholder(&pointer.oid)) });
		}
		let path = self
			.repository
			.common_dir
			.join("lfs")
			.join("objects")
			.join(&pointer.oid[..2])
			.join(&pointer.oid[2..4])
			.join(&pointer.oid);
		let object = match tokio::fs::read(&path).await {
			Ok(object) => object,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Ok(LoadedSide { bytes, unavailable: Some(lfs_placeholder(&pointer.oid)) });
			},
			Err(source) => return Err(GitModelError::LfsIo { path, source }),
		};
		if u64::try_from(object.len()).ok() != Some(pointer.size) {
			return Ok(LoadedSide { bytes, unavailable: Some(lfs_placeholder(&pointer.oid)) });
		}
		Ok(LoadedSide { bytes: Bytes::from(object), unavailable: None })
	}
}

#[derive(Clone, Copy)]
enum DiffSide {
	Old,
	New,
}

enum StreamSource<'a> {
	Empty,
	Git(&'a str),
	Worktree(&'a str),
}

#[derive(Default)]
struct StreamedSide {
	bytes:     Bytes,
	too_large: bool,
}

enum StreamEvent {
	Chunk(DiffSide, Bytes),
	Finished(DiffSide),
}

#[derive(Default)]
struct CompleteLineSplitter {
	pending: BytesMut,
}

impl CompleteLineSplitter {
	fn push(&mut self, bytes: &[u8]) -> Vec<Str> {
		self.pending.extend_from_slice(bytes);
		let mut lines = Vec::new();
		let mut start = 0;
		while let Some(relative) = self.pending[start..].iter().position(|byte| *byte == b'\n') {
			let end = start + relative;
			lines.push(decode_utf8(&self.pending[start..end]).to_str());
			start = end + 1;
		}
		if start > 0 {
			let _ = self.pending.split_to(start);
		}
		lines
	}

	fn finish(&mut self) -> Vec<Str> {
		if self.pending.is_empty() {
			Vec::new()
		} else {
			vec![decode_utf8(&self.pending.split().freeze()).to_str()]
		}
	}
}

#[derive(Default)]
struct SideStream {
	splitter:  CompleteLineSplitter,
	undecided: BytesMut,
	lines:     Vec<Str>,
	bytes:     usize,
	text:      Option<bool>,
}

impl SideStream {
	fn push(&mut self, path: &str, bytes: &[u8]) {
		self.bytes = self.bytes.saturating_add(bytes.len());
		match self.text {
			Some(true) => self.lines.extend(self.splitter.push(bytes)),
			Some(false) => {},
			None => {
				self.undecided.extend_from_slice(bytes);
				if self.undecided.len() >= BINARY_SNIFF_BYTES {
					self.classify(path);
				}
			},
		}
	}

	fn finish(&mut self, path: &str) {
		if self.text.is_none() {
			self.classify(path);
		}
		if self.text == Some(true) {
			self.lines.extend(self.splitter.finish());
		}
	}

	fn classify(&mut self, path: &str) {
		let header = &self.undecided[..self.undecided.len().min(BINARY_SNIFF_BYTES)];
		let text = self.undecided.is_empty()
			|| (!path_looks_like_media(path)
				&& !could_be_lfs_pointer(header)
				&& !looks_like_svg(header)
				&& !is_binary(header));
		self.text = Some(text);
		if text {
			self.lines.extend(self.splitter.push(&self.undecided));
		}
		self.undecided.clear();
	}

	fn take_lines(&mut self) -> Vec<Str> {
		std::mem::take(&mut self.lines)
	}
}

async fn collect_stream_events(
	path: &str,
	events: flume::Receiver<StreamEvent>,
	on_chunk: &mut (impl FnMut(Vec<Str>, Vec<Str>) + Send),
) -> Result<(), GitModelError> {
	let mut old = SideStream::default();
	let mut new = SideStream::default();
	let mut finished = 0_u8;
	let mut streaming = false;
	while finished < 2 {
		let Ok(event) = events.recv_async().await else {
			break;
		};
		match event {
			StreamEvent::Chunk(DiffSide::Old, bytes) => old.push(path, &bytes),
			StreamEvent::Chunk(DiffSide::New, bytes) => new.push(path, &bytes),
			StreamEvent::Finished(DiffSide::Old) => {
				old.finish(path);
				finished += 1;
			},
			StreamEvent::Finished(DiffSide::New) => {
				new.finish(path);
				finished += 1;
			},
		}
		let classified = old.text.is_some() && new.text.is_some();
		let streamable = old.text != Some(false) && new.text != Some(false);
		if !streaming
			&& classified
			&& streamable
			&& (old.bytes > STREAM_BUFFER_BYTES || new.bytes > STREAM_BUFFER_BYTES)
		{
			streaming = true;
		}
		if streaming {
			let old_lines = old.take_lines();
			let new_lines = new.take_lines();
			if !old_lines.is_empty() || !new_lines.is_empty() {
				on_chunk(old_lines, new_lines);
			}
		}
	}
	Ok(())
}

fn decode_utf8(bytes: &[u8]) -> String {
	String::from_units(xutf::transcode::<xutf::Utf8, xutf::Utf8>(bytes))
}

fn path_looks_like_media(path: &str) -> bool {
	Path::new(path)
		.extension()
		.and_then(|extension| extension.to_str())
		.is_some_and(|extension| {
			matches!(
				extension.to_ascii_lowercase().as_str(),
				"svg" | "svgz" | "png" | "jpg" | "jpeg" | "gif" | "webp"
			)
		})
}

fn could_be_lfs_pointer(bytes: &[u8]) -> bool {
	if bytes.len() > LFS_POINTER_MAX_BYTES {
		return false;
	}
	let prefix = LFS_VERSION.as_bytes();
	let compared = bytes.len().min(prefix.len());
	bytes[..compared] == prefix[..compared]
}

struct LoadedSide {
	bytes:       Bytes,
	unavailable: Option<Str>,
}

fn lfs_placeholder(oid: &Str) -> Str {
	omp_core::sf!("Git LFS object unavailable · sha256:{}…", oid.slice(..oid.len().min(12)))
}

const fn binary_placeholder() -> Str {
	Str::new_static("Binary object")
}

async fn rasterize_svg_side(path: &str, mut side: LoadedSide) -> LoadedSide {
	if side.bytes.is_empty() || side.unavailable.is_some() {
		return side;
	}
	let extension = Path::new(path)
		.extension()
		.and_then(|extension| extension.to_str());
	let gzip = extension.is_some_and(|extension| extension.eq_ignore_ascii_case("svgz"));
	if !gzip
		&& !extension.is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
		&& !looks_like_svg(&side.bytes)
	{
		return side;
	}
	let source = side.bytes.clone();
	match tokio::task::spawn_blocking(move || omp_tools::read::image::rasterize_svg(&source, gzip))
		.await
	{
		Ok(Ok(png)) => side.bytes = png,
		Ok(Err(_)) | Err(_) => {
			side.bytes = Bytes::new();
			side.unavailable = Some(Str::new_static("SVG preview unavailable"));
		},
	}
	side
}

#[derive(Debug, Eq, PartialEq)]
struct LfsPointer {
	oid:  Str,
	size: u64,
}

fn parse_lfs_pointer(bytes: &[u8]) -> Option<LfsPointer> {
	if bytes.len() > LFS_POINTER_MAX_BYTES {
		return None;
	}
	let text = xutf::to_string::<xutf::Utf8>(bytes).ok()?;
	let mut lines = text.lines();
	if lines.next()? != LFS_VERSION {
		return None;
	}
	let mut oid = None;
	let mut size = None;
	for line in lines {
		if let Some(value) = line.strip_prefix("oid sha256:")
			&& value.len() == 64
			&& value
				.bytes()
				.all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
		{
			oid = Some(value.to_str());
		} else if let Some(value) = line.strip_prefix("size ") {
			size = value.parse().ok();
		}
	}
	Some(LfsPointer { oid: oid?, size: size? })
}

fn media_format(path: &str, old: &LoadedSide, new: &LoadedSide) -> Option<Str> {
	let visible_side = (!old.bytes.is_empty() && old.unavailable.is_none())
		|| (!new.bytes.is_empty() && new.unavailable.is_none());
	let missing_side = old.unavailable.is_some() || new.unavailable.is_some();
	if !visible_side && !missing_side {
		return None;
	}
	if looks_like_svg(&old.bytes) || looks_like_svg(&new.bytes) {
		return Some(Str::new_static("svg"));
	}
	let Some(extension) = Path::new(path)
		.extension()
		.and_then(|extension| extension.to_str())
		.map(str::to_ascii_lowercase)
	else {
		return missing_side.then(|| Str::new_static("binary"));
	};
	let (token, format) = match extension.as_str() {
		"svg" | "svgz" => return Some(Str::new_static("svg")),
		"png" => ("png", image::ImageFormat::Png),
		"jpg" | "jpeg" => ("jpeg", image::ImageFormat::Jpeg),
		"gif" => ("gif", image::ImageFormat::Gif),
		"webp" => ("webp", image::ImageFormat::WebP),
		_ => return missing_side.then(|| Str::new_static("binary")),
	};
	(missing_side || has_image_format(old, format) || has_image_format(new, format))
		.then(|| token.to_str())
}

fn has_image_format(side: &LoadedSide, expected: image::ImageFormat) -> bool {
	side.unavailable.is_none()
		&& image::guess_format(&side.bytes).is_ok_and(|format| format == expected)
}

fn looks_like_svg(bytes: &[u8]) -> bool {
	let header = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
	if is_binary(header) {
		return false;
	}
	let Ok(text) = xutf::to_string::<xutf::Utf8>(header) else {
		return false;
	};
	let lowercase = text.to_ascii_lowercase();
	let mut rest = lowercase.as_str();
	while let Some(index) = rest.find("<svg") {
		let after = &rest[index + 4..];
		if after.starts_with('>') || after.starts_with(char::is_whitespace) {
			return true;
		}
		rest = after;
	}
	false
}

fn is_binary(bytes: &[u8]) -> bool {
	omp_tools::read::is_probably_binary_header(&bytes[..bytes.len().min(BINARY_SNIFF_BYTES)])
}

fn fingerprint(head: &[u8], status: &[u8]) -> [u8; 32] {
	let mut hasher = Hash32::hasher();
	hasher.update(head);
	hasher.update(&[0]);
	hasher.update(status);
	hasher.finalize().into_bytes()
}

fn line_range((start, end): (u32, u32)) -> Option<LineRange> {
	(start != 0 || end != 0).then(|| LineRange::new(u64::from(start), u64::from(end)))
}

fn rows_from_status(
	entries: &[StatusEntry],
	worktree_stats: &[NumstatEntry],
	staged_stats: &[NumstatEntry],
) -> (Vec<GitFileRow>, Vec<GitFileRow>) {
	let worktree_counts = count_map(worktree_stats);
	let staged_counts = count_map(staged_stats);
	let mut unstaged = Vec::new();
	let mut staged = Vec::new();
	for entry in entries {
		let path = lossy(entry.path.as_bytes());
		let orig_path = entry.orig_path.as_ref().map(|path| lossy(path.as_bytes()));
		if entry.untracked {
			unstaged.push(row(path, None, GitChangeKind::Untracked, GitArea::Unstaged, None));
			continue;
		}
		if entry.conflicted {
			unstaged.push(row(path, None, GitChangeKind::Conflicted, GitArea::Unstaged, None));
			continue;
		}
		if let Some(kind) = entry.staged {
			staged.push(row(
				path.clone(),
				orig_path,
				change_kind(kind),
				GitArea::Staged,
				staged_counts.get(entry.path.as_bytes()).copied(),
			));
		}
		if let Some(kind) = entry.worktree {
			unstaged.push(row(
				path,
				None,
				change_kind(kind),
				GitArea::Unstaged,
				worktree_counts.get(entry.path.as_bytes()).copied(),
			));
		}
	}
	(unstaged, staged)
}

fn apply_counts(rows: &mut [GitFileRow], entries: &[NumstatEntry]) {
	let counts = count_map(entries);
	for row in rows {
		if let Some((additions, deletions)) = counts.get(row.path.as_bytes()).copied() {
			row.additions = additions;
			row.deletions = deletions;
		}
	}
}

fn count_map(entries: &[NumstatEntry]) -> HashMap<&[u8], (Option<u64>, Option<u64>)> {
	entries
		.iter()
		.map(|entry| (entry.path.as_bytes(), (line_count(entry.added), line_count(entry.removed))))
		.collect()
}

fn row(
	path: Str,
	orig_path: Option<Str>,
	kind: GitChangeKind,
	area: GitArea,
	counts: Option<(Option<u64>, Option<u64>)>,
) -> GitFileRow {
	let (additions, deletions) = counts.unwrap_or((None, None));
	GitFileRow { path, orig_path, kind, area, additions, deletions }
}

fn commit_row(entry: NumstatEntry) -> GitFileRow {
	let additions = line_count(entry.added);
	let deletions = line_count(entry.removed);
	let kind = if additions.unwrap_or_default() > 0 && deletions == Some(0) {
		GitChangeKind::Added
	} else {
		GitChangeKind::Modified
	};
	GitFileRow {
		path: lossy(entry.path.as_bytes()),
		orig_path: entry.old_path.map(|path| lossy(path.as_bytes())),
		kind,
		area: GitArea::Commit,
		additions,
		deletions,
	}
}

fn line_count(count: LineCount) -> Option<u64> {
	match count {
		LineCount::Lines(lines) => Some(lines),
		LineCount::Binary => None,
	}
}

fn change_kind(kind: ChangeKind) -> GitChangeKind {
	match kind {
		ChangeKind::Added => GitChangeKind::Added,
		ChangeKind::Deleted => GitChangeKind::Deleted,
		ChangeKind::Renamed | ChangeKind::Copied => GitChangeKind::Renamed,
		ChangeKind::Unmerged => GitChangeKind::Conflicted,
		ChangeKind::Modified | ChangeKind::TypeChanged => GitChangeKind::Modified,
	}
}

fn lossy(bytes: &[u8]) -> Str {
	decode_utf8(bytes).to_str()
}

#[cfg(test)]
mod tests {
	use std::{fs, process::Command};

	use omp_envd::vcs::git::diff::parse_status_entries;

	use super::*;

	#[test]
	fn status_rows_preserve_git_areas_conflicts_renames_and_counts() {
		let entries = parse_status_entries(
			b"M  staged.txt\0 M work.txt\0?? new.txt\0UU conflict.txt\0R  renamed.txt\0old.txt\0",
		);
		let worktree = diff::parse_numstat(Bytes::from_static(b"3\t1\twork.txt\0")).unwrap();
		let staged_stats = diff::parse_numstat(Bytes::from_static(
			b"2\t0\tstaged.txt\x000\t0\t\x00old.txt\x00renamed.txt\x00",
		))
		.unwrap();
		let (unstaged, staged) = rows_from_status(&entries, &worktree, &staged_stats);
		assert_eq!(unstaged.len(), 3);
		assert_eq!(unstaged[0].additions, Some(3));
		assert_eq!(unstaged[1].kind, GitChangeKind::Untracked);
		assert_eq!(unstaged[2].kind, GitChangeKind::Conflicted);
		assert_eq!(staged.len(), 2);
		assert_eq!(staged[1].kind, GitChangeKind::Renamed);
		assert_eq!(staged[1].orig_path.as_deref(), Some("old.txt"));
	}
	fn fixture_git(cwd: &Path, arguments: &[&str]) {
		let output = Command::new("git")
			.current_dir(cwd)
			.args(arguments)
			.output()
			.expect("fixture git should launch");
		assert!(
			output.status.success(),
			"fixture git {arguments:?} failed: {}",
			decode_utf8(&output.stderr)
		);
	}

	#[tokio::test]
	async fn real_repository_refresh_emits_fast_then_stats_snapshots() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("tracked.txt"), "first\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "tracked.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);

		let cancel = CancellationToken::new();
		let mut model = GitModel::open(fixture.path(), None, &cancel).await.unwrap();
		let initial = model
			.refresh(&cancel)
			.await
			.unwrap()
			.expect("initial snapshot");
		assert_eq!(initial.branch.as_deref(), Some("main"));
		assert!(initial.unstaged.is_empty());
		assert!(initial.staged.is_empty());
		assert!(model.refresh(&cancel).await.unwrap().is_none());

		fs::write(fixture.path().join("tracked.txt"), "first\nsecond\n").expect("changed file");
		let changed = model
			.refresh(&cancel)
			.await
			.unwrap()
			.expect("changed snapshot");
		assert_eq!(changed.unstaged.len(), 1);
		assert_eq!(changed.unstaged[0].path.as_str(), "tracked.txt");
		assert_eq!(changed.unstaged[0].additions, None);
		let head = changed
			.head
			.as_ref()
			.expect("dirty snapshots retain HEAD metadata");
		assert_eq!(head.subject.as_str(), "seed");
		assert_eq!(head.author_name.as_str(), "OMP Test");

		let with_stats = model
			.load_deferred_stats(&cancel)
			.await
			.unwrap()
			.expect("dirty snapshot receives deferred stats");
		assert_eq!(with_stats.unstaged[0].additions, Some(1));
		assert!(model.load_deferred_stats(&cancel).await.unwrap().is_none());
		assert!(model.refresh(&cancel).await.unwrap().is_none());
	}

	#[test]
	fn lfs_pointer_parser_requires_canonical_version_oid_and_size() {
		let oid = "0123456789abcdef".repeat(4);
		let pointer = format!("{LFS_VERSION}\r\noid sha256:{oid}\r\nsize 1234\r\n");
		assert_eq!(
			parse_lfs_pointer(pointer.as_bytes()),
			Some(LfsPointer { oid: oid.to_str(), size: 1234 })
		);
		assert!(parse_lfs_pointer(format!("{LFS_VERSION}\noid sha256:{oid}\n").as_bytes()).is_none());
		assert!(
			parse_lfs_pointer(
				format!("{LFS_VERSION}\noid sha256:{}\nsize 1\n", oid.to_ascii_uppercase()).as_bytes()
			)
			.is_none()
		);
	}

	#[tokio::test]
	async fn staged_media_resolves_local_lfs_object_bytes() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("seed.txt"), "seed\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "seed.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);

		let oid = "0123456789abcdef".repeat(4);
		let object = b"\x89PNG\r\n\x1a\nlocal-lfs-image";
		let pointer = format!("{LFS_VERSION}\noid sha256:{oid}\nsize {}\n", object.len());
		fs::write(fixture.path().join("image.png"), pointer).expect("LFS pointer");
		fixture_git(fixture.path(), &["add", "image.png"]);
		let object_dir = fixture
			.path()
			.join(".git/lfs/objects")
			.join(&oid[..2])
			.join(&oid[2..4]);
		fs::create_dir_all(&object_dir).expect("LFS object directory");
		fs::write(object_dir.join(&oid), object).expect("LFS object");

		let cancel = CancellationToken::new();
		let mut model = GitModel::open(fixture.path(), None, &cancel).await.unwrap();
		let _ = model.refresh(&cancel).await.unwrap();
		let contents = model
			.contents(GitArea::Staged, "image.png", None, &cancel)
			.await
			.unwrap();
		assert_eq!(contents.media, Some(Str::new_static("png")));
		assert_eq!(contents.new_bytes.as_deref(), Some(&object[..]));
		assert!(contents.new_placeholder.is_none());

		fs::remove_file(object_dir.join(&oid)).expect("remove LFS object");
		let missing = model
			.contents(GitArea::Staged, "image.png", None, &cancel)
			.await
			.unwrap();
		assert_eq!(missing.media, Some(Str::new_static("png")));
		assert!(missing.new_bytes.is_none());
		assert!(
			missing
				.new_placeholder
				.is_some_and(|message| message.contains("Git LFS object unavailable"))
		);
	}

	#[tokio::test]
	async fn staged_svg_is_rasterized_for_terminal_preview() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("seed.txt"), "seed\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "seed.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);
		fs::write(
			fixture.path().join("image.svg"),
			r#"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="7"><rect width="12" height="7" fill="red"/></svg>"#,
		)
		.expect("SVG file");
		fixture_git(fixture.path(), &["add", "image.svg"]);

		let cancel = CancellationToken::new();
		let mut model = GitModel::open(fixture.path(), None, &cancel).await.unwrap();
		let _ = model.refresh(&cancel).await.unwrap();
		let contents = model
			.contents(GitArea::Staged, "image.svg", None, &cancel)
			.await
			.unwrap();
		assert_eq!(contents.media, Some(Str::new_static("svg")));
		assert!(
			contents
				.new_bytes
				.as_deref()
				.is_some_and(|bytes| bytes.starts_with(b"\x89PNG\r\n\x1a\n"))
		);
		assert!(contents.new_placeholder.is_none());
	}

	#[test]
	fn media_sniff_classifies_extensions_svg_content_and_binary_headers() {
		let empty = || LoadedSide { bytes: Bytes::new(), unavailable: None };
		let side = |bytes: &'static [u8]| LoadedSide {
			bytes:       Bytes::from_static(bytes),
			unavailable: None,
		};
		assert_eq!(
			media_format("art.JPG", &empty(), &side(b"\xff\xd8\xff")),
			Some(Str::new_static("jpeg"))
		);
		assert_eq!(
			media_format(
				"art.dat",
				&empty(),
				&side(b"<?xml version=\"1.0\"?><SVG viewBox=\"0 0 1 1\">")
			),
			Some(Str::new_static("svg"))
		);
		assert_eq!(media_format("notes.txt", &empty(), &side(b"plain text")), None);
		assert!(is_binary(b"plain\0binary"));
		assert!(!is_binary(b"plain UTF-8 text"));
	}

	#[test]
	fn complete_line_splitter_preserves_boundaries_crlf_and_lossy_utf8() {
		let mut splitter = CompleteLineSplitter::default();
		assert!(splitter.push(b"prefix \xf0\x9f").is_empty());
		assert_eq!(splitter.push(b"\x98\x80\r\nnext\n"), vec![
			Str::new_static("prefix 😀\r"),
			Str::new_static("next")
		]);
		assert!(splitter.push(b"trail").is_empty());
		assert_eq!(splitter.finish(), vec![Str::new_static("trail")]);

		let mut invalid = CompleteLineSplitter::default();
		assert_eq!(invalid.push(b"bad \xff\n"), vec![Str::new_static("bad �")]);
	}

	#[tokio::test]
	async fn small_contents_use_terminal_only_fast_path() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("small.txt"), "old\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "small.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);
		fs::write(fixture.path().join("small.txt"), "old\nnew\n").expect("changed file");

		let cancel = CancellationToken::new();
		let model = GitModel::open(fixture.path(), None, &cancel).await.unwrap();
		let mut chunks = Vec::new();
		let contents = model
			.contents_stream(GitArea::Unstaged, "small.txt", None, &cancel, |old, new| {
				chunks.push((old, new));
			})
			.await
			.unwrap();
		assert!(chunks.is_empty());
		assert_eq!(contents.old_text.as_str(), "old\n");
		assert_eq!(contents.new_text.as_str(), "old\nnew\n");
	}

	#[tokio::test]
	async fn large_contents_stream_complete_lines_before_terminal_contents() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("large.txt"), "seed\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "large.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);
		let changed = (0..8_000)
			.map(|line| format!("{line:05} {}\n", "streaming-content".repeat(3)))
			.collect::<String>();
		assert!(changed.len() > STREAM_BUFFER_BYTES);
		fs::write(fixture.path().join("large.txt"), &changed).expect("changed file");

		let cancel = CancellationToken::new();
		let model = GitModel::open(fixture.path(), None, &cancel).await.unwrap();
		let mut chunks = Vec::new();
		let contents = model
			.contents_stream(GitArea::Unstaged, "large.txt", None, &cancel, |old, new| {
				chunks.push((old, new));
			})
			.await
			.unwrap();
		assert!(!chunks.is_empty());
		assert_eq!(contents.new_text.as_str(), changed);
		let streamed_new = chunks.iter().map(|(_, new)| new.len()).sum::<usize>();
		assert_eq!(streamed_new, changed.lines().count());
		assert_eq!(
			chunks
				.iter()
				.flat_map(|(_, new)| new.iter().map(Str::as_str))
				.collect::<Vec<_>>(),
			changed.lines().collect::<Vec<_>>()
		);
	}

	#[test]
	fn fingerprint_distinguishes_head_and_raw_status_but_deduplicates_exact_input() {
		let first = fingerprint(b"abc", b" M file\0");
		assert_eq!(first, fingerprint(b"abc", b" M file\0"));
		assert_ne!(first, fingerprint(b"def", b" M file\0"));
		assert_ne!(first, fingerprint(b"abc", b"M  file\0"));
	}
}
