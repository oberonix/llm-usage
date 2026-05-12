//! Dashboard window — invoked on demand from the tray's "Open dashboard…" menu.
//! Tabs: Status (live snapshots + history) and Settings (config form).
//!
//! Runs as a separate binary so the always-resident tray doesn't carry the
//! egui/eframe dependency footprint.

mod history;
mod settings;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use llm_usage_core::config::Config;
use llm_usage_core::model::{ProviderId, ProviderStatus, UsageSnapshot};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::settings::ConfigDraft;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Status,
    Settings,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load_or_default()?;
    let initial_tab = parse_initial_tab();
    let popup_mode = std::env::args().skip(1).any(|a| a == "--popup");

    // Singleton: a second instance of the same window kind forwards a
    // focus request to the running one and exits. PID files / focus
    // triggers live next to snapshots.json.
    let mode_name = if popup_mode { "popup" } else { "dashboard" };
    let pid_path = match try_acquire_singleton(mode_name) {
        SingletonOutcome::Forwarded => return Ok(()),
        SingletonOutcome::Acquired(p) => p,
    };
    let _pid_guard = PidGuard {
        path: pid_path.clone(),
    };

    let viewport = if popup_mode {
        egui::ViewportBuilder::default()
            .with_inner_size([400.0, 500.0])
            .with_title("LLM Usage")
            .with_decorations(false)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false)
    } else {
        egui::ViewportBuilder::default()
            .with_inner_size([800.0, 620.0])
            .with_title("LLM Usage")
    };

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "llm-usage-dashboard",
        options,
        Box::new(move |cc| {
            // The egui Context is owned by eframe — we hold a clone in
            // the watcher so it can `request_repaint` from a background
            // thread when the snapshot file changes.
            Ok(Box::new(DashboardApp::new(
                config,
                cc.egui_ctx.clone(),
                initial_tab,
                popup_mode,
                mode_name,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {}", e))?;
    Ok(())
}

enum SingletonOutcome {
    /// We claimed the slot; caller owns the returned PID path.
    Acquired(std::path::PathBuf),
    /// A live instance already exists; the focus trigger was written
    /// and the caller should exit.
    Forwarded,
}

/// If a PID file for `name` exists and the recorded process is alive
/// AND is one of our own dashboard binaries, touch the focus-trigger
/// so the running instance brings itself to the front, and return
/// `Forwarded`. Otherwise claim the slot.
fn try_acquire_singleton(name: &str) -> SingletonOutcome {
    let pid_path = match llm_usage_core::config::singleton_pid_path(name) {
        Ok(p) => p,
        Err(_) => return SingletonOutcome::Acquired(std::path::PathBuf::new()),
    };
    let focus_path = llm_usage_core::config::singleton_focus_trigger_path(name)
        .unwrap_or_default();
    try_acquire_singleton_at(&pid_path, &focus_path, std::process::id(), is_our_process)
}

/// Pure version: same logic but with explicit paths and a process
/// liveness probe. Public so tests can drive it with tempdir paths and
/// a fake liveness function; the production wrapper resolves the
/// real paths via `ProjectDirs` and uses `is_our_process`.
fn try_acquire_singleton_at(
    pid_path: &std::path::Path,
    focus_path: &std::path::Path,
    my_pid: u32,
    is_alive: fn(u32) -> bool,
) -> SingletonOutcome {
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    if let Ok(s) = std::fs::read_to_string(pid_path) {
        if let Ok(pid) = s.trim().parse::<u32>() {
            if is_alive(pid) {
                let _ = std::fs::write(focus_path, chrono::Utc::now().to_rfc3339());
                return SingletonOutcome::Forwarded;
            }
        }
    }
    let _ = std::fs::write(pid_path, my_pid.to_string());
    SingletonOutcome::Acquired(pid_path.to_path_buf())
}

/// Return true only when `pid` names a live process whose command line
/// is one of our dashboard binaries. The cmdline check defends against
/// PID reuse — when a previous dashboard dies without cleanup, the
/// kernel can later hand its PID to an unrelated process and a naive
/// "is the PID alive?" check would incorrectly forward to it.
fn is_our_process(pid: u32) -> bool {
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !alive {
        return false;
    }
    // Linux: /proc/<pid>/cmdline holds the full argv joined by NULs.
    if let Ok(bytes) = std::fs::read(format!("/proc/{}/cmdline", pid)) {
        let s = String::from_utf8_lossy(&bytes);
        return s.contains("llm-usage-dashboard");
    }
    // macOS / fallback: ask `ps` for the binary name. `comm` is
    // truncated to ~16 chars on Linux but full on macOS, and "ps -p"
    // exits non-zero if the PID doesn't exist.
    let comm = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    comm.contains("llm-usage-dashb")
}

/// Removes the PID file on drop so the next instance can claim the
/// slot cleanly. Panics inside eframe still trigger Drop via
/// stack-unwinding, so this is best-effort but reliable in practice.
struct PidGuard {
    path: std::path::PathBuf,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Look for `--tab=status` / `--tab=settings` so the tray can launch
/// us directly on the Settings tab from its "Settings…" menu item.
fn parse_initial_tab() -> Tab {
    for arg in std::env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--tab=") {
            match value {
                "settings" => return Tab::Settings,
                "status" => return Tab::Status,
                _ => {}
            }
        }
    }
    Tab::Status
}

struct DashboardApp {
    config: Config,
    snapshots: Arc<Mutex<BTreeMap<ProviderId, UsageSnapshot>>>,
    last_updated: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    daily_history: Arc<Mutex<Vec<(chrono::NaiveDate, f64)>>>,
    /// Marker set when we've asked the tray to poll; cleared once the
    /// shared snapshot file's `updated_at` moves past the moment we
    /// touched the trigger. Drives the "polling…" spinner.
    refresh_pending: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    tab: Tab,
    draft: ConfigDraft,
    /// Serialized TOML of the last config we wrote to disk. Used to
    /// detect form changes so we can auto-save without firing on every
    /// frame.
    last_saved_toml: Option<String>,
    /// Held for the app's lifetime so notify keeps delivering events.
    _snapshots_watcher: Option<RecommendedWatcher>,
    /// Watches `<mode>.focus` so the running singleton brings itself
    /// to the foreground when a second invocation pings it.
    _focus_watcher: Option<RecommendedWatcher>,
    /// `true` when launched with `--popup`. Renders a compact,
    /// decorationless variant of the Status tab.
    popup_mode: bool,
}

impl DashboardApp {
    fn new(
        config: Config,
        ctx: egui::Context,
        initial_tab: Tab,
        popup_mode: bool,
        mode_name: &str,
    ) -> Self {
        let snapshots = Arc::new(Mutex::new(BTreeMap::new()));
        let last_updated = Arc::new(Mutex::new(None));
        let daily_history = Arc::new(Mutex::new(Vec::new()));
        let refresh_pending = Arc::new(Mutex::new(None));
        let draft = ConfigDraft::from_config(&config);
        let last_saved_toml = toml::to_string_pretty(&config).ok();

        // Initial load from the tray's shared file. If the tray hasn't
        // written one yet (fresh install), the map stays empty and the
        // UI shows "loading…" placeholders until the first poll lands.
        reload_snapshots(&snapshots, &last_updated, &refresh_pending);

        // Watch the file for changes so background polls show up
        // without the user having to click anything.
        let watcher = spawn_snapshots_watcher(
            snapshots.clone(),
            last_updated.clone(),
            refresh_pending.clone(),
            ctx.clone(),
        );

        let focus_watcher = spawn_focus_watcher(mode_name, ctx);

        if config.anthropic.enabled && config.anthropic.show_spend {
            kick_daily_history(&config.anthropic, daily_history.clone());
        }

        Self {
            config,
            snapshots,
            last_updated,
            daily_history,
            refresh_pending,
            tab: initial_tab,
            draft,
            last_saved_toml,
            _snapshots_watcher: watcher,
            _focus_watcher: focus_watcher,
            popup_mode,
        }
    }
}

/// Watches the focus-trigger file for our singleton slot. When a
/// second invocation writes to it, we send a Focus viewport command
/// so the existing window comes to the foreground.
fn spawn_focus_watcher(mode_name: &str, ctx: egui::Context) -> Option<RecommendedWatcher> {
    let trigger_path = llm_usage_core::config::singleton_focus_trigger_path(mode_name).ok()?;
    let parent = trigger_path.parent()?.to_path_buf();
    std::fs::create_dir_all(&parent).ok();

    let last_fire = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60)));
    let target = trigger_path.clone();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };
        if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
            return;
        }
        if !event.paths.iter().any(|p| p == &target) {
            return;
        }
        let now = Instant::now();
        let mut guard = last_fire.lock().expect("poisoned");
        if now.duration_since(*guard) < Duration::from_millis(150) {
            return;
        }
        *guard = now;
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }) {
        Ok(w) => w,
        Err(_) => return None,
    };
    if watcher.watch(&parent, RecursiveMode::NonRecursive).is_err() {
        return None;
    }
    Some(watcher)
}

