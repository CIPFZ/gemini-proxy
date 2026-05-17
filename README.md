# gemini-proxy

OpenAI-compatible local proxy for Gemini.

## What it does

- Exposes `POST /v1/chat/completions`
- Exposes `GET /v1/models`
- Exposes `GET /healthz`
- Keeps Gemini OAuth and upstream forwarding inside this proxy
- Supports request auth, concurrency limiting, logs, and `${VAR}` config expansion

## Build

```bash
cargo build --release
```

## Quick start

```bash
# Login
./target/release/gemini-proxy login

# Start server (foreground)
./target/release/gemini-proxy serve

# Start server (background daemon)
./target/release/gemini-proxy serve --daemon

# Stop daemon
./target/release/gemini-proxy stop

# Check status
./target/release/gemini-proxy status

# Check quota
./target/release/gemini-proxy quota
```

Default bind:

```text
http://127.0.0.1:8045
```

## Config

Config path:

```text
~/.gemini-proxy/config.json
```

Supported values include:

- `bind`
- `api_key`
- `max_concurrent_requests`
- `upstream_proxy.enabled`
- `upstream_proxy.url`
- `oauth.client_key`
- `oauth.redirect_uri`

Example:

```json
{
  "bind": "127.0.0.1:8045",
  "api_key": "${GEMINI_PROXY_API_KEY}",
  "max_concurrent_requests": 16,
  "upstream_proxy": {
    "enabled": false,
    "url": "http://127.0.0.1:7897"
  },
  "oauth": {
    "client_key": "${GEMINI_PROXY_CLIENT_ID}",
    "redirect_uri": "http://127.0.0.1:8045/callback"
  }
}
```

Environment variables can be used with `${NAME}` syntax.

## Proxy

Manage upstream proxy for OAuth and API calls:

```bash
./target/release/gemini-proxy proxy set socks5://127.0.0.1:1080
./target/release/gemini-proxy proxy enable
./target/release/gemini-proxy proxy disable
./target/release/gemini-proxy proxy show
```

## OAuth env vars

Set these before login:

- `GEMINI_PROXY_GOOGLE_CLIENT_ID`
- `GEMINI_PROXY_GOOGLE_CLIENT_SECRET`

Fallback names are also accepted:

- `GEMINI_PROXY_CLIENT_ID`
- `GEMINI_PROXY_CLIENT_SECRET`

## CCP usage

Point CCP to this proxy as an OpenAI-compatible provider:

```yaml
providers:
  gemini:
    type: openai-compatible
    base_url: http://127.0.0.1:8045
    api_key: ${GEMINI_PROXY_API_KEY}
    max_concurrent_requests: 16
```

Model aliases should map Claude names to Gemini model names handled by this proxy.

## API examples

Non-streaming:

```bash
curl http://127.0.0.1:8045/v1/chat/completions \
  -H "Authorization: Bearer $GEMINI_PROXY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}'
```

Streaming:

```bash
curl http://127.0.0.1:8045/v1/chat/completions \
  -H "Authorization: Bearer $GEMINI_PROXY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

## Logging

Logs are written under:

```text
~/.gemini-proxy/logs/
```
