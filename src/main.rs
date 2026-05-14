//! Gemini Proxy - Lightweight Google Gemini API reverse proxy
//!
//! A simple, lightweight tool to proxy Google Gemini API with your own OAuth credentials.

mod config;
mod oauth;
mod proxy;
mod quota;
mod openai;
mod token;
mod upstream;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Wait for OAuth callback and extract authorization code
async fn wait_for_oauth_callback(redirect_uri: &str) -> anyhow::Result<String> {
    // Parse redirect URI to get port
    let redirect_url = url::Url::parse(redirect_uri)?;
    let port = redirect_url.port().unwrap_or(8080);

    let bind = format!("127.0.0.1:{}", port);
    tracing::info!("Starting OAuth callback server on {}", bind);

    let listener = TcpListener::bind(&bind).await?;
    tracing::info!("Waiting for OAuth callback...");

    let (mut stream, _) = listener.accept().await?;

    // Read HTTP request manually
    let mut buffer = [0u8; 4096];
    let n = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..n]);

    tracing::debug!("Received request: {}", request.lines().take(5).collect::<Vec<_>>().join("\n"));

    // Extract code from URL in the request line
    let code = if let Some(get_line) = request.lines().next() {
        // Format: "GET /callback?code=xxx&state=xxx HTTP/1.1"
        if let Some(query_start) = get_line.find('?') {
            let query = &get_line[query_start + 1..];
            query
                .split('&')
                .find(|s| s.starts_with("code="))
                .map(|s| s.trim_start_matches("code="))
                .map(|s| urlencoding::decode(s).unwrap().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Send success response
    let response = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/html\r\n\
                    Content-Length: 78\r\n\
                    \r\n\
                    Authorization successful! You can close this page.";
    stream.write_all(response.as_bytes()).await?;

    tracing::info!("Received authorization code");
    Ok(code)
}

#[derive(Subcommand, Debug)]
enum ProxyCommands {
    /// Show current proxy configuration
    Show,
    /// Set proxy URL
    Set {
        /// Proxy URL (e.g., socks5://127.0.0.1:1080, http://127.0.0.1:8080)
        url: String,
    },
    /// Enable proxy
    Enable,
    /// Disable proxy
    Disable,
    /// Remove proxy configuration
    Unset,
}

/// CLI for Gemini Proxy
#[derive(Parser, Debug)]
#[command(name = "gemini-proxy")]
#[command(version = "0.1.0")]
#[command(about = "Lightweight Google Gemini API reverse proxy", long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = None)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the proxy server
    Serve {
        /// Bind address
        #[arg(short, long, default_value = "127.0.0.1:8045")]
        bind: String,
    },
    /// Login with Google OAuth
    Login {
        /// Force re-authentication
        #[arg(short, long)]
        force: bool,
        /// Proxy URL (e.g., socks5://127.0.0.1:1080)
        #[arg(short, long)]
        proxy: Option<String>,
    },
    /// Manage proxy configuration
    Proxy {
        #[command(subcommand)]
        command: ProxyCommands,
    },
    /// Check current login status
    Status,
    /// Show quota information
    Quota,
    /// Logout (clear stored token)
    Logout,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Get log directory path
    let log_dir = config::get_default_config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs");

    // Create log directory if not exists
    std::fs::create_dir_all(&log_dir)?;

    // Initialize file logging
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &log_dir, "gemini-proxy.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
        )
        .with(EnvFilter::from_default_env().add_directive("gemini_proxy=info".parse()?))
        .init();

    let cli = Cli::parse();

    // Load configuration
    let config_path = cli.config.as_ref()
        .map(|p| p.clone())
        .unwrap_or_else(|| config::get_default_config_path());

    let config = if config_path.exists() {
        config::Config::load(&config_path)?.resolved()
    } else {
        tracing::info!("Config not found at {:?}, creating with defaults", config_path);
        let cfg = config::Config::default();
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        cfg.save(&config_path)?;
        tracing::info!("Default config saved to {:?}", config_path);
        cfg.resolved()
    };

    // Execute command
    match cli.command {
        Some(Commands::Serve { bind }) => {
            cmd_serve(&config, &bind).await?;
        }
        Some(Commands::Login { force, .. }) => {
            cmd_login(&config, force, cli.config.as_ref()).await?;
        }
        Some(Commands::Status) => {
            cmd_status().await?;
        }
        Some(Commands::Quota) => {
            cmd_quota(&config).await?;
        }
        Some(Commands::Logout) => {
            cmd_logout().await?;
        }
        Some(Commands::Proxy { command }) => {
            cmd_proxy(&config, command)?;
        }
        None => {
            // No command specified, start server with default config
            cmd_serve(&config, &config.bind).await?;
        }
    }

    Ok(())
}

