//! Codex CLI on ChatGPT plan — no public API. We read the rollout JSONLs
//! under `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (and the optional
//! `~/.codex/archived_sessions/` mirror) to count tokens, requests and a
//! rough spend estimate within rolling 5-hour and 7-day windows.
//!
//! ## Observed schema (per JSONL line):
//!
//! ```text
//! {"type":"session_meta",  "timestamp":"…", "payload":{"id":"<uuid>", …}}
//! {"type":"turn_context",  "timestamp":"…", "payload":{"model":"gpt-5-codex", …}}
//! {"type":"event_msg",     "timestamp":"…", "payload":{
//!     "type":"token_count",
//!     "info":{
//!       "last_token_usage":  {"input_tokens":N,"output_tokens":N,
//!                             "cached_input_tokens":N,"total_tokens":N},
//!       "total_token_usage": { … cumulative … }
//!     }
//! }}
//! ```
//!
//! `last_token_usage` is the per-turn delta; `total_token_usage` is the
//! cumulative-since-session-start rollup. We prefer the delta. Note:
//! OpenAI's `input_tokens` field already includes `cached_input_tokens`
//! as a subset, so for billing we use `(input - cached)` at the regular
//! rate plus `cached` at the cached rate.
//!
//! ## Quota / rate-limit snapshot
//!
//! Same `event_msg{token_count}` payload also carries the live rate-limit
//! state straight from OpenAI's API response:
//!
//! ```json
//! {"payload": {"type":"token_count", "info": null,
//!   "rate_limits": {
//!     "limit_id":"codex","limit_name":null,
//!     "primary":   {"used_percent":1.0,  "window_minutes":300,   "resets_at":1778387659},
//!     "secondary": {"used_percent":17.0, "window_minutes":10080, "resets_at":1778649999},
//!     "credits":null, "plan_type":"plus", "rate_limit_reached_type":null
//!   }}}
//! ```
//!
//! `primary.window_minutes == 300` is the 5-hour rolling window;
//! `secondary.window_minutes == 10080` is the 7-day one. We take the
//! single newest snapshot across all rollouts as authoritative and feed
//! `used_percent` into each window's `fraction_used`. If `resets_at` is
//! already in the past relative to *now*, the window has rolled over
//! since that snapshot was written and we clamp fraction to 0 — the
//! user must run Codex again for a fresh number.
//!
//! Schema reverse-engineered against Codex CLI ~0.40.x. Confirmed by
//! cross-reading [soulduse/ai-token-monitor]'s `codex.rs` parser.

use crate::config::CodexCliConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;
use walkdir::WalkDir;

/// Per-file resumption state for the Codex rollouts incremental
/// scanner. Mirrors the Anthropic provider's `FileState` but also
/// carries the stateful per-file parser context (`current_model`
/// from the running `turn_context`, last seen token snapshot for the
/// "no progress this turn" filter).
#[derive(Default, Clone)]
struct FileState {
    #[allow(dead_code)]
    size: u64,
    offset: u64,
    current_model: String,
    prev_snapshot: Option<(u64, u64, u64)>,
}

/// Stop tracking events older than this. The longest Codex bucket is
/// "this week" (7 d) so a comfortable retention buffer keeps the
/// trim cheap without losing in-window data.
const EVENT_RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);

#[derive(Default)]
struct ScanCache {
    events: Vec<TokenEvent>,
    files: HashMap<PathBuf, FileState>,
    latest_rate_limits: Option<RateLimitsRecord>,
}

pub struct CodexCliProvider {
    cfg: CodexCliConfig,
    codex_dir: PathBuf,
    /// Resolved opencode SQLite path; `None` when explicitly disabled.
    opencode_db: Option<PathBuf>,
    /// Incremental scan cache — same rationale as the Anthropic
    /// provider. The old `collect_events` walked every rollout and
    /// re-parsed every line on every poll, which the file-watcher
    /// firing on every assistant turn turned into a tight re-walk
    /// loop. Cache stores TokenEvents + latest rate_limits + per-file
    /// offsets so subsequent polls only read appended bytes.
    cache: Mutex<ScanCache>,
}

impl CodexCliProvider {
    /// Construct with the default opencode path resolution
    /// (`~/.local/share/opencode/opencode.db`). Tests that want to
    /// inject a fixture should use `with_opencode_db`.
    pub fn new(cfg: CodexCliConfig) -> Self {
        let codex_dir = cfg.codex_dir.clone().unwrap_or_else(default_codex_dir);
        let opencode_db = Some(crate::opencode::default_db_path());
        Self {
            cfg,
            codex_dir,
            opencode_db,
            cache: Mutex::new(ScanCache::default()),
        }
    }

    /// Construct with an explicit opencode override:
    /// - `Some(path)` to read that file
    /// - `Some(empty)` or `None` to disable the integration entirely
    pub fn with_opencode_db(cfg: CodexCliConfig, opencode_db: Option<PathBuf>) -> Self {
        let codex_dir = cfg.codex_dir.clone().unwrap_or_else(default_codex_dir);
        let opencode_db = opencode_db.filter(|p| !p.as_os_str().is_empty());
        Self {
            cfg,
            codex_dir,
            opencode_db,
            cache: Mutex::new(ScanCache::default()),
        }
    }

