# Gemini Proxy OpenAI-Compatible Design

## Goal

Expose `gemini-proxy` as an OpenAI-compatible local provider so CCP can reuse its existing `openai-compatible` adapter.

Target flow:

```text
Claude Code
  -> CCP /v1/messages
  -> CCP provider type: openai-compatible
  -> gemini-proxy /v1/chat/completions
  -> Google Gemini v1internal
```

`gemini-proxy` remains responsible for Google OAuth, token refresh, project discovery, quota/model discovery, upstream proxy, and v1internal request wrapping. CCP remains responsible for Claude Code / Anthropic compatibility.

## Public API

Add:

- `POST /v1/chat/completions`
- `GET /v1/models`
- `GET /healthz`

Keep the existing Gemini native paths for compatibility:

- `/v1/models/{model}:generateContent`
- `/v1beta/models/{model}:generateContent`

## Auth

Use `config.api_key` when set.

Accepted request headers:

- `Authorization: Bearer <api_key>`
- `x-api-key: <api_key>`

If `api_key` is absent, local auth is disabled for backward compatibility.

## Concurrency Protection

Add config:

```json
{
  "max_concurrent_requests": 16
}
```

If the limiter is full, wait briefly and return HTTP `503` if no slot opens.

## OpenAI Request

Support the MVP-compatible subset:

```json
{
  "model": "gemini-3-flash",
  "messages": [
    {"role": "system", "content": "be concise"},
    {"role": "user", "content": "hi"}
  ],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "Read",
        "description": "Read a file",
        "parameters": {"type": "object"}
      }
    }
  ],
  "temperature": 0.7,
  "max_tokens": 1024,
  "stream": true
}
```

## Gemini Conversion

OpenAI messages map to Gemini:

- `system` -> `systemInstruction.parts[].text`
- `user` text -> `contents[].role = "user"`
- `assistant` text -> `contents[].role = "model"`
- OpenAI `tool` result -> Gemini `functionResponse`
- OpenAI assistant `tool_calls` -> Gemini `functionCall`

OpenAI tools map to Gemini:

- `tools[].function` -> `tools[].functionDeclarations[]`

Generation config:

- `max_tokens` -> `generationConfig.maxOutputTokens`
- `temperature` -> `generationConfig.temperature`

## OpenAI Response

Gemini text response maps to:

```json
{
  "id": "chatcmpl_xxx",
  "object": "chat.completion",
  "model": "gemini-3-flash",
  "choices": [
    {
      "index": 0,
      "message": {"role": "assistant", "content": "hi"},
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 1,
    "completion_tokens": 1,
    "total_tokens": 2
  }
}
```

Gemini `functionCall` maps to OpenAI `tool_calls`.

## Streaming

Preferred upstream method:

```text
streamGenerateContent
```

Public response:

```text
content-type: text/event-stream

data: {"choices":[{"delta":{"content":"hello"}}]}

data: [DONE]
```

If upstream streaming is unavailable, the route may fall back to non-streaming and emit a single OpenAI SSE chunk. This preserves Claude Code protocol compatibility while true streaming is completed.

## CCP Configuration

```yaml
providers:
  gemini:
    type: openai-compatible
    base_url: http://127.0.0.1:8045
    api_key: ${GEMINI_PROXY_API_KEY}
    max_concurrent_requests: 16

aliases:
  sonnet: gemini:gemini-3-flash
```

