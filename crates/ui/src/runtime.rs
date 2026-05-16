//! Background polling loop: runs each enabled provider on the configured
//! interval, evaluates quota thresholds, dispatches alerts.
//!
//! IMPORTANT: this module deliberately holds no `tray_icon::menu::MenuItem`
//! references — those types are not `Send` (they wrap GTK / NSMenu state in
//! `Rc<RefCell<…>>`). Snapshots are sent to the UI thread via mpsc and the
//! UI thread is the only one that touches menu items.

use llm_usage_core::config::Config;
use llm_usage_core::model::{ProviderId, UsageSnapshot};
use llm_usage_core::provider::Provider;
use llm_usage_core::providers::{AnthropicProvider, CodexCliProvider, OllamaCloudProvider};
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

/// One pass of: for each provider, poll → merge against cache →
/// record state → evaluate alerts → insert into `snapshots`. Returns
/// the alert messages produced (caller decides whether to actually
/// dispatch them based on `alerts.disabled`).
///
/// Disabled providers are removed from `snapshots` so a freshly
/// toggled-off provider doesn't keep haunting the tray menu.
///
/// Split out from `run` so tests can drive a single iteration with
/// fake `Provider` impls and an in-memory `Store`, instead of
/// spinning up the full tray + tokio sleep loop.
pub(crate) async fn poll_once(
    providers: &[Box<dyn Provider>],
    snapshots: &mut BTreeMap<ProviderId, UsageSnapshot>,
    engine: &mut QuotaEngine,
    store: &Store,
) -> Vec<String> {
    let mut alerts_out: Vec<String> = Vec::new();
    for provider in providers {
        if !provider.enabled() {
            snapshots.remove(&provider.id());
            continue;
        }
        let mut snapshot = match provider.poll().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(provider = %provider.id(), error = %e, "poll failed");
                UsageSnapshot::unavailable(provider.id(), format!("poll error: {}", e))
            }
        };
        // Fill any holes from the last good snapshot so providers
        // with intermittent endpoints (Anthropic OAuth, Ollama
        // session-cookie scrape, etc.) keep showing their last
        // known quotas with a ⚠ marker rather than vanishing.
        if let Some(cached) = snapshots.get_mut(&provider.id()) {
            // Forward-migrate caches written by an older build: a
            // snapshot.json predating the `subview` field deserialises
            // those windows as `subview = false`, so `merge_stale_from`
            // would graft a per-model window back as a false-stale
            // ghost. Re-stamp the provider's declared sub-view labels
            // before merging so old caches behave like new ones.
            for label in provider.subview_labels() {
                if let Some(w) = cached.windows.get_mut(*label) {
                    w.subview = true;
                }
            }
            let n = snapshot.merge_stale_from(cached);
            if n > 0 {
                tracing::debug!(
                    provider = %provider.id(),
                    windows = n,
                    "served cached quota windows as stale",
                );
            }
        }
        let _ = store.record_provider_state(
            &provider.id().to_string(),
            &format!("{:?}", snapshot.status),
            snapshot.error.as_deref(),
        );
        let alerts = engine.evaluate(&snapshot);
        snapshots.insert(snapshot.provider, snapshot);
        for a in alerts {
            alerts_out.push(a.message);
        }
    }
    alerts_out
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
    // Seed the in-memory cache from disk so a freshly started tray has
    // *something* to show before the first poll completes (and so the
    // first poll has a cache to graft from if it fails).
    let mut snapshots: BTreeMap<ProviderId, UsageSnapshot> = llm_usage_core::read_snapshots()
        .ok()
        .flatten()
        .map(|f| f.snapshots)
        .unwrap_or_default();
    // Set the next-check anchor in the past so the first iteration
    // through the loop fires the check immediately (subject to the
    // user's `check_for_updates` flag).
    let mut next_update_check = std::time::Instant::now() - Duration::from_secs(1);
    // First poll after launch: still RECORD which thresholds were
    // already over (so the SQLite dedupe table is in sync) but don't
    // fire desktop notifications for them. The user is opening the
    // app — they don't want to be greeted by alerts for state they
    // already know about; alerts should fire when state TRANSITIONS
    // past a threshold while the tray's running.
    let mut first_poll = true;

    loop {
        let alerts = poll_once(&state.providers, &mut snapshots, &mut state.engine, &store).await;
        if let Err(e) = llm_usage_core::write_snapshots(&snapshots) {
            tracing::warn!(error = %e, "failed to write shared snapshots file");
        }
        let _ = tx.send(RuntimeMessage::Snapshots(snapshots.clone()));
        if first_poll {
            if !alerts.is_empty() {
                tracing::info!(
                    suppressed = alerts.len(),
                    "first-poll alerts suppressed (already-over thresholds at startup)",
                );
            }
            first_poll = false;
        } else {
            for msg in alerts {
                if !state.alerts_disabled {
                    let _ = tx.send(RuntimeMessage::Alert(msg));
                }
            }
        }

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

#[cfg(test)]
mod tests {
    //! Tests for the polling-loop body without spinning up the full
    //! `run` orchestration. Constructs fake `Provider` impls, an
    //! in-memory `Store`, and calls `poll_once` directly.

    use super::*;
    use async_trait::async_trait;
    use llm_usage_core::model::{ProviderStatus, WindowUsage};
    use std::sync::Mutex;

    /// Programmable test double: returns each enqueued result on
    /// successive `poll()` calls, in order. After the queue empties
    /// further polls return an error — surfaces "poll() called more
    /// times than expected" as a test failure rather than a hang.
    struct FakeProvider {
        id: ProviderId,
        enabled: bool,
        responses: Mutex<Vec<anyhow::Result<UsageSnapshot>>>,
    }

    impl FakeProvider {
        fn ok(id: ProviderId, snap: UsageSnapshot) -> Box<dyn Provider> {
            Box::new(Self {
                id,
                enabled: true,
                responses: Mutex::new(vec![Ok(snap)]),
            })
        }

        fn failing(id: ProviderId, msg: &str) -> Box<dyn Provider> {
            Box::new(Self {
                id,
                enabled: true,
                responses: Mutex::new(vec![Err(anyhow::anyhow!(msg.to_string()))]),
            })
        }

        fn disabled(id: ProviderId) -> Box<dyn Provider> {
            Box::new(Self {
                id,
                enabled: false,
                responses: Mutex::new(Vec::new()),
            })
        }

        fn sequence(
            id: ProviderId,
            responses: Vec<anyhow::Result<UsageSnapshot>>,
        ) -> Box<dyn Provider> {
            Box::new(Self {
                id,
                enabled: true,
                responses: Mutex::new(responses),
            })
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn id(&self) -> ProviderId {
            self.id
        }
        fn enabled(&self) -> bool {
            self.enabled
        }
        fn subview_labels(&self) -> &'static [&'static str] {
            // Mirror the real AnthropicProvider so the cache-migration
            // path is exercised under test.
            match self.id {
                ProviderId::Anthropic => &["Sonnet", "Opus"],
                _ => &[],
            }
        }
        async fn poll(&self) -> anyhow::Result<UsageSnapshot> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(anyhow::anyhow!("FakeProvider: queue exhausted"));
            }
            q.remove(0)
        }
    }

    fn snap_with_fraction(provider: ProviderId, label: &str, frac: f64) -> UsageSnapshot {
        let mut s = UsageSnapshot {
            provider,
            timestamp: chrono::Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: Some("fixture".into()),
            plan_label: None,
        };
        s.windows.insert(
            label.to_string(),
            WindowUsage {
                fraction_used: Some(frac),
                ends_at: Some(chrono::Utc::now() + chrono::Duration::hours(2)),
                ..Default::default()
            },
        );
        s
    }

    fn fresh_state() -> (BTreeMap<ProviderId, UsageSnapshot>, QuotaEngine, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let engine = QuotaEngine::new(store.clone());
        (BTreeMap::new(), engine, store)
    }

    #[tokio::test]
    async fn poll_once_inserts_each_providers_snapshot() {
        let (mut snaps, mut engine, store) = fresh_state();
        let providers = vec![
            FakeProvider::ok(
                ProviderId::Anthropic,
                snap_with_fraction(ProviderId::Anthropic, "5h", 0.20),
            ),
            FakeProvider::ok(
                ProviderId::CodexCli,
                snap_with_fraction(ProviderId::CodexCli, "5h", 0.40),
            ),
        ];

        let alerts = poll_once(&providers, &mut snaps, &mut engine, &store).await;

        assert_eq!(alerts.len(), 0, "no thresholds configured");
        assert_eq!(snaps.len(), 2);
        assert!(snaps.contains_key(&ProviderId::Anthropic));
        assert!(snaps.contains_key(&ProviderId::CodexCli));
    }

    #[tokio::test]
    async fn poll_once_emits_alerts_above_threshold() {
        let (mut snaps, mut engine, store) = fresh_state();
        // 75% threshold; the snapshot is at 80%.
        engine.set_thresholds(ProviderId::Anthropic, vec![0.75]);
        let providers = vec![FakeProvider::ok(
            ProviderId::Anthropic,
            snap_with_fraction(ProviderId::Anthropic, "5h", 0.80),
        )];

        let alerts = poll_once(&providers, &mut snaps, &mut engine, &store).await;
        assert_eq!(alerts.len(), 1, "expected one alert: {:?}", alerts);
        // Engine wraps message text — we just want to confirm the
        // alert mentions the provider so dispatch isn't accidentally
        // routed to the wrong tray notification channel.
        assert!(
            alerts[0].to_lowercase().contains("anthropic"),
            "got: {}",
            alerts[0]
        );
    }

    #[tokio::test]
    async fn poll_once_failed_poll_grafts_cache_as_stale() {
        let (mut snaps, mut engine, store) = fresh_state();

        // First call: succeeds, populating the cache.
        let first = FakeProvider::ok(
            ProviderId::Anthropic,
            snap_with_fraction(ProviderId::Anthropic, "5h", 0.55),
        );
        poll_once(&[first], &mut snaps, &mut engine, &store).await;
        assert!(!snaps.get(&ProviderId::Anthropic).unwrap().windows["5h"].stale);

        // Second call: fails. The fresh snapshot would be `unavailable`
        // with no windows — `merge_stale_from` grafts the cached
        // window in and flips `stale = true`.
        let second = FakeProvider::failing(ProviderId::Anthropic, "boom");
        poll_once(&[second], &mut snaps, &mut engine, &store).await;
        let grafted = &snaps.get(&ProviderId::Anthropic).unwrap().windows["5h"];
        assert!(grafted.stale, "expected stale flag after graft");
        assert_eq!(grafted.fraction_used, Some(0.55));
    }

    #[tokio::test]
    async fn poll_once_legacy_cached_subview_not_grafted_as_stale() {
        // Regression: a snapshots.json written by an older build has a
        // per-model "Sonnet" window with NO `subview` field, so it
        // deserialises as `subview = false`. A fresh poll where
        // Anthropic omitted the idle Sonnet bucket must NOT graft that
        // legacy window back as a false-stale ghost — the runtime
        // re-stamps the provider's subview labels on the cache first.
        let (mut snaps, mut engine, store) = fresh_state();

        let mut legacy = snap_with_fraction(ProviderId::Anthropic, "5h", 0.40);
        legacy.windows.insert(
            "Sonnet".to_string(),
            WindowUsage {
                fraction_used: Some(0.01),
                subview: false, // legacy on-disk shape
                ..Default::default()
            },
        );
        snaps.insert(ProviderId::Anthropic, legacy);

        // Fresh poll: 5h present and fresh, no Sonnet (idle class).
        let providers = vec![FakeProvider::ok(
            ProviderId::Anthropic,
            snap_with_fraction(ProviderId::Anthropic, "5h", 0.06),
        )];
        poll_once(&providers, &mut snaps, &mut engine, &store).await;

        let result = snaps.get(&ProviderId::Anthropic).unwrap();
        assert!(
            !result.windows.contains_key("Sonnet"),
            "legacy Sonnet sub-view must not be grafted back from cache"
        );
        let w5 = &result.windows["5h"];
        assert_eq!(w5.fraction_used, Some(0.06), "fresh 5h wins");
        assert!(!w5.stale, "5h is fresh — no stale ⚠");
        assert!(
            matches!(result.status, ProviderStatus::Ok),
            "no spurious degrade when quota is actually fresh"
        );
        assert!(result.error.is_none(), "no bare ⚠ reason");
    }

    #[tokio::test]
    async fn poll_once_successful_poll_clears_stale_from_previous() {
        let (mut snaps, mut engine, store) = fresh_state();

        // Seed the cache with a snapshot that already carries
        // `stale = true` (as if the previous iteration grafted).
        let mut stale = snap_with_fraction(ProviderId::Anthropic, "5h", 0.55);
        stale.windows.get_mut("5h").unwrap().stale = true;
        snaps.insert(ProviderId::Anthropic, stale);

        // A fresh successful poll arrives.
        let providers = vec![FakeProvider::ok(
            ProviderId::Anthropic,
            snap_with_fraction(ProviderId::Anthropic, "5h", 0.30),
        )];
        poll_once(&providers, &mut snaps, &mut engine, &store).await;

        let fresh = &snaps.get(&ProviderId::Anthropic).unwrap().windows["5h"];
        assert_eq!(
            fresh.fraction_used,
            Some(0.30),
            "fresh poll must win over cache"
        );
        assert!(!fresh.stale, "fresh fraction is not stale");
    }

    #[tokio::test]
    async fn poll_once_drops_disabled_providers_from_cache() {
        let (mut snaps, mut engine, store) = fresh_state();

        // Anthropic was healthy in a previous iteration…
        snaps.insert(
            ProviderId::Anthropic,
            snap_with_fraction(ProviderId::Anthropic, "5h", 0.55),
        );
        // …but the user just toggled it off.
        let providers = vec![FakeProvider::disabled(ProviderId::Anthropic)];

        poll_once(&providers, &mut snaps, &mut engine, &store).await;
        assert!(
            !snaps.contains_key(&ProviderId::Anthropic),
            "disabled provider must be evicted from snapshot map"
        );
    }

    #[tokio::test]
    async fn poll_once_records_provider_state_in_store() {
        let (mut snaps, mut engine, store) = fresh_state();
        // A degraded snapshot (status != Ok) — the state should be
        // persisted with its error string so the dashboard can
        // surface it on next read.
        let mut snap = snap_with_fraction(ProviderId::CodexCli, "5h", 0.10);
        snap.status = ProviderStatus::Degraded;
        snap.error = Some("rate limited".into());
        let providers = vec![FakeProvider::ok(ProviderId::CodexCli, snap)];

        poll_once(&providers, &mut snaps, &mut engine, &store).await;
        // Smoke: a second poll on a fresh provider doesn't blow up
        // due to the prior state row already existing — INSERT OR
        // REPLACE in storage should handle the re-record cleanly.
        let providers = vec![FakeProvider::ok(
            ProviderId::CodexCli,
            snap_with_fraction(ProviderId::CodexCli, "5h", 0.20),
        )];
        poll_once(&providers, &mut snaps, &mut engine, &store).await;
        assert_eq!(
            snaps.get(&ProviderId::CodexCli).unwrap().windows["5h"].fraction_used,
            Some(0.20)
        );
    }

    #[tokio::test]
    async fn poll_once_sequence_of_iterations_overwrites_cleanly() {
        // Three successive polls, fractions changing each time.
        // Confirms snapshots map doesn't accumulate phantom data and
        // each iteration sees the previous one's fresh value.
        let (mut snaps, mut engine, store) = fresh_state();
        let provider = FakeProvider::sequence(
            ProviderId::Anthropic,
            vec![
                Ok(snap_with_fraction(ProviderId::Anthropic, "5h", 0.10)),
                Ok(snap_with_fraction(ProviderId::Anthropic, "5h", 0.30)),
                Ok(snap_with_fraction(ProviderId::Anthropic, "5h", 0.50)),
            ],
        );
        // poll_once consumes one response per iteration. We need to
        // wrap the same Box in a slice each iteration.
        let providers: [Box<dyn Provider>; 1] = [provider];

        for expected in [0.10, 0.30, 0.50] {
            poll_once(&providers, &mut snaps, &mut engine, &store).await;
            assert_eq!(
                snaps.get(&ProviderId::Anthropic).unwrap().windows["5h"].fraction_used,
                Some(expected),
                "iteration with expected={expected}",
            );
            // Map stays single-entry — no provider duplication.
            assert_eq!(snaps.len(), 1);
        }
    }

    #[tokio::test]
    async fn poll_once_alert_does_not_re_fire_within_same_window_bucket() {
        // The QuotaEngine de-duplicates alerts by (provider, window,
        // window_id, threshold) keyed in SQLite. Two back-to-back
        // polls at 80% with a 75% threshold should fire the alert
        // exactly once.
        let (mut snaps, mut engine, store) = fresh_state();
        engine.set_thresholds(ProviderId::Anthropic, vec![0.75]);
        let providers = vec![FakeProvider::sequence(
            ProviderId::Anthropic,
            vec![
                Ok(snap_with_fraction(ProviderId::Anthropic, "5h", 0.80)),
                Ok(snap_with_fraction(ProviderId::Anthropic, "5h", 0.85)),
            ],
        )];

        let first = poll_once(&providers, &mut snaps, &mut engine, &store).await;
        assert_eq!(first.len(), 1);
        let second = poll_once(&providers, &mut snaps, &mut engine, &store).await;
        assert!(
            second.is_empty(),
            "alert must dedupe within the same window bucket: {:?}",
            second
        );
    }
}