impl eframe::App for DashboardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_secs(2));

        if self.popup_mode {
            // Esc dismisses the popup; otherwise it lingers because we
            // disabled window decorations.
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.render_popup(ctx);
            return;
        }

        // Tab strip is now the top of the window — the app name is
        // already in the OS title bar, so a duplicate heading inside
        // the window just steals vertical space. No separator line
        // beneath it — the active-tab underline alone marks the divide.
        egui::TopBottomPanel::top("tab_strip")
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin {
                        left: 20.0,
                        right: 20.0,
                        top: 10.0,
                        bottom: 0.0,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    self.tab_button(ui, Tab::Status, "Status");
                    ui.add_space(8.0);
                    self.tab_button(ui, Tab::Settings, "Settings");
                });
                // No separator line — the underline under the active
                // tab button is the divider.
            });

        // Per-tab subheader (Refresh + last-updated indicator) keeps the
        // tab strip uncluttered. Only the Status tab needs it for now.
        if self.tab == Tab::Status {
            egui::TopBottomPanel::top("status_subheader")
                .show_separator_line(false)
                .frame(
                    egui::Frame::none()
                        .fill(ctx.style().visuals.window_fill)
                        .inner_margin(egui::Margin::symmetric(20.0, 10.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Refresh").clicked() {
                            self.request_refresh();
                        }
                        if self.refresh_pending.lock().unwrap().is_some() {
                            ui.spinner();
                            ui.label("polling…");
                        } else if let Some(at) = *self.last_updated.lock().unwrap() {
                            let age = chrono::Utc::now() - at;
                            ui.weak(format!("updated {}", fmt_age(age)));
                        }
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ctx, |ui| match self.tab {
                Tab::Status => self.render_status(ui),
                Tab::Settings => self.render_settings(ui),
            });
    }
}

