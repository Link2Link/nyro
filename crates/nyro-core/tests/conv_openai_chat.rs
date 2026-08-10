//! OpenAI Chat Completions conversion-correctness suite.
//!
//! Ported from `llm-bridge/test/openai-chat.test.ts` (+ OpenAI sections of
//! `provider-formats.test.ts` and the OpenAI→Anthropic cross-provider case),
//! adapted to Nyro's IR + codec architecture:
//!
//! * `toUniversal("openai", body)`      → `OpenAIDecoder::decode_request`
//! * `fromUniversal("openai", universal)` → `OpenAIEncoder::encode_request`
//! * system prompt                       → leading `Role::System` message (Nyro IR)
//! * `developer` role                    → `Role::System` (Nyro IR mapping)

mod conv_common;

use conv_common::*;

// ── basic text messages ──────────────────────────────────────────────────────

#[test]
fn basic_text_messages_to_universal() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "What is the capital of France?"},
                {"role": "assistant", "content": "The capital of France is Paris."}
            ]
        }),
    );

    assert_eq!(req.model, "gpt-4o");
    assert_roles(&req, &[Role::User, Role::Assistant]);
    assert_eq!(req.messages[0].content.to_text(), "What is the capital of France?");
    assert_eq!(req.messages[1].content.to_text(), "The capital of France is Paris.");
}

#[test]
fn system_message_becomes_leading_system_role_message() {
    // llm-bridge extracts `system` into a dedicated field; Nyro keeps it as a
    // leading `Role::System` message — same information, different home.
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello"}
            ]
        }),
    );

    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "You are a helpful assistant.");
    assert_eq!(req.messages[1].content.to_text(), "Hello");
}

#[test]
fn developer_role_is_mapped_to_system_role_message() {
    // llm-bridge preserves `developer` as a distinct message; Nyro maps it to
    // `Role::System` and keeps it as a regular (non-merged) message.
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "developer", "content": "Follow these guidelines strictly."},
                {"role": "user", "content": "Hello"}
            ]
        }),
    );

    assert_roles(&req, &[Role::System, Role::System, Role::User]);
    assert_eq!(req.messages[1].content.to_text(), "Follow these guidelines strictly.");
    assert_eq!(req.messages[2].content.to_text(), "Hello");
}

#[test]
fn multi_turn_conversation_preserves_order() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o-mini",
            "messages": [
                {"role": "system", "content": "You are a math tutor."},
                {"role": "user", "content": "What is 2+2?"},
                {"role": "assistant", "content": "2+2 equals 4."},
                {"role": "user", "content": "And 3+3?"},
                {"role": "assistant", "content": "3+3 equals 6."},
                {"role": "user", "content": "Thanks!"}
            ]
        }),
    );

    assert_roles(
        &req,
        &[
            Role::System,
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User,
        ],
    );
    assert_eq!(req.messages[5].content.to_text(), "Thanks!");
}

// ── tool definitions ─────────────────────────────────────────────────────────

#[test]
fn tool_definitions_preserve_strict_metadata() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "What's the weather in London?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather for a location",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {"type": "string", "description": "City name"}
                        },
                        "required": ["location"],
                        "additionalProperties": false
                    }
                }
            }]
        }),
    );

    let tools = req.tools.expect("tools preserved");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "get_weather");
    assert_eq!(tools[0].description.as_deref(), Some("Get current weather for a location"));
    assert_eq!(tools[0].strict, Some(true));
    assert_eq!(
        tools[0].parameters,
        json!({
            "type": "object",
            "properties": {"location": {"type": "string", "description": "City name"}},
            "required": ["location"],
            "additionalProperties": false
        })
    );
}

