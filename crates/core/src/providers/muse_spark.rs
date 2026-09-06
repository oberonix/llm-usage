//! Muse Spark usage tracker (Muse Code's `meta` provider).
//!
//! Muse Code DOES have a subscription quota — `/usage` shows plan
//! "Muse Code Everyday Usage" with `Current` (≈5h rolling) and
//! `Weekly` percentage windows — but those numbers arrive inside
//! live model-API responses and are never persisted locally: no
//! quota endpoint string, no cached payload, and no quota method in
//! the offline MSP schema export. Until Meta documents (or the
//! client reveals) a pollable quota source, there is no quota
//! fraction to fetch — so instead we read the same local session
//! logs Muse Code already writes and aggregate token activity, plus
//! an opt-in spend estimate from Meta's published standard-tier
//! pricing ($1.25 / 1M input, $4.25 / 1M output, $0.15 / 1M cached
//! input). Optional weekly/daily dollar budgets turn that activity
//! into quota-like windows (resetting at the week/day boundary) so
//! the tray, alerts, and icon rotation treat Muse Spark like any
//! other quota-bearing provider.
//!
//! Quota reverse-engineering: fitting `/usage` snapshots against
//! machine-wide log consumption pins the metered metric to
//! **non-cached input plus output**. The first fit (3 points) gave
//! Current ≈ 4.41 M units per fixed 5 h window and Weekly ≈ 13.2 M
//! per 7 d — but a Sep-11 calibration (official 11 %/12 % ~23:05 UTC
//! and 13 %/13 % ~23:20 UTC, time-sliced against log numerators)
//! showed the app under-reporting ~2× on both windows, so the limits
//! were recalibrated: Current 4.41 M retained (inside the newly
//! implied 2.8–5.6 M range), Weekly cut to 6.0 M (implied range
//! 4.6–7.4 M across both slices). The Current window is
//! usage-chained, not wall-clock: each 5 h window opens at the first
//! metered step after the previous one ends, so an idle reset opens
//! nothing until usage resumes (a fixed :41 grid fit one "Resets at
//! 7:41 PM" read to the minute but cannot survive idle gaps). The
//! Sunday-17:00 weekly slot was confirmed twice ("Resets Sep 13 at
//! 5:00 PM"). Fit against each snapshot's "as of" time: the `/usage`
//! session counters are live at render while the quota percentages
//! lag by minutes, and single-sample refits swing with that lag —
//! treat all four numbers as ±30 % until a quiet-period calibration
//! pins them. These fitted limits back the `quota_estimate`
//! fractions (on by default, headline tagged `~` and `(est.)`); a
//! local estimate systematically leads the official display, so the
//! labeling stays even though the flag defaults on.
//!
//! ## Observed schema (verified against Muse Code 1.0.x session logs
//! under `~/.local/share/muse/sessions/YYYY/MM/DD/*/session.jsonl`,
//! including `subagent/*/session.jsonl`)
//!
//! Each JSONL line is either a flat record or a `retained_frame`
//! bundling several records as embedded JSON strings:
//!
//! ```text
//! {"retained_frame":"…","children":[
//!   {"child_index":0,"record_json":"{\"…\"payload_type\":\"…\"…}"},
//!   …
//! ]}
//! {"schema_version":1,…,"recorded_at":1788457739718937,
//!  "payload_type":"runtime.session",
//!  "payload":{"event":{"kind":"goal_usage_attribution","record":{
//!     "usage_family":"provider","usage_id":"usage-…",
//!     "quantity":{"cached_tokens":26673,"input_tokens":29758,
//!                 "main_llm_steps":1,"output_tokens":91,
//!                 "reasoning_tokens":0,"reported":true,"unit":"tokens"},
//!     …}},"kind":"run",…}}
//! ```
//!
//! Only `usage_family == "provider"` rows carry model usage — the
//! `"tool"` rows are zero-valued bookkeeping. `recorded_at` is
//! Unix microseconds. `usage_id` is unique per step and serves as
//! the dedupe key. `cached_tokens` is a subset of `input_tokens`
//! (each step's cached count ≈ the previous step's input — the
//! OpenAI/Codex convention), so billing charges `(input − cached)`
//! at the input rate plus `cached` at the cached rate, and display
//! counts `input_tokens` as-is.
//!
//! Like the Anthropic and Codex providers this scans incrementally:
//! per-file offsets survive across `poll()` calls and only appended
//! bytes are re-read.

use crate::config::MuseSparkConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use crate::pricing::muse_spark_cost_usd;
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

/// Per-file resumption state. Mirrors the Anthropic provider: `offset`
/// is the authoritative "already consumed" marker, `size` is
/// diagnostic, and a shrink means truncation (re-read from the top).
#[derive(Default, Clone, Copy)]
struct FileState {
    #[allow(dead_code)]
    size: u64,
    offset: u64,
}

