//! Editable form state for the Settings tab. The form mirrors the
//! save-to-disk fields of `Config` — fields like per-model pricing overrides
//! that are only meaningful to power users stay edit-via-TOML for now.

use eframe::egui::{self, Color32, RichText};
use llm_usage_core::config::{
    self, AnthropicConfig, CodexCliConfig, Config, GeminiCliConfig, OllamaCloudConfig,
    OllamaLocalConfig, OpenAiConfig,
};
use llm_usage_core::model::ProviderId;
use std::path::PathBuf;

/// Result of a `Save` press — used to render a toast at the top of the window.
pub enum SaveOutcome {
    Saved(PathBuf),
    Error(String),
}

/// Editable mirror of the user-facing parts of `Config`. Strings stay strings
/// (text fields); numbers stay numbers (DragValue widgets); thresholds stay
/// as a comma-separated string for ergonomic editing.
pub struct ConfigDraft {
    pub poll_interval_secs: u64,

    pub anthropic_enabled: bool,
    pub anthropic_show_spend: bool,
    pub anthropic_weekly_budget_usd: f64,
    pub anthropic_weekly_budget_enabled: bool,
    pub anthropic_warn_at: String,

    pub openai_enabled: bool,
    pub openai_show_spend: bool,
    pub openai_api_key: String,
    pub openai_organization: String,
    pub openai_monthly_budget_usd: f64,
    pub openai_monthly_budget_enabled: bool,
    pub openai_warn_at: String,

    pub codex_enabled: bool,
    pub codex_show_spend: bool,
    pub codex_five_hour_warn: f64,
    pub codex_weekly_warn: f64,

    pub gemini_enabled: bool,
    pub gemini_show_spend: bool,
    pub gemini_monthly_budget_usd: f64,
    pub gemini_monthly_budget_enabled: bool,
    pub gemini_warn_at: String,

    pub ollama_local_enabled: bool,
    pub ollama_local_base_url: String,

    pub ollama_cloud_enabled: bool,
    pub ollama_cloud_api_key: String,
    pub ollama_cloud_session_cookie: String,
    pub ollama_cloud_base_url: String,

    // UI-only state for the setup-login flow. Not persisted.
    pub ollama_cloud_setup_status: Option<String>,
    pub ollama_cloud_setup_rx: Option<std::sync::mpsc::Receiver<SetupResult>>,
}

pub enum SetupResult {
    /// Setup tool exited successfully; config.toml was rewritten.
    Captured,
    Failed(String),
}

impl ConfigDraft {
    pub fn from_config(c: &Config) -> Self {
        Self {
            poll_interval_secs: c.poll_interval_secs,

            anthropic_enabled: c.anthropic.enabled,
            anthropic_show_spend: c.anthropic.show_spend,
            anthropic_weekly_budget_usd: c.anthropic.weekly_budget_usd.unwrap_or(50.0),
            anthropic_weekly_budget_enabled: c.anthropic.weekly_budget_usd.is_some(),
            anthropic_warn_at: format_thresholds(&c.anthropic.warn_at),

            openai_enabled: c.openai.enabled,
            openai_show_spend: c.openai.show_spend,
            openai_api_key: c.openai.api_key.clone().unwrap_or_default(),
            openai_organization: c.openai.organization.clone().unwrap_or_default(),
            openai_monthly_budget_usd: c.openai.monthly_budget_usd.unwrap_or(30.0),
            openai_monthly_budget_enabled: c.openai.monthly_budget_usd.is_some(),
            openai_warn_at: format_thresholds(&c.openai.warn_at),

            codex_enabled: c.codex_cli.enabled,
            codex_show_spend: c.codex_cli.show_spend,
            codex_five_hour_warn: c.codex_cli.five_hour_warn,
            codex_weekly_warn: c.codex_cli.weekly_warn,

            gemini_enabled: c.gemini_cli.enabled,
            gemini_show_spend: c.gemini_cli.show_spend,
            gemini_monthly_budget_usd: c.gemini_cli.monthly_budget_usd.unwrap_or(20.0),
            gemini_monthly_budget_enabled: c.gemini_cli.monthly_budget_usd.is_some(),
            gemini_warn_at: format_thresholds(&c.gemini_cli.warn_at),

            ollama_local_enabled: c.ollama_local.enabled,
            ollama_local_base_url: c.ollama_local.base_url.clone(),

            ollama_cloud_enabled: c.ollama_cloud.enabled,
            ollama_cloud_api_key: c.ollama_cloud.api_key.clone().unwrap_or_default(),
            ollama_cloud_session_cookie: c
                .ollama_cloud
                .session_cookie
                .clone()
                .unwrap_or_default(),
            ollama_cloud_base_url: c.ollama_cloud.base_url.clone(),

            ollama_cloud_setup_status: None,
            ollama_cloud_setup_rx: None,
        }
    }

