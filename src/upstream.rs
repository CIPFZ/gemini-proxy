//! Upstream client for Google Gemini API
//! With endpoint fallback and Antigravity-compatible identity headers

use crate::config::ProxyConfig;
use reqwest::{header, Client, Response, StatusCode};
use serde_json::Value;
use std::time::Duration;

// User-Agent for upstream requests — generic Chrome UA to avoid triggering
// geo-restrictions or per-client rate limits
const UPSTREAM_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";

// Cloud Code v1internal endpoints (fallback order: Daily → Prod → Sandbox)
const V1_INTERNAL_BASE_URL_PROD: &str = "https://cloudcode-pa.googleapis.com/v1internal";
const V1_INTERNAL_BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
const V1_INTERNAL_BASE_URL_SANDBOX: &str =
    "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal";

const V1_INTERNAL_BASE_URL_FALLBACKS: [&str; 3] = [
    V1_INTERNAL_BASE_URL_DAILY,   // Priority 1: Daily (works for Gemini)
    V1_INTERNAL_BASE_URL_PROD,    // Priority 2: Prod (backup)
    V1_INTERNAL_BASE_URL_SANDBOX,  // Priority 3: Sandbox (last resort)
];

/// Upstream client wrapper
pub struct UpstreamClient {
    client: Client,
}

impl UpstreamClient {
    /// Get reference to the HTTP client
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Create new upstream client with optional proxy
    pub fn new(proxy_config: Option<ProxyConfig>) -> anyhow::Result<Self> {
        let mut builder = Client::builder()
            .user_agent(UPSTREAM_USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(90));

        // Apply proxy if enabled
        if let Some(config) = proxy_config {
            if config.enabled && !config.url.is_empty() {
                builder = builder.proxy(reqwest::Proxy::all(&config.url)?);
                tracing::info!("Upstream proxy enabled: {}", config.url);
            }
        }

        let client = builder.build()?;
        Ok(Self { client })
    }

    /// Build v1internal URL
    fn build_url(base_url: &str, method: &str, query_string: Option<&str>) -> String {
        if method.starts_with('/') {
            // Full path provided
            if let Some(qs) = query_string {
                format!("{}{}?{}", base_url, method, qs)
            } else {
                format!("{}{}", base_url, method)
            }
        } else {
            // Method name only - append with colon
            if let Some(qs) = query_string {
                format!("{}:{}?{}", base_url, method, qs)
            } else {
                format!("{}:{}", base_url, method)
            }
        }
    }

    /// Determine if we should try next endpoint (fallback logic)
    fn should_try_next_endpoint(status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::NOT_FOUND
            || status.is_server_error()
    }

    /// Call v1internal API with automatic endpoint fallback
    pub async fn call_v1_internal(
        &self,
        method: &str,
        access_token: &str,
        body: Value,
        query_string: Option<&str>,
    ) -> anyhow::Result<Response> {
        // Build headers
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {}", access_token))
                .map_err(|e| anyhow::anyhow!("{}", e))?,
        );
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(UPSTREAM_USER_AGENT),
        );

        // NOTE: x-client-name and x-client-version intentionally omitted —
        // they trigger per-client rate limits and geo-restrictions on Gemini API

        let mut last_err: Option<String> = None;

        // Try endpoints in fallback order
        for (idx, base_url) in V1_INTERNAL_BASE_URL_FALLBACKS.iter().enumerate() {
            let url = Self::build_url(base_url, method, query_string);
            let has_next = idx + 1 < V1_INTERNAL_BASE_URL_FALLBACKS.len();

            tracing::info!("Trying endpoint: {} (full URL: {})", base_url, url);

            let response = self
                .client
                .post(&url)
                .headers(headers.clone())
                .json(&body)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        if idx > 0 {
                            tracing::info!(
                                "Endpoint fallback succeeded at #{}: {}",
                                idx + 1,
                                base_url
                            );
                        }
                        return Ok(resp);
                    }

                    // If has next endpoint and error is retryable, try next
                    if has_next && Self::should_try_next_endpoint(status) {
                        let err_msg = format!("Endpoint {} returned {}", base_url, status);
                        tracing::warn!("{}", err_msg);
                        last_err = Some(err_msg);
                        continue;
                    }

                    // Return error response as-is
                    return Ok(resp);
                }
                Err(e) => {
                    let msg = format!("HTTP request failed at {}: {}", base_url, e);
                    tracing::warn!("{}", msg);
                    last_err = Some(msg);

                    if !has_next {
                        break;
                    }
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!(last_err.unwrap_or_else(|| "All endpoints failed".to_string())))
    }

    /// Call v1internal API with extra headers
    pub async fn call_v1_internal_with_headers(
        &self,
        method: &str,
        access_token: &str,
        body: Value,
        query_string: Option<&str>,
        extra_headers: std::collections::HashMap<String, String>,
    ) -> anyhow::Result<Response> {
        // Build headers
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {}", access_token))
                .map_err(|e| anyhow::anyhow!("{}", e))?,
        );
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(UPSTREAM_USER_AGENT),
        );

        // NOTE: x-client-name and x-client-version intentionally omitted —
        // they trigger per-client rate limits and geo-restrictions on Gemini API

        // Add extra headers
        for (k, v) in extra_headers {
            if let Ok(hk) = header::HeaderName::from_bytes(k.as_bytes()) {
                if let Ok(hv) = header::HeaderValue::from_str(&v) {
                    headers.insert(hk, hv);
                }
            }
        }

        let mut last_err: Option<String> = None;

        for (idx, base_url) in V1_INTERNAL_BASE_URL_FALLBACKS.iter().enumerate() {
            let url = Self::build_url(base_url, method, query_string);
            let has_next = idx + 1 < V1_INTERNAL_BASE_URL_FALLBACKS.len();

            let response = self
                .client
                .post(&url)
                .headers(headers.clone())
                .json(&body)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        if idx > 0 {
                            tracing::info!(
                                "Endpoint fallback succeeded at #{}: {}",
                                idx + 1,
                                base_url
                            );
                        }
                        return Ok(resp);
                    }

                    if has_next && Self::should_try_next_endpoint(status) {
                        let err_msg = format!("Endpoint {} returned {}", base_url, status);
                        tracing::warn!("{}", err_msg);
                        last_err = Some(err_msg);
                        continue;
                    }

                    return Ok(resp);
                }
                Err(e) => {
                    let msg = format!("HTTP request failed at {}: {}", base_url, e);
                    tracing::warn!("{}", msg);
                    last_err = Some(msg);

                    if !has_next {
                        break;
                    }
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!(last_err.unwrap_or_else(|| "All endpoints failed".to_string())))
    }

    /// Fetch available models
    pub async fn fetch_available_models(
        &self,
        access_token: &str,
    ) -> anyhow::Result<Value> {
        let result = self
            .call_v1_internal(
                "fetchAvailableModels",
                access_token,
                serde_json::json!({}),
                None,
            )
            .await?;

        let json: Value = result
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Parse JSON failed: {}", e))?;
        Ok(json)
    }
}
