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
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .flat_map(rewrap_tool_for_chat_completions)
        .collect();

    // If `shell_command` is in the toolset, also advertise dedicated
    // `read_file` / `glob` / `grep` tools. These are typed wrappers that the
    // SSE processor rewrites into `shell_command` calls — modelled on opencode,
    // which sees noticeably better tool-use reliability on smaller models when
    // search and read are typed tools instead of free-form shell commands.
    let has_shell = tools.iter().any(|t| {
        t.get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            == Some("shell_command")
    });
    if has_shell {
        tools.extend(synth_simple_read_search_tools());
    }

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
                    arguments: ensure_json_arguments(arguments),
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
                    // Custom tool inputs are raw payloads (e.g. the apply_patch
                    // envelope), not JSON. Chat Completions requires `arguments`
                    // to be a valid JSON-encoded string, so wrap the payload as
                    // {"input": <text>}. Sarvam only schema-validates the
                    // outbound tool definitions; for echoed history it just
                    // needs the field to parse as JSON.
                    arguments: serde_json::to_string(
                        &serde_json::json!({ "input": input }),
                    )
                    .unwrap_or_else(|_| "{}".to_string()),
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
        | ResponseItem::CompactionTrigger
        | ResponseItem::Other => None,
    }
}

/// Ensure a tool-call `arguments` string is a valid JSON-encoded value.
///
/// Sarvam rejects requests where `tool_calls[*].function.arguments` does not
/// parse as JSON ("'arguments' must be a valid JSON-encoded string"). The
/// model occasionally emits malformed pseudo-JSON or plain text; we cache it
/// and replay it back on the next turn. Wrap any non-JSON payload so the
/// echoed history stays well-formed without losing the original text.
fn ensure_json_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return "{}".to_string();
    }
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return arguments.to_string();
    }
    serde_json::to_string(&serde_json::json!({ "raw_arguments": arguments }))
        .unwrap_or_else(|_| "{}".to_string())
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
/// - `type = "function"` tools are re-wrapped into the nested Chat Completions
///   shape (one tool in → one tool out).
/// - The `apply_patch` freeform/custom tool is replaced by two simpler
///   function tools (`edit_file`, `write_file`) modeled on opencode. Asking
///   smaller models (Sarvam, Kimi, etc.) to embed a full `*** Begin Patch ...`
///   envelope inside a JSON `arguments` string is fragile; the wire-level
///   error `'arguments' must be a valid JSON-encoded string` is the typical
///   failure mode. The SSE processor synthesises an apply_patch payload from
///   the simpler args, so the core handler is unchanged.
/// - All other non-function types are dropped.
fn rewrap_tool_for_chat_completions(tool: &Value) -> Vec<Value> {
    let Some(obj) = tool.as_object() else {
        return Vec::new();
    };
    let Some(tool_type) = obj.get("type").and_then(|v| v.as_str()) else {
        return Vec::new();
    };

    if tool_type == "custom" {
        let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name == "apply_patch" {
            return synth_simple_edit_tools();
        }
        return Vec::new();
    }

    if tool_type != "function" {
        return Vec::new();
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

    vec![Value::Object(serde_json::Map::from_iter([
        ("type".to_string(), Value::String("function".to_string())),
        (
            "function".to_string(),
            Value::Object(function_def),
        ),
    ]))]
}

/// Two function tools that stand in for `apply_patch` on Chat Completions
/// providers. The SSE processor converts incoming `edit_file` / `write_file`
/// tool calls into an `apply_patch` payload before forwarding to core, so the
/// existing handler picks them up unchanged.
fn synth_simple_edit_tools() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Edit an existing file by replacing one exact substring with another. The match must be unique in the file unless replace_all is true. For creating a new file, use write_file instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path to the file to edit, relative to the workspace root."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The exact substring to replace, including any surrounding context needed to make it unique."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The replacement text. Must differ from old_string."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "If true, replace every occurrence of old_string. Defaults to false (the match must be unique)."
                        }
                    },
                    "required": ["file_path", "old_string", "new_string"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create a new file with the given contents. Fails if the file already exists; use edit_file to modify existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path of the file to create, relative to the workspace root."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full text content for the new file."
                        }
                    },
                    "required": ["file_path", "content"]
                }
            }
        }),
    ]
}

