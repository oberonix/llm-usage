//! Bitmap renderer for the tray icon.
//!
//! Each provider gets its own coloured "card" with two stacked
//! progress bars: the upper bar represents the short-window (5h /
//! session / today) quota, the lower the weekly one. The bar fill
//! color shifts green → amber → red as utilization climbs so a glance
//! at the panel tells you whether anyone is close to a limit.
//!
//! No font rendering, no SVG — we paint into a 22×22 RGBA buffer
//! directly. tray-icon scales for HiDPI panels.

use llm_usage_core::model::{ProviderId, UsageSnapshot};
use tray_icon::Icon;

const SIZE: u32 = 22;
const SIZE_U: usize = SIZE as usize;

// Layout (22×22):
//   y 0..10  — provider tint (10 px) for the "what provider is this?" cue
//   y 10..15 — session bar (5 px, full-width)
//   y 15..16 — 1 px tint divider keeps the two bars visually distinct
//   y 16..22 — weekly bar (6 px, full-width, flush with bottom edge)
const TINT_HEIGHT: usize = 10;
const SESSION_Y: usize = 10;
const SESSION_HEIGHT: usize = 5;
const DIVIDER_Y: usize = 15;
const WEEKLY_Y: usize = 16;
const WEEKLY_HEIGHT: usize = 6;

/// Data for one bar: how much is used (`fraction`, 0..1) and where the
/// user is in the time window (`pace`, 0..1). If `pace` is set, a 1 px
/// red vertical line is drawn at that x-position across the bar so the
/// user can see whether they're ahead of or behind a steady consumption
/// rate.
#[derive(Default, Copy, Clone)]
pub struct BarSlot {
    pub fraction: Option<f64>,
    pub pace: Option<f64>,
}

/// Render the active provider's icon. Each `BarSlot` carries the
/// fraction used and the elapsed fraction of the matching time window.
pub fn render(provider: ProviderId, session: BarSlot, weekly: BarSlot) -> Icon {
    let mut buf = vec![0u8; SIZE_U * SIZE_U * 4];
    let tint = provider_tint(provider);
    // Top: full-bleed tint. Also used as the 1 px divider between bars.
    fill_rect(&mut buf, 0, 0, SIZE_U, TINT_HEIGHT, tint);
    fill_rect(&mut buf, 0, DIVIDER_Y, SIZE_U, 1, tint);

    draw_label(&mut buf, provider_label(provider), (0xFF, 0xFF, 0xFF));
    draw_bar(
        &mut buf,
        0,
        SESSION_Y,
        SIZE_U,
        SESSION_HEIGHT,
        session.fraction,
    );
    if let Some(p) = session.pace {
        draw_pace_marker(&mut buf, 0, SESSION_Y, SIZE_U, SESSION_HEIGHT, p);
    }
    draw_bar(
        &mut buf,
        0,
        WEEKLY_Y,
        SIZE_U,
        WEEKLY_HEIGHT,
        weekly.fraction,
    );
    if let Some(p) = weekly.pace {
        draw_pace_marker(&mut buf, 0, WEEKLY_Y, SIZE_U, WEEKLY_HEIGHT, p);
    }

    Icon::from_rgba(buf, SIZE, SIZE).expect("icon construction")
}

/// Used at startup before any snapshots arrive, and whenever no
/// provider has quota fractions to display. No pace marker — there's
/// no window context yet.
pub fn render_placeholder() -> Icon {
    let mut buf = vec![0u8; SIZE_U * SIZE_U * 4];
    let neutral = (0x4A, 0x4A, 0x4A);
    fill_rect(&mut buf, 0, 0, SIZE_U, TINT_HEIGHT, neutral);
    fill_rect(&mut buf, 0, DIVIDER_Y, SIZE_U, 1, neutral);
    draw_bar(&mut buf, 0, SESSION_Y, SIZE_U, SESSION_HEIGHT, None);
    draw_bar(&mut buf, 0, WEEKLY_Y, SIZE_U, WEEKLY_HEIGHT, None);
    Icon::from_rgba(buf, SIZE, SIZE).expect("icon construction")
}

fn draw_bar(buf: &mut [u8], x: usize, y: usize, w: usize, h: usize, frac: Option<f64>) {
    fill_rect(buf, x, y, w, h, (0x1F, 0x1F, 0x1F));
    if let Some(f) = frac {
        let clamped = f.clamp(0.0, 1.0);
        let fill_w = ((clamped * w as f64).round() as usize).min(w);
        if fill_w > 0 {
            fill_rect(buf, x, y, fill_w, h, bar_color(clamped));
        }
    }
}