    fn collect_events(&self) -> Result<Collected> {
        let mut out = Collected::default();
        if self.codex_dir.exists() {
            let mut cache = self.cache.lock().expect("poisoned");
            // Only the active sessions dir matters for rolling windows up to 7d;
            // we walk archived_sessions too in case ours is configured oddly.
            for sub in ["sessions", "archived_sessions"] {
                let root = self.codex_dir.join(sub);
                if !root.exists() {
                    continue;
                }
                for entry in WalkDir::new(&root)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
                {
                    let path = entry.path().to_path_buf();
                    if let Err(e) = scan_codex_file_incremental(&path, &mut cache) {
                        tracing::warn!(path = %path.display(), error = %e, "codex parse failed");
                    }
                }
            }
            // Trim out-of-window events so the cache doesn't grow
            // unboundedly across long tray sessions.
            let cutoff = Utc::now() - chrono::Duration::from_std(EVENT_RETENTION).unwrap();
            cache.events.retain(|e| e.timestamp > cutoff);
            // Clone the cached state into the outbound `Collected`.
            out.events.extend(cache.events.iter().cloned());
            out.latest_rate_limits = cache.latest_rate_limits.clone();
        }

        // Supplementary source: opencode's SQLite store. Users who hit
        // OpenAI via opencode (rather than the codex CLI directly)
        // won't have fresh rollouts; their token activity lives here
        // instead. opencode doesn't expose rate-limit headers, so the
        // quota fractions still have to come from rollouts when they
        // exist.
        if let Some(db) = &self.opencode_db {
            match crate::opencode::read_events(db, "openai") {
                Ok(events) => out
                    .events
                    .extend(events.into_iter().map(|e| TokenEvent {
                        timestamp: e.timestamp,
                        model: if e.model.is_empty() { "openai".into() } else { e.model },
                        input_tokens: e.input_tokens,
                        output_tokens: e.output_tokens,
                        cached_tokens: e.cached_tokens,
                    })),
                Err(e) => {
                    tracing::warn!(error = %e, path = %db.display(), "opencode db read failed");
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Default)]
struct Collected {
    events: Vec<TokenEvent>,
    /// Newest `rate_limits` snapshot seen across every rollout, keyed on
    /// the timestamp of the carrying `event_msg`. Used to set
    /// fraction_used + ends_at on the two windows.
    latest_rate_limits: Option<RateLimitsRecord>,
}

#[derive(Debug, Clone)]
struct RateLimitsRecord {
    /// When the rollout line that carried this snapshot was written.
    /// Used both to pick the newest record and to decide whether
    /// `resets_at` has lapsed since.
    record_at: DateTime<Utc>,
    primary: Option<RateLimitsBucket>,
    secondary: Option<RateLimitsBucket>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct RateLimitsBucket {
    used_percent: f64,
    window_minutes: u32,
    resets_at: Option<DateTime<Utc>>,
}

#[async_trait]
impl Provider for CodexCliProvider {
    fn id(&self) -> ProviderId {
        ProviderId::CodexCli
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        let now = Utc::now();
        if !self.codex_dir.exists() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::CodexCli,
                format!("no codex dir at {}", self.codex_dir.display()),
            ));
        }

        let collected = self.collect_events()?;
        if collected.events.is_empty() && collected.latest_rate_limits.is_none() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::CodexCli,
                "no codex sessions found",
            ));
        }

        let five_hour_cutoff = now - Duration::hours(5);
        let week_cutoff = now - Duration::days(7);

        let mut bucket_5h = Bucket::default();
        let mut bucket_7d = Bucket::default();
        for e in &collected.events {
            if e.timestamp > five_hour_cutoff {
                bucket_5h.add(e);
            }
            if e.timestamp > week_cutoff {
                bucket_7d.add(e);
            }
        }

        let mut snap = UsageSnapshot {
            provider: ProviderId::CodexCli,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
            plan_label: None,
        };

        let w5 = snap.window_mut(WindowKind::FiveHourRolling);
        w5.tokens_in = bucket_5h.input_tokens;
        w5.tokens_out = bucket_5h.output_tokens;
        w5.request_count = bucket_5h.turns;
        w5.spend_usd = Some(bucket_5h.cost_usd);

        let ww = snap.window_mut(WindowKind::ThisWeek);
        ww.tokens_in = bucket_7d.input_tokens;
        ww.tokens_out = bucket_7d.output_tokens;
        ww.request_count = bucket_7d.turns;
        ww.spend_usd = Some(bucket_7d.cost_usd);

        // Apply the most recent rate-limit snapshot we observed. Codex
        // emits these on every API turn; even when the snapshot's
        // declared `resets_at` has already passed, we keep the last
        // known fraction in place — the user can't run Codex if
        // they're locked out, so the API never produces a fresher
        // record. Renderers detect the stale state from `ends_at`
        // and surface a warning marker after a short grace period.
        let mut plan_label: Option<String> = None;
        if let Some(rl) = &collected.latest_rate_limits {
            plan_label = rl.plan_type.clone();
            if let Some(primary) = rl.primary {
                apply_rate_limits(
                    snap.window_mut(WindowKind::FiveHourRolling),
                    primary,
                    now,
                );
            }
            if let Some(secondary) = rl.secondary {
                apply_rate_limits(snap.window_mut(WindowKind::ThisWeek), secondary, now);
            }
            if rl.rate_limit_reached_type.is_some() {
                snap.status = ProviderStatus::Degraded;
                snap.error = Some(format!(
                    "rate limit hit: {}",
                    rl.rate_limit_reached_type.as_deref().unwrap_or("?")
                ));
            }
        }

        snap.plan_label = plan_label
            .as_deref()
            .map(crate::model::title_case_first);
        snap.headline = Some(build_headline(
            plan_label.as_deref(),
            snap.window(WindowKind::FiveHourRolling),
            snap.window(WindowKind::ThisWeek),
            bucket_5h.turns,
            bucket_7d.turns,
            self.cfg.show_spend.then_some(bucket_7d.cost_usd),
        ));
        if !self.cfg.show_spend {
            snap.strip_spend();
        }
        Ok(snap)
    }
}