    /// Apply the draft fields onto a fresh Config (keeping the source's
    /// non-editable fields like `model_rates` untouched by reading them from
    /// the existing on-disk config).
    pub fn to_config(&self) -> Config {
        let mut c = Config::load_or_default().unwrap_or_default();
        c.poll_interval_secs = self.poll_interval_secs.max(60);

        c.anthropic = AnthropicConfig {
            enabled: self.anthropic_enabled,
            show_spend: self.anthropic_show_spend,
            weekly_budget_usd: if self.anthropic_weekly_budget_enabled {
                Some(self.anthropic_weekly_budget_usd)
            } else {
                None
            },
            warn_at: parse_thresholds(&self.anthropic_warn_at),
            ..c.anthropic
        };

        c.openai = OpenAiConfig {
            enabled: self.openai_enabled,
            show_spend: self.openai_show_spend,
            api_key: empty_to_none(&self.openai_api_key),
            organization: empty_to_none(&self.openai_organization),
            monthly_budget_usd: if self.openai_monthly_budget_enabled {
                Some(self.openai_monthly_budget_usd)
            } else {
                None
            },
            warn_at: parse_thresholds(&self.openai_warn_at),
        };

        c.codex_cli = CodexCliConfig {
            enabled: self.codex_enabled,
            show_spend: self.codex_show_spend,
            five_hour_warn: self.codex_five_hour_warn.clamp(0.0, 1.0),
            weekly_warn: self.codex_weekly_warn.clamp(0.0, 1.0),
            ..c.codex_cli
        };

        c.gemini_cli = GeminiCliConfig {
            enabled: self.gemini_enabled,
            show_spend: self.gemini_show_spend,
            monthly_budget_usd: if self.gemini_monthly_budget_enabled {
                Some(self.gemini_monthly_budget_usd)
            } else {
                None
            },
            warn_at: parse_thresholds(&self.gemini_warn_at),
            ..c.gemini_cli
        };

        c.ollama_local = OllamaLocalConfig {
            enabled: self.ollama_local_enabled,
            base_url: self.ollama_local_base_url.trim().to_string(),
        };

        c.ollama_cloud = OllamaCloudConfig {
            enabled: self.ollama_cloud_enabled,
            api_key: empty_to_none(&self.ollama_cloud_api_key),
            session_cookie: empty_to_none(&self.ollama_cloud_session_cookie),
            base_url: self.ollama_cloud_base_url.trim().to_string(),
        };

        c
    }

