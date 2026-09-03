//! Elastic speculative transcript slots.
//!
//! The engine separates mutable viewport presentation from the irreversible
//! terminal history transaction. [`Slots::plan`] only stages rows; history and
//! lifecycle state advance in [`Slots::commit`] after the presenter reports
//! what reached the terminal.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use omp_core::Str;

use crate::{
	Frame, IntoComponent, Prop, Size, Ui, UiContext,
	components::{Pre, TextLeaf},
	frame_text,
};

const APPEND_TEXT_ID: &str = "elastic-slots-append";

/// Stable, creation-ordered block identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockId(NonZeroU32);

impl BlockId {
	fn index(self) -> usize {
		usize::try_from(self.0.get() - 1).expect("u32 block index fits usize")
	}

	/// Returns the one-based numeric identity.
	pub const fn get(self) -> u32 {
		self.0.get()
	}
}

/// A block's presentation contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum Mode {
	/// Snapshots may be replaced, but never enter history before finalization.
	Mutable,
	/// Content grows by prefix extension and stable rows may stream from the
	/// head.
	AppendOnly,
}

/// Native-history behavior when terminal width changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ResizePolicy {
	/// Keep the current native-history epoch unchanged.
	Preserve,
	/// Append a newly wrapped copy of logical history.
	Append,
	/// Clear the physical epoch and replay logical history at the new width.
	#[default]
	Rebuild,
}

/// Observable block lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockState {
	/// The block may still change.
	Active,
	/// The block is sealed but has rows awaiting delivery.
	Finalized,
	/// Every final row has been delivered exactly once.
	Committed,
}

/// One width-independent logical-history row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
	block:   BlockId,
	ordinal: u32,
	text:    Str,
}

impl Row {
	/// Block that owns this row.
	pub const fn block(&self) -> BlockId {
		self.block
	}

	/// Zero-based row position in the block snapshot that was committed.
	pub const fn ordinal(&self) -> u32 {
		self.ordinal
	}

	/// Plain semantic fallback used to rewrap history after a resize.
	pub fn text(&self) -> &str {
		self.text.as_str()
	}
}

/// One physical row staged for the presenter.
#[derive(Clone, Debug)]
pub struct PlannedRow {
	logical: Row,
	frame:   Arc<Frame>,
}

impl PlannedRow {
	/// Logical source of this physical row.
	pub const fn logical(&self) -> &Row {
		&self.logical
	}

	/// One-row frame ready for terminal materialization.
	pub fn frame(&self) -> &Frame {
		&self.frame
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanKind {
	Normal,
	Replay,
	Repair(BlockId),
}

/// Staged terminal write. Creating a plan has no history side effect.
#[derive(Clone, Debug)]
pub struct WritePlan {
	serial:   u64,
	kind:     PlanKind,
	rows:     Arc<[PlannedRow]>,
	viewport: Arc<Frame>,
	rebuild:  bool,
}

impl WritePlan {
	/// History rows to append, in delivery order.
	pub fn rows(&self) -> &[PlannedRow] {
		&self.rows
	}

	/// Final viewport that must remain visible after the history rows scroll.
	pub fn viewport(&self) -> &Frame {
		&self.viewport
	}

	/// Whether presentation starts a new physical history epoch.
	pub const fn rebuild(&self) -> bool {
		self.rebuild
	}

	/// Viewport height captured by this transaction.
	pub fn viewport_rows(&self) -> u16 {
		self.viewport.size().height
	}

	/// Whether the transaction carries no history rows and no epoch reset.
	pub fn is_paint_only(&self) -> bool {
		self.rows.is_empty() && !self.rebuild
	}
}

/// Presenter acknowledgement for a staged write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivered {
	/// Every staged history row reached the terminal.
	All,
	/// Exactly this prefix of staged history rows reached the terminal.
	Partial(usize),
}

struct Block {
	mode:        Mode,
	state:       BlockState,
	ui:          Option<Ui>,
	text:        String,
	rendered:    Vec<Arc<Frame>>,
	commit_rows: Vec<Arc<Frame>>,
	emitted:     usize,
}

impl Block {
	fn new(mode: Mode) -> Self {
		Self {
			mode,
			state: BlockState::Active,
			ui: None,
			text: String::new(),
			rendered: Vec::new(),
			commit_rows: Vec::new(),
			emitted: 0,
		}
	}
}

struct Replay {
	rows:       Arc<[PlannedRow]>,
	cursor:     usize,
	rebuild:    bool,
	reset_sent: bool,
}

struct Repair {
	block:      BlockId,
	rows:       Arc<[PlannedRow]>,
	cursor:     usize,
	reset_sent: bool,
}

/// Elastic transcript state: retained blocks, logical history, and viewport.
pub struct Slots {
	width:         u16,
	viewport_rows: u16,
	policy:        ResizePolicy,
	ctx:           UiContext,
	blocks:        Vec<Block>,
	frontier:      usize,
	history:       Vec<Row>,
	replay:        Option<Replay>,
	repair:        Option<Repair>,
	pending:       Option<WritePlan>,
	next_serial:   u64,
}

impl Slots {
	/// Creates an empty transcript at the supplied terminal geometry.
	///
	/// Width and viewport height are clamped to one because terminal frame
	/// materialization has no useful zero-geometry state.
	pub fn new(width: u16, viewport_rows: u16, policy: ResizePolicy) -> Self {
		Self {
			width: width.max(1),
			viewport_rows: viewport_rows.max(1),
			policy,
			ctx: UiContext::default(),
			blocks: Vec::new(),
			frontier: 0,
			history: Vec::new(),
			replay: None,
			repair: None,
			pending: None,
			next_serial: 1,
		}
	}

