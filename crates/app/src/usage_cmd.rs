//! Durable quota-history CLI over the inference-owned account state store.

use std::{
	fmt::Write as _,
	fs,
	path::Path,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use miette::{IntoDiagnostic as _, miette};
use omp_catalog::ProviderId;
use omp_core::Str;
use omp_inference::{
	account::AccountStateStore,
	answer::{UsageQuantity, UsageReport},
	call::{UsageRequest, UsageScope},
	id::AccountId,
};
use omp_proto::omp::auth::v1::{
	GetClientUsageRequest, auth_client::AuthClient, get_client_usage_response,
};
use omp_storage::index::{SessionIndex, UsageDimension, UsageQuery};
use serde_json::{Value, json};
use tonic::Request;

use crate::cli::{UsageAction, UsageArgs};

/// Renders durable quota snapshots, client attribution, or explicitly
/// invalidates them.
pub async fn run(args: UsageArgs) -> miette::Result<()> {
	if args.account.is_some() && args.provider.is_some() {
		return Err(miette!("--account and --provider are mutually exclusive"));
	}
	let data_dir = omp_core::dirs::data_dir(args.data_dir).into_diagnostic()?;
	fs::create_dir_all(&data_dir).into_diagnostic()?;
	if args.action == Some(UsageAction::Clients) {
		if args.invalidate || args.provider.is_some() || args.account.is_some() {
			return Err(miette!(
				"usage clients cannot be combined with --invalidate, --provider, or --account"
			));
		}
		return run_client_usage(&data_dir, args.days, args.json, args.gateway.as_ref()).await;
	}
	let store = AccountStateStore::open(data_dir.join("credentials.db")).into_diagnostic()?;
	let provider = args.provider.map(ProviderId::from);
	let account = args.account.map(AccountId::from);
	if args.invalidate {
		let removed = store
			.invalidate_usage(provider.as_deref(), account.as_deref())
			.into_diagnostic()?;
		if args.json {
			println!("{}", json!({ "invalidatedReceipts": removed }));
		} else {
			println!("invalidated {removed} durable usage receipt(s)");
		}
		return Ok(());
	}

	let snapshot = collect_quota(&data_dir, provider.as_ref(), account.as_ref()).await?;
	for error in &snapshot.refresh_errors {
		eprintln!("{error}");
	}
	if args.json {
		println!("{}", serde_json::to_string_pretty(&snapshot.rows).into_diagnostic()?);
		return Ok(());
	}
	if snapshot.rows.is_empty() {
		println!("no quota observations");
		return Ok(());
	}
	for row in snapshot.rows {
		print_row(&row);
	}
	Ok(())
}

async fn run_client_usage(
	data_dir: &Path,
	days: u32,
	json_output: bool,
	gateway: Option<&crate::endpoint::LocalEndpoint>,
) -> miette::Result<()> {
	let now_ms = unix_millis(SystemTime::now()).unwrap_or_default();
	let since_ms = now_ms.saturating_sub(u64::from(days.max(1)).saturating_mul(86_400_000));
	let clients = if let Some(gateway) = gateway {
		let channel = gateway.connect().await.into_diagnostic()?;
		AuthClient::new(channel)
			.get_client_usage(Request::new(GetClientUsageRequest { since_ms }))
			.await
			.into_diagnostic()?
			.into_inner()
			.clients
	} else {
		let index = SessionIndex::open_authoritative_reader(data_dir.join("sessions.sqlite3"))
			.into_diagnostic()?;
		index
			.client_usage(since_ms)
			.into_diagnostic()?
			.into_iter()
			.map(|client| get_client_usage_response::ClientUsage {
				install_id:    client.install_id.to_string(),
				hostname:      client
					.hostname
					.map_or_else(String::new, |value| value.to_string()),
				first_seen_ms: client.first_seen_ms,
				last_seen_ms:  client.last_seen_ms,
				providers:     client
					.providers
					.into_iter()
					.map(|provider| get_client_usage_response::client_usage::ProviderUsage {
						app:                provider
							.app
							.map_or_else(String::new, |value| value.to_string()),
						provider:           provider.provider.to_string(),
						model:              provider.model.to_string(),
						requests:           provider.requests,
						input_tokens:       provider.input_tokens,
						output_tokens:      provider.output_tokens,
						cache_read_tokens:  provider.cache_read_tokens,
						cache_write_tokens: provider.cache_write_tokens,
						nanos_usd:          provider.cost_nanos_usd,
					})
					.collect(),
			})
			.collect()
	};
	if json_output {
		let clients = clients.iter().map(client_usage_json).collect::<Vec<_>>();
		println!(
			"{}",
			serde_json::to_string_pretty(&json!({
				"generatedAt": now_ms,
				"sinceMs": since_ms,
				"clients": clients,
			}))
			.into_diagnostic()?
		);
	} else if clients.is_empty() {
		println!("no per-client usage recorded");
	} else {
		println!("{}", format_client_usage(&clients, since_ms, now_ms));
	}
	Ok(())
}

fn client_usage_json(client: &get_client_usage_response::ClientUsage) -> Value {
	json!({
		"installId": client.install_id.as_str(),
		"hostname": (!client.hostname.is_empty()).then_some(client.hostname.as_str()),
		"firstSeen": client.first_seen_ms,
		"lastSeen": client.last_seen_ms,
		"providers": client.providers.iter().map(|usage| json!({
			"app": (!usage.app.is_empty()).then_some(usage.app.as_str()),
			"provider": usage.provider.as_str(),
			"model": usage.model.as_str(),
			"requests": usage.requests,
			"inputTokens": usage.input_tokens,
			"outputTokens": usage.output_tokens,
			"cacheReadTokens": usage.cache_read_tokens,
			"cacheWriteTokens": usage.cache_write_tokens,
			"costUsd": usage.nanos_usd as f64 / 1_000_000_000.0,
		})).collect::<Vec<_>>(),
	})
}

/// Renders one section per installation and one row per app/provider/model
/// aggregate.
pub fn format_client_usage(
	clients: &[get_client_usage_response::ClientUsage],
	since_ms: u64,
	now_ms: u64,
) -> Str {
	let mut output = format!("Per-client token burn since {since_ms}\n");
	for client in clients {
		let label = if client.hostname.is_empty() {
			client.install_id.as_str()
		} else {
			client.hostname.as_str()
		};
		let short_id = client
			.install_id
			.get(..8)
			.unwrap_or(client.install_id.as_str());
		let age = now_ms.saturating_sub(client.last_seen_ms) / 1_000;
		let _ = writeln!(output, "\n{label} · {short_id} · last seen {age}s ago");
		if client.providers.is_empty() {
			output.push_str("  no usage in this window\n");
			continue;
		}
		output.push_str(
			"  app              provider/model                    requests      input     output    \
			 cache r    cache w      total   est cost\n",
		);
		let mut total_requests = 0_u64;
		let mut total_tokens = 0_u64;
		let mut total_cost = 0_u64;
		for usage in &client.providers {
			let app = if usage.app.is_empty() {
				"—"
			} else {
				usage.app.as_str()
			};
			let model = format!("{}/{}", usage.provider, usage.model);
			let tokens = usage
				.input_tokens
				.saturating_add(usage.output_tokens)
				.saturating_add(usage.cache_read_tokens)
				.saturating_add(usage.cache_write_tokens);
			total_requests = total_requests.saturating_add(usage.requests);
			total_tokens = total_tokens.saturating_add(tokens);
			total_cost = total_cost.saturating_add(usage.nanos_usd);
			let _ = writeln!(
				output,
				"  {app:<16} {model:<32} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}  ${:>8.2}",
				usage.requests,
				format_token_count(usage.input_tokens),
				format_token_count(usage.output_tokens),
				format_token_count(usage.cache_read_tokens),
				format_token_count(usage.cache_write_tokens),
				format_token_count(tokens),
				usage.nanos_usd as f64 / 1_000_000_000.0,
			);
		}
		let _ = writeln!(
			output,
			"  {:<16} {:<32} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}  ${:>8.2}",
			"",
			"total",
			total_requests,
			"",
			"",
			"",
			"",
			format_token_count(total_tokens),
			total_cost as f64 / 1_000_000_000.0,
		);
	}
	Str::from(output.trim_end())
}

fn format_token_count(value: u64) -> String {
	if value >= 1_000_000_000 {
		format!("{:.2}B", value as f64 / 1_000_000_000.0)
	} else if value >= 1_000_000 {
		format!("{:.1}M", value as f64 / 1_000_000.0)
	} else if value >= 1_000 {
		format!("{:.1}k", value as f64 / 1_000.0)
	} else {
		value.to_string()
	}
}

struct QuotaSnapshot {
	rows:           Vec<Value>,
	reports:        Vec<UsageReport>,
	refresh_errors: Vec<String>,
}

async fn collect_quota(
	data_dir: &Path,
	provider: Option<&ProviderId>,
	account: Option<&AccountId>,
) -> miette::Result<QuotaSnapshot> {
	let store = AccountStateStore::open(data_dir.join("credentials.db")).into_diagnostic()?;
	let records = store.load_accounts().into_diagnostic()?;
	let mut rows = Vec::new();
	for record in &records {
		if provider.is_some_and(|value| value != &record.provider)
			|| account.is_some_and(|value| value != &record.account)
		{
			continue;
		}
		let state = store.load_account(&record.account).into_diagnostic()?;
		for (window_id, window) in state.quota.windows() {
			let trend = window
				.receipts
				.iter()
				.filter_map(|receipt| {
					receipt
						.consumed
						.zip(receipt.limit)
						.and_then(|(consumed, limit)| {
							(limit != 0).then_some((consumed as f64 / limit as f64).clamp(0.0, 1.0))
						})
				})
				.collect::<Vec<_>>();
			rows.push(json!({
				"provider": record.provider.as_str(),
				"account": mask(record.account.as_str()),
				"window": window_id.as_str(),
				"consumed": window.consumed.map(|sample| sample.value),
				"remaining": window.remaining.map(|sample| sample.value),
				"limit": window.limit.map(|sample| sample.value),
				"resetAtMs": window.reset_at.and_then(|sample| unix_millis(sample.value)),
				"observedAtMs": window.receipts.last().and_then(|receipt| unix_millis(receipt.observed_at)),
				"historySamples": window.receipts.len(),
				"trend": trend,
			}));
		}
	}
	let manager = omp_driver::registry::production_usage_manager(data_dir)
		.await
		.into_diagnostic()?;
	let mut reports = Vec::new();
	let mut refresh_errors = Vec::new();
	for record in &records {
		if provider.is_some_and(|value| value != &record.provider)
			|| account.is_some_and(|value| value != &record.account)
		{
			continue;
		}
		let Some(route) = record.routes.iter().next() else {
			continue;
		};
		let request = UsageRequest {
			provider:    Some(record.provider.clone()),
			account:     Some(record.account.clone()),
			scope:       UsageScope::All,
			allow_stale: false,
		};
		match manager
			.execute(
				&record.provider,
				route,
				&request,
				Instant::now().checked_add(Duration::from_secs(20)),
			)
			.await
		{
			Ok(report) => {
				merge_fresh(&mut rows, &report);
				reports.push(report);
			},
			Err(error) => refresh_errors.push(format!(
				"usage refresh failed for {} / {}: {error}",
				record.provider.as_str(),
				mask(record.account.as_str())
			)),
		}
	}
	Ok(QuotaSnapshot { rows, reports, refresh_errors })
}

/// Renders durable per-model accounting and the latest provider quota windows.
pub async fn render_report(data_dir: &Path) -> miette::Result<Str> {
	fs::create_dir_all(data_dir).into_diagnostic()?;
	let index = SessionIndex::open_authoritative_reader(data_dir.join("sessions.sqlite3"))
		.into_diagnostic()?;
	let models = index.usage(&UsageQuery::default()).into_diagnostic()?;
	let quota = collect_quota(data_dir, None, None).await?;

	let mut rendered = String::from("**Usage**\n\n");
	if models.is_empty() {
		rendered.push_str("No durable model usage receipts recorded.\n");
	} else {
		rendered.push_str("| Model | Input | Output | Cache read | Cache write | Cost |\n");
		rendered.push_str("|---|---:|---:|---:|---:|---:|\n");
		let mut total_input = 0_u64;
		let mut total_output = 0_u64;
		let mut total_cache_read = 0_u64;
		let mut total_cache_write = 0_u64;
		let mut total_cost = 0_u64;
		for bucket in &models {
			let model = bucket
				.key
				.iter()
				.find_map(|(dimension, value)| {
					(*dimension == UsageDimension::Model).then_some(value.as_str())
				})
				.unwrap_or("unknown");
			total_input = total_input.saturating_add(bucket.usage.input_tokens);
			total_output = total_output.saturating_add(bucket.usage.output_tokens);
			total_cache_read = total_cache_read.saturating_add(bucket.usage.cache_read_tokens);
			total_cache_write = total_cache_write.saturating_add(bucket.usage.cache_write_tokens);
			total_cost = total_cost.saturating_add(bucket.cost.nanos_usd);
			let _ = writeln!(
				rendered,
				"| `{model}` | {} | {} | {} | {} | ${:.6} |",
				bucket.usage.input_tokens,
				bucket.usage.output_tokens,
				bucket.usage.cache_read_tokens,
				bucket.usage.cache_write_tokens,
				bucket.cost.nanos_usd as f64 / 1_000_000_000.0,
			);
		}
		let _ = writeln!(
			rendered,
			"| **Total** | **{total_input}** | **{total_output}** | **{total_cache_read}** | \
			 **{total_cache_write}** | **${:.6}** |",
			total_cost as f64 / 1_000_000_000.0,
		);
	}

	rendered.push_str("\n**Quota windows**\n");
	if quota.rows.is_empty() {
		rendered.push_str("\nNo quota observations.\n");
	} else {
		for row in &quota.rows {
			let provider = row["provider"].as_str().unwrap_or("unknown");
			let account = row["account"].as_str().unwrap_or("********");
			let window = row["window"].as_str().unwrap_or("unknown");
			let label = row["label"].as_str().unwrap_or(window);
			let consumed = row["consumed"].as_f64();
			let limit = row["limit"].as_f64();
			let remaining = row["remaining"].as_f64();
			let fraction = consumed
				.zip(limit)
				.and_then(|(used, total)| (total != 0.0).then_some(used / total));
			let _ = write!(
				rendered,
				"\n- **{provider}** `{account}` · {label}\n  `{}` {} / {}",
				quota_bar(fraction),
				consumed.map_or_else(|| "?".to_owned(), format_number),
				limit.map_or_else(|| "?".to_owned(), format_number),
			);
			if let Some(remaining) = remaining {
				let _ = write!(rendered, " · {} remaining", format_number(remaining));
			}
			if let Some(reset_at) = row["resetAtMs"].as_u64() {
				let _ = write!(rendered, " · resets at {reset_at}");
			}
			if let Some(observed_at) = row["observedAtMs"].as_u64() {
				let _ = write!(rendered, " · observed at {observed_at}");
			}
			rendered.push('\n');
		}
	}
	for error in quota.refresh_errors {
		let _ = writeln!(rendered, "\n_{error}_");
	}
	Ok(Str::from(rendered))
}

/// Lists or spends saved Codex rate-limit resets.
pub async fn reset_usage(data_dir: &Path, target: &str) -> miette::Result<Str> {
	let codex_provider = ProviderId::from("openai-codex");
	let quota = collect_quota(data_dir, Some(&codex_provider), None).await?;
	if quota.reports.is_empty() {
		return Ok(Str::new_static("No Codex accounts found. Use /login to add one."));
	}
	let accounts = quota
		.reports
		.iter()
		.map(|report| {
			let label = report
				.account_meta
				.email
				.as_ref()
				.or(report.account_meta.provider_account_id.as_ref())
				.map_or_else(|| mask(report.account.as_str()), ToString::to_string);
			let available = report
				.reset_credits
				.as_ref()
				.map_or(0, |credits| credits.available);
			(label, available, report.account.clone())
		})
		.collect::<Vec<_>>();
	let target = target.trim();
	if target.is_empty() {
		let mut rendered = String::from("Saved Codex rate-limit resets:");
		for (index, (label, available, _)) in accounts.iter().enumerate() {
			let active = if index == 0 { " (active)" } else { "" };
			let _ = write!(rendered, "\n- {label}: {available} available{active}");
		}
		rendered
			.push_str("\n\nSpend one with `/usage reset <account email>` or `/usage reset active`.");
		return Ok(Str::from(rendered));
	}
	let selected = if target.eq_ignore_ascii_case("active") {
		accounts.first()
	} else {
		accounts.iter().find(|(label, _, account)| {
			label.eq_ignore_ascii_case(target) || account.as_str().eq_ignore_ascii_case(target)
		})
	};
	let Some((label, available, selected_account)) = selected else {
		return Ok(Str::from(format!("No Codex account matches \"{target}\".")));
	};
	if *available == 0 {
		return Ok(Str::from(format!("{label}: no saved resets to spend.")));
	}
	let redeemed = omp_driver::registry::redeem_codex_reset(data_dir, selected_account)
		.await
		.into_diagnostic()?
		.ok_or_else(|| miette!("Codex reset redemption is unavailable"))?;
	if redeemed {
		Ok(Str::from(format!(
			"Reset applied for {label} — your rate-limit window has been refreshed.",
		)))
	} else {
		Ok(Str::from(format!("{label}: nothing to reset right now — no credit was spent.",)))
	}
}

fn format_number(value: f64) -> String {
	if value.fract() == 0.0 {
		format!("{value:.0}")
	} else {
		format!("{value:.2}")
	}
}

fn merge_fresh(rows: &mut Vec<Value>, report: &UsageReport) {
	let provider = report.provider.as_str();
	let account = mask(report.account.as_str());
	for window in &report.windows {
		let consumed = window.amount.consumed.map(quantity_value);
		let remaining = window.amount.remaining.map(quantity_value);
		let limit = window.amount.limit.map(quantity_value);
		let existing = rows.iter_mut().find(|row| {
			row["provider"].as_str() == Some(provider)
				&& row["account"].as_str() == Some(account.as_str())
				&& row["window"].as_str() == Some(window.id.as_str())
		});
		if let Some(row) = existing {
			row["consumed"] = json!(consumed);
			row["label"] = json!(window.label.as_deref());
			row["remaining"] = json!(remaining);
			row["limit"] = json!(limit);
			row["resetAtMs"] = json!(window.resets_at.and_then(unix_millis));
			row["observedAtMs"] = json!(unix_millis(window.observed_at));
			row["fresh"] = json!(true);
		} else {
			rows.push(json!({
				"provider": provider,
				"account": account,
				"window": window.id.as_str(),
				"label": window.label.as_deref(),
				"consumed": consumed,
				"remaining": remaining,
				"limit": limit,
				"resetAtMs": window.resets_at.and_then(unix_millis),
				"observedAtMs": unix_millis(window.observed_at),
				"historySamples": 0,
				"trend": [],
				"fresh": true,
			}));
		}
	}
}

fn quantity_value(quantity: UsageQuantity) -> f64 {
	quantity.units as f64 / 10_f64.powi(i32::from(quantity.decimal_exponent))
}

fn mask(value: &str) -> String {
	if value.len() <= 8 {
		return "********".to_owned();
	}
	format!("{}…{}", &value[..4], &value[value.len() - 4..])
}

fn unix_millis(time: SystemTime) -> Option<u64> {
	time
		.duration_since(UNIX_EPOCH)
		.ok()
		.and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn print_row(row: &Value) {
	let consumed_value = row["consumed"].as_f64();
	let consumed = consumed_value.map_or("?".to_owned(), |value| value.to_string());
	let limit_value = row["limit"].as_f64();
	let limit = limit_value.map_or("?".to_owned(), |value| value.to_string());
	let fraction = consumed_value
		.zip(limit_value)
		.and_then(|(used, total)| (total != 0.0).then_some(used / total));
	let bar = quota_bar(fraction);
	let trend = row["trend"]
		.as_array()
		.map_or_else(String::new, |samples| trend_bar(samples));
	println!(
		"{:<18} {:<12} {:<20} {bar} {consumed}/{limit} ({} sample(s)) {trend}",
		row["provider"].as_str().unwrap_or("unknown"),
		row["account"].as_str().unwrap_or("********"),
		row["window"].as_str().unwrap_or("unknown"),
		row["historySamples"].as_u64().unwrap_or_default(),
	);
}

fn quota_bar(fraction: Option<f64>) -> String {
	const WIDTH: usize = 20;
	let Some(fraction) = fraction else {
		return "·".repeat(WIDTH);
	};
	let filled = (fraction.clamp(0.0, 1.0) * WIDTH as f64).round() as usize;
	format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled))
}

