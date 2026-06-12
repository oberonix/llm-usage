//! Google Antigravity quota provider.
//!
//! Antigravity talks to the Cloud Code internal backend. The quota endpoint is
//! a JSON-transcoded RPC:
//!
//! ```text
//! POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary
//! Authorization: Bearer <Google OAuth token with aicode scope>
//! ```
//!
//! The CLI stores auth in the OS keyring; this provider reads that token
//! locally and asks `agy` to refresh it if it has expired. A command hook is
//! available for unusual setups where the keyring lookup is unavailable.

use crate::config::AntigravityConfig;
use crate::model::{ProviderId, ProviderStatus, UsageSnapshot, WindowUsage};
use crate::provider::Provider;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

const QUOTA_PATH: &str = "/v1internal:retrieveUserQuotaSummary";

pub struct AntigravityProvider {
    cfg: AntigravityConfig,
    http: reqwest::Client,
}

impl AntigravityProvider {
    pub fn new(cfg: AntigravityConfig) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("llm-usage/antigravity")
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest");
        Self { cfg, http }
    }

    async fn fetch_quota_json(&self) -> Result<Value> {
        let token = self.resolve_access_token().await?;
        let url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), QUOTA_PATH);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .headers(antigravity_headers())
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(anyhow!("auth rejected ({})", resp.status()));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("quota endpoint {}", resp.status()));
        }
        resp.json().await.context("parse quota JSON")
    }

    async fn resolve_access_token(&self) -> Result<String> {
        if let Ok(Some(tok)) = load_agy_keyring_token() {
            if !tok.is_expired() {
                return Ok(tok.access_token);
            }
            if refresh_agy_keyring_via_cli().is_ok() {
                if let Ok(Some(tok)) = load_agy_keyring_token() {
                    if !tok.is_expired() {
                        return Ok(tok.access_token);
                    }
                }
            }
        }
        let Some(cmd) = self.cfg.access_token_command.as_deref().map(str::trim) else {
            return Err(anyhow!(
                "no Antigravity keyring token found; set antigravity.access_token_command"
            ));
        };
        if cmd.is_empty() {
            return Err(anyhow!("empty access_token_command"));
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .with_context(|| format!("run access_token_command `{}`", cmd))?;
        if !output.status.success() {
            return Err(anyhow!(
                "access_token_command exited with {}",
                output.status
            ));
        }
        let token = String::from_utf8(output.stdout)
            .context("access_token_command stdout was not UTF-8")?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(anyhow!("access_token_command printed no token"));
        }
        Ok(token)
    }
}

fn antigravity_headers() -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("antigravity/2.1.0 darwin/arm64"),
    );
    h.insert(
        "x-goog-api-client",
        reqwest::header::HeaderValue::from_static("google-cloud-sdk vscode_cloudshelleditor/0.1"),
    );
    h.insert(
        "client-metadata",
        reqwest::header::HeaderValue::from_static(
            r#"{"ideType":"ANTIGRAVITY","platform":"MACOS","pluginType":"GEMINI"}"#,
        ),
    );
    h
}

#[derive(Debug, Clone)]
struct AgyToken {
    access_token: String,
    expires_at: Option<DateTime<Utc>>,
}

impl AgyToken {
    fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(t) => Utc::now() + chrono::Duration::seconds(60) >= t,
            None => true,
        }
    }
}

fn load_agy_keyring_token() -> Result<Option<AgyToken>> {
    if std::env::var_os("LLM_USAGE_DISABLE_AGY_KEYRING").is_some() {
        return Ok(None);
    }

    #[cfg(target_os = "macos")]
    let output = Command::new("security")
        .args(["find-generic-password", "-a", "antigravity", "-w"])
        .output();

    #[cfg(not(target_os = "macos"))]
    let output = Command::new("secret-tool")
        .args(["lookup", "service", "gemini", "username", "antigravity"])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };
    let raw = String::from_utf8(output.stdout).context("keyring token was not UTF-8")?;
    parse_agy_token(raw.trim()).map(Some)
}

fn parse_agy_token(raw: &str) -> Result<AgyToken> {
    let json = if let Some(encoded) = raw.strip_prefix("go-keyring-base64:") {
        decode_base64(encoded).context("decode agy keyring token")?
    } else {
        raw.to_string()
    };

    let payload: Value = serde_json::from_str(&json).context("parse agy keyring token")?;
    let token = payload
        .get("token")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("agy keyring token missing token object"))?;
    let access_token = token
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return Err(anyhow!("agy keyring token missing access_token"));
    }
    let expires_at = token
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    Ok(AgyToken {
        access_token,
        expires_at,
    })
}