#[test]
fn assistant_tool_calls_and_tool_results_are_decoded() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "What's the weather?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"London\"}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc123",
                    "content": "{\"temp\": 15, \"condition\": \"cloudy\"}"
                }
            ]
        }),
    );

    assert_roles(&req, &[Role::User, Role::Assistant, Role::Tool]);
    assert_eq!(req.messages.len(), 3);

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_abc123");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, "{\"location\":\"London\"}");

    assert_eq!(req.messages[2].role, Role::Tool);
    assert_eq!(req.messages[2].tool_call_id.as_deref(), Some("call_abc123"));
    assert_eq!(
        req.messages[2].content.to_text(),
        "{\"temp\": 15, \"condition\": \"cloudy\"}"
    );
}

// ── multimodal ───────────────────────────────────────────────────────────────

#[test]
fn image_url_content_becomes_image_block() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg", "detail": "high"}}
                ]
            }]
        }),
    );

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].as_text(), Some("What's in this image?"));
    match &blocks[1] {
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Url(url) => assert_eq!(url, "https://example.com/cat.jpg"),
            other => panic!("expected URL image source, got {other:?}"),
        },
        other => panic!("expected image block, got {other:?}"),
    }
}

#[test]
fn data_url_image_parses_mime_type_and_base64_data() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
                }]
            }]
        }),
    );

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    match &blocks[0] {
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "iVBORw0KGgo=");
            }
            other => panic!("expected base64 image source, got {other:?}"),
        },
        other => panic!("expected image block, got {other:?}"),
    }
}

// ── structured output ────────────────────────────────────────────────────────

#[test]
fn json_schema_response_format_maps_to_ir() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Extract the name and age."}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "person",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "age": {"type": "number"}
                        },
                        "required": ["name", "age"],
                        "additionalProperties": false
                    }
                }
            }
        }),
    );

    match req.response_format.expect("response_format preserved") {
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => {
            assert_eq!(name, "person");
            assert_eq!(strict, Some(true));
            assert_eq!(
                schema,
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "age": {"type": "number"}
                    },
                    "required": ["name", "age"],
                    "additionalProperties": false
                })
            );
        }
        other => panic!("expected JsonSchema, got {other:?}"),
    }
}

#[test]
fn json_object_response_format_maps_to_ir() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Return JSON."}],
            "response_format": {"type": "json_object"}
        }),
    );

    assert!(
        matches!(req.response_format, Some(ResponseFormat::JsonObject)),
        "expected JsonObject, got {:?}",
        req.response_format
    );
}

// ── reasoning ────────────────────────────────────────────────────────────────

#[test]
fn reasoning_effort_is_preserved_in_reasoning_config() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "o1",
            "messages": [{"role": "user", "content": "Solve this complex problem."}],
            "reasoning_effort": "high"
        }),
    );

    assert!(req.reasoning.enabled, "reasoning must be enabled");
    assert_eq!(req.reasoning.effort, Some(ReasoningEffort::High));
}

// ── round-trip: openai → universal → openai ─────────────────────────────────

#[test]
fn round_trip_preserves_generation_params() {
    let out = round_trip_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello!"},
                {"role": "assistant", "content": "Hi there! How can I help?"},
                {"role": "user", "content": "Tell me a joke."}
            ],
            "temperature": 0.7,
            "max_tokens": 1000,
            "top_p": 0.95,
            "frequency_penalty": 0.5,
            "presence_penalty": 0.2,
            "seed": 42
        }),
    );

    field_str_eq(&out, "/model", "gpt-4o");
    assert_eq!(field(&out, "/temperature"), &json!(0.7));
    assert_eq!(field(&out, "/max_tokens"), &json!(1000));
    assert_eq!(field(&out, "/top_p"), &json!(0.95));
    assert_eq!(field(&out, "/frequency_penalty"), &json!(0.5));
    assert_eq!(field(&out, "/presence_penalty"), &json!(0.2));
    assert_eq!(field(&out, "/seed"), &json!(42));

    let messages = field(&out, "/messages").as_array().expect("messages array");
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are a helpful assistant.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Hello!");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[3]["role"], "user");
}

