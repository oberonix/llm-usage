//! Tray-icon binary. Wires the core providers, the quota engine, native
//! notifications and a click-menu showing the latest snapshot of each provider.
//!
//! macOS: NSStatusItem, no Dock icon (see Info.plist LSUIElement=true at packaging).
//! Linux: StatusNotifierItem via tray-icon's gtk backend.

mod icon;
mod runtime;

use anyhow::Result;
use chrono::{DateTime, Utc};
use llm_usage_core::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowUsage};
use llm_usage_core::updates::UpdateInfo;
use llm_usage_core::Config;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::sync::Notify;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::runtime::{RuntimeHandle, RuntimeMessage};

/// Order providers iterate in when the icon rotates. Matches the menu order.
const PROVIDER_ORDER: [ProviderId; 3] = [
    ProviderId::Anthropic,
    ProviderId::CodexCli,
    ProviderId::OllamaCloud,
];

/// Lower bound to keep a hand-edited config from making the icon flicker.
const MIN_ROTATION_SECS: u64 = 5;

fn rotation_interval_from(cfg: &Config) -> Duration {
    Duration::from_secs(cfg.icon_rotation_secs.max(MIN_ROTATION_SECS))
}

const DASHBOARD_ID: &str = "dashboard";
const SETTINGS_ID: &str = "settings";
const HELP_ID: &str = "help";
const UPDATE_ID: &str = "update";
const QUIT_ID: &str = "quit";

