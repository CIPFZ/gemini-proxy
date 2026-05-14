//! Quota fetching module
//! Fetches available models and配额 from Google Gemini API

use crate::config::ProxyConfig;
use reqwest::header;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const NATIVE_OAUTH_USER_AGENT: &str = "vscode/1.X.X (Antigravity/4.1.31)";

const CLOUD_CODE_BASE_URL: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";

// Quota API endpoints (fallback order: Sandbox → Daily → Prod) - from AntigravityManager
const QUOTA_API_ENDPOINTS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:fetchAvailableModels",
    "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
    "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
];

/// Build HTTP client with optional proxy support
fn build_client(proxy: &Option<ProxyConfig>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(NATIVE_OAUTH_USER_AGENT)
        .timeout(Duration::from_secs(15));

    if let Some(p) = proxy {
        if p.enabled && !p.url.is_empty() {
            let proxy_url = reqwest::Proxy::all(&p.url)
                .map_err(|e| anyhow::anyhow!("Invalid proxy URL: {}", e))?;
            builder = builder.proxy(proxy_url);
        }
    }

    builder.build().map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))
}

/// Load project response
#[derive(Debug, Deserialize)]
struct LoadProjectResponse {
    #[serde(rename = "cloudaicompanionProject")]
    project_id: Option<String>,
    #[serde(rename = "currentTier")]
    current_tier: Option<Tier>,
    #[serde(rename = "paidTier")]
    paid_tier: Option<Tier>,
}

#[derive(Debug, Deserialize)]
struct Tier {
    id: Option<String>,
    name: Option<String>,
}

/// Model info from quota API
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub percentage: i32,
    pub reset_time: Option<String>,
    pub display_name: Option<String>,
    pub supports_images: Option<bool>,
    pub supports_thinking: Option<bool>,
    pub thinking_budget: Option<i32>,
    pub max_tokens: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct QuotaResponse {
    models: std::collections::HashMap<String, ModelInfoResponse>,
}

#[derive(Debug, Deserialize)]
struct ModelInfoResponse {
    #[serde(rename = "quotaInfo")]
    quota_info: Option<QuotaInfo>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "supportsImages")]
    supports_images: Option<bool>,
    #[serde(rename = "supportsThinking")]
    supports_thinking: Option<bool>,
    #[serde(rename = "thinkingBudget")]
    thinking_budget: Option<i32>,
    #[serde(rename = "maxTokens")]
    max_tokens: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct QuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

/// Fetch project ID and subscription tier
pub async fn fetch_project_id(access_token: &str, proxy: &Option<ProxyConfig>) -> anyhow::Result<(Option<String>, Option<String>)> {
    let client = build_client(proxy)?;

    let meta = serde_json::json!({"metadata": {"ideType": "ANTIGRAVITY"}});

    let res = client
        .post(format!("{}/v1internal:loadCodeAssist", CLOUD_CODE_BASE_URL))
        .header(header::AUTHORIZATION, format!("Bearer {}", access_token))
        .header(header::CONTENT_TYPE, "application/json")
        .json(&meta)
        .send()
        .await?;

    if res.status().is_success() {
        let data: LoadProjectResponse = res.json().await?;
        let project_id = data.project_id.clone();

        // Multi-level fallback for tier extraction
        let subscription_tier = data
            .paid_tier
            .as_ref()
            .and_then(|t| t.name.clone().or_else(|| t.id.clone()))
            .or_else(|| {
                data.current_tier
                    .as_ref()
                    .and_then(|t| t.name.clone().or_else(|| t.id.clone()))
            });

        tracing::info!(
            "Project ID: {:?}, Subscription tier: {:?}",
            project_id,
            subscription_tier
        );

        return Ok((project_id, subscription_tier));
    }

    Ok((None, None))
}

/// Fetch quota (available models) using the given access token
pub async fn fetch_quota(
    access_token: &str,
    project_id: Option<&str>,
    proxy: &Option<ProxyConfig>,
) -> anyhow::Result<Vec<ModelInfo>> {
    let client = build_client(proxy)?;

    // Build payload with project_id like the original implementation
    let payload = if let Some(pid) = project_id {
        serde_json::json!({ "project": pid })
    } else {
        serde_json::json!({})
    };

    tracing::info!("Fetching quota from upstream, project_id: {:?}", project_id);

    let mut last_error: Option<String> = None;

    // Try each endpoint in fallback order
    for (ep_idx, ep_url) in QUOTA_API_ENDPOINTS.iter().enumerate() {
        let has_next = ep_idx + 1 < QUOTA_API_ENDPOINTS.len();

        tracing::info!("Trying quota endpoint #{}: {}", ep_idx + 1, ep_url);

        match client
            .post(*ep_url)
            .header(header::AUTHORIZATION, format!("Bearer {}", access_token))
            .header(header::USER_AGENT, NATIVE_OAUTH_USER_AGENT)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();

                // Handle HTTP errors
                if let Err(_e) = response.error_for_status_ref() {
                    let text = response.text().await.unwrap_or_default();

                    // 403: mark as forbidden, return empty
                    if status.as_u16() == 403 {
                        tracing::error!("Quota API returned 403 Forbidden - account may not have access");
                        return Ok(Vec::new());
                    }

                    // 429/5xx: try next endpoint
                    if has_next && (status.as_u16() == 429 || status.is_server_error()) {
                        tracing::warn!("Quota API {} returned {}, trying next endpoint", ep_url, status);
                        last_error = Some(format!("HTTP {} - {}", status, text));
                        continue;
                    }

                    return Err(anyhow::anyhow!("Quota API error: {} - {}", status, text));
                }

                if ep_idx > 0 {
                    tracing::info!("Quota API fallback succeeded at endpoint #{}", ep_idx + 1);
                }

                let json: serde_json::Value = response.json().await
                    .map_err(|e| anyhow::anyhow!("Parse JSON failed: {}", e))?;

                tracing::debug!("Quota response: {}", serde_json::to_string_pretty(&json).unwrap_or_default());

                // Check if response contains error
                if json.get("error").is_some() {
                    let error_msg = json.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown error");
                    tracing::error!("Quota API error: {}", error_msg);
                    anyhow::bail!("Quota API error: {}", error_msg);
                }

                // Parse the response
                let quota_response: QuotaResponse = serde_json::from_value(json)
                    .map_err(|e| anyhow::anyhow!("Failed to parse quota response: {}", e))?;

                let mut models = Vec::new();

                for (name, info) in quota_response.models {
                    // Only keep models we care about
                    if name.starts_with("gemini") || name.starts_with("claude") || name.starts_with("gpt") || name.starts_with("image") || name.starts_with("imagen") {
                        if let Some(quota_info) = info.quota_info {
                            let percentage = quota_info
                                .remaining_fraction
                                .map(|f| (f * 100.0) as i32)
                                .unwrap_or(0);

                            models.push(ModelInfo {
                                name,
                                percentage,
                                reset_time: quota_info.reset_time,
                                display_name: info.display_name,
                                supports_images: info.supports_images,
                                supports_thinking: info.supports_thinking,
                                thinking_budget: info.thinking_budget,
                                max_tokens: info.max_tokens,
                            });
                        }
                    }
                }

                tracing::info!("Quota returned {} models", models.len());
                return Ok(models);
            }
            Err(e) => {
                tracing::warn!("Quota API request failed at {}: {}", ep_url, e);
                last_error = Some(e.to_string());
                if has_next {
                    continue;
                }
            }
        }
    }

    Err(anyhow::anyhow!("Quota fetch failed: {}", last_error.unwrap_or_else(|| "all endpoints exhausted".to_string())))
}