/// Vertical 1 px line across a bar at the given pace (0..1).
/// Magenta on every tier so it stays distinct from the
/// green/amber/red fill colours (and matches the dashboard and CLI
/// pace markers). Drawn on top of the fill so it's always visible
/// regardless of where it sits in the bar.
fn draw_pace_marker(buf: &mut [u8], x: usize, y: usize, w: usize, h: usize, pace: f64) {
    if w == 0 {
        return;
    }
    let pace = pace.clamp(0.0, 1.0);
    let pos = ((pace * (w - 1) as f64).round() as usize).min(w - 1);
    // Magenta on every tier — distinct from the green/amber/red fill
    // and from a grey stale bar, so the marker never blends in.
    let color = (0xE0, 0x40, 0xE0);
    for yy in y..(y + h).min(SIZE_U) {
        let xx = x + pos;
        if xx >= SIZE_U {
            break;
        }
        let i = (yy * SIZE_U + xx) * 4;
        buf[i] = color.0;
        buf[i + 1] = color.1;
        buf[i + 2] = color.2;
        buf[i + 3] = 0xFF;
    }
}

fn fill_rect(buf: &mut [u8], x: usize, y: usize, w: usize, h: usize, c: (u8, u8, u8)) {
    for yy in y..(y + h).min(SIZE_U) {
        for xx in x..(x + w).min(SIZE_U) {
            let i = (yy * SIZE_U + xx) * 4;
            buf[i] = c.0;
            buf[i + 1] = c.1;
            buf[i + 2] = c.2;
            buf[i + 3] = 0xFF;
        }
    }
}

fn bar_color(f: f64) -> (u8, u8, u8) {
    if f < 0.60 {
        (0x4C, 0xAF, 0x50) // green
    } else if f < 0.85 {
        (0xFF, 0xB3, 0x00) // amber
    } else {
        (0xE5, 0x39, 0x35) // red
    }
}

fn provider_tint(id: ProviderId) -> (u8, u8, u8) {
    id.tint_rgb()
}

/// Pick the (short-window, weekly) bar data out of a snapshot.
/// Includes both the utilization fraction and the pace through the
/// time window (so the icon can draw the red pace marker).
pub fn pick_bars(snap: &UsageSnapshot) -> (BarSlot, BarSlot) {
    let now = chrono::Utc::now();
    let session = bar_slot_for(
        snap.windows
            .get("5h")
            .or_else(|| snap.windows.get("Claude 5h"))
            .or_else(|| snap.windows.get("Gemini 5h")),
        "5h",
        now,
    );
    let weekly = bar_slot_for(
        snap.windows
            .get("week")
            .or_else(|| snap.windows.get("Claude week"))
            .or_else(|| snap.windows.get("Gemini week")),
        "week",
        now,
    );
    (session, weekly)
}

fn bar_slot_for(
    w: Option<&llm_usage_core::model::WindowUsage>,
    label: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> BarSlot {
    let mut out = BarSlot::default();
    let Some(w) = w else {
        return out;
    };
    out.fraction = w.fraction_used;
    // Pace = (window_duration − time_until_reset) / window_duration.
    // We hardcode the duration per label so we don't need the
    // provider to surface a "window length" field.
    let window_secs: i64 = match label {
        "5h" => 5 * 3600,
        "week" => 7 * 86_400,
        _ => return out,
    };
    if let Some(ends) = w.ends_at {
        let remaining = (ends - now).num_seconds().max(0).min(window_secs);
        let elapsed = window_secs - remaining;
        out.pace = Some(elapsed as f64 / window_secs as f64);
    }
    out
}

/// True if this snapshot has at least one window with a known quota
/// fraction. Providers without quota data (Codex, Gemini, OpenAI,
/// Ollama local) are skipped in the icon rotation entirely — there's
/// nothing meaningful to draw at 22 px.
pub fn has_quota_data(snap: &UsageSnapshot) -> bool {
    snap.windows.values().any(|w| w.fraction_used.is_some())
}

// ----- 5×7 bitmap font for the provider label -----
//
// Each glyph is 7 rows × 5 columns. The low 5 bits of every byte hold
// one row, MSB at x=0. We only ship the 11 letters we actually need
// (A, C, D, E, G, I, L, M, N, O, T) plus a blank fallback. No serde,
// no font crate — just constants.

const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;
const GLYPH_SPACING: usize = 1;

fn provider_label(id: ProviderId) -> &'static str {
    match id {
        ProviderId::Anthropic => "ANT",
        ProviderId::Antigravity => "AGY",
        ProviderId::CodexCli => "COD",
        ProviderId::OllamaCloud => "OLC",
    }
}

