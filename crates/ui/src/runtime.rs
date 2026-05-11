//! Background polling loop: runs each enabled provider on the configured
//! interval, evaluates quota thresholds, dispatches alerts.
//!
//! IMPORTANT: this module deliberately holds no `tray_icon::menu::MenuItem`
//! references — those types are not `Send` (they wrap GTK / NSMenu state in
//! `Rc<RefCell<…>>`). Snapshots are sent to the UI thread via mpsc and the
//! UI thread is the only one that touches menu items.

use llm_usage_core::config::Config;
use llm_usage_core::model::{ProviderId, UsageSnapshot};
use llm_usage_core::providers::{AnthropicProvider, CodexCliProvider, OllamaCloudProvider};
use llm_usage_core::provider::Provider;
use llm_usage_core::quota::QuotaEngine;
use llm_usage_core::storage::Store;
use llm_usage_core::updates::{self, UpdateInfo};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub enum RuntimeMessage {
    Snapshots(BTreeMap<ProviderId, UsageSnapshot>),
    Alert(String),
    ConfigReloaded,
    ConfigReloadFailed(String),
    UpdateAvailable(UpdateInfo),
}

pub struct RuntimeHandle {
    pub refresh: Arc<Notify>,
    pub reload: Arc<Notify>,
}

struct LoopState {
    providers: Vec<Box<dyn Provider>>,
    engine: QuotaEngine,
    interval: Duration,
    alerts_disabled: bool,
    check_for_updates: bool,
}

fn build_state(config: &Config, store: Arc<Store>) -> LoopState {
    let mut engine = QuotaEngine::new(store);
    if !config.anthropic.warn_at.is_empty() {
        engine.set_thresholds(ProviderId::Anthropic, config.anthropic.warn_at.clone());
    }
    if !config.codex_cli.warn_at.is_empty() {
        engine.set_thresholds(ProviderId::CodexCli, config.codex_cli.warn_at.clone());
    }
    if !config.ollama_cloud.warn_at.is_empty() {
        engine.set_thresholds(ProviderId::OllamaCloud, config.ollama_cloud.warn_at.clone());
    }

    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(AnthropicProvider::new(config.anthropic.clone())),
        Box::new(CodexCliProvider::new(config.codex_cli.clone())),
        Box::new(OllamaCloudProvider::new(config.ollama_cloud.clone())),
    ];

    LoopState {
        providers,
        engine,
        interval: Duration::from_secs(config.poll_interval_secs.max(60)),
        alerts_disabled: config.alerts.disabled,
        check_for_updates: config.check_for_updates,
    }
}

/// How often we ask GitHub for the latest release. 24 hours keeps us
/// far inside the unauthenticated rate limit (60 requests/hour/IP)
/// and matches user expectations for an "occasional banner" UX.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

pub async fn run(
    initial_config: Config,
    handle: RuntimeHandle,
    tx: std::sync::mpsc::Sender<RuntimeMessage>,
) {
    let store = match llm_usage_core::config::data_path() {
        std::result::Result::Ok(p) => match Store::open(&p) {
            std::result::Result::Ok(s) => Arc::new(s),
            std::result::Result::Err(e) => {
                tracing::error!(error = %e, "store init failed");
                return;
            }
        },
        std::result::Result::Err(e) => {
            tracing::error!(error = %e, "data_path failed");
            return;
        }
    };

    let refresh = handle.refresh.clone();
    let reload = handle.reload.clone();
    let mut state = build_state(&initial_config, store.clone());
    let mut snapshots: BTreeMap<ProviderId, UsageSnapshot> = BTreeMap::new();
    // Set the next-check anchor in the past so the first iteration
    // through the loop fires the check immediately (subject to the
    // user's `check_for_updates` flag).
    let mut next_update_check =
        std::time::Instant::now() - Duration::from_secs(1);

    loop {
        for provider in &state.providers {
            if !provider.enabled() {
                // If the user just disabled this provider, drop any stale
                // snapshot so the menu doesn't keep showing it.
                snapshots.remove(&provider.id());
                continue;
            }
            let snapshot = match provider.poll().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(provider = %provider.id(), error = %e, "poll failed");
                    UsageSnapshot::unavailable(provider.id(), format!("poll error: {}", e))
                }
            };
            let _ = store.record_provider_state(
                &provider.id().to_string(),
                &format!("{:?}", snapshot.status),
                snapshot.error.as_deref(),
            );
            let alerts = state.engine.evaluate(&snapshot);
            snapshots.insert(snapshot.provider, snapshot);
            for a in alerts {
                if !state.alerts_disabled {
                    let _ = tx.send(RuntimeMessage::Alert(a.message));
                }
            }
        }
        if let Err(e) = llm_usage_core::write_snapshots(&snapshots) {
            tracing::warn!(error = %e, "failed to write shared snapshots file");
        }
        let _ = tx.send(RuntimeMessage::Snapshots(snapshots.clone()));

        // Update check fires at most once per UPDATE_CHECK_INTERVAL and
        // only when the user has the toggle on. Network / parse
        // failures are logged at debug level and ignored.
        if state.check_for_updates && std::time::Instant::now() >= next_update_check {
            next_update_check = std::time::Instant::now() + UPDATE_CHECK_INTERVAL;
            match updates::check(env!("CARGO_PKG_VERSION")).await {
                Ok(Some(info)) => {
                    let _ = tx.send(RuntimeMessage::UpdateAvailable(info));
                }
                Ok(None) => {
                    tracing::debug!("update check: already on the latest release");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "update check failed");
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(state.interval) => {}
            _ = refresh.notified() => {
                tracing::debug!("manual refresh requested");
            }
            _ = reload.notified() => {
                match Config::load_or_default() {
                    Ok(new_cfg) => {
                        tracing::info!("config reloaded");
                        state = build_state(&new_cfg, store.clone());
                        let _ = tx.send(RuntimeMessage::ConfigReloaded);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "config reload failed");
                        let _ = tx.send(RuntimeMessage::ConfigReloadFailed(e.to_string()));
                    }
                }
            }
        }
    }
}

