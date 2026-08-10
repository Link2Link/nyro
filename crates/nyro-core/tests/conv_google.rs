//! Google Gemini conversion-correctness suite.
//!
//! Ported from `llm-bridge/test/google.test.ts` (+ Google sections of
//! `provider-formats.test.ts`), adapted to Nyro's IR + codec architecture:
//!
//! * `toUniversal("google", body)` → `GoogleDecoder::decode_with_model` (the
//!   model rides in the URL path, so tests pass it explicitly)
//! * `fromUniversal("google", universal)` → `GoogleEncoder::encode_request`
//! * `contents` role `"model"` → `Role::Assistant`
//! * `systemInstruction.parts` → leading `Role::System` message
//! * `generationConfig.thinkingConfig` → `reasoning` (Nyro reads it from
//!   `generationConfig`, not from the top level)
//! * `responseMimeType` / `responseSchema` / `toolConfig` → `GoogleExt`
//!   (Nyro keeps them out of the core IR)
//!
//! Known IR deviations from the llm-bridge universal format are annotated
//! inline with `KNOWN GAP`.

mod conv_common;

use conv_common::*;
use nyro_core::protocol::ir::{GoogleExt, ProtocolExt};

/// Decode a Google request body with a fixed path model.
fn google(body: Value) -> AiRequest {
    decode_google_request(body, "gemini-pro")
}

fn google_ext(req: &AiRequest) -> &GoogleExt {
    match &req.ext {
        Some(ProtocolExt::Google(ext)) => ext,
        other => panic!("expected GoogleExt, got {other:?}"),
    }
}

// ── basic text messages ──────────────────────────────────────────────────────

#[test]
fn user_text_message_to_universal() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello, Gemini!"}]}]
    }));

    assert_eq!(req.model, "gemini-pro");
    assert_roles(&req, &[Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "Hello, Gemini!");
}

#[test]
fn model_role_maps_to_assistant() {
    let req = google(json!({
        "contents": [
            {"role": "user", "parts": [{"text": "What is 2+2?"}]},
            {"role": "model", "parts": [{"text": "2+2 equals 4."}]}
        ]
    }));

    assert_roles(&req, &[Role::User, Role::Assistant]);
    assert_eq!(req.messages[0].content.to_text(), "What is 2+2?");
    assert_eq!(req.messages[1].content.to_text(), "2+2 equals 4.");
}

#[test]
fn multi_turn_conversation() {
    let req = google(json!({
        "contents": [
            {"role": "user", "parts": [{"text": "Hi"}]},
            {"role": "model", "parts": [{"text": "Hello!"}]},
            {"role": "user", "parts": [{"text": "How are you?"}]},
            {"role": "model", "parts": [{"text": "I'm doing well!"}]}
        ]
    }));

    assert_roles(&req, &[Role::User, Role::Assistant, Role::User, Role::Assistant]);
}

// ── systemInstruction ────────────────────────────────────────────────────────

#[test]
fn system_instruction_becomes_system_message() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "systemInstruction": {"parts": [{"text": "You are a helpful assistant."}]}
    }));

    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "You are a helpful assistant.");
}

#[test]
fn multiple_system_instruction_parts_are_joined() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "systemInstruction": {
            "parts": [
                {"text": "You are a pirate."},
                {"text": "Always speak in pirate talk."}
            ]
        }
    }));

    // Nyro joins with "\n" (llm-bridge joins with "").
    assert_eq!(
        req.messages[0].content.to_text(),
        "You are a pirate.\nAlways speak in pirate talk."
    );
}

// ── inlineData (base64 images) ───────────────────────────────────────────────

#[test]
fn inline_data_image_to_universal() {
    let req = google(json!({
        "contents": [{
            "role": "user",
            "parts": [
                {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgoAAAANS..."}},
                {"text": "What is in this image?"}
            ]
        }]
    }));

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    assert_eq!(blocks.len(), 2);
    match &blocks[0] {
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "iVBORw0KGgoAAAANS...");
            }
            other => panic!("expected base64 image source, got {other:?}"),
        },
        other => panic!("expected image block, got {other:?}"),
    }
    assert_eq!(blocks[1].as_text(), Some("What is in this image?"));
}

