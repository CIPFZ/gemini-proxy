//! Google OAuth module
//! Matches Antigravity-Manager OAuth implementation

use crate::config::ProxyConfig;
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// Google OAuth configuration
const CLIENT_ID_ENV: &str = "GEMINI_PROXY_GOOGLE_CLIENT_ID";
const CLIENT_SECRET_ENV: &str = "GEMINI_PROXY_GOOGLE_CLIENT_SECRET";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_REFRESH_SKEW_SECONDS: i64 = 900;

/// Native OAuth User-Agent — matching AntigravityManager
const NATIVE_OAUTH_USER_AGENT: &str = "vscode/1.X.X (Antigravity/4.1.31)";

/// Build HTTP client with optional proxy support (60s timeout, matching Antigravity)
fn build_client(proxy: &Option<ProxyConfig>) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(60));

    if let Some(p) = proxy {
        if p.enabled && !p.url.is_empty() {
            let proxy_url = reqwest::Proxy::all(&p.url)
                .map_err(|e| anyhow::anyhow!("Invalid proxy URL: {}", e))?;
            builder = builder.proxy(proxy_url);
            tracing::info!("OAuth using proxy: {}", p.url);
        }
    }

    builder.build().map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))
}

fn client_id(oauth_config: Option<&crate::config::OAuthConfig>) -> anyhow::Result<String> {
    let from_env = std::env::var(CLIENT_ID_ENV)
        .or_else(|_| std::env::var("GEMINI_PROXY_CLIENT_ID"))
        .ok();
    if let Some(ref id) = from_env {
        if !id.is_empty() {
            return Ok(id.clone());
        }
    }
    // Fallback to config file
    if let Some(cfg) = oauth_config {
        if let Some(ref id) = cfg.client_id {
            if !id.is_empty() {
                return Ok(id.clone());
            }
        }
    }
    anyhow::bail!("No OAuth client ID configured. Set GEMINI_PROXY_GOOGLE_CLIENT_ID env var or add oauth.client_id to config.json");
}

fn client_secret(oauth_config: Option<&crate::config::OAuthConfig>) -> anyhow::Result<String> {
    let from_env = std::env::var(CLIENT_SECRET_ENV)
        .or_else(|_| std::env::var("GEMINI_PROXY_CLIENT_SECRET"))
        .ok();
    if let Some(ref s) = from_env {
        if !s.is_empty() {
            return Ok(s.clone());
        }
    }
    // Fallback to config file
    if let Some(cfg) = oauth_config {
        if let Some(ref s) = cfg.client_secret {
            if !s.is_empty() {
                return Ok(s.clone());
            }
        }
    }
    anyhow::bail!("No OAuth client secret configured. Set GEMINI_PROXY_GOOGLE_CLIENT_SECRET env var or add oauth.client_secret to config.json");
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserInfo {
    pub email: String,
    pub name: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub expiry_timestamp: i64,
    pub email: Option<String>,
    pub project_id: Option<String>,
}

impl TokenData {
    pub fn new(
        access_token: String,
        refresh_token: String,
        expires_in: i64,
        email: Option<String>,
        project_id: Option<String>,
    ) -> Self {
        let expiry_timestamp = chrono::Local::now().timestamp() + expires_in;
        Self {
            access_token,
            refresh_token,
            expires_in,
            expiry_timestamp,
            email,
            project_id,
        }
    }

    pub fn is_expired(&self) -> bool {
        chrono::Local::now().timestamp() > self.expiry_timestamp - TOKEN_REFRESH_SKEW_SECONDS
    }
}

impl std::fmt::Display for TokenData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TokenData(email={}, expires_in={})",
            self.email.as_deref().unwrap_or("N/A"),
            self.expires_in
        )
    }
}

/// Generate OAuth authorization URL
pub fn get_auth_url(redirect_uri: &str, state: &str, oauth_config: &crate::config::OAuthConfig) -> anyhow::Result<String> {
    let client_id = client_id(Some(oauth_config))?;
    let scopes = vec![
        "openid",
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/userinfo.email",
        "https://www.googleapis.com/auth/userinfo.profile",
        "https://www.googleapis.com/auth/cclog",
        "https://www.googleapis.com/auth/experimentsandconfigs",
    ]
    .join(" ");

    let params = vec![
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", &scopes),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("include_granted_scopes", "true"),
        ("state", state),
    ];

    let url = url::Url::parse_with_params(AUTH_URL, &params)
        .map_err(|e| anyhow::anyhow!("Invalid Auth URL: {}", e))?;
    Ok(url.to_string())
}

