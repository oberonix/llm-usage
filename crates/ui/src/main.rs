//! Tray-icon binary. Wires the core providers, the quota engine, native
//! notifications and a click-menu showing the latest snapshot of each provider.
//!
//! macOS: NSStatusItem, no Dock icon (see Info.plist LSUIElement=true at packaging).
//! Linux: StatusNotifierItem via tray-icon's gtk backend.

mod icon;
mod runtime;

use anyhow::Result;
use llm_usage_core::model::{ProviderId, ProviderStatus, UsageSnapshot};
use llm_usage_core::Config;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::sync::Notify;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::runtime::{render_label, RuntimeHandle, RuntimeMessage};

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
const REFRESH_ID: &str = "refresh";
const QUIT_ID: &str = "quit";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,llm_usage=debug,llm_usage_core=debug".into()),
        )
        .init();

    let config = Config::load_or_default()?;

    #[cfg(target_os = "linux")]
    {
        gtk::init().map_err(|e| anyhow::anyhow!("gtk init failed: {}", e))?;
    }

    let event_loop = EventLoopBuilder::new().build();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(build_menu(&BTreeMap::new())))
        .with_tooltip("llm-usage — waiting for first poll")
        .with_icon(icon::render_placeholder())
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
    // dashboard-triggered refresh.trigger writes. Hold both for the
    // lifetime of the event loop; dropping cancels the subscription.
    let _config_watcher = spawn_config_watcher(reload.clone());
    let _trigger_watcher = spawn_refresh_trigger_watcher(refresh.clone());

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    let mut current_snapshots: BTreeMap<ProviderId, UsageSnapshot> = BTreeMap::new();
    let mut active_idx: usize = 0;
    let mut last_rotation = Instant::now();
    let mut rotation_interval = rotation_interval_from(&config);

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
            refresh_icon(&tray, &current_snapshots, &mut active_idx);
        }

        // Drain runtime messages.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                RuntimeMessage::Snapshots(snaps) => {
                    current_snapshots = snaps;
                    let new_menu = build_menu(&current_snapshots);
                    tray.set_menu(Some(Box::new(new_menu)));
                    refresh_icon(&tray, &current_snapshots, &mut active_idx);
                }
                RuntimeMessage::Alert(message) => {
                    let _ = notify_rust::Notification::new()
                        .summary("LLM usage alert")
                        .body(&message)
                        .timeout(notify_rust::Timeout::Milliseconds(8000))
                        .show();
                }
                RuntimeMessage::ConfigReloaded => {
                    // Pick up the rotation interval the user may have
                    // changed in Settings. Falls back to the in-flight
                    // value if the file can't be re-read.
                    if let Ok(new_cfg) = Config::load_or_default() {
                        rotation_interval = rotation_interval_from(&new_cfg);
                    }
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
            }
        }

        if let Ok(menu_event) = menu_channel.try_recv() {
            match menu_event.id.0.as_str() {
                QUIT_ID => *control_flow = ControlFlow::Exit,
                REFRESH_ID => refresh.notify_one(),
                DASHBOARD_ID => spawn_dashboard(&[]),
                SETTINGS_ID => spawn_dashboard(&["--tab=settings"]),
                _ => {}
            }
        }

        if let Ok(_tray_event) = tray_channel.try_recv() {
            // Click events come through here; no-op for now.
        }
    });
}

/// Rebuild the tray menu from the current snapshots. Providers with
/// `status == Unavailable` are omitted entirely (the user asked for
/// empty providers to be hidden by default). Static items keep stable
/// MenuIds so menu events stay matchable across rebuilds.
fn build_menu(snapshots: &BTreeMap<ProviderId, UsageSnapshot>) -> Menu {
    let menu = Menu::new();
    let mut visible_count = 0usize;
    for id in PROVIDER_ORDER {
        let Some(snap) = snapshots.get(&id) else {
            continue;
        };
        if matches!(snap.status, ProviderStatus::Unavailable) {
            continue;
        }
        let item = MenuItem::new(render_label(snap), false, None);
        let _ = menu.append(&item);
        visible_count += 1;
    }
    if visible_count == 0 {
        let placeholder = MenuItem::new("No provider data yet…", false, None);
        let _ = menu.append(&placeholder);
    }
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(DASHBOARD_ID),
        "Open dashboard…",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(SETTINGS_ID),
        "Settings…",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(REFRESH_ID),
        "Refresh now",
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

fn spawn_dashboard(args: &[&str]) {
    let candidate = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("llm-usage-dashboard")));
    let cmd = candidate
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "llm-usage-dashboard".to_string());
    let _ = std::process::Command::new(&cmd).args(args).spawn();
}

/// Repaint the tray icon and update its tooltip for whichever
/// quota-bearing provider is currently in the rotation slot. Skips
/// providers with no fraction data — rotation lands only on cards
/// the icon can meaningfully draw.
fn refresh_icon(
    tray: &TrayIcon,
    snapshots: &BTreeMap<ProviderId, UsageSnapshot>,
    active_idx: &mut usize,
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
    let (session, weekly) = icon::pick_fractions(snap);
    let _ = tray.set_icon(Some(icon::render(id, session, weekly)));
    let headline = snap.headline.as_deref().unwrap_or("");
    let tooltip = if headline.is_empty() {
        id.human().to_string()
    } else {
        format!("{} — {}", id.human(), headline)
    };
    let _ = tray.set_tooltip(Some(&tooltip));
}
