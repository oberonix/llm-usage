//! Reconnaissance helper: try to pull data from
//! `https://chatgpt.com/codex/cloud/settings/analytics`, which is the
//! ChatGPT-side admin page that shows current Codex Cloud quota
//! consumption. We don't know yet whether the percentages are
//! server-rendered (Ollama-style scrape) or fetched at runtime by an
//! XHR call to some `/backend-api/...` endpoint (the much more
//! likely scenario given chatgpt.com is a Next.js app).
//!
//! Run once with a fresh chatgpt.com session in your browser:
//!
//!     cargo run -p llm-usage-core --example dump_codex_cloud
//!
//! Then look at the output:
//!   - HTTP status (200 = page loaded, 4xx = auth / bot challenge,
//!     3xx with Location to /login = cookies didn't survive).
//!   - Body — is it HTML with `<div>27.8% used</div>` somewhere
//!     scrape-able, or just a Next.js shell?
//!   - When the body is a shell, scroll the network tab in your
//!     browser's devtools while loading the page and note which
//!     XHR call returns the usage JSON. That call is what we'd
//!     actually plumb into a CodexCloud provider.
//!
//! ⚠️ Output may contain bearer tokens, account ids, and other
//! session-bound material. Redact before sharing.
//!
//! ⚠️ Cookies are read non-interactively via `rookie`; on Linux that
//! triggers a keyring prompt the first time when Chrome's cookie
//! DB is encrypted with libsecret.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

const ANALYTICS_URL: &str = "https://chatgpt.com/codex/cloud/settings/analytics";
// A handful of plausible XHR endpoints to probe alongside the page.
// We have NOT confirmed these exist — they're educated guesses based
// on chatgpt.com's typical `/backend-api/<feature>/...` shape. If a
// 404 comes back the user should grab the real path from devtools.
const PROBE_ENDPOINTS: &[&str] = &[
    "https://chatgpt.com/backend-api/codex/cloud/settings/analytics/usage",
    "https://chatgpt.com/backend-api/codex/usage",
    "https://chatgpt.com/backend-api/me",
];

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    eprintln!(
        "WARNING: this prints your logged-in chatgpt.com response body, \
         which may include session-bound tokens. Don't paste publicly \
         without redaction.\n"
    );

    // Pull cookies for both chatgpt.com and its sibling auth domain.
    // The session-bearer is usually `__Secure-next-auth.session-token`
    // on chatgpt.com itself; auth0 / login cookies live on the
    // `auth.openai.com` sibling so OpenAI's redirect handshake works
    // on first load. We grab both and let the request pick what it
    // needs.
    let cookies = rookie::load(Some(vec![
        "chatgpt.com".to_string(),
        ".chatgpt.com".to_string(),
        "openai.com".to_string(),
        ".openai.com".to_string(),
    ]))
    .map_err(|e| anyhow!("rookie cookie load failed: {}", e))?;

    if cookies.is_empty() {
        return Err(anyhow!(
            "no chatgpt.com cookies found in any installed browser. \
             Sign in to chatgpt.com in your browser first, then re-run."
        ));
    }

    let cookie_header: String = cookies
        .iter()
        .filter(|c| {
            let d = c.domain.trim_start_matches('.');
            d.ends_with("chatgpt.com") || d.ends_with("openai.com")
        })
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ");

    eprintln!(
        "Loaded {} cookie(s) ({} chars) — domains: {}",
        cookies.len(),
        cookie_header.len(),
        cookies
            .iter()
            .map(|c| c.domain.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", "),
    );

    // Browser-shaped UA + accept headers so chatgpt.com doesn't decide
    // we're a bot and serve a stripped shell. We don't actually parse
    // anything here — the whole point is to see the raw response.
    let http = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/130.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::default())
        .build()?;

    probe(&http, &cookie_header, ANALYTICS_URL, true).await?;
    for url in PROBE_ENDPOINTS {
        probe(&http, &cookie_header, url, false).await?;
    }
    Ok(())
}

async fn probe(
    http: &reqwest::Client,
    cookie_header: &str,
    url: &str,
    accept_html: bool,
) -> Result<()> {
    eprintln!("\n--- GET {url} ---");
    let accept = if accept_html {
        "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.1"
    } else {
        "application/json,text/plain;q=0.9,*/*;q=0.1"
    };
    let resp = http
        .get(url)
        .header(reqwest::header::COOKIE, cookie_header)
        .header(reqwest::header::ACCEPT, accept)
        // chatgpt.com's backend rejects requests without these on
        // some paths; harmless to send always.
        .header("Sec-Fetch-Mode", if accept_html { "navigate" } else { "cors" })
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;

    let status = resp.status();
    let final_url = resp.url().clone();
    eprintln!("status: {status}  final url: {final_url}");
    for (k, v) in resp.headers().iter() {
        // Surface only the headers a reverse-engineer would care
        // about — content-type, redirect target, CSP, anti-bot
        // markers — to keep the noise down.
        let key = k.as_str();
        if matches!(
            key,
            "content-type" | "location" | "cf-mitigated" | "server" | "set-cookie" | "x-ratelimit-remaining"
        ) {
            eprintln!("  {key}: {}", v.to_str().unwrap_or("<binary>"));
        }
    }
    let body = resp.text().await.context("read body")?;
    eprintln!("body: {} bytes", body.len());
    // Print a small head + a hint of whether percentage tokens
    // appear. If the page is server-rendered we'd expect to see
    // raw "% used" strings; if it's a shell we won't.
    let head: String = body.chars().take(400).collect();
    eprintln!("---- head (first 400 chars) ----\n{head}\n--------");
    eprintln!(
        "  contains `% used`: {}",
        body.contains("% used")
    );
    eprintln!(
        "  contains `next/static`: {}  (Next.js shell marker)",
        body.contains("next/static")
    );
    Ok(())
}