/// Start the proxy server
async fn cmd_serve(config: &config::Config, bind: &str) -> anyhow::Result<()> {
    tracing::info!("Starting Gemini Proxy on {}", bind);

    // Initialize token manager
    let token_manager = token::TokenManager::new();

    // Check if logged in
    if !token_manager.is_logged_in().await {
        tracing::error!("Not logged in. Please run 'gemini-proxy login' first.");
        return Ok(());
    }

    // Initialize upstream client
    let upstream_client = upstream::UpstreamClient::new(Some(config.upstream_proxy.clone()))?;

    // Print startup banner
    println!();
    println!("=== Gemini Proxy Server Started ===");
    println!("Listen: http://{}", bind);
    println!("Log: ~/.gemini-proxy/logs/");
    println!();
    println!("Usage Examples:");
    println!();
    println!("# Chat completion");
    println!("curl -X POST http://{}/v1/models/gemini-3-flash:generateContent \\", bind);
    println!("     -H 'Content-Type: application/json' \\");
    println!("     -d '{{\"contents\":[{{\"parts\":[{{\"text\":\"Hello\"}}]}}]}}'");
    println!();
    println!("# List models");
    println!("curl http://{}/v1/models", bind);
    println!();

    // Start server
    proxy::start_server(
        config.clone(),
        Arc::new(token_manager),
        Arc::new(upstream_client),
        bind,
    )
    .await
}

/// Login with Google OAuth
async fn cmd_login(config: &config::Config, _force: bool, _cli_config_path: Option<&PathBuf>) -> anyhow::Result<()> {
    use oauth::{exchange_code, get_auth_url, get_user_info, save_token, TokenData};

    // Determine proxy to use: CLI arg > config file > none
    let proxy = if config.upstream_proxy.enabled && !config.upstream_proxy.url.is_empty() {
        Some(config.upstream_proxy.clone())
    } else {
        None
    };

    if let Some(ref p) = proxy {
        tracing::info!("Using proxy for OAuth: {}", p.url);
    } else {
        tracing::warn!("No proxy configured, OAuth may fail if Google is not reachable");
    }

    // Generate state for OAuth
    let state = format!("gemini_proxy_{}", chrono::Local::now().timestamp_millis());

    // Generate auth URL
    let auth_url = get_auth_url(&config.oauth.redirect_uri, &state)?;
    tracing::info!("Generated OAuth URL, opening browser...");

    println!("\n=== Gemini Proxy Login ===\n");
    if let Some(ref p) = proxy {
        println!("Using proxy: {}\n", p.url);
    }
    println!("1. Open the following URL in your browser:\n");
    println!("{}\n", auth_url);
    println!("2. Complete the Google authorization");
    println!("3. Wait for the authorization code to be captured automatically\n");

    // Open browser
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", &auth_url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&auth_url)
            .spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&auth_url)
            .spawn()?;
    }

    // Wait for OAuth callback with authorization code
    let code = wait_for_oauth_callback(&config.oauth.redirect_uri).await?;

    // Exchange code for token
    tracing::info!("Exchanging authorization code...");
    let token_response = exchange_code(&code, &config.oauth.redirect_uri, &proxy).await?;

    // Get refresh token
    let refresh_token = token_response
        .refresh_token
        .ok_or_else(|| anyhow::anyhow!("No refresh token returned. Please try again."))?;

    // Get user info
    tracing::info!("Fetching user info...");
    let user_info = get_user_info(&token_response.access_token, &proxy).await?;

    tracing::info!("Logged in as: {}", user_info.email);

    // Save token
    let token_data = TokenData::new(
        token_response.access_token,
        refresh_token,
        token_response.expires_in,
        Some(user_info.email),
        None, // project_id will be fetched later
    );

    save_token(&token_data)?;

    println!("\n✓ Login successful! Token saved.\n");

    Ok(())
}

