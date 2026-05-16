//! One-shot setup window: opens a provider's sign-in page in an
//! embedded webview, watches for the authenticated state, then
//! captures the session cookie(s) and writes them into the user's
//! config.
//!
//! Two targets:
//!   * `ollama` (default) — ollama.com/signin; success = landing on
//!     the authenticated `/settings` page; cookie → `[ollama_cloud]`.
//!   * `codex`            — chatgpt.com; success = a chatgpt.com
//!     session-token cookie is present while on the app (not the
//!     auth/login pages); cookie → `[codex_cli].chatgpt_session_cookie`.
//!
//! Why a separate binary: the always-resident tray and the egui-based
//! dashboard both run their own event loops; wry's webview wants its
//! own tao loop and pulls in webkit2gtk on Linux. Keeping it one-shot
//! means the tray's resident memory stays small, the webview deps are
//! only paid when the user asks for it, and nothing webview-related
//! stays resident on the user's machine afterwards.
//!
//! Why we read the cookie store rather than `document.cookie`: the
//! session cookie is HttpOnly, so JS can't see it. wry's
//! `WebView::cookies()` talks to the underlying CookieManager
//! (WebKitCookieManager / WKHTTPCookieStore), which has full access
//! including HttpOnly entries. This path also avoids the OS keyring /
//! libsecret permission prompt that reading a browser's own cookie DB
//! triggers.

use anyhow::{Context, Result};
use llm_usage_core::config::{config_path, Config};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const POLL_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Ollama,
    Codex,
}

impl Target {
    /// Default is Ollama so an argument-less invocation keeps the
    /// historical behaviour (older dashboards spawned us with no args).
    fn parse(args: &[String]) -> Result<Self> {
        let pos = args.iter().skip(1).find(|a| !a.starts_with('-'));
        match pos.map(|s| s.as_str()) {
            None | Some("ollama") | Some("ollama_cloud") => Ok(Target::Ollama),
            Some("codex") | Some("codex_cli") | Some("chatgpt") => Ok(Target::Codex),
            Some(other) => Err(anyhow::anyhow!(
                "unknown target {other:?} (expected 'ollama' or 'codex')"
            )),
        }
    }

    fn signin_url(self) -> &'static str {
        match self {
            Target::Ollama => "https://ollama.com/signin",
            Target::Codex => "https://chatgpt.com/",
        }
    }

    fn product(self) -> &'static str {
        match self {
            Target::Ollama => "Ollama Cloud",
            Target::Codex => "ChatGPT (Codex)",
        }
    }

    /// Cookie-domain suffixes to harvest. Codex needs both chatgpt.com
    /// and openai.com — the same pair the dashboard's rookie import
    /// filters on, because the auth flow round-trips through
    /// auth.openai.com.
    fn domains(self) -> &'static [&'static str] {
        match self {
            Target::Ollama => &["ollama.com"],
            Target::Codex => &["chatgpt.com", "openai.com"],
        }
    }

    /// Have we reached the authenticated state? `url` is the webview's
    /// current location; `header` is the already domain-filtered
    /// `name=value; …` string for this target.
    fn is_ready(self, url: &str, header: &str) -> bool {
        match self {
            // Unchanged: the authenticated Ollama dashboard lives at
            // /settings, and the cookie store may lag a tick behind
            // navigation (webkit2gtk loads it lazily) so we also wait
            // for a non-empty header.
            Target::Ollama => url.contains("/settings") && !header.is_empty(),
            // ChatGPT has no tidy post-login landmark, so key off the
            // session cookie itself: `__Secure-next-auth.session-token`
            // is only set once auth completes. Require we're back on
            // the app host (not auth.openai.com / a login page) so we
            // don't latch onto a half-finished SSO round-trip.
            Target::Codex => {
                url.contains("chatgpt.com")
                    && !url.contains("/auth")
                    && !url.contains("login")
                    && header.contains("session-token")
            }
        }
    }

    fn save(self, header: &str) -> Result<std::path::PathBuf> {
        let path = config_path().context("resolve config path")?;
        let mut cfg = Config::load_or_default()?;
        match self {
            Target::Ollama => {
                cfg.ollama_cloud.enabled = true;
                cfg.ollama_cloud.session_cookie = Some(header.to_string());
            }
            Target::Codex => {
                // Leave codex_cli.enabled alone (it defaults on and is
                // the user's call); the cookie is purely a live-quota
                // source override.
                cfg.codex_cli.chatgpt_session_cookie = Some(header.to_string());
            }
        }
        cfg.save(&path)?;
        Ok(path)
    }
}