// ── functionCall parts (tool calls) ──────────────────────────────────────────

#[test]
fn function_call_to_universal_tool_call() {
    let req = google(json!({
        "contents": [
            {"role": "user", "parts": [{"text": "What's the weather?"}]},
            {
                "role": "model",
                "parts": [{
                    "functionCall": {
                        "name": "get_weather",
                        "args": {"location": "Tokyo", "unit": "celsius"}
                    }
                }]
            }
        ]
    }));

    assert_eq!(req.messages[1].role, Role::Assistant);
    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "get_weather");
    assert!(!calls[0].id.is_empty(), "id is auto-generated");
    assert_eq!(
        calls[0].arguments,
        "{\"location\":\"Tokyo\",\"unit\":\"celsius\"}"
    );
}

#[test]
fn multiple_function_calls_in_one_message() {
    let req = google(json!({
        "contents": [{
            "role": "model",
            "parts": [
                {"functionCall": {"name": "get_weather", "args": {"location": "Tokyo"}}},
                {"functionCall": {"name": "get_time", "args": {"timezone": "Asia/Tokyo"}}}
            ]
        }]
    }));

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[1].name, "get_time");
}

// ── functionResponse parts (tool results) ────────────────────────────────────

#[test]
fn function_response_to_tool_result() {
    let req = google(json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "functionResponse": {
                    "name": "get_weather",
                    "response": {"temperature": 22, "condition": "sunny"}
                }
            }]
        }]
    }));

    assert_eq!(req.messages[0].role, Role::Tool);
    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("tool result must decode to blocks");
    };
    match &blocks[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            assert_eq!(tool_use_id, "get_weather");
            assert_eq!(
                *content,
                json!({"temperature": 22, "condition": "sunny"})
            );
        }
        other => panic!("expected tool_result block, got {other:?}"),
    }
}

// ── functionDeclarations (tool definitions) ──────────────────────────────────

#[test]
fn function_declarations_to_universal_tools() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Get weather"}]}],
        "tools": [{
            "functionDeclarations": [
                {
                    "name": "get_weather",
                    "description": "Get current weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {"type": "string", "description": "City name"}
                        },
                        "required": ["location"]
                    }
                },
                {
                    "name": "get_time",
                    "description": "Get current time for a timezone",
                    "parameters": {
                        "type": "object",
                        "properties": {"timezone": {"type": "string"}}
                    }
                }
            ]
        }]
    }));

    let tools = req.tools.expect("tools preserved");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "get_weather");
    assert_eq!(
        tools[0].description.as_deref(),
        Some("Get current weather for a location")
    );
    assert_eq!(
        tools[0].parameters,
        json!({
            "type": "object",
            "properties": {"location": {"type": "string", "description": "City name"}},
            "required": ["location"]
        })
    );
    assert_eq!(tools[1].name, "get_time");
}

#[test]
fn tool_config_is_preserved_in_google_ext() {
    // KNOWN GAP: `toolConfig.functionCallingConfig.mode` stays in `GoogleExt`
    // and is not normalised into `tool_choice` (llm-bridge maps AUTO→"auto").
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "tools": [{
            "functionDeclarations": [{
                "name": "greet",
                "description": "Greet the user",
                "parameters": {"type": "object", "properties": {}}
            }]
        }],
        "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}
    }));

    let ext = google_ext(&req);
    let tool_config = ext
        .tool_config
        .as_ref()
        .expect("toolConfig preserved in GoogleExt");
    assert_eq!(tool_config["functionCallingConfig"]["mode"], "AUTO");
}

// ── thinkingConfig ───────────────────────────────────────────────────────────

