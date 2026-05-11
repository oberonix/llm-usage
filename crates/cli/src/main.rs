//! `llm-usage-status` — terminal-friendly snapshot renderer.
//!
//! Prints the same per-provider quota bars the tray menu shows, then
//! exits. The intent is "I'm in a terminal and want to glance at my
//! current usage without opening the dashboard".
//!
//! Source of truth, in order of preference:
//! 1. The shared `snapshots.json` written by the tray. Free, instant.
//!    If you're already running the tray (usually true) this is what
//!    you want.
//! 2. A direct one-shot poll of every enabled provider, mirroring the
//!    `print_snapshots` example. Used when the tray hasn't run yet or
//!    `snapshots.json` is stale beyond `STALE_AFTER`.
//!
//! Pass `--refresh` to force a fresh poll regardless of the cache. The
//! exit code is 0 on success, 1 if no provider returned anything.

use anyhow::Result;
use llm_usage_core::config::Config;
use llm_usage_core::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowUsage};
use llm_usage_core::provider::Provider;
use llm_usage_core::providers::{AnthropicProvider, CodexCliProvider, OllamaCloudProvider};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

/// snapshots.json older than this triggers a fresh poll.
const STALE_AFTER: Duration = Duration::from_secs(300);

/// The same priority table the dashboard and tray use, so every
/// surface lists windows in the same order.
fn window_order(label: &str) -> u32 {
    match label {
        "5h" => 10,
        "week" => 20,
        "week (Sonnet)" => 21,
        "week (Opus)" => 22,
        "1h" => 100,
        "today" => 101,
        "month" => 102,
        _ => 50,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let force_refresh = std::env::args().skip(1).any(|a| a == "--refresh");
    let use_color = std::io::stdout().is_terminal();

    let snapshots = if force_refresh {
        poll_fresh().await?
    } else {
        match cached_snapshots() {
            Some(snaps) => snaps,
            None => poll_fresh().await?,
        }
    };

    if snapshots.is_empty() {
        eprintln!("No provider data. Enable a provider in the dashboard's Settings tab.");
        std::process::exit(1);
    }

    render(&snapshots, use_color);
    Ok(())
}

/// Read `snapshots.json` if it exists and was written recently.
/// Returns `None` if the file is missing or stale — caller falls back
/// to a direct poll.
fn cached_snapshots() -> Option<BTreeMap<ProviderId, UsageSnapshot>> {
    let file = llm_usage_core::read_snapshots().ok().flatten()?;
    let age = (chrono::Utc::now() - file.updated_at)
        .to_std()
        .unwrap_or(Duration::ZERO);
    if age > STALE_AFTER {
        tracing::debug!(?age, "snapshots.json is stale; falling back to live poll");
        return None;
    }
    Some(file.snapshots)
}

async fn poll_fresh() -> Result<BTreeMap<ProviderId, UsageSnapshot>> {
    let config = Config::load_or_default()?;
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(AnthropicProvider::new(config.anthropic.clone())),
        Box::new(CodexCliProvider::new(config.codex_cli.clone())),
        Box::new(OllamaCloudProvider::new(config.ollama_cloud.clone())),
    ];
    let mut out = BTreeMap::new();
    for p in &providers {
        if !p.enabled() {
            continue;
        }
        if let Ok(snap) = p.poll().await {
            out.insert(p.id(), snap);
        }
    }
    Ok(out)
}

fn render(snapshots: &BTreeMap<ProviderId, UsageSnapshot>, use_color: bool) {
    // Match the dashboard / tray ordering: Anthropic, Codex, Ollama Cloud.
    let order = [
        ProviderId::Anthropic,
        ProviderId::CodexCli,
        ProviderId::OllamaCloud,
    ];
    let mut first = true;
    for id in order {
        let Some(snap) = snapshots.get(&id) else {
            continue;
        };
        if matches!(snap.status, ProviderStatus::Unavailable) {
            continue;
        }
        let mut quota_windows: Vec<(&String, &WindowUsage)> = snap
            .windows
            .iter()
            .filter(|(_, w)| w.fraction_used.is_some())
            .collect();
        if quota_windows.is_empty() {
            continue;
        }
        quota_windows.sort_by_key(|(label, _)| (window_order(label.as_str()), label.as_str()));

        if !first {
            println!();
        }
        first = false;

        // Header — provider name + plan tag (when set).
        let plan = snap
            .plan_label
            .as_deref()
            .map(|p| format!(" · {}", p))
            .unwrap_or_default();
        let header = format!("{}{}", id.human(), plan);
        if use_color {
            // Bold header.
            println!("\x1b[1m{}\x1b[0m", header);
        } else {
            println!("{}", header);
        }

        for (label, w) in quota_windows {
            println!("  {}", format_quota_row(label, w, use_color));
        }
    }
}

fn format_quota_row(label: &str, w: &WindowUsage, use_color: bool) -> String {
    let frac = w.fraction_used.unwrap_or(0.0);
    let bar = unicode_bar(frac, 10);
    let pct_raw = format!("{:>3.0}%", frac * 100.0);
    let pct = if use_color {
        format!("{}{}\x1b[0m", color_for(frac), pct_raw)
    } else {
        pct_raw
    };
    let reset = w
        .ends_at
        .and_then(|t| {
            let secs = (t - chrono::Utc::now()).num_seconds();
            if secs > 0 {
                Some(format_reset(secs))
            } else {
                None
            }
        })
        .map(|s| format!(" · {}", s))
        .unwrap_or_default();
    format!("{} {} · {}{}", bar, pct, label, reset)
}

fn unicode_bar(fraction: f64, cells: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
    let mut s = String::with_capacity(cells * 3);
    for _ in 0..filled {
        s.push('▰');
    }
    for _ in filled..cells {
        s.push('▱');
    }
    s
}

fn color_for(fraction: f64) -> &'static str {
    if fraction < 0.60 {
        "\x1b[32m" // green
    } else if fraction < 0.85 {
        "\x1b[33m" // yellow
    } else {
        "\x1b[31m" // red
    }
}

fn format_reset(secs: i64) -> String {
    if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