	/// Replaces the ambient renderer context and refreshes retained snapshots.
	pub fn set_context(&mut self, ctx: UiContext) {
		self.rollback_pending();
		self.ctx = ctx;
		for block in &mut self.blocks {
			if let Some(ui) = &mut block.ui {
				ui.set_context(self.ctx.clone());
			}
		}
		self.render_all();
	}

	/// Opens a new active block in creation and commitment order.
	pub fn open(&mut self, mode: Mode) -> BlockId {
		self.rollback_pending();
		let number = u32::try_from(self.blocks.len() + 1).expect("slot block identity overflow");
		let id = BlockId(NonZeroU32::new(number).expect("block identities start at one"));
		self.blocks.push(Block::new(mode));
		id
	}

	/// Replaces a mutable active or finalized-but-uncommitted block snapshot.
	///
	/// Replacing a final whose prefix was physically delivered schedules an
	/// atomic Rebuild transaction; logical history changes only after the
	/// rebuilt band is acknowledged in full.
	pub fn set(&mut self, id: BlockId, content: impl IntoComponent) {
		self.rollback_pending();
		let width = self.width;
		let ctx = self.ctx.clone();
		let block = self.block_mut(id);
		assert_eq!(block.mode, Mode::Mutable, "set requires a mutable block");
		assert_ne!(block.state, BlockState::Committed, "committed blocks are immutable");
		block.ui = Some(Ui::from_root(content, width, ctx));
		block.text.clear();
		Self::render_block(width, block);
		block.commit_rows.clone_from(&block.rendered);
		if block.emitted > 0 {
			self.begin_repair(id);
		}
	}

	/// Extends an active append-only block by a stable text prefix.
	pub fn append(&mut self, id: BlockId, text: &str) {
		self.rollback_pending();
		let width = self.width;
		let ctx = self.ctx.clone();
		let block = self.block_mut(id);
		assert_eq!(block.mode, Mode::AppendOnly, "append requires an append-only block");
		assert_eq!(block.state, BlockState::Active, "append requires an active block");
		block.text.push_str(text);
		let ui = block.ui.get_or_insert_with(|| {
			Ui::from_root(
				TextLeaf::new()
					.with(Prop::Id, APPEND_TEXT_ID)
					.with(Prop::Reveal, true),
				width,
				ctx,
			)
		});
		ui.set_text(APPEND_TEXT_ID, Str::from(block.text.as_str()));
		Self::render_block(width, block);
	}

	/// Seals a block. Finalization itself never writes history.
	pub fn finalize(&mut self, id: BlockId) {
		self.rollback_pending();
		let width = self.width;
		let ctx = self.ctx.clone();
		let block = self.block_mut(id);
		assert_eq!(block.state, BlockState::Active, "only active blocks finalize");
		if block.mode == Mode::AppendOnly {
			if block.ui.is_none() {
				block.ui = Some(Ui::from_root(
					TextLeaf::new()
						.with(Prop::Id, APPEND_TEXT_ID)
						.with(Prop::Reveal, true),
					width,
					ctx.clone(),
				));
				Self::render_block(width, block);
			}
			block.commit_rows = Self::settled_text_rows(width, block.text.as_str(), ctx);
		}
		block.state = BlockState::Finalized;
		self.advance_frontier();
	}

