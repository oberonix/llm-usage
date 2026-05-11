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
    /// Draw the 1 px red "pace" line across each tray-icon bar at the
    /// elapsed-fraction of the corresponding window. Off keeps the
    /// icon as just the fill bars.
    pub show_pace_marker: bool,
    pub anthropic: AnthropicConfig,
    pub codex_cli: CodexCliConfig,
    pub ollama_cloud: OllamaCloudConfig,
    pub alerts: AlertsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 15 minutes by default. Local file readers (Anthropic JSONL,
            // Codex) are cheap, but Anthropic's OAuth /usage endpoint
            // rate-limits aggressive polling, so we bias the whole loop
            // towards "informative, not chatty".
            poll_interval_secs: 900,
            icon_rotation_secs: 15,
            show_pace_marker: true,
            anthropic: AnthropicConfig::default(),
            codex_cli: CodexCliConfig::default(),
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
pub struct CodexCliConfig {
    pub enabled: bool,
    /// Override path; defaults to ~/.codex.
    pub codex_dir: Option<PathBuf>,
    /// Fractions (0..1) at which to fire a quota alert for any window
    /// with a known utilization. Mirrors the same field on the other
    /// providers — kept here as a single unified list so the dashboard
    /// can show one threshold dropdown per provider.
    pub warn_at: Vec<f64>,
    /// When false (default) the UI shows turn counts and tokens but hides
    /// the reverse-engineered dollar estimate.
    pub show_spend: bool,
}

impl Default for CodexCliConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            codex_dir: None,
            warn_at: vec![0.75, 0.9],
            show_spend: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaCloudConfig {
    pub enabled: bool,
    /// Browser session cookie — required for the settings/billing page
    /// scrape. The dashboard's setup buttons populate this for you; the
    /// raw `Cookie:` request header value is the expected format
    /// (e.g. `session=abc123; csrf=xyz`).
    pub session_cookie: Option<String>,
    pub warn_at: Vec<f64>,
}

impl Default for OllamaCloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_cookie: None,
            warn_at: vec![0.75, 0.9],
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

/// PID file used to enforce a singleton window for `name` (e.g.
/// "dashboard" or "popup"). A second instance writes the matching
/// focus-trigger file and exits.
pub fn singleton_pid_path(name: &str) -> Result<PathBuf> {
    Ok(project_dirs()?
        .data_dir()
        .join(format!("{}.pid", name)))
}

/// Companion file to `singleton_pid_path`. The running instance watches
/// for writes here and brings its window to the foreground.
pub fn singleton_focus_trigger_path(name: &str) -> Result<PathBuf> {
    Ok(project_dirs()?
        .data_dir()
        .join(format!("{}.focus", name)))
}