impl DashboardApp {
    /// Compact "tray popup" layout: thin draggable header with title +
    /// close button, then the same Status cards as the full dashboard.
    /// No tab strip, no settings, no Refresh button (the tray polls).
    fn render_popup(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("popup_header")
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let title = ui.label(
                        RichText::new("LLM Usage").strong().size(14.0),
                    );
                    // Dragging the title bar moves the window — egui's
                    // standard pattern when running without decorations.
                    if title.interact(egui::Sense::drag()).dragged() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let close = ui.add(
                                egui::Button::new(
                                    RichText::new("✕")
                                        .size(13.0)
                                        .color(Color32::from_gray(180)),
                                )
                                .frame(false),
                            );
                            if close.clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        },
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        egui::Frame::none()
                            .inner_margin(egui::Margin {
                                left: 12.0,
                                right: 12.0,
                                top: 6.0,
                                bottom: 6.0,
                            })
                            .show(ui, |ui| {
                                self.render_status_body(ui);
                            });
                    });
            });
    }

    /// One tab button. Uses a larger font and paints a 3 px coloured
    /// underline beneath the active label so the active tab is obvious
    /// without needing the OS theme to draw a selection state.
    fn tab_button(&mut self, ui: &mut egui::Ui, tab: Tab, label: &str) {
        let active = self.tab == tab;
        let text = RichText::new(label)
            .size(15.0)
            .strong()
            .color(if active {
                ui.visuals().strong_text_color()
            } else {
                Color32::from_gray(160)
            });
        let resp = ui.add(
            egui::Button::new(text)
                .frame(false)
                .min_size(egui::vec2(80.0, 28.0)),
        );
        if resp.clicked() {
            self.tab = tab;
        }
        if active {
            let rect = resp.rect;
            let underline_color = Color32::from_rgb(0x4C, 0xAF, 0x50);
            let y = rect.bottom() + 2.0;
            let stroke = egui::Stroke::new(3.0, underline_color);
            ui.painter().line_segment(
                [egui::pos2(rect.left() + 8.0, y), egui::pos2(rect.right() - 8.0, y)],
                stroke,
            );
        }
    }
}

