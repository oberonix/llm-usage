//! OpenAI API usage. Polls /v1/usage (official) and /v1/dashboard/billing/usage
//! (unofficial — used by the OpenAI dashboard, periodically broken).
//!
//! Both endpoints take a date range (UTC). We pull month-to-date.

use crate::config::OpenAiConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowKind};
use crate::provider::Provider;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Datelike, Utc};
use serde::Deserialize;

pub struct OpenAiProvider {
    cfg: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(cfg: OpenAiConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self { cfg, client }
    }

    fn api_key(&self) -> Option<String> {
        self.cfg
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
    }

    async fn fetch_billing(&self, key: &str) -> Result<f64> {
        let now = Utc::now();
        let start = now.with_day(1).unwrap_or(now).format("%Y-%m-%d").to_string();
        // OpenAI's /dashboard/billing/usage requires start_date and end_date.
        let end = (now + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let url = format!(
            "https://api.openai.com/v1/dashboard/billing/usage?start_date={}&end_date={}",
            start, end
        );
        let mut req = self.client.get(&url).bearer_auth(key);
        if let Some(org) = &self.cfg.organization {
            req = req.header("OpenAI-Organization", org);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("billing endpoint {}", resp.status());
        }
        let body: BillingResponse = resp.json().await?;
        // total_usage is reported in cents.
        Ok(body.total_usage / 100.0)
    }
}

#[derive(Debug, Deserialize)]
struct BillingResponse {
    total_usage: f64,
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenAi
    }
    fn enabled(&self) -> bool {
        self.cfg.enabled
    }
    async fn poll(&self) -> Result<UsageSnapshot> {
        if !self.cfg.show_spend {
            // OpenAI's only data source is dollar spend; with that hidden
            // there's nothing to display. Skip the network call entirely.
            return Ok(UsageSnapshot::unavailable(
                ProviderId::OpenAi,
                "spend tracking hidden — set [openai].show_spend = true",
            ));
        }
        let Some(key) = self.api_key() else {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::OpenAi,
                "no API key (set [openai].api_key or OPENAI_API_KEY)",
            ));
        };

        let mut snap = UsageSnapshot {
            provider: ProviderId::OpenAi,
            timestamp: Utc::now(),
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
        };

        match self.fetch_billing(&key).await {
            Ok(spend) => {
                let month = snap.window_mut(WindowKind::ThisMonth);
                month.spend_usd = Some(spend);
                month.limit_usd = self.cfg.monthly_budget_usd;
                month.recompute_fraction();
                snap.headline = Some(format!("${:.2} this month", spend));
            }
            Err(err) => {
                snap.status = ProviderStatus::Degraded;
                snap.error = Some(format!("billing endpoint unavailable: {}", err));
                snap.headline = Some("billing endpoint unavailable".into());
            }
        }
        Ok(snap)
    }
}
