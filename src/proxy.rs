//! Simple proxy server using hyper
//! This is a simplified implementation that avoids complex axum routing issues

use crate::config::Config;
use crate::openai::{chat_response_to_sse, chat_to_gemini, gemini_to_chat, ChatCompletionRequest};
use crate::quota::{fetch_project_id, fetch_quota};
use crate::token::TokenManager;
use crate::upstream::UpstreamClient;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use tokio::net::TcpListener;

pub async fn start_server(
    config: Config,
    token_manager: Arc<TokenManager>,
    upstream_client: Arc<UpstreamClient>,
    bind: &str,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!("Proxy server listening on http://{}", bind);
    let limiter = Arc::new(Semaphore::new(config.max_concurrent_requests.max(1)));

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let io = TokioIo::new(stream);

                let token_manager = token_manager.clone();
                let upstream_client = upstream_client.clone();
                let config = config.clone();
                let limiter = limiter.clone();

                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let token_manager = token_manager.clone();
                        let upstream_client = upstream_client.clone();
                        let config = config.clone();
                        let limiter = limiter.clone();

                        async move {
                            handle_request(req, config, token_manager, upstream_client, limiter).await
                        }
                    });

                    if let Err(e) = http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::error!("Error serving connection: {}", e);
                    }
                });
            }
            Err(e) => {
                tracing::error!("Error accepting connection: {}", e);
            }
        }
    }
}

fn json_response(body: &str, status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string())
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn sse_response(body: String, status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

async fn handle_request(
    req: Request<Incoming>,
    config: Config,
    token_manager: Arc<TokenManager>,
    upstream_client: Arc<UpstreamClient>,
    limiter: Arc<Semaphore>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    tracing::debug!("{} {}", method, path);

    match (method, path.as_str()) {
        (Method::GET, "/healthz") => {
            Ok(json_response(r#"{"status":"ok"}"#, StatusCode::OK))
        }
        (Method::GET, "/internal/quota") | (Method::POST, "/internal/quota") => {
            handle_quota_request(token_manager, &config.upstream_proxy).await
        }
        (Method::GET, "/v1/models") => {
            handle_list_models(token_manager, &config.upstream_proxy, &config).await
        }
        (Method::POST, "/v1/chat/completions") => {
            handle_chat_completions(req, config, token_manager, upstream_client, limiter).await
        }
        _ => {
            handle_proxy_request(req, path, &config.upstream_proxy, token_manager, upstream_client).await
        }
    }
}

fn check_auth(req: &Request<Incoming>, api_key: &Option<String>) -> bool {
    let Some(expected) = api_key.as_ref().filter(|k| !k.is_empty()) else {
        return true;
    };

    req.headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v == expected)
        .unwrap_or_else(|| {
            req.headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == expected)
                .unwrap_or(false)
        })
}

async fn handle_quota_request(
    token_manager: Arc<TokenManager>,
    proxy: &crate::config::ProxyConfig,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let access_token = match token_manager.get_fresh_token(&Some(proxy.clone())).await {
        Ok(token) => token,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let (project_id, subscription_tier) = match fetch_project_id(&access_token, &Some(proxy.clone())).await {
        Ok(result) => result,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"Failed to fetch project ID: {}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let models = match fetch_quota(&access_token, project_id.as_deref(), &Some(proxy.clone())).await {
        Ok(models) => models,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"Failed to fetch quota: {}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let response = serde_json::json!({
        "project_id": project_id,
        "subscription_tier": subscription_tier,
        "models": models,
    });

    let body = serde_json::to_string(&response).unwrap();
    Ok(json_response(&body, StatusCode::OK))
}

async fn handle_list_models(
    token_manager: Arc<TokenManager>,
    proxy: &crate::config::ProxyConfig,
    config: &Config,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let access_token = match token_manager.get_fresh_token(&Some(proxy.clone())).await {
        Ok(token) => token,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let models = match fetch_quota(&access_token, None, &Some(proxy.clone())).await {
        Ok(models) => models,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let model_list: Vec<_> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.name,
                "object": "model",
                "created": 0,
                "owned_by": "gemini-proxy",
                "permission": [],
                "root": m.name,
                "parent": null,
                "display_name": m.display_name.as_deref().unwrap_or(&m.name),
                "max_tokens": m.max_tokens.unwrap_or(8192),
                "supports_images": m.supports_images.unwrap_or(false),
                "supports_thinking": m.supports_thinking.unwrap_or(false),
                "temperature": 1.0,
                "top_p": 0.95,
                "top_k": 64,
                "api_base": format!("http://{}", config.bind),
            })
        })
        .collect();

    let response = serde_json::json!({
        "models": model_list
    });

    let body = serde_json::to_string(&response).unwrap();
    Ok(json_response(&body, StatusCode::OK))
}

