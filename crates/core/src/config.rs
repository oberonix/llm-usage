use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::pricing::ModelRate;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub poll_interval_secs: u64,
    /// How often the tray icon swaps to the next quota-bearing provider.
    /// Stored in seconds; clamped to a minimum of 5 at read time so a
    /// hand-edited config can't make the icon flicker.
    pub icon_rotation_secs: u64,
    pub anthropic: AnthropicConfig,
    pub openai: OpenAiConfig,
    pub codex_cli: CodexCliConfig,
    pub gemini_cli: GeminiCliConfig,
    pub ollama_local: OllamaLocalConfig,
    pub ollama_cloud: OllamaCloudConfig,
    pub alerts: AlertsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 15 minutes by default. Local file readers (Anthropic JSONL,
            // Codex, Gemini) and the Ollama ping are cheap, but Anthropic's
            // OAuth /usage endpoint rate-limits aggressive polling, so we
            // bias the whole loop towards "informative, not chatty".
            poll_interval_secs: 900,
            icon_rotation_secs: 15,
            anthropic: AnthropicConfig::default(),
            openai: OpenAiConfig::default(),
            codex_cli: CodexCliConfig::default(),
            gemini_cli: GeminiCliConfig::default(),
            ollama_local: OllamaLocalConfig::default(),
            ollama_cloud: OllamaCloudConfig::default(),
            alerts: AlertsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicConfig {
    pub enabled: bool,
    /// Override path; defaults to ~/.claude/projects.
    pub claude_projects_dir: Option<PathBuf>,
    pub weekly_budget_usd: Option<f64>,
    pub daily_budget_usd: Option<f64>,
    pub warn_at: Vec<f64>,
    /// Per-model rate overrides (key = model id substring).
    pub model_rates: HashMap<String, ModelRate>,
    /// When false (default) the UI shows quota only; dollar amounts and
    /// dollar-derived progress bars are hidden. JSONL parsing still runs
    /// internally so toggling this on later is instant.
    pub show_spend: bool,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            claude_projects_dir: None,
            weekly_budget_usd: Some(50.0),
            daily_budget_usd: None,
            warn_at: vec![0.5, 0.75, 0.9],
            model_rates: HashMap::new(),
            show_spend: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub enabled: bool,
    /// Pulled from env OPENAI_API_KEY if not set here.
    pub api_key: Option<String>,
    pub organization: Option<String>,
    pub monthly_budget_usd: Option<f64>,
    pub warn_at: Vec<f64>,
    /// OpenAI exposes no non-spend quota, so when this is false (default)
    /// the provider reports "spend tracking hidden" and is skipped in the
    /// tray menu.
    pub show_spend: bool,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            organization: None,
            monthly_budget_usd: Some(30.0),
            warn_at: vec![0.75, 0.9],
            show_spend: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexCliConfig {
    pub enabled: bool,
    /// Override path; defaults to ~/.codex.
    pub codex_dir: Option<PathBuf>,
    /// Fraction (0..1) of the rolling 5h window after which to alert.
    pub five_hour_warn: f64,
    pub weekly_warn: f64,
    /// When false (default) the UI shows turn counts and tokens but hides
    /// the reverse-engineered dollar estimate.
    pub show_spend: bool,
}

impl Default for CodexCliConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            codex_dir: None,
            five_hour_warn: 0.8,
            weekly_warn: 0.85,
            show_spend: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiCliConfig {
    pub enabled: bool,
    /// Override path; defaults to ~/.gemini.
    pub gemini_dir: Option<PathBuf>,
    pub monthly_budget_usd: Option<f64>,
    pub warn_at: Vec<f64>,
    /// When false (default) the UI shows turn counts and tokens but hides
    /// the dollar estimate.
    pub show_spend: bool,
}

impl Default for GeminiCliConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gemini_dir: None,
            monthly_budget_usd: None,
            warn_at: vec![0.75, 0.9],
            show_spend: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaLocalConfig {
    pub enabled: bool,
    pub base_url: String,
}

impl Default for OllamaLocalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "http://localhost:11434".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaCloudConfig {
    pub enabled: bool,
    /// API key (currently unused — kept for the day Ollama publishes a usage API).
    pub api_key: Option<String>,
    /// Browser session cookie copied out of devtools — required for the
    /// settings/billing page scrape. Format: the full Cookie header value
    /// (e.g. `session=abc123; csrf=xyz`). Whatever Ollama's auth cookie is
    /// called, paste the raw header.
    pub session_cookie: Option<String>,
    pub base_url: String,
}

impl Default for OllamaCloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            session_cookie: None,
            base_url: "https://ollama.com".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    /// Minimum gap (seconds) between two notifications for the same provider+threshold.
    pub debounce_secs: u64,
    /// Skip notifications entirely (useful for tests/CI).
    pub disabled: bool,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            debounce_secs: 3600,
            disabled: false,
        }
    }
}

impl Config {
    pub fn load_or_default() -> Result<Self> {
        let path = config_path()?;
        if path.exists() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Self = toml::from_str(&s)
            .with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }
}

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("dev", "buffbit", "llm-usage")
        .context("could not resolve OS project dirs")
}

pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

pub fn data_path() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().join("usage.sqlite"))
}

/// Shared snapshot file: the tray writes here after every poll, the
/// dashboard reads from it. Co-located with the sqlite store under the
/// OS data directory.
pub fn snapshots_path() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().join("snapshots.json"))
}

/// Trigger file the dashboard touches when the user clicks "Refresh".
/// The tray watches it and forces an immediate poll. Co-located with
/// the snapshot file so a single watcher on the data directory catches
/// both.
pub fn refresh_trigger_path() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().join("refresh.trigger"))
}