/// Check login status
async fn cmd_status() -> anyhow::Result<()> {
    let token_manager = token::TokenManager::new();

    let token = token_manager.get_token().await;
    if let Some(t) = token {
        println!("\n✓ Logged in as: {}\n", t.email.as_deref().unwrap_or("N/A"));
        println!("  Token expires in: {} seconds", t.expires_in);
        println!("  Expiry timestamp: {}", t.expiry_timestamp);
    } else {
        println!("\n✗ Not logged in. Run 'gemini-proxy login' to login.\n");
    }

    Ok(())
}

/// Show quota information
async fn cmd_quota(config: &config::Config) -> anyhow::Result<()> {
    let token_manager = token::TokenManager::new();

    if !token_manager.is_logged_in().await {
        anyhow::bail!("Not logged in. Please run 'gemini-proxy login' first.");
    }

    let proxy = if config.upstream_proxy.enabled {
        Some(config.upstream_proxy.clone())
    } else {
        None
    };
    let access_token = token_manager.get_fresh_token(&proxy).await?;

    // Fetch project ID
    let (project_id, subscription_tier) = quota::fetch_project_id(&access_token, &proxy).await?;
    println!("\n=== Account Info ===");
    println!("Project ID: {:?}", project_id);
    println!("Subscription tier: {:?}\n", subscription_tier);

    // Fetch quota
    println!("=== Available Models ===\n");
    let models = quota::fetch_quota(&access_token, project_id.as_deref(), &proxy).await?;

    for model in models {
        let percentage = model.percentage;
        let bar = if percentage >= 100 {
            "██████████"
        } else if percentage >= 80 {
            "█████████░"
        } else if percentage >= 60 {
            "████████░░"
        } else if percentage >= 40 {
            "██████░░░░"
        } else if percentage >= 20 {
            "████░░░░░░"
        } else {
            "██░░░░░░░░"
        };

        println!(
            "  {:40} {:>3}% {}",
            model.name.chars().take(40).collect::<String>(),
            percentage,
            bar
        );
    }

    println!();
    Ok(())
}

/// Logout (clear stored token)
async fn cmd_logout() -> anyhow::Result<()> {
    let token_manager = token::TokenManager::new();
    token_manager.clear_token().await?;
    println!("\n✓ Logged out successfully.\n");
    Ok(())
}

/// Manage proxy configuration
fn cmd_proxy(config: &config::Config, command: ProxyCommands) -> anyhow::Result<()> {
    let config_path = config::get_default_config_path();
    let mut cfg = config.clone();

    match command {
        ProxyCommands::Show => {
            tracing::info!("Showing proxy configuration");
            println!("\n=== Proxy Configuration ===\n");
            if config.upstream_proxy.enabled {
                println!("  Status:  Enabled");
                println!("  URL:     {}\n", config.upstream_proxy.url);
            } else {
                println!("  Status:  Disabled\n");
            }
        }
        ProxyCommands::Set { url } => {
            tracing::info!("Setting proxy URL: {}", url);
            cfg.upstream_proxy.enabled = true;
            cfg.upstream_proxy.url = url.clone();
            cfg.save(&config_path)?;
            println!("\n✓ Proxy set to: {}\n", url);
        }
        ProxyCommands::Enable => {
            tracing::info!("Enabling proxy: {}", cfg.upstream_proxy.url);
            if cfg.upstream_proxy.url.is_empty() {
                anyhow::bail!("No proxy URL configured. Use 'proxy set <url>' first.");
            }
            cfg.upstream_proxy.enabled = true;
            cfg.save(&config_path)?;
            println!("\n✓ Proxy enabled: {}\n", cfg.upstream_proxy.url);
        }
        ProxyCommands::Disable => {
            tracing::info!("Disabling proxy");
            cfg.upstream_proxy.enabled = false;
            cfg.save(&config_path)?;
            println!("\n✓ Proxy disabled\n");
        }
        ProxyCommands::Unset => {
            tracing::info!("Removing proxy configuration");
            cfg.upstream_proxy = config::ProxyConfig::default();
            cfg.save(&config_path)?;
            println!("\n✓ Proxy configuration removed\n");
        }
    }

    Ok(())
}