/// How long after a Codex rate-limits record's declared `resets_at`
/// we keep treating the recorded fraction as "current" before
/// flipping the `stale` flag. Five minutes is the rough latency
/// between the API window rolling over and the user's next codex CLI
/// invocation that would refresh the snapshot — short enough not to
/// hide real staleness, long enough to absorb a clock-skew tick.
const STALE_GRACE_SECS: i64 = 5 * 60;

fn apply_rate_limits(
    w: &mut crate::model::WindowUsage,
    bucket: RateLimitsBucket,
    now: DateTime<Utc>,
) {
    // Always surface the last known fraction (clamped to 0..1). When
    // resets_at is in the past the data is stale, but the most useful
    // thing is still "this is what we last saw": a user who hit 100 %
    // and is still locked out wants to see "100 %", not a blank row.
    //
    // The `stale` flag tells downstream renderers to swap the reset
    // countdown for a ⚠ marker. `STALE_GRACE_SECS` covers the gap
    // between the declared reset moment and the next time the user
    // actually runs codex CLI (which is what refreshes the data).
    let raw_frac = (bucket.used_percent / 100.0).clamp(0.0, 1.0);
    w.fraction_used = Some(raw_frac);
    w.ends_at = bucket.resets_at;
    w.started_at = Some(now);
    w.stale = bucket
        .resets_at
        .is_some_and(|t| (now - t).num_seconds() > STALE_GRACE_SECS);
    let _ = bucket.window_minutes; // kept for future labelling
}

fn build_headline(
    plan: Option<&str>,
    w5: Option<&crate::model::WindowUsage>,
    w7: Option<&crate::model::WindowUsage>,
    turns_5h: u64,
    turns_7d: u64,
    spend_7d: Option<f64>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = plan {
        parts.push(p.to_string());
    }
    if let Some(w) = w5 {
        if let Some(frac) = w.fraction_used {
            parts.push(format!(
                "5h {:.0}%{}",
                frac * 100.0,
                reset_suffix(w.ends_at)
            ));
        } else {
            parts.push(format!("{} turns / 5h", turns_5h));
        }
    }
    if let Some(w) = w7 {
        if let Some(frac) = w.fraction_used {
            parts.push(format!(
                "7d {:.0}%{}",
                frac * 100.0,
                reset_suffix(w.ends_at)
            ));
        } else {
            parts.push(format!("{} turns / 7d", turns_7d));
        }
    }
    if let Some(spend) = spend_7d {
        parts.push(format!("${:.2}", spend));
    }
    parts.join(" · ")
}

fn reset_suffix(ends_at: Option<DateTime<Utc>>) -> String {
    let Some(t) = ends_at else { return String::new() };
    let now = Utc::now();
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
}

fn default_codex_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}


#[derive(Debug, Default)]
struct Bucket {
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cost_usd: f64,
}

impl Bucket {
    fn add(&mut self, e: &TokenEvent) {
        if e.input_tokens == 0 && e.output_tokens == 0 && e.cached_tokens == 0 {
            return;
        }
        self.turns += 1;
        self.input_tokens = self.input_tokens.saturating_add(e.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(e.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(e.cached_tokens);
        self.cost_usd += codex_cost_usd(&e.model, e.input_tokens, e.output_tokens, e.cached_tokens);
    }
}

#[derive(Debug, Clone)]
struct TokenEvent {
    timestamp: DateTime<Utc>,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OuterEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<String>,
    payload: Option<Value>,
}

fn scan_codex_file_incremental(
    path: &std::path::Path,
    cache: &mut ScanCache,
) -> Result<()> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let prev = cache.files.get(path).cloned().unwrap_or_default();
    let start = if prev.offset > size {
        // File was rotated / truncated — restart from the top and
        // drop the carried-over per-file parser state.
        FileState::default().offset
    } else if prev.offset == size {
        return Ok(());
    } else {
        prev.offset
    };

    // If we're restarting from 0 we need fresh per-file state; if
    // we're resuming we carry the previous `current_model` and
    // `prev_snapshot` so deduplication stays correct across calls.
    let mut state = if start == 0 {
        FileState { size, offset: 0, ..Default::default() }
    } else {
        let mut s = prev.clone();
        s.size = size;
        s
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
            // Partial trailing line — leave offset before it so the
            // next scan picks the line up once Codex finishes
            // writing it.
            break;
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            process_codex_line(trimmed, &mut state, cache);
        }
        consumed += n as u64;
    }
    state.offset = consumed;
    cache.files.insert(path.to_path_buf(), state);
    Ok(())
}