/// One billable model step, decoupled from the session log.
#[derive(Debug, Clone)]
struct TokenEvent {
    timestamp: DateTime<Utc>,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    reasoning_tokens: u64,
}

/// Stop tracking events older than this. The longest bucket is the
/// calendar `month` window (~31 days); 45 days matches the Anthropic
/// provider's buffer for UTC/local disagreement and bounds memory
/// for long-lived tray sessions.
const EVENT_RETENTION: std::time::Duration = std::time::Duration::from_secs(45 * 86_400);

/// Provider-scoped scan cache. Survives between `poll()` calls but
/// not across tray restarts; the first poll pays the full-walk cost
/// once and later polls only read appended bytes per session file.
#[derive(Default)]
struct ScanCache {
    events: Vec<TokenEvent>,
    files: HashMap<PathBuf, FileState>,
    /// `usage_id` dedupe — a re-read after truncation must not
    /// double-count steps already attributed.
    seen_ids: HashSet<String>,
}

pub struct MuseSparkProvider {
    cfg: MuseSparkConfig,
    sessions_dir: PathBuf,
    cache: Mutex<ScanCache>,
}

impl MuseSparkProvider {
    pub fn new(cfg: MuseSparkConfig) -> Self {
        let sessions_dir = cfg
            .sessions_dir
            .clone()
            .unwrap_or_else(default_sessions_dir);
        Self::with_sessions_dir(cfg, sessions_dir)
    }

    /// Construct with an explicit sessions root. Tests inject a
    /// fixture dir here; production uses [`default_sessions_dir`].
    pub fn with_sessions_dir(cfg: MuseSparkConfig, sessions_dir: PathBuf) -> Self {
        Self {
            cfg,
            sessions_dir,
            cache: Mutex::new(ScanCache::default()),
        }
    }

    fn collect_events(&self, now: DateTime<Utc>) -> Result<Vec<TokenEvent>> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let mut cache = self.cache.lock().expect("poisoned");
        for entry in WalkDir::new(&self.sessions_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl")
            })
        {
            let path = entry.path().to_path_buf();
            if let Err(err) = scan_file_incremental(&path, &mut cache) {
                tracing::warn!(path = %path.display(), error = %err, "muse session parse failed");
            }
        }
        // Drop events that fell out of the longest bucket.
        let cutoff = now - chrono::Duration::from_std(EVENT_RETENTION).unwrap();
        cache.events.retain(|e| e.timestamp > cutoff);
        Ok(cache.events.clone())
    }
}

