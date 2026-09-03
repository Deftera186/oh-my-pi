//! Vibe-mode Director declaration.

use omp_core::Str;
use omp_dom::Node;

use crate::director::{BindValue, Director, Slot};

const CLAIMS: &[Slot] = &[Slot::Mode, Slot::Loop];

/// Restricts the roster and defers delivery while coordinating vibe workers.
pub struct Vibe {
	binds: Vec<(Str, BindValue)>,
}

impl Vibe {
	/// Creates the standard vibe engagement.
	#[must_use]
	pub fn new() -> Self {
		Self { binds: vibe_binds() }
	}

	/// Reconstructs a vibe engagement from its DOM element.
	#[must_use]
	pub fn from_node(_node: &Node) -> Self {
		Self::new()
	}
}

impl Default for Vibe {
	fn default() -> Self {
		Self::new()
	}
}

impl Director for Vibe {
	fn id(&self) -> &'static str {
		"vibe"
	}

	fn claims(&self) -> &'static [Slot] {
		CLAIMS
	}

	fn binds(&self) -> &[(Str, BindValue)] {
		&self.binds
	}

	fn state(&self) -> Vec<(Str, BindValue)> {
		vec![(Str::new_static("tool"), BindValue::Str(Str::new_static("vibe_spawn")))]
	}
}

fn vibe_binds() -> Vec<(Str, BindValue)> {
	vec![
		(Str::new_static("toolset"), BindValue::Str(Str::new_static("read,todo"))),
		(Str::new_static("delivery_policy"), BindValue::Str(Str::new_static("defer-to-settle"))),
	]
}
