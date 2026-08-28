#![recursion_limit = "256"]

//! Headless coding-agent harness: session composition, execution modes,
//! orchestration, discovery, and settings for OMP.

pub mod advisor;
pub mod auth_backend;
pub mod auth_flow;
pub mod autolearn;
pub mod autoresearch;
pub mod bridges;
pub mod chat;
pub mod cleanse;
pub mod codex_redemption;
pub mod collab;
pub mod commit;
pub mod compress;
pub mod discovery;
pub mod export;
pub mod ext_updates;
pub mod goal;
pub mod headless;
pub mod hub;
pub mod memory;
pub mod model_controls;
pub mod modes;
pub mod plan;
pub mod power;
pub mod prompt_head;
pub mod prompt_input;
pub mod prompt_prep;
pub mod prompt_templates;
pub mod registry;
pub mod rulebook;
pub mod rules;
pub mod secrets;
pub mod security_review;
pub mod session_search;
pub mod session_state;
pub mod session_title;
pub mod settings;
pub mod share;
pub mod skills;
pub mod stats_api;
pub mod stats_dashboard;
pub mod stats_server;
pub mod subagent;
pub mod task;
pub mod telemetry_upload;
pub mod vibe;
pub mod workspace_roots;

use omp_settings::{DomainRegistration, LayerNormalizer, SettingsCatalog, SettingsContribution};

/// Driver-owned settings domains and persisted-layer normalization.
pub const SETTINGS_CONTRIBUTION: SettingsContribution = SettingsContribution {
	domains:     &[
		DomainRegistration::of::<settings::Settings>(),
		DomainRegistration::of::<settings::DisplaySettings>(),
		DomainRegistration::of::<settings::AppearanceSettings>(),
		DomainRegistration::of::<settings::TuiSettings>(),
		DomainRegistration::of::<settings::RootDisplaySettings>(),
		DomainRegistration::of::<settings::CompletionSettings>(),
		DomainRegistration::of::<settings::ErrorNotificationSettings>(),
		DomainRegistration::of::<settings::InteractionSettings>(),
		DomainRegistration::of::<settings::TtsrSettings>(),
		DomainRegistration::of::<settings::ShareSettings>(),
		DomainRegistration::of::<settings::TitleSettings>(),
		DomainRegistration::of::<settings::RecapSettings>(),
		DomainRegistration::of::<settings::LifecycleSettings>(),
		DomainRegistration::of::<rulebook::RulebookSettings>(),
		DomainRegistration::of::<discovery::foreign::ForeignContentSettings>(),
		DomainRegistration::of::<discovery::settings::DiscoverySettings>(),
		DomainRegistration::of::<discovery::skills::SkillDiscoverySettings>(),
		DomainRegistration::of::<power::PowerSettings>(),
		DomainRegistration::of::<prompt_prep::settings::PromptSettings>(),
		DomainRegistration::of::<subagent::settings::TaskSettings>(),
		DomainRegistration::of::<subagent::settings::IrcSettings>(),
	],
	normalizers: &[LayerNormalizer::new(subagent::settings::normalize_persisted_agent_overrides)],
};

/// Production settings composition for the complete headless driver runtime.
pub const SETTINGS_CATALOG: SettingsCatalog = SettingsCatalog::new(&[
	&omp_settings::SETTINGS_CONTRIBUTION,
	&omp_catalog::SETTINGS_CONTRIBUTION,
	&omp_inference::SETTINGS_CONTRIBUTION,
	&omp_envd::SETTINGS_CONTRIBUTION,
	&omp_tools::SETTINGS_CONTRIBUTION,
	&SETTINGS_CONTRIBUTION,
]);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn production_settings_catalog_has_the_complete_descriptor_multiset() {
		let names = SETTINGS_CATALOG
			.descriptors()
			.into_iter()
			.map(|descriptor| descriptor.name)
			.collect::<Vec<_>>();
		assert_eq!(names, [
			"acp",
			"app-core",
			"appearance",
			"async",
			"browser",
			"completion",
			"discovery",
			"display",
			"error",
			"fetch",
			"foreign",
			"images",
			"interaction",
			"irc",
			"lifecycle",
			"lsp",
			"lsp",
			"mcp",
			"model",
			"power",
			"prompt",
			"provider_runtime",
			"read",
			"recap",
			"retry",
			"root-display",
			"rules",
			"sampling",
			"sandbox",
			"share",
			"shell",
			"skills",
			"task",
			"title",
			"tools",
			"ttsr",
			"tui",
			"web_search",
		]);
	}
}