fn main() -> Result<()> {
    // Default filter matches the dashboard binary. Set RUST_LOG to
    // raise verbosity per-crate when triaging a bug — e.g.
    //   RUST_LOG=info,llm_usage=debug,llm_usage_core=debug
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load_or_default()?;

    #[cfg(target_os = "linux")]
    {
        gtk::init().map_err(|e| anyhow::anyhow!("gtk init failed: {}", e))?;
    }

    let event_loop = EventLoopBuilder::new().build();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(build_menu(&BTreeMap::new(), None)))
        .with_tooltip("llm-usage — waiting for first poll")
        .with_icon(icon::render_placeholder())
        // Left-click spawns the popup window instead of opening the
        // native menu. Right-click still shows the menu.
        .with_menu_on_left_click(false)
        .build()?;

    let refresh = Arc::new(Notify::new());
    let reload = Arc::new(Notify::new());

    // Runtime thread: polls providers, sends snapshots/alerts via mpsc.
    let (tx, rx) = std::sync::mpsc::channel::<RuntimeMessage>();
    let cfg_clone = config.clone();
    let refresh_clone = refresh.clone();
    let reload_clone = reload.clone();
    std::thread::Builder::new()
        .name("llm-usage-runtime".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(runtime::run(
                cfg_clone,
                RuntimeHandle {
                    refresh: refresh_clone,
                    reload: reload_clone,
                },
                tx,
            ));
        })?;

    // File watchers: signal `reload` on config writes, `refresh` on
    // dashboard-triggered refresh.trigger writes, and `refresh` again
    // whenever a local data source (Claude Code JSONL, Codex rollout,
    // opencode SQLite) changes. Hold all for the lifetime of the
    // event loop; dropping cancels the subscription.
    let _config_watcher = spawn_config_watcher(reload.clone());
    let _trigger_watcher = spawn_refresh_trigger_watcher(refresh.clone());
    let _data_watchers = spawn_data_source_watchers(&config, refresh.clone());

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    let mut current_snapshots: BTreeMap<ProviderId, UsageSnapshot> = BTreeMap::new();
    let mut active_idx: usize = 0;
    let mut last_rotation = Instant::now();
    let mut rotation_interval = rotation_interval_from(&config);
    let mut show_pace_marker = config.show_pace_marker;
    let mut latest_update: Option<UpdateInfo> = None;

    event_loop.run(move |_event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(250),
        );

        // Rotate to the next quota-bearing provider on the configured
        // cadence. If there's only one (or none) eligible, this is a
        // no-op aside from a re-render against whatever fresh data we
        // have.
        if last_rotation.elapsed() >= rotation_interval {
            active_idx = active_idx.wrapping_add(1);
            last_rotation = Instant::now();
            refresh_icon(&tray, &current_snapshots, &mut active_idx, show_pace_marker);
        }

        // Drain runtime messages.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                RuntimeMessage::Snapshots(snaps) => {
                    current_snapshots = snaps;
                    let new_menu = build_menu(&current_snapshots, latest_update.as_ref());
                    tray.set_menu(Some(Box::new(new_menu)));
                    refresh_icon(&tray, &current_snapshots, &mut active_idx, show_pace_marker);
                }
                RuntimeMessage::Alert(message) => {
                    let _ = notify_rust::Notification::new()
                        .summary("LLM usage alert")
                        .body(&message)
                        .timeout(notify_rust::Timeout::Milliseconds(8000))
                        .show();
                }
                RuntimeMessage::ConfigReloaded => {
                    // Pick up the rotation interval / pace marker
                    // toggle the user may have changed in Settings.
                    // Falls back to the in-flight values if the file
                    // can't be re-read.
                    if let Ok(new_cfg) = Config::load_or_default() {
                        rotation_interval = rotation_interval_from(&new_cfg);
                        show_pace_marker = new_cfg.show_pace_marker;
                    }
                    refresh_icon(&tray, &current_snapshots, &mut active_idx, show_pace_marker);
                    let _ = notify_rust::Notification::new()
                        .summary("llm-usage")
                        .body("Config reloaded")
                        .timeout(notify_rust::Timeout::Milliseconds(2500))
                        .show();
                }
                RuntimeMessage::ConfigReloadFailed(err) => {
                    let _ = notify_rust::Notification::new()
                        .summary("llm-usage — config reload failed")
                        .body(&err)
                        .timeout(notify_rust::Timeout::Milliseconds(6000))
                        .show();
                }
                RuntimeMessage::UpdateAvailable(info) => {
                    let first_time = latest_update.as_ref() != Some(&info);
                    let body = format!("v{} is available — click the tray menu", info.version);
                    latest_update = Some(info);
                    // Rebuild the menu so the new banner appears
                    // immediately, not just on the next Snapshots tick.
                    let new_menu =
                        build_menu(&current_snapshots, latest_update.as_ref());
                    tray.set_menu(Some(Box::new(new_menu)));
                    // One-time native notification when the version
                    // first changes, so users notice without having to
                    // open the menu themselves.
                    if first_time {
                        let _ = notify_rust::Notification::new()
                            .summary("llm-usage update available")
                            .body(&body)
                            .timeout(notify_rust::Timeout::Milliseconds(6000))
                            .show();
                    }
                }
            }
        }

        if let Ok(menu_event) = menu_channel.try_recv() {
            match menu_event.id.0.as_str() {
                QUIT_ID => *control_flow = ControlFlow::Exit,
                DASHBOARD_ID => spawn_dashboard(&[]),
                SETTINGS_ID => spawn_dashboard(&["--tab=settings"]),
                HELP_ID => spawn_dashboard(&["--tab=help"]),
                UPDATE_ID => {
                    if let Some(info) = &latest_update {
                        open_url(&info.url);
                    }
                }
                _ => {}
            }
        }

        if let Ok(TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }) = tray_channel.try_recv()
        {
            // Spawn (or focus, if already running) the popup
            // window with the graphical quota view.
            spawn_dashboard(&["--popup"]);
        }
    });
}

