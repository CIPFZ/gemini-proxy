//! Simple Token Manager
//! Single-account token storage with auto-refresh

use crate::config::ProxyConfig;
use crate::oauth::{ensure_fresh_token, load_token, save_token, TokenData};
use std::sync::Arc;

/// Simple token manager for single account
pub struct TokenManager {
    token: Arc<tokio::sync::RwLock<Option<TokenData>>>,
}

impl TokenManager {
    pub fn new() -> Self {
        let token = load_token().ok().flatten();
        if token.is_some() {
            tracing::info!("Token loaded from storage");
        } else {
            tracing::debug!("No token found in storage");
        }
        Self {
            token: Arc::new(tokio::sync::RwLock::new(token)),
        }
    }

    /// Set token after login
    pub fn set_token(&self, token: TokenData) -> anyhow::Result<()> {
        tracing::info!("Saving token to storage");
        save_token(&token)?;
        let mut guard = self.token.blocking_write();
        *guard = Some(token);
        Ok(())
    }

    /// Get current token (no refresh) - async version
    pub async fn get_token(&self) -> Option<TokenData> {
        self.token.read().await.clone()
    }

    /// Get fresh token (auto-refresh if expired)
    pub async fn get_fresh_token(&self, proxy: &Option<ProxyConfig>) -> anyhow::Result<String> {
        let token = {
            let guard = self.token.read().await;
            guard.clone()
        };

        let token = token.ok_or_else(|| anyhow::anyhow!("No token available. Please login first."))?;

        // Check and refresh if needed
        tracing::debug!("Getting fresh token, expiry: {}", token.expiry_timestamp);
        let fresh = ensure_fresh_token(&token, proxy).await?;

        // Save if refreshed
        if fresh.access_token != token.access_token {
            tracing::info!("Token refreshed, saving to storage");
            save_token(&fresh)?;
            let mut guard = self.token.write().await;
            *guard = Some(fresh.clone());
        }

        Ok(fresh.access_token)
    }

    /// Check if logged in - async
    pub async fn is_logged_in(&self) -> bool {
        self.token.read().await.is_some()
    }

    /// Clear token (logout) - async
    pub async fn clear_token(&self) -> anyhow::Result<()> {
        let path = crate::config::get_token_path();
        tracing::info!("Clearing token from storage");
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let mut guard = self.token.write().await;
        *guard = None;
        Ok(())
    }
}

impl Default for TokenManager {
    fn default() -> Self {
        Self::new()
    }
}
