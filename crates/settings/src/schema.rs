//! Type-owned settings reflection.

use std::{
	fmt,
	num::{ParseFloatError, ParseIntError},
	sync::Arc,
};

use serde::{Serialize, de::DeserializeOwned};
use toml::de;

/// A persistent settings layer that may own a field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingScope {
	/// User/profile-wide settings in the native user root.
	Global,
	/// Repository settings in the nearest native `.omp` root.
	Project,
	/// Process-local override that is never persisted.
	Runtime,
}

/// The value shape accepted by a reflected field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum SettingKind {
	/// Boolean value with CLI aliases such as `yes` and `off`.
	Boolean,
	/// Finite floating-point number.
	Number,
	/// Signed integer number.
	Integer,
	/// Unquoted string value.
	String,
	/// Duration string validated by the owning domain.
	Duration,
	/// Filesystem path represented as a string.
	Path,
	/// One of the named values.
	Enum(&'static [&'static str]),
	/// TOML array expression.
	Array,
	/// TOML inline-table expression.
	Table,
}

/// One selectable value shown by schema-driven clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingOption {
	/// Persisted value.
	pub value:       &'static str,
	/// Human-readable label.
	pub label:       &'static str,
	/// Optional explanatory text.
	pub description: Option<&'static str>,
}

/// One runtime-supplied selectable value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DynamicOption {
	/// Persisted value.
	pub value:       Arc<str>,
	/// Human-readable label.
	pub label:       Arc<str>,
	/// Optional explanatory text.
	pub description: Option<Arc<str>>,
}

/// A field's selectable-value source.
#[derive(Clone, Copy)]
pub enum OptionProvider {
	/// Compile-time options.
	Static(&'static [SettingOption]),
	/// Runtime options supplied by the behavior owner.
	Dynamic(fn() -> Arc<[DynamicOption]>),
}

impl fmt::Debug for OptionProvider {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Static(options) => formatter.debug_tuple("Static").field(options).finish(),
			Self::Dynamic(_) => formatter.write_str("Dynamic(..)"),
		}
	}
}

/// A visibility/enabled condition evaluated against another reflected field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Condition {
	/// Referenced dotted field path.
	pub field:  &'static str,
	/// TOML scalar spelling that enables this field.
	pub equals: &'static str,
}

/// Reflection for one field of an owning Rust domain type.
#[derive(Clone, Copy, Debug)]
pub struct FieldDescriptor {
	/// Stable dotted path in the merged TOML document.
	pub path:        &'static str,
	/// Display label.
	pub label:       &'static str,
	/// User-facing description.
	pub description: &'static str,
	/// Accepted value shape.
	pub kind:        SettingKind,
	/// Layers where mutation is allowed.
	pub scopes:      &'static [SettingScope],
	/// Stable ordering within the domain.
	pub order:       u16,
	/// Optional selectable values.
	pub options:     Option<OptionProvider>,
	/// Optional conditional availability.
	pub condition:   Option<Condition>,
	/// Whether output must be redacted whenever a value is present.
	pub secret:      bool,
}

impl FieldDescriptor {
	/// Parses a CLI value into the field's TOML representation.
	pub fn parse(&self, raw: &str) -> Result<toml::Value, ValidationError> {
		let value = match self.kind {
			SettingKind::Boolean => match raw.trim().to_ascii_lowercase().as_str() {
				"true" | "1" | "yes" | "on" => toml::Value::Boolean(true),
				"false" | "0" | "no" | "off" => toml::Value::Boolean(false),
				_ => return Err(ValidationError::InvalidBoolean { path: self.path }),
			},
			SettingKind::Number => {
				let number = raw
					.trim()
					.parse::<f64>()
					.map_err(|source| ValidationError::InvalidNumber { path: self.path, source })?;
				if !number.is_finite() {
					return Err(ValidationError::NonFiniteNumber { path: self.path });
				}
				toml::Value::Float(number)
			},
			SettingKind::Integer => {
				let number = raw
					.trim()
					.parse::<i64>()
					.map_err(|source| ValidationError::InvalidInteger { path: self.path, source })?;
				toml::Value::Integer(number)
			},
			SettingKind::String | SettingKind::Path => toml::Value::String(raw.trim().to_owned()),
			SettingKind::Duration => {
				let raw = raw.trim();
				let split = raw
					.find(|character: char| !character.is_ascii_digit())
					.unwrap_or(raw.len());
				let (number, unit) = raw.split_at(split);
				let valid_unit = matches!(unit, "ns" | "us" | "ms" | "s" | "m" | "h");
				if number.is_empty()
					|| number == "0"
					|| !number.bytes().all(|byte| byte.is_ascii_digit())
					|| !valid_unit
				{
					return Err(ValidationError::InvalidDuration { path: self.path });
				}
				toml::Value::String(raw.to_owned())
			},
			SettingKind::Enum(values) => {
				let raw = raw.trim();
				if !values.contains(&raw) {
					return Err(ValidationError::InvalidEnum { path: self.path });
				}
				toml::Value::String(raw.to_owned())
			},
			SettingKind::Array | SettingKind::Table => {
				let wrapped = format!("value = {raw}");
				let document: toml::Table = toml::from_str(&wrapped)
					.map_err(|source| ValidationError::InvalidToml { path: self.path, source })?;
				document
					.get("value")
					.cloned()
					.ok_or(ValidationError::MissingParsedValue { path: self.path })?
			},
		};
		match (self.kind, &value) {
			(SettingKind::Array, toml::Value::Array(_))
			| (SettingKind::Table, toml::Value::Table(_))
			| (SettingKind::Boolean, toml::Value::Boolean(_))
			| (SettingKind::Number, toml::Value::Float(_))
			| (SettingKind::Integer, toml::Value::Integer(_))
			| (
				SettingKind::String | SettingKind::Duration | SettingKind::Path,
				toml::Value::String(_),
			)
			| (SettingKind::Enum(_), toml::Value::String(_)) => Ok(value),
			_ => Err(ValidationError::WrongShape { path: self.path }),
		}
	}
}