#[rustfmt::skip]
const GLYPH_A: [u8; 7] = [
    0b01110,
    0b10001,
    0b10001,
    0b11111,
    0b10001,
    0b10001,
    0b10001,
];
#[rustfmt::skip]
const GLYPH_C: [u8; 7] = [
    0b01111,
    0b10000,
    0b10000,
    0b10000,
    0b10000,
    0b10000,
    0b01111,
];
#[rustfmt::skip]
const GLYPH_D: [u8; 7] = [
    0b11110,
    0b10001,
    0b10001,
    0b10001,
    0b10001,
    0b10001,
    0b11110,
];
#[rustfmt::skip]
const GLYPH_E: [u8; 7] = [
    0b11111,
    0b10000,
    0b10000,
    0b11110,
    0b10000,
    0b10000,
    0b11111,
];
#[rustfmt::skip]
const GLYPH_G: [u8; 7] = [
    0b01111,
    0b10000,
    0b10000,
    0b10011,
    0b10001,
    0b10001,
    0b01111,
];
#[rustfmt::skip]
const GLYPH_I: [u8; 7] = [
    0b11111,
    0b00100,
    0b00100,
    0b00100,
    0b00100,
    0b00100,
    0b11111,
];
#[rustfmt::skip]
const GLYPH_L: [u8; 7] = [
    0b10000,
    0b10000,
    0b10000,
    0b10000,
    0b10000,
    0b10000,
    0b11111,
];
#[rustfmt::skip]
const GLYPH_M: [u8; 7] = [
    0b10001,
    0b11011,
    0b10101,
    0b10101,
    0b10001,
    0b10001,
    0b10001,
];
#[rustfmt::skip]
const GLYPH_N: [u8; 7] = [
    0b10001,
    0b11001,
    0b10101,
    0b10101,
    0b10011,
    0b10001,
    0b10001,
];
#[rustfmt::skip]
const GLYPH_O: [u8; 7] = [
    0b01110,
    0b10001,
    0b10001,
    0b10001,
    0b10001,
    0b10001,
    0b01110,
];
#[rustfmt::skip]
const GLYPH_T: [u8; 7] = [
    0b11111,
    0b00100,
    0b00100,
    0b00100,
    0b00100,
    0b00100,
    0b00100,
];
const GLYPH_BLANK: [u8; 7] = [0; 7];

fn glyph(c: char) -> &'static [u8; 7] {
    match c {
        'A' => &GLYPH_A,
        'C' => &GLYPH_C,
        'D' => &GLYPH_D,
        'E' => &GLYPH_E,
        'G' => &GLYPH_G,
        'I' => &GLYPH_I,
        'L' => &GLYPH_L,
        'M' => &GLYPH_M,
        'N' => &GLYPH_N,
        'O' => &GLYPH_O,
        'T' => &GLYPH_T,
        _ => &GLYPH_BLANK,
    }
}

fn draw_label(buf: &mut [u8], label: &str, color: (u8, u8, u8)) {
    let chars = label.chars().count();
    if chars == 0 {
        return;
    }
    let total_w = chars * GLYPH_W + chars.saturating_sub(1) * GLYPH_SPACING;
    // Center inside the tint area. Width 22 with a 3-char label → 17 px
    // of glyph row → 2 px left margin, 3 px right (or vice versa); the
    // integer divide leans us toward the left edge by half a pixel.
    let start_x = SIZE_U.saturating_sub(total_w) / 2;
    let start_y = TINT_HEIGHT.saturating_sub(GLYPH_H) / 2;

    for (i, ch) in label.chars().enumerate() {
        let g = glyph(ch);
        let gx = start_x + i * (GLYPH_W + GLYPH_SPACING);
        for (y, &row) in g.iter().enumerate() {
            for x in 0..GLYPH_W {
                let bit_set = (row >> (GLYPH_W - 1 - x)) & 1 == 1;
                if bit_set {
                    put_pixel(buf, gx + x, start_y + y, color);
                }
            }
        }
    }
}