impl DashboardApp {
    fn render_status(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                // Horizontal padding lives inside the scroll area so the
                // scroll bar sits at the panel edge, not 20 px in.
                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        left: 20.0,
                        right: 20.0,
                        top: 12.0,
                        bottom: 0.0,
                    })
                    .show(ui, |ui| {
                        self.render_status_body(ui);
                    });
            });
    }

    fn render_status_body(&self, ui: &mut egui::Ui) {
        let snaps = self.snapshots.lock().unwrap().clone();
        let provider_iter = [
            (ProviderId::Anthropic, self.config.anthropic.enabled),
            (ProviderId::CodexCli, self.config.codex_cli.enabled),
            (ProviderId::OllamaCloud, self.config.ollama_cloud.enabled),
        ];
        let mut shown = 0usize;
        for (id, enabled) in provider_iter {
            // Only render providers the user has enabled in config.
            // Disabled providers vanish entirely.
            if !enabled {
                continue;
            }
            shown += 1;
            if let Some(snap) = snaps.get(&id) {
                render_provider_card(ui, snap);
                if id == ProviderId::Anthropic && self.config.anthropic.show_spend {
                    let history = self.daily_history.lock().unwrap().clone();
                    if !history.is_empty() {
                        render_daily_history_card(ui, &history);
                    }
                }
            } else {
                render_loading_card(ui, id);
            }
            ui.add_space(10.0);
        }
        if shown == 0 {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("No providers enabled.")
                        .size(15.0)
                        .color(egui::Color32::from_gray(180)),
                );
                ui.add_space(4.0);
                ui.weak(
                    "Switch to the Settings tab to enable Anthropic, \
                     Codex, or Ollama Cloud.",
                );
            });
        }
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        left: 20.0,
                        right: 20.0,
                        top: 12.0,
                        bottom: 0.0,
                    })
                    .show(ui, |ui| {
                        self.render_settings_body(ui);
                    });
            });
    }

    fn render_settings_body(&mut self, ui: &mut egui::Ui) {
        self.draft.render(ui);
        ui.add_space(16.0);
        // `add_sized` centers the button's contents inside the fixed
        // bounds; using `min_size` left-aligns instead.
        let reset = ui.add_sized(
            [140.0, 26.0],
            egui::Button::new(RichText::new("Reset to defaults").size(13.0)),
        );
        if reset.clicked() {
            self.draft = ConfigDraft::from_config(&Config::default());
        }
        ui.add_space(24.0);

        // Auto-save: any form change re-serialises the resulting Config
        // and writes it to disk if the bytes differ from the last write.
        // The tray's config watcher picks the new file up and reloads.
        self.auto_save_if_changed();
    }

    /// Called every frame from the Settings tab. Cheap when nothing has
    /// changed (one serialisation + string compare); writes to disk
    /// only on actual edits.
    fn auto_save_if_changed(&mut self) {
        let new_cfg = self.draft.to_config();
        let Ok(new_toml) = toml::to_string_pretty(&new_cfg) else {
            return;
        };
        if Some(&new_toml) == self.last_saved_toml.as_ref() {
            return;
        }
        let Ok(path) = llm_usage_core::config::config_path() else {
            return;
        };
        if let Err(e) = new_cfg.save(&path) {
            tracing::warn!(error = %e, "auto-save failed");
            return;
        }
        self.last_saved_toml = Some(new_toml);
        self.config = new_cfg;
    }
}

fn render_provider_card(ui: &mut egui::Ui, snap: &UsageSnapshot) {
    let tint_rgb = snap.provider.tint_rgb();
    let tint = Color32::from_rgb(tint_rgb.0, tint_rgb.1, tint_rgb.2);
    card_frame(ui, tint, |ui| {
        header_row(ui, snap.provider, snap.plan_label.as_deref(), snap.status, tint);
        // Headline removed: the window grid below already shows the
        // same percentages and reset times; the headline duplicated
        // them and felt like a "subtitle line" the user didn't want.
        if let Some(err) = &snap.error {
            ui.add_space(2.0);
            ui.colored_label(Color32::from_rgb(0xE5, 0x39, 0x35), err);
        }
        if !snap.windows.is_empty() {
            ui.add_space(8.0);
            // Quota-bearing windows first (5h / week / week (Sonnet) /
            // week (Opus) / session), then activity-only windows
            // (1h / today / month) at the bottom. Sort key encodes both
            // grouping and intra-group order.
            let mut entries: Vec<(&String, &llm_usage_core::model::WindowUsage)> =
                snap.windows.iter().collect();
            entries.sort_by_key(|(label, _)| (window_order(label.as_str()), label.as_str()));
            render_windows_table(ui, snap.provider, &entries);
        }
    });
}

