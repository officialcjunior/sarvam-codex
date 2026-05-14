# Sarvam AI Provider Integration for Codex

Successfully implemented `WireApi::ChatCompletions` support in the Codex Rust CLI to enable OpenAI-compatible Chat Completions API providers (specifically Sarvam AI) to work alongside the native Responses API.

This file exists as an outline and not as a comprehensive summary.
---

## What Was Added

### 1. **WireApi Enum Extension** (`model-provider-info/src/lib.rs`)

Added `ChatCompletions` variant to the `WireApi` enum:

```rust
pub enum WireApi {
    #[default]
    Responses,
    ChatCompletions,  // ← NEW: for Chat Completions API
}
```

- Serializes as `"chat_completions"` in TOML configs
- Added validation guard: `ChatCompletions` + `supports_websockets = true` returns an error (Chat Completions doesn't support WebSocket transport)

### 2. **Translation Layer** (`codex-api/src/chat_completions.rs`)

Core function `responses_to_chat_completions_request()` that converts Codex's internal `ResponsesApiRequest` format to Sarvam-compatible Chat Completions:

**Key translations:**
- `instructions` → leading `system` message
- `ResponseItem::Message { role: "developer", ... }` → remapped to `role: "system"` (Sarvam only accepts `assistant`, `system`, `tool`, `user`)
- `ResponseItem::Message { content: Vec<ContentItem> }` → `ChatMessage { content: String }` (flattens array to plain string)
  - `InputText` / `OutputText` → concatenated with `\n`
  - `InputImage` → `[image]` placeholder (images can't be embedded in plain-string content)
- `ResponseItem::FunctionCall` → `ChatMessage { role: "assistant", tool_calls: [...] }`
- `ResponseItem::FunctionCallOutput` → `ChatMessage { role: "tool", ... }`
- Tools re-wrapped: Responses API flat shape `{type,name,description,parameters}` → Chat Completions nested shape `{type:"function", function:{name,description,parameters}}`
- Non-function tools (local_shell, web_search, etc.) → silently dropped (Sarvam only supports `function` tools)
- `req.reasoning.effort` → `reasoning_effort` field (`None/Minimal/Low` → `"low"`, `Medium` → `"medium"`, `High/XHigh` → `"high"`)

**Request types:**
```rust
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<Value>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub stream: bool,
    pub stream_options: Option<StreamOptions>,    // include_usage when streaming
    pub reasoning_effort: Option<String>,         // "low" | "medium" | "high"
}

pub struct ChatMessage {
    pub role: String,
    pub content: String,  // Always plain string — never array
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<Vec<OutboundToolCall>>,
}
```

**Unit Tests (12 tests):** ✅ All passing
- System message extraction
- Content flattening (text, images, multi-part)
- Function call ↔ tool_calls mapping
- Multi-turn conversations with tool calls
- Tool rewrapping (Responses → Chat Completions format)
- Non-function tool filtering

### 3. **SSE Stream Processor** (`codex-api/src/sse/chat_completions.rs`)

`spawn_chat_completions_stream()` and `process_chat_completions_sse()` handle the Chat Completions SSE stream:

**Event processing:**
- **Before any delta:** emits a synthetic `OutputItemAdded` (empty assistant message) so `turn.rs` has an `active_item` set before processing deltas — without this, the first `OutputTextDelta` would panic in debug builds
- Text deltas (`delta.content`) → `ResponseEvent::OutputTextDelta`
- Reasoning deltas (`delta.reasoning_content`) → `ResponseEvent::ReasoningContentDelta` — streams Sarvam's thinking output to the UI
- Tool call deltas (buffered by index) → emitted as `ToolCallInputDelta` for live display
- `[DONE]` sentinel → stream termination; emits final `OutputItemDone` with accumulated text
- `finish_reason` mapping:
  - `"stop"` → `end_turn: Some(true)`
  - `"tool_calls"` → `end_turn: Some(false)`
  - Others → `end_turn: Some(false)`
- Token usage extraction → `TokenUsage` struct

**State management:**
```rust
tool_calls_buf: BTreeMap<usize, ToolCallBuffer>  // accumulate by index
accumulated_text: String                          // gather text chunks
text_item_id: Option<String>                      // ID of synthetic OutputItemAdded
reasoning_content_index: i64                      // block index for reasoning deltas
finish_reason: Option<String>                     // from last choice
usage: Option<ChunkUsage>                         // from final chunk
```

**Event ordering (critical):**

The Responses API emits `OutputItemAdded` *before* any deltas. Chat Completions has no equivalent event — the SSE processor synthesises one on the first incoming delta (text or reasoning), so the ordering seen by `turn.rs` is always:

```
OutputItemAdded  ← synthetic, emitted on first delta
OutputTextDelta  (× N)
ReasoningContentDelta  (× M, if model reasoned)
OutputItemDone   ← with full accumulated text
Completed
```

### 4. **HTTP Client** (`codex-api/src/endpoint/chat_completions.rs`)

`ChatCompletionsClient<T>` struct (modelled on `ResponsesClient`):

```rust
pub async fn stream_request(
    &self,
    request: ResponsesApiRequest,
    options: ChatCompletionsOptions,
) -> Result<ResponseStream, ApiError>
```

- POSTs to `chat/completions` endpoint
- Sets `Accept: text/event-stream` header
- Internally calls `responses_to_chat_completions_request()` to translate the request
- Spawns the SSE processor

### 5. **Dispatch in Core** (`core/src/client.rs`)

Added new method `stream_chat_completions_api()` and extended the `stream()` method:

```rust
match wire_api {
    WireApi::Responses => { /* existing code */ }
    WireApi::ChatCompletions => {
        self.stream_chat_completions_api(
            prompt, model_info, session_telemetry,
            effort, summary, service_tier, inference_trace
        ).await
    }
}
```

### 6. **Module Registration**

Updated:
- `codex-api/src/lib.rs` — added module declarations and re-exports
- `codex-api/src/endpoint/mod.rs` — registered `chat_completions` submodule
- `codex-api/src/sse/mod.rs` — registered SSE processor

### 7. **Integration Tests** (`codex-api/tests/integration_chat_completions.rs`)

Four integration tests: ✅ All passing

```
✓ chat_completions_request_serializes_correctly
✓ content_flattening_respects_sarvam_constraints
✓ tool_calls_formatted_for_sarvam
✓ tool_rewrapping_from_responses_to_chat_completions
```

## Configuration

Sarvam is a **built-in provider** — no `[model_providers.sarvam]` block
is needed. The provider definition (base URL, env key, wire API) and its
model catalog are compiled into the binary.

Add only this to `~/.codex/config.toml`:

```toml
model_provider = "sarvam"
model = "sarvam-105b"    # or "sarvam-30b"
```

Then set the environment variable:
```bash
export SARVAM_API_KEY="sk_..."
codex "your prompt here"
```

Model and reasoning effort can also be switched live in the TUI's
pickers — selections are persisted back to `~/.codex/config.toml` via
`ConfigEdit::SetModel` (it writes the `model` and
`model_reasoning_effort` keys).

### Built-in model catalog

| Slug          | Context | Default effort | Reasoning levels |
|---------------|---------|----------------|------------------|
| `sarvam-105b` | 128K    | medium         | low / medium / high |
| `sarvam-30b`  | 64K     | medium         | low / medium / high |

Defined in `codex-rs/model-provider/src/sarvam/catalog.rs`. Both models
are text-only, do not support parallel tool calls, and have web search /
image generation disabled via `ProviderCapabilities`.

### Sarvam-specific system prompt

`codex-rs/model-provider/src/sarvam/prompt.md` is prepended to the
shared `BASE_INSTRUCTIONS` via `include_str!`. It tells the model to
call tools via the API's native `tool_calls` mechanism rather than
emitting XML-style `<function-calls>` tags or inline JSON tool-call
text. Reason it exists: `sarvam-105b` was mimicking the Freeform
`apply_patch` JSON example shown in the shared `prompt.md` and
emitting tool calls as text content, which the SSE processor then
rendered as visible bullets instead of executing.

---

## Example Chat Completions Request

Here's what Codex sends to Sarvam:

```json
{
  "model": "sarvam-30b",
  "messages": [
    {
      "role": "system",
      "content": "You are helpful."
    },
    {
      "role": "user",
      "content": "Hello"
    }
  ],
  "tool_choice": "auto",
  "parallel_tool_calls": false,
  "stream": true,
  "stream_options": {
    "include_usage": true
  },
  "reasoning_effort": "medium"
}
```

Note the key constraints:
- All `content` fields are plain strings, never arrays — required by Sarvam
- `"developer"` role is remapped to `"system"` before sending
- `reasoning_effort` is included when Codex has a reasoning effort configured

---

## Build Status

| Component | Status | Notes |
|-----------|--------|-------|
| `codex-api` | ✅ Builds | No warnings |
| `codex-model-provider-info` | ✅ Builds | Clean |
| `codex-core` | ✅ Builds | Clean |
| Unit tests (12) | ✅ All pass | `cargo test -p codex-api --lib chat_completions` |
| Integration tests (4) | ✅ All pass | `cargo test --test integration_chat_completions` |

---

## Prompt & Tool Journey

How the system prompt and tools travel from source files to the Sarvam API.

### System Prompt

```
model-provider/src/sarvam/prompt.md          ← Sarvam-specific preamble (tool calling rules,
  include_str!() at compile time               apply_patch override). Edit this file to change
                                               Sarvam behavior without touching Rust.
        +
models-manager/prompt.md                     ← Shared base agent prompt (all providers).
  compiled in as BASE_INSTRUCTIONS             OpenAI models get their prompt from OpenAI's
                                               servers instead; this is the local fallback.
        ↓
model-provider/src/sarvam/catalog.rs         ← format!("{SARVAM_PROMPT_PREFIX}{BASE_INSTRUCTIONS}")
  ModelInfo.base_instructions                  stored in the static catalog entry.
        ↓
core/src/client.rs                           ← base_instructions → ResponsesApiRequest.instructions
        ↓
codex-api/src/chat_completions.rs            ← instructions → {"role":"system","content":"..."}
  responses_to_chat_completions_request()      prepended as the first message.
        ↓
POST https://api.sarvam.ai/v1/chat/completions
```

### Tools

Tools reach the model alongside the messages as a separate `tools` array:

```
core/src/tools/handlers/                     ← Each handler exposes a ToolSpec.
  shell/shell_command.rs  → ToolSpec::Function("shell_command")   ✅ passes through
  apply_patch.rs          → ToolSpec::Freeform ("apply_patch")    ← special-cased below
  (local_shell, web_search, etc.)                                  ❌ dropped

        ↓
codex-api/src/chat_completions.rs
  rewrap_tool_for_chat_completions()
    • type="function"  → rewrapped into {type:"function",function:{name,description,parameters}}
    • type="custom", name="apply_patch" → synthesised function tool with patch: string parameter
    • all others       → dropped

        ↓
POST .../chat/completions  →  "tools": [...]
```

### apply_patch round-trip (inbound)

When Sarvam calls `apply_patch` back via `tool_calls`, the SSE processor re-routes it:

```
SSE tool_calls delta  →  buf.name == "apply_patch"
        ↓
codex-api/src/sse/chat_completions.rs        ← extracts buf.args["patch"], emits as
  process_chat_completions_sse()               ResponseItem::CustomToolCall {input: patch}
        ↓
core/src/tools/router.rs                     ← routes to ToolPayload::Custom {input}
        ↓
core/src/tools/handlers/apply_patch.rs       ← existing handler, no changes needed
```

### Reasoning tokens

Sarvam streams thinking steps in `delta.reasoning_content`. The SSE processor forwards them
as `ReasoningContentDelta` events → TUI renders them only when `show_raw_agent_reasoning` is
true (set via `--oss` flag). Summary-level reasoning is always shown; raw chain-of-thought is
OSS-only by design.

---

## Why This Approach?

### Problem

Codex exclusively speaks the OpenAI Responses API (`POST /v1/responses`), which has:
- Complex internal request format (`ResponsesApiRequest` with `Vec<ContentItem>` arrays)
- Complex SSE event types (`response.output_text.delta`, `response.completed`, etc.)

Sarvam AI only supports Chat Completions API (`POST /v1/chat/completions`), which:
- Uses plain strings for message content (no arrays of parts)
- Uses standard Chat Completions SSE format (`choices[0].delta.content`)

### Solution

Rather than route requests through an external translation proxy, we added native Chat Completions support directly in Codex:

1. **One-way translation** — `ResponsesApiRequest` → `ChatCompletionsRequest`
   - Happens once per request
   - Role remapping handles `developer` → `system` at translation time
2. **Symmetric SSE parsing** — Chat Completions stream → `ResponseEvent`
   - Maps Chat Completions events back to Codex's unified event format
   - Synthesises required `OutputItemAdded` ordering
   - All downstream code sees the same `ResponseEvent` stream regardless of `wire_api`
3. **Config-driven dispatch** — `wire_api = "chat_completions"` in TOML
   - No binary changes needed; users just configure their provider
   - Existing Responses API path unaffected

### Trade-offs

✅ **Advantages:**
- No proxy overhead
- Sarvam API fully supported (no flattening workarounds in separate tools)
- Reasoning output (`reasoning_content`) streamed and displayed natively
- `reasoning_effort` forwarded from Codex config to Sarvam request
- Clean separation of concerns (translation logic isolated)
- Extensible: adding other Chat Completions providers is a config change

❌ **Limitations:**
- Context compaction is not available when using Chat Completions
- Chat Completions doesn't support WebSocket transport — enforced at validation time with a clear error message

---

## Files Modified/Created

| File | Status | Purpose |
|------|--------|---------|
| `model-provider-info/src/lib.rs` | Modified | `WireApi::ChatCompletions` variant + validation + built-in Sarvam provider registration |
| `codex-api/src/chat_completions.rs` | Created | Translation layer + types + 12 unit tests |
| `codex-api/src/sse/chat_completions.rs` | Created | SSE processor |
| `codex-api/src/endpoint/chat_completions.rs` | Created | HTTP client |
| `codex-api/tests/integration_chat_completions.rs` | Created | 4 integration tests |
| `codex-api/src/lib.rs` | Modified | Module registration + re-exports |
| `codex-api/src/endpoint/mod.rs` | Modified | Submodule declaration |
| `codex-api/src/sse/mod.rs` | Modified | Submodule declaration |
| `core/src/client.rs` | Modified | Imports + `stream_chat_completions_api()` + dispatch |
| `model-provider/src/sarvam/catalog.rs` | Created | Static catalog with `sarvam-30b` and `sarvam-105b` |
| `model-provider/src/sarvam/mod.rs` | Created | `SarvamModelProvider` (no-auth, static catalog) |
| `model-provider/src/sarvam/prompt.md` | Created | Sarvam-specific tool-calling preamble (`include_str!`) |
| `model-provider/src/lib.rs` | Modified | `mod sarvam` |
| `model-provider/src/provider.rs` | Modified | `is_sarvam()` dispatch in `create_model_provider()` |

---

## Testing Checklist

- ✅ Unit tests: 12/12 passing (content flattening, tool mapping, etc.)
- ✅ Integration tests: 4/4 passing (full request→JSON serialization, Sarvam compliance)
- ✅ Compilation: Zero errors, clean builds for `codex-api`, `codex-model-provider-info`, `codex-core`
- ✅ Config validation: Detects `ChatCompletions` + `supports_websockets` conflict
- ✅ Bug fix: `developer` role remapped to `system` before sending to Sarvam
- ✅ Bug fix: `OutputItemAdded` emitted before first delta to prevent crash in `turn.rs`
- ✅ Reasoning: `reasoning_content` deltas parsed and forwarded as `ReasoningContentDelta` events
- ✅ Reasoning: `reasoning_effort` mapped from Codex effort config and sent in request

---

## References

- **Sarvam API spec:** `chat-completion-sarvam.md` (in repo root)
- **OpenAI Chat Completions:** https://platform.openai.com/docs/api-reference/chat/create
