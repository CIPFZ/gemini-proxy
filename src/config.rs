//! Configuration module for gemini-proxy

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Proxy configuration for upstream (出口代理)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Enable proxy
    pub enabled: bool,
    /// Proxy URL (http://, https://, socks5://)
    pub url: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
        }
    }
}

/// OAuth client configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Client key (default: "antigravity_enterprise")
    pub client_key: String,
    /// OAuth redirect URI
    pub redirect_uri: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            client_key: "antigravity_enterprise".to_string(),
            redirect_uri: "http://localhost:8045/callback".to_string(),
        }
    }
}

/// Main configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Bind address for the proxy server
    pub bind: String,
    /// Optional API key for authentication
    pub api_key: Option<String>,
    /// Maximum number of concurrent requests
    pub max_concurrent_requests: usize,
    /// Upstream proxy configuration
    pub upstream_proxy: ProxyConfig,
    /// OAuth configuration
    pub oauth: OAuthConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8045".to_string(),
            api_key: None,
            max_concurrent_requests: 16,
            upstream_proxy: ProxyConfig::default(),
            oauth: OAuthConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from file
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Resolve environment placeholders in-place.
    pub fn resolve_env(&mut self) {
        self.bind = expand_env_vars(&self.bind);
        self.api_key = self.api_key.as_ref().map(|v| expand_env_vars(v)).and_then(|v| if v.is_empty() { None } else { Some(v) });
        self.upstream_proxy.url = expand_env_vars(&self.upstream_proxy.url);
        self.oauth.client_key = expand_env_vars(&self.oauth.client_key);
        self.oauth.redirect_uri = expand_env_vars(&self.oauth.redirect_uri);
    }

    /// Return a resolved copy with environment placeholders expanded.
    pub fn resolved(mut self) -> Self {
        self.resolve_env();
        self
    }

    /// Save configuration to file
    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

/// Get default config path: ~/.gemini-proxy/config.json
pub fn get_default_config_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini-proxy").join("config.json")
}

/// Get token storage path: ~/.gemini-proxy/token.json
pub fn get_token_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini-proxy").join("token.json")
}

/// Get PID file path: ~/.gemini-proxy/gemini-proxy.pid
pub fn get_pid_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini-proxy").join("gemini-proxy.pid")
}

fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            while let Some(next) = chars.next() {
                if next == '}' {
                    break;
                }
                name.push(next);
            }
            if name.is_empty() {
                out.push_str("${}");
            } else {
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
        } else {
            out.push(ch);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_environment_variables_in_config_values() {
        std::env::set_var("CCP_TEST_BIND", "127.0.0.1:9000");
        std::env::set_var("CCP_TEST_API_KEY", "secret");
        std::env::set_var("CCP_TEST_PROXY", "http://127.0.0.1:7897");

        let mut config = Config {
            bind: "${CCP_TEST_BIND}".to_string(),
            api_key: Some("${CCP_TEST_API_KEY}".to_string()),
            max_concurrent_requests: 8,
            upstream_proxy: ProxyConfig {
                enabled: true,
                url: "${CCP_TEST_PROXY}".to_string(),
            },
            oauth: OAuthConfig {
                client_key: "${CCP_TEST_API_KEY}".to_string(),
                redirect_uri: "http://${CCP_TEST_BIND}/callback".to_string(),
            },
        };

        config.resolve_env();

        assert_eq!(config.bind, "127.0.0.1:9000");
        assert_eq!(config.api_key.as_deref(), Some("secret"));
        assert_eq!(config.upstream_proxy.url, "http://127.0.0.1:7897");
        assert_eq!(config.oauth.redirect_uri, "http://127.0.0.1:9000/callback");
    }

    #[test]
    fn default_concurrency_is_reasonable() {
        assert!(Config::default().max_concurrent_requests >= 1);
    }
}