#[test]
fn round_trip_tool_definitions() {
    // KNOWN GAP: the OpenAI Chat encoder drops `strict` from function tools,
    // so the flag does not survive the round trip.
    let out = round_trip_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Get the weather"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the weather",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"],
                        "additionalProperties": false
                    }
                }
            }]
        }),
    );

    let tools = field(&out, "/tools").as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_weather");
    assert_eq!(
        tools[0]["function"]["strict"],
        true,
        "KNOWN GAP: `strict` should survive the round trip, got {:?}",
        tools[0]["function"].get("strict")
    );
    assert_eq!(
        tools[0]["function"]["parameters"],
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false
        })
    );
}

#[test]
fn round_trip_json_schema_response_format() {
    let out = round_trip_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Extract info."}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "extraction",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"],
                        "additionalProperties": false
                    }
                }
            }
        }),
    );

    let rf = field(&out, "/response_format");
    assert_eq!(rf["type"], "json_schema");
    assert_eq!(rf["json_schema"]["name"], "extraction");
    assert_eq!(rf["json_schema"]["strict"], true);
    assert_eq!(
        rf["json_schema"]["schema"],
        json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        })
    );
}

#[test]
fn round_trip_reasoning_effort() {
    let out = round_trip_request(
        P::OpenAiChat,
        json!({
            "model": "o1",
            "messages": [{"role": "user", "content": "Think hard."}],
            "reasoning_effort": "medium"
        }),
    );

    assert_eq!(field(&out, "/reasoning_effort"), "medium");
}

// ── provider-formats.test.ts: OpenAI section ─────────────────────────────────

#[test]
fn openai_request_to_universal_basic() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "You are helpful"},
                {"role": "user", "content": "Hello"}
            ],
            "temperature": 0.7,
            "max_tokens": 100
        }),
    );

    assert_eq!(req.model, "gpt-4");
    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "You are helpful");
    assert_eq!(req.messages[1].content.to_text(), "Hello");
    assert_eq!(req.generation.temperature, Some(0.7));
    assert_eq!(req.generation.max_tokens, Some(100));
}

#[test]
fn openai_multimodal_content() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4-vision",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,xyz"}}
                ]
            }]
        }),
    );

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    assert_eq!(blocks.len(), 2);
    assert!(matches!(blocks[0], ContentBlock::Text { .. }));
    assert!(matches!(blocks[1], ContentBlock::Image { .. }));
}

#[test]
fn openai_tool_calls() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"location\": \"NYC\"}"
                    }
                }]
            }]
        }),
    );

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_123");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, "{\"location\": \"NYC\"}");
}

#[test]
fn universal_to_openai_format() {
    // fromUniversal("openai", universal) — built straight from the IR.
    let req = request(
        "gpt-4",
        vec![system_msg("You are helpful"), user_msg("Hello")],
    );
    let mut req = req;
    req.generation.temperature = Some(0.7);
    req.generation.max_tokens = Some(100);

    let out = encode_request(P::OpenAiChat, &req);

    field_str_eq(&out, "/model", "gpt-4");
    let messages = field(&out, "/messages").as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are helpful");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Hello");
    assert_eq!(field(&out, "/temperature"), &json!(0.7));
    assert_eq!(field(&out, "/max_tokens"), &json!(100));
}

// ── cross-provider: OpenAI → Anthropic ───────────────────────────────────────

#[test]
fn openai_to_anthropic_cross_provider() {
    // translateBetweenProviders("openai", "anthropic", body)
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "You are helpful"},
                {"role": "user", "content": "Hello"}
            ],
            "temperature": 0.7,
            "max_tokens": 100
        }),
        P::AnthropicMessages,
    );

    field_str_eq(&out, "/model", "gpt-4");
    field_str_eq(&out, "/system", "You are helpful");
    let messages = field(&out, "/messages").as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    // Nyro's Anthropic encoder normalises all message content to block arrays.
    assert_eq!(
        messages[0]["content"],
        json!([{"type": "text", "text": "Hello"}])
    );
    assert_eq!(field(&out, "/temperature"), &json!(0.7));
    assert_eq!(field(&out, "/max_tokens"), &json!(100));
}

