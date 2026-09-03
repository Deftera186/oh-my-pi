//! Typed web-search routing and provider convars.

use omp_con::Ctx;
use omp_core::Str;
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantNames};
use url::Url;

/// Antigravity endpoint selection.
#[derive(Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq, VariantNames)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum AntigravityMode {
	/// Select the endpoint automatically.
	#[default]
	Auto,
	/// Use the production endpoint.
	Production,
	/// Use the sandbox endpoint.
	Sandbox,
}

omp_con::con_enum!(AntigravityMode);

/// Search provider routing and endpoint policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WebSearchSettings {
	/// Automatic provider preference order.
	pub order:                Vec<Str>,
	/// Providers omitted from automatic search.
	pub exclusions:           Vec<Str>,
	/// Per-provider attempt timeout in seconds.
	pub timeout_seconds:      u32,
	/// Optional self-hosted SearXNG endpoint.
	pub searxng_endpoint:     Option<Str>,
	/// Optional Gemini grounding model.
	pub gemini_model:         Option<Str>,
	/// Antigravity endpoint selection (`auto`, `production`, or `sandbox`).
	pub antigravity_mode:     Str,
	/// Whether Perplexity uses its Responses endpoint.
	pub perplexity_responses: bool,
}

/// Resolves a user-facing search engine name to its catalog provider key.
pub fn catalog_provider_name(name: &str) -> &str {
	match name {
		"google" => "google-search",
		_ => name,
	}
}

impl Default for WebSearchSettings {
	fn default() -> Self {
		Self {
			order:                default_order(),
			exclusions:           Vec::new(),
			timeout_seconds:      60,
			searxng_endpoint:     None,
			gemini_model:         None,
			antigravity_mode:     Str::new_static("auto"),
			perplexity_responses: false,
		}
	}
}

impl WebSearchSettings {
	/// Projects web-search routing from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		let searxng_endpoint = AI_SEARCH_SEARXNG_ENDPOINT.get(ctx);
		let gemini_model = AI_SEARCH_GEMINI_MODEL.get(ctx);
		let mode: &'static str = AI_SEARCH_ANTIGRAVITY_MODE.get(ctx).into();
		Self {
			order:                AI_SEARCH_ORDER.get(ctx),
			exclusions:           AI_SEARCH_EXCLUSIONS.get(ctx),
			timeout_seconds:      AI_SEARCH_TIMEOUT_SECONDS.get(ctx),
			searxng_endpoint:     (!searxng_endpoint.is_empty()).then_some(searxng_endpoint),
			gemini_model:         (!gemini_model.is_empty()).then_some(gemini_model),
			antigravity_mode:     Str::new_static(mode),
			perplexity_responses: AI_SEARCH_PERPLEXITY_RESPONSES.get(ctx),
		}
	}

	/// Reports whether all cross-variable search policy invariants hold.
	#[must_use]
	pub fn validate(&self) -> bool {
		let unique = |values: &[Str]| {
			values.iter().all(|value| !value.is_empty())
				&& values
					.iter()
					.enumerate()
					.all(|(index, value)| values[..index].iter().all(|prior| prior != value))
		};
		let endpoint_valid = self.searxng_endpoint.as_deref().is_none_or(|endpoint| {
			Url::parse(endpoint).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
		});
		unique(&self.order)
			&& unique(&self.exclusions)
			&& (1..=300).contains(&self.timeout_seconds)
			&& matches!(self.antigravity_mode.as_str(), "auto" | "production" | "sandbox")
			&& endpoint_valid
	}
}

fn default_order() -> Vec<Str> {
	[
		"perplexity",
		"gemini",
		"anthropic",
		"codex",
		"xai",
		"zai",
		"exa",
		"tinyfish",
		"jina",
		"kagi",
		"tavily",
		"firecrawl",
		"brave",
		"kimi",
		"parallel",
		"synthetic",
		"searxng",
		"startpage",
		"duckduckgo",
		"ecosia",
		"google",
		"mojeek",
		"public",
	]
	.into_iter()
	.map(Str::new_static)
	.collect()
}

fn invalid(reason: &'static str) -> Result<(), Str> {
	Err(Str::new_static(reason))
}

fn validate_unique(_: &Ctx, values: &Vec<Str>) -> Result<(), Str> {
	if values.iter().all(|value| !value.is_empty())
		&& values
			.iter()
			.enumerate()
			.all(|(index, value)| values[..index].iter().all(|prior| prior != value))
	{
		Ok(())
	} else {
		invalid("search provider lists require non-empty unique values")
	}
}

fn validate_searxng_endpoint(_: &Ctx, endpoint: &Str) -> Result<(), Str> {
	if endpoint.is_empty()
		|| Url::parse(endpoint).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
	{
		Ok(())
	} else {
		invalid("SearXNG endpoint must be empty or an HTTPS URL with a host")
	}
}

omp_con::var! {
	/// Automatic web-search provider order.
	pub static AI_SEARCH_ORDER = ai_search_order: Vec<Str> {
		default: default_order(),
		validate: validate_unique,
		flags: archive | inherit,
	};
	/// Providers excluded from automatic web search.
	pub static AI_SEARCH_EXCLUSIONS = ai_search_exclusions: Vec<Str> {
		default: Vec::new(),
		validate: validate_unique,
		flags: archive | inherit,
	};
	/// Per-provider search timeout in seconds.
	pub static AI_SEARCH_TIMEOUT_SECONDS = ai_search_timeout_seconds: u32 {
		default: 60,
		min: 1,
		max: 300,
		flags: archive | inherit,
	};
	/// Optional HTTPS SearXNG endpoint; empty disables it.
	pub static AI_SEARCH_SEARXNG_ENDPOINT = ai_search_searxng_endpoint: Str {
		default: Str::new_static(""),
		validate: validate_searxng_endpoint,
		flags: archive | inherit,
	};
	/// Optional Gemini grounding model; empty preserves provider default.
	pub static AI_SEARCH_GEMINI_MODEL = ai_search_gemini_model: Str {
		default: Str::new_static(""),
		flags: archive | inherit,
	};
	/// Antigravity endpoint mode.
	pub static AI_SEARCH_ANTIGRAVITY_MODE = ai_search_antigravity_mode: AntigravityMode {
		default: AntigravityMode::Auto,
		flags: archive | inherit,
	};
	/// Use the Perplexity Responses endpoint.
	pub static AI_SEARCH_PERPLEXITY_RESPONSES = ai_search_perplexity_responses: bool {
		default: false,
		flags: archive | inherit,
	};
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn projection_reads_ctx_and_rejects_invalid_timeout() {
		let ctx = Ctx::new();
		AI_SEARCH_TIMEOUT_SECONDS
			.set(&ctx, 42)
			.expect("set search timeout");
		let projected = WebSearchSettings::from_con(&ctx);
		assert_eq!(projected.timeout_seconds, 42);
		assert!(projected.validate());
		assert!(!WebSearchSettings { timeout_seconds: 301, ..Default::default() }.validate());
	}
}