#[async_trait]
impl Provider for MuseSparkProvider {
    fn id(&self) -> ProviderId {
        ProviderId::MuseSpark
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        let now = Utc::now();
        if !self.sessions_dir.exists() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::MuseSpark,
                format!("no muse sessions dir at {}", self.sessions_dir.display()),
            ));
        }
        let events = self.collect_events(now)?;
        if events.is_empty() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::MuseSpark,
                "no muse sessions found",
            ));
        }

        // Estimate windows. "Current" is a usage-chained 5 h window
        // (each window opens at the first metered step after the
        // previous one ends, so an idle reset opens nothing until
        // usage resumes — there is no wall-clock grid); "Weekly" runs
        // Sunday 17:00 to Sunday 17:00 local (confirmed by an
        // official "Resets Sep 13 at 5:00 PM" read). Both rules are
        // approximate — see `current_window` / `weekly_slot`.
        let (slot5_start, slot5_end) = current_window(&events, now);
        let (slot7_start, slot7_end) = weekly_slot(now);
        let estimate = self.cfg.quota_estimate;

        let mut hour = Bucket::default();
        let mut today = Bucket::default();
        let mut five = Bucket::default();
        let mut week = Bucket::default();
        let mut month = Bucket::default();
        for e in &events {
            if (now - e.timestamp).num_hours().abs() < 1 {
                hour.add(e);
            }
            if same_local_day(e.timestamp, now) {
                today.add(e);
            }
            if e.timestamp >= slot5_start && e.timestamp <= slot5_end.min(now) {
                five.add(e);
            }
            if estimate {
                // Estimate mode counts the week window inside the
                // Sunday–Sunday quota slot so the numerator matches
                // the fitted denominator; otherwise it stays the
                // calendar ISO week like every sibling provider.
                if e.timestamp >= slot7_start {
                    week.add(e);
                }
            } else if same_iso_week(e.timestamp, now) {
                week.add(e);
            }
            if e.timestamp.year() == now.year() && e.timestamp.month() == now.month() {
                month.add(e);
            }
        }

        let mut snap = UsageSnapshot {
            provider: ProviderId::MuseSpark,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
            plan_label: None,
        };

        let h = snap.window_mut(WindowKind::LastHour);
        h.tokens_in = hour.input_tokens;
        h.tokens_out = hour.output_tokens;
        h.request_count = hour.turns;
        h.spend_usd = Some(hour.cost_usd);

        let t = snap.window_mut(WindowKind::Today);
        t.tokens_in = today.input_tokens;
        t.tokens_out = today.output_tokens;
        t.request_count = today.turns;
        t.spend_usd = Some(today.cost_usd);
        // A user-supplied daily budget turns `today` into a
        // quota-like window: `ends_at` both renders the reset
        // countdown and marks it quota-derived so `strip_spend`
        // keeps the fraction when spend display is off.
        t.limit_usd = self.cfg.daily_budget_usd;
        if t.limit_usd.is_some() {
            t.ends_at = Some(end_of_local_day(now));
            t.started_at = Some(now);
        }
        t.recompute_fraction();

        // The 5 h activity window mirrors Codex's rolling-5 h row so
        // the card has the same density; in estimate mode it doubles
        // as the "Current" quota window below.
        let w5 = snap.window_mut(WindowKind::FiveHourRolling);
        w5.tokens_in = five.input_tokens;
        w5.tokens_out = five.output_tokens;
        w5.request_count = five.turns;
        w5.spend_usd = Some(five.cost_usd);
        let current_frac = if estimate {
            match self.cfg.current_limit_units {
                Some(limit) if limit > 0 => {
                    let frac = five.uncached_units as f64 / limit as f64;
                    w5.fraction_used = Some(frac);
                    w5.ends_at = Some(slot5_end);
                    w5.started_at = Some(slot5_start);
                    Some(frac)
                }
                _ => None,
            }
        } else {
            None
        };

        let w = snap.window_mut(WindowKind::ThisWeek);
        w.tokens_in = week.input_tokens;
        w.tokens_out = week.output_tokens;
        w.request_count = week.turns;
        w.spend_usd = Some(week.cost_usd);
        let weekly_frac = if estimate {
            match self.cfg.weekly_limit_units {
                Some(limit) if limit > 0 => {
                    let frac = week.uncached_units as f64 / limit as f64;
                    w.fraction_used = Some(frac);
                    w.ends_at = Some(slot7_end);
                    w.started_at = Some(slot7_start);
                    Some(frac)
                }
                _ => None,
            }
        } else {
            // Dollar-budget quota treatment for the weekly budget:
            // `ends_at` is the ISO-week boundary, which also feeds
            // the tray icon's weekly bar and pace marker via the
            // shared "week" label.
            w.limit_usd = self.cfg.weekly_budget_usd;
            if w.limit_usd.is_some() {
                w.ends_at = Some(end_of_iso_week(now));
                w.started_at = Some(now);
            }
            w.recompute_fraction();
            w.fraction_used
        };

        let m = snap.window_mut(WindowKind::ThisMonth);
        m.tokens_in = month.input_tokens;
        m.tokens_out = month.output_tokens;
        m.request_count = month.turns;
        m.spend_usd = Some(month.cost_usd);
        m.recompute_fraction();

        snap.headline = Some(match (estimate, current_frac, weekly_frac) {
            // Estimated quota headline in `/usage` vocabulary. The
            // "~" before each percent plus the "(est.)" tag are
            // load-bearing: these fractions are fitted from a
            // handful of snapshots, and the official display lags
            // live consumption by minutes.
            (true, Some(c), Some(wk)) => {
                format!(
                    "Current ~{:.0}% · Weekly ~{:.0}% (est.)",
                    c * 100.0,
                    wk * 100.0
                )
            }
            (true, Some(c), None) => {
                format!("Current ~{:.0}% (est.)", c * 100.0)
            }
            (true, None, Some(wk)) => {
                format!("Weekly ~{:.0}% (est.)", wk * 100.0)
            }
            _ if self.cfg.show_spend => format!(
                "${:.2} today · ${:.2} 7d · {} turns",
                today.cost_usd, week.cost_usd, week.turns
            ),
            _ => format!("{} turns today · {} turns 7d", today.turns, week.turns),
        });
        if !self.cfg.show_spend {
            snap.strip_spend();
        }
        Ok(snap)
    }
}

#[derive(Debug, Default)]
struct Bucket {
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    /// Non-cached input plus output (reasoning folded into output):
    /// the fitted subscription meter. Cached tokens ride the prompt
    /// cache and meter at zero weight.
    uncached_units: u64,
    cost_usd: f64,
}