fn decode_base64(input: &str) -> Result<String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut buf = 0u32;
    let mut bits = 0u8;
    let mut out = Vec::new();
    for b in input.bytes().filter(|b| !b"\r\n\t ".contains(b)) {
        if b == b'=' {
            break;
        }
        let val = TABLE
            .iter()
            .position(|x| *x == b)
            .ok_or_else(|| anyhow!("invalid base64 byte"))? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8(out).context("decoded token was not UTF-8")
}

fn refresh_agy_keyring_via_cli() -> Result<()> {
    let output = Command::new("agy")
        .arg("models")
        .output()
        .context("run `agy models` to refresh Antigravity keyring token")?;
    if !output.status.success() {
        return Err(anyhow!("`agy models` exited with {}", output.status));
    }
    Ok(())
}

#[async_trait]
impl Provider for AntigravityProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Antigravity
    }

    fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    fn subview_labels(&self) -> &'static [&'static str] {
        &["Claude/GPT 5h", "Claude/GPT week"]
    }

    async fn poll(&self) -> Result<UsageSnapshot> {
        let json = match self.fetch_quota_json().await {
            Ok(v) => v,
            Err(e) => {
                return Ok(UsageSnapshot::unavailable(
                    ProviderId::Antigravity,
                    format!("Fetch failed: {}", e),
                ));
            }
        };

        let quotas = parse_quota_summary(&json);
        if quotas.is_empty() {
            return Ok(UsageSnapshot::unavailable(
                ProviderId::Antigravity,
                "Parse failed — quota response contained no Gemini/Claude usage fractions",
            ));
        }

        let now = Utc::now();
        let mut snap = UsageSnapshot {
            provider: ProviderId::Antigravity,
            timestamp: now,
            status: ProviderStatus::Ok,
            error: None,
            windows: Default::default(),
            headline: None,
            plan_label: None,
        };

        for quota in quotas {
            let mut w = WindowUsage {
                started_at: quota.started_at,
                ends_at: quota.ends_at,
                fraction_used: Some(quota.fraction_used),
                ..WindowUsage::default()
            };
            w.mark_stale_if_expired(now);
            snap.windows.insert(quota.label, w);
        }

        let summary = snap
            .windows
            .iter()
            .map(|(label, w)| format!("{} {:.0}%", label, w.fraction_used.unwrap_or(0.0) * 100.0))
            .collect::<Vec<_>>()
            .join(" · ");
        snap.headline = Some(summary);
        Ok(snap)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedQuota {
    label: String,
    fraction_used: f64,
    started_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
}

fn parse_quota_summary(root: &Value) -> Vec<ParsedQuota> {
    let grouped = parse_grouped_quota_summary(root);
    if !grouped.is_empty() {
        return grouped;
    }

    let mut out = Vec::new();
    visit_quota_objects(root, &mut Vec::new(), &mut out);
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out.dedup_by(|a, b| a.label == b.label);
    out
}

fn parse_grouped_quota_summary(root: &Value) -> Vec<ParsedQuota> {
    let Some(groups) = root.get("groups").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for group in groups {
        let group_name = group
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let family = if group_name.contains("gemini") {
            "Gemini"
        } else if group_name.contains("claude") {
            "Claude"
        } else {
            continue;
        };
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else {
            continue;
        };
        for bucket in buckets {
            let Some(obj) = bucket.as_object() else {
                continue;
            };
            let Some(remaining) = field_number(obj, &["remainingFraction"]) else {
                continue;
            };
            let window = obj
                .get("window")
                .and_then(Value::as_str)
                .or_else(|| obj.get("displayName").and_then(Value::as_str))
                .unwrap_or_default()
                .to_ascii_lowercase();
            let window_label = if window.contains("5h") || window.contains("five") {
                "5h"
            } else if window.contains("week") {
                "week"
            } else {
                window.trim()
            };
            if window_label.is_empty() {
                continue;
            }
            out.push(ParsedQuota {
                label: format!("{} {}", family, window_label),
                fraction_used: (1.0 - remaining).clamp(0.0, 10.0),
                started_at: field_time(obj, &["startTime", "startedAt", "windowStartTime"]),
                ends_at: field_time(obj, &["resetTime", "resetAt", "endTime", "endsAt"]),
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

fn visit_quota_objects(value: &Value, path: &mut Vec<String>, out: &mut Vec<ParsedQuota>) {
    match value {
        Value::Object(map) => {
            if let Some(q) = quota_from_object(map, path) {
                out.push(q);
            }
            for (k, v) in map {
                path.push(k.to_ascii_lowercase());
                visit_quota_objects(v, path, out);
                path.pop();
            }
        }
        Value::Array(items) => {
            for item in items {
                visit_quota_objects(item, path, out);
            }
        }
        _ => {}
    }
}

fn quota_from_object(map: &serde_json::Map<String, Value>, path: &[String]) -> Option<ParsedQuota> {
    let fraction = field_number(map, &["fractionUsed", "usedFraction", "usageFraction"])
        .or_else(|| {
            field_number(map, &["usedPercent", "usagePercent", "percentUsed"]).map(|v| v / 100.0)
        })
        .or_else(|| field_number(map, &["remainingFraction"]).map(|v| 1.0 - v))
        .or_else(|| {
            let used = field_number(map, &["used", "current", "consumed"])?;
            let limit = field_number(map, &["limit", "total", "capacity"])?;
            (limit > 0.0).then_some(used / limit)
        })?;

    let label = label_for_quota(map, path)?;
    Some(ParsedQuota {
        label,
        fraction_used: fraction.clamp(0.0, 10.0),
        started_at: field_time(map, &["startTime", "startedAt", "windowStartTime"]),
        ends_at: field_time(
            map,
            &["endTime", "endsAt", "resetTime", "resetAt", "windowEndTime"],
        ),
    })
}

fn label_for_quota(map: &serde_json::Map<String, Value>, path: &[String]) -> Option<String> {
    let haystack = map
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| format!("{} {}", k, s)))
        .chain(path.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();

    let family = if haystack.contains("claude") || haystack.contains("anthropic") {
        "Claude"
    } else if haystack.contains("gemini") || haystack.contains("google") {
        "Gemini"
    } else {
        return None;
    };

    let window = if haystack.contains("5h")
        || haystack.contains("5 h")
        || haystack.contains("five")
        || haystack.contains("rolling")
    {
        " 5h"
    } else if haystack.contains("week") {
        " week"
    } else if haystack.contains("day") || haystack.contains("daily") {
        " day"
    } else {
        ""
    };
    Some(format!("{}{}", family, window))
}

fn field_number(map: &serde_json::Map<String, Value>, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        map.get(*name)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()))
    })
}

fn field_time(map: &serde_json::Map<String, Value>, names: &[&str]) -> Option<DateTime<Utc>> {
    names.iter().find_map(|name| {
        let v = map.get(*name)?;
        if let Some(s) = v.as_str() {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        } else {
            let n = v.as_i64()?;
            if n > 10_000_000_000 {
                Utc.timestamp_millis_opt(n).single()
            } else {
                Utc.timestamp_opt(n, 0).single()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_family_quota_objects() {
        let payload = serde_json::json!({
            "quotaSummaries": [
                {
                    "modelFamily": "GEMINI",
                    "window": "FIVE_HOURS",
                    "usedPercent": 42.0,
                    "resetTime": "2026-06-12T20:00:00Z"
                },
                {
                    "modelFamily": "CLAUDE",
                    "window": "FIVE_HOURS",
                    "fractionUsed": 0.67,
                    "resetTime": 1781294400
                }
            ]
        });

        let parsed = parse_quota_summary(&payload);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].label, "Claude 5h");
        assert!((parsed[0].fraction_used - 0.67).abs() < f64::EPSILON);
        assert_eq!(parsed[1].label, "Gemini 5h");
        assert!((parsed[1].fraction_used - 0.42).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_live_grouped_quota_shape() {
        let payload = serde_json::json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {"window": "weekly", "resetTime": "2026-06-18T23:55:04Z", "remainingFraction": 0.8181425},
                        {"window": "5h", "resetTime": "2026-06-12T22:34:19Z", "remainingFraction": 0.003725}
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {"window": "weekly", "resetTime": "2026-06-18T23:57:55Z", "remainingFraction": 0.74540466},
                        {"window": "5h", "resetTime": "2026-06-12T23:33:05Z", "remainingFraction": 0.2746008}
                    ]
                }
            ]
        });

        let parsed = parse_quota_summary(&payload);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].label, "Claude 5h");
        assert!((parsed[0].fraction_used - 0.7253992).abs() < 1e-9);
        assert_eq!(parsed[1].label, "Claude week");
        assert_eq!(parsed[2].label, "Gemini 5h");
        assert!((parsed[2].fraction_used - 0.996275).abs() < 1e-9);
        assert_eq!(parsed[3].label, "Gemini week");
    }

    #[test]
    fn parses_linux_agy_keyring_json() {
        let tok = parse_agy_token(
            r#"{"token":{"access_token":"access","refresh_token":"refresh","expiry":"2026-06-12T16:20:20.332769614-07:00"}}"#,
        )
        .unwrap();
        assert_eq!(tok.access_token, "access");
        assert!(tok.expires_at.is_some());
    }
}