/// Per-line parser. Pure (no I/O) so we can fuzz it in tests later.
/// Updates `state.current_model` on `turn_context`, dedupes
/// consecutive identical `token_count` snapshots, and pushes
/// `TokenEvent` / updates `latest_rate_limits` on the cache.
fn process_codex_line(line: &str, state: &mut FileState, cache: &mut ScanCache) {
    let Ok(entry) = serde_json::from_str::<OuterEntry>(line) else {
        return;
    };
    let Some(payload) = entry.payload else { return };
    let record_ts = entry
        .timestamp
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)));

    match entry.entry_type.as_deref() {
        Some("turn_context") => {
            if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
                state.current_model = m.to_string();
            }
        }
        Some("event_msg") => {
            if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
                return;
            }

            // Rate-limit snapshot, if present. Lives on the
            // event_msg payload itself, not under `info`.
            if let Some(rl) = payload.get("rate_limits") {
                if let (Some(ts), Some(record)) =
                    (record_ts, parse_rate_limits(rl, record_ts))
                {
                    maybe_update_latest_rate_limits(&mut cache.latest_rate_limits, ts, record);
                }
            }

            let info = payload.get("info");
            let usage = info
                .filter(|v| !v.is_null())
                .and_then(|i| i.get("last_token_usage").or_else(|| i.get("total_token_usage")));
            let Some(u) = usage else { return };
            let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cached = u.get("cached_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            if input == 0 && output == 0 && cached == 0 {
                return;
            }
            // Skip duplicate consecutive snapshots — Codex emits cumulative
            // total_token_usage on each event so identical snapshots in a row
            // mean "no progress this turn".
            let snap = (input, output, cached);
            if state.prev_snapshot == Some(snap) {
                return;
            }
            state.prev_snapshot = Some(snap);

            let Some(ts) = record_ts else { return };

            cache.events.push(TokenEvent {
                timestamp: ts,
                model: if state.current_model.is_empty() {
                    "gpt-5-codex".into()
                } else {
                    state.current_model.clone()
                },
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
            });
        }
        _ => {}
    }
}

/// Decide whether to install `record` as the new `latest_rate_limits`.
/// The naive rule "newer record_at wins" produces visible flipping
/// around quota-exhaustion time on Codex CLI v0.129+: that version
/// alternates between bucket-bearing records (`primary: {used_percent
/// : 82.0, ...}`) and "credits-only" records (`primary: null,
/// secondary: null` with a sibling `credits` block). Letting a
/// credits-only record blow away a meaningful cached one strips the
/// snapshot's `fraction_used` for that poll; the runtime's
/// `merge_stale_from` then grafts back the previous fraction with
/// `stale = true`, only to flip back to "fresh" on the next
/// bucket-bearing record. Net effect: the user sees the bar tier
/// oscillating between live colours and the stale grey poll-by-poll
/// despite no real change in their quota state.
///
/// Two rules:
///
/// 1. Always reject older records (strict `>` on `record_at`).
/// 2. Reject a newer record that has NEITHER primary nor secondary
///    buckets when the cached one has at least one. The cached
///    record's `plan_type` and `rate_limit_reached_type` are
///    preserved alongside its buckets, which is what the caller
///    actually renders. (We still accept a null-null record when
///    the cache itself is empty or null-null — the original
///    "fresh session supersedes ancient stale" rationale still
///    applies in that case.)
fn maybe_update_latest_rate_limits(
    slot: &mut Option<RateLimitsRecord>,
    new_at: DateTime<Utc>,
    new_record: RateLimitsRecord,
) {
    let take = match slot {
        None => true,
        Some(prev) => {
            let is_strictly_newer = new_at > prev.record_at;
            let demotes_to_null_null = new_record.primary.is_none()
                && new_record.secondary.is_none()
                && (prev.primary.is_some() || prev.secondary.is_some());
            is_strictly_newer && !demotes_to_null_null
        }
    };
    if take {
        *slot = Some(new_record);
    }
}

fn parse_rate_limits(v: &Value, record_at: Option<DateTime<Utc>>) -> Option<RateLimitsRecord> {
    let record_at = record_at?;
    let primary = v.get("primary").and_then(parse_rate_limits_bucket);
    let secondary = v.get("secondary").and_then(parse_rate_limits_bucket);
    // Accept records even with null buckets — Codex CLI v0.129.0+ emits
    // rate_limits with `primary:null, secondary:null` and a `credits`
    // block. We must track these so the newest record correctly supersedes
    // stale data from older sessions, otherwise we'd show phantom
    // percentages from days ago.
    Some(RateLimitsRecord {
        record_at,
        primary,
        secondary,
        plan_type: v
            .get("plan_type")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        rate_limit_reached_type: v
            .get("rate_limit_reached_type")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    })
}

fn parse_rate_limits_bucket(v: &Value) -> Option<RateLimitsBucket> {
    let used_percent = v.get("used_percent")?.as_f64()?;
    let window_minutes = v.get("window_minutes")?.as_u64()? as u32;
    let resets_at = v
        .get("resets_at")
        .and_then(|x| x.as_i64())
        .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
    Some(RateLimitsBucket {
        used_percent,
        window_minutes,
        resets_at,
    })
}