/// Two-column grid: left column holds the window label, monospaced
/// and left-aligned so labels stack neatly even when their widths
/// vary (e.g. `5h` vs `week (Sonnet)`). The right column carries the
/// progress bar, token totals, reset countdown, and any spend amount
/// for that row.
fn render_windows_table(
    ui: &mut egui::Ui,
    provider: ProviderId,
    entries: &[(&String, &llm_usage_core::model::WindowUsage)],
) {
    egui::Grid::new(format!("windows-{:?}", provider))
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            for (label, w) in entries {
                ui.label(
                    RichText::new(*label)
                        .monospace()
                        .size(12.5)
                        .color(Color32::from_gray(210)),
                );
                ui.horizontal(|ui| {
                    render_window_usage(ui, w);
                });
                ui.end_row();
            }
        });
}

fn render_window_usage(ui: &mut egui::Ui, w: &llm_usage_core::model::WindowUsage) {
    match w.fraction_used {
        Some(frac) => {
            let bar = egui::ProgressBar::new(frac.min(1.0) as f32)
                .desired_width(220.0)
                .fill(fraction_color(frac))
                .text(
                    RichText::new(format!("{:.0}%", frac * 100.0))
                        .size(11.0)
                        .strong(),
                );
            ui.add(bar);
            if w.stale {
                // Fraction came from a cached snapshot rather than the
                // current poll. Surface the warning in place of the
                // reset countdown — the countdown would be misleading
                // since the underlying data is the previous window's.
                ui.label(
                    RichText::new(format!(
                        "{} stale",
                        llm_usage_core::model::STALE_MARKER
                    ))
                    .color(Color32::from_rgb(240, 180, 60))
                    .strong(),
                );
            } else if let Some(ends) = w.ends_at {
                let secs = (ends - chrono::Utc::now()).num_seconds();
                if secs > 0 {
                    ui.weak(reset_label(secs));
                }
            }
            if let Some(spend) = w.spend_usd {
                ui.label(
                    RichText::new(format!("${:.2}", spend))
                        .color(Color32::from_gray(220))
                        .strong(),
                );
            }
            if let Some(limit) = w.limit_usd {
                ui.weak(format!("of ${:.0}", limit));
            }
        }
        None => {
            // Explicit `in` / `out` / `reqs` labels with `|` separators —
            // arrow glyphs read as "tokens up/down" if you know the
            // convention but were hard to scan otherwise.
            let mut parts: Vec<String> = Vec::new();
            if w.tokens_in > 0 {
                parts.push(format!("{} in", fmt_tokens(w.tokens_in)));
            }
            if w.tokens_out > 0 {
                parts.push(format!("{} out", fmt_tokens(w.tokens_out)));
            }
            if w.request_count > 0 {
                parts.push(format!("{} reqs", w.request_count));
            }
            if let Some(spend) = w.spend_usd {
                parts.push(format!("${:.2}", spend));
            }
            if parts.is_empty() {
                ui.weak("no activity");
            } else {
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        ui.weak(
                            RichText::new("|").color(Color32::from_gray(90)),
                        );
                    }
                    ui.weak(p);
                }
            }
        }
    }
}

fn reset_label(secs: i64) -> String {
    if secs < 3600 {
        format!("resets {}m", secs / 60)
    } else if secs < 86_400 {
        format!("resets {}h", secs / 3600)
    } else {
        format!("resets {}d", secs / 86_400)
    }
}

fn window_order(label: &str) -> u32 {
    match label {
        // Quota windows — short rolling first, then weekly.
        "5h" => 10,
        "week" => 20,
        "week (Sonnet)" => 21,
        "week (Opus)" => 22,
        // Activity windows
        "1h" => 100,
        "today" => 101,
        "month" => 102,
        // Anything new / unknown lands between quota and activity.
        _ => 50,
    }
}

