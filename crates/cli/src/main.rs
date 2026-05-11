//! `llm-usage-status` — terminal/CLI usage view.
//!
//! Default: live mode. The CLI watches the tray's shared `snapshots.json`
//! and redraws the per-provider quota bars whenever the tray writes a
//! new poll. Picks up the tray's polling cadence implicitly — no
//! configuration of its own.
//!
//! Flags:
//!   --once       Render once and exit (handy for piping to a file or
//!                running under `watch -n N`).
//!   --refresh    Touch the refresh trigger so the tray polls right
//!                away. Works in both live and one-shot mode.
//!
//! Intended use: open a small terminal window on the side of your
//! screen, run `llm-usage-status`, and leave it. Press Ctrl+C to exit.

use anyhow::Result;
use llm_usage_core::config::Config;
use llm_usage_core::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowUsage};
use llm_usage_core::provider::Provider;
use llm_usage_core::providers::{AnthropicProvider, CodexCliProvider, OllamaCloudProvider};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// snapshots.json older than this triggers a fresh poll (one-shot mode).
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let force_refresh = args.iter().any(|a| a == "--refresh");
    let once = args.iter().any(|a| a == "--once");

    if force_refresh {
        // Best-effort: tells the tray to poll immediately. Tray's
        // notify watcher picks the trigger up; the new poll then
        // rewrites snapshots.json which our live loop sees.
        let _ = llm_usage_core::touch_refresh_trigger();
    }

    let use_color = std::io::stdout().is_terminal();

    if once {
        run_once(use_color).await
    } else {
        run_live(use_color).await
    }
}

/// One-shot mode — render the current data and exit. Reads
/// `snapshots.json` if fresh; otherwise polls providers directly.
async fn run_once(use_color: bool) -> Result<()> {
    let snapshots = match cached_snapshots() {
        Some(s) => s,
        None => poll_fresh().await?,
    };
    if snapshots.is_empty() {
        eprintln!("No provider data. Enable a provider in the dashboard's Settings tab.");
        std::process::exit(1);
    }
    print!("{}", build_screen(&snapshots, use_color, /*clear*/ false));
    let _ = std::io::stdout().flush();
    Ok(())
}

/// Live mode — watch `snapshots.json` for writes and redraw on each
/// change. Blocks until Ctrl+C; the final render stays on screen.
async fn run_live(use_color: bool) -> Result<()> {
    // Initial paint — try cache first; fall back to a live poll so the
    // user isn't staring at "no data" while the tray's first poll runs.
    let initial = match cached_snapshots() {
        Some(s) => s,
        None => poll_fresh().await.unwrap_or_default(),
    };
    render_screen(&initial, use_color);

    let snap_path = llm_usage_core::config::snapshots_path()?;
    let parent = snap_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("snapshots path has no parent"))?;
    std::fs::create_dir_all(&parent).ok();

    let (tx, rx) = mpsc::channel::<()>();
    let target = snap_path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else {
            return;
        };
        if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
            return;
        }
        if event.paths.iter().any(|p| p == &target) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    // Block on incoming events. recv_timeout with a generous duration
    // is just there so a missed signal can't permanently wedge the loop.
    loop {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => {
                // Coalesce a burst of writes (atomic save = temp+rename
                // fires more than one event) into one redraw.
                let drain_until = Instant::now() + Duration::from_millis(50);
                while Instant::now() < drain_until {
                    if rx.try_recv().is_err() {
                        break;
                    }
                }
                if let Ok(Some(file)) = llm_usage_core::read_snapshots() {
                    render_screen(&file.snapshots, use_color);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

fn render_screen(snapshots: &BTreeMap<ProviderId, UsageSnapshot>, use_color: bool) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(build_screen(snapshots, use_color, true).as_bytes());
    let _ = out.flush();
}

/// Build the rendered text. When `clear` is true (live mode on a TTY),
/// the output starts with an ANSI clear-screen + home-cursor sequence
/// so each redraw replaces the previous frame in place.
fn build_screen(
    snapshots: &BTreeMap<ProviderId, UsageSnapshot>,
    use_color: bool,
    clear: bool,
) -> String {
    let mut s = String::new();
    if clear && use_color {
        s.push_str("\x1b[2J\x1b[H");
    }

    let order = [
        ProviderId::Anthropic,
        ProviderId::CodexCli,
        ProviderId::OllamaCloud,
    ];

    let mut first = true;
    let mut any_rendered = false;
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
            s.push('\n');
        }
        first = false;
        any_rendered = true;

        let plan = snap
            .plan_label
            .as_deref()
            .map(|p| format!(" · {}", p))
            .unwrap_or_default();
        let header = format!("{}{}", id.human(), plan);
        if use_color {
            s.push_str(&format!("\x1b[1m{}\x1b[0m\n", header));
        } else {
            s.push_str(&header);
            s.push('\n');
        }

        for (label, w) in quota_windows {
            s.push_str("  ");
            s.push_str(&format_quota_row(label, w, use_color));
            s.push('\n');
        }
    }

    if !any_rendered {
        s.push_str("waiting for first snapshot…\n");
    }
    s
}

fn cached_snapshots() -> Option<BTreeMap<ProviderId, UsageSnapshot>> {
    let file = llm_usage_core::read_snapshots().ok().flatten()?;
    let age = (chrono::Utc::now() - file.updated_at)
        .to_std()
        .unwrap_or(Duration::ZERO);
    if age > STALE_AFTER {
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