/// Reflection contract implemented by each runtime-owned Rust settings type.
pub trait SettingsDomain:
	Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static
{
	/// Stable domain identifier used for revision subscriptions.
	const DOMAIN: &'static str;
	/// Optional root table containing this domain. App-wide aggregate domains
	/// use `None`; ordinary owners use `Some(Self::DOMAIN)`.
	const PREFIX: Option<&'static str> = Some(Self::DOMAIN);
	/// Fields owned by this type.
	const FIELDS: &'static [FieldDescriptor];

	/// Validates the fully layered typed projection.
	fn validate(&self) -> Result<(), ValidationError> {
		Ok(())
	}
}

/// Type-erased reflection for one linked settings domain.
#[derive(Clone, Copy)]
pub struct DomainDescriptor {
	/// Stable domain identifier.
	pub name:             &'static str,
	/// Optional root table decoded as the owning Rust type.
	pub prefix:           Option<&'static str>,
	/// Reflected fields.
	pub fields:           &'static [FieldDescriptor],
	/// Serializes the Rust type's `Default` value as a TOML document.
	pub default_document: fn() -> toml::Table,
	/// Validates a fully layered document through the Rust type.
	pub validate:         fn(&toml::Table, SettingsCatalog) -> Result<(), ValidationError>,
}

/// Registration emitted next to the runtime-owned Rust type.
#[derive(Debug)]
pub struct DomainRegistration {
	descriptor: fn() -> DomainDescriptor,
}

impl DomainRegistration {
	/// Builds a registration for `D`.
	pub const fn of<D: SettingsDomain>() -> Self {
		Self { descriptor: descriptor_of::<D> }
	}

	/// Returns the type-owned descriptor.
	pub fn descriptor(&self) -> DomainDescriptor {
		(self.descriptor)()
	}
}

/// One crate's settings contribution: domains plus layer normalizers,
/// assembled into a [`SettingsCatalog`] by the composition root.
#[derive(Debug)]
pub struct SettingsContribution {
	/// Domains owned by the contributing crate.
	pub domains:     &'static [DomainRegistration],
	/// Layer normalizers owned by the contributing crate.
	pub normalizers: &'static [crate::LayerNormalizer],
}

/// Explicit set of every settings contribution linked into one composition.
///
/// Copy (one wide pointer); stored by value inside `SettingsSnapshot`.
#[derive(Clone, Copy, Debug)]
pub struct SettingsCatalog {
	contributions: &'static [&'static SettingsContribution],
}

impl SettingsCatalog {
	/// Builds a catalog from explicit contributions; order = normalizer
	/// application order.
	pub const fn new(contributions: &'static [&'static SettingsContribution]) -> Self {
		Self { contributions }
	}

	/// All domain descriptors in deterministic name order.
	pub fn descriptors(&self) -> Vec<DomainDescriptor> {
		let mut domains = self
			.contributions
			.iter()
			.flat_map(|contribution| contribution.domains)
			.map(DomainRegistration::descriptor)
			.collect::<Vec<_>>();
		domains.sort_unstable_by_key(|domain| domain.name);
		domains
	}

	/// Applies every layer normalizer to one persisted document, in contribution
	/// order.
	pub fn normalize(&self, document: &mut toml::Table) {
		for normalizer in self
			.contributions
			.iter()
			.flat_map(|contribution| contribution.normalizers)
		{
			normalizer.apply(document);
		}
	}
}

