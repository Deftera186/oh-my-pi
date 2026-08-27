//! Metric parsing, path safety, and experiment statistics.

use std::collections::{BTreeMap, BTreeSet};

use omp_core::{Str, sf};

use super::types::{Asi, ExperimentStatus, MetricDirection, Metrics};

const DENIED_KEYS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// One settled measurement used by baseline and confidence math.
#[derive(Clone, Debug, PartialEq)]
pub struct Measurement {
	/// Primary metric value.
	pub metric:  f64,
	/// Run status.
	pub status:  ExperimentStatus,
	/// Segment containing the measurement.
	pub segment: u32,
	/// Whether this run is excluded from control-state math.
	pub flagged: bool,
}

/// Parses finite `METRIC name=value` lines, rejecting prototype-pollution keys.
pub fn parse_metric_lines(output: &str) -> Metrics {
	let mut metrics = BTreeMap::new();
	for line in output.lines() {
		let Some(body) = line.strip_prefix("METRIC ") else {
			continue;
		};
		let Some((name, raw)) = body.trim().split_once('=') else {
			continue;
		};
		if !valid_key(name, true) {
			continue;
		}
		let Ok(value) = raw.parse::<f64>() else {
			continue;
		};
		if value.is_finite() {
			metrics.insert(Str::from(name), value);
		}
	}
	metrics
}

/// Parses and recursively sanitizes `ASI key=value` lines.
pub fn parse_asi_lines(output: &str) -> Asi {
	let mut asi = Asi::new();
	for line in output.lines() {
		let Some(body) = line.strip_prefix("ASI ") else {
			continue;
		};
		let Some((key, raw)) = body.trim().split_once('=') else {
			continue;
		};
		if !valid_key(key, true) {
			continue;
		}
		let parsed = serde_json::from_str(raw.trim())
			.unwrap_or_else(|_| serde_json::Value::String(raw.trim().to_owned()));
		if let Some(value) = sanitize_json(parsed) {
			asi.insert(key.to_owned(), value);
		}
	}
	asi
}

fn valid_key(key: &str, metric: bool) -> bool {
	!key.is_empty()
		&& !DENIED_KEYS.contains(&key)
		&& key.chars().all(|character| {
			character.is_alphanumeric()
				|| matches!(character, '_' | '-' | '.' | 'µ')
				|| (!metric && character == '/')
		})
}

fn sanitize_json(value: serde_json::Value) -> Option<serde_json::Value> {
	match value {
		serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
			Some(value)
		},
		serde_json::Value::Number(number) if number.as_f64().is_some_and(f64::is_finite) => {
			Some(serde_json::Value::Number(number))
		},
		serde_json::Value::Number(_) => None,
		serde_json::Value::Array(values) => {
			Some(serde_json::Value::Array(values.into_iter().filter_map(sanitize_json).collect()))
		},
		serde_json::Value::Object(values) => {
			let values = values
				.into_iter()
				.filter(|(key, _)| !DENIED_KEYS.contains(&key.as_str()))
				.filter_map(|(key, value)| sanitize_json(value).map(|value| (key, value)))
				.collect();
			Some(serde_json::Value::Object(values))
		},
	}
}

/// Infers the conventional unit suffix from a metric key.
pub fn infer_metric_unit(name: &str) -> &'static str {
	if name.ends_with("µs") || name.ends_with("_µs") {
		"µs"
	} else if name.ends_with("ms") || name.ends_with("_ms") {
		"ms"
	} else if name.ends_with("_s") || name.ends_with("_sec") || name.ends_with("_secs") {
		"s"
	} else if name.ends_with("_kb") || name.ends_with("kb") {
		"kb"
	} else if name.ends_with("_mb") || name.ends_with("mb") {
		"mb"
	} else {
		""
	}
}

/// Returns whether `current` improves on `best`.
pub const fn is_better(current: f64, best: f64, direction: MetricDirection) -> bool {
	match direction {
		MetricDirection::Lower => current < best,
		MetricDirection::Higher => current > best,
	}
}

