//! Reads ~/.claude/projects/**/*.jsonl, extracts per-message usage from
//! Claude Code session logs, and aggregates by time window.
//!
//! Schema (verified against Claude Code 2.1.x JSONL):
//!   { "type": "assistant", "timestamp": "2026-05-07T16:23:08.922Z",
//!     "message": { "model": "claude-opus-4-7",
//!                  "id": "msg_…",   // de-dupe key
//!                  "usage": { "input_tokens": …, "output_tokens": …,
//!                             "cache_read_input_tokens": …,
//!                             "cache_creation": {
//!                                "ephemeral_5m_input_tokens": …,
//!                                "ephemeral_1h_input_tokens": … } } } }

use crate::anthropic_oauth::{
    self, OAuthBackoff, OAuthCredentials, OAuthError, OAuthUsageResponse, QuotaBucket,
};
use crate::config::AnthropicConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind, WindowUsage};
use crate::pricing::{anthropic_default, AnthropicTokenUsage, ModelRate};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
#[cfg(test)]
use chrono::TimeZone;
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;

/// Minimum time between successive `/api/oauth/usage` HTTP calls.
/// Within this window we serve the cached `OAuthBackoff::last_good`
/// instead of re-fetching — protects against file-watcher-driven
/// refresh storms (Claude Code can write to `~/.claude/projects/`
/// once per assistant turn).
///
/// Bumped progressively:
///   60 s   — got 429'd within 30 minutes under heavy use.
///   300 s  — got 429'd again after a few hours of use.
///   900 s  — 4 calls/hour ceiling, comfortably inside whatever
///            Anthropic's actual budget is. Local-file token
///            counts still update instantly via the data-source
///            watcher; only the quota *percentage* lags by at
///            most 15 min, same cadence as the pre-watcher idle
///            poll default.
const MIN_HTTP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(900);
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use walkdir::WalkDir;

/// Per-file resumption state. We track size + offset so we can detect
/// truncation (size shrank → start over) and skip files that haven't
/// changed since the last scan.
#[derive(Default, Clone, Copy)]
struct FileState {
    /// File size as observed at the last successful scan. Diagnostic
    /// only — the no-new-bytes short-circuit compares `offset` to the
    /// fresh `metadata().len()`.
    #[allow(dead_code)]
    size: u64,
    offset: u64,
}

/// One parsed assistant turn, decoupled from the original file. We
/// cache the raw `AnthropicTokenUsage` + model name rather than a
/// precomputed cost so that a config-time rate change re-prices
/// existing events on the next aggregate call.
#[derive(Clone)]
struct CachedEvent {
    timestamp: DateTime<Utc>,
    tokens: AnthropicTokenUsage,
    model: String,
}

/// Stop tracking events older than this. The longest window we
/// bucket into is "this month" (~31 days); 45 days gives a comfortable
/// buffer in case the user's local time and the events' UTC stamps
/// disagree, and bounds memory at a few MB even for heavy users.
const EVENT_RETENTION: Duration = Duration::from_secs(45 * 86_400);

/// Provider-scoped scan cache. Survives between `poll()` calls but
/// not across tray restarts; the first poll after startup pays the
/// full-walk cost once and subsequent polls only read appended
/// bytes per JSONL.
#[derive(Default)]
struct ScanCache {
    events: Vec<CachedEvent>,
    files: HashMap<PathBuf, FileState>,
    /// `requestId|messageId` dedupe — see `process_lines` for the
    /// full key derivation. Kept alongside `events` so a duplicate
    /// line from a re-read of the same file (after truncation, say)
    /// doesn't double-count.
    seen_ids: HashSet<String>,
}

pub struct AnthropicProvider {
    cfg: AnthropicConfig,
    projects_dir: PathBuf,
    http: reqwest::Client,
    /// Persists across polls — survives 429s by serving last-good and
    /// holding off the next request until cooldown ends.
    oauth_backoff: Mutex<OAuthBackoff>,
    /// Optional opencode SQLite path; `None` disables the integration.
    opencode_db: Option<PathBuf>,
    /// Incremental aggregation cache. The previous implementation
    /// walked every JSONL in `~/.claude/projects/` and re-parsed every
    /// line on every poll, which pinned the runtime at 90 %+ CPU
    /// under heavy Claude Code use with 76 MB of history. The cache
    /// stores already-parsed events keyed nowhere — we just walk it
    /// linearly each aggregate call and bucket against `now`.
    cache: Mutex<ScanCache>,
}

impl AnthropicProvider {
    pub fn new(cfg: AnthropicConfig) -> Self {
        Self::with_opencode_db(cfg, Some(crate::opencode::default_db_path()))
    }

    pub fn with_opencode_db(cfg: AnthropicConfig, opencode_db: Option<PathBuf>) -> Self {
        let projects_dir = cfg
            .claude_projects_dir
            .clone()
            .unwrap_or_else(default_projects_dir);
        let http = reqwest::Client::builder()
            .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest");
        let opencode_db = opencode_db.filter(|p| !p.as_os_str().is_empty());
        Self {
            cfg,
            projects_dir,
            http,
            oauth_backoff: Mutex::new(OAuthBackoff::default()),
            opencode_db,
            cache: Mutex::new(ScanCache::default()),
        }
    }