#[test]
fn thinking_budget_maps_to_reasoning() {
    // Nyro reads thinkingConfig from generationConfig (llm-bridge: top level).
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Think about this."}]}],
        "generationConfig": {"thinkingConfig": {"thinkingBudget": 8192}}
    }));

    assert!(req.reasoning.enabled, "reasoning must be enabled");
    assert_eq!(req.reasoning.budget_tokens, Some(8192));
}

#[test]
fn thinking_level_maps_to_reasoning_effort() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Think about this."}]}],
        "generationConfig": {"thinkingConfig": {"thinkingLevel": "medium"}}
    }));

    assert!(req.reasoning.enabled, "reasoning must be enabled");
    assert_eq!(req.reasoning.effort, Some(ReasoningEffort::Medium));
}

#[test]
fn thinking_budget_and_level_together() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Complex task."}]}],
        "generationConfig": {
            "thinkingConfig": {"thinkingBudget": 16384, "thinkingLevel": "high"}
        }
    }));

    assert!(req.reasoning.enabled, "reasoning must be enabled");
    assert_eq!(req.reasoning.budget_tokens, Some(16384));
    assert_eq!(req.reasoning.effort, Some(ReasoningEffort::High));
}

// ── thought parts ────────────────────────────────────────────────────────────

#[test]
fn thought_parts_become_thinking_blocks() {
    // KNOWN GAP: `GooglePart` is `#[serde(untagged)]` and declares `Text`
    // before the `Other` catch-all, so a `{"thought": true, "text": ...}`
    // part deserialises as plain `Text` and the thinking marker is lost.
    let req = google(json!({
        "contents": [
            {"role": "user", "parts": [{"text": "Think step by step."}]},
            {
                "role": "model",
                "parts": [
                    {"thought": true, "text": "Let me reason through this..."},
                    {"text": "Here is my answer."}
                ]
            }
        ]
    }));

    let MessageContent::Blocks(blocks) = &req.messages[1].content else {
        panic!("KNOWN GAP: thought part should not collapse to text: {:?}", req.messages[1].content);
    };
    assert_eq!(blocks.len(), 2);
    match &blocks[0] {
        ContentBlock::Thinking { thinking, .. } => {
            assert_eq!(thinking, "Let me reason through this...");
        }
        other => panic!("KNOWN GAP: expected thinking block, got {other:?}"),
    }
    assert_eq!(blocks[1].as_text(), Some("Here is my answer."));
}

// ── generationConfig with structured output ──────────────────────────────────

#[test]
fn response_mime_type_and_schema_go_to_google_ext() {
    // KNOWN GAP: structured output stays in `GoogleExt` (llm-bridge maps it to
    // a universal `structured_output`).
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "List 3 colors"}]}],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": {
                "type": "object",
                "properties": {
                    "colors": {"type": "array", "items": {"type": "string"}}
                }
            }
        }
    }));

    let ext = google_ext(&req);
    assert_eq!(ext.response_mime_type.as_deref(), Some("application/json"));
    assert_eq!(
        ext.response_json_schema.as_ref(),
        Some(&json!({
            "type": "object",
            "properties": {
                "colors": {"type": "array", "items": {"type": "string"}}
            }
        }))
    );
}

#[test]
fn response_mime_type_without_schema_goes_to_google_ext() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Return JSON"}]}],
        "generationConfig": {"responseMimeType": "application/json"}
    }));

    let ext = google_ext(&req);
    assert_eq!(ext.response_mime_type.as_deref(), Some("application/json"));
}

#[test]
fn generation_config_params_are_extracted() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "generationConfig": {
            "temperature": 0.8,
            "maxOutputTokens": 2048,
            "topP": 0.95
        }
    }));

    assert_eq!(req.generation.temperature, Some(0.8));
    assert_eq!(req.generation.max_tokens, Some(2048));
    assert_eq!(req.generation.top_p, Some(0.95));
}

// ── round-trip: google → universal → google ──────────────────────────────────