	/// Changes terminal geometry and applies the configured native-history
	/// policy.
	///
	/// Height-only changes are logically and physically Preserve operations.
	pub fn resize(&mut self, width: u16, viewport_rows: u16) {
		self.rollback_pending();
		let width = width.max(1);
		let viewport_rows = viewport_rows.max(1);
		let width_changed = width != self.width;
		self.width = width;
		self.viewport_rows = viewport_rows;
		self.render_all();
		if let Some(index) = self
			.blocks
			.iter()
			.position(|block| block.state == BlockState::Finalized && block.emitted > 0)
		{
			self.begin_repair(Self::id(index));
			return;
		}
		if !width_changed || self.policy == ResizePolicy::Preserve {
			return;
		}
		let rows = self.replay_rows();
		self.replay = Some(Replay {
			rows:       rows.into(),
			cursor:     0,
			rebuild:    self.policy == ResizePolicy::Rebuild,
			reset_sent: false,
		});
	}

	/// Stages the next history delivery and the resulting viewport.
	///
	/// Repeated calls before [`Slots::commit`] return the same transaction.
	pub fn plan(&mut self) -> WritePlan {
		if let Some(plan) = &self.pending {
			return plan.clone();
		}
		let viewport = Arc::new(self.viewport());
		let (kind, rows, rebuild) = if let Some(repair) = &self.repair {
			(PlanKind::Repair(repair.block), repair.rows[repair.cursor..].to_vec(), !repair.reset_sent)
		} else if let Some(replay) = &self.replay {
			(
				PlanKind::Replay,
				replay.rows[replay.cursor..].to_vec(),
				replay.rebuild && !replay.reset_sent,
			)
		} else {
			(PlanKind::Normal, self.normal_rows(), false)
		};
		let plan = WritePlan { serial: self.next_serial, kind, rows: rows.into(), viewport, rebuild };
		self.next_serial = self.next_serial.wrapping_add(1).max(1);
		self.pending = Some(plan.clone());
		plan
	}

	/// Commits the acknowledged prefix of a staged terminal delivery.
	///
	/// Undelivered rows remain staged for the next [`Slots::plan`]. A presenter
	/// must not acknowledge more rows than the plan contains.
	pub fn commit(&mut self, plan: WritePlan, delivered: Delivered) {
		let Some(pending) = self.pending.take() else {
			panic!("commit requires a staged write plan");
		};
		assert_eq!(pending.serial, plan.serial, "write plan is stale");
		let count = match delivered {
			Delivered::All => plan.rows.len(),
			Delivered::Partial(rows) => rows,
		};
		assert!(count <= plan.rows.len(), "delivered prefix exceeds the write plan");
		match plan.kind {
			PlanKind::Normal => self.commit_normal(&plan.rows[..count]),
			PlanKind::Replay => self.commit_replay(count),
			PlanKind::Repair(id) => self.commit_repair(id, count),
		}
	}

	/// Canonical logical history, in block and row order.
	pub fn logical_history(&self) -> impl Iterator<Item = &Row> {
		self.history.iter()
	}

	/// Current lifecycle of a known block.
	pub fn state(&self, id: BlockId) -> BlockState {
		self.block(id).state
	}

	/// Stable row prefix already acknowledged for a block.
	pub fn emitted(&self, id: BlockId) -> usize {
		self.block(id).emitted
	}

	/// Current terminal width.
	pub const fn width(&self) -> u16 {
		self.width
	}

	/// Current viewport height.
	pub const fn viewport_height(&self) -> u16 {
		self.viewport_rows
	}

	/// Advances renderer-owned effects such as append-only stream reveal.
	///
	/// Returns whether any retained block repainted. Hosts should call this
	/// from their normal frame clock and present a new plan when it returns
	/// `true`.
	pub fn tick(&mut self, now: Duration) -> bool {
		self.rollback_pending();
		let width = self.width;
		let mut changed = false;
		for block in &mut self.blocks {
			if block.state == BlockState::Active && block.ui.as_mut().is_some_and(|ui| ui.tick(now)) {
				Self::render_block(width, block);
				changed = true;
			}
		}
		changed
	}