    fn rate_for(&self, model: &str) -> ModelRate {
        for (key, rate) in &self.cfg.model_rates {
            if model.contains(key.as_str()) {
                return *rate;
            }
        }
        anthropic_default(model)
    }

    fn aggregate(&self, now: DateTime<Utc>) -> Result<Aggregate> {
        let mut agg = Aggregate::default();
        if !self.projects_dir.exists() {
            agg.error = Some(format!(
                "claude projects dir not found: {}",
                self.projects_dir.display()
            ));
            return Ok(agg);
        }

        // Incremental scan: only read newly-appended bytes per file.
        // Updates `self.cache.events` in place; existing entries are
        // reused as-is.
        {
            let mut cache = self.cache.lock().expect("poisoned");
            for entry in WalkDir::new(&self.projects_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl")
                })
            {
                let path = entry.path().to_path_buf();
                if let Err(err) = self.scan_file_incremental(&path, &mut cache) {
                    tracing::warn!(path = %path.display(), error = %err, "failed to parse claude jsonl");
                }
            }
            // Drop events that fell out of the longest bucket. Keeps
            // memory bounded for users who run the tray for weeks.
            let cutoff = now - chrono::Duration::from_std(EVENT_RETENTION).unwrap();
            cache.events.retain(|e| e.timestamp > cutoff);
        }

        // Bucket cached events into the running `Aggregate`. Cost is
        // computed here (not at cache time) so a config-time rate
        // change re-prices old events without re-scanning.
        let cache = self.cache.lock().expect("poisoned");
        for e in &cache.events {
            let cost = e.tokens.cost_usd(self.rate_for(&e.model));
            agg.add(e.timestamp, now, e.tokens, cost);
        }
        drop(cache);