    /// Spawn the `llm-usage-setup` sibling binary in a background
    /// thread. The thread blocks on the child's exit, then sends a
    /// `SetupResult` back to the UI via mpsc — `poll_setup_result` picks
    /// it up on the next frame and refreshes the captured-cookie field.
    fn start_setup_tool(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let exe = match resolve_setup_binary() {
            Some(p) => p,
            None => {
                self.ollama_cloud_setup_status = Some(
                    "Could not find llm-usage-setup binary next to the dashboard."
                        .into(),
                );
                return;
            }
        };
        self.ollama_cloud_setup_rx = Some(rx);
        self.ollama_cloud_setup_status = Some(
            "Setup window opened — sign in to Ollama Cloud; the form will \
             refresh automatically when capture completes."
                .into(),
        );
        std::thread::spawn(move || {
            let result = match std::process::Command::new(&exe).status() {
                Ok(status) if status.success() => SetupResult::Captured,
                Ok(status) => SetupResult::Failed(format!(
                    "setup tool exited with {}",
                    status
                )),
                Err(e) => SetupResult::Failed(format!("spawn failed: {}", e)),
            };
            let _ = tx.send(result);
        });
    }

    /// Read the user's ollama.com session cookie out of an
    /// already-logged-in browser (Chrome, Firefox, Edge, Brave, Safari,
    /// Vivaldi, …) using the `rookie` crate. No window pops up; the
    /// platform may show a keyring auth prompt the first time on Linux
    /// (Chrome's cookie DB is encrypted with libsecret).
    fn import_from_browser(&mut self) {
        let cookies = match rookie::load(Some(vec!["ollama.com".to_string()])) {
            Ok(c) => c,
            Err(e) => {
                self.ollama_cloud_setup_status =
                    Some(format!("Browser import failed: {}", e));
                return;
            }
        };
        let header: String = cookies
            .iter()
            .filter(|c| c.domain.trim_start_matches('.').ends_with("ollama.com"))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        if header.is_empty() {
            self.ollama_cloud_setup_status = Some(
                "No ollama.com cookies found in any installed browser. \
                 Sign in to ollama.com in your browser first, then click again."
                    .into(),
            );
            return;
        }
        let path = match config::config_path() {
            Ok(p) => p,
            Err(e) => {
                self.ollama_cloud_setup_status =
                    Some(format!("Could not resolve config path: {}", e));
                return;
            }
        };
        let mut cfg = config::Config::load_or_default().unwrap_or_default();
        cfg.ollama_cloud.enabled = true;
        cfg.ollama_cloud.session_cookie = Some(header.clone());
        if let Err(e) = cfg.save(&path) {
            self.ollama_cloud_setup_status = Some(format!("Save failed: {}", e));
            return;
        }
        self.ollama_cloud_session_cookie = header;
        self.ollama_cloud_enabled = true;
        self.ollama_cloud_setup_status = Some(format!(
            "Imported from browser. Saved to {}.",
            path.display()
        ));
    }

    /// Drain any pending setup result from the channel and apply it to
    /// the form. Called once per frame from `render`.
    fn poll_setup_result(&mut self) {
        let Some(rx) = &self.ollama_cloud_setup_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        match result {
            SetupResult::Captured => {
                // Setup tool wrote directly to config.toml — re-read it
                // so the form reflects the new cookie. We don't clobber
                // the rest of the draft because the user may have
                // unsaved edits in other sections.
                if let Ok(cfg) = config::Config::load_or_default() {
                    self.ollama_cloud_session_cookie = cfg
                        .ollama_cloud
                        .session_cookie
                        .clone()
                        .unwrap_or_default();
                    self.ollama_cloud_enabled = cfg.ollama_cloud.enabled;
                }
                self.ollama_cloud_setup_status =
                    Some("Captured. Cookie saved to config.toml.".into());
            }
            SetupResult::Failed(e) => {
                self.ollama_cloud_setup_status = Some(format!("Setup failed: {}", e));
            }
        }
        self.ollama_cloud_setup_rx = None;
    }