	fn block(&self, id: BlockId) -> &Block {
		self.blocks.get(id.index()).expect("unknown block identity")
	}

	fn block_mut(&mut self, id: BlockId) -> &mut Block {
		self
			.blocks
			.get_mut(id.index())
			.expect("unknown block identity")
	}

	fn rollback_pending(&mut self) {
		self.pending = None;
	}

	fn render_all(&mut self) {
		let width = self.width;
		for block in &mut self.blocks {
			if let Some(ui) = &mut block.ui {
				ui.resize(width);
			}
			Self::render_block(width, block);
			if block.mode == Mode::Mutable {
				block.commit_rows.clone_from(&block.rendered);
			} else if block.state != BlockState::Active {
				block.commit_rows =
					Self::settled_text_rows(width, block.text.as_str(), self.ctx.clone());
			}
			block.emitted = block.emitted.min(block.commit_rows.len());
		}
	}

	fn render_block(width: u16, block: &mut Block) {
		block.rendered.clear();
		let Some(ui) = &block.ui else {
			return;
		};
		block.rendered = Self::rows_from_frame(width, ui.frame());
	}

	fn settled_text_rows(width: u16, text: &str, ctx: UiContext) -> Vec<Arc<Frame>> {
		let ui = Ui::from_root(Pre::new().text(text), width, ctx);
		Self::rows_from_frame(width, ui.frame())
	}

	fn rows_from_frame(width: u16, frame: &Frame) -> Vec<Arc<Frame>> {
		let mut rows = Vec::with_capacity(usize::from(frame.size().height));
		for row in 0..frame.size().height {
			let mut one = Frame::new(Size::new(width, 1));
			one.blit(frame, row, 1, 0, 0);
			rows.push(Arc::new(one));
		}
		rows
	}

	fn normal_rows(&self) -> Vec<PlannedRow> {
		let mut out = Vec::new();
		for index in self.frontier..self.blocks.len() {
			let block = &self.blocks[index];
			let id = Self::id(index);
			match block.state {
				BlockState::Committed => continue,
				BlockState::Finalized => {
					self.extend_rows(&mut out, id, block, &block.commit_rows, block.commit_rows.len());
				},
				BlockState::Active if index == self.frontier && block.mode == Mode::AppendOnly => {
					let demand = self.live_demand();
					if demand > usize::from(self.viewport_rows) {
						let stable = if block.text.ends_with('\n') {
							block.rendered.len()
						} else {
							block.rendered.len().saturating_sub(1)
						};
						let excess = demand - usize::from(self.viewport_rows);
						self.extend_rows(
							&mut out,
							id,
							block,
							&block.rendered,
							stable.min(block.emitted + excess),
						);
					}
					break;
				},
				BlockState::Active => break,
			}
		}
		out
	}

	fn extend_rows(
		&self,
		out: &mut Vec<PlannedRow>,
		id: BlockId,
		block: &Block,
		frames: &[Arc<Frame>],
		end: usize,
	) {
		for ordinal in block.emitted..end {
			let frame = Arc::clone(&frames[ordinal]);
			out.push(PlannedRow { logical: Self::logical_row(id, ordinal, &frame), frame });
		}
	}

	fn logical_row(id: BlockId, ordinal: usize, frame: &Frame) -> Row {
		Row {
			block:   id,
			ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
			text:    Str::from(frame_text(frame).trim_end_matches('\n')),
		}
	}

	fn live_demand(&self) -> usize {
		self
			.blocks
			.iter()
			.filter(|block| block.state == BlockState::Active)
			.map(|block| block.rendered.len().saturating_sub(block.emitted).max(1))
			.sum()
	}

