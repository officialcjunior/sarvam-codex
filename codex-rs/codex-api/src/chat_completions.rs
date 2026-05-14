//! Types and translation logic for the OpenAI-compatible Chat Completions API.
//!
//! Codex internally uses the Responses API format (`ResponsesApiRequest`).
//! This module translates that format into a Chat Completions request so that
//! providers which only speak Chat Completions (e.g. Sarvam AI) can be used.

use crate::common::ResponsesApiRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Top-level Chat Completions request payload.
#[derive(Debug, Serialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Maps to Sarvam's `reasoning_effort` field ("low" | "medium" | "high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// A single message in the `messages` array.
#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    /// Always a plain string — no array-of-parts.
    pub content: String,
    /// Present only on `role = "tool"` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Present on `role = "assistant"` messages that contain tool invocations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OutboundToolCall>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutboundToolCall {
    pub id: String,
    pub r#type: String,
    pub function: OutboundToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutboundToolCallFunction {
    pub name: String,
    pub arguments: String,
}

/// Requests per-chunk usage statistics in the final streaming chunk.
#[derive(Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

// ── SSE chunk types (used by sse/chat_completions.rs) ───────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsChunk {
    pub id: Option<String>,
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<ChunkUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkChoice {
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

// ── Translation ──────────────────────────────────────────────────────────────

/// Convert a `ResponsesApiRequest` into a `ChatCompletionsRequest`.
///
/// - `instructions` becomes a leading `system` message.
/// - `input` items are translated message-by-message; Responses-API-only
///   items (reasoning, local shell, web search, compaction, …) are silently
///   skipped because Chat Completions has no equivalent.
/// - Tool definitions are re-wrapped from the Responses API flat shape
///   `{type,name,description,parameters}` into the Chat Completions nested
///   shape `{type:"function",function:{name,description,parameters}}`.
/// - Responses-API-only request fields (`reasoning`, `text`, `include`,
///   `store`, `prompt_cache_key`, `client_metadata`, `service_tier`) are
///   dropped.
pub fn responses_to_chat_completions_request(
    req: &ResponsesApiRequest,
) -> ChatCompletionsRequest {
    let mut messages: Vec<ChatMessage> = Vec::new();

    // System prompt
    if !req.instructions.is_empty() {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: req.instructions.clone(),
            tool_call_id: None,
            tool_calls: None,
        });
    }

    // Conversation history
    for item in &req.input {
        if let Some(msg) = response_item_to_chat_message(item) {
            messages.push(msg);
        }
    }

    // Re-wrap function tools from Responses format to Chat Completions format.
    // Responses: {"type":"function","name":...,"description":...,"parameters":...}
    // Chat Completions: {"type":"function","function":{"name":...,"description":...,"parameters":...}}
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(rewrap_tool_for_chat_completions)
        .collect();

    let stream_options = if req.stream {
        Some(StreamOptions {
            include_usage: true,
        })
    } else {
        None
    };

    let reasoning_effort = req
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.as_ref())
        .map(|e| match e {
            ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => {
                "low".to_string()
            }
            ReasoningEffort::Medium => "medium".to_string(),
            ReasoningEffort::High | ReasoningEffort::XHigh => "high".to_string(),
        });

    ChatCompletionsRequest {
        model: req.model.clone(),
        messages,
        tools,
        tool_choice: req.tool_choice.clone(),
        parallel_tool_calls: req.parallel_tool_calls,
        stream: req.stream,
        stream_options,
        reasoning_effort,
    }
}

fn response_item_to_chat_message(item: &ResponseItem) -> Option<ChatMessage> {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text = flatten_content_items(content);
            // Sarvam only accepts: assistant, system, tool, user.
            // The Responses API uses "developer" for system-level messages — map it down.
            let mapped_role = if role == "developer" {
                "system".to_string()
            } else {
                role.clone()
            };
            Some(ChatMessage {
                role: mapped_role,
                content: text,
                tool_call_id: None,
                tool_calls: None,
            })
        }

        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => Some(ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(vec![OutboundToolCall {
                id: call_id.clone(),
                r#type: "function".to_string(),
                function: OutboundToolCallFunction {
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            }]),
        }),

        ResponseItem::FunctionCallOutput { call_id, output } => Some(ChatMessage {
            role: "tool".to_string(),
            content: output.body.to_text().unwrap_or_default(),
            tool_call_id: Some(call_id.clone()),
            tool_calls: None,
        }),

        ResponseItem::CustomToolCall {
            call_id,
            name,
            input,
            ..
        } => Some(ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(vec![OutboundToolCall {
                id: call_id.clone(),
                r#type: "function".to_string(),
                function: OutboundToolCallFunction {
                    name: name.clone(),
                    arguments: input.clone(),
                },
            }]),
        }),

        ResponseItem::CustomToolCallOutput { call_id, output, .. } => Some(ChatMessage {
            role: "tool".to_string(),
            content: output.body.to_text().unwrap_or_default(),
            tool_call_id: Some(call_id.clone()),
            tool_calls: None,
        }),

        // All Responses-API-only item types are silently skipped.
        ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => None,
    }
}