/// Conservative pricing defaults (USD per 1M tokens). User can override
/// via config in a future iteration. OpenAI's `input_tokens` includes
/// `cached_input_tokens` as a subset, so we charge `(input - cached)` at
/// the regular input rate and `cached` at the cached rate.
fn codex_cost_usd(model: &str, input: u64, output: u64, cached: u64) -> f64 {
    let m = model.to_ascii_lowercase();
    let (i_rate, o_rate, c_rate) = if m.contains("gpt-5") || m.contains("codex") {
        (1.25, 10.0, 0.125)
    } else if m.contains("gpt-4o") {
        (2.5, 10.0, 1.25)
    } else {
        (1.0, 4.0, 0.5)
    };
    let billed_input = input.saturating_sub(cached);
    (billed_input as f64 * i_rate
        + output as f64 * o_rate
        + cached as f64 * c_rate)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parses_real_codex_schema() {
        let dir = TempDir::new().unwrap();
        let day = dir.path().join("sessions/2026/05/08");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-test.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        writeln!(out, r#"{{"type":"session_meta","timestamp":"2026-05-08T10:00:00Z","payload":{{"id":"sess-1"}}}}"#).unwrap();
        writeln!(out, r#"{{"type":"turn_context","timestamp":"2026-05-08T10:00:01Z","payload":{{"model":"gpt-5-codex"}}}}"#).unwrap();
        writeln!(out, r#"{{"type":"event_msg","timestamp":"2026-05-08T10:00:05Z","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1000,"output_tokens":200,"cached_input_tokens":100,"total_tokens":1200}}}}}}}}"#).unwrap();
        // Duplicate snapshot — should be skipped.
        writeln!(out, r#"{{"type":"event_msg","timestamp":"2026-05-08T10:00:06Z","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1000,"output_tokens":200,"cached_input_tokens":100,"total_tokens":1200}}}}}}}}"#).unwrap();
        // Different snapshot — counted.
        writeln!(out, r#"{{"type":"event_msg","timestamp":"2026-05-08T10:00:09Z","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":50,"output_tokens":50,"cached_input_tokens":0,"total_tokens":100}}}}}}}}"#).unwrap();
        drop(out);

        let mut cfg = CodexCliConfig::default();
        cfg.codex_dir = Some(dir.path().to_path_buf());
        // Disable opencode merge for this fixture-driven test.
        let p = CodexCliProvider::with_opencode_db(cfg, None);
        let collected = p.collect_events().unwrap();
        assert_eq!(
            collected.events.len(),
            2,
            "should dedupe consecutive identical snapshots"
        );
        assert_eq!(collected.events[0].input_tokens, 1000);
        assert_eq!(collected.events[0].cached_tokens, 100);
        assert_eq!(collected.events[1].input_tokens, 50);
    }

    #[test]
    fn codex_cost_per_model_tier() {
        // gpt-5 / codex use the same rate.
        let c = codex_cost_usd("gpt-5-codex", 1_000_000, 0, 0);
        let expected = 1.25;
        assert!((c - expected).abs() < 1e-9, "got {}", c);
        // gpt-4o uses the higher-cost row.
        let c = codex_cost_usd("gpt-4o", 1_000_000, 0, 0);
        let expected = 2.5;
        assert!((c - expected).abs() < 1e-9, "got {}", c);
        // Unknown model falls back.
        let c = codex_cost_usd("mystery-model", 1_000_000, 0, 0);
        assert!((c - 1.0).abs() < 1e-9, "got {}", c);
    }

    #[test]
    fn codex_cost_charges_cached_tokens_separately() {
        // input_tokens INCLUDES cached_input_tokens per OpenAI convention;
        // we should bill (input - cached) at the regular rate and `cached`
        // at the cached rate.
        let c = codex_cost_usd("gpt-5", /* input */ 1_000_000, /* output */ 0, /* cached */ 500_000);
        // billed_input = 500_000 → 500_000 * 1.25/1e6 = 0.625
        // cached = 500_000 → 500_000 * 0.125/1e6 = 0.0625
        // total = 0.6875
        assert!((c - 0.6875).abs() < 1e-9, "got {}", c);
    }

    #[test]
    fn codex_cost_output_is_pricier_than_input() {
        // sanity: output rate > input rate for gpt-5.
        let in_cost = codex_cost_usd("gpt-5", 1_000_000, 0, 0);
        let out_cost = codex_cost_usd("gpt-5", 0, 1_000_000, 0);
        assert!(out_cost > in_cost, "out {} should be > in {}", out_cost, in_cost);
    }

    #[test]
    fn codex_cost_handles_saturating_subtraction() {
        // cached > input is invalid but should not panic — the formula
        // uses saturating_sub so billed_input goes to zero.
        let c = codex_cost_usd("gpt-5", 100, 0, 1_000_000);
        // billed_input = 0; cached charged: 1M * 0.125 / 1M = 0.125.
        assert!((c - 0.125).abs() < 1e-9, "got {}", c);
    }

    #[test]
    fn bucket_skips_zero_event() {
        let mut b = Bucket::default();
        let e = TokenEvent {
            timestamp: Utc::now(),
            model: "gpt-5".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
        };
        b.add(&e);
        assert_eq!(b.turns, 0);
    }

    #[test]
    fn bucket_aggregates_multiple_events() {
        let mut b = Bucket::default();
        let now = Utc::now();
        for _ in 0..3 {
            b.add(&TokenEvent {
                timestamp: now,
                model: "gpt-5".into(),
                input_tokens: 100,
                output_tokens: 50,
                cached_tokens: 10,
            });
        }
        assert_eq!(b.turns, 3);
        assert_eq!(b.input_tokens, 300);
        assert_eq!(b.output_tokens, 150);
        assert_eq!(b.cached_tokens, 30);
        assert!(b.cost_usd > 0.0);
    }

    #[test]
    fn parses_real_rate_limits_schema() {
        let dir = TempDir::new().unwrap();
        let day = dir.path().join("sessions/2026/05/09");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-rl.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        // Real shape from a live ollama.com user's rollout: rate_limits
        // sits on the event_msg payload alongside `info`, NOT inside it.
        writeln!(
            out,
            r#"{{"timestamp":"2026-05-09T23:34:19.205Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","limit_name":null,"primary":{{"used_percent":1.0,"window_minutes":300,"resets_at":1778387659}},"secondary":{{"used_percent":17.0,"window_minutes":10080,"resets_at":1778649999}},"credits":null,"plan_type":"plus","rate_limit_reached_type":null}}}}}}"#
        )
        .unwrap();
        // Older rollout with a *smaller* used_percent — must NOT override
        // the newer record above.
        writeln!(
            out,
            r#"{{"timestamp":"2026-05-09T20:00:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":50.0,"window_minutes":300,"resets_at":1778000000}},"plan_type":"plus"}}}}}}"#
        )
        .unwrap();
        drop(out);

        let mut cfg = CodexCliConfig::default();
        cfg.codex_dir = Some(dir.path().to_path_buf());
        let p = CodexCliProvider::with_opencode_db(cfg, None);
        let collected = p.collect_events().unwrap();
        let rl = collected
            .latest_rate_limits
            .expect("rate_limits should be captured");
        assert_eq!(rl.plan_type.as_deref(), Some("plus"));
        // Newest record (23:34Z) wins, not the older 50% one.
        let primary = rl.primary.expect("primary present");
        assert!((primary.used_percent - 1.0).abs() < 1e-6);
        assert_eq!(primary.window_minutes, 300);
        assert_eq!(
            primary.resets_at,
            Utc.timestamp_opt(1778387659, 0).single()
        );
        let secondary = rl.secondary.expect("secondary present");
        assert!((secondary.used_percent - 17.0).abs() < 1e-6);
        assert_eq!(secondary.window_minutes, 10080);
    }

    // ---- parse_rate_limits_bucket: corner cases ----

    fn json(v: serde_json::Value) -> serde_json::Value {
        v
    }

    #[test]
    fn bucket_missing_used_percent_returns_none() {
        let v = json(serde_json::json!({"window_minutes": 300, "resets_at": 1778000000}));
        assert!(parse_rate_limits_bucket(&v).is_none());
    }

    #[test]
    fn bucket_missing_window_minutes_returns_none() {
        let v = json(serde_json::json!({"used_percent": 12.0, "resets_at": 1778000000}));
        assert!(parse_rate_limits_bucket(&v).is_none());
    }

    #[test]
    fn bucket_null_resets_at_is_tolerated() {
        // v0.129+ sometimes emits resets_at: null alongside otherwise
        // valid bucket data. We accept the percentage/window and
        // leave the reset time unset.
        let v = json(serde_json::json!({
            "used_percent": 42.0,
            "window_minutes": 60,
            "resets_at": null,
        }));
        let b = parse_rate_limits_bucket(&v).unwrap();
        assert!((b.used_percent - 42.0).abs() < 1e-6);
        assert_eq!(b.window_minutes, 60);
        assert!(b.resets_at.is_none());
    }

    #[test]
    fn bucket_non_numeric_used_percent_returns_none() {
        // Defensive: an upstream typo emitting a string instead of a
        // number should not panic — `parse_rate_limits_bucket` returns
        // None so the record falls through to the parent guard.
        let v = json(serde_json::json!({
            "used_percent": "27.5",
            "window_minutes": 300,
        }));
        assert!(parse_rate_limits_bucket(&v).is_none());
    }

    // ---- parse_rate_limits (outer record): corner cases ----

    #[test]
    fn record_missing_record_at_returns_none() {
        // No timestamp at all → we can't place the record in time, so
        // reject the whole thing. Otherwise it'd silently win over a
        // record with a real timestamp later.
        let v = json(serde_json::json!({
            "primary": {"used_percent": 1.0, "window_minutes": 300, "resets_at": 1778000000},
        }));
        assert!(parse_rate_limits(&v, None).is_none());
    }

    // ---- maybe_update_latest_rate_limits ----
    //
    // Regression tests for the "Codex bar flipping at quota
    // exhaustion" symptom: Codex CLI v0.129+ alternates between
    // bucket-bearing records and credits-only (null/null) records,
    // and the previous "newer wins unconditionally" rule made the
    // bar oscillate between fresh + stale every other poll.

    fn rec_with_buckets(record_at: DateTime<Utc>, primary_pct: f64) -> RateLimitsRecord {
        RateLimitsRecord {
            record_at,
            primary: Some(RateLimitsBucket {
                used_percent: primary_pct,
                window_minutes: 300,
                resets_at: Some(record_at + chrono::Duration::hours(2)),
            }),
            secondary: Some(RateLimitsBucket {
                used_percent: 50.0,
                window_minutes: 10_080,
                resets_at: Some(record_at + chrono::Duration::days(3)),
            }),
            plan_type: Some("plus".into()),
            rate_limit_reached_type: None,
        }
    }

    fn rec_null_null(record_at: DateTime<Utc>) -> RateLimitsRecord {
        RateLimitsRecord {
            record_at,
            primary: None,
            secondary: None,
            plan_type: Some("plus".into()),
            rate_limit_reached_type: None,
        }
    }

    #[test]
    fn update_latest_rate_limits_takes_first_record() {
        let mut slot: Option<RateLimitsRecord> = None;
        let now = Utc::now();
        maybe_update_latest_rate_limits(&mut slot, now, rec_with_buckets(now, 80.0));
        assert!(slot.as_ref().unwrap().primary.is_some());
    }

    #[test]
    fn update_latest_rate_limits_takes_newer_buckets() {
        let now = Utc::now();
        let mut slot = Some(rec_with_buckets(now - chrono::Duration::seconds(60), 70.0));
        maybe_update_latest_rate_limits(&mut slot, now, rec_with_buckets(now, 80.0));
        let p = slot.as_ref().unwrap().primary.unwrap();
        assert!((p.used_percent - 80.0).abs() < 1e-9);
    }

    #[test]
    fn update_latest_rate_limits_rejects_older_record() {
        let now = Utc::now();
        let mut slot = Some(rec_with_buckets(now, 80.0));
        let older_at = now - chrono::Duration::seconds(60);
        maybe_update_latest_rate_limits(&mut slot, older_at, rec_with_buckets(older_at, 50.0));
        let p = slot.as_ref().unwrap().primary.unwrap();
        assert!((p.used_percent - 80.0).abs() < 1e-9, "older record must not win");
    }

    #[test]
    fn update_latest_rate_limits_keeps_buckets_when_newer_is_null_null() {
        // The flip-fix regression test. Cache has fresh 80 %; a newer
        // credits-only record arrives. We must NOT install the
        // null-null record over the meaningful one — otherwise the
        // next call to `apply_rate_limits` does nothing and the
        // bar disappears for the poll.
        let now = Utc::now();
        let mut slot = Some(rec_with_buckets(now - chrono::Duration::seconds(30), 80.0));
        maybe_update_latest_rate_limits(&mut slot, now, rec_null_null(now));
        let kept = slot.as_ref().unwrap();
        assert!(kept.primary.is_some(), "buckets must be preserved");
        let p = kept.primary.unwrap();
        assert!((p.used_percent - 80.0).abs() < 1e-9);
    }

    #[test]
    fn update_latest_rate_limits_accepts_null_null_when_cache_is_already_null_null() {
        // The original "fresh session supersedes ancient stale"
        // rationale: when the cached record itself has no buckets,
        // a fresh null-null is a legitimate signal that the prior
        // session's data is gone.
        let now = Utc::now();
        let old = now - chrono::Duration::days(3);
        let mut slot = Some(rec_null_null(old));
        maybe_update_latest_rate_limits(&mut slot, now, rec_null_null(now));
        assert_eq!(slot.as_ref().unwrap().record_at, now);
    }

    #[test]
    fn update_latest_rate_limits_takes_newer_record_when_it_has_buckets() {
        // Symmetric to the keep-meaningful case: a newer record
        // that DOES have buckets always wins, even if the cache
        // also had buckets.
        let now = Utc::now();
        let mut slot = Some(rec_with_buckets(now - chrono::Duration::seconds(30), 80.0));
        maybe_update_latest_rate_limits(&mut slot, now, rec_with_buckets(now, 85.0));
        let p = slot.as_ref().unwrap().primary.unwrap();
        assert!((p.used_percent - 85.0).abs() < 1e-9);
    }

    #[test]
    fn record_with_only_credits_and_null_buckets_is_kept() {
        // Codex CLI v0.129+ emits rate_limits with both buckets null
        // (plus a `credits` block) when the user is on a plan that
        // surfaces credits. We must keep the record (so it
        // supersedes any earlier numbers) even though it carries no
        // percentage data.
        let v = json(serde_json::json!({
            "primary": null,
            "secondary": null,
            "credits": {"used_pct": 0.0},
            "plan_type": "plus",
        }));
        let now = Utc::now();
        let rec = parse_rate_limits(&v, Some(now)).unwrap();
        assert!(rec.primary.is_none());
        assert!(rec.secondary.is_none());
        assert_eq!(rec.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn record_propagates_rate_limit_reached_type() {
        let v = json(serde_json::json!({
            "primary": null,
            "secondary": null,
            "rate_limit_reached_type": "session",
            "plan_type": "plus",
        }));
        let rec = parse_rate_limits(&v, Some(Utc::now())).unwrap();
        assert_eq!(rec.rate_limit_reached_type.as_deref(), Some("session"));
    }

    #[test]
    fn record_with_only_primary_is_valid() {
        // Older CLI versions don't emit a secondary bucket at all.
        let v = json(serde_json::json!({
            "primary": {"used_percent": 33.0, "window_minutes": 300, "resets_at": 1778000000},
        }));
        let rec = parse_rate_limits(&v, Some(Utc::now())).unwrap();
        assert!(rec.primary.is_some());
        assert!(rec.secondary.is_none());
        assert!(rec.plan_type.is_none());
    }

    #[test]
    fn record_with_malformed_primary_drops_only_that_bucket() {
        // primary is unparseable (missing fields) → drop primary but
        // keep secondary and the rest of the record.
        let v = json(serde_json::json!({
            "primary": {"used_percent": 1.0},  // window_minutes missing
            "secondary": {"used_percent": 50.0, "window_minutes": 10080, "resets_at": 1778000000},
            "plan_type": "plus",
        }));
        let rec = parse_rate_limits(&v, Some(Utc::now())).unwrap();
        assert!(rec.primary.is_none(), "malformed primary dropped");
        assert!(rec.secondary.is_some());
    }

    /// Builds a minimal opencode-shaped SQLite file at `path` with two
    /// messages: one user (which the parser should skip) and one
    /// assistant in the requested `provider_id`.
    fn write_opencode_fixture(
        path: &std::path::Path,
        provider_id: &str,
        completed_ms: i64,
        input: u64,
        output: u64,
    ) {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        let user_data = serde_json::json!({
            "role": "user",
            "time": {"created": completed_ms - 1000},
        })
        .to_string();
        let asst_data = serde_json::json!({
            "role": "assistant",
            "providerID": provider_id,
            "modelID": "gpt-5.5",
            "time": {"created": completed_ms - 500, "completed": completed_ms},
            "tokens": {
                "input": input,
                "output": output,
                "cache": {"read": 0, "write": 0},
                "reasoning": 0,
                "total": input + output,
            },
            "cost": 0,
        })
        .to_string();
        conn.execute(
            "INSERT INTO message VALUES (?,?,?,?,?)",
            rusqlite::params!["u1", "ses1", completed_ms - 1000, completed_ms - 1000, user_data],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?,?,?,?,?)",
            rusqlite::params!["a1", "ses1", completed_ms - 500, completed_ms, asst_data],
        )
        .unwrap();
    }

    #[test]
    fn collect_events_merges_opencode_into_rollouts() {
        let dir = TempDir::new().unwrap();
        // Codex rollout with one event.
        let day = dir.path().join("sessions/2026/05/08");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join("rollout-test.jsonl");
        let mut out = std::fs::File::create(&f).unwrap();
        let recent = chrono::Utc::now() - chrono::Duration::minutes(5);
        let ts = recent.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        writeln!(
            out,
            r#"{{"type":"turn_context","timestamp":"{ts}","payload":{{"model":"gpt-5-codex"}}}}"#
        )
        .unwrap();
        writeln!(
            out,
            r#"{{"type":"event_msg","timestamp":"{ts}","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":100,"output_tokens":50,"cached_input_tokens":0,"total_tokens":150}}}}}}}}"#
        )
        .unwrap();
        drop(out);

        // opencode db with an openai assistant message.
        let db = dir.path().join("opencode.db");
        write_opencode_fixture(
            &db,
            "openai",
            chrono::Utc::now().timestamp_millis(),
            500,
            120,
        );

        let mut cfg = CodexCliConfig::default();
        cfg.codex_dir = Some(dir.path().to_path_buf());
        let p = CodexCliProvider::with_opencode_db(cfg, Some(db));
        let collected = p.collect_events().unwrap();
        // 1 rollout event + 1 opencode event = 2.
        assert_eq!(collected.events.len(), 2);
        // Sum of input tokens across both sources.
        let total_in: u64 = collected.events.iter().map(|e| e.input_tokens).sum();
        assert_eq!(total_in, 600);
    }

    #[test]
    fn empty_opencode_db_path_disables_integration() {
        let dir = TempDir::new().unwrap();
        let mut cfg = CodexCliConfig::default();
        cfg.codex_dir = Some(dir.path().to_path_buf());
        // Both forms disable.
        let p = CodexCliProvider::with_opencode_db(cfg.clone(), None);
        assert!(p.collect_events().unwrap().events.is_empty());
        let p = CodexCliProvider::with_opencode_db(cfg, Some(PathBuf::new()));
        assert!(p.collect_events().unwrap().events.is_empty());
    }

    #[test]
    fn apply_rate_limits_keeps_fraction_and_flags_stale() {
        // When `resets_at` is in the past we deliberately keep the
        // last observed fraction in place: the API only writes a
        // rate_limits payload when the user actually runs Codex, and
        // a quota-exhausted user can't. The previous "blank it out"
        // behaviour was misleading — it made the row vanish exactly
        // when the user most wanted to see it. The `stale` flag tells
        // renderers to swap the reset countdown for a ⚠ marker.
        use crate::model::WindowUsage;
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        let stale_bucket = RateLimitsBucket {
            used_percent: 97.0,
            window_minutes: 300,
            // Past the 5-minute grace window.
            resets_at: Some(now - chrono::Duration::minutes(10)),
        };
        apply_rate_limits(&mut w, stale_bucket, now);
        assert!((w.fraction_used.unwrap() - 0.97).abs() < 1e-9);
        assert_eq!(w.ends_at, stale_bucket.resets_at);
        assert!(w.stale, "expected stale flag past grace period");
    }

    #[test]
    fn apply_rate_limits_within_grace_is_not_stale() {
        // Just-past resets_at is in the grace window — codex CLI may
        // be polled again in seconds. Don't flag stale yet.
        use crate::model::WindowUsage;
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        let bucket = RateLimitsBucket {
            used_percent: 80.0,
            window_minutes: 300,
            resets_at: Some(now - chrono::Duration::seconds(30)),
        };
        apply_rate_limits(&mut w, bucket, now);
        assert!(!w.stale);
    }

    #[test]
    fn apply_rate_limits_clamps_over_one_hundred() {
        // Defensive: OpenAI has been observed to occasionally emit
        // used_percent values slightly above 100 (e.g. 100.1 once a
        // burst pushes past the soft cap). The bar widget expects a
        // 0..1 fraction, so we clamp.
        use crate::model::WindowUsage;
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        let bucket = RateLimitsBucket {
            used_percent: 137.5,
            window_minutes: 300,
            resets_at: Some(now + chrono::Duration::hours(2)),
        };
        apply_rate_limits(&mut w, bucket, now);
        assert_eq!(w.fraction_used, Some(1.0));
    }

    #[test]
    fn apply_rate_limits_keeps_fraction_when_fresh() {
        use crate::model::WindowUsage;
        let now = chrono::Utc::now();
        let mut w = WindowUsage::default();
        let fresh_bucket = RateLimitsBucket {
            used_percent: 28.0,
            window_minutes: 300,
            resets_at: Some(now + chrono::Duration::hours(3)),
        };
        apply_rate_limits(&mut w, fresh_bucket, now);
        assert!((w.fraction_used.unwrap() - 0.28).abs() < 1e-9);
        assert!(w.ends_at.is_some());
    }
}
