//! Typed settings owned by immutable prompt preparation.

use std::{env, path::Path};

use omp_agent::{Personality, PromptSettingsInput};
use omp_core::Str;
use omp_settings::{
	FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope, SettingsDomain,
};
use serde::{Deserialize, Serialize};

use crate::{prompt_input, prompt_input::PromptInputError};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const RUNTIME: &[SettingScope] = &[SettingScope::Runtime];
const PERSONALITIES: &[&str] = &["default", "friendly", "pragmatic", "none"];
const PERSONALITY_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "default",
		label:       "Default",
		description: Some("Terse, evidence-first engineering guidance."),
	},
	SettingOption {
		value:       "friendly",
		label:       "Friendly",
		description: Some("Warm, encouraging collaboration guidance."),
	},
	SettingOption {
		value:       "pragmatic",
		label:       "Pragmatic",
		description: Some("Direct, clarity- and rigor-focused guidance."),
	},
	SettingOption {
		value:       "none",
		label:       "None",
		description: Some("Omit personality guidance."),
	},
];

/// Complete typed settings projection consumed by prompt preparation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PromptSettings {
	/// Communication style rendered into the prompt.
	pub personality:             Personality,
	/// Resolved user-level `PERSONALITY.md` override.
	pub personality_override:    Option<Str>,
	/// Surface the active provider-qualified model identifier.
	pub include_model_in_prompt: bool,
	/// Include bounded Environment workstation facts.
	pub include_workstation:     bool,
	/// Include the workspace directory tree.
	pub include_workspace_tree:  bool,
	/// Permit Mermaid diagram guidance.
	pub render_mermaid:          bool,
	/// Include enabled skills.
	pub skills_enabled:          bool,
	/// Inline or file-valued custom system prompt input.
	pub custom_prompt:           Option<Str>,
	/// Inline or file-valued prompt appended after ordinary guidance.
	pub append_prompt:           Option<Str>,
	/// Explicit developer/test empty-provider bypass.
	pub null_prompt:             bool,
}

impl Default for PromptSettings {
	fn default() -> Self {
		Self {
			personality:             Personality::Default,
			personality_override:    None,
			include_model_in_prompt: true,
			include_workstation:     true,
			include_workspace_tree:  false,
			render_mermaid:          true,
			skills_enabled:          true,
			custom_prompt:           None,
			append_prompt:           None,
			null_prompt:             false,
		}
	}
}
/// Invocation-scoped prompt overrides supplied by a composition layer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PromptOverrides {
	/// Optional personality override.
	pub personality:             Option<Personality>,
	/// Optional model-label inclusion override.
	pub include_model_in_prompt: Option<bool>,
	/// Optional workstation-facts inclusion override.
	pub include_workstation:     Option<bool>,
	/// Optional workspace-tree inclusion override.
	pub include_workspace_tree:  Option<bool>,
	/// Optional Mermaid rendering override.
	pub render_mermaid:          Option<bool>,
	/// Optional skill-prompt inclusion override.
	pub skills_enabled:          Option<bool>,
	/// Optional complete custom system prompt.
	pub custom_prompt:           Option<Str>,
	/// Optional text appended to the system prompt.
	pub append_prompt:           Option<Str>,
	/// Whether to suppress the ordinary system prompt.
	pub null_prompt:             bool,
}

impl PromptSettings {
	/// Applies invocation overrides and the explicit `NULL_PROMPT=true`
	/// developer/test escape hatch before freezing a turn snapshot.
	pub fn with_overrides(mut self, overrides: &PromptOverrides) -> Self {
		if let Some(value) = overrides.personality {
			self.personality = value;
		}
		if let Some(value) = overrides.include_model_in_prompt {
			self.include_model_in_prompt = value;
		}
		if let Some(value) = overrides.include_workstation {
			self.include_workstation = value;
		}
		if let Some(value) = overrides.include_workspace_tree {
			self.include_workspace_tree = value;
		}
		if let Some(value) = overrides.render_mermaid {
			self.render_mermaid = value;
		}
		if let Some(value) = overrides.skills_enabled {
			self.skills_enabled = value;
		}
		if let Some(value) = &overrides.custom_prompt {
			self.custom_prompt = Some(value.clone());
		}
		if let Some(value) = &overrides.append_prompt {
			self.append_prompt = Some(value.clone());
		}
		self.null_prompt = self.null_prompt
			|| overrides.null_prompt
			|| env::var("NULL_PROMPT").is_ok_and(|value| value.eq_ignore_ascii_case("true"));
		self
	}

