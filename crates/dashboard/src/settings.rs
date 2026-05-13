//! Editable form state for the Settings tab. The form mirrors the
//! save-to-disk fields of `Config` — per-model pricing overrides and
//! similar power-user knobs stay edit-via-TOML.

use eframe::egui::{self, Color32, RichText};
use llm_usage_core::config::{
    self, AnthropicConfig, CodexCliConfig, Config, OllamaCloudConfig,
};
use llm_usage_core::model::ProviderId;
use std::path::PathBuf;

/// Pre-baked dropdown options for the poll interval.
/// Anthropic's OAuth endpoint rate-limits aggressive polling, so we
/// start at 1 min and don't expose anything shorter. 1 hour is the
/// top end — beyond that the menu feels broken.
const POLL_OPTIONS: &[(u64, &str)] = &[
    (60, "1 minute"),
    (300, "5 minutes"),
    (900, "15 minutes"),
    (1800, "30 minutes"),
    (3600, "1 hour"),
];

/// Tray-icon rotation cadence. Goes down to 5 s for a "live carousel"
/// feel; the top end is mostly there for users who'd rather pick a
/// favourite provider manually.
const ROTATION_OPTIONS: &[(u64, &str)] = &[
    (5, "5 seconds"),
    (10, "10 seconds"),
    (15, "15 seconds"),
    (30, "30 seconds"),
    (60, "1 minute"),
    (300, "5 minutes"),
];

/// Alert threshold presets — one dropdown per provider. The "Custom"
/// case isn't enumerated here; any vec that doesn't match a preset is
/// surfaced as a `Custom (…)` entry at runtime so hand-edited TOML
/// round-trips cleanly.
const ALERT_PRESETS: &[(&str, &[f64])] = &[
    ("Off", &[]),
    ("At 75%", &[0.75]),
    ("At 90%", &[0.90]),
    ("Aggressive (50%, 75%, 90%)", &[0.50, 0.75, 0.90]),
];

pub struct ConfigDraft {
    pub poll_interval_secs: u64,
    pub icon_rotation_secs: u64,
    pub show_pace_marker: bool,
    pub check_for_updates: bool,

    pub anthropic_enabled: bool,
    pub anthropic_show_spend: bool,
    pub anthropic_weekly_budget_usd: f64,
    pub anthropic_weekly_budget_enabled: bool,
    pub anthropic_warn_at: Vec<f64>,

    pub codex_enabled: bool,
    pub codex_show_spend: bool,
    pub codex_warn_at: Vec<f64>,
    /// `Cookie:` header for chatgpt.com — when present the Codex
    /// provider hits `/backend-api/wham/usage` for live quota
    /// fractions. Captured via the "Import from browser…" button
    /// (rookie reads from Chrome / Brave / Firefox / etc).
    pub codex_chatgpt_session_cookie: String,
    pub codex_chatgpt_setup_status: Option<String>,

    pub ollama_cloud_enabled: bool,
    pub ollama_cloud_session_cookie: String,
    pub ollama_cloud_warn_at: Vec<f64>,

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
            icon_rotation_secs: c.icon_rotation_secs,
            show_pace_marker: c.show_pace_marker,
            check_for_updates: c.check_for_updates,

            anthropic_enabled: c.anthropic.enabled,
            anthropic_show_spend: c.anthropic.show_spend,
            anthropic_weekly_budget_usd: c.anthropic.weekly_budget_usd.unwrap_or(50.0),
            anthropic_weekly_budget_enabled: c.anthropic.weekly_budget_usd.is_some(),
            anthropic_warn_at: c.anthropic.warn_at.clone(),

            codex_enabled: c.codex_cli.enabled,
            codex_show_spend: c.codex_cli.show_spend,
            codex_warn_at: c.codex_cli.warn_at.clone(),
            codex_chatgpt_session_cookie: c
                .codex_cli
                .chatgpt_session_cookie
                .clone()
                .unwrap_or_default(),
            codex_chatgpt_setup_status: None,