fn render_loading_card(ui: &mut egui::Ui, id: ProviderId) {
    let tint_rgb = id.tint_rgb();
    let tint = Color32::from_rgb(tint_rgb.0, tint_rgb.1, tint_rgb.2);
    card_frame(ui, tint, |ui| {
        ui.label(
            RichText::new(format!("{} — waiting for first poll…", id.human()))
                .strong()
                .size(15.0)
                .color(Color32::from_gray(180)),
        );
    });
}

/// Draw a card with a 4 px provider-coloured accent on the left edge.
/// Body is rendered inside `body` with consistent padding. Shared with
/// the Settings tab so cards line up between Status and Settings.
pub fn card_frame(ui: &mut egui::Ui, tint: Color32, body: impl FnOnce(&mut egui::Ui)) {
    let body_fill = ui
        .visuals()
        .widgets
        .noninteractive
        .bg_fill
        .gamma_multiply(1.05);
    let stroke = egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let outer = egui::Frame::default()
        .fill(body_fill)
        .stroke(stroke)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::ZERO);

    outer.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            // Left accent stripe.
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(4.0, ui.available_height().max(64.0)),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(rect, 0.0, tint);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(8.0);
                ui.scope(|ui| {
                    ui.set_max_width(ui.available_width() - 12.0);
                    body(ui);
                });
                ui.add_space(8.0);
            });
        });
    });
}

fn header_row(
    ui: &mut egui::Ui,
    provider: ProviderId,
    plan_label: Option<&str>,
    _status: ProviderStatus,
    _tint: Color32,
) {
    // Provider name and plan tag share one bold label so the dash sits
    // mid-line in the same style. The left-edge accent stripe on the
    // card already identifies the provider by colour. No status chip —
    // the error line below (if any) is the only indicator of a problem.
    let title = match plan_label {
        Some(plan) => format!("{} · {}", provider.human(), plan),
        None => provider.human().to_string(),
    };
    ui.label(RichText::new(title).strong().size(15.5));
}

fn render_daily_history_card(ui: &mut egui::Ui, history: &[(chrono::NaiveDate, f64)]) {
    let max = history
        .iter()
        .map(|(_, v)| *v)
        .fold(0f64, f64::max)
        .max(0.01);
    let tint_rgb = ProviderId::Anthropic.tint_rgb();
    let tint = Color32::from_rgb(tint_rgb.0, tint_rgb.1, tint_rgb.2);
    ui.add_space(6.0);
    card_frame(ui, tint, |ui| {
        ui.label(
            RichText::new("Anthropic — daily spend (last 14 days)")
                .strong()
                .size(13.0),
        );
        ui.add_space(6.0);
        for (date, spend) in history {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [70.0, 18.0],
                    egui::Label::new(
                        RichText::new(date.format("%a %m-%d").to_string())
                            .monospace()
                            .size(12.0)
                            .color(Color32::from_gray(180)),
                    ),
                );
                let bar = egui::ProgressBar::new((spend / max) as f32)
                    .desired_width(380.0)
                    .fill(tint)
                    .text(RichText::new(format!("${:.2}", spend)).size(11.0));
                ui.add(bar);
            });
        }
    });
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fraction_color(frac: f64) -> Color32 {
    if frac < 0.60 {
        Color32::from_rgb(0x4C, 0xAF, 0x50)
    } else if frac < 0.85 {
        Color32::from_rgb(0xFF, 0xB3, 0x00)
    } else {
        Color32::from_rgb(0xE5, 0x39, 0x35)
    }
}

impl DashboardApp {
    /// Ask the tray to poll right now and refresh the shared file.
    /// The tray's `refresh.trigger` watcher picks this up; the file
    /// watcher we spawned will re-load the result automatically when
    /// the tray rewrites snapshots.json.
    fn request_refresh(&mut self) {
        let now = chrono::Utc::now();
        if let Err(e) = llm_usage_core::touch_refresh_trigger() {
            tracing::warn!(error = %e, "could not touch refresh trigger");
            return;
        }
        *self.refresh_pending.lock().unwrap() = Some(now);
        if self.config.anthropic.enabled && self.config.anthropic.show_spend {
            kick_daily_history(&self.config.anthropic, self.daily_history.clone());
        }
    }
}