        // Supplementary source: opencode's SQLite store. Users hitting
        // Anthropic via opencode rather than Claude Code won't write to
        // ~/.claude/projects, so their turn activity lives there. We
        // run those tokens through the same `rate_for(model)` table so
        // the resulting spend column is consistent with the JSONL path.
        if let Some(db) = &self.opencode_db {
            match crate::opencode::read_events(db, "anthropic") {
                Ok(events) => {
                    for e in events {
                        let usage = AnthropicTokenUsage {
                            input_tokens: e.input_tokens,
                            output_tokens: e.output_tokens,
                            cache_read_input_tokens: e.cached_tokens,
                            cache_creation_5m_input_tokens: 0,
                            cache_creation_1h_input_tokens: 0,
                        };
                        let cost = match e.cost_usd {
                            Some(c) => c,
                            None => usage.cost_usd(self.rate_for(&e.model)),
                        };
                        agg.add(e.timestamp, now, usage, cost);
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, path = %db.display(), "opencode db read failed");
                }
            }
        }
        Ok(agg)
    }

    /// Read newly-appended bytes from `path` and push any newly-parsed
    /// assistant events into `cache.events`. Cheap when nothing has
    /// changed since the last scan — the file's metadata is checked
    /// first and we bail without opening the file when size + offset
    /// already match. On truncation (size < cached offset) we restart
    /// from the beginning of the file.
    fn scan_file_incremental(&self, path: &Path, cache: &mut ScanCache) -> Result<()> {
        let meta = std::fs::metadata(path)?;
        let size = meta.len();
        let prev = cache.files.get(path).copied().unwrap_or_default();
        let start = if prev.offset > size {
            // File was truncated (rare — Claude Code doesn't do this,
            // but a manual `rm` + new session reusing the path would).
            // Conservatively re-read from the top.
            0
        } else if prev.offset == size {
            // No new bytes since the last scan. The `mtime` test
            // catches the same case earlier on many file systems,
            // but offset is the authoritative no-op signal.
            return Ok(());
        } else {
            prev.offset
        };

        let f = std::fs::File::open(path)?;
        let mut reader = BufReader::new(f);
        if start > 0 {
            reader.seek(SeekFrom::Start(start))?;
        }

        let mut consumed = start;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            if !line.ends_with('\n') {
                // Partial trailing line (Claude Code is mid-write, or
                // the file ended without `\n`). Leave it for the next
                // scan — don't advance the offset past it.
                break;
            }
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                self.parse_and_push(path, trimmed, cache);
            }
            consumed += n as u64;
        }

        cache.files.insert(
            path.to_path_buf(),
            FileState {
                size,
                offset: consumed,
            },
        );
        Ok(())
    }

    /// Pure parse step shared by the incremental scanner: turn one
    /// JSONL line into a `CachedEvent` and append it to `cache.events`
    /// (with dedupe). Errors are silently swallowed — partial / weird
    /// lines simply don't contribute, which is the same behaviour the
    /// original full-walk implementation had.
    fn parse_and_push(&self, path: &Path, line: &str, cache: &mut ScanCache) {
        let Ok(entry) = serde_json::from_str::<AssistantEntry>(line) else {
            return;
        };
        if entry.entry_type.as_deref() != Some("assistant") {
            return;
        }
        let Some(message) = entry.message else { return };
        let Some(usage) = message.usage else { return };
        let Some(timestamp) = entry.timestamp.as_deref().and_then(|t| {
            DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|d| d.with_timezone(&Utc))
        }) else {
            return;
        };

        // Dedupe — Anthropic emits the same (requestId, messageId)
        // when a turn produces multiple tool_use blocks; we only want
        // to count tokens once. Fall back to "path:offset" when ids
        // aren't present, which is unique enough for an incremental
        // append-only stream.
        let dedupe_key = format!(
            "{}:{}",
            path.display(),
            cache.files.get(path).map(|s| s.offset).unwrap_or(0)
        );
        let id_key = message
            .id
            .as_deref()
            .map(|id| {
                entry
                    .request_id
                    .as_deref()
                    .map(|r| format!("{}|{}", r, id))
                    .unwrap_or_else(|| id.to_string())
            })
            .unwrap_or(dedupe_key);
        if !cache.seen_ids.insert(id_key) {
            return;
        }

        let tokens = AnthropicTokenUsage {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation_5m_input_tokens: usage
                .cache_creation
                .as_ref()
                .and_then(|c| c.ephemeral_5m_input_tokens)
                .unwrap_or(0),
            cache_creation_1h_input_tokens: usage
                .cache_creation
                .as_ref()
                .and_then(|c| c.ephemeral_1h_input_tokens)
                .unwrap_or(0),
        };
        cache.events.push(CachedEvent {
            timestamp,
            tokens,
            model: message.model.unwrap_or_default(),
        });
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        let now = Utc::now();
        let agg = self.aggregate(now)?;
        let mut snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: now,
            status: if agg.error.is_some() {
                ProviderStatus::Degraded
            } else {
                ProviderStatus::Ok
            },
            error: agg.error.clone(),
            windows: Default::default(),
            headline: None,
            plan_label: None,
        };

        // Fill windows.
        let hour = snap.window_mut(WindowKind::LastHour);
        hour.spend_usd = Some(agg.last_hour.cost);
        hour.tokens_in = agg.last_hour.tokens_in;
        hour.tokens_out = agg.last_hour.tokens_out;
        hour.request_count = agg.last_hour.requests;
        hour.recompute_fraction();

        let today = snap.window_mut(WindowKind::Today);
        today.spend_usd = Some(agg.today.cost);
        today.tokens_in = agg.today.tokens_in;
        today.tokens_out = agg.today.tokens_out;
        today.request_count = agg.today.requests;
        today.limit_usd = self.cfg.daily_budget_usd;
        today.recompute_fraction();

        let weekly_fraction = {
            let week = snap.window_mut(WindowKind::ThisWeek);
            week.spend_usd = Some(agg.this_week.cost);
            week.tokens_in = agg.this_week.tokens_in;
            week.tokens_out = agg.this_week.tokens_out;
            week.request_count = agg.this_week.requests;
            week.limit_usd = self.cfg.weekly_budget_usd;
            week.recompute_fraction();
            week.fraction_used
        };

        let month = snap.window_mut(WindowKind::ThisMonth);
        month.spend_usd = Some(agg.this_month.cost);
        month.tokens_in = agg.this_month.tokens_in;
        month.tokens_out = agg.this_month.tokens_out;
        month.request_count = agg.this_month.requests;
        month.recompute_fraction();

        // Quota windows from Anthropic's OAuth /usage endpoint — same data
        // Claude Code's `/usage` shows. We fetch it ourselves on each poll
        // using the access token in ~/.claude/.credentials.json. If we got
        // 429'd recently OR the last successful fetch is too recent to
        // warrant a re-call, serve the last-good cached response instead.
        let mut quota_headline = String::new();
        let mut quota_error: Option<String> = None;
        let mut serving_stale = false;

        let (skip_http, in_cooldown) = self
            .oauth_backoff
            .lock()
            .ok()
            .map(|b| {
                (
                    b.should_skip_http(now, MIN_HTTP_INTERVAL),
                    b.in_cooldown(now),
                )
            })
            .unwrap_or((false, false));

        if skip_http {
            // Either rate-limit cooldown or a recent success short-
            // circuits the network call. Reuse last-good and (if it
            // was a 429 cooldown) flag the windows stale so the tray
            // shows ⚠ markers.
            let snapshot_data = self
                .oauth_backoff
                .lock()
                .ok()
                .and_then(|b| b.last_good.clone());
            if let Some(usage) = snapshot_data {
                apply_oauth_usage(&mut snap, &usage, now, &mut quota_headline);
                if in_cooldown {
                    mark_oauth_windows_stale(&mut snap);
                    serving_stale = true;
                }
            }
            if in_cooldown {
                let remaining = self
                    .oauth_backoff
                    .lock()
                    .ok()
                    .map(|b| b.cooldown_remaining(now))
                    .unwrap_or(0);
                quota_error = Some(format!(
                    "Rate-limited by Anthropic — refresh paused for {} min",
                    remaining.div_euclid(60).max(1)
                ));
            }
        } else {
            tracing::info!(
                throttle_secs = MIN_HTTP_INTERVAL.as_secs(),
                "anthropic /api/oauth/usage — calling upstream"
            );
            match anthropic_oauth::fetch_usage(&self.http).await {
                Ok(usage) => {
                    apply_oauth_usage(&mut snap, &usage, now, &mut quota_headline);
                    if let Ok(mut b) = self.oauth_backoff.lock() {
                        b.record_success(now, &usage);
                    }
                }
                Err(OAuthError::RateLimited) => {
                    if let Ok(mut b) = self.oauth_backoff.lock() {
                        b.record_429(now);
                        // Serve last-good if any while we're in cooldown.
                        if let Some(usage) = b.last_good.clone() {
                            apply_oauth_usage(&mut snap, &usage, now, &mut quota_headline);
                            mark_oauth_windows_stale(&mut snap);
                            serving_stale = true;
                        }
                    }
                    tracing::warn!(
                        "anthropic oauth /usage rate-limited; backing off {}s",
                        OAuthBackoff::INITIAL_COOLDOWN_SECS
                    );
                    quota_error = Some("Rate-limited by Anthropic — backing off".into());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "anthropic oauth usage fetch failed");
                    // Capitalise + drop the trailing "." some
                    // OAuthError variants embed so the chip reads
                    // like a UI message, not a log line.
                    let mut s = e.to_string();
                    if let Some(first) = s.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    s = s.trim_end_matches('.').to_string();
                    quota_error = Some(s);
                }
            }
        }

        let stale_marker = if serving_stale { " (stale)" } else { "" };
        snap.headline = if self.cfg.show_spend {
            let mut spend_headline = format!("${:.2} today", agg.today.cost);
            if quota_headline.is_empty() {
                if let Some(f) = weekly_fraction {
                    push_segment(&mut spend_headline, &format!("{:.0}% of weekly", f * 100.0));
                }
            }
            Some(if !quota_headline.is_empty() {
                format!("{}{} · {}", quota_headline, stale_marker, spend_headline)
            } else {
                spend_headline
            })
        } else if !quota_headline.is_empty() {
            Some(format!("{}{}", quota_headline, stale_marker))
        } else {
            None
        };

        // Soften status to Degraded when we have spend data but couldn't
        // reach the quota endpoint — the menu still shows useful info but
        // the user knows quota numbers may be stale.
        if let Some(err) = quota_error {
            if matches!(snap.status, ProviderStatus::Ok) {
                snap.status = ProviderStatus::Degraded;
                snap.error = Some(err);
            }
        }

        // Best-effort plan tag from the credentials file ("Max 5x",
        // "Pro", etc.). Failing to read it is non-fatal — the header
        // just falls back to the provider name alone.
        if let Ok(creds) = OAuthCredentials::load() {
            snap.plan_label = creds.plan_label();
        }

        if !self.cfg.show_spend {
            snap.strip_spend();
        }

        Ok(snap)
    }
}