	fn viewport(&self) -> Frame {
		let mut viewport = Frame::new(Size::new(self.width, self.viewport_rows));
		let active = self
			.blocks
			.iter()
			.enumerate()
			.filter(|(_, block)| block.state == BlockState::Active)
			.collect::<Vec<_>>();
		if active.is_empty() {
			return viewport;
		}
		let visible_start = active.len().saturating_sub(usize::from(self.viewport_rows));
		let visible = &active[visible_start..];
		let mut allocations = vec![1_usize; visible.len()];
		let mut room = usize::from(self.viewport_rows).saturating_sub(visible.len());
		for (slot, (_, block)) in visible.iter().enumerate().rev() {
			let target = block
				.rendered
				.len()
				.saturating_sub(block.emitted)
				.clamp(1, 3);
			let extra = target.saturating_sub(1).min(room);
			allocations[slot] += extra;
			room -= extra;
		}
		let total: usize = allocations.iter().sum();
		let mut y = usize::from(self.viewport_rows).saturating_sub(total);
		for ((_, block), rows) in visible.iter().zip(allocations) {
			let available = block.rendered.len().saturating_sub(block.emitted);
			let take = rows.min(available);
			let source = block.rendered.len().saturating_sub(take);
			for frame in &block.rendered[source..] {
				viewport.blit(frame, 0, 1, 0, u16::try_from(y).expect("viewport row fits u16"));
				y += 1;
			}
			if take == 0 {
				y += 1;
			}
		}
		viewport
	}

	fn commit_normal(&mut self, rows: &[PlannedRow]) {
		for planned in rows {
			let id = planned.logical.block;
			let expected = self.block(id).emitted;
			assert_eq!(planned.logical.ordinal as usize, expected, "non-contiguous delivery");
			self.history.push(planned.logical.clone());
			self.block_mut(id).emitted += 1;
		}
		self.advance_frontier();
	}

	fn advance_frontier(&mut self) {
		while let Some(block) = self.blocks.get_mut(self.frontier) {
			if block.state != BlockState::Finalized || block.emitted != block.commit_rows.len() {
				break;
			}
			block.state = BlockState::Committed;
			self.frontier += 1;
		}
	}

	fn replay_rows(&self) -> Vec<PlannedRow> {
		let mut rows = Vec::new();
		for logical in &self.history {
			let ui =
				Ui::from_root(Pre::new().text(logical.text.clone()), self.width, self.ctx.clone());
			for physical in 0..ui.frame().size().height {
				let mut frame = Frame::new(Size::new(self.width, 1));
				frame.blit(ui.frame(), physical, 1, 0, 0);
				rows.push(PlannedRow { logical: logical.clone(), frame: Arc::new(frame) });
			}
		}
		rows
	}

	fn commit_replay(&mut self, count: usize) {
		let replay = self
			.replay
			.as_mut()
			.expect("replay plan requires replay state");
		replay.cursor += count;
		replay.reset_sent |= replay.rebuild;
		if replay.cursor == replay.rows.len() {
			self.replay = None;
		}
	}

	fn begin_repair(&mut self, id: BlockId) {
		let mut rows = Vec::new();
		for index in 0..id.index() {
			let block_id = Self::id(index);
			let block = &self.blocks[index];
			for (ordinal, frame) in block.commit_rows.iter().enumerate() {
				rows.push(PlannedRow {
					logical: Self::logical_row(block_id, ordinal, frame),
					frame:   Arc::clone(frame),
				});
			}
		}
		let block = self.block(id);
		for (ordinal, frame) in block.commit_rows.iter().enumerate() {
			rows.push(PlannedRow {
				logical: Self::logical_row(id, ordinal, frame),
				frame:   Arc::clone(frame),
			});
		}
		self.repair =
			Some(Repair { block: id, rows: rows.into(), cursor: 0, reset_sent: false });
	}

	fn commit_repair(&mut self, id: BlockId, count: usize) {
		let repair = self
			.repair
			.as_mut()
			.expect("repair plan requires repair state");
		assert_eq!(repair.block, id, "repair plan block changed");
		repair.cursor += count;
		repair.reset_sent = true;
		if repair.cursor != repair.rows.len() {
			return;
		}
		let replacement = repair
			.rows
			.iter()
			.map(|planned| planned.logical.clone())
			.collect::<Vec<_>>();
		self.history = replacement;
		let emitted = self.block(id).commit_rows.len();
		let block = self.block_mut(id);
		block.emitted = emitted;
		block.state = BlockState::Committed;
		self.frontier = id.index() + 1;
		self.repair = None;
		self.advance_frontier();
	}

	fn id(index: usize) -> BlockId {
		let number = u32::try_from(index + 1).expect("slot block identity overflow");
		BlockId(NonZeroU32::new(number).expect("block identities start at one"))
	}
}
