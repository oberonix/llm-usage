//! Ollama local — polls the running daemon for loaded models. No spend.
//! Endpoint: http://localhost:11434/api/ps

use crate::config::OllamaLocalConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;

pub struct OllamaLocalProvider {
    cfg: OllamaLocalConfig,
    client: reqwest::Client,
}

impl OllamaLocalProvider {
    pub fn new(cfg: OllamaLocalConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("reqwest");
        Self { cfg, client }
    }
}

#[async_trait]
impl Provider for OllamaLocalProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OllamaLocal
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        let url = format!("{}/api/ps", self.cfg.base_url.trim_end_matches('/'));
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(UsageSnapshot {
                    provider: ProviderId::OllamaLocal,
                    timestamp: Utc::now(),
                    status: ProviderStatus::Unavailable,
                    error: Some(format!("ollama daemon unreachable: {}", e)),
                    windows: Default::default(),
                    headline: Some("offline".into()),
                });
            }
        };
        if !resp.status().is_success() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::OllamaLocal,
                format!("status {}", resp.status()),
            ));
        }
        let body: PsResponse = resp.json().await?;
        let loaded = body.models.len();
        let names: Vec<String> = body.models.iter().map(|m| m.name.clone()).collect();

        Ok(UsageSnapshot {
            provider: ProviderId::OllamaLocal,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: Some(if loaded == 0 {
                "idle".to_string()
            } else {
                format!("{} model{} loaded: {}", loaded, if loaded == 1 { "" } else { "s" }, names.join(", "))
            }),
        })
    }
}

#[derive(Debug, Deserialize)]
struct PsResponse {
    #[serde(default)]
    models: Vec<PsModel>,
}

#[derive(Debug, Deserialize)]
struct PsModel {
    name: String,
}
