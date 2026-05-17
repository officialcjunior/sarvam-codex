/// Sarvam-specific system prompt, compiled in from prompt.md at build time.
///
/// This is a self-contained prompt — we deliberately do NOT concatenate the
/// shared `BASE_INSTRUCTIONS` (which is OpenAI/Responses-API-oriented and was
/// causing Sarvam to mis-format tool calls, e.g. wrapping `apply_patch` inside
/// a `shell {"command":["apply_patch", ...]}` envelope instead of calling the
/// `apply_patch` function tool directly).
///
/// Edit model-provider/src/sarvam/prompt.md to change Sarvam's behavior — no
/// Rust changes needed.
const SARVAM_PROMPT: &str = include_str!("sarvam-prompt.md");
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::WebSearchToolType;

// Sarvam context windows per API spec:
// sarvam-30b  → 64K tokens
// sarvam-105b → 128K tokens
const SARVAM_30B_CONTEXT_WINDOW: i64 = 64_000;
const SARVAM_105B_CONTEXT_WINDOW: i64 = 128_000;

// Sarvam supports exactly low / medium / high reasoning effort (no minimal / xhigh).
// Our chat_completions translation maps:
//   Codex Low    → "low"
//   Codex Medium → "medium"
//   Codex High   → "high"
fn sarvam_reasoning_levels() -> Vec<ReasoningEffortPreset> {
    vec![
        ReasoningEffortPreset {
            effort: ReasoningEffort::Low,
            description: "Fast responses with lighter reasoning".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::Medium,
            description: "Balances speed and reasoning depth for everyday tasks".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::High,
            description: "Greater reasoning depth for complex problems".to_string(),
        },
    ]
}

pub(crate) fn static_model_catalog() -> ModelsResponse {
    ModelsResponse {
        models: vec![
            // 105B listed first (priority 0) — larger, higher capability.
            sarvam_model(
                "sarvam-105b",
                "Sarvam 105B",
                "128K-context flagship model. Best for complex, long-context coding tasks.",
                SARVAM_105B_CONTEXT_WINDOW,
                /*priority*/ 0,
            ),
            sarvam_model(
                "sarvam-30b",
                "Sarvam 30B",
                "64K-context model. Fast and cost-effective for everyday coding tasks.",
                SARVAM_30B_CONTEXT_WINDOW,
                /*priority*/ 1,
            ),
        ],
    }
}

fn sarvam_model(
    slug: &str,
    display_name: &str,
    description: &str,
    context_window: i64,
    priority: i32,
) -> ModelInfo {
    ModelInfo {
        slug: slug.to_string(),
        display_name: display_name.to_string(),
        description: Some(description.to_string()),
        default_reasoning_level: Some(ReasoningEffort::Medium),
        supported_reasoning_levels: sarvam_reasoning_levels(),
        shell_type: ConfigShellToolType::ShellCommand,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        availability_nux: None,
        upgrade: None,
        base_instructions: SARVAM_PROMPT.to_string(),
        model_messages: None,
        // Chat Completions has no reasoning_summary field.
        supports_reasoning_summaries: false,
        default_reasoning_summary: ReasoningSummary::None,
        // Chat Completions has no verbosity field.
        support_verbosity: false,
        default_verbosity: None,
        // Sarvam accepts apply_patch as a regular function tool call.
        apply_patch_tool_type: Some(ApplyPatchToolType::Freeform),
        // Provider capabilities disable web search; this field controls format only.
        web_search_tool_type: WebSearchToolType::Text,
        truncation_policy: TruncationPolicyConfig::tokens(/*limit*/ 10_000),
        // Our translation layer sends parallel_tool_calls: false.
        supports_parallel_tool_calls: false,
        // Chat Completions content must be plain strings — no image attachments.
        supports_image_detail_original: false,
        input_modalities: vec![InputModality::Text],
        context_window: Some(context_window),
        max_context_window: Some(context_window),
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        used_fallback_model_metadata: false,
        supports_search_tool: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_both_models_in_priority_order() {
        let catalog = static_model_catalog();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].slug, "sarvam-105b");
        assert_eq!(catalog.models[1].slug, "sarvam-30b");
    }

    #[test]
    fn both_models_advertise_low_medium_high_reasoning() {
        let catalog = static_model_catalog();
        for model in &catalog.models {
            let efforts: Vec<ReasoningEffort> = model
                .supported_reasoning_levels
                .iter()
                .map(|p| p.effort)
                .collect();
            assert_eq!(
                efforts,
                vec![ReasoningEffort::Low, ReasoningEffort::Medium, ReasoningEffort::High],
                "model {} should expose exactly low/medium/high",
                model.slug
            );
            assert_eq!(model.default_reasoning_level, Some(ReasoningEffort::Medium));
        }
    }

    #[test]
    fn models_have_correct_context_windows() {
        let catalog = static_model_catalog();
        let m105b = catalog.models.iter().find(|m| m.slug == "sarvam-105b").unwrap();
        let m30b = catalog.models.iter().find(|m| m.slug == "sarvam-30b").unwrap();
        assert_eq!(m105b.context_window, Some(SARVAM_105B_CONTEXT_WINDOW));
        assert_eq!(m30b.context_window, Some(SARVAM_30B_CONTEXT_WINDOW));
    }

    #[test]
    fn models_are_text_only_no_parallel_tool_calls() {
        let catalog = static_model_catalog();
        for model in &catalog.models {
            assert_eq!(model.input_modalities, vec![InputModality::Text]);
            assert!(!model.supports_parallel_tool_calls);
            assert!(!model.supports_image_detail_original);
        }
    }
}