#[test]
fn round_trip_basic_text_messages() {
    let out = round_trip_request(
        P::GoogleGemini,
        json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Hello!"}]},
                {"role": "model", "parts": [{"text": "Hi there!"}]}
            ]
        }),
    );

    let contents = field(&out, "/contents").as_array().expect("contents array");
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Hello!");
    assert_eq!(contents[1]["role"], "model");
}

#[test]
fn round_trip_system_instruction() {
    let out = round_trip_request(
        P::GoogleGemini,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
            "systemInstruction": {"parts": [{"text": "You are a pirate."}]}
        }),
    );

    assert_eq!(field(&out, "/systemInstruction/parts/0/text"), "You are a pirate.");
}

#[test]
fn round_trip_tools() {
    let out = round_trip_request(
        P::GoogleGemini,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Get weather"}]}],
            "tools": [{
                "functionDeclarations": [{
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}}
                    }
                }]
            }]
        }),
    );

    let tools = field(&out, "/tools").as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["functionDeclarations"].as_array().map(Vec::len), Some(1));
    assert_eq!(tools[0]["functionDeclarations"][0]["name"], "get_weather");
}

#[test]
fn round_trip_thinking_config() {
    let out = round_trip_request(
        P::GoogleGemini,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Think"}]}],
            "generationConfig": {
                "thinkingConfig": {"thinkingBudget": 4096, "thinkingLevel": "high"}
            }
        }),
    );

    assert_eq!(field(&out, "/generationConfig/thinkingConfig/thinkingBudget"), &json!(4096));
    assert_eq!(field(&out, "/generationConfig/thinkingConfig/thinkingLevel"), "high");
}

#[test]
fn round_trip_generation_config_params() {
    let out = round_trip_request(
        P::GoogleGemini,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
            "generationConfig": {
                "temperature": 0.7,
                "maxOutputTokens": 4096,
                "topP": 0.9
            }
        }),
    );

    assert_eq!(field(&out, "/generationConfig/temperature"), &json!(0.7));
    assert_eq!(field(&out, "/generationConfig/maxOutputTokens"), &json!(4096));
    assert_eq!(field(&out, "/generationConfig/topP"), &json!(0.9));
}

// ── fromUniversal: google output ─────────────────────────────────────────────

#[test]
fn universal_text_messages_to_google_format() {
    let req = request(
        "gemini-pro",
        vec![user_msg("Hello Gemini!"), assistant_msg("Hello!")],
    );

    let out = encode_request(P::GoogleGemini, &req);

    let contents = field(&out, "/contents").as_array().expect("contents array");
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Hello Gemini!");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(contents[1]["parts"][0]["text"], "Hello!");
}