    pub fn save(&self) -> SaveOutcome {
        let path = match config::config_path() {
            Ok(p) => p,
            Err(e) => return SaveOutcome::Error(format!("config path: {}", e)),
        };
        let cfg = self.to_config();
        match cfg.save(&path) {
            Ok(()) => SaveOutcome::Saved(path),
            Err(e) => SaveOutcome::Error(e.to_string()),
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) {
        self.poll_setup_result();

        section_header(ui, "Polling");
        provider_card(ui, neutral_tint(), |ui| {
            section_header_row(ui, "Refresh interval", None);
            field_row(ui, "Every", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.poll_interval_secs)
                        .speed(60.0)
                        .range(60..=86_400)
                        .suffix(" sec"),
                );
                ui.weak(format!(
                    "≈ {} min (minimum 60 s, default 900)",
                    (self.poll_interval_secs as f64 / 60.0).round() as u64
                ));
            });
        });

        ui.add_space(14.0);
        section_header(ui, "Providers");

        self.render_anthropic(ui);
        self.render_openai(ui);
        self.render_codex(ui);
        self.render_gemini(ui);
        self.render_ollama_local(ui);
        self.render_ollama_cloud(ui);
    }

    fn render_anthropic(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::Anthropic), |ui| {
            section_header_row(ui, "Anthropic (Claude Code)", Some(ProviderId::Anthropic));
            enabled_row(
                ui,
                &mut self.anthropic_enabled,
                Some(&mut self.anthropic_show_spend),
                "Show $ spend",
            );
            thresholds_row(ui, "Alert at", &mut self.anthropic_warn_at);
            if self.anthropic_show_spend {
                field_row(ui, "Weekly budget", |ui| {
                    ui.checkbox(&mut self.anthropic_weekly_budget_enabled, "set");
                    ui.add_enabled(
                        self.anthropic_weekly_budget_enabled,
                        egui::DragValue::new(&mut self.anthropic_weekly_budget_usd)
                            .speed(1.0)
                            .range(0.0..=100_000.0)
                            .prefix("$"),
                    );
                });
            }
            help(
                ui,
                "Quota (5h / 7d) comes from Anthropic OAuth — no extra setup beyond \
                 being signed in to Claude Code on this machine.",
            );
        });
    }

    fn render_openai(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::OpenAi), |ui| {
            section_header_row(ui, "OpenAI API", Some(ProviderId::OpenAi));
            enabled_row(
                ui,
                &mut self.openai_enabled,
                Some(&mut self.openai_show_spend),
                "Show $ spend",
            );
            if !self.openai_show_spend {
                help(
                    ui,
                    "OpenAI exposes no non-spend quota — enable \"Show $ spend\" \
                     for this provider to do anything.",
                );
            }
            field_row(ui, "API key", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.openai_api_key)
                        .password(true)
                        .desired_width(360.0)
                        .hint_text("sk-… (or leave empty to read $OPENAI_API_KEY)"),
                );
            });
            field_row(ui, "Organization", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.openai_organization)
                        .desired_width(280.0)
                        .hint_text("optional (org-…)"),
                );
            });
            if self.openai_show_spend {
                field_row(ui, "Monthly budget", |ui| {
                    ui.checkbox(&mut self.openai_monthly_budget_enabled, "set");
                    ui.add_enabled(
                        self.openai_monthly_budget_enabled,
                        egui::DragValue::new(&mut self.openai_monthly_budget_usd)
                            .speed(1.0)
                            .range(0.0..=100_000.0)
                            .prefix("$"),
                    );
                });
            }
            thresholds_row(ui, "Alert at", &mut self.openai_warn_at);
        });
    }

    fn render_codex(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::CodexCli), |ui| {
            section_header_row(ui, "Codex CLI", Some(ProviderId::CodexCli));
            enabled_row(
                ui,
                &mut self.codex_enabled,
                Some(&mut self.codex_show_spend),
                "Show $ spend (estimate)",
            );
            field_row(ui, "5h warn", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.codex_five_hour_warn)
                        .speed(0.05)
                        .range(0.0..=1.0)
                        .fixed_decimals(2),
                );
                ui.add_space(12.0);
                ui.label("Weekly warn");
                ui.add(
                    egui::DragValue::new(&mut self.codex_weekly_warn)
                        .speed(0.05)
                        .range(0.0..=1.0)
                        .fixed_decimals(2),
                );
                ui.weak("(0.00–1.00 fraction)");
            });
            help(
                ui,
                "Codex plan limits aren't exposed locally — these thresholds fire on \
                 the rolling 5h and 7d turn-count windows we estimate from session logs.",
            );
        });
    }

    fn render_gemini(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::GeminiCli), |ui| {
            section_header_row(ui, "Gemini CLI", Some(ProviderId::GeminiCli));
            enabled_row(
                ui,
                &mut self.gemini_enabled,
                Some(&mut self.gemini_show_spend),
                "Show $ spend (estimate)",
            );
            if self.gemini_show_spend {
                field_row(ui, "Monthly budget", |ui| {
                    ui.checkbox(&mut self.gemini_monthly_budget_enabled, "set");
                    ui.add_enabled(
                        self.gemini_monthly_budget_enabled,
                        egui::DragValue::new(&mut self.gemini_monthly_budget_usd)
                            .speed(1.0)
                            .range(0.0..=100_000.0)
                            .prefix("$"),
                    );
                });
            }
            thresholds_row(ui, "Alert at", &mut self.gemini_warn_at);
        });
    }

    fn render_ollama_local(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::OllamaLocal), |ui| {
            section_header_row(ui, "Ollama (local)", Some(ProviderId::OllamaLocal));
            enabled_row(ui, &mut self.ollama_local_enabled, None, "");
            field_row(ui, "Base URL", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.ollama_local_base_url)
                        .desired_width(360.0)
                        .hint_text("http://localhost:11434"),
                );
            });
        });
    }

    fn render_ollama_cloud(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::OllamaCloud), |ui| {
            section_header_row(ui, "Ollama Cloud", Some(ProviderId::OllamaCloud));
            enabled_row(ui, &mut self.ollama_cloud_enabled, None, "");
            field_row(ui, "Base URL", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.ollama_cloud_base_url)
                        .desired_width(360.0)
                        .hint_text("https://ollama.com"),
                );
            });

            ui.add_space(6.0);
            ui.label(
                RichText::new("Sign in")
                    .strong()
                    .size(12.5)
                    .color(Color32::from_gray(200)),
            );
            let captured = !self.ollama_cloud_session_cookie.is_empty();
            let in_flight = self.ollama_cloud_setup_rx.is_some();
            let mut launch_now = false;
            let mut import_now = false;
            ui.horizontal(|ui| {
                let popup_label = if in_flight {
                    "Setup window open…"
                } else if captured {
                    "Re-run popup sign-in"
                } else {
                    "Sign in via popup window…"
                };
                if ui
                    .add_enabled(!in_flight, egui::Button::new(popup_label))
                    .on_hover_text(
                        "Opens an embedded browser at ollama.com/signin and \
                         captures the cookie automatically.",
                    )
                    .clicked()
                {
                    launch_now = true;
                }
                if ui
                    .add_enabled(!in_flight, egui::Button::new("Import from browser…"))
                    .on_hover_text(
                        "Reads the ollama.com cookie from any already-logged-in \
                         desktop browser. Zero clicks beyond this button.",
                    )
                    .clicked()
                {
                    import_now = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if captured {
                        ui.colored_label(
                            Color32::from_rgb(0x4C, 0xAF, 0x50),
                            "✓ cookie captured",
                        );
                    }
                });
            });
            if launch_now {
                self.start_setup_tool();
            }
            if import_now {
                self.import_from_browser();
            }
            if let Some(msg) = &self.ollama_cloud_setup_status {
                ui.add_space(2.0);
                ui.weak(msg);
            }

            ui.add_space(4.0);
            ui.collapsing(
                RichText::new("Manual cookie / API key (advanced)").size(11.5),
                |ui| {
                    field_row(ui, "Session cookie", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ollama_cloud_session_cookie)
                                .password(true)
                                .desired_width(360.0)
                                .hint_text("paste browser Cookie header"),
                        );
                    });
                    field_row(ui, "API key", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ollama_cloud_api_key)
                                .password(true)
                                .desired_width(360.0)
                                .hint_text("(reserved for when Ollama ships a usage API)"),
                        );
                    });
                },
            );
            help(
                ui,
                "Usage is scraped from /settings using your session cookie. Re-run \
                 sign-in if you log out or rotate.",
            );
        });
    }
}

