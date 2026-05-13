//! One-shot setup window: opens the Ollama signin page in an embedded
//! webview, watches for the user to land on the authenticated
//! `/settings` page, then captures every cookie set for `ollama.com` and
//! writes it to `[ollama_cloud].session_cookie` in the user's config.
//!
//! Why a separate binary: the always-resident tray and the egui-based
//! dashboard both run their own event loops; wry's webview wants its own
//! tao loop and pulls in webkit2gtk on Linux. Keeping it one-shot means
//! the tray's resident memory stays small and the webview deps are only
//! paid when the user clicks "Set up login…".
//!
//! Why we read the cookie store rather than `document.cookie`: the
//! session cookie is HttpOnly, so JS can't see it. wry's `WebView::cookies()`
//! talks to the underlying CookieManager (WebKitCookieManager / WKHTTPCookieStore),
//! which has full access including HttpOnly entries.

use anyhow::{Context, Result};
use llm_usage_core::config::{config_path, Config};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const SIGNIN_URL: &str = "https://ollama.com/signin";
const SUCCESS_PATH: &str = "/settings";
const POLL_INTERVAL: Duration = Duration::from_millis(750);

fn print_help() {
    println!("llm-usage-setup - one-shot sign-in window for Ollama Cloud");
    println!();
    println!("USAGE:");
    println!("  llm-usage-setup              Open the sign-in webview window");
    println!("  llm-usage-setup --help|-h    Print this help");
    println!();
    println!("This binary opens an embedded browser at ollama.com/signin.");
    println!("After you sign in and land on the /settings page, the session");
    println!("cookie is captured automatically and saved to config.toml.");
    println!("The window closes once capture succeeds.");
    println!();
    println!("The binary is normally spawned by the dashboard's Settings tab");
    println!("(Ollama Cloud → \"Sign in via popup window\"), not run directly.");
    println!();
    println!("The primary, recommended way to configure Ollama Cloud is the");
    println!("\"Import from browser\" button in the Settings tab, which reads");
    println!("cookies directly from your already-logged-in browser.");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,llm_usage_setup=debug".into()),
        )
        .init();

    #[cfg(target_os = "linux")]
    {
        gtk::init().map_err(|e| anyhow::anyhow!("gtk init failed: {}", e))?;
    }

    let event_loop = EventLoopBuilder::new().build();
    let window = WindowBuilder::new()
        .with_title("Sign in to Ollama Cloud")
        .with_inner_size(LogicalSize::new(960.0, 720.0))
        .build(&event_loop)?;

    let webview = WebViewBuilder::new().with_url(SIGNIN_URL).build(&window)?;

    let mut last_check = Instant::now() - POLL_INTERVAL;
    let mut captured = false;

    event_loop.run(move |event, _, control_flow| {
        // Schedule the next cookie poll. Using WaitUntil rather than
        // Poll keeps the event loop idle between ticks, so the webview
        // is responsive without spinning.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL);

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if !captured {
                    tracing::info!("setup window closed before login completed");
                }
                *control_flow = ControlFlow::Exit;
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. })
            | Event::NewEvents(StartCause::Init) => {
                if captured {
                    return;
                }
                if last_check.elapsed() < POLL_INTERVAL {
                    return;
                }
                last_check = Instant::now();

                let url = webview.url().unwrap_or_default();
                if !url.contains(SUCCESS_PATH) {
                    let title = if url.contains("ollama.com") {
                        "Sign in to Ollama Cloud — waiting for /settings…"
                    } else {
                        "Sign in to Ollama Cloud"
                    };
                    window.set_title(title);
                    return;
                }

                let cookies = match webview.cookies() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "cookie read failed; will retry");
                        return;
                    }
                };
                let header = format_cookies(&cookies);
                if header.is_empty() {
                    // Logged in URL but cookie store hasn't surfaced
                    // anything yet — webkit2gtk loads the cookie manager
                    // lazily, so the first poll after navigation often
                    // returns empty. Try again next tick.
                    return;
                }

                match save_session_cookie(&header) {
                    Ok(path) => {
                        tracing::info!(path = %path.display(), "captured cookie, exiting");
                        captured = true;
                        window.set_title("Captured! Closing…");
                        *control_flow = ControlFlow::Exit;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to save cookie");
                        window.set_title("Could not save cookie — see logs and retry");
                    }
                }
            }
            _ => {}
        }
    });
}

/// Combine every cookie scoped to `ollama.com` into a single Cookie
/// request-header string (`name1=value1; name2=value2`). We keep all of
/// them rather than just the auth cookie because Ollama's settings page
/// may rely on CSRF / preferences cookies as a sanity check.
fn format_cookies(cookies: &[cookie::Cookie<'static>]) -> String {
    cookies
        .iter()
        .filter(|c| {
            c.domain()
                .map(|d| d.trim_start_matches('.').ends_with("ollama.com"))
                .unwrap_or(false)
        })
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn save_session_cookie(header: &str) -> Result<std::path::PathBuf> {
    let path = config_path().context("resolve config path")?;
    let mut cfg = Config::load_or_default()?;
    cfg.ollama_cloud.enabled = true;
    cfg.ollama_cloud.session_cookie = Some(header.to_string());
    cfg.save(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(domain: &str, name: &str, value: &str) -> cookie::Cookie<'static> {
        let mut c = cookie::Cookie::new(name.to_string(), value.to_string());
        c.set_domain(domain.to_string());
        c
    }

    #[test]
    fn format_cookies_filters_to_ollama_domain() {
        let v = vec![
            cookie("ollama.com", "session", "abc"),
            cookie("google.com", "irrelevant", "xyz"),
            cookie(".ollama.com", "csrf", "def"),
        ];
        let s = format_cookies(&v);
        assert!(s.contains("session=abc"));
        assert!(s.contains("csrf=def"));
        assert!(!s.contains("irrelevant"));
    }

    #[test]
    fn format_cookies_joins_with_semicolon_space() {
        let v = vec![
            cookie("ollama.com", "a", "1"),
            cookie("ollama.com", "b", "2"),
        ];
        let s = format_cookies(&v);
        assert_eq!(s, "a=1; b=2");
    }

    #[test]
    fn format_cookies_handles_leading_dot_subdomain() {
        // The browser stores cookies with `.ollama.com` to denote
        // "all subdomains". Our filter must strip that leading dot
        // before matching.
        let v = vec![cookie(".ollama.com", "x", "y")];
        let s = format_cookies(&v);
        assert_eq!(s, "x=y");
    }

    #[test]
    fn format_cookies_no_match_returns_empty_string() {
        let v = vec![cookie("foo.bar", "n", "v")];
        assert!(format_cookies(&v).is_empty());
    }
}
