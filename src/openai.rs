use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ChatTool>,
    pub temperature: Option<f64>,
    #[serde(rename = "max_tokens")]
    pub max_tokens: Option<i32>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Vec<ChatToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatToolFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatToolCallFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Serialize)]
pub struct ChatChoice {
    pub index: i32,
    pub message: ChatResponseMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize)]
pub struct ChatResponseMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
}

#[derive(Debug, Serialize)]
pub struct ChatUsage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

/// Stateful converter: Gemini SSE stream chunks -> OpenAI SSE delta chunks.
///
/// Gemini sends accumulated text; we compute deltas and emit OpenAI-compatible
/// SSE lines. Call [`process_chunk`] for each upstream SSE JSON event, then
/// send each resulting string to the client.
pub struct GeminiStreamConverter {
    id: String,
    model: String,
    created: i64,
    sent_text_len: usize,
    finished: bool,
}

impl GeminiStreamConverter {
    pub fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl_{}", chrono::Utc::now().timestamp_millis()),
            model,
            created: chrono::Utc::now().timestamp(),
            sent_text_len: 0,
            finished: false,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Process one Gemini SSE JSON chunk.
    /// Returns SSE event strings to send to the client (e.g. `data: {...}\n\n`).
    /// Returns an empty vec if the chunk contains no new content.
    pub fn process_chunk(&mut self, gemini: &Value) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }

        // v1internal API wraps each chunk in a "response" field
        let gemini = gemini.get("response").unwrap_or(gemini);

        let candidate = gemini
            .get("candidates")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first());

        let parts = candidate
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        let mut full_text = String::new();
        for part in &parts {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                full_text.push_str(t);
            }
        }

        let finish_reason_raw = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(|v| v.as_str())
            .map(|r| normalize_finish_reason(r));

        let mut out = Vec::new();

        // Emit text delta if there's new text
        if full_text.len() > self.sent_text_len {
            let delta = full_text[self.sent_text_len..].to_string();
            self.sent_text_len = full_text.len();
            out.push(self.build_sse(&json!({
                "content": delta,
            }), None));
        }

        // Emit finish chunk + [DONE] when finishReason appears
        if let Some(reason) = finish_reason_raw {
            out.push(self.build_sse(&json!({}), Some(&reason)));
            out.push("data: [DONE]\n\n".to_string());
            self.finished = true;
        }

        out
    }

    fn build_sse(&self, delta: &Value, finish_reason: Option<&str>) -> String {
        let delta_map = delta.clone();
        if delta_map.as_object().map_or(true, |o| o.is_empty()) && finish_reason.is_none() {
            return String::new();
        }

        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta_map,
                "finish_reason": finish_reason,
            }]
        });
        format!("data: {}\n\n", chunk)
    }
}

pub fn chat_to_gemini(req: &ChatCompletionRequest) -> Value {
    let mut contents = Vec::new();
    let mut system_text = String::new();

    for msg in &req.messages {
        match msg.role.as_str() {
            "system" => {
                system_text.push_str(&content_to_text(msg.content.as_ref()));
            }
            "assistant" => {
                let mut parts = Vec::new();
                if let Some(text) = text_content(msg.content.as_ref()) {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                for call in &msg.tool_calls {
                    let args: Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({ "arguments": call.function.arguments }));
                    parts.push(json!({
                        "functionCall": {
                            "name": call.function.name,
                            "args": args
                        }
                    }));
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            "tool" => {
                let content = content_to_text(msg.content.as_ref());
                let name = msg.tool_call_id.clone().unwrap_or_else(|| "tool".to_string());
                contents.push(json!({
                    "role": "function",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": { "content": content }
                        }
                    }]
                }));
            }
            _ => {
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": content_to_text(msg.content.as_ref()) }]
                }));
            }
        }
    }

    let mut generation_config = json!({});
    if let Some(t) = req.temperature {
        generation_config["temperature"] = json!(t);
    }
    if let Some(max) = req.max_tokens {
        generation_config["maxOutputTokens"] = json!(max);
    }

    let mut out = json!({
        "contents": contents,
        "generationConfig": generation_config,
    });

    if !system_text.is_empty() {
        out["systemInstruction"] = json!({
            "parts": [{ "text": system_text }]
        });
    }

    if !req.tools.is_empty() {
        let declarations: Vec<Value> = req.tools.iter().filter_map(|tool| {
            if tool.kind != "function" {
                return None;
            }
            Some(json!({
                "name": tool.function.name,
                "description": tool.function.description.clone().unwrap_or_default(),
                "parameters": tool.function.parameters.clone().unwrap_or_else(|| json!({"type":"object"}))
            }))
        }).collect();
        if !declarations.is_empty() {
            out["tools"] = json!([{ "functionDeclarations": declarations }]);
        }
    }

    out
}