// ----- shared section/card helpers (Settings-only; render uses these
// to line up with the Status tab's `card_frame`) -----

fn tint(id: ProviderId) -> Color32 {
    let (r, g, b) = id.tint_rgb();
    Color32::from_rgb(r, g, b)
}

fn neutral_tint() -> Color32 {
    Color32::from_rgb(0x60, 0x60, 0x60)
}

fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(text)
            .strong()
            .size(15.0)
            .color(Color32::from_gray(220)),
    );
    ui.add_space(4.0);
}

fn provider_card(ui: &mut egui::Ui, tint: Color32, body: impl FnOnce(&mut egui::Ui)) {
    crate::card_frame(ui, tint, body);
    ui.add_space(8.0);
}

fn section_header_row(ui: &mut egui::Ui, title: &str, id: Option<ProviderId>) {
    ui.horizontal(|ui| {
        if let Some(id) = id {
            let t = tint(id);
            ui.label(RichText::new("●").color(t).size(14.0));
        }
        ui.label(RichText::new(title).strong().size(14.0));
    });
    ui.add_space(2.0);
}

/// First row of every provider: the "Enabled" checkbox plus an
/// optional secondary toggle (typically "Show $ spend").
fn enabled_row(
    ui: &mut egui::Ui,
    enabled: &mut bool,
    secondary: Option<&mut bool>,
    secondary_label: &str,
) {
    ui.horizontal(|ui| {
        ui.checkbox(enabled, "Enabled");
        if let Some(flag) = secondary {
            ui.add_space(12.0);
            ui.checkbox(flag, secondary_label);
        }
    });
}

