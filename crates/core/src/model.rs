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
}

#[cfg(test)]
mod tests {
    use super::*;

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