#[test]
fn universal_image_to_google_inline_data() {
    let req = request(
        "gemini-pro",
        vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::Base64 {
                    media_type: "image/jpeg".to_string(),
                    data: "base64imagedata".to_string(),
                },
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let out = encode_request(P::GoogleGemini, &req);
    let part = field(&out, "/contents/0/parts/0");
    assert_eq!(part["inlineData"]["data"], "base64imagedata");
    assert_eq!(part["inlineData"]["mimeType"], "image/jpeg");
}

#[test]
fn universal_tool_call_to_google_function_call() {
    let req = request(
        "gemini-pro",
        vec![assistant_tool_call_msg("call_123", "get_weather", "{\"location\":\"Paris\"}")],
    );

    let out = encode_request(P::GoogleGemini, &req);
    let part = field(&out, "/contents/0/parts/0");
    assert_eq!(part["functionCall"]["name"], "get_weather");
    assert_eq!(part["functionCall"]["args"], json!({"location": "Paris"}));
}

#[test]
fn universal_tool_result_to_google_function_response() {
    let req = request(
        "gemini-pro",
        vec![Message {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_123".to_string(),
                content: json!({"temperature": 25}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let out = encode_request(P::GoogleGemini, &req);
    let part = field(&out, "/contents/0/parts/0");
    assert_eq!(part["functionResponse"]["name"], "call_123");
    assert_eq!(part["functionResponse"]["response"], json!({"temperature": 25}));
}

#[test]
fn universal_thinking_content_to_google_thought_part() {
    // KNOWN GAP: the Gemini encoder drops the `thought: true` marker and emits
    // a plain text part (llm-bridge emits `{"thought": true, "text": ...}`).
    let req = request(
        "gemini-pro",
        vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "Let me think...".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "The answer is 42.".to_string(),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let out = encode_request(P::GoogleGemini, &req);
    let parts = field(&out, "/contents/0/parts").as_array().expect("parts array");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "Let me think...");
    assert_eq!(parts[1]["text"], "The answer is 42.");
}

#[test]
fn universal_tools_to_google_function_declarations() {
    let req = request("gemini-pro", vec![user_msg("Hello")]);
    let mut req = req;
    req.tools = Some(vec![ToolSpec {
        name: "search".to_string(),
        description: Some("Search the web".to_string()),
        kind: Default::default(),
        namespace: None,
        parameters: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}}
        }),
        strict: None,
        cache_control: None,
        meta: None,
    }]);

    let out = encode_request(P::GoogleGemini, &req);
    let tools = field(&out, "/tools").as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["functionDeclarations"].as_array().map(Vec::len), Some(1));
    assert_eq!(tools[0]["functionDeclarations"][0]["name"], "search");
    assert_eq!(tools[0]["functionDeclarations"][0]["description"], "Search the web");
}

#[test]
fn universal_thinking_config_written_back() {
    // KNOWN GAP: `google_reasoning_config` returns only `thinkingBudget` when
    // a budget is present, silently dropping the effort level even though the
    // IR carries both.
    let req = request("gemini-pro", vec![user_msg("Think")]);
    let mut req = req;
    req.reasoning = ReasoningConfig {
        enabled: true,
        budget_tokens: Some(2048),
        effort: Some(ReasoningEffort::Medium),
        display: None,
    };

    let out = encode_request(P::GoogleGemini, &req);
    assert_eq!(
        field(&out, "/generationConfig/thinkingConfig/thinkingBudget"),
        &json!(2048)
    );
    match field(&out, "/generationConfig/thinkingConfig/thinkingLevel").as_str() {
        Some("medium") => {}
        other => panic!(
            "KNOWN GAP: effort level should survive alongside the budget, got {other:?}"
        ),
    }
}

#[test]
fn structured_output_round_trips_via_generation_config() {
    // The wire `generationConfig` (responseMimeType + responseSchema) survives
    // the decode→encode round trip through the raw generation config bag.
    let out = round_trip_request(
        P::GoogleGemini,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "JSON"}]}],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}}
                }
            }
        }),
    );

    assert_eq!(
        field(&out, "/generationConfig/responseMimeType"),
        "application/json"
    );
    assert_eq!(
        field(&out, "/generationConfig/responseSchema"),
        &json!({
            "type": "object",
            "properties": {"name": {"type": "string"}}
        })
    );
}

// KNOWN GAP (documented, not asserted): writing structured output back from a
// hand-built IR is not possible yet — the Google encoder consumes the raw
// ingress bag, not `GoogleExt`, so a `GoogleExt`-only request drops
// `responseMimeType`/`responseSchema`.

// ── edge cases ───────────────────────────────────────────────────────────────

#[test]
fn empty_contents_array() {
    let req = google(json!({"contents": []}));
    assert_eq!(req.messages.len(), 0);
}

#[test]
fn missing_tools_field() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}]
    }));
    assert!(req.tools.is_none());
}

#[test]
fn safety_settings_are_preserved() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "safetySettings": [
            {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}
        ]
    }));

    let settings = req.safety_settings.expect("safety settings preserved");
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0].category, "HARM_CATEGORY_HARASSMENT");
    assert_eq!(settings[0].threshold, "BLOCK_NONE");
}

