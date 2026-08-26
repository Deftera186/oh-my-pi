//! Disk-free hashline and replace engines over immutable exact-byte snapshots.
//!
//! Parsing, application, stale recovery, and fuzzy replacement produce exact
//! byte edits for a caller-owned transaction coordinator. Anchor-scoped
//! hashline edits preserve authored source identity through stale recovery.

/// Exact-byte hashline application.
pub mod apply;
/// Syntax-aware block edit lowering.
pub mod block;
/// Transaction-local cut and paste registers.
pub mod clipboard;
/// Compact post-edit diff previews.
pub mod diff_preview;
/// Codex apply-patch envelope parsing.
pub mod foreign_patch;
/// Hashline sigils, display helpers, and compatible snapshot tags.
pub mod format;
/// Packaged constrained-decoding grammars.
pub mod grammars;
/// Patch-envelope and file-section splitting.
pub mod input;
/// Repeated no-op escalation.
pub mod loop_guard;
/// Pi-compatible stale snapshot mismatch diagnostics.
pub mod mismatch;
/// BOM and line-ending normalization.
pub mod normalize;
/// Lenient operation state-machine parsing.
pub mod parser;
/// Conservative stale-edit recovery.
pub mod recovery;
/// Exact and fuzzy replacement.
pub mod replace;
/// Pure sloppy edit parsing and transformation.
pub mod sloppy;
/// Collision-aware retained read snapshots.
pub mod snapshots;
/// Syntax probes for conservative edit repair.
pub mod syntax;
/// Stateful line tokenization.
pub mod tokenizer;
/// Shared domain, edit, and diagnostic types.
pub mod types;
/// Unified-hunk parsing and transformation.
pub mod unified_hunk;

pub use apply::{
	ApplyError, ApplyMode, ApplyOptions, ApplyResult, ByteEdit, apply_edits, apply_parsed_patch,
	is_head_tail_only,
};
pub use clipboard::Clipboard;
pub use diff_preview::{NumberedDiff, numbered_diff};
pub use format::{
	compute_file_hash, format_cut_header, format_gap_locator, format_hashline_header,
	format_insert_header, format_numbered_line, format_numbered_lines, format_register,
	format_replace_header, normalize_file_hash_text, split_addressable_file_lines,
};
pub use input::{
	Patch, PatchSection, contains_recognizable_hashline_operations, split_patch_input,
};
pub use loop_guard::NoopLoopGuard;
pub use mismatch::{
	HEADTAIL_DRIFT_WARNING, MISMATCH_CONTEXT, MismatchDetails, MismatchError,
	RECOVERY_EXTERNAL_WARNING, RECOVERY_LINE_REMAP_WARNING, RECOVERY_SESSION_CHAIN_WARNING,
	format_anchored_context,
};
pub use normalize::{
	BomResult, LineEnding, detect_line_ending, normalize_to_lf, restore_bom, restore_line_endings,
	strip_bom,
};
pub use parser::{Executor, MAX_EXPANDED_RANGE_LINES, parse_patch, parse_patch_streaming};
pub use recovery::{
	ExactByteEdit, RecoveryEdit, RecoveryError, RecoveryResult, recover_exact, recover_from_store,
};
pub use replace::{ReplaceEdit, ReplaceError, ReplaceOptions, ReplaceResult, apply_replace};
pub use snapshots::{
	RevisionToken, Snapshot, SnapshotLookupError, SnapshotStore, SnapshotStoreError,
	SnapshotStoreOptions, compute_snapshot_tag,
};
pub use tokenizer::{
	BlockTarget, Token, Tokenizer, clone_cursor, is_hunk_header_text, parse_lid,
	split_hashline_lines,
};
pub use types::{
	Anchor, ApplyWarning, BlockMode, BlockResolution, BlockSpan, Cursor, Diagnostic, DiagnosticCode,
	DiagnosticSeverity, Edit, FileOp, InsertMode, ParseError, ParsedPatch, ParsedRange, PasteTarget,
	SplitOptions,
};
