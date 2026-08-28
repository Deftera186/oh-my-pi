//! Owner-local session prompt search over the private FTS projection.

use omp_agent::strip_system_wrapper;
use omp_core::Str;
use omp_storage::{
	index::{Error, PromptHit, SessionIndex},
	transcript::SessionId,
};

/// Application-facing prompt search result. Prompt bodies are returned only to
/// this owner-local surface and are never copied into analytics counters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPromptHit {
	/// Session containing the prompt.
	pub session:     SessionId,
	/// Stable physical journal event.
	pub event_index: u64,
	/// Readable private prompt text.
	pub prompt:      Str,
}

/// Runs bounded Unicode FTS with the index's substring fallback.
pub fn search_session_prompts(
	index: &SessionIndex,
	query: &str,
	limit: u32,
) -> Result<Vec<SessionPromptHit>, Error> {
	let query = query.trim();
	if query.is_empty() || limit == 0 {
		return Ok(Vec::new());
	}
	index
		.search_prompts(query, limit.min(100))
		.map(|hits| hits.into_iter().map(project_hit).collect())
}

fn project_hit(hit: PromptHit) -> SessionPromptHit {
	let stripped = strip_system_wrapper(hit.prompt.as_str()).map(Str::new);
	SessionPromptHit {
		session:     hit.session,
		event_index: hit.event_index,
		prompt:      stripped.unwrap_or(hit.prompt),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn projects_legacy_wrapped_prompt_as_readable_body() {
		let hit = project_hit(PromptHit {
			session:     SessionId(Str::new_static("session")),
			event_index: 7,
			prompt:      Str::new_static(
				"<system-reminder>\n2 todo items still open. Keep working.\n</system-reminder>",
			),
		});

		assert_eq!(hit.prompt.as_str(), "2 todo items still open. Keep working.");
	}
}