impl Bucket {
    /// One usage record is one model step, so `turns` counts steps
    /// across every local session — deliberately broader than
    /// `/usage`'s per-session "Turns" (user turns, subagents split
    /// out). Same convention as the Codex provider's per-event
    /// turn counting.
    fn add(&mut self, e: &TokenEvent) {
        if e.input_tokens == 0
            && e.output_tokens == 0
            && e.cached_tokens == 0
            && e.reasoning_tokens == 0
        {
            return;
        }
        self.turns += 1;
        // `cached_tokens` is a subset of `input_tokens`
        // (OpenAI-style) — count input as-is, don't add cached.
        self.input_tokens = self.input_tokens.saturating_add(e.input_tokens);
        // No separate reasoning price is published; bill thinking at
        // the output rate like the archived Gemini provider did.
        self.output_tokens = self
            .output_tokens
            .saturating_add(e.output_tokens.saturating_add(e.reasoning_tokens));
        self.uncached_units = self.uncached_units.saturating_add(
            e.input_tokens
                .saturating_sub(e.cached_tokens)
                .saturating_add(e.output_tokens)
                .saturating_add(e.reasoning_tokens),
        );
        self.cost_usd += muse_spark_cost_usd(
            e.input_tokens,
            e.output_tokens,
            e.cached_tokens,
            e.reasoning_tokens,
        );
    }
}

/// Read newly-appended bytes from `path` and push newly-seen usage
/// attributions into `cache.events`. Bails without opening the file
/// when size + offset already match; restarts from the top on
/// truncation. A trailing partial line (Muse mid-write) is left for
/// the next scan — the offset never advances past it.
fn scan_file_incremental(path: &Path, cache: &mut ScanCache) -> Result<()> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let prev = cache.files.get(path).copied().unwrap_or_default();
    let start = if prev.offset > size {
        0
    } else if prev.offset == size {
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
            break;
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            parse_line_into(trimmed, cache);
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

/// Parse one JSONL line — flat record or `retained_frame` with
/// embedded `record_json` children — and append any provider-family
/// usage attributions. Malformed lines are skipped silently, matching
/// every sibling provider: a half-written session must never fail a
/// poll.
fn parse_line_into(line: &str, cache: &mut ScanCache) {
    let Ok(top) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    // Borrow the children array when present; otherwise inspect the
    // line itself. The empty-slice fallback keeps the loop below
    // uniform without cloning.
    let empty = Vec::new();
    let children = top
        .get("children")
        .and_then(|c| c.as_array())
        .unwrap_or(&empty);
    if children.is_empty() {
        push_if_usage(&top, cache);
        return;
    }
    for child in children {
        let Some(inner) = child.get("record_json").and_then(|s| s.as_str()) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<serde_json::Value>(inner) else {
            continue;
        };
        push_if_usage(&record, cache);
    }
}

fn push_if_usage(record: &serde_json::Value, cache: &mut ScanCache) {
    let Ok(parsed) = serde_json::from_value::<UsageRecord>(record.clone()) else {
        return;
    };
    let Some(payload) = parsed.payload else {
        return;
    };
    let Some(event) = payload.event else { return };
    if event.kind.as_deref() != Some("goal_usage_attribution") {
        return;
    }
    let Some(attribution) = event.record else {
        return;
    };
    if attribution.usage_family.as_deref() != Some("provider") {
        return;
    }
    let Some(usage_id) = attribution.usage_id else {
        return;
    };
    let quantity = attribution.quantity.unwrap_or_default();
    if quantity.input_tokens == 0
        && quantity.output_tokens == 0
        && quantity.cached_tokens == 0
        && quantity.reasoning_tokens == 0
    {
        return;
    }
    if !cache.seen_ids.insert(usage_id) {
        return;
    }
    let Some(micros) = parsed.recorded_at else {
        return;
    };
    let Some(timestamp) = DateTime::from_timestamp_micros(micros) else {
        return;
    };
    cache.events.push(TokenEvent {
        timestamp,
        input_tokens: quantity.input_tokens,
        output_tokens: quantity.output_tokens,
        cached_tokens: quantity.cached_tokens,
        reasoning_tokens: quantity.reasoning_tokens,
    });
}

#[derive(Debug, Deserialize)]
struct UsageRecord {
    #[serde(default)]
    recorded_at: Option<i64>,
    #[serde(default)]
    payload: Option<UsagePayload>,
}

#[derive(Debug, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    event: Option<UsageEvent>,
}

#[derive(Debug, Deserialize)]
struct UsageEvent {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    record: Option<UsageAttribution>,
}

#[derive(Debug, Deserialize)]
struct UsageAttribution {
    #[serde(default)]
    usage_family: Option<String>,
    #[serde(default)]
    usage_id: Option<String>,
    #[serde(default)]
    quantity: Option<UsageQuantity>,
}

