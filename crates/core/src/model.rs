use chrono::{DateTime, Duration, Utc};

/// Symbol displayed next to a stale `WindowUsage` in any text-based
/// renderer (tray menu, CLI bars, dashboard quota row). Centralised
/// so a future change ("⚠" not rendering in some user's terminal,
/// swap to "(?)" or "stale") is a one-line edit.
pub const STALE_MARKER: &str = "⚠";
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Upper-case the first character of `s`, leave the rest untouched.
/// Used to turn plan tags like "plus" / "pro" into "Plus" / "Pro" for
/// display, shared by every provider that exposes a plan label.
pub fn title_case_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Anthropic,
    Antigravity,
    CodexCli,
    OllamaCloud,
    MuseSpark,
    OpenCodeGo,
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderId::Anthropic => "anthropic",
            ProviderId::Antigravity => "antigravity",
            ProviderId::CodexCli => "codex_cli",
            ProviderId::OllamaCloud => "ollama_cloud",
            ProviderId::MuseSpark => "muse_spark",
            ProviderId::OpenCodeGo => "opencode_go",
        };
        f.write_str(s)
    }
}

impl ProviderId {
    pub fn human(&self) -> &'static str {
        match self {
            ProviderId::Anthropic => "Anthropic",
            ProviderId::Antigravity => "Antigravity",
            ProviderId::CodexCli => "Codex",
            ProviderId::OllamaCloud => "Ollama Cloud",
            ProviderId::MuseSpark => "Muse Spark",
            ProviderId::OpenCodeGo => "OpenCode Go",
        }
    }

    /// Brand-ish accent color for this provider. Used by the tray icon
    /// for its background tint and by the dashboard for the card's
    /// left-edge accent — keeping the two in lockstep so users can
    /// match a tray colour to a card at a glance.
    pub fn tint_rgb(&self) -> (u8, u8, u8) {
        match self {
            ProviderId::Anthropic => (0xCC, 0x78, 0x5C),
            ProviderId::Antigravity => (0x42, 0xA5, 0xF5),
            ProviderId::CodexCli => (0x9C, 0x6B, 0xFF),
            ProviderId::OllamaCloud => (0x3B, 0x82, 0xF6),
            ProviderId::MuseSpark => (0x06, 0xB6, 0xD4),
            ProviderId::OpenCodeGo => (0x10, 0xB9, 0x81),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    LastHour,
    Today,
    ThisWeek,
    ThisMonth,
    /// 5-hour rolling window — Codex CLI on ChatGPT plan.
    FiveHourRolling,
}

impl WindowKind {
    pub fn label(&self) -> &'static str {
        match self {
            WindowKind::LastHour => "1h",
            WindowKind::Today => "today",
            WindowKind::ThisWeek => "week",
            WindowKind::ThisMonth => "month",
            WindowKind::FiveHourRolling => "5h",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Ok,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WindowUsage {
    pub started_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub spend_usd: Option<f64>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub request_count: u64,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<u64>,
    /// 0.0–1.0+ if a limit is known. >1.0 means over.
    pub fraction_used: Option<f64>,
    /// True when this window's quota fields came from a cached
    /// snapshot rather than the current poll. Renderers replace the
    /// reset countdown with a warning marker so the user knows the
    /// number may be out of date. `#[serde(default)]` keeps old
    /// on-disk snapshot files (pre-stale) deserialising cleanly.
    #[serde(default)]
    pub stale: bool,
    /// True when this window is a *derived sub-view* of another
    /// window rather than an independent quota signal (e.g.
    /// Anthropic's per-model weekly split). The provider that owns
    /// the upstream shape sets this; the generic core only reads it.
    /// `merge_stale_from` never grafts a sub-view from cache: upstream
    /// omits these when that class has no recent usage, so a grafted
    /// ghost would raise the provider ⚠ while the real quota is fresh.
    /// `#[serde(default)]` keeps pre-existing snapshots deserialising.
    #[serde(default)]
    pub subview: bool,
    /// True window length in seconds, when the provider reports it
    /// (Codex buckets carry `window_minutes`). Pace markers prefer
    /// this over the label-implied 5h/7d default. Every other
    /// provider leaves it `None` and the hardcoded durations apply —
    /// see [`implied_window_secs`]. `#[serde(default)]` keeps
    /// pre-existing snapshots deserialising.
    #[serde(default)]
    pub window_secs: Option<i64>,
}

/// Window length in seconds implied by labels with known pacing
/// semantics. Antigravity (`Claude 5h`, `Gemini week`, …) and Ollama
/// (`5h` for "Session usage") are deliberately hardcoded to the same
/// 5h/7d rolling windows as Anthropic/Codex: neither source exposes
/// a trustworthy duration, so a shared assumption beats per-row
/// guesswork. Returns `None` for labels we can't confidently size
/// (`1h` / `today` / `month` …).
pub fn implied_window_secs(label: &str) -> Option<i64> {
    if label == "5h" || label.ends_with(" 5h") {
        Some(5 * 3600)
    } else if label == "week" || label.ends_with(" week") || label == "Sonnet" || label == "Opus" {
        Some(7 * 86_400)
    } else {
        None
    }
}

impl WindowUsage {
    pub fn recompute_fraction(&mut self) {
        self.fraction_used = match (self.limit_usd, self.spend_usd, self.limit_tokens) {
            (Some(limit), Some(spend), _) if limit > 0.0 => Some(spend / limit),
            (_, _, Some(limit_tok)) if limit_tok > 0 => {
                let used = self.tokens_in.saturating_add(self.tokens_out) as f64;
                Some(used / limit_tok as f64)
            }
            _ => None,
        };
    }

    /// How long after `ends_at` has passed before quota data is
    /// considered stale. Five minutes absorbs clock skew and the
    /// latency between the API reset moment and the next poll.
    pub const STALE_GRACE_SECS: i64 = 5 * 60;

    /// Elapsed fraction (0..1) of this quota window for the pace
    /// marker overlay. Prefers the provider-reported [`Self::window_secs`]
    /// when positive, otherwise the label-implied duration from
    /// [`implied_window_secs`]. Returns `None` when the label has no
    /// known duration, when `ends_at` is missing, or when the reset
    /// lands more than a full window in the future: that contradicts
    /// the assumed window length (observed live on Antigravity and
    /// Ollama rows), and no marker beats one pinned at zero. A reset
    /// already past yields 1.0 (window fully elapsed).
    pub fn pace_fraction(&self, label: &str, now: DateTime<Utc>) -> Option<f64> {
        let window_secs = self
            .window_secs
            .filter(|s| *s > 0)
            .or_else(|| implied_window_secs(label))?;
        let ends = self.ends_at?;
        let remaining = (ends - now).num_seconds();
        if remaining > window_secs {
            return None;
        }
        let remaining = remaining.max(0);
        Some((window_secs - remaining) as f64 / window_secs as f64)
    }

    /// Mark the window stale if its reset time is in the past beyond
    /// the grace period. Call this after setting `ends_at` on a fresh
    /// quota window. The flag is additive — once stale it stays stale,
    /// so a 429-cooldown `mark_oauth_windows_stale` call afterwards
    /// just reaffirms it.
    pub fn mark_stale_if_expired(&mut self, now: DateTime<Utc>) {
        if self
            .ends_at
            .is_some_and(|t| (now - t).num_seconds() > Self::STALE_GRACE_SECS)
        {
            self.stale = true;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub provider: ProviderId,
    pub timestamp: DateTime<Utc>,
    pub status: ProviderStatus,
    pub error: Option<String>,
    /// BTreeMap so iteration is deterministic for the UI.
    pub windows: BTreeMap<String, WindowUsage>,
    /// One-line tray summary, e.g. "$4.21 today · 62% of weekly".
    pub headline: Option<String>,
    /// Optional plan / tier tag (e.g. "plus", "pro"). Surfaced in the
    /// dashboard's provider header alongside the provider name.
    #[serde(default)]
    pub plan_label: Option<String>,
}

impl UsageSnapshot {
    pub fn unavailable(provider: ProviderId, error: impl Into<String>) -> Self {
        Self {
            provider,
            timestamp: Utc::now(),
            status: ProviderStatus::Unavailable,
            error: Some(error.into()),
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        }
    }

    pub fn window_mut(&mut self, kind: WindowKind) -> &mut WindowUsage {
        self.windows.entry(kind.label().to_string()).or_default()
    }

    pub fn window(&self, kind: WindowKind) -> Option<&WindowUsage> {
        self.windows.get(kind.label())
    }

    /// Drop dollar-denominated data so the UI shows quota only.
    ///
    /// Clears `spend_usd` / `limit_usd` on every window, plus any
    /// `fraction_used` that was derived from spend. We treat a window as
    /// quota-derived (and so leave its fraction alone) when it carries an
    /// `ends_at` — only the OAuth quota path sets that field. Token counts
    /// and request counts are left intact.
    pub fn strip_spend(&mut self) {
        for w in self.windows.values_mut() {
            let quota_derived = w.ends_at.is_some();
            w.spend_usd = None;
            w.limit_usd = None;
            if !quota_derived {
                w.fraction_used = None;
            }
        }
    }

    /// How old a cached snapshot can be before [`merge_stale_from`]
    /// refuses to graft from it. Seven days is generous — a quota
    /// window from a week ago likely doesn't reflect anything useful,
    /// and showing "55 % used (stale)" forever after a provider has
    /// truly gone away would be worse than showing nothing.
    pub const STALE_CACHE_MAX_AGE_DAYS: i64 = 7;

    /// Graft cached quota state from a previous snapshot onto this one,
    /// marking any borrowed window as `stale`. Renderers replace the
    /// reset countdown with a ⚠ marker for stale windows so the user
    /// can tell live data from cached data at a glance.
    ///
    /// Two cases are merged:
    ///
    /// 1. A window present in `cached` but absent (or quota-empty) here
    ///    — happens when a provider's quota endpoint is down. We copy
    ///    the whole window in and mark it stale.
    /// 2. A window present in both, but with `fraction_used = None`
    ///    here — happens when the local-activity path succeeded but
    ///    the quota path failed. We graft just the quota fields
    ///    (`fraction_used`, `ends_at`, `started_at`, `limit_*`) so the
    ///    activity counters from the fresh poll stay authoritative.
    ///
    /// Snapshots older than [`STALE_CACHE_MAX_AGE_DAYS`] are ignored:
    /// at some point yesterday's numbers stop being a useful hint
    /// about today's reality. Use the fresh snapshot's `timestamp` as
    /// the "now" reference, so a slow clock doesn't artificially age
    /// the cache.
    ///
    /// Windows flagged `subview` (derived per-model splits) are never
    /// grafted: upstream omits them when idle, so a cached ghost would
    /// raise the provider ⚠ while the real quota is fresh.
    ///
    /// Side effect: if any primary window is grafted stale and the
    /// fresh snapshot didn't already explain itself, this sets
    /// `error` (preferring the cached reason) and downgrades an `Ok`
    /// `status` to `Degraded`, so the ⚠ is never shown without a why.
    ///
    /// Returns the number of windows that ended up stale, so callers
    /// can log "served 2 stale windows from cache" if useful.
    pub fn merge_stale_from(&mut self, cached: &UsageSnapshot) -> usize {
        if self.provider != cached.provider {
            return 0;
        }
        let max_age = Duration::days(Self::STALE_CACHE_MAX_AGE_DAYS);
        let age = self.timestamp - cached.timestamp;
        if age > max_age {
            return 0;
        }
        let has_fresh_quota = self
            .windows
            .values()
            .any(|w| w.fraction_used.is_some() && !w.stale && !w.subview);
        let mut stale_count = 0;
        for (label, cw) in &cached.windows {
            if cw.fraction_used.is_none() {
                continue; // cache has nothing useful to graft
            }
            if cw.subview {
                continue; // sub-views mirror the live response, never the cache
            }
            if cw
                .ends_at
                .is_some_and(|ends| self.timestamp - ends > max_age)
            {
                continue; // window itself expired beyond max cache age
            }
            match self.windows.get_mut(label) {
                Some(nw) if nw.fraction_used.is_none() => {
                    nw.fraction_used = cw.fraction_used;
                    nw.ends_at = cw.ends_at;
                    nw.started_at = cw.started_at;
                    nw.window_secs = nw.window_secs.or(cw.window_secs);
                    nw.limit_usd = nw.limit_usd.or(cw.limit_usd);
                    nw.limit_tokens = nw.limit_tokens.or(cw.limit_tokens);
                    nw.stale = true;
                    stale_count += 1;
                }
                None => {
                    if has_fresh_quota
                        && cw.ends_at.is_some_and(|ends| ends <= self.timestamp)
                    {
                        continue; // live quota poll succeeded and this missing window already reset
                    }
                    // Window didn't exist on the new snapshot at all
                    // — e.g. poll() returned Err and produced an
                    // `unavailable` snapshot. Reinstate the cached
                    // window verbatim, then mark it stale.
                    let mut copied = cw.clone();
                    copied.stale = true;
                    self.windows.insert(label.clone(), copied);
                    stale_count += 1;
                }
                _ => {} // fresh fraction wins
            }
        }
        // If the new snapshot has no headline AND nothing else useful,
        // fall back to the cached headline so the menu/CLI still show
        // a one-liner. Don't overwrite a fresh headline — that's the
        // poll's own report.
        if self.headline.is_none() && self.windows.values().all(|w| w.stale) {
            self.headline = cached.headline.clone();
        }
        // Same for plan label.
        if self.plan_label.is_none() {
            self.plan_label = cached.plan_label.clone();
        }
        // If we grafted anything stale and the fresh poll didn't already
        // explain itself, surface *why* the data is old so the ⚠ is
        // never bare. Prefer the cached snapshot's own reason (e.g. the
        // Anthropic "rate-limited / refresh paused" message); fall back
        // to a generic line otherwise. Drop status to Degraded too so
        // the provider-state store and the dashboard agree the reading
        // isn't live. With per-model sub-views excluded above,
        // `stale_count > 0` means a *primary* quota window is stale —
        // a genuinely degraded state worth a reason.
        if stale_count > 0 {
            if self.error.is_none() {
                self.error = cached.error.clone().or_else(|| {
                    Some(
                        "Showing cached quota — provider returned no fresh data this poll"
                            .to_string(),
                    )
                });
            }
            if matches!(self.status, ProviderStatus::Ok) {
                self.status = ProviderStatus::Degraded;
            }
        }
        stale_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_snapshot_with_fraction(provider: ProviderId) -> UsageSnapshot {
        let mut s = UsageSnapshot {
            provider,
            timestamp: Utc::now() - chrono::Duration::hours(1),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: Some("cached headline".into()),
            plan_label: Some("Plus".into()),
        };
        s.windows.insert(
            "5h".into(),
            WindowUsage {
                fraction_used: Some(0.65),
                ends_at: Some(Utc::now() + chrono::Duration::hours(2)),
                limit_usd: Some(50.0),
                ..Default::default()
            },
        );
        s.windows.insert(
            "week".into(),
            WindowUsage {
                fraction_used: Some(0.40),
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn merge_stale_from_grafts_missing_windows() {
        // Fresh poll completely failed → "unavailable" snapshot has no
        // windows. Merge should reinstate the cached ones as stale.
        let cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        let mut fresh = UsageSnapshot::unavailable(ProviderId::Anthropic, "boom");
        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 2, "both cached windows should be reinstated");
        assert!(fresh.windows.get("5h").unwrap().stale);
        assert!(fresh.windows.get("week").unwrap().stale);
        // Headline backfilled when nothing fresh was available.
        assert_eq!(fresh.headline.as_deref(), Some("cached headline"));
        assert_eq!(fresh.plan_label.as_deref(), Some("Plus"));
    }

    #[test]
    fn merge_stale_from_grafts_quota_when_activity_is_fresh() {
        // Anthropic-style partial failure: token aggregation worked
        // (fresh windows with counts) but the OAuth /usage call
        // failed (so fraction_used is None). Cache should fill in
        // just the fraction without clobbering activity counters.
        let cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        let mut fresh = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Degraded,
            error: Some("oauth down".into()),
            windows: BTreeMap::new(),
            headline: Some("fresh headline".into()),
            plan_label: None,
        };
        fresh.windows.insert(
            "5h".into(),
            WindowUsage {
                tokens_in: 12345,
                tokens_out: 678,
                request_count: 9,
                ..Default::default()
            },
        );
        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 2);
        let w5h = fresh.windows.get("5h").unwrap();
        assert_eq!(w5h.fraction_used, Some(0.65), "fraction grafted from cache");
        assert_eq!(w5h.tokens_in, 12345, "fresh activity counters preserved");
        assert!(w5h.stale, "grafted window flagged stale");
        // Fresh headline wins — cache must not overwrite.
        assert_eq!(fresh.headline.as_deref(), Some("fresh headline"));
    }

    #[test]
    fn merge_stale_from_leaves_fresh_fractions_alone() {
        // If the poll DID return fractions, don't overwrite them with
        // the cache. (And don't mark them stale.)
        let cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        let mut fresh = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        };
        fresh.windows.insert(
            "5h".into(),
            WindowUsage {
                fraction_used: Some(0.10),
                ..Default::default()
            },
        );
        fresh.merge_stale_from(&cached);
        let w = fresh.windows.get("5h").unwrap();
        assert_eq!(w.fraction_used, Some(0.10));
        assert!(!w.stale);
    }

    #[test]
    fn merge_stale_from_does_not_graft_stale_per_model_subview() {
        // The exact false-⚠ bug: a healthy Anthropic poll returns
        // fresh 5h + week, but Anthropic omitted the per-model Sonnet
        // split (no recent Sonnet usage). The cache still holds a
        // Sonnet window. merge_stale_from must NOT reinstate it as
        // stale — doing so raised the provider ⚠ while the real quota
        // was perfectly fresh, with no reason text anywhere.
        let mut cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        cached.windows.insert(
            "Sonnet".into(),
            WindowUsage {
                fraction_used: Some(0.01),
                subview: true,
                ..Default::default()
            },
        );
        let mut fresh = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: Some("5h 6% · 7d 4%".into()),
            plan_label: None,
        };
        fresh.windows.insert(
            "5h".into(),
            WindowUsage {
                fraction_used: Some(0.06),
                ..Default::default()
            },
        );
        fresh.windows.insert(
            "week".into(),
            WindowUsage {
                fraction_used: Some(0.04),
                ..Default::default()
            },
        );

        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 0, "5h/week fresh, Sonnet is a sub-view → graft nothing");
        assert!(
            !fresh.windows.contains_key("Sonnet"),
            "stale Sonnet ghost must not be reinstated"
        );
        assert!(
            !fresh.windows.values().any(|w| w.stale),
            "no window should be stale"
        );
        assert!(
            matches!(fresh.status, ProviderStatus::Ok),
            "status stays Ok"
        );
        assert!(fresh.error.is_none(), "no spurious reason");
    }

    #[test]
    fn merge_stale_from_surfaces_reason_on_primary_graft() {
        // Real degradation: the OAuth /usage call failed/throttled so
        // the fresh poll has no quota fractions. The cached snapshot
        // carried the rate-limit message. After grafting the stale
        // primary windows the ⚠ must be explained — the cached reason
        // wins and status drops to Degraded.
        let mut cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        cached.error = Some("Rate-limited by Anthropic — refresh paused for 12 min".into());
        let mut fresh = UsageSnapshot::unavailable(ProviderId::Anthropic, "boom");
        // `unavailable()` sets its own error/status; reset so we're
        // testing the cached-reason fallback specifically.
        fresh.error = None;
        fresh.status = ProviderStatus::Ok;

        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 2);
        assert_eq!(
            fresh.error.as_deref(),
            Some("Rate-limited by Anthropic — refresh paused for 12 min"),
            "cached rate-limit reason should explain the ⚠"
        );
        assert!(matches!(fresh.status, ProviderStatus::Degraded));
    }

    #[test]
    fn merge_stale_from_uses_generic_reason_when_cache_has_none() {
        // Cache had no error of its own, but we still grafted stale
        // primary windows → the ⚠ gets a generic-but-present reason
        // rather than being bare.
        let cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        let mut fresh = UsageSnapshot::unavailable(ProviderId::Anthropic, "boom");
        fresh.error = None;
        fresh.status = ProviderStatus::Ok;

        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 2);
        let reason = fresh.error.as_deref().expect("a reason must be set");
        assert!(
            reason.contains("cached"),
            "generic fallback should mention cached data, got: {reason}"
        );
        assert!(matches!(fresh.status, ProviderStatus::Degraded));
    }

    #[test]
    fn merge_stale_from_refuses_stale_cache_older_than_max_age() {
        // Cache from 10 days ago is too old to be useful — refuse to
        // graft. Otherwise a long-dead provider would keep showing
        // ancient percentages forever.
        let mut cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        cached.timestamp = Utc::now() - chrono::Duration::days(10);
        let mut fresh = UsageSnapshot::unavailable(ProviderId::Anthropic, "boom");
        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 0, "cache older than max age must be ignored");
        assert!(fresh.windows.is_empty(), "no stale graft from old cache");
    }

    #[test]
    fn merge_stale_from_accepts_cache_just_under_max_age() {
        // Just under the 7-day cap — cache still usable. (We deliberately
        // pick `7 days - 1 hour` so future cap changes either way show
        // up as a test failure to revisit.)
        let mut cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        cached.timestamp = Utc::now() - chrono::Duration::days(7) + chrono::Duration::hours(1);
        let mut fresh = UsageSnapshot::unavailable(ProviderId::Anthropic, "boom");
        let n = fresh.merge_stale_from(&cached);
        assert!(n > 0, "cache just under max age should still graft");
    }

    #[test]
    fn merge_stale_from_rejects_cross_provider_graft() {
        // Defensive: never accept a graft from a different provider's
        // cached snapshot. (Should be unreachable in practice, but the
        // map is keyed by provider and a refactor mistake here would
        // be very confusing to debug.)
        let cached = cached_snapshot_with_fraction(ProviderId::Anthropic);
        let mut fresh = UsageSnapshot::unavailable(ProviderId::CodexCli, "x");
        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 0);
        assert!(fresh.windows.is_empty());
    }

    #[test]
    fn merge_stale_from_drops_expired_missing_window_when_poll_has_fresh_quota() {
        let now = Utc::now();
        let mut cached = UsageSnapshot {
            provider: ProviderId::Antigravity,
            timestamp: now - chrono::Duration::minutes(5),
            status: ProviderStatus::Degraded,
            error: Some("Showing cached quota — provider returned no fresh data this poll".into()),
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        };
        cached.windows.insert(
            "Claude 5h".into(),
            WindowUsage {
                fraction_used: Some(0.10),
                ends_at: Some(now + chrono::Duration::hours(3)),
                ..Default::default()
            },
        );
        cached.windows.insert(
            "Gemini week".into(),
            WindowUsage {
                fraction_used: Some(0.05),
                ends_at: Some(now - chrono::Duration::days(4)),
                stale: true,
                ..Default::default()
            },
        );

        let mut fresh = UsageSnapshot {
            provider: ProviderId::Antigravity,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: Some("Claude 5h 10% · Gemini 5h 4%".into()),
            plan_label: None,
        };
        fresh.windows.insert(
            "Claude 5h".into(),
            WindowUsage {
                fraction_used: Some(0.10),
                ends_at: Some(now + chrono::Duration::hours(3)),
                ..Default::default()
            },
        );
        fresh.windows.insert(
            "Gemini 5h".into(),
            WindowUsage {
                fraction_used: Some(0.04),
                ends_at: Some(now + chrono::Duration::hours(4)),
                ..Default::default()
            },
        );

        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 0);
        assert!(!fresh.windows.contains_key("Gemini week"));
        assert_eq!(fresh.status, ProviderStatus::Ok);
        assert!(fresh.error.is_none());
    }

    #[test]
    fn merge_stale_from_ignores_window_whose_reset_exceeds_max_age() {
        let now = Utc::now();
        let mut cached = cached_snapshot_with_fraction(ProviderId::Antigravity);
        cached.timestamp = now - chrono::Duration::minutes(5);
        cached.windows.insert(
            "Gemini 5h".into(),
            WindowUsage {
                fraction_used: Some(0.02),
                ends_at: Some(now - chrono::Duration::days(10)),
                stale: true,
                ..Default::default()
            },
        );

        let mut fresh = UsageSnapshot::unavailable(ProviderId::Antigravity, "boom");
        let n = fresh.merge_stale_from(&cached);
        assert_eq!(n, 2);
        assert!(!fresh.windows.contains_key("Gemini 5h"));
    }

    #[test]
    fn title_case_first_handles_common_inputs() {
        assert_eq!(title_case_first("plus"), "Plus");
        assert_eq!(title_case_first("pro"), "Pro");
        assert_eq!(title_case_first("a"), "A");
        assert_eq!(title_case_first(""), "");
        // Already upper — only the first char is touched, others preserved.
        assert_eq!(title_case_first("Max 5x"), "Max 5x");
        // Non-alpha first char: passes through unchanged.
        assert_eq!(title_case_first("5x"), "5x");
    }

    #[test]
    fn title_case_first_preserves_remainder_casing() {
        // Important property: we don't lowercase the tail.
        assert_eq!(title_case_first("aBC"), "ABC");
        assert_eq!(title_case_first("ABC"), "ABC");
    }

    #[test]
    fn provider_display_uses_snake_case() {
        assert_eq!(ProviderId::Anthropic.to_string(), "anthropic");
        assert_eq!(ProviderId::CodexCli.to_string(), "codex_cli");
        assert_eq!(ProviderId::OllamaCloud.to_string(), "ollama_cloud");
    }

    #[test]
    fn provider_human_names_are_stable() {
        assert_eq!(ProviderId::Anthropic.human(), "Anthropic");
        assert_eq!(ProviderId::CodexCli.human(), "Codex");
        assert_eq!(ProviderId::OllamaCloud.human(), "Ollama Cloud");
    }

    #[test]
    fn provider_tint_returns_distinct_colours() {
        let a = ProviderId::Anthropic.tint_rgb();
        let c = ProviderId::CodexCli.tint_rgb();
        let o = ProviderId::OllamaCloud.tint_rgb();
        assert_ne!(a, c);
        assert_ne!(a, o);
        assert_ne!(c, o);
    }

    #[test]
    fn window_labels_match_storage_keys() {
        // The dashboard and the tray look up windows by these exact
        // strings; regressions here silently break the grid sort.
        assert_eq!(WindowKind::LastHour.label(), "1h");
        assert_eq!(WindowKind::Today.label(), "today");
        assert_eq!(WindowKind::ThisWeek.label(), "week");
        assert_eq!(WindowKind::ThisMonth.label(), "month");
        assert_eq!(WindowKind::FiveHourRolling.label(), "5h");
    }

    #[test]
    fn recompute_fraction_prefers_dollar_limit_over_token_limit() {
        let mut w = WindowUsage::default();
        w.spend_usd = Some(20.0);
        w.limit_usd = Some(40.0);
        w.limit_tokens = Some(1_000);
        w.tokens_in = 500;
        w.recompute_fraction();
        // 20/40 = 0.5; token ratio (500/1000 = 0.5) happens to coincide
        // but we want to assert the *path*: dollar limit wins.
        assert_eq!(w.fraction_used, Some(0.5));
    }

    #[test]
    fn recompute_fraction_falls_back_to_token_limit() {
        let mut w = WindowUsage::default();
        w.limit_tokens = Some(1_000);
        w.tokens_in = 300;
        w.tokens_out = 200;
        w.recompute_fraction();
        assert_eq!(w.fraction_used, Some(0.5));
    }

    #[test]
    fn recompute_fraction_is_none_without_either_limit() {
        let mut w = WindowUsage::default();
        w.spend_usd = Some(1.0);
        w.tokens_in = 100;
        w.recompute_fraction();
        assert!(w.fraction_used.is_none());
    }

    #[test]
    fn recompute_fraction_treats_zero_limit_as_unknown() {
        let mut w = WindowUsage::default();
        w.spend_usd = Some(1.0);
        w.limit_usd = Some(0.0);
        w.recompute_fraction();
        // 0.0 limit means "no real limit set" — don't divide by zero.
        assert!(w.fraction_used.is_none());
    }

    #[test]
    fn unavailable_constructor_sets_status_and_error() {
        let snap = UsageSnapshot::unavailable(ProviderId::CodexCli, "no creds");
        assert_eq!(snap.provider, ProviderId::CodexCli);
        assert_eq!(snap.status, ProviderStatus::Unavailable);
        assert_eq!(snap.error.as_deref(), Some("no creds"));
        assert!(snap.windows.is_empty());
        assert!(snap.plan_label.is_none());
    }

    fn make_snap_with_windows() -> UsageSnapshot {
        let mut snap = UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        };
        // Spend-derived (no ends_at) — strip_spend should clear fraction.
        let today = snap.window_mut(WindowKind::Today);
        today.spend_usd = Some(4.21);
        today.limit_usd = Some(10.0);
        today.fraction_used = Some(0.421);
        // OAuth-set (has ends_at) — strip_spend must preserve fraction.
        let week = snap.window_mut(WindowKind::ThisWeek);
        week.spend_usd = Some(99.0);
        week.limit_usd = Some(100.0);
        week.fraction_used = Some(0.6);
        week.ends_at = Some(Utc::now() + chrono::Duration::days(2));
        snap
    }

    #[test]
    fn strip_spend_clears_dollar_fields_everywhere() {
        let mut snap = make_snap_with_windows();
        snap.strip_spend();
        for w in snap.windows.values() {
            assert!(w.spend_usd.is_none());
            assert!(w.limit_usd.is_none());
        }
    }

    #[test]
    fn strip_spend_keeps_oauth_fractions() {
        let mut snap = make_snap_with_windows();
        snap.strip_spend();
        let today = snap.window(WindowKind::Today).unwrap();
        let week = snap.window(WindowKind::ThisWeek).unwrap();
        // Today had no ends_at — fraction wiped along with spend.
        assert!(today.fraction_used.is_none());
        // Week had ends_at (OAuth-set) — fraction survives.
        assert_eq!(week.fraction_used, Some(0.6));
    }

    #[test]
    fn window_mut_creates_when_absent() {
        let mut snap = UsageSnapshot::unavailable(ProviderId::Anthropic, "_");
        let w = snap.window_mut(WindowKind::Today);
        w.tokens_in = 42;
        let r = snap.window(WindowKind::Today).unwrap();
        assert_eq!(r.tokens_in, 42);
    }

    #[test]
    fn mark_stale_if_expired_flags_past_ends_at_beyond_grace() {
        let now = Utc::now();
        let mut w = WindowUsage {
            fraction_used: Some(0.97),
            ends_at: Some(now - chrono::Duration::minutes(10)),
            ..Default::default()
        };
        w.mark_stale_if_expired(now);
        assert!(w.stale);
    }

    #[test]
    fn mark_stale_if_expired_within_grace_leaves_false() {
        let now = Utc::now();
        let mut w = WindowUsage {
            fraction_used: Some(0.80),
            ends_at: Some(now - chrono::Duration::seconds(30)),
            ..Default::default()
        };
        w.mark_stale_if_expired(now);
        assert!(!w.stale);
    }

    #[test]
    fn mark_stale_if_expired_future_ends_at_not_stale() {
        let now = Utc::now();
        let mut w = WindowUsage {
            fraction_used: Some(0.50),
            ends_at: Some(now + chrono::Duration::hours(3)),
            ..Default::default()
        };
        w.mark_stale_if_expired(now);
        assert!(!w.stale);
    }

    #[test]
    fn mark_stale_if_expired_no_ends_at_not_stale() {
        let now = Utc::now();
        let mut w = WindowUsage {
            fraction_used: Some(0.50),
            ..Default::default()
        };
        w.mark_stale_if_expired(now);
        assert!(!w.stale);
    }

    #[test]
    fn pace_fraction_maps_elapsed_to_zero_one() {
        // 1h left of a 5h window → 4/5 elapsed.
        let now = Utc::now();
        let w = WindowUsage {
            ends_at: Some(now + chrono::Duration::hours(1)),
            ..Default::default()
        };
        for label in ["5h", "Claude 5h", "Gemini 5h"] {
            let p = w.pace_fraction(label, now).expect(label);
            assert!((p - 0.8).abs() < 0.01, "{label}: got {p}");
        }
        // 2d left of a 7d window → 5/7 elapsed.
        let w = WindowUsage {
            ends_at: Some(now + chrono::Duration::days(2)),
            ..Default::default()
        };
        for label in ["week", "Sonnet", "Opus", "Claude week", "Gemini week"] {
            let p = w.pace_fraction(label, now).expect(label);
            assert!((p - (5.0 / 7.0)).abs() < 0.01, "{label}: got {p}");
        }
    }

    #[test]
    fn pace_fraction_returns_none_when_reset_contradicts_window() {
        // Live Antigravity/Ollama rows report resets days beyond a
        // "5h" label. A marker pinned at zero would read "window just
        // started", so report no pace instead.
        let now = Utc::now();
        for (label, ends) in [
            ("5h", now + chrono::Duration::days(5)),
            ("Gemini 5h", now + chrono::Duration::days(5)),
            ("week", now + chrono::Duration::days(12)),
            ("Claude week", now + chrono::Duration::days(11)),
        ] {
            let w = WindowUsage {
                ends_at: Some(ends),
                ..Default::default()
            };
            assert!(w.pace_fraction(label, now).is_none(), "{label}");
        }
    }

    #[test]
    fn pace_fraction_pins_past_reset_to_one() {
        let now = Utc::now();
        let w = WindowUsage {
            ends_at: Some(now - chrono::Duration::hours(1)),
            ..Default::default()
        };
        assert_eq!(w.pace_fraction("5h", now), Some(1.0));
    }

    #[test]
    fn pace_fraction_returns_none_without_duration_or_reset() {
        let now = Utc::now();
        let w = WindowUsage {
            ends_at: Some(now + chrono::Duration::hours(1)),
            ..Default::default()
        };
        assert!(w.pace_fraction("today", now).is_none());
        assert!(w.pace_fraction("month", now).is_none());
        let w = WindowUsage::default();
        assert!(w.pace_fraction("5h", now).is_none());
    }

    #[test]
    fn pace_fraction_prefers_provider_window_secs() {
        // Codex reports its own window length; it wins over the label.
        let now = Utc::now();
        let w = WindowUsage {
            ends_at: Some(now + chrono::Duration::minutes(30)),
            window_secs: Some(3600),
            ..Default::default()
        };
        let p = w.pace_fraction("5h", now).expect("override wins");
        assert!((p - 0.5).abs() < 0.01, "got {p}");
        // Non-positive overrides fall back to the label default.
        let w = WindowUsage {
            ends_at: Some(now + chrono::Duration::hours(1)),
            window_secs: Some(0),
            ..Default::default()
        };
        let p = w.pace_fraction("5h", now).expect("label fallback");
        assert!((p - 0.8).abs() < 0.01, "got {p}");
    }
}
