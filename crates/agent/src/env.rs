//! Environment-to-kernel events carried by the single upward mailbox.

use omp_core::Str;

/// Environment observation or control request for the session authority.
///
/// These messages are ephemeral transport. The kernel translates accepted
/// changes into session patches before they become authoritative.
#[derive(Clone, Debug)]
pub enum EnvEvent {
	/// The extension/device roster changed.
	DeviceAvailability {
		/// Canonical JSON projection of the available devices.
		payload: Str,
	},
	/// A checkpoint or rewind request crossed the environment boundary.
	CheckpointControl {
		/// Stable operation name (`checkpoint` or `schedule_rewind`).
		operation: Str,
		/// Canonical JSON operation arguments, including the host-issued receipt.
		payload:   Str,
	},
	/// A staged mutation requires a host-side resolution director.
	StagedPreview {
		/// Stable staged proposal identity.
		proposal_id: Str,
		/// Tool that produced the proposal.
		source_tool: Str,
	},
}
