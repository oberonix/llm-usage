//! `llm-usage` — terminal/CLI usage view.
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
//! screen, run `llm-usage`, and leave it. Press Ctrl+C to exit.

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
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("llm-usage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let force_refresh = args.iter().any(|a| a == "--refresh");
    let once = args.iter().any(|a| a == "--once");
    // Surface typos rather than silently dropping into watch mode.
    if let Some(unknown) = args.iter().find(|a| {
        !matches!(a.as_str(), "--refresh" | "--once" | "--help" | "-h" | "--version" | "-V")
    }) {
        eprintln!("unknown flag: {unknown}\n");
        print_usage();
        std::process::exit(2);
    }

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

fn print_usage() {
    println!(
        "llm-usage — tray-equivalent CLI view of LLM provider quotas.\n\
         \n\
         USAGE:\n\
            llm-usage [FLAGS]\n\
         \n\
         FLAGS:\n\
            --once         Render the current snapshot once and exit. Defaults\n\
                           to live mode (re-renders when the tray writes).\n\
            --refresh      Touch the tray's refresh trigger before rendering,\n\
                           so the next snapshot is fresh rather than cached.\n\
            -h, --help     Show this message.\n\
            -V, --version  Print the binary version.\n\
         \n\
         Reads `snapshots.json` written by `llm-usage-tray`. If the tray isn't\n\
         running, falls back to polling every enabled provider directly."
    );
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
///
/// Two redraw triggers feed into the same paint:
///   1. `snapshots.json` was written (provider poll completed).
///   2. `TICK` elapsed with no write — repaint with the last-seen data
///      so the pace marker and "last refreshed" clock keep ticking
///      between the (typically 15-minute) poll intervals.
async fn run_live(use_color: bool) -> Result<()> {
    /// Repaint cadence in the absence of a fresh snapshot. Fast
    /// enough that the 5h pace marker moves at most ~1 cell per 30
    /// real minutes — and the footer's wall-clock looks live.
    const TICK: Duration = Duration::from_secs(30);

    // Initial paint — try cache first; fall back to a live poll so the
    // user isn't staring at "no data" while the tray's first poll runs.
    let mut latest = match cached_snapshots() {
        Some(s) => s,
        None => poll_fresh().await.unwrap_or_default(),
    };
    render_screen(&latest, use_color);

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

    loop {
        match rx.recv_timeout(TICK) {
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
                    latest = file.snapshots;
                }
                render_screen(&latest, use_color);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic tick — same data, fresh `now`. Cheap: one
                // ANSI clear + ~30 lines of stdout.
                render_screen(&latest, use_color);
            }
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
        let has_activity = snap
            .windows
            .values()
            .any(|w| w.request_count > 0 || w.tokens_in > 0 || w.tokens_out > 0);
        // Activity-only providers (e.g. Codex when rate_limits is
        // stale but opencode captured turns) still get a header +
        // headline summary row. Only providers with nothing at all
        // get skipped.
        if quota_windows.is_empty() && !has_activity {
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

        if !quota_windows.is_empty() {
            for (label, w) in quota_windows {
                s.push_str("  ");
                s.push_str(&format_quota_row(label, w, use_color));
                s.push('\n');
            }
        } else if let Some(h) = &snap.headline {
            s.push_str("  ");
            s.push_str(h);
            s.push('\n');
        }
    }

    if !any_rendered {
        s.push_str("waiting for first snapshot…\n");
    }

    // Footer: when this frame was rendered. Intentionally has no
    // trailing newline — the cursor parks at the end of this line in
    // live mode, which gives the window a calm "ready / idle" look
    // until the next frame.
    s.push('\n');
    let now = chrono::Local::now().format("%H:%M:%S").to_string();
    if use_color {
        s.push_str(&format!("\x1b[90mlast refreshed {}\x1b[0m", now));
    } else {
        s.push_str(&format!("last refreshed {}", now));
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
    let opencode = config.resolve_opencode_db();
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(AnthropicProvider::with_opencode_db(
            config.anthropic.clone(),
            opencode.clone(),
        )),
        Box::new(CodexCliProvider::with_opencode_db(
            config.codex_cli.clone(),
            opencode.clone(),
        )),
        Box::new(OllamaCloudProvider::with_opencode_db(
            config.ollama_cloud.clone(),
            opencode,
        )),
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
    let now = chrono::Utc::now();
    let frac = w.fraction_used.unwrap_or(0.0);
    let pace_idx = pace_index(label, w, now, 10);
    let bar = colored_bar(frac, 10, pace_idx, use_color);
    let pct_raw = format!("{:>3.0}%", frac * 100.0);
    let pct = if use_color {
        format!("{}{}\x1b[0m", color_for(frac), pct_raw)
    } else {
        pct_raw
    };
    let suffix = quota_suffix(w, now);
    format!("{} {} · {}{}", bar, pct, label, suffix)
}

// Cell index (0..cells) of the pacing marker, given how much of the
// underlying window has elapsed. Mirrors the tray icon's pace logic
// in `crates/ui/src/icon.rs` — only labels with a known total
// duration get a marker. Returns `None` for labels we can't
// confidently size (1h / today / month: the renderer would have
// nothing to compare elapsed-vs-total against).
fn pace_index(
    label: &str,
    w: &WindowUsage,
    now: chrono::DateTime<chrono::Utc>,
    cells: usize,
) -> Option<usize> {
    let window_secs: i64 = match label {
        "5h" => 5 * 3600,
        "week" | "week (Sonnet)" | "week (Opus)" => 7 * 86_400,
        _ => return None,
    };
    let ends = w.ends_at?;
    let remaining = (ends - now).num_seconds().max(0).min(window_secs);
    let elapsed = (window_secs - remaining) as f64 / window_secs as f64;
    Some(((elapsed * (cells - 1) as f64).round() as usize).min(cells - 1))
}

// 10-cell bar with two layers of colour when `use_color`:
//   - filled pips get the same green/amber/red ramp as the % text
//   - one pip (at `pace_idx`) is painted purple to mark where the
//     time window itself currently sits.
// In plain-text mode (output piped, NO_COLOR set, redirect, etc.)
// we return the unstyled `▰`/`▱` glyphs and drop the pace marker —
// a purple `▰` would otherwise look like an extra unit of usage.
fn colored_bar(fraction: f64, cells: usize, pace_idx: Option<usize>, use_color: bool) -> String {
    let frac = fraction.clamp(0.0, 1.0);
    let filled = ((frac * cells as f64).round() as usize).min(cells);
    if !use_color {
        return unicode_bar(fraction, cells);
    }
    const PACE: &str = "\x1b[35m"; // magenta — distinct from green/amber/red
    const RESET: &str = "\x1b[0m";
    let fill_color = color_for(frac);
    let mut s = String::with_capacity(cells * 8);
    for i in 0..cells {
        if pace_idx == Some(i) {
            // Pace marker is always visible regardless of fill state;
            // glyph stays `▰` so it pops against `▱` background.
            s.push_str(PACE);
            s.push('▰');
            s.push_str(RESET);
        } else if i < filled {
            s.push_str(fill_color);
            s.push('▰');
            s.push_str(RESET);
        } else {
            s.push('▱');
        }
    }
    s
}

// Mirrors `quota_suffix` in the tray crate — see the comment there.
//   stale flag → " · ⚠"
//   future     → " · 2h"
//   otherwise  → ""
fn quota_suffix(w: &WindowUsage, now: chrono::DateTime<chrono::Utc>) -> String {
    if w.stale {
        return format!(" · {}", llm_usage_core::model::STALE_MARKER);
    }
    match w.ends_at {
        Some(t) if t > now => format!(" · {}", format_reset((t - now).num_seconds())),
        _ => String::new(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_bar_zero_is_all_empty() {
        assert_eq!(unicode_bar(0.0, 10), "▱▱▱▱▱▱▱▱▱▱");
    }

    #[test]
    fn unicode_bar_full_is_all_filled() {
        assert_eq!(unicode_bar(1.0, 10), "▰▰▰▰▰▰▰▰▰▰");
    }

    #[test]
    fn unicode_bar_half() {
        assert_eq!(unicode_bar(0.5, 10), "▰▰▰▰▰▱▱▱▱▱");
    }

    #[test]
    fn unicode_bar_clamps_above_one() {
        // Over-cap input should still produce a clean fully-filled bar.
        assert_eq!(unicode_bar(1.5, 8), "▰▰▰▰▰▰▰▰");
    }

    #[test]
    fn unicode_bar_clamps_below_zero() {
        assert_eq!(unicode_bar(-0.2, 5), "▱▱▱▱▱");
    }

    #[test]
    fn unicode_bar_rounds_to_nearest_cell() {
        // 0.25 of 10 = 2.5 → rounds to 2 or 3 depending on Rust's
        // round-half-away-from-zero policy (it rounds away → 3).
        let s = unicode_bar(0.25, 10);
        let filled = s.chars().filter(|c| *c == '▰').count();
        assert!(filled == 2 || filled == 3, "got {} filled in {}", filled, s);
    }

    #[test]
    fn color_for_thresholds() {
        let g = color_for(0.1);
        let a = color_for(0.7);
        let r = color_for(0.95);
        assert!(g.contains("32"));
        assert!(a.contains("33"));
        assert!(r.contains("31"));
        assert_eq!(color_for(0.59), g);
        assert_eq!(color_for(0.60), a);
        assert_eq!(color_for(0.85), r);
    }

    #[test]
    fn format_reset_picks_right_unit() {
        assert_eq!(format_reset(30), "0m"); // sub-minute rounds down
        assert_eq!(format_reset(90), "1m");
        assert_eq!(format_reset(3600), "1h");
        assert_eq!(format_reset(86_400), "1d");
        assert_eq!(format_reset(2 * 86_400), "2d");
    }

    #[test]
    fn quota_suffix_dispatches_on_stale_flag() {
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        w.ends_at = Some(now + chrono::Duration::hours(2));
        assert!(quota_suffix(&w, now).contains("2h"));
        w.stale = true;
        assert!(quota_suffix(&w, now).contains("⚠"));
        // Stale wins over countdown.
        assert!(!quota_suffix(&w, now).contains("2h"));
    }

    #[test]
    fn colored_bar_plain_mode_matches_unicode_bar() {
        // No-color output must match the original plain ▰/▱ shape so
        // anyone piping to grep / a file isn't surprised by escape
        // codes — and a purple pace marker would just look like an
        // extra unit of usage.
        let plain = colored_bar(0.4, 10, Some(7), /*use_color*/ false);
        assert_eq!(plain, unicode_bar(0.4, 10));
    }

    #[test]
    fn colored_bar_paints_fill_with_threshold_color() {
        // 40 % usage → green ramp. ANSI green is \x1b[32m.
        let s = colored_bar(0.4, 10, None, true);
        assert!(s.contains("\x1b[32m"), "expected green: {:?}", s);
        // 70 % → amber.
        let s = colored_bar(0.7, 10, None, true);
        assert!(s.contains("\x1b[33m"), "expected amber: {:?}", s);
        // 95 % → red.
        let s = colored_bar(0.95, 10, None, true);
        assert!(s.contains("\x1b[31m"), "expected red: {:?}", s);
    }

    #[test]
    fn colored_bar_paints_pace_index_in_purple() {
        // A 40 % bar with pace at index 7 should have one purple pip
        // beyond the filled region.
        let s = colored_bar(0.4, 10, Some(7), true);
        assert!(s.contains("\x1b[35m"), "expected magenta pace marker: {:?}", s);
        // Pace pip in unfilled region keeps the `▰` glyph so it's
        // visible against the `▱` background — and the unfilled
        // count is one less than it would be without pace.
        assert_eq!(s.matches('▰').count(), 4 + 1, "got {:?}", s);
        assert_eq!(s.matches('▱').count(), 10 - 4 - 1, "got {:?}", s);
    }

    #[test]
    fn colored_bar_pace_inside_fill_region_still_renders_purple() {
        // When pace is below the fill level, the marker still wins:
        // we want the user to see "here's where time says you'd be"
        // even when they're ahead of pace.
        let s = colored_bar(0.8, 10, Some(3), true);
        assert!(s.contains("\x1b[35m"));
        // Pip count is unchanged because the marker swaps for a
        // would-be fill pip.
        assert_eq!(s.matches('▰').count(), 8);
    }

    #[test]
    fn pace_index_unknown_label_returns_none() {
        let mut w = WindowUsage::default();
        w.ends_at = Some(chrono::Utc::now() + chrono::Duration::hours(2));
        assert!(pace_index("today", &w, chrono::Utc::now(), 10).is_none());
        assert!(pace_index("month", &w, chrono::Utc::now(), 10).is_none());
        assert!(pace_index("1h", &w, chrono::Utc::now(), 10).is_none());
    }

    #[test]
    fn pace_index_5h_window_maps_elapsed_fraction_to_cell() {
        // ends_at = now + 1h means 4h of the 5h window have elapsed →
        // pace fraction 0.8 → cell index 7 in a 10-cell bar.
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        w.ends_at = Some(now + chrono::Duration::hours(1));
        let idx = pace_index("5h", &w, now, 10).unwrap();
        assert_eq!(idx, 7, "got {idx}");
    }

    #[test]
    fn pace_index_week_variants_all_use_seven_day_window() {
        // The Sonnet- and Opus-specific weekly windows share the same
        // duration as plain "week"; if a refactor accidentally drops
        // one of them we want a test failure, not a silent missing
        // pace marker.
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        w.ends_at = Some(now + chrono::Duration::days(2));
        for label in ["week", "week (Sonnet)", "week (Opus)"] {
            let idx = pace_index(label, &w, now, 10).expect(label);
            // 5 of 7 days elapsed → ~0.71 → cell 6.
            assert_eq!(idx, 6, "{label}: got {idx}");
        }
    }

    #[test]
    fn pace_index_past_ends_at_pins_to_last_cell() {
        // Stale snapshot (resets_at lapsed). The pace marker should
        // sit at cell `cells - 1`, not wrap or panic.
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        w.ends_at = Some(now - chrono::Duration::hours(1));
        assert_eq!(pace_index("5h", &w, now, 10).unwrap(), 9);
    }

    #[test]
    fn format_quota_row_keeps_fraction_and_warns_when_stale() {
        let mut w = WindowUsage::default();
        w.fraction_used = Some(1.0);
        w.ends_at = Some(chrono::Utc::now() + chrono::Duration::hours(2));
        w.stale = true;
        let s = format_quota_row("5h", &w, /*use_color*/ false);
        assert!(s.contains("100%"), "expected fraction kept: {}", s);
        assert!(s.contains("⚠"), "expected stale marker: {}", s);
    }

    #[test]
    fn window_order_matches_dashboard() {
        let mut labels = vec!["month", "1h", "week", "5h", "today"];
        labels.sort_by_key(|l| window_order(l));
        assert_eq!(labels, vec!["5h", "week", "1h", "today", "month"]);
    }

    fn make_snapshot(provider: ProviderId) -> UsageSnapshot {
        let mut snap = UsageSnapshot {
            provider,
            timestamp: chrono::Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: Some("Plus".into()),
        };
        snap.windows.insert(
            "5h".into(),
            WindowUsage {
                fraction_used: Some(0.40),
                ends_at: Some(chrono::Utc::now() + chrono::Duration::hours(3)),
                ..Default::default()
            },
        );
        snap.windows.insert(
            "week".into(),
            WindowUsage {
                fraction_used: Some(0.20),
                ends_at: Some(chrono::Utc::now() + chrono::Duration::days(2)),
                ..Default::default()
            },
        );
        snap
    }

    #[test]
    fn build_screen_includes_header_bars_and_footer() {
        let mut snaps = BTreeMap::new();
        snaps.insert(ProviderId::CodexCli, make_snapshot(ProviderId::CodexCli));
        let s = build_screen(&snaps, /*use_color*/ false, /*clear*/ false);
        assert!(s.contains("Codex"), "expected provider name in: {}", s);
        assert!(s.contains("Plus"), "expected plan tag in: {}", s);
        assert!(s.contains("5h"), "expected window label in: {}", s);
        assert!(s.contains("week"), "expected weekly label in: {}", s);
        assert!(s.contains("last refreshed"), "expected footer in: {}", s);
        // No trailing newline (cursor parks on the footer line).
        assert!(!s.ends_with('\n'), "got trailing newline in: {:?}", s.chars().last());
    }

    #[test]
    fn build_screen_empty_shows_waiting() {
        let snaps: BTreeMap<ProviderId, UsageSnapshot> = BTreeMap::new();
        let s = build_screen(&snaps, false, false);
        assert!(s.contains("waiting for first snapshot"));
    }

    #[test]
    fn build_screen_shows_activity_only_providers_via_headline() {
        // Provider with no fraction_used but with token activity now
        // gets rendered as a header + headline row instead of being
        // skipped — matches the tray menu's behaviour.
        let mut snap = make_snapshot(ProviderId::Anthropic);
        snap.windows.clear();
        snap.windows.insert(
            "today".into(),
            WindowUsage {
                tokens_in: 100,
                ..Default::default()
            },
        );
        snap.headline = Some("100 tokens in today".into());
        let mut snaps = BTreeMap::new();
        snaps.insert(ProviderId::Anthropic, snap);
        let s = build_screen(&snaps, false, false);
        assert!(s.contains("Anthropic"), "got: {}", s);
        assert!(s.contains("100 tokens in today"), "got: {}", s);
        assert!(!s.contains("waiting for first snapshot"), "got: {}", s);
    }

    #[test]
    fn build_screen_skips_providers_with_no_data_at_all() {
        let mut snap = make_snapshot(ProviderId::Anthropic);
        snap.windows.clear();
        snap.headline = None;
        let mut snaps = BTreeMap::new();
        snaps.insert(ProviderId::Anthropic, snap);
        let s = build_screen(&snaps, false, false);
        assert!(s.contains("waiting for first snapshot"), "got: {}", s);
    }

    #[test]
    fn build_screen_with_color_includes_ansi_clear_when_requested() {
        let mut snaps = BTreeMap::new();
        snaps.insert(ProviderId::CodexCli, make_snapshot(ProviderId::CodexCli));
        let with_clear = build_screen(&snaps, /*color*/ true, /*clear*/ true);
        let without_clear = build_screen(&snaps, /*color*/ true, /*clear*/ false);
        // Clear sequence at the top of one but not the other.
        assert!(with_clear.starts_with("\x1b[2J\x1b[H"));
        assert!(!without_clear.starts_with("\x1b[2J\x1b[H"));
    }
}