/// Rebuild the tray menu from the current snapshots. Each provider
/// gets a header row carrying the name + plan tag (`Anthropic · Max 5x`)
/// followed by one informational (disabled) row per quota-bearing
/// window with a Unicode block bar: `▰▰▰▱▱▱▱▱ 35% week (resets 2d)`.
/// No separator between providers — the bare header row visually
/// delineates them and keeps the lines tighter together.
fn build_menu(
    snapshots: &BTreeMap<ProviderId, UsageSnapshot>,
    update: Option<&UpdateInfo>,
) -> Menu {
    let menu = Menu::new();

    // Update banner at the top so the user sees it before anything
    // else when they open the menu. Clickable; opens the release page.
    if let Some(info) = update {
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(UPDATE_ID),
            format!("Update available: v{} →", info.version),
            true,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    let mut printed_a_provider = false;

    for id in PROVIDER_ORDER {
        let Some(snap) = snapshots.get(&id) else {
            continue;
        };
        if matches!(snap.status, ProviderStatus::Unavailable) {
            continue;
        }
        let mut quota_windows: Vec<(&String, &WindowUsage)> = snap
            .windows
            .iter()
            .filter(|(label, w)| {
                // Keep this filter aligned with `menu_window_order`. The
                // per-model weekly buckets (Sonnet, Opus) live in the
                // full dashboard; the tray menu shows just the
                // all-models 5h and weekly rows so the menu stays narrow.
                // (Pre-rename these were "week (Sonnet)" / "week (Opus)";
                // commit 83a8286 dropped the "week (" prefix without
                // updating this filter, which silently leaked four-row
                // menus instead of two-row ones into the tray and may
                // have been what made the menu unscrollable on smaller
                // panels.)
                w.fraction_used.is_some()
                    && !matches!(label.as_str(), "Sonnet" | "Opus")
            })
            .collect();
        let has_activity = snap
            .windows
            .values()
            .any(|w| w.request_count > 0 || w.tokens_in > 0 || w.tokens_out > 0);
        // Skip only when the provider has nothing to say at all — no
        // quota fractions AND no activity counts. Providers with just
        // activity (e.g. Codex when rate_limits is stale but opencode
        // captured turns) still get a header + a one-line summary.
        if quota_windows.is_empty() && !has_activity {
            continue;
        }
        quota_windows.sort_by_key(|(label, _)| menu_window_order(label.as_str()));
        printed_a_provider = true;

        let plan = snap
            .plan_label
            .as_deref()
            .map(|p| format!(" · {}", p))
            .unwrap_or_default();
        // Single ⚠ next to the provider name when any window is
        // showing cached data — replaces the per-row markers. The
        // gray-shifted bars below still tell the user which rows
        // specifically are stale.
        let stale_mark = if snap.windows.values().any(|w| w.stale) {
            format!(" {}", llm_usage_core::model::STALE_MARKER)
        } else {
            String::new()
        };
        let header = format!("{}{}{}", snap.provider.human(), plan, stale_mark);
        let _ = menu.append(&MenuItem::new(header, false, None));

        if !quota_windows.is_empty() {
            for (label, w) in &quota_windows {
                let text = format_quota_row(label, w);
                let _ = menu.append(&MenuItem::new(text, false, None));
            }
        } else if let Some(h) = &snap.headline {
            // Activity-only fall-through. The provider's own headline
            // is the best summary it can give us (e.g. "0 turns / 5h ·
            // 51 turns / 7d") — render it as one indented row so the
            // visual structure still matches the quota-row case.
            let _ = menu.append(&MenuItem::new(format!("  {}", h), false, None));
        }
    }

    if !printed_a_provider {
        let placeholder = MenuItem::new("No quota data yet…", false, None);
        let _ = menu.append(&placeholder);
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(DASHBOARD_ID),
        "Open dashboard",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(SETTINGS_ID),
        "Settings",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(HELP_ID),
        "Help",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(QUIT_ID),
        "Quit",
        true,
        None,
    ));
    menu
}

/// Mirror of the dashboard's window_order so the menu rows and the
/// dashboard cards present windows in the same order.
fn menu_window_order(label: &str) -> u32 {
    match label {
        "5h" => 10,
        "week" => 20,
        "Sonnet" => 21,
        "Opus" => 22,
        _ => 50,
    }
}

fn format_quota_row(label: &str, w: &WindowUsage) -> String {
    let frac = w.fraction_used.unwrap_or(0.0);
    // 10-cell bar — each cell = 10% so the visual count maps directly
    // onto the percentage next to it (▰▰▰▱▱▱▱▱▱▱ → 30%).
    let bar = unicode_bar(frac, 10);
    let pct = format!("{:>3.0}%", frac * 100.0);
    let suffix = quota_suffix(w, chrono::Utc::now());
    format!("{} {} · {}{}", bar, pct, label, suffix)
}