fn apply_oauth_usage(
    snap: &mut UsageSnapshot,
    usage: &OAuthUsageResponse,
    now: DateTime<Utc>,
    headline: &mut String,
) {
    if let Some(b) = &usage.five_hour {
        fill_quota_window(snap.window_mut(WindowKind::FiveHourRolling), b, now);
        push_segment(headline, &format_quota("5h", b, now));
    }
    if let Some(b) = &usage.seven_day {
        fill_quota_window(snap.window_mut(WindowKind::ThisWeek), b, now);
        push_segment(headline, &format_quota("7d", b, now));
    }
    // Sonnet- and Opus-specific weekly buckets are surfaced as separate
    // labelled windows so the dashboard can show all of them, but we leave
    // them out of the compact tray headline (it's already busy). The
    // display labels match the all-models "week" window so users can read
    // them as siblings of it.
    if let Some(b) = &usage.seven_day_sonnet {
        let w = snap.windows.entry("Sonnet".to_string()).or_default();
        fill_quota_window(w, b, now);
    }
    if let Some(b) = &usage.seven_day_opus {
        let w = snap.windows.entry("Opus".to_string()).or_default();
        fill_quota_window(w, b, now);
    }
}

fn fill_quota_window(window: &mut WindowUsage, q: &QuotaBucket, now: DateTime<Utc>) {
    window.fraction_used = Some(q.utilization / 100.0);
    window.ends_at = q.resets_at_utc();
    window.started_at = Some(now);
    // A successful OAuth response is fresh even if Anthropic reports a
    // reset timestamp that has just passed or is absent. Grey/stale is
    // reserved for cache fallback after a failed/throttled quota poll.
}