fn trend_bar(samples: &[Value]) -> String {
	const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
	samples
		.iter()
		.rev()
		.take(48)
		.rev()
		.filter_map(Value::as_f64)
		.map(|fraction| {
			let index = (fraction.clamp(0.0, 1.0) * LEVELS.len() as f64)
				.floor()
				.min((LEVELS.len() - 1) as f64) as usize;
			LEVELS[index]
		})
		.collect()
}
#[cfg(test)]
mod tests {
	use omp_proto::omp::auth::v1::get_client_usage_response::{
		ClientUsage, client_usage::ProviderUsage,
	};

	use super::format_client_usage;

	#[test]
	fn client_report_breaks_usage_down_by_application() {
		let rendered = format_client_usage(
			&[ClientUsage {
				install_id:    "install-123456".to_owned(),
				hostname:      "host-a".to_owned(),
				first_seen_ms: 1_000,
				last_seen_ms:  2_000,
				providers:     vec![
					ProviderUsage {
						app:                "omp".to_owned(),
						provider:           "anthropic".to_owned(),
						model:              "claude".to_owned(),
						requests:           2,
						input_tokens:       1_000,
						output_tokens:      20,
						cache_read_tokens:  30,
						cache_write_tokens: 40,
						nanos_usd:          500_000_000,
					},
					ProviderUsage {
						app:                "robomp".to_owned(),
						provider:           "anthropic".to_owned(),
						model:              "claude".to_owned(),
						requests:           1,
						input_tokens:       10,
						output_tokens:      2,
						cache_read_tokens:  0,
						cache_write_tokens: 0,
						nanos_usd:          10_000_000,
					},
				],
			}],
			0,
			3_000,
		);
		assert!(rendered.contains("host-a · install-"));
		assert!(rendered.contains("omp"));
		assert!(rendered.contains("robomp"));
		assert!(rendered.contains("anthropic/claude"));
		assert!(rendered.contains("total"));
	}
}
