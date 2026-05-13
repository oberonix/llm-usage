//! Debug helper: fetch the Ollama Cloud /settings page using the
//! configured session cookie and write the raw HTML to stdout. Use this
//! to iterate on the scraper's selectors when Ollama changes the page.
//!
//! Usage:
//!   cargo run -p llm-usage-core --example dump_ollama_cloud > /tmp/ollama-settings.html
//!
//! The session cookie is read from your normal config (or
//! LLM_USAGE_CONFIG=/path/to/config.toml).
//!
//! ⚠️  Treat the output as sensitive. The HTML embeds session-bound
//! CSRF tokens, your account email, and other identifiers from your
//! logged-in ollama.com session. Don't paste the file into a public
//! issue or share it without first stripping those fields.

use llm_usage_core::config::Config;
use llm_usage_core::providers::OllamaCloudProvider;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = match std::env::var("LLM_USAGE_CONFIG") {
        Ok(p) => Config::load_from(std::path::Path::new(&p))?,
        Err(_) => Config::load_or_default()?,
    };

    if cfg.ollama_cloud.session_cookie.is_none() {
        eprintln!(
            "no [ollama_cloud].session_cookie set — paste the browser Cookie header \
             into your config.toml first"
        );
        std::process::exit(2);
    }
    eprintln!(
        "WARNING: output contains your logged-in ollama.com session HTML \
         (CSRF tokens, account email, etc.). Don't share it publicly without \
         redacting first."
    );

    let provider = OllamaCloudProvider::new(cfg.ollama_cloud);
    let html = provider.fetch_settings_html().await?;
    println!("{}", html);
    Ok(())
}