/// After `apply_oauth_usage` has populated the canonical quota windows
/// from a backoff-cached response, mark them `stale = true` so the
/// tray / CLI render a ⚠ marker — same channel as the merge-from-cache
/// fallback in `UsageSnapshot::merge_stale_from`. Without this, a
/// rate-limited 429 cooldown would surface the old fractions with a
/// normal countdown, hiding from the user that the data is cached.
fn mark_oauth_windows_stale(snap: &mut UsageSnapshot) {
    let labels = [
        WindowKind::FiveHourRolling.label(),
        WindowKind::ThisWeek.label(),
        "Sonnet",
        "Opus",
    ];
    for label in labels {
        if let Some(w) = snap.windows.get_mut(label) {
            w.stale = true;
        }
    }
}

fn format_quota(label: &str, q: &QuotaBucket, now: DateTime<Utc>) -> String {
    let reset = q
        .resets_at_utc()
        .map(|t| {
            let secs = (t - now).num_seconds();
            if secs <= 0 {
                String::new()
            } else if secs < 3600 {
                format!(" R:{}m", secs / 60)
            } else if secs < 86_400 {
                format!(" R:{}h", secs / 3600)
            } else {
                format!(" R:{}d", secs / 86_400)
            }
        })
        .unwrap_or_default();
    format!("{} {:.0}%{}", label, q.utilization, reset)
}

fn push_segment(s: &mut String, seg: &str) {
    if !s.is_empty() {
        s.push_str(" · ");
    }
    s.push_str(seg);
}

fn default_projects_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".claude").join("projects"))
        .unwrap_or_else(|| PathBuf::from(".claude/projects"))
}

#[derive(Debug, Default, Clone, Copy)]
struct WindowAgg {
    cost: f64,
    tokens_in: u64,
    tokens_out: u64,
    requests: u64,
}

impl WindowAgg {
    fn add(&mut self, t: AnthropicTokenUsage, cost: f64) {
        self.cost += cost;
        self.tokens_in = self.tokens_in.saturating_add(
            t.input_tokens
                + t.cache_read_input_tokens
                + t.cache_creation_5m_input_tokens
                + t.cache_creation_1h_input_tokens,
        );
        self.tokens_out = self.tokens_out.saturating_add(t.output_tokens);
        self.requests += 1;
    }
}

#[derive(Debug, Default)]
struct Aggregate {
    last_hour: WindowAgg,
    today: WindowAgg,
    this_week: WindowAgg,
    this_month: WindowAgg,
    error: Option<String>,
}

impl Aggregate {
    fn add(&mut self, ts: DateTime<Utc>, now: DateTime<Utc>, t: AnthropicTokenUsage, cost: f64) {
        if (now - ts).num_hours().abs() < 1 {
            self.last_hour.add(t, cost);
        }
        if same_local_day(ts, now) {
            self.today.add(t, cost);
        }
        if same_iso_week(ts, now) {
            self.this_week.add(t, cost);
        }
        if same_month(ts, now) {
            self.this_month.add(t, cost);
        }
    }
}

fn same_local_day(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    let al = a.with_timezone(&chrono::Local);
    let bl = b.with_timezone(&chrono::Local);
    al.date_naive() == bl.date_naive()
}

fn same_iso_week(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    let ai = a.iso_week();
    let bi = b.iso_week();
    ai.year() == bi.year() && ai.week() == bi.week()
}

fn same_month(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.year() == b.year() && a.month() == b.month()
}

#[derive(Debug, Deserialize)]
struct AssistantEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<MessageBody>,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    id: Option<String>,
    model: Option<String>,
    usage: Option<UsageBlock>,
}

#[derive(Debug, Deserialize)]
struct UsageBlock {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
}