async fn handle_chat_completions(
    req: Request<Incoming>,
    config: Config,
    token_manager: Arc<TokenManager>,
    upstream_client: Arc<UpstreamClient>,
    limiter: Arc<Semaphore>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if !check_auth(&req, &config.api_key) {
        return Ok(json_response(r#"{"error":{"message":"unauthorized"}}"#, StatusCode::UNAUTHORIZED));
    }

    let permit = match timeout(Duration::from_millis(200), limiter.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            return Ok(json_response(r#"{"error":{"message":"server busy"}}"#, StatusCode::SERVICE_UNAVAILABLE));
        }
        Err(_) => {
            return Ok(json_response(r#"{"error":{"message":"server busy"}}"#, StatusCode::SERVICE_UNAVAILABLE));
        }
    };

    let access_token = match token_manager.get_fresh_token(&Some(config.upstream_proxy.clone())).await {
        Ok(token) => token,
        Err(e) => {
            drop(permit);
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let project_id = match fetch_project_id(&access_token, &Some(config.upstream_proxy.clone())).await {
        Ok((pid, _)) => pid.unwrap_or_else(|| "cosmic-task-h4r8v".to_string()),
        Err(_) => "cosmic-task-h4r8v".to_string(),
    };

    let body_bytes = match req.into_body().collect().await {
        Ok(bytes) => bytes.to_bytes().to_vec(),
        Err(e) => {
            drop(permit);
            return Ok(json_response(&format!(r#"{{"error":"Failed to read body: {}"}}"#, e), StatusCode::BAD_REQUEST));
        }
    };

    let openai_req: ChatCompletionRequest = match serde_json::from_slice(&body_bytes) {
        Ok(req) => req,
        Err(e) => {
            drop(permit);
            return Ok(json_response(&format!(r#"{{"error":"Invalid request: {}"}}"#, e), StatusCode::BAD_REQUEST));
        }
    };

    let gemini_body = chat_to_gemini(&openai_req);
    let model_name = openai_req.model.clone();
    let method = if openai_req.stream { "streamGenerateContent" } else { "generateContent" };
    let response = match upstream_client
        .call_v1_internal(method, &access_token, wrap_request_for_v1internal(&gemini_body, &model_name, &project_id), None)
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            drop(permit);
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::BAD_GATEWAY));
        }
    };

    if openai_req.stream {
        let status = response.status();
        let body = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                drop(permit);
                return Ok(json_response(&format!(r#"{{"error":"Failed to read response: {}"}}"#, e), StatusCode::BAD_GATEWAY));
            }
        };
        if !status.is_success() {
            drop(permit);
            return Ok(json_response(&body, status));
        }
        let gemini_json: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
        let chat = gemini_to_chat(&model_name, &gemini_json);
        drop(permit);
        return Ok(sse_response(chat_response_to_sse(&chat), StatusCode::OK));
    }

    let status = response.status();
    let body = match response.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(e) => {
            drop(permit);
            return Ok(json_response(&format!(r#"{{"error":"Failed to read response: {}"}}"#, e), StatusCode::BAD_GATEWAY));
        }
    };
    if !status.is_success() {
        drop(permit);
        return Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("content-length", body.len().to_string())
            .body(Full::new(Bytes::from(body)))
            .unwrap());
    }

    let gemini_json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let chat = gemini_to_chat(&model_name, &gemini_json);
    let body = serde_json::to_string(&chat).unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
    drop(permit);
    Ok(json_response(&body, StatusCode::OK))
}

async fn handle_proxy_request(
    req: Request<Incoming>,
    path: String,
    proxy: &crate::config::ProxyConfig,
    token_manager: Arc<TokenManager>,
    upstream_client: Arc<UpstreamClient>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Get token
    let access_token = match token_manager.get_fresh_token(&Some(proxy.clone())).await {
        Ok(token) => token,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    // Fetch project ID for v1internal API
    let project_id = match fetch_project_id(&access_token, &Some(proxy.clone())).await {
        Ok((pid, _)) => pid.unwrap_or_else(|| "cosmic-task-h4r8v".to_string()),
        Err(_) => "cosmic-task-h4r8v".to_string(),
    };

    // Collect request body
    let body_bytes = match req.into_body().collect().await {
        Ok(bytes) => bytes.to_bytes().to_vec(),
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"Failed to read body: {}"}}"#, e), StatusCode::BAD_REQUEST));
        }
    };

    let body: serde_json::Value = if body_bytes.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}))
    };

    // Parse the path: /v1beta/models/:model/:action or /v1beta/models/:model:action
    let upstream_method = if path.contains(":") {
        path.rsplit(':').next().unwrap_or("generateContent").to_string()
    } else {
        "generateContent".to_string()
    };

    // Extract model name from path (e.g., "gemini-3-flash" from "/v1beta/models/gemini-3-flash:generateContent")
    let model_name = path
        .split('/')
        .filter(|s| !s.is_empty())
        .last()
        .and_then(|s| s.split(':').next())
        .unwrap_or("gemini-3-flash")
        .to_string();

    // Wrap request for v1internal API
    let wrapped_body = wrap_request_for_v1internal(&body, &model_name, &project_id);

    tracing::info!("Proxying request: model={}, method={}, project={}", model_name, upstream_method, project_id);

    // Call upstream
    let response = match upstream_client
        .call_v1_internal(&upstream_method, &access_token, wrapped_body, None)
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"{}"}}"#, e), StatusCode::BAD_GATEWAY));
        }
    };

    let status = response.status();
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(e) => {
            return Ok(json_response(&format!(r#"{{"error":"Failed to read response: {}"}}"#, e), StatusCode::BAD_GATEWAY));
        }
    };

    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", body_bytes.len().to_string())
        .body(Full::new(Bytes::from(body_bytes)))
        .unwrap())
}

/// Wrap request body for v1internal API format
fn wrap_request_for_v1internal(body: &serde_json::Value, model_name: &str, project_id: &str) -> serde_json::Value {
    // Generate a simple session ID
    let session_id = format!("gemini-proxy-{}", chrono::Utc::now().timestamp_millis());
    let request_id = format!("agent/antigravity/{}/1", &session_id[..session_id.len().min(8)]);

    // Clone the body to use as inner request
    let mut inner_request = body.clone();

    // Ensure contents exists
    if !inner_request.get("contents").is_some() {
        inner_request["contents"] = serde_json::json!([{"role": "user", "parts": [{"text": "Hi"}]}]);
    }

    // Ensure generationConfig exists with proper defaults
    if !inner_request.get("generationConfig").is_some() {
        inner_request["generationConfig"] = serde_json::json!({
            "temperature": 0.9,
            "topP": 1.0,
            "topK": 40
        });
    }

    // Build the v1internal request format
    serde_json::json!({
        "project": project_id,
        "requestId": request_id,
        "request": inner_request,
        "model": model_name,
        "userAgent": "antigravity",
        "requestType": "agent"
    })
}