/// Build an apply_patch payload from `edit_file` arguments.
///
/// Returns `Err` with a human-readable reason if the arguments are missing
/// required fields or malformed. The caller (the SSE processor) is expected
/// to forward the patch text as a `CustomToolCall { name: "apply_patch", input }`
/// so the core handler can verify and apply it.
pub(crate) fn synth_apply_patch_from_edit_file(args_json: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("edit_file arguments are not valid JSON: {e}"))?;
    let file_path = v
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| "edit_file requires `file_path`".to_string())?;
    let old_string = v
        .get("old_string")
        .and_then(Value::as_str)
        .ok_or_else(|| "edit_file requires `old_string`".to_string())?;
    let new_string = v
        .get("new_string")
        .and_then(Value::as_str)
        .ok_or_else(|| "edit_file requires `new_string`".to_string())?;
    let mut out = String::new();
    out.push_str("*** Begin Patch\n");
    out.push_str("*** Update File: ");
    out.push_str(file_path);
    out.push('\n');
    out.push_str("@@\n");
    for line in old_string.split('\n') {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new_string.split('\n') {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("*** End Patch\n");
    Ok(out)
}

/// Build an apply_patch payload from `write_file` arguments (creates a new file).
pub(crate) fn synth_apply_patch_from_write_file(args_json: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("write_file arguments are not valid JSON: {e}"))?;
    let file_path = v
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| "write_file requires `file_path`".to_string())?;
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "write_file requires `content`".to_string())?;
    let mut out = String::new();
    out.push_str("*** Begin Patch\n");
    out.push_str("*** Add File: ");
    out.push_str(file_path);
    out.push('\n');
    // Add File expects every line prefixed with '+'. Preserve a trailing
    // newline if present by emitting an empty '+' line.
    let has_trailing_nl = content.ends_with('\n');
    let body = if has_trailing_nl {
        &content[..content.len() - 1]
    } else {
        content
    };
    for line in body.split('\n') {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    if has_trailing_nl {
        out.push_str("+\n");
    }
    out.push_str("*** End Patch\n");
    Ok(out)
}

/// Three function tools that wrap `shell_command` for common read/search
/// operations: `read_file`, `glob`, `grep`. The SSE processor rewrites each
/// inbound tool call into a `shell_command` invocation with the right
/// `sed`/`rg` invocation, so the core handler is unchanged.
fn synth_simple_read_search_tools() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file. Optionally limit to a line range. For full files under a few thousand lines, omit offset and limit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path to the file, relative to the workspace root."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "1-based line number to start reading from. Defaults to 1."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read. Defaults to the whole file."
                        }
                    },
                    "required": ["file_path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "List files matching a glob pattern (e.g. \"src/**/*.rs\"). Faster and more reliable than asking the model to formulate a `find`/`rg --files` shell invocation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match, e.g. \"**/*.ts\" or \"src/*.rs\"."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search in. Defaults to the workspace root."
                        }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search file contents using a regex pattern (ripgrep). Returns matching lines with file:line prefixes.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory or file to search in. Defaults to the workspace root."
                        },
                        "include": {
                            "type": "string",
                            "description": "Optional glob to restrict which files are searched, e.g. \"*.rs\" or \"src/**/*.{ts,tsx}\"."
                        }
                    },
                    "required": ["pattern"]
                }
            }
        }),
    ]
}

/// Shell-quote a string for safe inclusion in a single-quoted command segment.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build a `shell_command` JSON `arguments` string from `read_file` args.
pub(crate) fn synth_shell_command_from_read_file(args_json: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("read_file arguments are not valid JSON: {e}"))?;
    let file_path = v
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| "read_file requires `file_path`".to_string())?;
    let offset = v.get("offset").and_then(Value::as_i64);
    let limit = v.get("limit").and_then(Value::as_i64);
    let quoted_path = shell_single_quote(file_path);
    let command = match (offset, limit) {
        (None, None) => format!("cat -- {quoted_path}"),
        (Some(start), None) => {
            let start = start.max(1);
            format!("sed -n '{start},$p' -- {quoted_path}")
        }
        (None, Some(n)) => {
            let n = n.max(1);
            format!("sed -n '1,{n}p' -- {quoted_path}")
        }
        (Some(start), Some(n)) => {
            let start = start.max(1);
            let n = n.max(1);
            let end = start.saturating_add(n - 1);
            format!("sed -n '{start},{end}p' -- {quoted_path}")
        }
    };
    Ok(serde_json::to_string(&serde_json::json!({ "command": command }))
        .unwrap_or_else(|_| "{}".to_string()))
}

/// Build a `shell_command` JSON `arguments` string from `glob` args.
pub(crate) fn synth_shell_command_from_glob(args_json: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("glob arguments are not valid JSON: {e}"))?;
    let pattern = v
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| "glob requires `pattern`".to_string())?;
    let path = v.get("path").and_then(Value::as_str);
    let quoted_pattern = shell_single_quote(pattern);
    let mut command = format!("rg --files --hidden -g {quoted_pattern}");
    if let Some(path) = path {
        command.push(' ');
        command.push_str(&shell_single_quote(path));
    }
    Ok(serde_json::to_string(&serde_json::json!({ "command": command }))
        .unwrap_or_else(|_| "{}".to_string()))
}