// Pick the trailing tokens on a quota row. Just the reset
// countdown — the staleness ⚠ lives on the provider header now,
// so per-row markers would just clutter.
//
//   fresh + future ends_at  → " · 2h"
//   stale + future ends_at  → " · 2h"
//   stale + past   ends_at  → " · 0m"   (should-have-rolled,
//                                          fresh data due
//                                          any moment)
//   otherwise               → ""
fn quota_suffix(w: &WindowUsage, now: DateTime<Utc>) -> String {
    let Some(t) = w.ends_at else {
        return String::new();
    };
    let secs = (t - now).num_seconds().max(0);
    if secs == 0 && !w.stale {
        return String::new();
    }
    format!(" · {}", format_reset(secs))
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
    use llm_usage_core::model::WindowUsage;

    #[test]
    fn unicode_bar_zero_full_and_partial() {
        assert_eq!(unicode_bar(0.0, 10), "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(unicode_bar(1.0, 10), "▰▰▰▰▰▰▰▰▰▰");
        assert_eq!(unicode_bar(0.5, 10), "▰▰▰▰▰▱▱▱▱▱");
    }

    #[test]
    fn format_reset_units() {
        assert_eq!(format_reset(30 * 60), "30m");
        assert_eq!(format_reset(2 * 3600), "2h");
        assert_eq!(format_reset(3 * 86_400), "3d");
    }

    #[test]
    fn menu_window_order_lifts_quota_above_activity() {
        let mut labels = vec!["Sonnet", "5h", "month", "week"];
        labels.sort_by_key(|l| menu_window_order(l));
        // Unknown labels (50) come AFTER quota but BEFORE activity (100+).
        assert_eq!(labels, vec!["5h", "week", "Sonnet", "month"]);
    }

    #[test]
    fn format_quota_row_includes_bar_pct_label_and_reset() {
        let mut w = WindowUsage::default();
        w.fraction_used = Some(0.42);
        w.ends_at = Some(chrono::Utc::now() + chrono::Duration::hours(3));
        let s = format_quota_row("5h", &w);
        assert!(s.contains("5h"), "got {}", s);
        assert!(s.contains("42%"), "got {}", s);
        // "· " separator before label, before reset → at least 2.
        let dots = s.matches('·').count();
        assert!(dots >= 2, "expected ≥2 separator dots, got: {}", s);
    }

    #[test]
    fn format_quota_row_stale_keeps_countdown_no_per_row_marker() {
        // Stale rows show fraction + countdown like fresh rows. The
        // provider-header ⚠ (added in `build_menu`) is the
        // disconnected-state cue; per-row markers would clutter.
        let mut w = WindowUsage::default();
        w.fraction_used = Some(1.0);
        w.ends_at = Some(chrono::Utc::now() + chrono::Duration::minutes(185));
        w.stale = true;
        let s = format_quota_row("5h", &w);
        assert!(s.contains("100%"), "got: {}", s);
        assert!(s.contains("3h"), "got: {}", s);
        assert!(!s.contains("⚠"), "row should not carry marker: {}", s);
    }

    #[test]
    fn format_quota_row_no_marker_when_fresh() {
        let mut w = WindowUsage::default();
        w.fraction_used = Some(0.75);
        w.ends_at = Some(chrono::Utc::now() + chrono::Duration::minutes(185));
        let s = format_quota_row("5h", &w);
        assert!(!s.contains("⚠"), "got: {}", s);
        assert!(s.contains("3h"), "got: {}", s);
    }

    #[test]
    fn quota_suffix_countdown_only_no_per_row_marker() {
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        // Fresh, future ends_at → countdown.
        w.ends_at = Some(now + chrono::Duration::hours(2));
        let s = quota_suffix(&w, now);
        assert!(s.contains("2h") && !s.contains("⚠"), "got: {:?}", s);
        // Stale + future ends_at → still just countdown.
        w.stale = true;
        let s = quota_suffix(&w, now);
        assert!(s.contains("2h") && !s.contains("⚠"), "got: {:?}", s);
        // Stale + past ends_at → "0m" floor (fresh data expected).
        w.ends_at = Some(now - chrono::Duration::hours(1));
        let s = quota_suffix(&w, now);
        assert!(s.contains("0m") && !s.contains("⚠"), "got: {:?}", s);
        // No ends_at → empty (whether stale or not).
        w.ends_at = None;
        assert_eq!(quota_suffix(&w, now), "");
        w.stale = false;
        assert_eq!(quota_suffix(&w, now), "");
        // Fresh + past ends_at → empty (we don't second-guess a
        // fresh poll's reset time).
        w.ends_at = Some(now - chrono::Duration::hours(1));
        assert_eq!(quota_suffix(&w, now), "");
    }

    #[test]
    fn format_quota_row_zero_percent_renders_cleanly() {
        let mut w = WindowUsage::default();
        w.fraction_used = Some(0.0);
        let s = format_quota_row("5h", &w);
        assert!(s.contains("0%"), "got {}", s);
        // Bar fully empty.
        assert!(s.starts_with("▱"), "got {}", s);
    }

    // ---- data_source_paths ----
    //
    // The watcher itself is event-loop-driven (best tested by
    // touching files and asserting on `refresh.notify_one`), but the
    // path resolver is pure and easy to pin down.

    #[test]
    fn data_source_paths_includes_default_three_when_no_overrides() {
        let cfg = Config::default();
        let paths = data_source_paths(&cfg);
        let labels: Vec<&str> = paths.iter().map(|(_, _, l)| *l).collect();
        // Order matters in the live code (Claude first, then Codex,
        // then opencode) — the watcher logs by label, so pinning the
        // order keeps the operator's mental model consistent.
        assert_eq!(
            labels,
            vec!["claude_projects", "codex_sessions", "opencode"]
        );
        // Recursion mode: JSONLs nest under project / day dirs so
        // both Claude and Codex need recursive; opencode is one DB
        // file in a flat dir.
        assert!(matches!(paths[0].1, RecursiveMode::Recursive));
        assert!(matches!(paths[1].1, RecursiveMode::Recursive));
        assert!(matches!(paths[2].1, RecursiveMode::NonRecursive));
    }

    #[test]
    fn data_source_paths_respects_config_overrides() {
        // Custom project / codex paths must flow into the watcher
        // so a user running Claude Code from a non-default $HOME
        // (sandbox, dev container, etc.) still gets fast refresh.
        let mut cfg = Config::default();
        cfg.anthropic.claude_projects_dir = Some(std::path::PathBuf::from("/tmp/custom-claude"));
        cfg.codex_cli.codex_dir = Some(std::path::PathBuf::from("/tmp/custom-codex"));
        let paths = data_source_paths(&cfg);
        assert!(paths
            .iter()
            .any(|(p, _, l)| *l == "claude_projects" && p == &std::path::PathBuf::from("/tmp/custom-claude")));
        // Codex appends `/sessions` to the configured codex_dir.
        assert!(paths.iter().any(|(p, _, l)| *l == "codex_sessions"
            && p == &std::path::PathBuf::from("/tmp/custom-codex/sessions")));
    }

    #[test]
    fn data_source_paths_drops_opencode_when_disabled() {
        // Empty-string opencode_db is the documented "disable"
        // signal — see `Config::resolve_opencode_db`. The watcher
        // must not subscribe to a non-existent path.
        let mut cfg = Config::default();
        cfg.opencode_db = Some(std::path::PathBuf::new());
        let paths = data_source_paths(&cfg);
        assert!(paths.iter().all(|(_, _, l)| *l != "opencode"));
        // The other two are still present.
        assert!(paths.iter().any(|(_, _, l)| *l == "claude_projects"));
        assert!(paths.iter().any(|(_, _, l)| *l == "codex_sessions"));
    }

    #[test]
    fn is_meaningful_data_path_accepts_jsonl_and_opencode_db() {
        use std::path::PathBuf;
        assert!(is_meaningful_data_path(&PathBuf::from(
            "/home/u/.claude/projects/foo/session-abc.jsonl"
        )));
        assert!(is_meaningful_data_path(&PathBuf::from(
            "/home/u/.codex/sessions/2026/05/12/rollout-x.jsonl"
        )));
        assert!(is_meaningful_data_path(&PathBuf::from(
            "/home/u/.local/share/opencode/opencode.db"
        )));
    }

    #[test]
    fn is_meaningful_data_path_rejects_sqlite_sidecars() {
        // These are the events that caused the 92 % CPU pin: opencode
        // commits its WAL several times per assistant turn, and a
        // watcher that treats those as "interesting" feeds the
        // runtime an event storm. The whole `-wal` + `-shm` family
        // must filter out.
        use std::path::PathBuf;
        let sidecars = [
            "/home/u/.local/share/opencode/opencode.db-wal",
            "/home/u/.local/share/opencode/opencode.db-shm",
            "/home/u/.local/share/opencode/opencode.db-journal",
        ];
        for p in sidecars {
            assert!(
                !is_meaningful_data_path(&PathBuf::from(p)),
                "should reject {}",
                p
            );
        }
    }

    #[test]
    fn is_meaningful_data_path_rejects_misc_noise() {
        use std::path::PathBuf;
        // A vim swapfile next to a JSONL; an editor backup. Neither
        // is upstream activity worth re-polling for.
        assert!(!is_meaningful_data_path(&PathBuf::from(
            "/home/u/.claude/projects/foo/.session-abc.jsonl.swp"
        )));
        assert!(!is_meaningful_data_path(&PathBuf::from(
            "/home/u/.claude/projects/foo/session-abc.jsonl~"
        )));
    }

    #[test]
    fn data_source_paths_uses_opencode_parent_dir() {
        // Watcher attaches to the *parent* of `opencode.db` so it
        // catches writes to `opencode.db`, `opencode.db-wal`, and
        // `opencode.db-shm` in one subscription.
        let mut cfg = Config::default();
        cfg.opencode_db = Some(std::path::PathBuf::from("/tmp/x/y/opencode.db"));
        let paths = data_source_paths(&cfg);
        let opencode = paths
            .iter()
            .find(|(_, _, l)| *l == "opencode")
            .expect("opencode entry present");
        assert_eq!(opencode.0, std::path::PathBuf::from("/tmp/x/y"));
    }
}

/// Watch the config file for writes and signal the runtime to reload.
///
/// We watch the *parent directory* (non-recursive) rather than the file
/// itself: many editors do atomic saves (write to .tmp, rename over the
/// target), and a watcher attached directly to the file would lose its
/// subscription after the rename. Returns the watcher; caller must hold
/// it in scope for the watcher to stay live.
fn spawn_config_watcher(reload: Arc<Notify>) -> Option<RecommendedWatcher> {
    let config_path = llm_usage_core::config::config_path().ok()?;
    let parent = config_path.parent()?.to_path_buf();
    if let Err(e) = std::fs::create_dir_all(&parent) {
        tracing::warn!(error = %e, "could not ensure config dir for watcher");
    }

    // Coalesce the burst of events a single save typically produces into one
    // reload signal. notify v7 fires create/modify/close events on a write,
    // so without a debounce we'd reload three times in quick succession.
    let last_fire = Arc::new(std::sync::Mutex::new(Instant::now() - Duration::from_secs(60)));
    let target = config_path.clone();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "config watcher error");
                return;
            }
        };
        let interesting = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        );
        if !interesting {
            return;
        }
        if !event.paths.iter().any(|p| p == &target) {
            return;
        }
        let now = Instant::now();
        let mut guard = last_fire.lock().expect("poisoned");
        if now.duration_since(*guard) < Duration::from_millis(250) {
            return;
        }
        *guard = now;
        reload.notify_one();
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "could not start config watcher (live-reload disabled)");
            return None;
        }
    };

    if let Err(e) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
        tracing::warn!(error = %e, path = %parent.display(), "config watcher subscribe failed");
        return None;
    }
    tracing::info!(path = %config_path.display(), "config watcher live");
    Some(watcher)
}

