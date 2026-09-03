//! Status-line values derived only from the actor's DOM replica.

use omp_core::Str;
use omp_dom::{Dom, KnownTag, PropId, Tag, Value};

/// Observer-visible status values.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusLine {
	/// Last assistant model or model prompt fact.
	pub model:             Str,
	/// Session location projected from prompt facts.
	pub session:           Str,
	/// Home directory projected from prompt facts, for `~` shortening.
	pub home:              Str,
	/// User-facing session title from the `<meta>` `name` prop, when the
	/// session has been named.
	pub name:              Option<Str>,
	/// Input tokens of the most recent receipt: the live context size.
	pub context:           u64,
	/// Total input tokens across visible turns.
	pub tokens_in:         u64,
	/// Total output tokens across visible turns.
	pub tokens_out:        u64,
	/// Total prompt-cache tokens read across visible turns.
	pub cache_read:        u64,
	/// Total prompt-cache tokens written across visible turns.
	pub cache_write:       u64,
	/// Total spend across visible turns in nano-US dollars.
	pub cost_nano_usd:     u64,
	/// Output throughput of the most recent receipt (`tokens_out` over
	/// `duration-ms`), when the receipt journals a duration.
	pub tokens_per_second: Option<f32>,
	/// Number of explicit turn elements.
	pub turns:             usize,
}

impl StatusLine {
	/// Derives a status line from one materialized tree.
	#[must_use]
	pub fn from_dom(dom: &Dom) -> Self {
		let mut model = prompt_fact(dom, "model", "identifier").unwrap_or_default();
		let session = prompt_fact(dom, "cwd", "").unwrap_or_else(|| Str::new_static("session"));
		let home = prompt_fact(dom, "home", "").unwrap_or_default();
		let name = dom
			.get(dom.meta())
			.and_then(|meta| meta.prop(&PropId::Name.into()))
			.and_then(Value::as_str)
			.filter(|title| !title.is_empty())
			.map(Str::new);
		let mut context = 0_u64;
		let mut tokens_in = 0_u64;
		let mut tokens_out = 0_u64;
		let mut cache_read = 0_u64;
		let mut cache_write = 0_u64;
		let mut cost_nano_usd = 0_u64;
		let mut tokens_per_second = None;
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
						let out = prop_u64(child, PropId::TokensOut);
						tokens_in = tokens_in.saturating_add(context);
						tokens_out = tokens_out.saturating_add(out);
						cache_read = cache_read.saturating_add(prop_u64(child, PropId::CacheRead));
						cache_write = cache_write.saturating_add(prop_u64(child, PropId::CacheWrite));
						cost_nano_usd =
							cost_nano_usd.saturating_add(prop_u64(child, PropId::CostNanoUsd));
						tokens_per_second = throughput(out, prop_u64(child, PropId::DurationMs));
					},
					_ => {},
				}
			}
		}
		Self {
			model,
			session,
			home,
			name,
			context,
			tokens_in,
			tokens_out,
			cache_read,
			cache_write,
			cost_nano_usd,
			tokens_per_second,
			turns,
		}
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

/// Output tokens per second of one receipt; `None` without a journaled
/// duration (pi `calculateTokensPerSecond`).
fn throughput(tokens_out: u64, duration_ms: u64) -> Option<f32> {
	(duration_ms > 0).then(|| (tokens_out as f64 * 1_000.0 / duration_ms as f64) as f32)
}

fn prop_u64(node: &omp_dom::Node, prop: PropId) -> u64 {
	match node.prop(&prop.into()) {
		Some(Value::Int(value)) => u64::try_from(*value).unwrap_or_default(),
		_ => 0,
	}
}

#[cfg(test)]
mod tests {
	use omp_dom::{Handle, Op, Txn};
	use omp_journal::data::TurnReceipt;
	use omp_session::{ComponentRegistry, Session};

	use super::*;

	fn session() -> Session {
		let directory = tempfile::tempdir().expect("temp directory");
		Session::create(directory.keep().join("status.oms"), ComponentRegistry::standard())
			.expect("session")
	}

	fn set(session: &mut Session, handle: Handle, prop: PropId, value: Value) {
		let cause = session.head().expect("head");
		session
			.patch(Txn {
				cause,
				label: None,
				ops: vec![Op::Set { h: handle, prop: prop.into(), value }],
			})
			.expect("patch");
	}

	#[test]
	fn from_dom_sums_receipts_and_reads_the_last_throughput() {
		let mut session = session();
		session.begin_turn().expect("turn");
		session.user("one", Vec::new()).expect("user");
		session
			.receipt(TurnReceipt {
				tokens_in:     1_000,
				tokens_out:    200,
				cost_nano_usd: 120_000_000,
				cache_read:    900,
				cache_write:   50,
				ttft_ms:       None,
				duration_ms:   Some(4_000),
			})
			.expect("receipt");

		session.begin_turn().expect("turn");
		session.user("two", Vec::new()).expect("user");
		session
			.receipt(TurnReceipt {
				tokens_in:     3_000,
				tokens_out:    400,
				cost_nano_usd: 5_000_000,
				cache_read:    2_500,
				cache_write:   0,
				ttft_ms:       None,
				duration_ms:   Some(2_000),
			})
			.expect("receipt");

		let status = StatusLine::from_dom(session.dom());
		assert_eq!(status.turns, 2);
		assert_eq!(status.context, 3_000, "context is the newest receipt's input");
		assert_eq!(status.tokens_in, 4_000);
		assert_eq!(status.tokens_out, 600);
		assert_eq!(status.cache_read, 3_400);
		assert_eq!(status.cache_write, 50);
		assert_eq!(status.cost_nano_usd, 125_000_000);
		assert_eq!(status.tokens_per_second, Some(200.0), "400 tokens over 2s");
	}

	#[test]
	fn from_dom_leaves_unjournaled_facts_empty() {
		let mut session = session();
		session.begin_turn().expect("turn");
		session.user("one", Vec::new()).expect("user");
		session
			.receipt(TurnReceipt::tokens(1_000, 200, 0))
			.expect("receipt");
		let status = StatusLine::from_dom(session.dom());
		assert_eq!(status.name, None);
		assert_eq!(status.cache_read, 0);
		assert_eq!(status.cache_write, 0);
		assert_eq!(status.cost_nano_usd, 0);
		assert_eq!(status.tokens_per_second, None, "no duration journaled");
	}

	#[test]
	fn from_dom_reads_the_session_title_from_meta() {
		let mut session = session();
		let meta = session.dom().meta();
		set(&mut session, meta, PropId::Name, Value::Str(Str::new_static("refactor auth")));
		let status = StatusLine::from_dom(session.dom());
		assert_eq!(status.name.as_deref(), Some("refactor auth"));
	}
}