// ── provider-formats.test.ts: Google section ─────────────────────────────────

#[test]
fn google_request_to_universal_basic() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello Gemini"}]}],
        "generationConfig": {"temperature": 0.8, "maxOutputTokens": 150}
    }));

    assert_eq!(req.model, "gemini-pro");
    assert_roles(&req, &[Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "Hello Gemini");
    assert_eq!(req.generation.temperature, Some(0.8));
    assert_eq!(req.generation.max_tokens, Some(150));
}

#[test]
fn google_multimodal_content() {
    let req = google(json!({
        "contents": [{
            "role": "user",
            "parts": [
                {"text": "What's in this image?"},
                {"inlineData": {"mimeType": "image/jpeg", "data": "xyz"}}
            ]
        }]
    }));

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    assert_eq!(blocks.len(), 2);
    assert!(matches!(blocks[0], ContentBlock::Text { .. }));
    assert!(matches!(blocks[1], ContentBlock::Image { .. }));
}

#[test]
fn google_system_instruction() {
    let req = google(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "systemInstruction": {"parts": [{"text": "You are a helpful assistant"}]}
    }));

    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "You are a helpful assistant");
}

#[test]
fn universal_to_google_format() {
    // fromUniversal("google", universal) — built straight from the IR.
    let mut req = request("gemini-pro", vec![user_msg("Hello Gemini")]);
    req.generation.temperature = Some(0.8);
    req.generation.max_tokens = Some(150);

    let out = encode_request(P::GoogleGemini, &req);

    let contents = field(&out, "/contents").as_array().expect("contents array");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Hello Gemini");
    assert_eq!(field(&out, "/generationConfig/temperature"), &json!(0.8));
    assert_eq!(field(&out, "/generationConfig/maxOutputTokens"), &json!(150));
}

// ── fix-verification / universal-conversion ported cases ─────────────────────

#[test]
fn file_data_non_image_becomes_file_block() {
    // `fileData` (universal-conversion: "should handle Google multimodal
    // content") → Nyro's `ContentBlock::File` with a URL source.
    let req = google(json!({
        "contents": [{
            "role": "user",
            "parts": [
                {"text": "What do you see in this image?"},
                {"inlineData": {"mimeType": "image/jpeg", "data": "iVBORw0KGgoAAAANSUhEUgAA..."}},
                {"fileData": {"mimeType": "application/pdf", "fileUri": "gs://bucket/document.pdf"}}
            ]
        }]
    }));

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("expected blocks: {:?}", req.messages[0].content);
    };
    assert_eq!(blocks.len(), 3);
    assert!(matches!(blocks[0], ContentBlock::Text { .. }));
    match &blocks[1] {
        ContentBlock::Image { source, .. } => {
            assert!(matches!(source, MediaSource::Base64 { .. }));
        }
        other => panic!("expected image block, got {other:?}"),
    }
    match &blocks[2] {
        ContentBlock::File { source, media_type } => {
            assert!(matches!(source, MediaSource::Url(u) if u == "gs://bucket/document.pdf"));
            assert_eq!(media_type.as_deref(), Some("application/pdf"));
        }
        other => panic!("expected file block, got {other:?}"),
    }
}

#[test]
fn file_data_image_uses_url_source() {
    let req = google(json!({
        "contents": [{
            "role": "user",
            "parts": [{"fileData": {"mimeType": "image/webp", "fileUri": "gs://bucket/photo.webp"}}]
        }]
    }));

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("expected blocks: {:?}", req.messages[0].content);
    };
    match &blocks[0] {
        ContentBlock::Image { source, .. } => {
            assert!(matches!(source, MediaSource::Url(u) if u == "gs://bucket/photo.webp"));
        }
        other => panic!("expected image block, got {other:?}"),
    }
}