/// Watch `<data_dir>/refresh.trigger` for writes; signal the runtime to
/// poll immediately. Same atomic-rename-aware approach as the config
/// watcher: we attach to the parent directory non-recursively so we
/// keep getting events through atomic saves.
fn spawn_refresh_trigger_watcher(refresh: Arc<Notify>) -> Option<RecommendedWatcher> {
    let trigger_path = llm_usage_core::config::refresh_trigger_path().ok()?;
    let parent = trigger_path.parent()?.to_path_buf();
    if let Err(e) = std::fs::create_dir_all(&parent) {
        tracing::warn!(error = %e, "could not ensure data dir for refresh watcher");
    }

    let last_fire = Arc::new(std::sync::Mutex::new(Instant::now() - Duration::from_secs(60)));
    let target = trigger_path.clone();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "refresh watcher error");
                return;
            }
        };
        let interesting = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_)
        );
        if !interesting {
            return;
        }
        if !event.paths.iter().any(|p| p == &target) {
            return;
        }
        let now = Instant::now();
        let mut guard = last_fire.lock().expect("poisoned");
        if now.duration_since(*guard) < Duration::from_millis(250) {
            return;
        }
        *guard = now;
        refresh.notify_one();
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "could not start refresh trigger watcher");
            return None;
        }
    };

    if let Err(e) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
        tracing::warn!(error = %e, path = %parent.display(), "refresh watcher subscribe failed");
        return None;
    }
    tracing::info!(path = %trigger_path.display(), "refresh trigger watcher live");
    Some(watcher)
}

