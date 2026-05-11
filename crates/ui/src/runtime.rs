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
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub enum RuntimeMessage {
    Snapshots(BTreeMap<ProviderId, UsageSnapshot>),
    Alert(String),
    ConfigReloaded,
    ConfigReloadFailed(String),
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
    }
}

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