	/// Resolves file-or-inline customization and project-over-user `SYSTEM.md`
	/// discovery before the immutable settings projection is published.
	pub fn resolve_inputs(mut self, cwd: &Path, home: &Path) -> Result<Self, PromptInputError> {
		let (custom, append) = prompt_input::resolve_system_inputs(
			cwd,
			home,
			self.custom_prompt.as_deref(),
			self.append_prompt.as_deref(),
		)?;
		self.custom_prompt = custom;
		self.append_prompt = append;
		if self.personality != Personality::None {
			self.personality_override =
				prompt_input::discover_user_prompt_file(cwd, home, "PERSONALITY.md")?;
		} else {
			self.personality_override = None;
		}
		Ok(self)
	}
}

impl From<PromptSettings> for PromptSettingsInput {
	fn from(settings: PromptSettings) -> Self {
		Self {
			personality:            settings.personality,
			personality_override:   settings.personality_override,
			include_model:          settings.include_model_in_prompt,
			include_workstation:    settings.include_workstation,
			include_workspace_tree: settings.include_workspace_tree,
			render_mermaid:         settings.render_mermaid,
			include_skills:         settings.skills_enabled,
			tool_inventory:         Default::default(),
			intent_field:           None,
			secrets_enabled:        false,
			custom_prompt:          settings.custom_prompt,
			append_prompt:          settings.append_prompt,
			null_prompt:            settings.null_prompt,
		}
	}
}

impl SettingsDomain for PromptSettings {
	const DOMAIN: &'static str = "prompt";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "prompt.personality",
			label:       "Personality",
			description: "Communication style rendered into the system prompt.",
			kind:        SettingKind::Enum(PERSONALITIES),
			scopes:      PERSISTED,
			order:       10,
			options:     Some(OptionProvider::Static(PERSONALITY_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.includeModelInPrompt",
			label:       "Include Model",
			description: "Surface the active model identifier in workstation facts.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.includeWorkstation",
			label:       "Include Workstation",
			description: "Include bounded Environment-owned workstation facts.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.includeWorkspaceTree",
			label:       "Include Workspace Tree",
			description: "Render a bounded workspace tree in project context.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.renderMermaid",
			label:       "Render Mermaid",
			description: "Permit Mermaid diagram rendering guidance.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.skillsEnabled",
			label:       "Skills",
			description: "Include enabled skills in prompt preparation.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       60,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.customPrompt",
			label:       "Custom Prompt",
			description: "Inline or file-valued replacement prompt input.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       70,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.appendPrompt",
			label:       "Append Prompt",
			description: "Inline or file-valued prompt appended after ordinary guidance.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       80,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "prompt.nullPrompt",
			label:       "Null Prompt",
			description: "Developer/test empty-provider bypass.",
			kind:        SettingKind::Boolean,
			scopes:      RUNTIME,
			order:       90,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];
}

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsCatalog, SettingsSnapshot};

	use super::*;

	const CATALOG: SettingsCatalog =
		SettingsCatalog::new(&[&omp_settings::SETTINGS_CONTRIBUTION, &crate::SETTINGS_CONTRIBUTION]);

	#[test]
	fn defaults_match_pi_prompt_behavior() {
		let defaults = PromptSettings::default();
		assert_eq!(defaults.personality, Personality::Default);
		assert!(defaults.include_model_in_prompt);
		assert!(defaults.include_workstation);
		assert!(!defaults.include_workspace_tree);
		assert!(defaults.render_mermaid);
		assert!(defaults.skills_enabled);
		assert!(defaults.custom_prompt.is_none());
		assert!(defaults.append_prompt.is_none());
		assert!(!defaults.null_prompt);

		let snapshot =
			SettingsSnapshot::isolated(defaults.clone(), CATALOG).expect("isolated snapshot");
		let projection = snapshot
			.project::<PromptSettings>()
			.expect("prompt projection");
		assert_eq!(projection.get(), &defaults);
	}

	#[test]
	fn cli_overrides_freeze_without_changing_pi_defaults() {
		let cli = PromptOverrides {
			personality: Some(Personality::Friendly),
			include_workspace_tree: Some(true),
			render_mermaid: Some(false),
			custom_prompt: Some(Str::from("custom")),
			append_prompt: Some(Str::from("append")),
			..Default::default()
		};
		let settings = PromptSettings::default().with_overrides(&cli);
		assert_eq!(settings.personality, Personality::Friendly);
		assert!(settings.include_workspace_tree);
		assert!(!settings.render_mermaid);
		assert_eq!(settings.custom_prompt.as_deref(), Some("custom"));
		assert_eq!(settings.append_prompt.as_deref(), Some("append"));
		assert!(PromptSettings::default().include_model_in_prompt);
	}
}