/// Watch the local data sources upstream of each provider (Claude
/// Code JSONLs, Codex CLI rollouts, opencode SQLite) and signal the
/// runtime's `refresh` whenever they change. This is what lets a
/// `codex` invocation surface in the tray within a second or two
/// rather than at the next 15-minute poll boundary.
///
/// Each watcher path is best-effort: if the directory doesn't exist
/// on this machine, or if inotify subscription fails (per-user
/// watch limit, missing kernel support, etc.), we log at WARN and
/// skip that source. The remaining sources still work.
///
/// The HTTP-throttle in each provider keeps refresh storms (Claude
/// Code can write a JSONL on every assistant turn) from translating
/// into per-event upstream calls. See `MIN_HTTP_INTERVAL` in
/// `crates/core/src/providers/anthropic.rs` and `ollama_cloud.rs`.
fn spawn_data_source_watchers(
    config: &Config,
    refresh: Arc<Notify>,
) -> Vec<RecommendedWatcher> {
    let mut out = Vec::new();
    let paths = data_source_paths(config);
    // Shared debounce: any source can fire it. 5 s is the floor we
    // arrived at after a 92 % CPU pin in heavy Claude Code use —
    // opencode's SQLite WAL writes constantly during an active
    // session, and a tight debounce was kicking the runtime into a
    // continuous poll → re-walk-76MB-of-JSONLs loop. The HTTP
    // throttles in the providers add a second line of defence (5 min
    // for Anthropic OAuth, 60 s for Ollama scrape) so this debounce
    // can be relatively generous without making the UI feel laggy:
    // the first event in a quiet period still fires immediately,
    // we just coalesce bursts.
    let last_fire = Arc::new(std::sync::Mutex::new(Instant::now() - Duration::from_secs(60)));
    for (path, mode, label) in paths {
        if !path.exists() {
            tracing::debug!(path = %path.display(), source = label, "data source dir absent; not watching");
            continue;
        }
        let refresh_ = refresh.clone();
        let last_fire_ = last_fire.clone();
        let path_ = path.clone();
        let label_owned = label.to_string();
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let event = match res {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::warn!(error = %err, source = %label_owned, "data watcher error");
                        return;
                    }
                };
                let interesting =
                    matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
                if !interesting {
                    return;
                }
                // Filter the event paths to ones that actually
                // signal "interesting upstream activity". The
                // standout chatter source is opencode's SQLite
                // sidecars — `.db-wal` and `.db-shm` get written on
                // every transaction commit, far more often than the
                // logical `opencode.db` rolls over.
                if !event.paths.iter().any(|p| is_meaningful_data_path(p)) {
                    return;
                }
                let now = Instant::now();
                let mut guard = last_fire_.lock().expect("poisoned");
                if now.duration_since(*guard) < Duration::from_secs(5) {
                    return;
                }
                *guard = now;
                tracing::debug!(
                    path = %path_.display(),
                    source = %label_owned,
                    "data source changed; firing fast-refresh",
                );
                refresh_.notify_one();
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(error = %e, source = label, "data watcher start failed");
                    continue;
                }
            };
        if let Err(e) = watcher.watch(&path, mode) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                source = label,
                "data watcher subscribe failed (inotify limit? \
                 see sysctl fs.inotify.max_user_watches on Linux)",
            );
            continue;
        }
        tracing::info!(path = %path.display(), source = label, "data source watcher live");
        out.push(watcher);
    }
    out
}