pub fn gemini_to_chat(model: &str, gemini: &Value) -> ChatCompletionResponse {
    // v1internal API wraps response in a "response" field
    let gemini = gemini.get("response").unwrap_or(gemini);

    let candidate = gemini
        .get("candidates")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let parts = candidate
        .get("content")
        .and_then(|v| v.get("parts"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
            text.push_str(t);
        }
        if let Some(fc) = part.get("functionCall") {
            let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            tool_calls.push(ChatToolCall {
                id: format!("call_{}", idx),
                kind: "function".to_string(),
                function: ChatToolCallFunction {
                    name: name.to_string(),
                    arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                },
            });
        }
    }

    let usage = gemini.get("usageMetadata").map(|u| {
        let prompt = u.get("promptTokenCount").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let completion = u.get("candidatesTokenCount").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        ChatUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        }
    });

    ChatCompletionResponse {
        id: format!("chatcmpl_{}", chrono::Utc::now().timestamp_millis()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatResponseMessage {
                role: "assistant".to_string(),
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls,
            },
            finish_reason: candidate
                .get("finishReason")
                .and_then(|v| v.as_str())
                .map(normalize_finish_reason)
                .unwrap_or_else(|| "stop".to_string()),
        }],
        usage,
    }
}

fn normalize_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" => "content_filter",
        _ => "stop",
    }
    .to_string()
}

fn content_to_text(content: Option<&Value>) -> String {
    text_content(content).unwrap_or_default()
}

fn text_content(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                }
            }
            Some(out)
        }
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_openai_text_request_to_gemini() {
        let req = ChatCompletionRequest {
            model: "gemini-3-flash".to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: Some(json!("be concise")),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: Some(json!("hi")),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
            ],
            tools: vec![],
            temperature: Some(0.7),
            max_tokens: Some(128),
            stream: false,
        };
        let out = chat_to_gemini(&req);
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "be concise");
        assert_eq!(out["contents"][0]["role"], "user");
        assert_eq!(out["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(out["generationConfig"]["maxOutputTokens"], 128);
    }

    #[test]
    fn converts_openai_tools_to_gemini_function_declarations() {
        let req = ChatCompletionRequest {
            model: "gemini-3-flash".to_string(),
            messages: vec![],
            tools: vec![ChatTool {
                kind: "function".to_string(),
                function: ChatToolFunction {
                    name: "Read".to_string(),
                    description: Some("Read a file".to_string()),
                    parameters: Some(json!({"type":"object"})),
                },
            }],
            temperature: None,
            max_tokens: None,
            stream: false,
        };
        let out = chat_to_gemini(&req);
        assert_eq!(out["tools"][0]["functionDeclarations"][0]["name"], "Read");
    }

    #[test]
    fn converts_gemini_text_response_to_openai() {
        let gemini = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "hello" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 4
            }
        });
        let out = gemini_to_chat("gemini-3-flash", &gemini);
        assert_eq!(out.choices[0].message.content.as_deref(), Some("hello"));
        assert_eq!(out.choices[0].finish_reason, "stop");
        assert_eq!(out.usage.unwrap().total_tokens, 7);
    }

    #[test]
    fn converts_gemini_function_call_to_openai_tool_call() {
        let gemini = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "Read",
                            "args": { "file_path": "a.txt" }
                        }
                    }]
                }
            }]
        });
        let out = gemini_to_chat("gemini-3-flash", &gemini);
        assert_eq!(out.choices[0].message.tool_calls[0].function.name, "Read");
        assert!(out.choices[0].message.tool_calls[0].function.arguments.contains("a.txt"));
    }

    #[test]
    fn streams_gemini_chunks_to_openai_delta_sse() {
        let mut c = GeminiStreamConverter::new("gemini-3-flash".to_string());

        // First chunk: partial text
        let lines = c.process_chunk(&json!({
            "candidates": [{"content": {"parts": [{"text": "Hello"}], "role": "model"}}]
        }));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(r#""delta":{"content":"Hello"}"#));
        assert!(!c.is_finished());

        // Second chunk: accumulated text
        let lines = c.process_chunk(&json!({
            "candidates": [{"content": {"parts": [{"text": "Hello world"}], "role": "model"}}]
        }));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(r#""content":" world""#)); // only delta

        // Third chunk: finish signal
        let lines = c.process_chunk(&json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hello world"}], "role": "model"},
                "finishReason": "STOP"
            }]
        }));
        assert_eq!(lines.len(), 2); // finish chunk + [DONE]
        assert!(lines[0].contains(r#""finish_reason":"stop""#));
        assert_eq!(lines[1], "data: [DONE]\n\n");
        assert!(c.is_finished());

        // Extra chunks after finish are ignored
        assert!(c.process_chunk(&json!({"candidates":[]})).is_empty());
    }
}
