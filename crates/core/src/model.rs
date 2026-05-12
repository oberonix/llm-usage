use chrono::{DateTime, Utc};
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
    CodexCli,
    OllamaCloud,
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderId::Anthropic => "anthropic",
            ProviderId::CodexCli => "codex_cli",
            ProviderId::OllamaCloud => "ollama_cloud",
        };
        f.write_str(s)
    }
}

impl ProviderId {
    pub fn human(&self) -> &'static str {
        match self {
            ProviderId::Anthropic => "Anthropic",
            ProviderId::CodexCli => "Codex",
            ProviderId::OllamaCloud => "Ollama Cloud",
        }
    }

    /// Brand-ish accent color for this provider. Used by the tray icon
    /// for its background tint and by the dashboard for the card's
    /// left-edge accent — keeping the two in lockstep so users can
    /// match a tray colour to a card at a glance.
    pub fn tint_rgb(&self) -> (u8, u8, u8) {
        match self {
            ProviderId::Anthropic => (0xCC, 0x78, 0x5C),
            ProviderId::CodexCli => (0x9C, 0x6B, 0xFF),
            ProviderId::OllamaCloud => (0x3B, 0x82, 0xF6),
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
        self.windows
            .entry(kind.label().to_string())
            .or_default()
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
    /// Returns the number of windows that ended up stale, so callers
    /// can log "served 2 stale windows from cache" if useful.
    pub fn merge_stale_from(&mut self, cached: &UsageSnapshot) -> usize {
        if self.provider != cached.provider {
            return 0;
        }
        let mut stale_count = 0;
        for (label, cw) in &cached.windows {
            if cw.fraction_used.is_none() {
                continue; // cache has nothing useful to graft
            }
            match self.windows.get_mut(label) {
                Some(nw) if nw.fraction_used.is_none() => {
                    nw.fraction_used = cw.fraction_used;
                    nw.ends_at = cw.ends_at;
                    nw.started_at = cw.started_at;
                    nw.limit_usd = nw.limit_usd.or(cw.limit_usd);
                    nw.limit_tokens = nw.limit_tokens.or(cw.limit_tokens);
                    nw.stale = true;
                    stale_count += 1;
                }
                None => {
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
        // window() should now return the same data.
        let r = snap.window(WindowKind::Today).unwrap();
        assert_eq!(r.tokens_in, 42);
    }
}