#[derive(Debug, Deserialize)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn push_segment_separates_with_middle_dot() {
        let mut s = String::new();
        push_segment(&mut s, "5h 12%");
        push_segment(&mut s, "7d 30%");
        assert_eq!(s, "5h 12% · 7d 30%");
    }

    #[test]
    fn push_segment_no_leading_separator_on_empty_base() {
        let mut s = String::new();
        push_segment(&mut s, "only");
        assert_eq!(s, "only");
    }

    #[test]
    fn format_quota_shows_reset_in_appropriate_unit() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 30 minutes ahead → "Rm".
        let b = QuotaBucket {
            utilization: 12.0,
            resets_at: Some("2026-05-10T10:30:00Z".into()),
        };
        let s = format_quota("5h", &b, now);
        assert!(s.contains("12%"));
        assert!(s.contains("R:30m"), "got {}", s);

        // 5 hours ahead → "Rh".
        let b = QuotaBucket {
            utilization: 30.0,
            resets_at: Some("2026-05-10T15:00:00Z".into()),
        };
        let s = format_quota("7d", &b, now);
        assert!(s.contains("30%"));
        assert!(s.contains("R:5h"), "got {}", s);

        // 3 days ahead → "Rd".
        let b = QuotaBucket {
            utilization: 80.0,
            resets_at: Some("2026-05-13T10:00:00Z".into()),
        };
        let s = format_quota("7d", &b, now);
        assert!(s.contains("R:3d"), "got {}", s);
    }

    #[test]
    fn format_quota_omits_reset_when_in_the_past() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = QuotaBucket {
            utilization: 42.0,
            resets_at: Some("2026-05-10T09:00:00Z".into()),
        };
        let s = format_quota("5h", &b, now);
        assert!(s.contains("42%"));
        assert!(!s.contains("R:"), "got {}", s);
    }

    #[test]
    fn same_local_day_returns_true_for_identical_instants() {
        // Local-day equality is timezone-dependent at the edges; we
        // assert the identity case (same instant) which always holds
        // and the day-apart case which always differs.
        let a = chrono::DateTime::parse_from_rfc3339("2026-05-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_local_day(a, a));
        let c = a + chrono::Duration::days(2);
        assert!(!same_local_day(a, c));
    }

    #[test]
    fn same_iso_week_groups_mid_week_timestamps() {
        // Wednesday and Friday of the same ISO week.
        let a = chrono::DateTime::parse_from_rfc3339("2026-05-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = chrono::DateTime::parse_from_rfc3339("2026-05-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_iso_week(a, b));
    }

    #[test]
    fn same_month_is_year_and_month_strict() {
        let a = chrono::DateTime::parse_from_rfc3339("2026-05-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = chrono::DateTime::parse_from_rfc3339("2026-05-31T23:59:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(same_month(a, b));
        let c = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!same_month(a, c));
        // Different year, same month number → still different.
        let d = chrono::DateTime::parse_from_rfc3339("2025-05-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!same_month(a, d));
    }

    #[test]
    fn window_agg_add_accumulates_components() {
        let mut a = WindowAgg::default();
        a.add(
            AnthropicTokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 25,
                cache_creation_5m_input_tokens: 10,
                cache_creation_1h_input_tokens: 5,
            },
            0.42,
        );
        assert_eq!(a.requests, 1);
        assert!((a.cost - 0.42).abs() < 1e-9);
        // tokens_in is the sum of input + every cache bucket.
        assert_eq!(a.tokens_in, 100 + 25 + 10 + 5);
        assert_eq!(a.tokens_out, 50);
    }

    #[test]
    fn apply_oauth_usage_populates_canonical_windows() {
        use crate::anthropic_oauth::{OAuthUsageResponse, QuotaBucket};
        let mut snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        };
        let resets_5h = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let resets_7d = (Utc::now() + chrono::Duration::days(3)).to_rfc3339();
        let usage = OAuthUsageResponse {
            five_hour: Some(QuotaBucket {
                utilization: 55.0,
                resets_at: Some(resets_5h),
            }),
            seven_day: Some(QuotaBucket {
                utilization: 58.0,
                resets_at: Some(resets_7d.clone()),
            }),
            seven_day_sonnet: Some(QuotaBucket {
                utilization: 31.0,
                resets_at: Some(resets_7d.clone()),
            }),
            seven_day_opus: None,
            extra_usage: None,
        };
        let mut headline = String::new();
        apply_oauth_usage(&mut snap, &usage, Utc::now(), &mut headline);

        // 5h and week populate at canonical labels.
        let five_h = snap
            .window(WindowKind::FiveHourRolling)
            .expect("5h present");
        assert!((five_h.fraction_used.unwrap() - 0.55).abs() < 1e-9);
        let week = snap.window(WindowKind::ThisWeek).expect("week present");
        assert!((week.fraction_used.unwrap() - 0.58).abs() < 1e-9);
        // Sonnet-specific weekly stashed as its own labelled window.
        let sonnet = snap.windows.get("Sonnet").expect("sonnet present");
        assert!((sonnet.fraction_used.unwrap() - 0.31).abs() < 1e-9);
        // Opus absent from response → window not created.
        assert!(!snap.windows.contains_key("Opus"));
        // Compact headline includes 5h and 7d but not the per-model
        // breakdown (those would make it too noisy).
        assert!(headline.contains("5h 55%"), "got: {headline}");
        assert!(headline.contains("7d 58%"), "got: {headline}");
        assert!(!headline.contains("Sonnet"), "got: {headline}");
    }

    #[test]
    fn mark_oauth_windows_stale_flags_canonical_labels() {
        use crate::anthropic_oauth::{OAuthUsageResponse, QuotaBucket};
        let mut snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        };
        let usage = OAuthUsageResponse {
            five_hour: Some(QuotaBucket {
                utilization: 50.0,
                resets_at: None,
            }),
            seven_day: Some(QuotaBucket {
                utilization: 75.0,
                resets_at: None,
            }),
            seven_day_sonnet: Some(QuotaBucket {
                utilization: 20.0,
                resets_at: None,
            }),
            seven_day_opus: None,
            extra_usage: None,
        };
        let mut headline = String::new();
        apply_oauth_usage(&mut snap, &usage, Utc::now(), &mut headline);
        mark_oauth_windows_stale(&mut snap);

        assert!(snap.window(WindowKind::FiveHourRolling).unwrap().stale);
        assert!(snap.window(WindowKind::ThisWeek).unwrap().stale);
        assert!(snap.windows.get("Sonnet").unwrap().stale);
        // week (Opus) wasn't populated → no entry to mark, but the
        // helper must not panic on the missing key.
        assert!(!snap.windows.contains_key("Opus"));
    }

    #[test]
    fn fill_quota_window_handles_unparseable_resets_at() {
        // QuotaBucket whose resets_at can't be parsed — fraction
        // should still populate, ends_at falls back to None so the
        // renderer doesn't show a phantom countdown.
        use crate::anthropic_oauth::QuotaBucket;
        let now = Utc::now();
        let mut w = WindowUsage::default();
        let q = QuotaBucket {
            utilization: 12.5,
            resets_at: Some("not a date".to_string()),
        };
        fill_quota_window(&mut w, &q, now);
        assert_eq!(w.fraction_used, Some(0.125));
        assert!(w.ends_at.is_none());
        assert_eq!(w.started_at, Some(now));
    }

    #[test]
    fn fill_quota_window_zero_utilization_records_zero_not_none() {
        use crate::anthropic_oauth::QuotaBucket;
        let now = Utc::now();
        let mut w = WindowUsage::default();
        fill_quota_window(
            &mut w,
            &QuotaBucket {
                utilization: 0.0,
                resets_at: None,
            },
            now,
        );
        assert_eq!(w.fraction_used, Some(0.0));
    }

    #[test]
    fn default_projects_dir_ends_in_claude_projects() {
        let p = default_projects_dir();
        let s = p.to_string_lossy();
        assert!(s.ends_with(".claude/projects"), "got: {s}");
    }

    #[test]
    fn parses_real_schema() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("project-a");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("session.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        // Two assistant messages in the same request — must dedupe to 1.
        let line = r#"{"type":"assistant","timestamp":"2026-05-08T10:00:00.000Z","requestId":"req_1","message":{"model":"claude-opus-4-7","id":"msg_1","usage":{"input_tokens":100,"output_tokens":200,"cache_read_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":500}}}}"#;
        writeln!(out, "{}", line).unwrap();
        writeln!(out, "{}", line).unwrap(); // duplicate
                                            // Second turn, different request, same id — also distinct.
        let line2 = r#"{"type":"assistant","timestamp":"2026-05-08T10:01:00.000Z","requestId":"req_2","message":{"model":"claude-opus-4-7","id":"msg_2","usage":{"input_tokens":50,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}"#;
        writeln!(out, "{}", line2).unwrap();
        drop(out);

        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.path().to_path_buf());
        // Disable opencode merge so we only assert against the JSONL.
        let p = AnthropicProvider::with_opencode_db(cfg, None);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 30, 0).unwrap();
        let agg = p.aggregate(now).unwrap();
        assert_eq!(agg.last_hour.requests, 2);
        // 100 input + 200 output + 1000 cache_read + 500 cache_creation_1h, opus rate
        // first turn cost: (100/1M)*15 + (200/1M)*75 + (1000/1M)*15*0.1 + (500/1M)*15*2 = 0.0015+0.015+0.0015+0.015 = 0.033
        // second turn: (50/1M)*15 + (50/1M)*75 = 0.00075 + 0.00375 = 0.0045
        // total ≈ 0.0375
        assert!(
            (agg.today.cost - 0.0375).abs() < 1e-6,
            "got {}",
            agg.today.cost
        );
    }

    // ---- incremental scan tests ----
    //
    // These pin the load-bearing behaviour added to fix the 92 % CPU
    // pin: the cache short-circuits when nothing changed, picks up
    // appended lines (without re-parsing the old ones), recovers
    // gracefully from truncation, and tolerates partial trailing
    // writes.

    fn one_assistant_line(req: &str, msg: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"{req}","message":{{"model":"claude-opus-4-7","id":"{msg}","usage":{{"input_tokens":100,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}}}}}"#
        )
    }

    #[test]
    fn aggregate_caches_events_across_calls() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("s.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        writeln!(
            out,
            "{}",
            one_assistant_line("r1", "m1", "2026-05-08T10:00:00Z")
        )
        .unwrap();
        drop(out);

        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.path().to_path_buf());
        let p = AnthropicProvider::with_opencode_db(cfg, None);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 30, 0).unwrap();

        // First call: one event scanned into cache.
        let agg1 = p.aggregate(now).unwrap();
        assert_eq!(agg1.last_hour.requests, 1);
        // The cache should now hold exactly that event.
        let cache_len = p.cache.lock().unwrap().events.len();
        assert_eq!(cache_len, 1);

        // Second call with no file changes: cache reused, same agg.
        let agg2 = p.aggregate(now).unwrap();
        assert_eq!(agg2.last_hour.requests, 1);
        assert!((agg1.last_hour.cost - agg2.last_hour.cost).abs() < 1e-9);
        // Cache size unchanged — we didn't re-parse and double-push.
        assert_eq!(p.cache.lock().unwrap().events.len(), 1);
    }

    #[test]
    fn aggregate_picks_up_appended_lines_without_replaying_old_ones() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("s.jsonl");

        let mut out = std::fs::File::create(&f).unwrap();
        writeln!(
            out,
            "{}",
            one_assistant_line("r1", "m1", "2026-05-08T10:00:00Z")
        )
        .unwrap();
        drop(out);

        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.path().to_path_buf());
        let p = AnthropicProvider::with_opencode_db(cfg, None);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 30, 0).unwrap();

        let _ = p.aggregate(now).unwrap();
        let offset_after_first = p
            .cache
            .lock()
            .unwrap()
            .files
            .get(&f)
            .copied()
            .map(|s| s.offset)
            .unwrap();

        // Append a second turn to the same file.
        let mut out = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
        writeln!(
            out,
            "{}",
            one_assistant_line("r2", "m2", "2026-05-08T10:05:00Z")
        )
        .unwrap();
        drop(out);

        let agg = p.aggregate(now).unwrap();
        assert_eq!(agg.last_hour.requests, 2, "second turn must be picked up");
        // The cached offset must have advanced past the original
        // first-call value — proves we only read the appended bytes.
        let offset_after_second = p
            .cache
            .lock()
            .unwrap()
            .files
            .get(&f)
            .copied()
            .map(|s| s.offset)
            .unwrap();
        assert!(
            offset_after_second > offset_after_first,
            "offset must advance: {} → {}",
            offset_after_first,
            offset_after_second
        );
    }

    #[test]
    fn aggregate_handles_partial_trailing_line() {
        // Simulate a mid-write: the file ends without a newline, so
        // the last "line" is a partial record. The incremental
        // scanner must NOT consume it — instead leave the offset
        // before it, so the next scan (after the writer completes
        // the line) parses it as a whole.
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("s.jsonl");
        let complete = one_assistant_line("r1", "m1", "2026-05-08T10:00:00Z");
        let partial = r#"{"type":"assistant","timestamp":"2026-05-08T10:05:00Z","requestId":"r2","message":{"model":"claude-opus-4-7","id":"m2","usage":{"input_tokens":50"#;
        {
            let mut out = std::fs::File::create(&f).unwrap();
            writeln!(out, "{}", complete).unwrap();
            // No trailing newline on the partial line.
            out.write_all(partial.as_bytes()).unwrap();
        }

        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.path().to_path_buf());
        let p = AnthropicProvider::with_opencode_db(cfg, None);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 30, 0).unwrap();
        let agg = p.aggregate(now).unwrap();
        assert_eq!(
            agg.last_hour.requests, 1,
            "only the complete line should count"
        );

        // Now finish the partial line. Next scan should pick it up.
        let mut out = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
        out.write_all(
            b",\"output_tokens\":50,\"cache_read_input_tokens\":0,\"cache_creation\":{\"ephemeral_5m_input_tokens\":0,\"ephemeral_1h_input_tokens\":0}}}}\n",
        )
        .unwrap();
        drop(out);

        let agg = p.aggregate(now).unwrap();
        assert_eq!(agg.last_hour.requests, 2, "completed line should now count");
    }

    #[test]
    fn aggregate_re_reads_from_start_on_truncation() {
        // Defensive: someone manually truncated / removed-and-recreated
        // the file with the same path. The scanner detects offset > new
        // size and restarts from the top so the new contents are
        // observed correctly.
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("s.jsonl");

        {
            let mut out = std::fs::File::create(&f).unwrap();
            for i in 0..5 {
                writeln!(
                    out,
                    "{}",
                    one_assistant_line(&format!("r{i}"), &format!("m{i}"), "2026-05-08T10:00:00Z")
                )
                .unwrap();
            }
        }
        let mut cfg = AnthropicConfig::default();
        cfg.claude_projects_dir = Some(dir.path().to_path_buf());
        let p = AnthropicProvider::with_opencode_db(cfg, None);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 30, 0).unwrap();
        let agg1 = p.aggregate(now).unwrap();
        assert_eq!(agg1.last_hour.requests, 5);

        // Replace the file with a single (different) line.
        {
            let mut out = std::fs::File::create(&f).unwrap();
            writeln!(
                out,
                "{}",
                one_assistant_line("r_new", "m_new", "2026-05-08T10:10:00Z")
            )
            .unwrap();
        }

        let agg2 = p.aggregate(now).unwrap();
        // The cached 5 events stay (the cache doesn't know they're
        // gone) but the new event also lands. With dedupe by
        // (request|message) id, the new one is distinct, so total = 6.
        assert!(
            agg2.last_hour.requests >= 6,
            "truncation must not lose the new line: {:?}",
            agg2.last_hour
        );
    }
}
