use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Anthropic,
    OpenAi,
    CodexCli,
    GeminiCli,
    OllamaLocal,
    OllamaCloud,
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderId::Anthropic => "anthropic",
            ProviderId::OpenAi => "openai",
            ProviderId::CodexCli => "codex_cli",
            ProviderId::GeminiCli => "gemini_cli",
            ProviderId::OllamaLocal => "ollama_local",
            ProviderId::OllamaCloud => "ollama_cloud",
        };
        f.write_str(s)
    }
}

impl ProviderId {
    pub fn human(&self) -> &'static str {
        match self {
            ProviderId::Anthropic => "Anthropic",
            ProviderId::OpenAi => "OpenAI API",
            ProviderId::CodexCli => "Codex CLI",
            ProviderId::GeminiCli => "Gemini CLI",
            ProviderId::OllamaLocal => "Ollama (local)",
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
            ProviderId::OllamaCloud => (0x3B, 0x82, 0xF6),
            ProviderId::OpenAi => (0x10, 0xA3, 0x7F),
            ProviderId::CodexCli => (0x9C, 0x6B, 0xFF),
            ProviderId::GeminiCli => (0x42, 0x85, 0xF4),
            ProviderId::OllamaLocal => (0x60, 0x60, 0x60),
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