#[derive(Debug, Deserialize, Default)]
struct UsageQuantity {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
}

fn default_sessions_dir() -> PathBuf {
    if let Some(d) = dirs::data_dir() {
        let p = d.join("muse").join("sessions");
        if p.exists() {
            return p;
        }
    }
    if let Some(h) = dirs::home_dir() {
        let p = h.join(".local").join("share").join("muse").join("sessions");
        if p.exists() {
            return p;
        }
    }
    if let Some(h) = dirs::home_dir() {
        let p = h.join(".muse").join("sessions");
        if p.exists() {
            return p;
        }
    }
    dirs::data_dir()
        .map(|d| d.join("muse").join("sessions"))
        .unwrap_or_else(|| PathBuf::from(".muse-sessions"))
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

/// Next local midnight as UTC — the reset moment for a daily budget
/// window. Falls back to 24 h from now across a DST fold where local
/// midnight is ambiguous.
fn end_of_local_day(now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&chrono::Local);
    let next_midnight = local.date_naive() + chrono::Days::new(1);
    let naive = next_midnight.and_hms_opt(0, 0, 0).expect("midnight exists");
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or(now + chrono::Duration::hours(24))
}

/// Next Monday local midnight as UTC — the reset moment for a weekly
/// budget window. Same DST fallback as [`end_of_local_day`].
fn end_of_iso_week(now: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Weekday;
    let local = now.with_timezone(&chrono::Local);
    let days_ahead = match local.weekday() {
        Weekday::Mon => 7,
        d => 7 - d.num_days_from_monday(),
    };
    let monday = local.date_naive() + chrono::Days::new(u64::from(days_ahead));
    let naive = monday.and_hms_opt(0, 0, 0).expect("midnight exists");
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or(now + chrono::Duration::days(7))
}

/// Start and end of the usage-chained 5 h "Current" quota window.
/// Consecutive 5 h windows are chained from first use: each window
/// opens at the first metered step at/after the previous window's
/// end, so an idle reset opens no window until usage resumes — there
/// is deliberately no wall-clock grid. (A fixed :41 grid fit one
/// observed "Resets at 7:41 PM" read to the minute, but the quota
/// does not start a window without usage, and 24 h is not a multiple
/// of 5 h so no daily grid can hold anyway.) Returns the live window
/// containing `now`, or the most recently ended one when idle past
/// its end — the numerator is static then and the countdown reads
/// elapsed. The 5 h length itself is solid (an official reset stayed
/// fixed across snapshots 45 min apart, ruling out a rolling
/// window); the chaining rule is falsifiable at the next reset after
/// an idle gap, when a grid would tick to a wall-clock boundary and
/// chaining holds the expired window instead.
fn current_window(events: &[TokenEvent], now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    const WINDOW: chrono::Duration = chrono::Duration::hours(5);
    // Cache order follows file appends, not time, and multi-file
    // scans interleave — sort, and ignore clock-skewed future steps
    // (they join the chain on a later poll once `now` passes them).
    let mut stamps: Vec<DateTime<Utc>> = events
        .iter()
        .map(|e| e.timestamp)
        .filter(|t| *t <= now)
        .collect();
    stamps.sort();
    let Some(first) = stamps.first() else {
        return (now, now + WINDOW);
    };
    let mut start = *first;
    for t in &stamps {
        if *t > start + WINDOW {
            start = *t;
        }
    }
    (start, start + WINDOW)
}

/// Start and end of the 7 d "Weekly" quota slot containing `now`:
/// most recent Sunday 17:00 local through the following Sunday.
/// Inferred from one observed reset (Sun Sep 6, 5 PM) — same
/// approximate-alignment caveat as [`current_slot`].
fn weekly_slot(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let local = now.with_timezone(&chrono::Local);
    let days_since_sunday = i64::from(local.weekday().number_from_sunday()) - 1;
    let mut start_naive = (local.date_naive() - chrono::Days::new(days_since_sunday as u64))
        .and_hms_opt(17, 0, 0)
        .expect("17:00 exists");
    if local.naive_local() < start_naive {
        // Sunday before 17:00 belongs to the slot that ends today.
        start_naive -= chrono::Duration::days(7);
    }
    let end_naive = start_naive + chrono::Duration::days(7);
    (
        to_utc_or(start_naive, now - chrono::Duration::days(7)),
        to_utc_or(end_naive, now),
    )
}