/// Re-read the shared snapshot file into the dashboard's maps.
/// Resets the "polling…" pending marker if the file we just read is
/// newer than the moment the user clicked Refresh.
fn reload_snapshots(
    snapshots: &Arc<Mutex<BTreeMap<ProviderId, UsageSnapshot>>>,
    last_updated: &Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    refresh_pending: &Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
) {
    let Ok(Some(file)) = llm_usage_core::read_snapshots() else {
        return;
    };
    {
        let mut guard = snapshots.lock().unwrap();
        *guard = file.snapshots;
    }
    let updated_at = file.updated_at;
    *last_updated.lock().unwrap() = Some(updated_at);
    let mut pending = refresh_pending.lock().unwrap();
    if let Some(asked_at) = *pending {
        if updated_at >= asked_at {
            *pending = None;
        }
    }
}

fn spawn_snapshots_watcher(
    snapshots: Arc<Mutex<BTreeMap<ProviderId, UsageSnapshot>>>,
    last_updated: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    refresh_pending: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    ctx: egui::Context,
) -> Option<RecommendedWatcher> {
    let target = llm_usage_core::config::snapshots_path().ok()?;
    let parent = target.parent()?.to_path_buf();
    std::fs::create_dir_all(&parent).ok();

    let last_fire = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60)));
    let path_match = target.clone();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "snapshots watcher error");
                return;
            }
        };
        let interesting = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_)
        );
        if !interesting {
            return;
        }
        if !event.paths.iter().any(|p| p == &path_match) {
            return;
        }
        let now = Instant::now();
        let mut guard = last_fire.lock().expect("poisoned");
        if now.duration_since(*guard) < Duration::from_millis(150) {
            return;
        }
        *guard = now;
        reload_snapshots(&snapshots, &last_updated, &refresh_pending);
        ctx.request_repaint();
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "could not start snapshots watcher");
            return None;
        }
    };
    if let Err(e) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
        tracing::warn!(error = %e, path = %parent.display(), "snapshots watcher subscribe failed");
        return None;
    }
    tracing::info!(path = %target.display(), "snapshots watcher live");
    Some(watcher)
}

