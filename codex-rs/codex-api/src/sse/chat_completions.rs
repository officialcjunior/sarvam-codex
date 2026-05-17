//! SSE stream processor for the OpenAI-compatible Chat Completions API.

use crate::chat_completions::ChunkUsage;
use crate::chat_completions::ChatCompletionsChunk;
use crate::chat_completions::synth_apply_patch_from_edit_file;
use crate::chat_completions::synth_apply_patch_from_write_file;
use crate::chat_completions::synth_shell_command_from_glob;
use crate::chat_completions::synth_shell_command_from_grep;
use crate::chat_completions::synth_shell_command_from_read_file;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

/// Accumulated state for a single in-progress tool call.
#[derive(Default)]
struct ToolCallBuffer {
    id: String,
    name: String,
    args: String,
}

/// Spawn an async task that reads the Chat Completions SSE stream and sends
/// `ResponseEvent`s over the returned `ResponseStream`.
pub fn spawn_chat_completions_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    let upstream_request_id = stream_response
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(process_chat_completions_sse(
        stream_response.bytes,
        tx_event,
        idle_timeout,
        telemetry,
    ));

    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

pub async fn process_chat_completions_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) {
    let mut stream = stream.eventsource();
    let mut response_id = String::new();
    let mut tool_calls_buf: BTreeMap<usize, ToolCallBuffer> = BTreeMap::new();
    let mut accumulated_text = String::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<ChunkUsage> = None;
    let mut _received_done = false;
    // ID for the synthetic assistant message item; set on first text/reasoning delta.
    let mut text_item_id: Option<String> = None;
    // Index counter for reasoning content blocks (Sarvam streams reasoning_content deltas).
    let mut reasoning_content_index: i64 = 0;

    loop {
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }

        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                // Stream closed — treat like [DONE] if not already handled.
                break;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(
                        "idle timeout waiting for SSE".to_string(),
                    )))
                    .await;
                return;
            }
        };

        trace!("chat_completions SSE: {}", &sse.data);

        if sse.data == "[DONE]" {
            _received_done = true;
            break;
        }

        // Surface upstream error objects (e.g. {"error":{"message":"..."}}).
        let chunk: ChatCompletionsChunk = match serde_json::from_str(&sse.data) {
            Ok(c) => c,
            Err(parse_err) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&sse.data) {
                    if let Some(msg) = val
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                    {
                        let _ = tx_event
                            .send(Err(ApiError::Stream(msg.to_string())))
                            .await;
                        return;
                    }
                }
                debug!(
                    "Failed to parse chat completions SSE chunk: {parse_err}, data: {}",
                    &sse.data
                );
                continue;
            }
        };

        // Capture the response ID from the first chunk that has one.
        if response_id.is_empty() {
            if let Some(id) = &chunk.id {
                if !id.is_empty() {
                    response_id = id.clone();
                }
            }
        }

        // Capture usage from the final usage-only chunk (when stream_options
        // include_usage is true, the last chunk before [DONE] carries usage).
        if let Some(u) = chunk.usage {
            usage = Some(u);
        }

        for choice in &chunk.choices {
            let delta = &choice.delta;

            // Text delta
            if let Some(content) = &delta.content {
                if !content.is_empty() {
                    // Before the first delta, emit OutputItemAdded so that
                    // turn.rs has an active_item when it processes deltas.
                    if text_item_id.is_none() {
                        let id = if response_id.is_empty() {
                            "chat_completions_msg_0".to_string()
                        } else {
                            format!("{response_id}_msg_0")
                        };
                        text_item_id = Some(id.clone());
                        let placeholder = ResponseItem::Message {
                            id: Some(id),
                            role: "assistant".to_string(),
                            content: vec![],
                            phase: None,
                        };
                        if tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(placeholder)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    accumulated_text.push_str(content);
                    if tx_event
                        .send(Ok(ResponseEvent::OutputTextDelta(content.clone())))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }

            // Reasoning content delta (Sarvam-specific field)
            if let Some(reasoning) = &delta.reasoning_content {
                if !reasoning.is_empty() {
                    // Ensure OutputItemAdded fires before any delta.
                    if text_item_id.is_none() {
                        let id = if response_id.is_empty() {
                            "chat_completions_msg_0".to_string()
                        } else {
                            format!("{response_id}_msg_0")
                        };
                        text_item_id = Some(id.clone());
                        let placeholder = ResponseItem::Message {
                            id: Some(id),
                            role: "assistant".to_string(),
                            content: vec![],
                            phase: None,
                        };
                        if tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(placeholder)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::ReasoningContentDelta {
                            delta: reasoning.clone(),
                            content_index: reasoning_content_index,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    reasoning_content_index = reasoning_content_index.saturating_add(1);
                }
            }

            // Tool call deltas
            if let Some(tc_deltas) = &delta.tool_calls {
                for tc_delta in tc_deltas {
                    let buf = tool_calls_buf.entry(tc_delta.index).or_default();

                    if let Some(id) = &tc_delta.id {
                        if !id.is_empty() {
                            buf.id = id.clone();
                        }
                    }

                    if let Some(func) = &tc_delta.function {
                        if let Some(name) = &func.name {
                            buf.name.push_str(name);
                        }
                        if let Some(args_fragment) = &func.arguments {
                            buf.args.push_str(args_fragment);
                            // Stream incremental tool-input for live display.
                            let item_id = if buf.id.is_empty() {
                                format!("tool_call_{}", tc_delta.index)
                            } else {
                                buf.id.clone()
                            };
                            if tx_event
                                .send(Ok(ResponseEvent::ToolCallInputDelta {
                                    item_id,
                                    call_id: None,
                                    delta: args_fragment.clone(),
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }

            if let Some(fr) = &choice.finish_reason {
                if !fr.is_empty() {
                    finish_reason = Some(fr.clone());
                }
            }
        }
    }

    // Emit the assembled assistant text message (if any).
    if !accumulated_text.is_empty() {
        let id = text_item_id; // already emitted OutputItemAdded with this ID during streaming
        let item = ResponseItem::Message {
            id,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: accumulated_text,
            }],
            phase: None,
        };
        // OutputItemAdded was already sent before the first delta; only send Done.
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await;
    }

    // Emit assembled tool call items.
    for (_, buf) in tool_calls_buf {
        // The apply_patch handler expects ToolPayload::Custom { input } with a
        // raw apply_patch envelope. Chat Completions providers cannot emit a
        // "custom" tool call directly — they speak function calls only. We
        // expose two simpler function tools (edit_file, write_file) and
        // synthesise the apply_patch envelope here, so the core handler is
        // unchanged regardless of provider.
        let item = if buf.name == "edit_file" || buf.name == "write_file" {
            let synth = if buf.name == "edit_file" {
                synth_apply_patch_from_edit_file(&buf.args)
            } else {
                synth_apply_patch_from_write_file(&buf.args)
            };
            match synth {
                Ok(patch) => ResponseItem::CustomToolCall {
                    id: Some(buf.id.clone()),
                    status: None,
                    call_id: buf.id,
                    name: "apply_patch".to_string(),
                    input: patch,
                },
                Err(reason) => {
                    // Surface the error to the model as a function-call result
                    // so it can retry with corrected arguments.
                    debug!("rejecting {} tool_call: {reason}", buf.name);
                    ResponseItem::FunctionCall {
                        id: None,
                        name: buf.name,
                        namespace: None,
                        arguments: buf.args,
                        call_id: buf.id,
                    }
                }
            }
        } else if buf.name == "read_file" || buf.name == "glob" || buf.name == "grep" {
            // Typed read/search tools forwarded through the existing
            // shell_command handler. Synthesise the shell invocation here so
            // the core handler is unchanged.
            let synth = match buf.name.as_str() {
                "read_file" => synth_shell_command_from_read_file(&buf.args),
                "glob" => synth_shell_command_from_glob(&buf.args),
                _ => synth_shell_command_from_grep(&buf.args),
            };
            match synth {
                Ok(shell_args) => ResponseItem::FunctionCall {
                    id: None,
                    name: "shell_command".to_string(),
                    namespace: None,
                    arguments: shell_args,
                    call_id: buf.id,
                },
                Err(reason) => {
                    debug!("rejecting {} tool_call: {reason}", buf.name);
                    ResponseItem::FunctionCall {
                        id: None,
                        name: buf.name,
                        namespace: None,
                        arguments: buf.args,
                        call_id: buf.id,
                    }
                }
            }
        } else if buf.name == "apply_patch" {
            // Legacy path: some providers may still emit apply_patch directly
            // (e.g. if the prompt convinced them to). Accept it.
            let patch = serde_json::from_str::<serde_json::Value>(&buf.args)
                .ok()
                .and_then(|v| v.get("patch").and_then(|p| p.as_str()).map(str::to_string))
                .unwrap_or(buf.args);
            ResponseItem::CustomToolCall {
                id: Some(buf.id.clone()),
                status: None,
                call_id: buf.id,
                name: buf.name,
                input: patch,
            }
        } else {
            ResponseItem::FunctionCall {
                id: None,
                name: buf.name,
                namespace: None,
                arguments: buf.args,
                call_id: buf.id,
            }
        };
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item.clone())))
            .await;
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await;
    }

    // Map Chat Completions finish_reason → Responses API end_turn semantics.
    let end_turn = finish_reason.as_deref().map(|fr| match fr {
        "stop" => true,
        "tool_calls" => false,
        "length" | "content_filter" | "function_call" => false,
        _ => false,
    });

    let token_usage = usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        cached_input_tokens: 0,
        output_tokens: u.completion_tokens,
        reasoning_output_tokens: 0,
        total_tokens: u.prompt_tokens + u.completion_tokens,
    });

    let _ = tx_event
        .send(Ok(ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        }))
        .await;
}