/// True when a filesystem event path is one of the file types our
/// providers actually consume — `*.jsonl` (Claude Code projects,
/// Codex rollouts) or the literal `opencode.db`. SQLite's `-wal`
/// and `-shm` sidecars and any other detritus in those dirs are
/// excluded: opencode commits to its WAL many times per assistant
/// turn, and a watcher that fires on every WAL write keeps the
/// runtime in a constant re-poll → re-walk-JSONLs loop (92 % CPU
/// observed under heavy Claude Code use).
fn is_meaningful_data_path(p: &std::path::Path) -> bool {
    if p.extension().is_some_and(|ext| ext == "jsonl") {
        return true;
    }
    // The opencode SQLite file proper is interesting; its WAL /
    // SHM siblings are not.
    matches!(p.file_name().and_then(|n| n.to_str()), Some("opencode.db"))
}

/// The three (or fewer, depending on what's installed) local paths
/// we watch for "something happened, re-poll the providers" signals.
/// Each entry is (path, recursion mode, log-friendly label).
fn data_source_paths(
    config: &Config,
) -> Vec<(std::path::PathBuf, RecursiveMode, &'static str)> {
    let mut out = Vec::new();

    // Anthropic JSONLs — config override or ~/.claude/projects.
    let claude_projects = config
        .anthropic
        .claude_projects_dir
        .clone()
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude").join("projects")));
    if let Some(p) = claude_projects {
        out.push((p, RecursiveMode::Recursive, "claude_projects"));
    }

    // Codex rollouts — config override or ~/.codex/sessions.
    let codex_sessions = config
        .codex_cli
        .codex_dir
        .clone()
        .map(|d| d.join("sessions"))
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex").join("sessions")));
    if let Some(p) = codex_sessions {
        out.push((p, RecursiveMode::Recursive, "codex_sessions"));
    }

    // opencode SQLite — watch the parent dir non-recursively so we
    // catch writes to `opencode.db` and its `-wal` / `-shm` siblings.
    if let Some(db) = config.resolve_opencode_db() {
        if let Some(parent) = db.parent() {
            out.push((parent.to_path_buf(), RecursiveMode::NonRecursive, "opencode"));
        }
    }

    out
}

