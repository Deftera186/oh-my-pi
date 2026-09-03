//! Layered settings projection owned by the discovery runtime.

use omp_con::{Kv, Value};
use omp_core::Str;
use serde::{Deserialize, Serialize};

use super::manifest::CapabilityKind;
use crate::settings::{
	FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Explicit user-configured claim over a bundled capability name.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct BuiltinShadow {
	/// Capability family containing the claimed name.
	pub kind: CapabilityKind,
	/// Stable capability key claimed by user configuration.
	pub key:  Str,
}

omp_con::var! {
	/// Discovery providers disabled across every capability.
	pub static SV_DISCOVERY_DISABLED_PROVIDERS = sv_discovery_disabled_providers: Vec<Str> {
		default: Vec::new(),
		flags: archive | inherit,
	};
	/// Individual discovery sources disabled without disabling their provider.
	pub static SV_DISCOVERY_DISABLED_SOURCES = sv_discovery_disabled_sources: Vec<Str> {
		default: Vec::new(),
		flags: archive | inherit,
	};
	/// Explicit user claims over bundled capability names.
	pub static SV_DISCOVERY_BUILTIN_SHADOWS = sv_discovery_builtin_shadows: Vec<Kv> {
		default: Vec::new(),
		flags: archive | inherit,
	};
}

/// Layered discovery provider, source, and built-in-shadow policy.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverySettings {
	/// Providers disabled across every capability they contribute.
	pub disabled_providers: Vec<Str>,
	/// Individual provider-defined source IDs disabled without disabling their
	/// entire provider.
	pub disabled_sources:   Vec<Str>,
	/// Explicit user claims over bundled names. Merely loading first never
	/// creates this precedence.
	pub builtin_shadows:    Vec<BuiltinShadow>,
}

impl DiscoverySettings {
	/// Resolves discovery policy from the process console context.
	#[must_use]
	pub fn from_con(ctx: &omp_con::Ctx) -> Self {
		let builtin_shadows = SV_DISCOVERY_BUILTIN_SHADOWS
			.get(ctx)
			.into_iter()
			.filter_map(|entry| {
				let kind = match entry.get("kind")? {
					Value::Str(value) | Value::Enum(value) => value.parse().ok()?,
					_ => return None,
				};
				let key = match entry.get("key")? {
					Value::Str(value) | Value::Enum(value) => value.clone(),
					_ => return None,
				};
				Some(BuiltinShadow { kind, key })
			})
			.collect();
		Self {
			disabled_providers: SV_DISCOVERY_DISABLED_PROVIDERS.get(ctx),
			disabled_sources: SV_DISCOVERY_DISABLED_SOURCES.get(ctx),
			builtin_shadows,
		}
	}

	/// Returns whether the provider is enabled by the immutable projection.
	pub fn provider_enabled(&self, provider_id: &str) -> bool {
		!self
			.disabled_providers
			.iter()
			.any(|disabled| disabled == provider_id)
	}

	/// Returns whether the provider-defined source is enabled.
	pub fn source_enabled(&self, source_id: &str) -> bool {
		!self
			.disabled_sources
			.iter()
			.any(|disabled| disabled == source_id)
	}

	/// Returns whether user configuration explicitly claims this bundled name.
	pub fn shadows_builtin(&self, kind: CapabilityKind, key: &str) -> bool {
		self
			.builtin_shadows
			.iter()
			.any(|claim| claim.kind == kind && claim.key == key)
	}
}

impl SettingsDomain for DiscoverySettings {
	const DOMAIN: &'static str = "discovery";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "discovery.disabled_providers",
			label:       "Disabled discovery providers",
			description: "Provider IDs excluded from every capability load.",
			kind:        SettingKind::Array,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "discovery.disabled_sources",
			label:       "Disabled discovery sources",
			description: "Provider-defined source IDs disabled while retaining key shadow claims.",
			kind:        SettingKind::Array,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "discovery.builtin_shadows",
			label:       "Built-in capability shadows",
			description: "Explicit typed user claims over bundled capability names.",
			kind:        SettingKind::Array,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let valid = self.disabled_providers.iter().all(|id| !id.is_empty())
			&& self.disabled_sources.iter().all(|id| !id.is_empty())
			&& self
				.builtin_shadows
				.iter()
				.all(|claim| !claim.key.is_empty());
		if valid {
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn discovery_settings_projection_is_typed() {
		let ctx = omp_con::Ctx::new();
		SV_DISCOVERY_DISABLED_PROVIDERS
			.set(&ctx, vec!["foreign-content".into()])
			.expect("provider setting");
		SV_DISCOVERY_DISABLED_SOURCES
			.set(&ctx, vec!["project-rules".into()])
			.expect("source setting");
		SV_DISCOVERY_BUILTIN_SHADOWS
			.set(&ctx, vec![Kv(vec![
				("kind".into(), Value::Str("skills".into())),
				("key".into(), Value::Str("rust".into())),
			])])
			.expect("shadow setting");
		let settings = DiscoverySettings::from_con(&ctx);
		assert!(!settings.provider_enabled("foreign-content"));
		assert!(!settings.source_enabled("project-rules"));
		assert!(settings.shadows_builtin(CapabilityKind::Skills, "rust"));
	}

	#[test]
	fn discovery_settings_reject_empty_authority_keys() {
		let settings = DiscoverySettings {
			disabled_providers: vec![Str::default()],
			..DiscoverySettings::default()
		};
		assert!(settings.validate().is_err());
	}
}