// ── tool_choice passthrough ──────────────────────────────────────────────────

#[test]
fn tool_choice_auto_round_trips() {
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "greet",
                    "description": "Greet user",
                    "parameters": {"type": "object", "properties": {}}
                }
            }],
            "tool_choice": "auto"
        }),
    );

    assert!(matches!(req.tool_choice, Some(ToolChoice::Auto)));

    let out = encode_request(P::OpenAiChat, &req);
    assert_eq!(field(&out, "/tool_choice"), "auto");
}

// ── fix-verification ported cases ────────────────────────────────────────────

#[test]
fn null_content_with_tool_calls_decodes_to_empty_text() {
    // fix-verification "should handle null content in assistant messages with
    // tool_calls": the content collapses to empty text, never the literal
    // "null".
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"NYC\"}"}
                }]
            }]
        }),
    );

    assert_eq!(req.messages[0].content.to_text(), "");
    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_123");
    assert_eq!(calls[0].name, "get_weather");
}

#[test]
fn malformed_json_arguments_kept_as_raw_text() {
    // fix-verification "should handle malformed JSON in tool call arguments":
    // Nyro keeps the raw argument text in the IR (llm-bridge parses to `{}`
    // and stashes the original in metadata). Decoding must not fail.
    let req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gpt-4",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {"name": "search", "arguments": "not valid json{"}
                }]
            }]
        }),
    );

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, "not valid json{");
}

#[test]
fn base64_image_reconstructed_as_data_url() {
    // fix-verification "should reconstruct data URL from base64 when no URL
    // available".
    let mut req = request("gpt-4o", vec![Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgo=".to_string(),
            },
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }]);

    let out = encode_request(P::OpenAiChat, &mut req);
    let part = field(&out, "/messages/0/content/0");
    assert_eq!(part["type"], "image_url");
    assert_eq!(part["image_url"]["url"], "data:image/png;base64,iVBORw0KGgo=");
}

#[test]
fn audio_block_encodes_as_input_audio() {
    // fix-verification "should reconstruct input_audio parts in complex
    // content".
    let mut req = request("gpt-4o-audio", vec![Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "What does this say?".to_string(),
                cache_control: None,
            },
            ContentBlock::Audio {
                source: MediaSource::Base64 {
                    media_type: "audio/wav".to_string(),
                    data: "base64audiodata".to_string(),
                },
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }]);

    let out = encode_request(P::OpenAiChat, &mut req);
    let parts = field(&out, "/messages/0/content").as_array().expect("content");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[1]["type"], "input_audio");
    // Nyro writes the reconstructed data URL into `input_audio.data` and
    // emits no `format` field (llm-bridge splits base64 + format).
    assert_eq!(
        parts[1]["input_audio"]["data"],
        "data:audio/wav;base64,base64audiodata"
    );
    assert!(parts[1]["input_audio"].get("format").is_none());
}

#[test]
fn tool_result_message_encodes_with_tool_call_id() {
    // fix-verification "should extract tool_call_id from tool_result content
    // when metadata is missing": a Role::Tool message encodes with
    // `tool_call_id` on the wire.
    let req = request("gpt-4o", vec![
        Message {
            role: Role::Assistant,
            content: MessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall::function("call_xyz", "search", "{\"q\":\"a\"}")]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_xyz".to_string(),
                content: json!("Found results"),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
    ]);

    let out = encode_request(P::OpenAiChat, &req);
    let messages = field(&out, "/messages").as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("tool message missing: {messages:?}"));
    assert_eq!(tool["tool_call_id"], "call_xyz");
    assert_eq!(tool["content"], "Found results");
}