/// Flatten a `Vec<ContentItem>` down to a single plain string.
///
/// Text items are concatenated; image items become an inline placeholder
/// because the Chat Completions plain-string format has no image embedding.
fn flatten_content_items(items: &[ContentItem]) -> String {
    let parts: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if text.is_empty() { None } else { Some(text.as_str()) }
            }
            ContentItem::InputImage { image_url, .. } => {
                // Images cannot be represented as plain strings; use a placeholder.
                let _ = image_url;
                Some("[image]")
            }
        })
        .collect();
    parts.join("\n")
}

/// Re-wrap a Responses API tool definition into the Chat Completions shape.
///
/// `type = "function"` tools are re-wrapped into the nested Chat Completions
/// shape. The `apply_patch` freeform/custom tool is special-cased into a
/// function tool so Chat Completions providers can call it natively. All other
/// non-function types are dropped.
fn rewrap_tool_for_chat_completions(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    let tool_type = obj.get("type")?.as_str()?;

    // apply_patch is a Freeform/custom tool in the Responses API format, but
    // Chat Completions providers need it as a proper function tool. Convert it
    // so the model can call it via the native tool_calls mechanism.
    if tool_type == "custom" {
        if obj.get("name")?.as_str()? == "apply_patch" {
            return Some(serde_json::json!({
                "type": "function",
                "function": {
                    "name": "apply_patch",
                    "description": "Edit files by applying a unified diff patch. Use this to create, modify, or delete files.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "patch": {
                                "type": "string",
                                "description": "Patch content in apply_patch format, starting with '*** Begin Patch' followed by file hunks."
                            }
                        },
                        "required": ["patch"]
                    }
                }
            }));
        }
        return None;
    }

    if tool_type != "function" {
        return None;
    }

    // Build the nested function definition expected by Chat Completions.
    let mut function_def = serde_json::Map::new();
    if let Some(name) = obj.get("name") {
        function_def.insert("name".to_string(), name.clone());
    }
    if let Some(description) = obj.get("description") {
        function_def.insert("description".to_string(), description.clone());
    }
    if let Some(parameters) = obj.get("parameters") {
        function_def.insert("parameters".to_string(), parameters.clone());
    }

    Some(Value::Object(serde_json::Map::from_iter([
        ("type".to_string(), Value::String("function".to_string())),
        (
            "function".to_string(),
            Value::Object(function_def),
        ),
    ])))
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;

    fn make_request(instructions: &str, input: Vec<ResponseItem>) -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "sarvam-m".to_string(),
            instructions: instructions.to_string(),
            input,
            tools: vec![],
            tool_choice: "auto".to_string(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        }
    }

    #[test]
    fn system_message_from_instructions() {
        let req = make_request("You are a helpful assistant.", vec![]);
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages.len(), 1);
        assert_eq!(cc.messages[0].role, "system");
        assert_eq!(cc.messages[0].content, "You are a helpful assistant.");
    }

    #[test]
    fn empty_instructions_omitted() {
        let req = make_request("", vec![]);
        let cc = responses_to_chat_completions_request(&req);
        assert!(cc.messages.is_empty());
    }

    #[test]
    fn simple_user_message_flattened() {
        let req = make_request(
            "",
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "Hello, world!".to_string(),
                }],
                phase: None,
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages.len(), 1);
        assert_eq!(cc.messages[0].role, "user");
        assert_eq!(cc.messages[0].content, "Hello, world!");
    }

    #[test]
    fn multi_part_content_joined() {
        let req = make_request(
            "",
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "Part one.".to_string(),
                    },
                    ContentItem::InputText {
                        text: "Part two.".to_string(),
                    },
                ],
                phase: None,
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages[0].content, "Part one.\nPart two.");
    }

    #[test]
    fn image_item_becomes_placeholder() {
        let req = make_request(
            "",
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputImage {
                    image_url: "http://example.com/img.png".to_string(),
                    detail: None,
                }],
                phase: None,
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages[0].content, "[image]");
    }

    #[test]
    fn function_call_becomes_assistant_tool_calls() {
        let req = make_request(
            "",
            vec![ResponseItem::FunctionCall {
                id: None,
                name: "my_tool".to_string(),
                namespace: None,
                arguments: r#"{"x":1}"#.to_string(),
                call_id: "call_abc".to_string(),
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages[0].role, "assistant");
        assert_eq!(cc.messages[0].content, "");
        let tool_calls = cc.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].id, "call_abc");
        assert_eq!(tool_calls[0].function.name, "my_tool");
        assert_eq!(tool_calls[0].function.arguments, r#"{"x":1}"#);
    }

    #[test]
    fn function_call_output_becomes_tool_message() {
        let req = make_request(
            "",
            vec![ResponseItem::FunctionCallOutput {
                call_id: "call_abc".to_string(),
                output: FunctionCallOutputPayload::from_text("result text".to_string()),
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages[0].role, "tool");
        assert_eq!(cc.messages[0].content, "result text");
        assert_eq!(
            cc.messages[0].tool_call_id.as_deref(),
            Some("call_abc")
        );
    }

    #[test]
    fn multiturn_with_tool_call() {
        let req = make_request(
            "sys",
            vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "run something".to_string(),
                    }],
                    phase: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{}".to_string(),
                    call_id: "c1".to_string(),
                },
                ResponseItem::FunctionCallOutput {
                    call_id: "c1".to_string(),
                    output: FunctionCallOutputPayload::from_text("ok".to_string()),
                },
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "Done!".to_string(),
                    }],
                    phase: None,
                },
            ],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.messages.len(), 5); // system + user + assistant(tool) + tool + assistant
        assert_eq!(cc.messages[0].role, "system");
        assert_eq!(cc.messages[1].role, "user");
        assert_eq!(cc.messages[2].role, "assistant");
        assert!(cc.messages[2].tool_calls.is_some());
        assert_eq!(cc.messages[3].role, "tool");
        assert_eq!(cc.messages[4].role, "assistant");
        assert_eq!(cc.messages[4].content, "Done!");
    }

    #[test]
    fn reasoning_items_skipped() {
        let req = make_request(
            "",
            vec![ResponseItem::Reasoning {
                id: "r1".to_string(),
                summary: vec![],
                content: None,
                encrypted_content: None,
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert!(cc.messages.is_empty());
    }

    #[test]
    fn tool_rewrapped_correctly() {
        let responses_tool = serde_json::json!({
            "type": "function",
            "name": "get_weather",
            "description": "Get weather",
            "strict": false,
            "parameters": {"type": "object", "properties": {}}
        });
        let mut req = make_request("", vec![]);
        req.tools = vec![responses_tool];
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.tools.len(), 1);
        let tool = &cc.tools[0];
        assert_eq!(tool["type"], "function");
        let func = &tool["function"];
        assert_eq!(func["name"], "get_weather");
        assert_eq!(func["description"], "Get weather");
        assert!(func.get("strict").is_none(), "strict must not be forwarded");
        assert!(func["parameters"].is_object());
    }

    #[test]
    fn non_function_tools_dropped() {
        let responses_tools = vec![
            serde_json::json!({"type": "local_shell"}),
            serde_json::json!({"type": "web_search"}),
            serde_json::json!({
                "type": "function",
                "name": "kept",
                "description": "",
                "parameters": {}
            }),
        ];
        let mut req = make_request("", vec![]);
        req.tools = responses_tools;
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.tools.len(), 1);
        assert_eq!(cc.tools[0]["function"]["name"], "kept");
    }

    #[test]
    fn apply_patch_freeform_converted_to_function_tool() {
        // apply_patch is a Freeform/custom tool in Responses API format.
        // It must be converted to a proper function tool for Chat Completions
        // providers so the model can call it via the native tool_calls mechanism.
        let apply_patch_tool = serde_json::json!({
            "type": "custom",
            "name": "apply_patch",
        });
        let mut req = make_request("", vec![]);
        req.tools = vec![apply_patch_tool];
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.tools.len(), 1, "apply_patch must survive the filter");
        let tool = &cc.tools[0];
        assert_eq!(tool["type"], "function");
        let func = &tool["function"];
        assert_eq!(func["name"], "apply_patch");
        let params = &func["parameters"];
        assert!(params["properties"]["patch"].is_object(), "must have a patch parameter");
        assert_eq!(params["required"][0], "patch");
    }

    #[test]
    fn other_custom_tools_still_dropped() {
        let tools = vec![
            serde_json::json!({"type": "custom", "name": "some_other_custom"}),
            serde_json::json!({"type": "custom", "name": "apply_patch"}),
        ];
        let mut req = make_request("", vec![]);
        req.tools = tools;
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.tools.len(), 1, "only apply_patch should survive");
        assert_eq!(cc.tools[0]["function"]["name"], "apply_patch");
    }

    #[test]
    fn stream_options_included_when_streaming() {
        let req = make_request("", vec![]);
        let cc = responses_to_chat_completions_request(&req);
        assert!(cc.stream_options.is_some());
        assert!(cc.stream_options.unwrap().include_usage);
    }
}