/// A label + right-aligned input block. Keeps all forms in the tab
/// visually aligned without forcing a full grid layout.
fn field_row(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [120.0, 20.0],
            egui::Label::new(
                RichText::new(label)
                    .size(12.5)
                    .color(Color32::from_gray(180)),
            ),
        );
        body(ui);
    });
}

fn help(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(text)
            .size(11.5)
            .color(Color32::from_gray(150))
            .italics(),
    );
}

fn thresholds_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    field_row(ui, label, |ui| {
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(220.0)
                .hint_text("0.5, 0.75, 0.9"),
        );
        ui.weak("(comma-separated, 0–1)");
    });
}

/// Find the `llm-usage-setup` binary next to the dashboard's own
/// binary. Falls back to PATH lookup if it isn't a sibling — handy
/// during `cargo run` from arbitrary working dirs.
fn resolve_setup_binary() -> Option<PathBuf> {
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join(if cfg!(windows) {
                "llm-usage-setup.exe"
            } else {
                "llm-usage-setup"
            });
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    Some(PathBuf::from("llm-usage-setup"))
}

fn empty_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn format_thresholds(thresholds: &[f64]) -> String {
    thresholds
        .iter()
        .map(|t| format!("{:.2}", t))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_thresholds(s: &str) -> Vec<f64> {
    s.split(',')
        .filter_map(|p| {
            let t = p.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok()
            }
        })
        .filter(|v| (0.0..=1.0).contains(v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_thresholds() {
        let parsed = parse_thresholds("0.5, 0.75, 0.9");
        assert_eq!(parsed, vec![0.5, 0.75, 0.9]);
        assert_eq!(format_thresholds(&parsed), "0.50, 0.75, 0.90");
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(parse_thresholds("0.5, 1.5, -0.1, 0.9"), vec![0.5, 0.9]);
    }
}
