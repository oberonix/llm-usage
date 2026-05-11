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

/// Render the active provider's icon. `session` and `weekly` are
/// 0..1 fractions; `None` draws an empty bar (dim grey, no fill).
pub fn render(provider: ProviderId, session: Option<f64>, weekly: Option<f64>) -> Icon {
    let mut buf = vec![0u8; SIZE_U * SIZE_U * 4];
    let tint = provider_tint(provider);
    // Top: full-bleed tint. Also used as the 1 px divider between bars.
    fill_rect(&mut buf, 0, 0, SIZE_U, TINT_HEIGHT, tint);
    fill_rect(&mut buf, 0, DIVIDER_Y, SIZE_U, 1, tint);

    draw_label(&mut buf, provider_label(provider), (0xFF, 0xFF, 0xFF));
    draw_bar(&mut buf, 0, SESSION_Y, SIZE_U, SESSION_HEIGHT, session);
    draw_bar(&mut buf, 0, WEEKLY_Y, SIZE_U, WEEKLY_HEIGHT, weekly);

    Icon::from_rgba(buf, SIZE, SIZE).expect("icon construction")
}

/// Used at startup before any snapshots arrive, and whenever no
/// provider has quota fractions to display.
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

/// Pick the (short-window, weekly) fractions out of a snapshot.
/// Searches a small fallback list of window keys because providers
/// label their short window differently (Anthropic uses "5h", Ollama
/// Cloud uses "session", Gemini uses "today").
pub fn pick_fractions(snap: &UsageSnapshot) -> (Option<f64>, Option<f64>) {
    let session = ["5h", "session", "today"]
        .iter()
        .find_map(|k| snap.windows.get(*k).and_then(|w| w.fraction_used));
    let weekly = snap.windows.get("week").and_then(|w| w.fraction_used);
    (session, weekly)
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
        ProviderId::OpenAi => "OAI",
        ProviderId::CodexCli => "COD",
        ProviderId::GeminiCli => "GEM",
        ProviderId::OllamaLocal => "OLM",
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
        for y in 0..GLYPH_H {
            let row = g[y];
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