/// Computes improvement divided by median absolute deviation.
pub fn mad_confidence(
	measurements: &[Measurement],
	segment: u32,
	direction: MetricDirection,
) -> Option<f64> {
	let current: Vec<_> = measurements
		.iter()
		.filter(|entry| entry.segment == segment && !entry.flagged && entry.metric > 0.0)
		.collect();
	if current.len() < 3 {
		return None;
	}
	let values: Vec<_> = current.iter().map(|entry| entry.metric).collect();
	let median_value = median(&values);
	let deviations: Vec<_> = values
		.iter()
		.map(|value| (value - median_value).abs())
		.collect();
	let mad = median(&deviations);
	if mad == 0.0 {
		return None;
	}
	let baseline = current
		.iter()
		.find(|entry| entry.status == ExperimentStatus::Keep)
		.map(|entry| entry.metric)?;
	let best = current
		.iter()
		.filter(|entry| entry.status == ExperimentStatus::Keep)
		.map(|entry| entry.metric)
		.reduce(|best, value| {
			if is_better(value, best, direction) {
				value
			} else {
				best
			}
		})?;
	(best != baseline).then_some((best - baseline).abs() / mad)
}

fn median(values: &[f64]) -> f64 {
	if values.is_empty() {
		return 0.0;
	}
	let mut sorted = values.to_vec();
	sorted.sort_by(f64::total_cmp);
	let midpoint = sorted.len() / 2;
	if sorted.len().is_multiple_of(2) {
		(sorted[midpoint - 1] + sorted[midpoint]) / 2.0
	} else {
		sorted[midpoint]
	}
}

/// Normalizes one repository-relative path specification.
pub fn normalize_path(value: &str) -> Str {
	let value = value.trim().replace('\\', "/");
	let value = value.trim_start_matches("./").trim_end_matches('/');
	if value.is_empty() || value == "." {
		sf!(".")
	} else {
		Str::from(value)
	}
}

/// Returns whether a path is equal to or below one normalized path prefix.
pub fn path_matches(path: &str, prefix: &str) -> bool {
	let path = normalize_path(path);
	let prefix = normalize_path(prefix);
	prefix == "."
		|| path == prefix
		|| path
			.strip_prefix(prefix.as_str())
			.is_some_and(|rest| rest.starts_with('/'))
}

/// Computes scope violations for changed paths.
pub fn scope_deviations(
	paths: impl IntoIterator<Item = Str>,
	scope: &[Str],
	off_limits: &[Str],
) -> Vec<Str> {
	let mut deviations = BTreeSet::new();
	for path in paths {
		if off_limits
			.iter()
			.any(|prefix| path_matches(path.as_str(), prefix.as_str()))
			|| !scope.is_empty()
				&& !scope
					.iter()
					.any(|prefix| path_matches(path.as_str(), prefix.as_str()))
		{
			deviations.insert(path);
		}
	}
	deviations.into_iter().collect()
}

/// Makes a bounded, dated branch candidate in the repository namespace.
pub fn branch_candidate(goal: Option<&str>, date: &str, suffix: Option<u32>) -> Str {
	let mut slug = String::new();
	for character in goal.unwrap_or("experiment").chars() {
		if character.is_ascii_alphanumeric() {
			slug.push(character.to_ascii_lowercase());
		} else if !slug.ends_with('-') {
			slug.push('-');
		}
	}
	let slug = slug.trim_matches('-');
	let slug = if slug.is_empty() { "experiment" } else { slug };
	let suffix_len = suffix.map_or(0, |value| value.to_string().len() + 1);
	let max_slug = 48usize.saturating_sub("autoresearch/".len() + date.len() + suffix_len + 1);
	let slug = &slug[..slug.floor_char_boundary(slug.len().min(max_slug))];
	match suffix {
		Some(value) => Str::from(format!("autoresearch/{slug}-{date}-{value}")),
		None => Str::from(format!("autoresearch/{slug}-{date}")),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn metric_and_asi_parsers_reject_pollution_keys() {
		let output =
			"METRIC latency_ms=12.5\nMETRIC __proto__=9\nASI ok={\"prototype\":1,\"safe\":true}\n";
		assert_eq!(parse_metric_lines(output).get("latency_ms"), Some(&12.5));
		assert!(!parse_metric_lines(output).contains_key("__proto__"));
		assert_eq!(parse_asi_lines(output)["ok"]["safe"], true);
		assert!(parse_asi_lines(output)["ok"].get("prototype").is_none());
	}

	#[test]
	fn confidence_uses_segment_mad_and_ignores_flags() {
		let values = [10.0, 9.0, 8.0, 100.0]
			.into_iter()
			.enumerate()
			.map(|(index, metric)| Measurement {
				metric,
				status: ExperimentStatus::Keep,
				segment: 1,
				flagged: index == 3,
			})
			.collect::<Vec<_>>();
		assert_eq!(mad_confidence(&values, 1, MetricDirection::Lower), Some(2.0));
	}
}