fn put_pixel(buf: &mut [u8], x: usize, y: usize, c: (u8, u8, u8)) {
    if x >= SIZE_U || y >= SIZE_U {
        return;
    }
    let i = (y * SIZE_U + x) * 4;
    buf[i] = c.0;
    buf[i + 1] = c.1;
    buf[i + 2] = c.2;
    buf[i + 3] = 0xFF;
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_usage_core::model::{ProviderStatus, UsageSnapshot, WindowUsage};
    use std::collections::BTreeMap;

    #[test]
    fn provider_tint_matches_core_table() {
        // The icon and the dashboard share `ProviderId::tint_rgb()` —
        // this test catches the case where the tray helper drifts.
        assert_eq!(
            provider_tint(ProviderId::Anthropic),
            ProviderId::Anthropic.tint_rgb()
        );
        assert_eq!(
            provider_tint(ProviderId::CodexCli),
            ProviderId::CodexCli.tint_rgb()
        );
        assert_eq!(
            provider_tint(ProviderId::OllamaCloud),
            ProviderId::OllamaCloud.tint_rgb()
        );
    }

    #[test]
    fn bar_color_changes_at_thresholds() {
        assert_eq!(bar_color(0.10), (0x4C, 0xAF, 0x50));
        assert_eq!(bar_color(0.70), (0xFF, 0xB3, 0x00));
        assert_eq!(bar_color(0.95), (0xE5, 0x39, 0x35));
    }

    #[test]
    fn provider_label_three_chars_per_provider() {
        assert_eq!(provider_label(ProviderId::Anthropic), "ANT");
        assert_eq!(provider_label(ProviderId::Antigravity), "AGY");
        assert_eq!(provider_label(ProviderId::CodexCli), "COD");
        assert_eq!(provider_label(ProviderId::OllamaCloud), "OLC");
    }

    fn snap_with_quota(short: Option<f64>, weekly: Option<f64>, with_reset: bool) -> UsageSnapshot {
        let now = chrono::Utc::now();
        let mut windows: BTreeMap<String, WindowUsage> = BTreeMap::new();
        if let Some(f) = short {
            windows.insert(
                "5h".into(),
                WindowUsage {
                    fraction_used: Some(f),
                    ends_at: with_reset.then(|| now + chrono::Duration::hours(3)),
                    ..Default::default()
                },
            );
        }
        if let Some(f) = weekly {
            windows.insert(
                "week".into(),
                WindowUsage {
                    fraction_used: Some(f),
                    ends_at: with_reset.then(|| now + chrono::Duration::days(2)),
                    ..Default::default()
                },
            );
        }
        UsageSnapshot {
            provider: ProviderId::Anthropic,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows,
            headline: None,
            plan_label: None,
        }
    }

    #[test]
    fn pick_bars_extracts_fraction_and_pace_when_ends_at_present() {
        let snap = snap_with_quota(Some(0.2), Some(0.5), true);
        let (s, w) = pick_bars(&snap);
        assert_eq!(s.fraction, Some(0.2));
        assert_eq!(w.fraction, Some(0.5));
        // Pace = elapsed / total. 3h left of 5h → elapsed 2h → 0.4.
        let p = s.pace.unwrap();
        assert!((p - 0.4).abs() < 0.05, "got {}", p);
        let p = w.pace.unwrap();
        // 2 days left of 7 → elapsed 5/7 ≈ 0.71.
        assert!(p > 0.6 && p < 0.8, "got {}", p);
    }

    #[test]
    fn pick_bars_no_pace_when_ends_at_missing() {
        let snap = snap_with_quota(Some(0.2), Some(0.5), false);
        let (s, w) = pick_bars(&snap);
        assert_eq!(s.fraction, Some(0.2));
        assert!(s.pace.is_none());
        assert_eq!(w.fraction, Some(0.5));
        assert!(w.pace.is_none());
    }

    #[test]
    fn pick_bars_handles_absent_windows() {
        let snap = snap_with_quota(None, None, false);
        let (s, w) = pick_bars(&snap);
        assert!(s.fraction.is_none());
        assert!(s.pace.is_none());
        assert!(w.fraction.is_none());
        assert!(w.pace.is_none());
    }

    #[test]
    fn pick_bars_uses_antigravity_claude_bucket_names() {
        let now = chrono::Utc::now();
        let mut snap = UsageSnapshot {
            provider: ProviderId::Antigravity,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: BTreeMap::new(),
            headline: None,
            plan_label: None,
        };
        snap.windows.insert(
            "Claude 5h".into(),
            WindowUsage {
                fraction_used: Some(0.72),
                ends_at: Some(now + chrono::Duration::hours(2)),
                ..Default::default()
            },
        );
        snap.windows.insert(
            "Claude week".into(),
            WindowUsage {
                fraction_used: Some(0.24),
                ends_at: Some(now + chrono::Duration::days(6)),
                ..Default::default()
            },
        );

        let (short, week) = pick_bars(&snap);
        assert_eq!(short.fraction, Some(0.72));
        assert_eq!(week.fraction, Some(0.24));
        assert!(short.pace.is_some());
        assert!(week.pace.is_some());
    }

    #[test]
    fn has_quota_data_matches_fraction_presence() {
        let snap = snap_with_quota(Some(0.2), None, false);
        assert!(has_quota_data(&snap));
        let snap = snap_with_quota(None, None, false);
        assert!(!has_quota_data(&snap));
    }

    #[test]
    fn render_produces_correct_size_rgba_buffer() {
        // Smoke: rendering shouldn't panic and the size const should be
        // honoured. (Icon::from_rgba would panic if the dims were off.)
        let session = BarSlot {
            fraction: Some(0.5),
            pace: Some(0.3),
        };
        let weekly = BarSlot {
            fraction: None,
            pace: None,
        };
        let _ = render(ProviderId::Anthropic, session, weekly);
        let _ = render_placeholder();
    }
}