/// Local-naive → UTC with a DST-fold fallback to `or`.
fn to_utc_or(naive: chrono::NaiveDateTime, or: DateTime<Utc>) -> DateTime<Utc> {
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or(or)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn micros(dt: DateTime<Utc>) -> i64 {
        dt.timestamp_micros()
    }

    fn usage_line(
        micros_ts: i64,
        id: &str,
        family: &str,
        input: u64,
        output: u64,
        cached: u64,
        reasoning: u64,
    ) -> String {
        format!(
            r#"{{"schema_version":1,"recorded_at":{micros_ts},"payload_type":"runtime.session","payload":{{"event":{{"kind":"goal_usage_attribution","record":{{"usage_family":"{family}","usage_id":"{id}","quantity":{{"cached_tokens":{cached},"input_tokens":{input},"main_llm_steps":1,"output_tokens":{output},"reasoning_tokens":{reasoning},"reported":true,"unit":"tokens"}}}}}},"kind":"run"}}}}"#,
        )
    }

    fn framed_line(inner_json_lines: &[String]) -> String {
        let children: Vec<String> = inner_json_lines
            .iter()
            .enumerate()
            .map(|(i, inner)| {
                let escaped = inner.replace('\\', "\\\\").replace('"', "\\\"");
                format!(r#"{{"child_index":{i},"record_json":"{escaped}"}}"#)
            })
            .collect();
        format!(
            r#"{{"retained_frame":"test","frame_schema_version":1,"children":[{}]}}"#,
            children.join(",")
        )
    }

    fn fixture_dir_with(lines: &[String]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let sess = dir.path().join("2026/09/03/sess-1");
        std::fs::create_dir_all(&sess).unwrap();
        let mut out = std::fs::File::create(sess.join("session.jsonl")).unwrap();
        for line in lines {
            writeln!(out, "{line}").unwrap();
        }
        out.flush().unwrap();
        dir
    }

    fn provider_for(dir: &TempDir) -> MuseSparkProvider {
        MuseSparkProvider::with_sessions_dir(MuseSparkConfig::default(), dir.path().to_path_buf())
    }

    fn activity_only_provider_for(dir: &TempDir) -> MuseSparkProvider {
        let mut cfg = MuseSparkConfig::default();
        cfg.quota_estimate = false;
        MuseSparkProvider::with_sessions_dir(cfg, dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn parses_nested_frame_and_flat_record() {
        let now = Utc::now();
        let m = micros(now);
        let inner_provider = usage_line(m, "usage-1", "provider", 1000, 200, 100, 10);
        let inner_tool = usage_line(m, "usage-2", "tool", 0, 0, 0, 0);
        let lines = vec![
            framed_line(&[inner_provider, inner_tool]),
            usage_line(m, "usage-3", "provider", 500, 50, 0, 0),
            r#"{"this is not json"#.to_string(),
            r#"{"schema_version":1,"payload":{"event":{"kind":"something_else"}}}"#.to_string(),
        ];
        let dir = fixture_dir_with(&lines);
        let p = activity_only_provider_for(&dir);
        let snap = p.poll().await.unwrap();
        assert!(matches!(snap.status, ProviderStatus::Ok));
        let today = snap.window(WindowKind::Today).expect("today window");
        // usage-1 + usage-3; the zero-valued tool row is skipped.
        assert_eq!(today.request_count, 2);
        assert_eq!(today.tokens_in, 1500);
        // output folds reasoning in: (200 + 10) + 50.
        assert_eq!(today.tokens_out, 260);
        // Activity-only by default — no quota fractions anywhere.
        assert!(snap.windows.values().all(|w| w.fraction_used.is_none()));
        assert_eq!(snap.headline.as_deref(), Some("2 turns today · 2 turns 7d"));
    }

    #[tokio::test]
    async fn dedupes_repeated_polls_and_partial_appends() {
        let now = Utc::now();
        let m = micros(now);
        let dir = fixture_dir_with(&[usage_line(m, "usage-1", "provider", 1000, 100, 0, 0)]);
        let p = provider_for(&dir);
        let first = p.poll().await.unwrap();
        assert_eq!(first.window(WindowKind::Today).unwrap().request_count, 1);
        // Second poll with no changes: same counts, no double-count.
        let second = p.poll().await.unwrap();
        assert_eq!(second.window(WindowKind::Today).unwrap().request_count, 1);
        // Append one new step; only it is picked up.
        let path = dir.path().join("2026/09/03/sess-1/session.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            "{}",
            usage_line(m, "usage-2", "provider", 2000, 200, 500, 0)
        )
        .unwrap();
        drop(f);
        let third = p.poll().await.unwrap();
        let today = third.window(WindowKind::Today).unwrap();
        assert_eq!(today.request_count, 2);
        assert_eq!(today.tokens_in, 3000);
    }

    #[tokio::test]
    async fn budgets_create_quota_windows_that_survive_strip_spend() {
        let now = Utc::now();
        let m = micros(now);
        // 1M input + 1M output ≈ $5.50 at standard-tier pricing.
        let dir = fixture_dir_with(&[usage_line(
            m, "usage-1", "provider", 1_000_000, 1_000_000, 0, 0,
        )]);
        let mut cfg = MuseSparkConfig::default();
        cfg.weekly_budget_usd = Some(11.0);
        cfg.daily_budget_usd = Some(11.0);
        // Estimate off: this test covers the dollar-budget path, and
        // the estimate would otherwise override the week fraction.
        cfg.quota_estimate = false;
        // show_spend stays false: spend hidden, but the budget
        // fractions must survive via their ends_at markers.
        let p = MuseSparkProvider::with_sessions_dir(cfg, dir.path().to_path_buf());
        let snap = p.poll().await.unwrap();
        let week = snap.window(WindowKind::ThisWeek).unwrap();
        assert!(week.ends_at.is_some(), "budget window needs a reset time");
        let frac = week.fraction_used.expect("budget fraction");
        assert!((frac - 0.5).abs() < 1e-6, "got {}", frac);
        assert!(
            week.spend_usd.is_none(),
            "spend hidden when show_spend=false"
        );
        assert!(
            week.limit_usd.is_none(),
            "limit hidden when show_spend=false"
        );
        let today = snap.window(WindowKind::Today).unwrap();
        assert!(today.fraction_used.is_some());
        // The shared "week" label feeds the tray icon's weekly bar.
        assert!(crate::model::WindowKind::ThisWeek.label() == "week");
    }

    #[tokio::test]
    async fn spend_headline_when_opted_in() {
        let now = Utc::now();
        let m = micros(now);
        let dir = fixture_dir_with(&[usage_line(
            m, "usage-1", "provider", 1_000_000, 0, 1_000_000, 0,
        )]);
        let mut cfg = MuseSparkConfig::default();
        cfg.show_spend = true;
        // Estimate off so the spend headline (not the estimate
        // headline) is what gets asserted here.
        cfg.quota_estimate = false;
        let p = MuseSparkProvider::with_sessions_dir(cfg, dir.path().to_path_buf());
        let snap = p.poll().await.unwrap();
        // All input cached: 1M × $0.15 = $0.15.
        let today = snap.window(WindowKind::Today).unwrap();
        assert!((today.spend_usd.unwrap() - 0.15).abs() < 1e-9);
        assert!(snap.headline.as_deref().unwrap().starts_with("$0.15 today"));
    }

    #[tokio::test]
    async fn missing_dir_is_unavailable() {
        let dir = TempDir::new().unwrap();
        let p = MuseSparkProvider::with_sessions_dir(
            MuseSparkConfig::default(),
            dir.path().join("does-not-exist"),
        );
        let snap = p.poll().await.unwrap();
        assert!(matches!(snap.status, ProviderStatus::Unavailable));
    }

    #[tokio::test]
    async fn empty_sessions_dir_is_unavailable() {
        let dir = TempDir::new().unwrap();
        let p = provider_for(&dir);
        let snap = p.poll().await.unwrap();
        assert!(matches!(snap.status, ProviderStatus::Unavailable));
    }

    #[test]
    fn week_and_day_boundaries_are_in_the_future() {
        let now = Utc::now();
        assert!(end_of_local_day(now) > now);
        assert!(end_of_iso_week(now) > now);
        // ISO-week boundary is at most 7 days out.
        assert!((end_of_iso_week(now) - now).num_days() <= 7);
    }

    #[test]
    fn quota_estimate_is_on_by_default() {
        assert!(MuseSparkConfig::default().quota_estimate);
    }

    #[tokio::test]
    async fn estimate_emits_labeled_current_and_weekly_fractions() {
        let now = Utc::now();
        let m = micros(now);
        // Uncached units: (1000 − 100) + 200 + 10 = 1110, plus
        // (500 − 0) + 50 = 550 → 1660 total.
        let dir = fixture_dir_with(&[
            usage_line(m, "usage-1", "provider", 1000, 200, 100, 10),
            usage_line(m, "usage-2", "provider", 500, 50, 0, 0),
        ]);
        let mut cfg = MuseSparkConfig::default();
        cfg.quota_estimate = true;
        cfg.current_limit_units = Some(3320);
        cfg.weekly_limit_units = Some(3320);
        // show_spend stays false: the estimate fractions must stand
        // on their own via ends_at, with spend hidden.
        let p = MuseSparkProvider::with_sessions_dir(cfg, dir.path().to_path_buf());
        let snap = p.poll().await.unwrap();
        // 1660 / 3320 = exactly 50 % on both windows.
        let five = snap.window(WindowKind::FiveHourRolling).unwrap();
        assert_eq!(five.fraction_used, Some(0.5));
        assert!(five.ends_at.is_some(), "estimate needs a reset time");
        assert!(five.spend_usd.is_none());
        let week = snap.window(WindowKind::ThisWeek).unwrap();
        assert_eq!(week.fraction_used, Some(0.5));
        assert!(week.ends_at.is_some());
        assert_eq!(
            snap.headline.as_deref(),
            Some("Current ~50% · Weekly ~50% (est.)")
        );
    }

    #[tokio::test]
    async fn estimate_without_limits_falls_back_to_turns() {
        let now = Utc::now();
        let dir = fixture_dir_with(&[usage_line(
            micros(now),
            "usage-1",
            "provider",
            100,
            10,
            0,
            0,
        )]);
        let mut cfg = MuseSparkConfig::default();
        cfg.quota_estimate = true;
        cfg.current_limit_units = None;
        cfg.weekly_limit_units = None;
        let p = MuseSparkProvider::with_sessions_dir(cfg, dir.path().to_path_buf());
        let snap = p.poll().await.unwrap();
        assert!(
            snap.windows.values().all(|w| w.fraction_used.is_none()),
            "no limits → no estimate fractions"
        );
        assert_eq!(snap.headline.as_deref(), Some("1 turns today · 1 turns 7d"));
    }

    /// Build a UTC instant from local wall-clock components so slot
    /// tests hold in any machine timezone.
    fn local_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        chrono::Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("unambiguous local time")
            .with_timezone(&Utc)
    }

    fn local_ymdhm(dt: DateTime<Utc>) -> (i32, u32, u32, u32, u32) {
        use chrono::Timelike;
        let l = dt.with_timezone(&chrono::Local);
        (l.year(), l.month(), l.day(), l.hour(), l.minute())
    }

    fn step_at(hours_from_base: i64, base: DateTime<Utc>) -> TokenEvent {
        TokenEvent {
            timestamp: base + chrono::Duration::hours(hours_from_base),
            input_tokens: 100,
            output_tokens: 10,
            cached_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    #[test]
    fn current_window_chains_from_first_use() {
        // Steps at T, T+1h, T+6h, T+7h with now = T+7h: the T+6h
        // step opens the second window (first use after T+5h).
        let base = chrono::Utc::now() - chrono::Duration::hours(7);
        let events = vec![
            step_at(0, base),
            step_at(1, base),
            step_at(6, base),
            step_at(7, base),
        ];
        let (s, e) = current_window(&events, base + chrono::Duration::hours(7));
        assert_eq!(s, base + chrono::Duration::hours(6));
        assert_eq!(e, base + chrono::Duration::hours(11));
    }

    #[test]
    fn current_window_holds_expired_window_when_idle() {
        // An idle reset opens nothing until usage resumes: with the
        // only step at T and now = T+8h, the reported window is still
        // [T, T+5h] with a static numerator — not a fresh empty one.
        let base = chrono::Utc::now() - chrono::Duration::hours(8);
        let events = vec![step_at(0, base)];
        let (s, e) = current_window(&events, base + chrono::Duration::hours(8));
        assert_eq!(s, base);
        assert_eq!(e, base + chrono::Duration::hours(5));
    }

    #[test]
    fn current_window_sorts_and_ignores_future_steps() {
        // Out-of-order cache plus a clock-skewed future step: the
        // chain follows timestamp order and the future step joins
        // only once `now` passes it.
        let base = chrono::Utc::now() - chrono::Duration::hours(2);
        let events = vec![step_at(1, base), step_at(0, base), step_at(48, base)];
        let now = base + chrono::Duration::hours(2);
        let (s, e) = current_window(&events, now);
        assert_eq!(s, base);
        assert_eq!(e, base + chrono::Duration::hours(5));
    }

    #[test]
    fn weekly_slot_runs_sunday_17_to_sunday_17() {
        // Thu → Sun Aug 30 17:00 – Sun Sep 6 17:00, the observed reset.
        let (s, e) = weekly_slot(local_utc(2026, 9, 3, 13, 30));
        assert_eq!(local_ymdhm(s), (2026, 8, 30, 17, 0));
        assert_eq!(local_ymdhm(e), (2026, 9, 6, 17, 0));
        // Sunday before 17:00 still belongs to the ending slot.
        let (s, e) = weekly_slot(local_utc(2026, 9, 6, 10, 0));
        assert_eq!(local_ymdhm(s), (2026, 8, 30, 17, 0));
        assert_eq!(local_ymdhm(e), (2026, 9, 6, 17, 0));
        // Sunday evening opens the next slot.
        let (s, e) = weekly_slot(local_utc(2026, 9, 6, 18, 0));
        assert_eq!(local_ymdhm(s), (2026, 9, 6, 17, 0));
        assert_eq!(local_ymdhm(e), (2026, 9, 13, 17, 0));
    }
}
