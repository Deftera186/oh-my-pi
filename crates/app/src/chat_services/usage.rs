//! `/usage` feed: durable quota windows plus one fresh refresh per stored
//! account (the same sources as `omp usage`), collapsed into one card per
//! provider for the dashboard and rendered as the classic per-account
//! detail report.

use std::{
	collections::BTreeMap,
	fmt::Write as _,
	fs,
	path::Path,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use omp_catalog::{ProviderId, snapshot::Catalog};
use omp_chat::overlays::services::{
	Pending, ServiceError, ServiceResult, UsageAccount, UsageReport, UsageStatus, UsageWindow,
};
use omp_core::Str;
use serde_json::Value;

use super::ServiceState;
use crate::usage_cmd::{self, QuotaSnapshot};

/// Fraction at or above which a window is exhausted.
const EXHAUSTED: f64 = 1.0;
/// Fraction at or above which a window warns.
const WARNING: f64 = 0.8;
/// Fraction at or below which a window is untouched (pi `IDLE_FRACTION`).
const IDLE: f64 = 0.005;
const NO_ACTIVITY: &str =
	"Usage history unavailable (this host keeps no per-day cost telemetry).";

/// Starts the quota fetch on the runtime; the receiver settles with the
/// dashboard report.
pub fn fetch(state: &ServiceState) -> ServiceResult<Pending<UsageReport>> {
	let (tx, rx) = flume::bounded(1);
	let data_dir = state.data_dir.clone();
	let catalog = state.catalog.clone();
	state.runtime.spawn(async move {
		let result = build(&data_dir, catalog.as_deref()).await;
		let _ = tx.send(result);
	});
	Ok(rx)
}

/// `/usage reset [account|active]`: lists or spends saved Codex resets.
/// The redemption is a short network call; the actor blocks on it the
/// same way `omp usage` does.
pub fn reset(state: &ServiceState, target: &str) -> ServiceResult<Str> {
	let data_dir = state.data_dir.clone();
	let target = target.to_owned();
	let runtime = state.runtime.clone();
	let on_worker = tokio::runtime::Handle::try_current()
		.is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
	let outcome = if on_worker {
		tokio::task::block_in_place(|| runtime.block_on(usage_cmd::reset_usage(&data_dir, &target)))
	} else {
		let (tx, rx) = flume::bounded(1);
		runtime.spawn(async move {
			let _ = tx.send(usage_cmd::reset_usage(&data_dir, &target).await);
		});
		rx.recv().map_err(|_| {
			ServiceError::Failed(Str::new_static("usage reset task ended without a result"))
		})?
	};
	outcome.map_err(ServiceError::failed)
}

async fn build(data_dir: &Path, catalog: Option<&Catalog>) -> ServiceResult<UsageReport> {
	fs::create_dir_all(data_dir).map_err(ServiceError::failed)?;
	let snapshot = usage_cmd::collect_quota(data_dir, None, None)
		.await
		.map_err(ServiceError::failed)?;
	let now_ms = unix_ms(SystemTime::now()).unwrap_or_default();
	Ok(UsageReport {
		checked_at_ms: Some(
			snapshot
				.rows
				.iter()
				.filter_map(|row| row["observedAtMs"].as_u64())
				.max()
				.unwrap_or(now_ms),
		),
		accounts: cards(&snapshot, catalog, now_ms),
		activity: Vec::new(),
		activity_note: Some(Str::new_static(NO_ACTIVITY)),
		detail: detail(&snapshot),
	})
}

fn unix_ms(time: SystemTime) -> Option<u64> {
	time
		.duration_since(UNIX_EPOCH)
		.ok()
		.and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

fn fraction(row: &Value) -> Option<f64> {
	let consumed = row["consumed"].as_f64()?;
	let limit = row["limit"].as_f64()?;
	(limit > 0.0).then(|| (consumed / limit).max(0.0))
}

/// Window health from its consumed fraction.
fn status(fraction: Option<f64>) -> UsageStatus {
	match fraction {
		None => UsageStatus::Unknown,
		Some(value) if value >= EXHAUSTED => UsageStatus::Exhausted,
		Some(value) if value >= WARNING => UsageStatus::Warning,
		Some(value) if value <= IDLE => UsageStatus::Idle,
		Some(_) => UsageStatus::Ok,
	}
}

/// One provider's quota buckets while folding rows.
#[derive(Default)]
struct ProviderFold {
	accounts: Vec<Str>,
	/// Window id → (label, per-account observations).
	windows:  BTreeMap<Str, (Str, Vec<(Option<f64>, Option<u64>)>)>,
}

/// One card per provider: each window bucket shows the mean used fraction
/// across accounts with the most-used account's reset countdown (pi
/// `buildProviderCards`).
fn cards(snapshot: &QuotaSnapshot, catalog: Option<&Catalog>, now_ms: u64) -> Vec<UsageAccount> {
	let mut folds: BTreeMap<Str, ProviderFold> = BTreeMap::new();
	for row in &snapshot.rows {
		let Some(provider) = row["provider"].as_str() else {
			continue;
		};
		let fold = folds.entry(Str::new(provider)).or_default();
		let account = Str::new(row["account"].as_str().unwrap_or("********"));
		if !fold.accounts.contains(&account) {
			fold.accounts.push(account);
		}
		let window = Str::new(row["window"].as_str().unwrap_or("default"));
		let label = row["label"]
			.as_str()
			.map_or_else(|| window.clone(), Str::new);
		fold
			.windows
			.entry(window)
			.or_insert_with(|| (label, Vec::new()))
			.1
			.push((fraction(row), row["resetAtMs"].as_u64()));
	}
	// Refresh failures are keyed by provider in their message prefix.
	for error in &snapshot.refresh_errors {
		if let Some(provider) = error
			.strip_prefix("usage refresh failed for ")
			.and_then(|rest| rest.split(" / ").next())
		{
			folds.entry(Str::new(provider)).or_default();
		}
	}
	folds
		.into_iter()
		.map(|(provider, fold)| {
			let mut windows = fold
				.windows
				.into_iter()
				.map(|(_, (label, observations))| {
					let known = observations
						.iter()
						.filter_map(|(fraction, _)| *fraction)
						.collect::<Vec<_>>();
					#[allow(clippy::cast_precision_loss, reason = "account counts are tiny")]
					let mean = (!known.is_empty())
						.then(|| known.iter().sum::<f64>() / known.len() as f64);
					let worst = observations
						.iter()
						.max_by(|a, b| {
							a.0
								.unwrap_or(-1.0)
								.total_cmp(&b.0.unwrap_or(-1.0))
						})
						.and_then(|(_, reset)| *reset)
						.filter(|reset| *reset > now_ms)
						.map(|reset| Duration::from_millis(reset - now_ms));
					UsageWindow {
						label,
						fraction: mean.unwrap_or(0.0),
						resets_in: worst,
						status: status(mean),
					}
				})
				.collect::<Vec<_>>();
			windows.sort_by(|a, b| b.fraction.total_cmp(&a.fraction));
			let errors = snapshot
				.refresh_errors
				.iter()
				.filter(|error| {
					error
						.strip_prefix("usage refresh failed for ")
						.is_some_and(|rest| rest.starts_with(provider.as_str()))
				})
				.map(String::as_str)
				.collect::<Vec<_>>();
			let title = catalog
				.and_then(|catalog| catalog.provider(&ProviderId::from(provider.as_str())))
				.map_or_else(|| provider.clone(), |definition| definition.name.clone());
			UsageAccount {
				provider,
				title,
				accounts: fold.accounts,
				windows,
				error: (!errors.is_empty()).then(|| Str::new(errors.join("; "))),
			}
		})
		.collect()
}

/// Classic per-account report (pi `renderDetail`), markdown.
fn detail(snapshot: &QuotaSnapshot) -> Str {
	let mut out = String::from("**Usage**\n\n");
	if snapshot.rows.is_empty() {
		out.push_str("No provider quota observations recorded.\n");
	}
	let mut previous: Option<&str> = None;
	for row in &snapshot.rows {
		let provider = row["provider"].as_str().unwrap_or("unknown");
		if previous != Some(provider) {
			if previous.is_some() {
				out.push('\n');
			}
			let _ = writeln!(out, "### {provider}\n");
			previous = Some(provider);
		}
		let account = row["account"].as_str().unwrap_or("********");
		let window = row["label"]
			.as_str()
			.or_else(|| row["window"].as_str())
			.unwrap_or("default");
		let consumed = row["consumed"]
			.as_f64()
			.map_or_else(|| "—".to_owned(), format_number);
		let limit = row["limit"]
			.as_f64()
			.map_or_else(|| "—".to_owned(), format_number);
		let _ = write!(out, "- `{account}` · {window}: {consumed} / {limit}");
		if let Some(fraction) = fraction(row) {
			let _ = write!(out, " ({}% used)", (fraction * 100.0).round());
		}
		if let Some(reset) = row["resetAtMs"].as_u64()
			&& let Some(now) = unix_ms(SystemTime::now())
			&& reset > now
		{
			let _ = write!(
				out,
				" · resets in {}",
				omp_chat::notices::format_duration(reset - now)
			);
		}
		if row["fresh"].as_bool() != Some(true) {
			out.push_str(" · stale");
		}
		out.push('\n');
	}
	for report in &snapshot.reports {
		if let Some(credits) = &report.reset_credits {
			let _ = writeln!(
				out,
				"\n`{}` saved resets: {} available",
				usage_cmd::mask(report.account.as_str()),
				credits.available
			);
		}
		for note in &report.notes {
			let _ = writeln!(out, "\n> {note}");
		}
	}
	if !snapshot.refresh_errors.is_empty() {
		out.push_str("\n**Refresh errors**\n\n");
		for error in &snapshot.refresh_errors {
			let _ = writeln!(out, "- {error}");
		}
	}
	Str::from(out)
}

fn format_number(value: f64) -> String {
	if value.fract() == 0.0 {
		format!("{value:.0}")
	} else {
		format!("{value:.2}")
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	fn snapshot() -> QuotaSnapshot {
		QuotaSnapshot {
			rows:           vec![
				json!({"provider": "openai-codex", "account": "abcd…wxyz", "window": "primary", "label": "5h", "consumed": 40.0, "limit": 100.0, "resetAtMs": 1_800_000_060_000_u64, "observedAtMs": 1_800_000_000_000_u64, "fresh": true}),
				json!({"provider": "openai-codex", "account": "efgh…stuv", "window": "primary", "label": "5h", "consumed": 100.0, "limit": 100.0, "resetAtMs": 1_800_000_120_000_u64, "observedAtMs": 1_800_000_000_000_u64, "fresh": true}),
				json!({"provider": "anthropic", "account": "ijkl…mnop", "window": "weekly", "consumed": 0.0, "limit": 50.0, "observedAtMs": 1_799_999_000_000_u64}),
			],
			reports:        Vec::new(),
			refresh_errors: vec!["usage refresh failed for anthropic / ijkl…mnop: boom".to_owned()],
		}
	}

	#[test]
	fn cards_average_accounts_and_take_the_worst_reset() {
		let cards = cards(&snapshot(), None, 1_800_000_000_000);
		let codex = cards
			.iter()
			.find(|card| card.provider.as_str() == "openai-codex")
			.unwrap();
		assert_eq!(codex.accounts.len(), 2);
		assert_eq!(codex.windows.len(), 1);
		let window = &codex.windows[0];
		assert_eq!(window.label.as_str(), "5h");
		assert!((window.fraction - 0.7).abs() < 1e-9);
		assert_eq!(window.resets_in, Some(Duration::from_secs(120)));
		assert_eq!(window.status, UsageStatus::Ok);
		let anthropic = cards
			.iter()
			.find(|card| card.provider.as_str() == "anthropic")
			.unwrap();
		assert_eq!(anthropic.windows[0].status, UsageStatus::Idle);
		assert!(anthropic.error.as_deref().unwrap().contains("boom"));
	}

	#[test]
	fn status_thresholds_match_the_dashboard_contract() {
		assert_eq!(status(None), UsageStatus::Unknown);
		assert_eq!(status(Some(1.0)), UsageStatus::Exhausted);
		assert_eq!(status(Some(0.8)), UsageStatus::Warning);
		assert_eq!(status(Some(0.005)), UsageStatus::Idle);
		assert_eq!(status(Some(0.3)), UsageStatus::Ok);
	}

	#[test]
	fn detail_groups_rows_by_provider() {
		let text = detail(&snapshot());
		assert!(text.starts_with("**Usage**"));
		assert!(text.contains("### openai-codex"));
		assert!(text.contains("`abcd…wxyz` · 5h: 40 / 100 (40% used)"));
		assert!(text.contains("· stale"), "{text}");
		assert!(text.contains("**Refresh errors**"));
	}
}