/// Open a URL in the user's default browser. `xdg-open` on Linux,
/// `open` on macOS, `start` (via cmd) on Windows. Best-effort — we
/// don't surface errors, the user can always copy the URL from the
/// notification body.
fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "start";
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

fn spawn_dashboard(args: &[&str]) {
    let candidate = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("llm-usage-dashboard")));
    let cmd = candidate
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "llm-usage-dashboard".to_string());
    // Log spawn failures so the user has a fighting chance of
    // diagnosing "the menu click did nothing". Most commonly this
    // hits when the user moves the tray binary out of the sibling
    // directory of the dashboard binary — e.g. `cargo install
    // llm-usage-tray` without also installing the dashboard.
    if let Err(e) = std::process::Command::new(&cmd).args(args).spawn() {
        tracing::warn!(
            command = %cmd, error = %e,
            "failed to spawn dashboard — is llm-usage-dashboard installed alongside llm-usage-tray?"
        );
    }
}

/// Repaint the tray icon and update its tooltip for whichever
/// quota-bearing provider is currently in the rotation slot. Skips
/// providers with no fraction data — rotation lands only on cards
/// the icon can meaningfully draw.
fn refresh_icon(
    tray: &TrayIcon,
    snapshots: &BTreeMap<ProviderId, UsageSnapshot>,
    active_idx: &mut usize,
    show_pace_marker: bool,
) {
    let eligible: Vec<ProviderId> = PROVIDER_ORDER
        .iter()
        .copied()
        .filter(|id| {
            snapshots
                .get(id)
                .is_some_and(|s| !matches!(s.status, ProviderStatus::Unavailable))
                .then_some(())
                .is_some()
                && snapshots.get(id).is_some_and(icon::has_quota_data)
        })
        .collect();

    if eligible.is_empty() {
        let _ = tray.set_icon(Some(icon::render_placeholder()));
        let _ = tray.set_tooltip(Some("llm-usage — no quota data yet"));
        return;
    }

    if *active_idx >= eligible.len() {
        *active_idx = 0;
    }
    let id = eligible[*active_idx];
    let Some(snap) = snapshots.get(&id) else {
        return;
    };
    let (mut session, mut weekly) = icon::pick_bars(snap);
    if !show_pace_marker {
        session.pace = None;
        weekly.pace = None;
    }
    let _ = tray.set_icon(Some(icon::render(id, session, weekly)));
    let headline = snap.headline.as_deref().unwrap_or("");
    let tooltip = if headline.is_empty() {
        id.human().to_string()
    } else {
        format!("{} — {}", id.human(), headline)
    };
    let _ = tray.set_tooltip(Some(&tooltip));
}