/// Build a `shell_command` JSON `arguments` string from `grep` args.
pub(crate) fn synth_shell_command_from_grep(args_json: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("grep arguments are not valid JSON: {e}"))?;
    let pattern = v
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| "grep requires `pattern`".to_string())?;
    let path = v.get("path").and_then(Value::as_str);
    let include = v.get("include").and_then(Value::as_str);
    let mut command = format!("rg -n --color never {}", shell_single_quote(pattern));
    if let Some(include) = include {
        command.push_str(" -g ");
        command.push_str(&shell_single_quote(include));
    }
    if let Some(path) = path {
        command.push(' ');
        command.push_str(&shell_single_quote(path));
    }
    Ok(serde_json::to_string(&serde_json::json!({ "command": command }))
        .unwrap_or_else(|_| "{}".to_string()))
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
    fn apply_patch_custom_is_replaced_by_edit_and_write_tools() {
        // The apply_patch freeform/custom tool is swapped for two simpler
        // function tools (edit_file, write_file). The SSE processor turns
        // their tool_calls back into apply_patch payloads.
        let apply_patch_tool = serde_json::json!({
            "type": "custom",
            "name": "apply_patch",
        });
        let mut req = make_request("", vec![]);
        req.tools = vec![apply_patch_tool];
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(cc.tools.len(), 2, "apply_patch is replaced by two simpler tools");
        let names: Vec<&str> = cc
            .tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"write_file"));
        for tool in &cc.tools {
            assert_eq!(tool["type"], "function");
            assert!(tool["function"]["parameters"]["properties"].is_object());
        }
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
        // apply_patch expands to two tools; the other custom is dropped.
        assert_eq!(cc.tools.len(), 2);
        let names: Vec<&str> = cc
            .tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"write_file"));
    }

    #[test]
    fn synth_edit_file_builds_update_patch() {
        let args = r#"{"file_path":"src/lib.rs","old_string":"foo","new_string":"bar"}"#;
        let patch = synth_apply_patch_from_edit_file(args).unwrap();
        assert!(patch.starts_with("*** Begin Patch\n"));
        assert!(patch.contains("*** Update File: src/lib.rs\n"));
        assert!(patch.contains("@@\n-foo\n+bar\n"));
        assert!(patch.ends_with("*** End Patch\n"));
    }

    #[test]
    fn synth_edit_file_handles_multiline_strings() {
        let args = r#"{"file_path":"a.txt","old_string":"line1\nline2","new_string":"newA\nnewB"}"#;
        let patch = synth_apply_patch_from_edit_file(args).unwrap();
        assert!(patch.contains("-line1\n-line2\n"));
        assert!(patch.contains("+newA\n+newB\n"));
    }

    #[test]
    fn synth_edit_file_rejects_missing_fields() {
        let err = synth_apply_patch_from_edit_file(r#"{"file_path":"x"}"#).unwrap_err();
        assert!(err.contains("old_string"));
    }

    #[test]
    fn synth_write_file_builds_add_patch() {
        let args = r#"{"file_path":"notes.md","content":"hello\nworld\n"}"#;
        let patch = synth_apply_patch_from_write_file(args).unwrap();
        assert!(patch.starts_with("*** Begin Patch\n"));
        assert!(patch.contains("*** Add File: notes.md\n"));
        assert!(patch.contains("+hello\n+world\n"));
        assert!(patch.ends_with("*** End Patch\n"));
    }

    #[test]
    fn custom_tool_call_arguments_are_json_wrapped_on_replay() {
        // CustomToolCall.input is a raw payload (e.g. the apply_patch envelope),
        // not JSON. When we replay the conversation back to Sarvam, it must be
        // wrapped so that `arguments` parses as a JSON-encoded string.
        let patch = "*** Begin Patch\n*** Update File: x\n@@\n-foo\n+bar\n*** End Patch\n";
        let req = make_request(
            "",
            vec![ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "call_1".to_string(),
                name: "apply_patch".to_string(),
                input: patch.to_string(),
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        let tool_calls = cc.messages[0].tool_calls.as_ref().unwrap();
        let args = &tool_calls[0].function.arguments;
        // Must parse as JSON.
        let parsed: serde_json::Value = serde_json::from_str(args)
            .expect("arguments must be valid JSON");
        assert_eq!(parsed["input"].as_str().unwrap(), patch);
    }

    #[test]
    fn function_call_with_malformed_arguments_is_sanitized() {
        // If the model emits non-JSON `arguments`, we must not echo it back
        // verbatim — Sarvam rejects the whole request.
        let req = make_request(
            "",
            vec![ResponseItem::FunctionCall {
                id: None,
                name: "shell_command".to_string(),
                namespace: None,
                arguments: "command='ls'".to_string(), // not JSON
                call_id: "c1".to_string(),
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        let args = &cc.messages[0].tool_calls.as_ref().unwrap()[0]
            .function
            .arguments;
        let parsed: serde_json::Value =
            serde_json::from_str(args).expect("arguments must be valid JSON");
        assert_eq!(parsed["raw_arguments"].as_str().unwrap(), "command='ls'");
    }

    #[test]
    fn function_call_with_valid_json_is_unchanged() {
        let req = make_request(
            "",
            vec![ResponseItem::FunctionCall {
                id: None,
                name: "shell_command".to_string(),
                namespace: None,
                arguments: r#"{"command":"ls"}"#.to_string(),
                call_id: "c1".to_string(),
            }],
        );
        let cc = responses_to_chat_completions_request(&req);
        assert_eq!(
            cc.messages[0].tool_calls.as_ref().unwrap()[0]
                .function
                .arguments,
            r#"{"command":"ls"}"#
        );
    }

    #[test]
    fn synth_write_file_rejects_invalid_json() {
        let err = synth_apply_patch_from_write_file("not json").unwrap_err();
        assert!(err.to_lowercase().contains("json"));
    }

    #[test]
    fn read_search_tools_are_added_when_shell_command_present() {
        let shell_tool = serde_json::json!({
            "type": "function",
            "name": "shell_command",
            "description": "Run a shell command",
            "parameters": {"type": "object"}
        });
        let mut req = make_request("", vec![]);
        req.tools = vec![shell_tool];
        let cc = responses_to_chat_completions_request(&req);
        let names: Vec<&str> = cc
            .tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"shell_command"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"glob"));
        assert!(names.contains(&"grep"));
    }

    #[test]
    fn read_search_tools_not_added_when_no_shell() {
        let mut req = make_request("", vec![]);
        req.tools = vec![serde_json::json!({
            "type": "function",
            "name": "some_other_tool",
            "description": "x",
            "parameters": {}
        })];
        let cc = responses_to_chat_completions_request(&req);
        let names: Vec<&str> = cc
            .tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"read_file"));
        assert!(!names.contains(&"glob"));
        assert!(!names.contains(&"grep"));
    }

    #[test]
    fn synth_read_file_full_uses_cat() {
        let args = r#"{"file_path":"src/lib.rs"}"#;
        let out = synth_shell_command_from_read_file(args).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["command"].as_str().unwrap(), "cat -- 'src/lib.rs'");
    }

    #[test]
    fn synth_read_file_with_offset_and_limit_uses_sed() {
        let args = r#"{"file_path":"a.txt","offset":10,"limit":5}"#;
        let out = synth_shell_command_from_read_file(args).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["command"].as_str().unwrap(), "sed -n '10,14p' -- 'a.txt'");
    }

    #[test]
    fn synth_read_file_quotes_single_quote_in_path() {
        let args = r#"{"file_path":"weird's name.txt"}"#;
        let out = synth_shell_command_from_read_file(args).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["command"].as_str().unwrap(),
            r"cat -- 'weird'\''s name.txt'"
        );
    }

    #[test]
    fn synth_glob_builds_rg_files_command() {
        let args = r#"{"pattern":"**/*.rs","path":"src"}"#;
        let out = synth_shell_command_from_glob(args).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["command"].as_str().unwrap(),
            "rg --files --hidden -g '**/*.rs' 'src'"
        );
    }

    #[test]
    fn synth_grep_builds_rg_command_with_include() {
        let args = r#"{"pattern":"TODO","path":".","include":"*.rs"}"#;
        let out = synth_shell_command_from_grep(args).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["command"].as_str().unwrap(),
            "rg -n --color never 'TODO' -g '*.rs' '.'"
        );
    }

    #[test]
    fn synth_grep_rejects_missing_pattern() {
        let err = synth_shell_command_from_grep(r#"{"path":"."}"#).unwrap_err();
        assert!(err.contains("pattern"));
    }

    #[test]
    fn stream_options_included_when_streaming() {
        let req = make_request("", vec![]);
        let cc = responses_to_chat_completions_request(&req);
        assert!(cc.stream_options.is_some());
        assert!(cc.stream_options.unwrap().include_usage);
    }
}