fn print_help() {
    println!("llm-usage-setup - one-shot sign-in window");
    println!();
    println!("USAGE:");
    println!("  llm-usage-setup [ollama|codex]   Open the sign-in webview");
    println!("  llm-usage-setup --help|-h        Print this help");
    println!();
    println!("Targets:");
    println!("  ollama  (default)  Sign in to Ollama Cloud (ollama.com).");
    println!("  codex              Sign in to ChatGPT for Codex live quota");
    println!("                     (chatgpt.com).");
    println!();
    println!("After you sign in, the session cookie is captured");
    println!("automatically and saved to config.toml, and the window");
    println!("closes. Nothing webview-related stays resident afterwards.");
    println!();
    println!("Normally spawned by the dashboard's Settings tab, not run");
    println!("directly. The \"Import from browser\" button is the other");
    println!("way to configure either provider — it reads cookies from an");
    println!("already-logged-in browser instead of opening this window.");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    let target = Target::parse(&args)?;

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
        .with_title(format!("Sign in to {}", target.product()))
        .with_inner_size(LogicalSize::new(960.0, 720.0))
        .build(&event_loop)?;

    let webview = WebViewBuilder::new()
        .with_url(target.signin_url())
        .build(&window)?;

    let mut last_check = Instant::now() - POLL_INTERVAL;
    let mut captured = false;

    event_loop.run(move |event, _, control_flow| {
        // WaitUntil (not Poll) keeps the loop idle between ticks so the
        // webview stays responsive without spinning.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL);

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if captured {
                    *control_flow = ControlFlow::Exit;
                } else {
                    // User closed the window before completing sign-in.
                    // Exit non-zero so the dashboard reports "setup
                    // cancelled" instead of a false "Captured" — a
                    // plain ControlFlow::Exit is status 0, which the
                    // spawner maps to success.
                    tracing::info!("setup window closed before login completed");
                    std::process::exit(2);
                }
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
                let cookies = match webview.cookies() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "cookie read failed; will retry");
                        return;
                    }
                };
                let header = format_cookies(&cookies, target.domains());

                if !target.is_ready(&url, &header) {
                    window.set_title(&format!(
                        "Sign in to {} — waiting…",
                        target.product()
                    ));
                    return;
                }

                match target.save(&header) {
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

/// Combine every cookie whose domain matches one of `domains` into a
/// single Cookie request-header string (`name1=value1; name2=value2`).
/// We keep all matching cookies rather than just the auth one because
/// the settings/usage pages may rely on CSRF / preference cookies as a
/// sanity check.
fn format_cookies(cookies: &[cookie::Cookie<'static>], domains: &[&str]) -> String {
    cookies
        .iter()
        .filter(|c| {
            c.domain()
                .map(|d| {
                    let d = d.trim_start_matches('.');
                    // Exact host or a true sub-domain only — a bare
                    // `ends_with("openai.com")` would also swallow a
                    // cookie scoped to `evilopenai.com`.
                    domains
                        .iter()
                        .any(|suffix| d == *suffix || d.ends_with(&format!(".{suffix}")))
                })
                .unwrap_or(false)
        })
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ")
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
    fn parse_defaults_to_ollama_and_aliases() {
        assert_eq!(Target::parse(&["bin".into()]).unwrap(), Target::Ollama);
        assert_eq!(
            Target::parse(&["bin".into(), "ollama".into()]).unwrap(),
            Target::Ollama
        );
        assert_eq!(
            Target::parse(&["bin".into(), "codex".into()]).unwrap(),
            Target::Codex
        );
        assert_eq!(
            Target::parse(&["bin".into(), "chatgpt".into()]).unwrap(),
            Target::Codex
        );
        // Flags are skipped when picking the positional target.
        assert_eq!(
            Target::parse(&["bin".into(), "--verbose".into(), "codex".into()]).unwrap(),
            Target::Codex
        );
        assert!(Target::parse(&["bin".into(), "bogus".into()]).is_err());
    }

    #[test]
    fn format_cookies_filters_to_ollama_domain() {
        let v = vec![
            cookie("ollama.com", "session", "abc"),
            cookie("google.com", "irrelevant", "xyz"),
            cookie(".ollama.com", "csrf", "def"),
        ];
        let s = format_cookies(&v, Target::Ollama.domains());
        assert!(s.contains("session=abc"));
        assert!(s.contains("csrf=def"));
        assert!(!s.contains("irrelevant"));
    }

    #[test]
    fn format_cookies_codex_keeps_chatgpt_and_openai() {
        let v = vec![
            cookie("chatgpt.com", "__Secure-next-auth.session-token", "tok"),
            cookie(".openai.com", "oai-sc", "x"),
            cookie("ollama.com", "session", "nope"),
        ];
        let s = format_cookies(&v, Target::Codex.domains());
        assert!(s.contains("__Secure-next-auth.session-token=tok"));
        assert!(s.contains("oai-sc=x"));
        assert!(!s.contains("session=nope"));
    }

    #[test]
    fn format_cookies_joins_with_semicolon_space() {
        let v = vec![
            cookie("ollama.com", "a", "1"),
            cookie("ollama.com", "b", "2"),
        ];
        let s = format_cookies(&v, &["ollama.com"]);
        assert_eq!(s, "a=1; b=2");
    }

    #[test]
    fn format_cookies_handles_leading_dot_subdomain() {
        let v = vec![cookie(".ollama.com", "x", "y")];
        let s = format_cookies(&v, &["ollama.com"]);
        assert_eq!(s, "x=y");
    }

    #[test]
    fn format_cookies_no_match_returns_empty_string() {
        let v = vec![cookie("foo.bar", "n", "v")];
        assert!(format_cookies(&v, &["ollama.com"]).is_empty());
    }

    #[test]
    fn format_cookies_rejects_lookalike_suffix_domain() {
        // `evilopenai.com` must NOT match the `openai.com` suffix; a
        // genuine sub-domain still must.
        let v = vec![
            cookie("evilopenai.com", "stolen", "1"),
            cookie("notchatgpt.com", "x", "2"),
            cookie("auth.openai.com", "ok", "3"),
            cookie("chatgpt.com", "ok2", "4"),
        ];
        let s = format_cookies(&v, Target::Codex.domains());
        assert!(!s.contains("stolen"));
        assert!(!s.contains("x=2"));
        assert!(s.contains("ok=3"));
        assert!(s.contains("ok2=4"));
    }

    #[test]
    fn ollama_ready_requires_settings_path_and_cookies() {
        let t = Target::Ollama;
        assert!(!t.is_ready("https://ollama.com/signin", "s=1"));
        assert!(!t.is_ready("https://ollama.com/settings", ""));
        assert!(t.is_ready("https://ollama.com/settings", "s=1"));
    }

    #[test]
    fn codex_ready_requires_session_token_on_app_host() {
        let t = Target::Codex;
        // Still on the login / SSO round-trip — not ready.
        assert!(!t.is_ready("https://auth.openai.com/authorize", ""));
        assert!(!t.is_ready(
            "https://chatgpt.com/auth/login",
            "__Secure-next-auth.session-token=tok"
        ));
        // Pre-login chatgpt.com with only a Cloudflare cookie — not ready.
        assert!(!t.is_ready("https://chatgpt.com/", "cf_clearance=zzz"));
        // Authenticated on the app with the session token — ready.
        assert!(t.is_ready(
            "https://chatgpt.com/",
            "cf_clearance=zzz; __Secure-next-auth.session-token=tok"
        ));
    }
}