fn descriptor_of<D: SettingsDomain>() -> DomainDescriptor {
	DomainDescriptor {
		name:             D::DOMAIN,
		prefix:           D::PREFIX,
		fields:           D::FIELDS,
		default_document: default_document_of::<D>,
		validate:         validate_document_as::<D>,
	}
}

fn default_document_of<D: SettingsDomain>() -> toml::Table {
	let Ok(toml::Value::Table(table)) = toml::Value::try_from(D::default()) else {
		return toml::Table::new();
	};
	if let Some(prefix) = D::PREFIX {
		let mut document = toml::Table::new();
		document.insert(prefix.to_owned(), toml::Value::Table(table));
		document
	} else {
		table
	}
}

fn validate_document_as<D: SettingsDomain>(
	document: &toml::Table,
	catalog: SettingsCatalog,
) -> Result<(), ValidationError> {
	let domain = projection_value::<D>(document, catalog)
		.try_into::<D>()
		.map_err(|source| ValidationError::DomainDecode { domain: D::DOMAIN, source })?;
	domain.validate()
}

pub(crate) fn projection_value<D: SettingsDomain>(
	document: &toml::Table,
	catalog: SettingsCatalog,
) -> toml::Value {
	let Some(prefix) = D::PREFIX else {
		return toml::Value::Table(document.clone());
	};
	let mut table = document
		.get(prefix)
		.and_then(toml::Value::as_table)
		.cloned()
		.unwrap_or_default();
	for domain in catalog
		.descriptors()
		.into_iter()
		.filter(|domain| domain.name == D::DOMAIN)
	{
		for field in domain.fields {
			if !D::FIELDS.iter().any(|owned| owned.path == field.path)
				&& let Some(relative) = field
					.path
					.strip_prefix(prefix)
					.and_then(|path| path.strip_prefix('.'))
			{
				remove_path(&mut table, relative);
			}
		}
	}
	toml::Value::Table(table)
}

fn remove_path(table: &mut toml::Table, path: &str) {
	let mut segments = path.splitn(2, '.');
	let Some(head) = segments.next() else {
		return;
	};
	if let Some(tail) = segments.next() {
		let remove_parent = table
			.get_mut(head)
			.and_then(toml::Value::as_table_mut)
			.is_some_and(|child| {
				remove_path(child, tail);
				child.is_empty()
			});
		if remove_parent {
			table.remove(head);
		}
	} else {
		table.remove(head);
	}
}

/// Schema parsing and typed-domain validation failures.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
	/// A boolean CLI spelling was not recognized.
	#[error("setting {path} requires a boolean")]
	InvalidBoolean {
		/// Reflected field path.
		path: &'static str,
	},
	/// A number failed to parse.
	#[error("setting {path} requires a number")]
	InvalidNumber {
		/// Reflected field path.
		path:   &'static str,
		/// Numeric parser failure.
		#[source]
		source: ParseFloatError,
	},
	/// An integer failed to parse.
	#[error("setting {path} requires an integer")]
	InvalidInteger {
		/// Reflected field path.
		path:   &'static str,
		/// Integer parser failure.
		#[source]
		source: ParseIntError,
	},
	/// A number was NaN or infinite.
	#[error("setting {path} requires a finite number")]
	NonFiniteNumber {
		/// Reflected field path.
		path: &'static str,
	},
	/// A duration lacked a positive integer or explicit unit.
	#[error("setting {path} requires a positive duration with an explicit ns/us/ms/s/m/h unit")]
	InvalidDuration {
		/// Reflected field path.
		path: &'static str,
	},
	/// An enum value was outside the declared vocabulary.
	#[error("setting {path} is not one of its declared values")]
	InvalidEnum {
		/// Reflected field path.
		path: &'static str,
	},
	/// A composite CLI value was malformed TOML.
	#[error("setting {path} has malformed TOML")]
	InvalidToml {
		/// Reflected field path.
		path:   &'static str,
		/// TOML parser failure.
		#[source]
		source: de::Error,
	},
	/// A parsed wrapper unexpectedly omitted its value.
	#[error("setting {path} did not produce a value")]
	MissingParsedValue {
		/// Reflected field path.
		path: &'static str,
	},
	/// A parsed value did not have the declared shape.
	#[error("setting {path} does not have the declared shape")]
	WrongShape {
		/// Reflected field path.
		path: &'static str,
	},
	/// The layered document could not decode as its owning Rust type.
	#[error("settings domain {domain} rejected the layered document")]
	DomainDecode {
		/// Stable domain identifier.
		domain: &'static str,
		/// Typed TOML decode failure.
		#[source]
		source: de::Error,
	},
	/// A domain-specific invariant failed.
	#[error("settings domain invariant failed for {domain}")]
	DomainInvariant {
		/// Stable domain identifier.
		domain: &'static str,
	},
}
