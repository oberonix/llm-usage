//! Headless test harness — polls every enabled provider once and prints
//! the resulting snapshots. Lets you sanity-check provider data sources
//! without needing the tray UI to compile.
//!
//! Usage:
//!   cargo run -p llm-usage-core --example print_snapshots
//!
//! Optional env:
//!   LLM_USAGE_CONFIG=/path/to/config.toml

use llm_usage_core::config::Config;
use llm_usage_core::provider::Provider;
use llm_usage_core::providers::{AnthropicProvider, CodexCliProvider, OllamaCloudProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = match std::env::var("LLM_USAGE_CONFIG") {
        Ok(p) => Config::load_from(std::path::Path::new(&p))?,
        Err(_) => Config::load_or_default()?,
    };

    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(AnthropicProvider::new(config.anthropic.clone())),
        Box::new(CodexCliProvider::new(config.codex_cli.clone())),
        Box::new(OllamaCloudProvider::new(config.ollama_cloud.clone())),
    ];

    for p in providers {
        if !p.enabled() {
            println!("{:>14}  [disabled]", p.id().human());
            continue;
        }
        match p.poll().await {
            Ok(snap) => {
                println!(
                    "{:>14}  [{:?}] {}",
                    p.id().human(),
                    snap.status,
                    snap.headline.as_deref().unwrap_or("(no headline)"),
                );
                for (label, w) in &snap.windows {
                    let frac = w
                        .fraction_used
                        .map(|f| format!("  {:.0}%", f * 100.0))
                        .unwrap_or_default();
                    let spend = w
                        .spend_usd
                        .map(|s| format!("${:.4}", s))
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "                  {:<5} spend={:>10} in={:>10} out={:>10} req={:>4}{}",
                        label, spend, w.tokens_in, w.tokens_out, w.request_count, frac
                    );
                }
                if let Some(err) = &snap.error {
                    println!("                  ! {}", err);
                }
            }
            Err(e) => println!("{:>14}  ERROR: {}", p.id().human(), e),
        }
    }

    Ok(())
}