            ollama_cloud_enabled: c.ollama_cloud.enabled,
            ollama_cloud_session_cookie: c
                .ollama_cloud
                .session_cookie
                .clone()
                .unwrap_or_default(),
            ollama_cloud_warn_at: c.ollama_cloud.warn_at.clone(),

            ollama_cloud_setup_status: None,
            ollama_cloud_setup_rx: None,
        }
    }

    /// Apply the draft fields onto a fresh Config (keeping the source's
    /// non-editable fields like `model_rates` untouched by reading them
    /// from the existing on-disk config).
    pub fn to_config(&self) -> Config {
        let mut c = Config::load_or_default().unwrap_or_default();
        c.poll_interval_secs = self.poll_interval_secs.max(60);
        c.icon_rotation_secs = self.icon_rotation_secs.max(5);
        c.show_pace_marker = self.show_pace_marker;
        c.check_for_updates = self.check_for_updates;

        c.anthropic = AnthropicConfig {
            enabled: self.anthropic_enabled,
            show_spend: self.anthropic_show_spend,
            weekly_budget_usd: if self.anthropic_weekly_budget_enabled {
                Some(self.anthropic_weekly_budget_usd)
            } else {
                None
            },
            warn_at: self.anthropic_warn_at.clone(),
            ..c.anthropic
        };

        c.codex_cli = CodexCliConfig {
            enabled: self.codex_enabled,
            show_spend: self.codex_show_spend,
            warn_at: self.codex_warn_at.clone(),
            chatgpt_session_cookie: empty_to_none(&self.codex_chatgpt_session_cookie),
            ..c.codex_cli
        };

        c.ollama_cloud = OllamaCloudConfig {
            enabled: self.ollama_cloud_enabled,
            session_cookie: empty_to_none(&self.ollama_cloud_session_cookie),
            warn_at: self.ollama_cloud_warn_at.clone(),
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

    pub fn render(&mut self, ui: &mut egui::Ui) {
        self.poll_setup_result();

        section_header(ui, "Polling & display");
        provider_card(ui, neutral_tint(), |ui| {
            field_row(ui, "Max refresh every", |ui| {
                interval_combo(
                    ui,
                    "poll_interval",
                    &mut self.poll_interval_secs,
                    POLL_OPTIONS,
                );
            });
            help(
                ui,
                "Upper bound on how often providers are polled when nothing else \
                 has changed. The tray watches Claude Code / Codex / opencode \
                 data files and refreshes within ~1\u{00A0}second of writes, so \
                 this interval mostly applies during idle periods. HTTP endpoints \
                 (Anthropic OAuth, Ollama Cloud) are throttled to one call per \
                 minute regardless to avoid rate limits.",
            );
            field_row(ui, "Tray rotates every", |ui| {
                interval_combo(
                    ui,
                    "icon_rotation",
                    &mut self.icon_rotation_secs,
                    ROTATION_OPTIONS,
                );
            });
            help(
                ui,
                "How often the tray icon swaps to the next quota-bearing provider's \
                 gauge. Shorter feels live; longer is calmer.",
            );
            ui.add_space(4.0);
            ui.checkbox(&mut self.show_pace_marker, "Show pace marker on tray icon");
            help(
                ui,
                "1 px red line marking elapsed time in each window. Off keeps the icon \
                 as just the fill bars.",
            );
            ui.add_space(4.0);
            ui.checkbox(&mut self.check_for_updates, "Check for new releases on GitHub");
            help(
                ui,
                "Once a day the tray asks GitHub whether a newer version has been released \
                 and surfaces a menu item if so. Off skips the check entirely.",
            );
        });

        ui.add_space(14.0);
        section_header(ui, "Providers");

        self.render_anthropic(ui);
        self.render_codex(ui);
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
            field_row(ui, "Alert at", |ui| {
                alert_preset_combo(ui, "anthropic_alert", &mut self.anthropic_warn_at);
            });
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

    fn render_codex(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::CodexCli), |ui| {
            section_header_row(ui, "Codex", Some(ProviderId::CodexCli));
            enabled_row(
                ui,
                &mut self.codex_enabled,
                Some(&mut self.codex_show_spend),
                "Show $ spend (estimate)",
            );
            field_row(ui, "Alert at", |ui| {
                alert_preset_combo(ui, "codex_alert", &mut self.codex_warn_at);
            });
            help(
                ui,
                "Quota (5h, 7d) comes from OpenAI's rate-limit headers, which the \
                 Codex CLI writes into your local rollouts on every turn. \
                 Optionally also hit chatgpt.com directly for live quota \u{2014} \
                 same data the Codex Cloud Settings page shows.",
            );
            ui.add_space(6.0);
            field_row(ui, "ChatGPT cookies", |ui| {
                if ui.button("Import from browser \u{2014} recommended").clicked() {
                    self.import_chatgpt_cookies_from_browser();
                }
                if !self.codex_chatgpt_session_cookie.is_empty() {
                    ui.weak(format!(
                        "\u{2713} {} chars saved",
                        self.codex_chatgpt_session_cookie.len()
                    ));
                }
            });
            if let Some(msg) = &self.codex_chatgpt_setup_status {
                help(ui, msg);
            }
        });
    }

    /// Pull chatgpt.com / openai.com cookies via `rookie` and persist
    /// them to `codex_cli.chatgpt_session_cookie`. Mirrors
    /// `import_from_browser` for Ollama Cloud — same dependency, same
    /// failure modes (no logged-in browser; libsecret keyring on
    /// Linux). The Codex provider treats the saved cookie as a live
    /// quota source override that supersedes the rollouts' lagging
    /// `rate_limits` records.
    fn import_chatgpt_cookies_from_browser(&mut self) {
        let cookies = match rookie::load(Some(vec![
            "chatgpt.com".to_string(),
            ".chatgpt.com".to_string(),
            "openai.com".to_string(),
            ".openai.com".to_string(),
        ])) {
            Ok(c) => c,
            Err(e) => {
                self.codex_chatgpt_setup_status =
                    Some(format!("Browser import failed: {}", e));
                return;
            }
        };
        let header: String = cookies
            .iter()
            .filter(|c| {
                let d = c.domain.trim_start_matches('.');
                d.ends_with("chatgpt.com") || d.ends_with("openai.com")
            })
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        if header.is_empty() {
            self.codex_chatgpt_setup_status = Some(
                "No chatgpt.com / openai.com cookies found in any installed browser. \
                 Sign in to chatgpt.com in your browser first, then click again."
                    .into(),
            );
            return;
        }
        let path = match config::config_path() {
            Ok(p) => p,
            Err(e) => {
                self.codex_chatgpt_setup_status =
                    Some(format!("Could not resolve config path: {}", e));
                return;
            }
        };
        let mut cfg = config::Config::load_or_default().unwrap_or_default();
        cfg.codex_cli.chatgpt_session_cookie = Some(header.clone());
        if let Err(e) = cfg.save(&path) {
            self.codex_chatgpt_setup_status = Some(format!("Save failed: {}", e));
            return;
        }
        self.codex_chatgpt_session_cookie = header;
        self.codex_chatgpt_setup_status = Some(format!(
            "Imported from browser. Saved to {}.",
            path.display()
        ));
    }

    fn render_ollama_cloud(&mut self, ui: &mut egui::Ui) {
        provider_card(ui, tint(ProviderId::OllamaCloud), |ui| {
            section_header_row(ui, "Ollama Cloud", Some(ProviderId::OllamaCloud));
            enabled_row(ui, &mut self.ollama_cloud_enabled, None, "");
            field_row(ui, "Alert at", |ui| {
                alert_preset_combo(ui, "ollama_cloud_alert", &mut self.ollama_cloud_warn_at);
            });

            ui.add_space(8.0);
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

            // Cookie capture (rookie) is the recommended path — it's
            // zero-click for anyone already logged in to ollama.com in
            // a desktop browser. Rendered first and emphasised.
            ui.horizontal(|ui| {
                let label = if captured {
                    "Re-import from browser"
                } else {
                    "Import from browser — recommended"
                };
                let button = egui::Button::new(
                    RichText::new(label)
                        .strong()
                        .color(Color32::WHITE)
                        .size(13.0),
                )
                .fill(Color32::from_rgb(0x4C, 0xAF, 0x50))
                .min_size(egui::vec2(240.0, 28.0));
                if ui
                    .add_enabled(!in_flight, button)
                    .on_hover_text(
                        "Reads the ollama.com cookie from any already-logged-in desktop \
                         browser (Chrome / Firefox / Edge / Brave / Safari / Vivaldi). \
                         Zero clicks beyond this button.",
                    )
                    .clicked()
                {
                    import_now = true;
                }
                if captured && !in_flight {
                    ui.colored_label(
                        Color32::from_rgb(0x4C, 0xAF, 0x50),
                        "✓ captured",
                    );
                }
            });
            ui.add_space(4.0);

            // Fallback: spawn a webview if the user isn't logged in to
            // ollama.com in any local browser, or the browser path
            // hits a keyring/permission snag.
            ui.horizontal(|ui| {
                let popup_label = if in_flight {
                    "Setup window open…"
                } else if captured {
                    "Re-run popup sign-in (backup)"
                } else {
                    "Sign in via popup window (backup)"
                };
                if ui
                    .add_enabled(!in_flight, egui::Button::new(popup_label))
                    .on_hover_text(
                        "Backup: opens an embedded browser at ollama.com/signin and \
                         captures the cookie automatically once you reach /settings.",
                    )
                    .clicked()
                {
                    launch_now = true;
                }
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

            ui.add_space(2.0);
            help(
                ui,
                "Usage is scraped from ollama.com/settings using your session cookie. \
                 Re-import after logging out or rotating.",
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

fn section_header_row(ui: &mut egui::Ui, title: &str, _id: Option<ProviderId>) {
    // No coloured dot before the title — the left-edge accent stripe
    // on the card already identifies the provider by colour.
    ui.label(RichText::new(title).strong().size(14.0));
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

/// Inline label + input. Label hugs the left edge with no fixed-width
/// allocation, so the row reads as a single sentence rather than a
/// two-column form.
fn field_row(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .size(12.5)
                .color(Color32::from_gray(180)),
        );
        body(ui);
    });
}

/// A dropdown over a fixed set of `(seconds, human label)` options.
/// Hand-edited TOML values not in the list survive as a "Custom (Ns)"
/// entry at the top so round-tripping never silently overwrites.
fn interval_combo(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut u64,
    options: &[(u64, &str)],
) {
    let in_list = options.iter().any(|(v, _)| *v == *value);
    let selected_label = options
        .iter()
        .find(|(v, _)| *v == *value)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| format!("Custom ({} s)", value));

    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_label)
        .width(160.0)
        .show_ui(ui, |ui| {
            if !in_list {
                ui.selectable_value(
                    value,
                    *value,
                    format!("Custom ({} s)", value),
                );
                ui.separator();
            }
            for (secs, label) in options {
                ui.selectable_value(value, *secs, *label);
            }
        });
}

/// Unified dropdown for alert thresholds. Maps the current Vec<f64> to
/// the nearest preset; non-matching values appear as `Custom (...)` so
/// hand-edited TOML round-trips without loss.
fn alert_preset_combo(ui: &mut egui::Ui, id: &str, value: &mut Vec<f64>) {
    // Precompute everything we need to know about each preset before
    // entering the closure — egui's ComboBox::show_ui takes a `FnOnce`
    // and we can't borrow `value` again inside it for the match check.
    let preset_states: Vec<(&'static str, &'static [f64], bool)> = ALERT_PRESETS
        .iter()
        .map(|(label, preset)| {
            let is_match = preset.len() == value.len()
                && preset
                    .iter()
                    .zip(value.iter())
                    .all(|(a, b)| (a - b).abs() < 1e-6);
            (*label, *preset, is_match)
        })
        .collect();
    let matched_label = preset_states
        .iter()
        .find(|(_, _, m)| *m)
        .map(|(l, _, _)| (*l).to_string());
    let custom_label = format!("Custom ({})", format_thresholds(value));
    let selected_label = matched_label.clone().unwrap_or_else(|| custom_label.clone());

    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_label)
        .width(240.0)
        .show_ui(ui, |ui| {
            if matched_label.is_none() {
                // Render an inert custom row so the user can see what
                // they currently have (and selecting it leaves the
                // values untouched).
                let _ = ui.selectable_label(true, &custom_label);
                ui.separator();
            }
            for (label, preset, is_selected) in &preset_states {
                if ui.selectable_label(*is_selected, *label).clicked() {
                    *value = preset.to_vec();
                }
            }
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
    if thresholds.is_empty() {
        return "off".to_string();
    }
    thresholds
        .iter()
        .map(|t| format!("{:.0}%", t * 100.0))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_thresholds_basic() {
        assert_eq!(format_thresholds(&[]), "off");
        assert_eq!(format_thresholds(&[0.75, 0.9]), "75%, 90%");
    }

    #[test]
    fn alert_presets_have_expected_shape() {
        // Sanity check: "Off" is empty, others have descending utility.
        assert!(ALERT_PRESETS[0].1.is_empty(), "first preset must be Off");
        for (_, preset) in ALERT_PRESETS.iter().skip(1) {
            for w in preset.windows(2) {
                assert!(w[0] < w[1], "preset thresholds should be ascending");
            }
        }
    }

    #[test]
    fn format_thresholds_displays_off_for_empty() {
        assert_eq!(format_thresholds(&[]), "off");
    }

    #[test]
    fn empty_to_none_trims_whitespace() {
        assert!(empty_to_none("   ").is_none());
        assert!(empty_to_none("").is_none());
        assert_eq!(empty_to_none("  foo  "), Some("foo".to_string()));
    }

    #[test]
    fn poll_options_are_ascending_seconds() {
        let secs: Vec<u64> = POLL_OPTIONS.iter().map(|(s, _)| *s).collect();
        let mut sorted = secs.clone();
        sorted.sort();
        assert_eq!(secs, sorted, "POLL_OPTIONS must be sorted by seconds");
    }

    #[test]
    fn rotation_options_start_at_five_seconds() {
        assert_eq!(ROTATION_OPTIONS.first().unwrap().0, 5);
    }

    #[test]
    fn config_draft_round_trips_through_to_config() {
        let mut cfg = Config::default();
        cfg.icon_rotation_secs = 30;
        cfg.show_pace_marker = false;
        cfg.check_for_updates = false;
        cfg.anthropic.show_spend = true;
        cfg.anthropic.warn_at = vec![0.5, 0.9];
        cfg.codex_cli.warn_at = vec![0.75, 0.9];
        cfg.ollama_cloud.enabled = true;
        cfg.ollama_cloud.session_cookie = Some("session=abc".into());
        cfg.ollama_cloud.warn_at = vec![0.9];

        let draft = ConfigDraft::from_config(&cfg);
        let round = draft.to_config();
        assert_eq!(round.icon_rotation_secs, 30);
        assert!(!round.show_pace_marker);
        assert!(!round.check_for_updates);
        assert!(round.anthropic.show_spend);
        assert_eq!(round.anthropic.warn_at, vec![0.5, 0.9]);
        assert_eq!(round.codex_cli.warn_at, vec![0.75, 0.9]);
        assert!(round.ollama_cloud.enabled);
        assert_eq!(round.ollama_cloud.session_cookie.as_deref(), Some("session=abc"));
        assert_eq!(round.ollama_cloud.warn_at, vec![0.9]);
    }

    #[test]
    fn config_draft_to_config_clamps_minimums() {
        let mut cfg = Config::default();
        cfg.poll_interval_secs = 30; // below 60 floor
        cfg.icon_rotation_secs = 1;  // below 5 floor
        let draft = ConfigDraft::from_config(&cfg);
        let round = draft.to_config();
        assert_eq!(round.poll_interval_secs, 60);
        assert_eq!(round.icon_rotation_secs, 5);
    }
}