/// Exchange authorization code for token
pub async fn exchange_code(code: &str, redirect_uri: &str, proxy: &Option<ProxyConfig>, oauth_config: &crate::config::OAuthConfig) -> anyhow::Result<TokenResponse> {
    let client = build_client(proxy)?;
    let client_id = client_id(Some(oauth_config))?;
    let client_secret = client_secret(Some(oauth_config))?;

    let params = [
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ];

    tracing::info!("Exchanging authorization code for token...");

    let response = client
        .post(TOKEN_URL)
        .header(header::USER_AGENT, NATIVE_OAUTH_USER_AGENT)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                tracing::error!(
                    "Token exchange connection/timeout: {}. Check proxy/network settings.",
                    e
                );
                anyhow::anyhow!(
                    "Token exchange request failed: {}. 请检查你的网络代理设置，确保可以稳定连接 Google 服务。",
                    e
                )
            } else {
                tracing::error!("Token exchange request failed: {}", e);
                anyhow::anyhow!("Token exchange request failed: {}", e)
            }
        })?;

    if response.status().is_success() {
        let token_res = response
            .json::<TokenResponse>()
            .await
            .map_err(|e| anyhow::anyhow!("Token parsing failed: {}", e))?;

        tracing::info!(
            "Token exchange successful! access_token: {}..., refresh_token: {}",
            &token_res.access_token.chars().take(20).collect::<String>(),
            if token_res.refresh_token.is_some() { "✓" } else { "✗ Missing" }
        );

        if token_res.refresh_token.is_none() {
            tracing::warn!(
                "Warning: Google did not return a refresh_token. Potential reasons:\n\
                 1. User has previously authorized this application\n\
                 2. Need to revoke access in Google Cloud Console and retry"
            );
        }

        Ok(token_res)
    } else {
        let error_text = response.text().await.unwrap_or_default();
        Err(anyhow::anyhow!("Token exchange failed: {}", error_text))
    }
}

/// Refresh access token
pub async fn refresh_access_token(refresh_token: &str, proxy: &Option<ProxyConfig>, oauth_config: &crate::config::OAuthConfig) -> anyhow::Result<TokenResponse> {
    let client = build_client(proxy)?;
    let client_id = client_id(Some(oauth_config))?;
    let client_secret = client_secret(Some(oauth_config))?;

    let params = [
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];

    tracing::info!("Refreshing access token...");

    let response = client
        .post(TOKEN_URL)
        .header(header::USER_AGENT, NATIVE_OAUTH_USER_AGENT)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                anyhow::anyhow!(
                    "Refresh request failed: {}. 请检查你的网络代理设置，确保可以稳定连接 Google 服务。",
                    e
                )
            } else {
                anyhow::anyhow!("Refresh request failed: {}", e)
            }
        })?;

    if response.status().is_success() {
        let token_res = response
            .json::<TokenResponse>()
            .await
            .map_err(|e| anyhow::anyhow!("Refresh data parsing failed: {}", e))?;

        tracing::info!(
            "Token refreshed successfully! Expires in: {} seconds",
            token_res.expires_in
        );
        Ok(token_res)
    } else {
        let error_text = response.text().await.unwrap_or_default();
        Err(anyhow::anyhow!("Refresh failed: {}", error_text))
    }
}

/// Get user info
pub async fn get_user_info(access_token: &str, proxy: &Option<ProxyConfig>) -> anyhow::Result<UserInfo> {
    let client = build_client(proxy)?;

    let response = client
        .get(USERINFO_URL)
        .header(header::AUTHORIZATION, format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("User info request failed: {}", e))?;

    if response.status().is_success() {
        response
            .json::<UserInfo>()
            .await
            .map_err(|e| anyhow::anyhow!("User info parsing failed: {}", e))
    } else {
        let error_text = response.text().await.unwrap_or_default();
        Err(anyhow::anyhow!("Failed to get user info: {}", error_text))
    }
}

/// Ensure we have a fresh token, refresh if needed
pub async fn ensure_fresh_token(current: &TokenData, proxy: &Option<ProxyConfig>, oauth_config: &crate::config::OAuthConfig) -> anyhow::Result<TokenData> {
    let now = chrono::Local::now().timestamp();

    // Keep enough validity to avoid immediate post-switch refresh failure.
    if current.expiry_timestamp > now + TOKEN_REFRESH_SKEW_SECONDS {
        return Ok(current.clone());
    }

    tracing::info!("Token expiring soon, refreshing...");

    let response = refresh_access_token(&current.refresh_token, proxy, oauth_config).await?;

    // Construct new TokenData
    Ok(TokenData::new(
        response.access_token,
        current.refresh_token.clone(), // refresh_token may not be returned on refresh
        response.expires_in,
        current.email.clone(),
        current.project_id.clone(),
    ))
}

/// Save token to file
pub fn save_token(token: &TokenData) -> anyhow::Result<()> {
    let path = crate::config::get_token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(token)?;
    std::fs::write(&path, content)?;
    tracing::info!("Token saved to {:?}", path);
    Ok(())
}

/// Load token from file
pub fn load_token() -> anyhow::Result<Option<TokenData>> {
    let path = crate::config::get_token_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let token: TokenData = serde_json::from_str(&content)?;
    Ok(Some(token))
}
