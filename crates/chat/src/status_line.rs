//! Status-line values derived only from the actor's DOM replica.

use omp_core::Str;
use omp_dom::{Dom, KnownTag, PropId, Tag, Value};

/// Observer-visible status values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusLine {
	/// Last assistant model or model prompt fact.
	pub model:      Str,
	/// Session location projected from prompt facts.
	pub session:    Str,
	/// Home directory projected from prompt facts, for `~` shortening.
	pub home:       Str,
	/// Input tokens of the most recent receipt: the live context size.
	pub context:    u64,
	/// Total input tokens across visible turns.
	pub tokens_in:  u64,
	/// Total output tokens across visible turns.
	pub tokens_out: u64,
	/// Number of explicit turn elements.
	pub turns:      usize,
}

impl StatusLine {
	/// Derives a status line from one materialized tree.
	#[must_use]
	pub fn from_dom(dom: &Dom) -> Self {
		let mut model = prompt_fact(dom, "model", "identifier").unwrap_or_default();
		let session = prompt_fact(dom, "cwd", "").unwrap_or_else(|| Str::new_static("session"));
		let home = prompt_fact(dom, "home", "").unwrap_or_default();
		let mut context = 0_u64;
		let mut tokens_in = 0_u64;
		let mut tokens_out = 0_u64;
		let mut turns = 0;
		for turn in dom.children(dom.body()) {
			let Some(node) = dom.get(*turn) else {
				continue;
			};
			if node.tag != Tag::Known(KnownTag::Turn) {
				continue;
			}
			turns += 1;
			for child in dom
				.children(*turn)
				.iter()
				.filter_map(|handle| dom.get(*handle))
			{
				match child.tag {
					Tag::Known(KnownTag::Assistant) => {
						if let Some(value) = child.prop(&PropId::Model.into()).and_then(Value::as_str) {
							model = Str::new(value);
						}
					},
					Tag::Known(KnownTag::Usage) => {
						context = prop_u64(child, PropId::TokensIn);
						tokens_in = tokens_in.saturating_add(prop_u64(child, PropId::TokensIn));
						tokens_out = tokens_out.saturating_add(prop_u64(child, PropId::TokensOut));
					},
					_ => {},
				}
			}
		}
		Self { model, session, home, context, tokens_in, tokens_out, turns }
	}

	/// Builds the compact one-row presentation string on state change.
	#[must_use]
	pub fn text(&self) -> Str {
		Str::new(format!(
			"{} · {} · turn {} · {} in / {} out",
			if self.model.is_empty() {
				"model"
			} else {
				self.model.as_str()
			},
			self.session,
			self.turns,
			self.tokens_in,
			self.tokens_out
		))
	}
}

fn prompt_fact(dom: &Dom, outer: &str, inner: &str) -> Option<Str> {
	let value = dom
		.get(dom.meta())?
		.prop(&omp_dom::PropKey::Custom(Str::new_static("prompt-facts")))?;
	let Value::Json(raw) = value else {
		return None;
	};
	let value: serde_json::Value = serde_json::from_str(raw.get()).ok()?;
	let selected = value.get(outer)?;
	let text = if inner.is_empty() {
		selected.as_str()?
	} else {
		selected.get(inner)?.as_str()?
	};
	Some(Str::new(text))
}

fn prop_u64(node: &omp_dom::Node, prop: PropId) -> u64 {
	match node.prop(&prop.into()) {
		Some(Value::Int(value)) => u64::try_from(*value).unwrap_or_default(),
		_ => 0,
	}
}
