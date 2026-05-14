# OpenAI-Compatible Implementation Plan

## Phase 1: Config, Auth, And Limiters

- Add `max_concurrent_requests` to `Config`.
- Add request auth check using `api_key`.
- Add a simple semaphore limiter.
- Apply limiter to the new OpenAI-compatible route.

## Phase 2: OpenAI Type Definitions

- Add `src/openai.rs`.
- Define chat request, messages, tools, tool calls, response, choices, usage.
- Define helper functions for SSE output.

## Phase 3: OpenAI -> Gemini Conversion

- Convert messages into Gemini `contents`.
- Convert tools into Gemini `functionDeclarations`.
- Convert generation config.
- Preserve function call history and tool result history.

## Phase 4: Gemini -> OpenAI Conversion

- Convert text parts into OpenAI assistant text.
- Convert `functionCall` into OpenAI `tool_calls`.
- Convert usage metadata.
- Normalize finish reasons.

## Phase 5: Route

- Add `POST /v1/chat/completions`.
- For `stream: false`, call v1internal `generateContent` and return OpenAI JSON.
- For `stream: true`, call v1internal `streamGenerateContent` if it works; otherwise emit a single SSE chunk from `generateContent`.

## Phase 6: Verification

- Unit test conversion helpers.
- Run `cargo test`.
- Build release binary.
- Start gemini-proxy and test:

```bash
curl -s http://127.0.0.1:8045/v1/chat/completions \
  -H "Authorization: Bearer $GEMINI_PROXY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gemini-3-flash","messages":[{"role":"user","content":"hi"}]}'
```

Then configure CCP:

```yaml
providers:
  gemini:
    type: openai-compatible
    base_url: http://127.0.0.1:8045
    api_key: ${GEMINI_PROXY_API_KEY}
```