/// Kick a one-shot JSONL walk to refresh the 14-day Anthropic spend
/// chart. The chart is dashboard-local (the tray doesn't need it).
fn kick_daily_history(
    cfg: &llm_usage_core::config::AnthropicConfig,
    history: Arc<Mutex<Vec<(chrono::NaiveDate, f64)>>>,
) {
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        let computed = history::anthropic_daily_spend(&cfg, 14);
        *history.lock().unwrap() = computed;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_order_groups_quota_first_then_activity() {
        let mut labels = vec![
            "month",
            "today",
            "1h",
            "week (Sonnet)",
            "5h",
            "week",
            "week (Opus)",
            "unknown",
        ];
        labels.sort_by_key(|l| window_order(l));
        assert_eq!(
            labels,
            vec![
                "5h",
                "week",
                "week (Sonnet)",
                "week (Opus)",
                "unknown",   // bucketed between quota & activity (50)
                "1h",
                "today",
                "month",
            ]
        );
    }

    #[test]
    fn fmt_tokens_scales_with_size() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(15_000), "15.0k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn fraction_color_thresholds() {
        // Green below 60% utilization, amber 60-85%, red above.
        let g = fraction_color(0.10);
        let a = fraction_color(0.70);
        let r = fraction_color(0.95);
        assert_ne!(g, a);
        assert_ne!(a, r);
        assert_ne!(g, r);
        // Threshold edge cases.
        assert_eq!(fraction_color(0.0), g);
        assert_eq!(fraction_color(0.59), g);
        assert_eq!(fraction_color(0.60), a);
        assert_eq!(fraction_color(0.84), a);
        assert_eq!(fraction_color(0.85), r);
        assert_eq!(fraction_color(1.0), r);
    }

    #[test]
    fn fmt_age_scales_with_seconds_elapsed() {
        assert_eq!(fmt_age(chrono::Duration::seconds(15)), "15s ago");
        assert_eq!(fmt_age(chrono::Duration::seconds(120)), "2m ago");
        assert_eq!(fmt_age(chrono::Duration::seconds(3 * 3600)), "3h ago");
        assert_eq!(fmt_age(chrono::Duration::seconds(2 * 86_400)), "2d ago");
        // Negative durations (clock skew) clamp to 0.
        assert_eq!(fmt_age(chrono::Duration::seconds(-30)), "0s ago");
    }

    // ---- Singleton acquire / PID-reuse tests ----
    //
    // The PID-reuse bug we fixed earlier is the kind of thing that
    // gets quietly reintroduced by a refactor and never noticed in
    // testing (it only manifests when an old PID happens to collide
    // with an unrelated live process). These tests pin down the
    // behaviour of the pure helper with a fake `is_alive` so the
    // outcomes are deterministic.

    use tempfile::TempDir;

    fn always_alive(_pid: u32) -> bool {
        true
    }
    fn always_dead(_pid: u32) -> bool {
        false
    }

    #[test]
    fn singleton_acquires_when_no_pid_file_exists() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("dashboard.pid");
        let focus_path = dir.path().join("dashboard.focus");
        let outcome = try_acquire_singleton_at(&pid_path, &focus_path, 12345, always_alive);
        match outcome {
            SingletonOutcome::Acquired(p) => assert_eq!(p, pid_path),
            SingletonOutcome::Forwarded => panic!("expected Acquired"),
        }
        // PID file now contains our PID.
        let s = std::fs::read_to_string(&pid_path).unwrap();
        assert_eq!(s.trim(), "12345");
        // Focus trigger was NOT touched on first acquire.
        assert!(!focus_path.exists());
    }

    #[test]
    fn singleton_forwards_when_recorded_pid_is_alive() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("dashboard.pid");
        let focus_path = dir.path().join("dashboard.focus");
        // Pretend an earlier instance recorded its PID.
        std::fs::write(&pid_path, "999").unwrap();
        let outcome = try_acquire_singleton_at(&pid_path, &focus_path, 12345, always_alive);
        assert!(matches!(outcome, SingletonOutcome::Forwarded));
        // PID file unchanged (still the previous instance's PID).
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap().trim(), "999");
        // Focus trigger was written to ping the running instance.
        assert!(focus_path.exists());
    }

    #[test]
    fn singleton_claims_slot_when_recorded_pid_is_dead() {
        // The PID-reuse defence: an old PID file from a crashed
        // instance must not block us. With `always_dead` standing in
        // for "the PID is no longer ours / no longer alive", we take
        // the slot.
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("dashboard.pid");
        let focus_path = dir.path().join("dashboard.focus");
        std::fs::write(&pid_path, "999").unwrap();
        let outcome = try_acquire_singleton_at(&pid_path, &focus_path, 22222, always_dead);
        assert!(matches!(outcome, SingletonOutcome::Acquired(_)));
        // PID file now belongs to us.
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap().trim(), "22222");
        // No focus ping (we didn't hand off to anyone).
        assert!(!focus_path.exists());
    }

    #[test]
    fn singleton_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        // Parent directory doesn't exist yet — the helper must create
        // it before the write.
        let pid_path = dir.path().join("nested").join("subdir").join("dashboard.pid");
        let focus_path = dir.path().join("nested").join("subdir").join("dashboard.focus");
        let outcome = try_acquire_singleton_at(&pid_path, &focus_path, 1, always_dead);
        assert!(matches!(outcome, SingletonOutcome::Acquired(_)));
        assert!(pid_path.exists());
    }

    #[test]
    fn singleton_tolerates_garbage_in_pid_file() {
        // A truncated / non-numeric PID file from a botched write
        // must not be treated as "another instance is alive". We
        // claim the slot and overwrite.
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("dashboard.pid");
        let focus_path = dir.path().join("dashboard.focus");
        std::fs::write(&pid_path, "not a pid").unwrap();
        let outcome = try_acquire_singleton_at(&pid_path, &focus_path, 7, always_alive);
        assert!(matches!(outcome, SingletonOutcome::Acquired(_)));
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap().trim(), "7");
    }

    #[test]
    fn singleton_pid_with_surrounding_whitespace_is_parsed() {
        // The trim() in the helper handles trailing newlines (some
        // editors / shells append one).
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("dashboard.pid");
        let focus_path = dir.path().join("dashboard.focus");
        std::fs::write(&pid_path, "  4242\n").unwrap();
        let outcome = try_acquire_singleton_at(&pid_path, &focus_path, 7, always_alive);
        assert!(matches!(outcome, SingletonOutcome::Forwarded));
    }
}

fn fmt_age(age: chrono::Duration) -> String {
    let secs = age.num_seconds().max(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
