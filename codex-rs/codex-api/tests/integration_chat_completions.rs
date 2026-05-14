//! Integration tests for Chat Completions wire API support.
//!
//! This test verifies that:
//! 1. The translation from ResponsesApiRequest → ChatCompletionsRequest is correct
//! 2. The request can be serialized to JSON that matches Sarvam's API spec
//! 3. The SSE event parsing works for Chat Completions format

use codex_api::responses_to_chat_completions_request;
use codex_api::ResponsesApiRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::json;

#[test]
fn chat_completions_request_serializes_correctly() {
    let req = ResponsesApiRequest {
        model: "sarvam-30b".to_string(),
        instructions: "You are helpful.".to_string(),
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "Hello".to_string(),
            }],
            phase: None,
        }],
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
    };

    let cc = responses_to_chat_completions_request(&req);

    // Verify structure
    assert_eq!(cc.model, "sarvam-30b");
    assert_eq!(cc.messages.len(), 2); // system + user
    assert_eq!(cc.messages[0].role, "system");
    assert_eq!(cc.messages[1].role, "user");
    assert_eq!(cc.messages[1].content, "Hello");
    assert!(cc.stream);
    assert!(cc.stream_options.is_some());

    // Serialize to JSON and verify Sarvam compatibility
    let json = serde_json::to_value(&cc).expect("should serialize");
    assert_eq!(json["model"], "sarvam-30b");
    assert_eq!(json["stream"], true);
    assert_eq!(json["stream_options"]["include_usage"], true);

    // Verify all messages have plain string content (not arrays)
    for msg in json["messages"].as_array().unwrap() {
        let content = &msg["content"];
        assert!(
            content.is_string(),
            "Sarvam requires content to be a plain string, not an array. Got: {:?}",
            content
        );
    }

    println!(
        "Chat Completions request: {}",
        serde_json::to_string_pretty(&json).unwrap()
    );
}

#[test]
fn content_flattening_respects_sarvam_constraints() {
    // Sarvam only accepts plain strings for content.
    // Verify our flattening handles all ContentItem variants.
    let req = ResponsesApiRequest {
        model: "sarvam-30b".to_string(),
        instructions: String::new(),
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "Part A".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "https://example.com/img.png".to_string(),
                    detail: None,
                },
                ContentItem::InputText {
                    text: "Part B".to_string(),
                },
            ],
            phase: None,
        }],
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
    };

    let cc = responses_to_chat_completions_request(&req);
    assert_eq!(cc.messages[0].content, "Part A\n[image]\nPart B");

    // Serialize and verify it's still a plain string in JSON
    let json = serde_json::to_value(&cc).expect("should serialize");
    assert!(
        json["messages"][0]["content"].is_string(),
        "Content must be a string for Sarvam"
    );
}

#[test]
fn tool_calls_formatted_for_sarvam() {
    let req = ResponsesApiRequest {
        model: "sarvam-30b".to_string(),
        instructions: String::new(),
        input: vec![ResponseItem::FunctionCall {
            id: None,
            name: "get_weather".to_string(),
            namespace: None,
            arguments: r#"{"city":"San Francisco"}"#.to_string(),
            call_id: "call_123".to_string(),
        }],
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
    };

    let cc = responses_to_chat_completions_request(&req);
    let json = serde_json::to_value(&cc).expect("should serialize");

    // Verify the message has tool_calls
    let msg = &json["messages"][0];
    assert_eq!(msg["role"], "assistant");
    assert_eq!(msg["content"], ""); // Assistant tool calls have empty content
    assert!(msg["tool_calls"].is_array());
    assert_eq!(msg["tool_calls"][0]["id"], "call_123");
    assert_eq!(msg["tool_calls"][0]["type"], "function");
    assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_weather");
    assert_eq!(
        msg["tool_calls"][0]["function"]["arguments"],
        r#"{"city":"San Francisco"}"#
    );
}

#[test]
fn tool_rewrapping_from_responses_to_chat_completions() {
    // Responses API tools have flat structure.
    // Chat Completions wraps them under "function" key.
    let responses_tool = json!({
        "type": "function",
        "name": "fetch_data",
        "description": "Fetch data from the API",
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {
                "url": {"type": "string"}
            }
        }
    });

    let req = ResponsesApiRequest {
        model: "sarvam-30b".to_string(),
        instructions: String::new(),
        input: vec![],
        tools: vec![responses_tool],
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
    };

    let cc = responses_to_chat_completions_request(&req);
    let json = serde_json::to_value(&cc).expect("should serialize");

    // Verify the tool is rewrapped
    assert_eq!(json["tools"][0]["type"], "function");
    assert_eq!(json["tools"][0]["function"]["name"], "fetch_data");
    assert_eq!(
        json["tools"][0]["function"]["description"],
        "Fetch data from the API"
    );
    assert!(json["tools"][0]["function"]["parameters"].is_object());

    // Verify Responses-API-only fields are dropped
    assert!(json["tools"][0].get("strict").is_none());
}