#[test]
fn file_block_round_trips_as_file_data() {
    // `document` → `fileData` on the wire (fix-verification "should use
    // fileData when media has fileUri").
    let req = request("gemini-pro", vec![Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::File {
            source: MediaSource::Url("gs://bucket/document.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }]);

    let out = encode_request(P::GoogleGemini, &req);
    let part = field(&out, "/contents/0/parts/0");
    assert_eq!(part["fileData"]["fileUri"], "gs://bucket/document.pdf");
    assert_eq!(part["fileData"]["mimeType"], "application/pdf");
    assert!(part.get("inlineData").is_none());
}

#[test]
fn tool_result_string_wraps_in_result_object() {
    // fix-verification "should wrap string result in { output: ... } for
    // Gemini": for text-form tool messages (as produced by the Anthropic
    // decoder) Nyro wraps the string result in `{"result": ...}` instead of
    // llm-bridge's `{"output": ...}`. Block-form results pass through raw
    // (see `tool_result_object_passes_through`).
    let req = request("gemini-pro", vec![
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "search".to_string(),
                input: json!({"q": "test"}),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: Role::Tool,
            content: MessageContent::Text("Found 5 results".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            meta: None,
        },
    ]);

    let out = encode_request(P::GoogleGemini, &req);
    let part = field(&out, "/contents/1/parts/0");
    assert_eq!(part["functionResponse"]["name"], "call_1");
    assert_eq!(
        part["functionResponse"]["response"],
        json!({"result": "Found 5 results"})
    );
}

#[test]
fn tool_result_object_passes_through_for_gemini() {
    let req = request("gemini-pro", vec![
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "search".to_string(),
                input: json!({"q": "test"}),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: json!({"count": 5, "items": ["a", "b"]}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            meta: None,
        },
    ]);

    let out = encode_request(P::GoogleGemini, &req);
    let part = field(&out, "/contents/1/parts/0");
    assert_eq!(
        part["functionResponse"]["response"],
        json!({"count": 5, "items": ["a", "b"]})
    );
}

#[test]
fn synthetic_call_id_generated_when_function_call_id_missing() {
    // fix-verification "should generate synthetic ID when functionCall.id is
    // missing".
    let req = google(json!({
        "contents": [{
            "role": "model",
            "parts": [{"functionCall": {"name": "get_weather", "args": {"location": "London"}}}]
        }]
    }));

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "get_weather");
    assert!(!calls[0].id.is_empty(), "synthetic call id generated");
    assert!(calls[0].id.starts_with("call_"));
    assert_eq!(calls[0].arguments, "{\"location\":\"London\"}");
}

#[test]
fn default_model_is_gemini_2_flash() {
    // fix-verification "should fallback to gemini-pro when model not in body":
    // Nyro's Google decoder defaults to `gemini-2.0-flash` when the model is
    // not supplied.
    use nyro_core::protocol::codec::google::gemini::decoder::GoogleDecoder;
    use nyro_core::protocol::RequestDecoder;

    let req = GoogleDecoder
        .decode_request(json!({"contents": [{"role": "user", "parts": [{"text": "Hello"}]}]}))
        .expect("decode without model");
    assert_eq!(req.model, "gemini-2.0-flash");
}

#[test]
fn schema_unsupported_fields_stripped_for_gemini() {
    // fix-verification "recursive schema stripping": Nyro strips
    // `$schema`/`additionalProperties`/`$ref`/`definitions`/`$defs`
    // recursively. llm-bridge additionally strips `default`/`examples`/
    // `deprecated`/`readOnly`/`$comment`; Nyro passes those through.
    let mut req = request("gemini-pro", vec![user_msg("Hello")]);
    req.tools = Some(vec![ToolSpec {
        name: "search".to_string(),
        description: Some("Search the web".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "additionalProperties": false,
                    "default": "test",
                    "examples": ["foo"],
                    "deprecated": true,
                    "readOnly": true,
                    "$comment": "test",
                    "$schema": "http://json-schema.org/draft-07/schema#"
                },
                "nested": {
                    "type": "object",
                    "properties": {"inner": {"type": "string", "additionalProperties": true}},
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        }),
        kind: Default::default(),
        namespace: None,
        strict: None,
        cache_control: None,
        meta: None,
    }]);

    let out = encode_request(P::GoogleGemini, &req);
    let params = field(&out, "/tools/0/functionDeclarations/0/parameters");
    assert!(params.get("additionalProperties").is_none(), "{params}");
    let query = &params["properties"]["query"];
    assert!(query.get("additionalProperties").is_none());
    assert!(query.get("$schema").is_none());
    assert!(query.get("$comment").is_some(), "passes through: {query}");
    assert!(query.get("default").is_some(), "passes through: {query}");
    assert!(params["properties"]["nested"].get("additionalProperties").is_none());
    assert!(
        params["properties"]["nested"]["properties"]["inner"]
            .get("additionalProperties")
            .is_none()
    );
}

#[test]
fn thinking_budget_written_verbatim_without_clamp() {
    // fix-verification "should clamp thinkingBudget to Gemini max of 24576":
    // Nyro writes the budget verbatim; there is no clamp.
    let mut req = request("gemini-pro", vec![user_msg("Hi")]);
    req.reasoning = ReasoningConfig {
        enabled: true,
        budget_tokens: Some(100_000),
        effort: None,
        display: None,
    };

    let out = encode_request(P::GoogleGemini, &req);
    assert_eq!(
        field(&out, "/generationConfig/thinkingConfig/thinkingBudget"),
        &json!(100_000)
    );
}

#[test]
fn redacted_thinking_serializes_verbatim_for_gemini() {
    // fix-verification "should convert redacted_thinking to empty thought part
    // for Google": Nyro's encoder has no dedicated `redacted_thinking` arm, so
    // the block falls through to the serde fallback and round-trips as an
    // opaque `{"type":"redacted_thinking",...}` part rather than llm-bridge's
    // `{thought: true, text: ""}`.
    let req = request("gemini-pro", vec![Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::RedactedThinking {
                data: "base64data".to_string(),
            },
            ContentBlock::Text {
                text: "Hello".to_string(),
                cache_control: None,
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }]);

    let out = encode_request(P::GoogleGemini, &req);
    let parts = field(&out, "/contents/0/parts").as_array().expect("parts");
    assert_eq!(parts[0]["type"], "redacted_thinking");
    assert_eq!(parts[0]["data"], "base64data");
    assert_eq!(parts[1], json!({"text": "Hello"}));
}

#[test]
fn multiple_text_blocks_in_system_message_join_into_single_part() {
    // universal-conversion "should handle multimodal system messages in Google
    // format": llm-bridge emits one systemInstruction part per text block;
    // Nyro emits one part per system *message*, with the blocks concatenated.
    let req = request("gemini-pro", vec![
        Message {
            role: Role::System,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "You are a helpful assistant.".to_string(),
                    cache_control: None,
                },
                ContentBlock::Text {
                    text: "Additional context.".to_string(),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        user_msg("Hello!"),
    ]);

    let out = encode_request(P::GoogleGemini, &req);
    let parts = field(&out, "/systemInstruction/parts").as_array().expect("parts");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["text"], "You are a helpful assistant.Additional context.");
    let contents = field(&out, "/contents").as_array().expect("contents");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
}

#[test]
fn non_array_system_parts_is_a_decode_error() {
    // fix-verification "should handle non-array parts gracefully": llm-bridge
    // tolerates `systemInstruction.parts: "not an array"`; Nyro's typed
    // `GoogleRequest` rejects it with a decode error.
    use nyro_core::protocol::codec::google::gemini::decoder::GoogleDecoder;
    use nyro_core::protocol::RequestDecoder;

    let result = GoogleDecoder.decode_request(json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
        "systemInstruction": {"parts": "not an array"}
    }));
    assert!(result.is_err(), "non-array parts must fail deserialisation");
}
