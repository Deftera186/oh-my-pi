//! Typed browser-tool availability and surface-mode settings.

use serde::{Deserialize, Serialize};

use crate::{DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Layered browser-tool availability and presentation mode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserSettings {
	/// Enables the browser automation tool.
	pub enabled:  bool,
	/// Uses an offscreen frame surface instead of an engine-owned window.
	pub headless: bool,
}

impl Default for BrowserSettings {
	fn default() -> Self {
		Self { enabled: true, headless: true }
	}
}

impl SettingsDomain for BrowserSettings {
	const DOMAIN: &'static str = "browser";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "browser.enabled",
			label:       "Browser",
			description: "Enable the browser tool for scripted web automation.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "browser.headless",
			label:       "Headless Browser",
			description: "Run browser automation offscreen instead of showing a browser window.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];
}

inventory::submit! {
	DomainRegistration::of::<BrowserSettings>()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{SettingsSnapshot, registered_domains};

	#[test]
	fn defaults_preserve_current_enabled_headless_surface() {
		let settings = BrowserSettings::default();
		assert!(settings.enabled);
		assert!(settings.headless);
	}

	#[test]
	fn snapshot_projects_browser_table() {
		let snapshot =
			SettingsSnapshot::isolated(BrowserSettings { enabled: false, headless: false })
				.expect("browser settings snapshot");
		let projected = snapshot
			.project::<BrowserSettings>()
			.expect("browser settings projection");
		assert_eq!(projected.get(), &BrowserSettings { enabled: false, headless: false });
	}

	#[test]
	fn schema_registers_browser_fields() {
		let domain = registered_domains()
			.into_iter()
			.find(|domain| domain.name == "browser")
			.expect("browser domain");
		assert_eq!(
			domain
				.fields
				.iter()
				.map(|field| field.path)
				.collect::<Vec<_>>(),
			vec!["browser.enabled", "browser.headless"]
		);
	}
}
