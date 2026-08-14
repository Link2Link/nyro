//! Anthropic Messages conversion-correctness suite.
//!
//! Ported from `llm-bridge/test/anthropic.test.ts` (+ Anthropic sections of
//! `provider-formats.test.ts`), adapted to Nyro's IR + codec architecture:
//!
//! * `toUniversal("anthropic", body)` → `AnthropicDecoder::decode_request`
//! * `fromUniversal("anthropic", universal)` → `AnthropicEncoder::encode_request`
//! * top-level `system` → leading `Role::System` message (Nyro IR)
//! * `tool_use` blocks → `message.tool_calls` (+ `ContentBlock::ToolUse` in blocks)
//! * `tool_result` → `Role::Tool` message with `tool_call_id`
//!
//! Known IR deviations from the llm-bridge universal format are annotated
//! inline with `KNOWN GAP` where the wire↔IR mapping is lossy in Nyro today.

mod conv_common;

use conv_common::*;

// ── basic text messages ──────────────────────────────────────────────────────

#[test]
fn user_text_message_to_universal() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello, Claude!"}]
        }),
    );

    assert_eq!(req.model, "claude-sonnet-4-20250514");
    assert_eq!(req.generation.max_tokens, Some(1024));
    assert_roles(&req, &[Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "Hello, Claude!");
}

#[test]
fn user_and_assistant_messages_to_universal() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "What is 2+2?"},
                {"role": "assistant", "content": "2+2 equals 4."},
                {"role": "user", "content": "And 3+3?"}
            ]
        }),
    );

    assert_roles(&req, &[Role::User, Role::Assistant, Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "What is 2+2?");
    assert_eq!(req.messages[1].content.to_text(), "2+2 equals 4.");
    assert_eq!(req.messages[2].content.to_text(), "And 3+3?");
}

#[test]
fn array_content_block_with_single_text() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "Describe this image:"}]
            }]
        }),
    );

    assert_eq!(req.messages[0].content.to_text(), "Describe this image:");
}

// ── system message ───────────────────────────────────────────────────────────

#[test]
fn string_system_prompt_becomes_system_message() {
    // llm-bridge lifts `system` into a dedicated field; Nyro keeps it as a
    // leading `Role::System` message.
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": "You are a helpful assistant who speaks French.",
            "messages": [{"role": "user", "content": "Hello"}]
        }),
    );

    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(
        req.messages[0].content.to_text(),
        "You are a helpful assistant who speaks French."
    );
}

#[test]
fn array_system_prompt_joined_into_system_message() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": [
                {"type": "text", "text": "You are a helpful assistant."},
                {"type": "text", "text": "Be concise in your responses."}
            ],
            "messages": [{"role": "user", "content": "Hello"}]
        }),
    );

    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(
        req.messages[0].content.to_text(),
        "You are a helpful assistant.\nBe concise in your responses."
    );
}

#[test]
fn system_with_cache_control_round_trips() {
    let body = json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "system": [{
            "type": "text",
            "text": "You are a helpful assistant with a long context.",
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let req = decode_request(P::AnthropicMessages, body.clone());
    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(
        req.messages[0].content.to_text(),
        "You are a helpful assistant with a long context."
    );

    // Round-trip must re-emit the system array with its cache breakpoint.
    let out = encode_request(P::AnthropicMessages, &req);
    let system = field(&out, "/system");
    if let Some(arr) = system.as_array() {
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
    } else {
        panic!("system with cache_control must round-trip as an array, got {system}");
    }
}

// ── images ───────────────────────────────────────────────────────────────────

#[test]
fn base64_image_source_to_universal() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "iVBORw0KGgo..."
                        }
                    },
                    {"type": "text", "text": "What is in this image?"}
                ]
            }]
        }),
    );

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    assert_eq!(blocks.len(), 2);
    match &blocks[0] {
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "iVBORw0KGgo...");
            }
            other => panic!("expected base64 image source, got {other:?}"),
        },
        other => panic!("expected image block, got {other:?}"),
    }
    assert_eq!(blocks[1].as_text(), Some("What is in this image?"));
}

#[test]
fn url_image_source_to_universal() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image",
                        "source": {
                            "type": "url",
                            "url": "https://example.com/photo.jpg",
                            "media_type": "image/jpeg"
                        }
                    },
                    {"type": "text", "text": "Describe this."}
                ]
            }]
        }),
    );

    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("multimodal content must decode to blocks");
    };
    match &blocks[0] {
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Url(url) => assert_eq!(url, "https://example.com/photo.jpg"),
            other => panic!("expected url image source, got {other:?}"),
        },
        other => panic!("expected image block, got {other:?}"),
    }
    // KNOWN GAP: `MediaSource::Url` does not carry the companion `media_type`.
}

// ── tool definitions and tool_choice ─────────────────────────────────────────

#[test]
fn tool_definitions_with_input_schema() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "What's the weather in Paris?"}],
            "tools": [{
                "name": "get_weather",
                "description": "Get the current weather in a location",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string", "description": "City name"},
                        "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                    },
                    "required": ["location"]
                }
            }]
        }),
    );

    let tools = req.tools.expect("tools preserved");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "get_weather");
    assert_eq!(
        tools[0].description.as_deref(),
        Some("Get the current weather in a location")
    );
    assert_eq!(
        tools[0].parameters,
        json!({
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City name"},
                "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["location"]
        })
    );
}

#[test]
fn tool_choice_auto_to_universal() {
    // KNOWN GAP: the Anthropic decoder only maps the object form
    // `{"type": "auto"}`; the spec-valid string form `"auto"` lands in
    // `ToolChoice::Raw` instead of `ToolChoice::Auto`.
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "name": "greet",
                "description": "Greet user",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": "auto"
        }),
    );

    match req.tool_choice {
        Some(ToolChoice::Auto) => {}
        other => panic!("KNOWN GAP: string `tool_choice` should map to Auto, got {other:?}"),
    }
}

#[test]
fn tool_choice_any_maps_to_required() {
    // llm-bridge keeps `"any"` verbatim; Nyro's IR normalises it to Required.
    // KNOWN GAP: as with `"auto"`, only the object form is parsed today.
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "name": "greet",
                "description": "Greet user",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": "any"
        }),
    );

    match req.tool_choice {
        Some(ToolChoice::Required) => {}
        other => panic!("KNOWN GAP: string `tool_choice` should map to Required, got {other:?}"),
    }
}

// ── tool use / tool result blocks ────────────────────────────────────────────

#[test]
fn assistant_tool_use_blocks_become_tool_calls() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "What's the weather?"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me check the weather for you."},
                        {
                            "type": "tool_use",
                            "id": "toolu_01A09q90qw90lq917835lhak",
                            "name": "get_weather",
                            "input": {"location": "San Francisco", "unit": "celsius"}
                        }
                    ]
                }
            ]
        }),
    );

    assert_eq!(req.messages[1].role, Role::Assistant);
    assert_eq!(
        req.messages[1].content.to_text(),
        "Let me check the weather for you."
    );

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_01A09q90qw90lq917835lhak");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(
        calls[0].arguments,
        "{\"location\":\"San Francisco\",\"unit\":\"celsius\"}"
    );
}

// ── server_tool_use blocks (Claude Code >=2.8 / server tools) ─────────────────

#[test]
fn server_tool_use_block_decodes() {
    // Regression: Claude Code 2.8.x emits `server_tool_use` blocks for
    // server-side tools (webReader / webSearch). Before the fix this payload
    // failed with "data did not match any variant of untagged enum
    // AnthropicContent" (400 from the gateway).
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "glm-5.2",
            "max_tokens": 65535,
            "messages": [
                {"role": "user", "content": "评审变更"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Let me review.", "signature": "sig_01"},
                        {"type": "text", "text": "我来评审当前的变更。"},
                        {
                            "type": "server_tool_use",
                            "id": "call_de0eb30ed07a4f0dbc7688f4",
                            "name": "webReader",
                            "input": {"url": "file:///dev/null"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call_de0eb30ed07a4f0dbc7688f4",
                        "content": [{"type": "text", "text": "MCP error -400"}]
                    }]
                }
            ]
        }),
    );

    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[0].role, Role::User);
    assert_eq!(req.messages[1].role, Role::Assistant);

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_de0eb30ed07a4f0dbc7688f4");
    assert_eq!(calls[0].name, "webReader");
    assert_eq!(calls[0].arguments, "{\"url\":\"file:///dev/null\"}");

    let MessageContent::Blocks(blocks) = &req.messages[1].content else {
        panic!("assistant content must be block form");
    };
    assert!(matches!(
        blocks[2],
        ContentBlock::ServerToolUse { ref name, .. } if name == "webReader"
    ));

    // The tool result resolves to a Role::Tool message carrying the call id.
    assert_eq!(req.messages[2].role, Role::Tool);
    assert_eq!(
        req.messages[2].tool_call_id.as_deref(),
        Some("call_de0eb30ed07a4f0dbc7688f4")
    );
}

#[test]
fn server_tool_use_round_trips_to_anthropic() {
    let body = json!({
        "model": "glm-5.2",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "What's the latest news?"},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me search."},
                    {
                        "type": "server_tool_use",
                        "id": "srvtool_01",
                        "name": "webSearch",
                        "input": {"query": "claude code 2.8"}
                    }
                ]
            }
        ]
    });

    let out = round_trip_request(P::AnthropicMessages, body);

    let content = field(&out, "/messages/1/content")
        .as_array()
        .expect("content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "server_tool_use");
    assert_eq!(content[1]["id"], "srvtool_01");
    assert_eq!(content[1]["name"], "webSearch");
    assert_eq!(content[1]["input"], json!({"query": "claude code 2.8"}));
}

#[test]
fn server_tool_use_translates_to_openai_chat() {
    // Cross-protocol: a server tool call must surface as an OpenAI
    // `tool_calls` entry and must NOT leak into `content` (OpenAI
    // chat/completions rejects unknown content part types). The call is
    // paired with its tool_result, as real Claude Code traffic always is.
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "glm-5.2",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "What's the latest news?"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "server_tool_use",
                            "id": "srvtool_01",
                            "name": "webSearch",
                            "input": {"query": "claude code"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "srvtool_01",
                        "content": "2.8.4 released"
                    }]
                }
            ]
        }),
        P::OpenAiChat,
    );

    let calls = field(&out, "/messages/1/tool_calls")
        .as_array()
        .expect("tool_calls array");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "webSearch");
    assert_eq!(calls[0]["id"], "srvtool_01");

    let content = field(&out, "/messages/1/content");
    assert!(
        content.is_null() || content.as_array().is_none_or(|a| a.is_empty()),
        "server_tool_use must be stripped from OpenAI content, got: {content}"
    );

    assert_eq!(field(&out, "/messages/2/role"), "tool");
    assert_eq!(field(&out, "/messages/2/tool_call_id"), "srvtool_01");
}

// ── redacted_thinking blocks ──────────────────────────────────────────────────

#[test]
fn redacted_thinking_block_decodes() {
    // Regression: the wire decoder must accept redacted thinking blocks
    // (returned when the thinking signature fails verification and echoed
    // back verbatim in subsequent request history). Same failure class as
    // server_tool_use — "data did not match any variant of untagged enum
    // AnthropicContent".
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Think hard."},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "redacted_thinking", "data": "YWJjZGVmZ2hpams="},
                        {"type": "text", "text": "Done thinking."}
                    ]
                }
            ]
        }),
    );

    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[1].role, Role::Assistant);

    let MessageContent::Blocks(blocks) = &req.messages[1].content else {
        panic!("assistant content must be block form");
    };
    assert_eq!(blocks.len(), 2);
    assert!(matches!(
        blocks[0],
        ContentBlock::RedactedThinking { ref data } if data == "YWJjZGVmZ2hpams="
    ));
    assert_eq!(blocks[1].as_text(), Some("Done thinking."));
}

#[test]
fn redacted_thinking_round_trips_to_anthropic() {
    let body = json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "Think hard."},
            {
                "role": "assistant",
                "content": [
                    {"type": "redacted_thinking", "data": "YWJjZGVmZ2hpams="},
                    {"type": "text", "text": "Done thinking."}
                ]
            }
        ]
    });

    let out = round_trip_request(P::AnthropicMessages, body);

    let content = field(&out, "/messages/1/content")
        .as_array()
        .expect("content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "redacted_thinking");
    assert_eq!(content[0]["data"], "YWJjZGVmZ2hpams=");
}

// ── fast mode passthrough ─────────────────────────────────────────────────────

#[test]
fn speed_field_round_trips() {
    // Claude Code >=2.8 sends `speed: "fast"` when fast mode is on; it must
    // survive the Anthropic round-trip instead of being silently dropped.
    let body = json!({
        "model": "glm-5.2",
        "max_tokens": 1024,
        "speed": "fast",
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let out = round_trip_request(P::AnthropicMessages, body);
    assert_eq!(field(&out, "/speed"), "fast");
}

#[test]
fn no_speed_when_absent() {
    let body = json!({
        "model": "glm-5.2",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let out = round_trip_request(P::AnthropicMessages, body);
    assert!(
        out.get("speed").is_none(),
        "no speed expected, got: {:?}",
        out.get("speed")
    );
}

#[test]
fn full_claude_code_request_shape_round_trips() {
    // End-to-end shape test mirroring the real Claude Code 2.8.4 → glm-5.2
    // traffic that triggered the original 400: block-form system with
    // cache_control, adaptive thinking, output_config effort, fast speed,
    // a thinking+signature block, a server_tool_use call paired with its
    // tool_result, and metadata. Every part must survive decode → encode
    // without error.
    let body = json!({
        "model": "glm-5.2",
        "max_tokens": 65535,
        "speed": "fast",
        "stream": true,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "max"},
        "metadata": {"user_id": "device-1", "session_id": "sess-1"},
        "system": [
            {"type": "text", "text": "You are Claude Code."},
            {
                "type": "text",
                "text": "Long instructions...",
                "cache_control": {"type": "ephemeral"}
            }
        ],
        "messages": [
            {"role": "user", "content": "评审变更"},
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Let me review.", "signature": "sig_01"},
                    {"type": "text", "text": "我来评审。"},
                    {
                        "type": "server_tool_use",
                        "id": "call_de0eb30ed07a4f0dbc7688f4",
                        "name": "webReader",
                        "input": {"url": "file:///dev/null"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_de0eb30ed07a4f0dbc7688f4",
                    "content": [{"type": "text", "text": "MCP error -400"}]
                }]
            }
        ]
    });

    let out = round_trip_request(P::AnthropicMessages, body.clone());

    assert_eq!(field(&out, "/model"), "glm-5.2");
    assert_eq!(field(&out, "/max_tokens"), &json!(65535));
    assert_eq!(field(&out, "/speed"), "fast");
    assert_eq!(field(&out, "/stream"), &json!(true));
    assert_eq!(field(&out, "/thinking"), &json!({"type": "adaptive"}));
    assert_eq!(field(&out, "/output_config"), &json!({"effort": "max"}));
    assert_eq!(
        field(&out, "/metadata"),
        &json!({"user_id": "device-1", "session_id": "sess-1"})
    );

    // system blocks (with cache_control) preserved as an array.
    let system = field(&out, "/system").as_array().expect("system array");
    assert_eq!(system.len(), 2);
    assert_eq!(system[1]["cache_control"], json!({"type": "ephemeral"}));

    // Assistant turn keeps thinking + text + server_tool_use in order.
    let content = field(&out, "/messages/1/content")
        .as_array()
        .expect("content array");
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "sig_01");
    assert_eq!(content[2]["type"], "server_tool_use");
    assert_eq!(content[2]["name"], "webReader");

    // Tool result survives as a user message with a tool_result block.
    let tool_msg = field(&out, "/messages/2/content")
        .as_array()
        .expect("tool content array");
    assert_eq!(tool_msg[0]["type"], "tool_result");
    assert_eq!(tool_msg[0]["tool_use_id"], "call_de0eb30ed07a4f0dbc7688f4");

    // And the decoded IR is equally sound. The block-form system becomes a
    // leading Role::System message, so: [system, user, assistant, tool].
    let req = decode_request(P::AnthropicMessages, body);
    assert_eq!(req.generation.max_tokens, Some(65535));
    assert!(req.stream.enabled);
    assert!(req.reasoning.enabled);
    assert_eq!(req.messages[0].role, Role::System);
    assert_eq!(req.messages[2].role, Role::Assistant);
    assert_eq!(req.messages[3].role, Role::Tool);
    assert_eq!(tool_calls(&req)[0].name, "webReader");
    // The server tool result keeps its upstream-issued id.
    assert_eq!(
        req.messages[3].tool_call_id.as_deref(),
        Some("call_de0eb30ed07a4f0dbc7688f4")
    );
}

#[test]
fn tool_result_to_tool_message() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01A09q90qw90lq917835lhak",
                    "content": "15°C, partly cloudy"
                }]
            }]
        }),
    );

    assert_eq!(req.messages[0].role, Role::Tool);
    assert_eq!(
        req.messages[0].tool_call_id.as_deref(),
        Some("toolu_01A09q90qw90lq917835lhak")
    );
    assert_eq!(req.messages[0].content.to_text(), "15°C, partly cloudy");
}

#[test]
fn tool_result_is_error_flag_decode_gap() {
    // KNOWN GAP: the Anthropic wire type drops `is_error` on decode; the
    // request still decodes and the correlation survives, but the flag is lost.
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_error_123",
                    "content": "Error: Location not found",
                    "is_error": true
                }]
            }]
        }),
    );

    assert_eq!(req.messages[0].role, Role::Tool);
    assert_eq!(
        req.messages[0].tool_call_id.as_deref(),
        Some("toolu_error_123")
    );
    assert_eq!(
        req.messages[0].content.to_text(),
        "Error: Location not found"
    );
}

// ── extended thinking ────────────────────────────────────────────────────────

#[test]
fn thinking_config_maps_to_reasoning() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 16000,
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "messages": [{"role": "user", "content": "Solve this complex math problem."}]
        }),
    );

    assert!(req.reasoning.enabled, "reasoning must be enabled");
    assert_eq!(req.reasoning.budget_tokens, Some(10000));
}

#[test]
fn thinking_content_block_with_signature() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 16000,
            "messages": [
                {"role": "user", "content": "Solve this."},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "Let me reason through this step by step...",
                            "signature": "ErUBCkYIAxgCIkD3sMj2test_sig"
                        },
                        {"type": "text", "text": "The answer is 42."}
                    ]
                }
            ]
        }),
    );

    let MessageContent::Blocks(blocks) = &req.messages[1].content else {
        panic!("thinking content must decode to blocks");
    };
    assert_eq!(blocks.len(), 2);
    match &blocks[0] {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "Let me reason through this step by step...");
            assert_eq!(signature.as_deref(), Some("ErUBCkYIAxgCIkD3sMj2test_sig"));
        }
        other => panic!("expected thinking block, got {other:?}"),
    }
    assert_eq!(blocks[1].as_text(), Some("The answer is 42."));
}

#[test]
fn redacted_thinking_encodes_from_ir() {
    // KNOWN GAP: the block encoder supports `redacted_thinking` but the final
    // payload validation (`ALLOWED_BLOCK_TYPES`) rejects it, so the request
    // fails with "unsupported block type" instead of round-tripping.
    use nyro_core::protocol::RequestEncoder;
    use nyro_core::protocol::codec::anthropic::messages::encoder::AnthropicEncoder;

    let req = request(
        "claude-sonnet-4-20250514",
        vec![
            user_msg("Explain."),
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::RedactedThinking {
                        data: "base64encodeddata...".to_string(),
                    },
                    ContentBlock::Text {
                        text: "Here is my response.".to_string(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (out, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("redacted_thinking blocks should encode");
    let blocks = field(&out, "/messages/1/content")
        .as_array()
        .expect("content array");
    assert_eq!(blocks[0]["type"], "redacted_thinking");
    assert_eq!(blocks[0]["data"], "base64encodeddata...");
    assert_eq!(blocks[1]["type"], "text");
    assert_eq!(blocks[1]["text"], "Here is my response.");
}

// ── cache_control on content blocks ──────────────────────────────────────────

#[test]
fn cache_control_on_content_block_round_trips() {
    // The decoder collapses a single text block into `MessageContent::Text`
    // (losing the per-block flag in the IR), but the raw wire snapshot keeps
    // the cache breakpoint alive for the round-trip.
    let body = json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "Here is a very long document to cache...",
                "cache_control": {"type": "ephemeral"}
            }]
        }]
    });

    let req = decode_request(P::AnthropicMessages, body);
    assert_eq!(
        req.messages[0].content.to_text(),
        "Here is a very long document to cache..."
    );

    let out = encode_request(P::AnthropicMessages, &req);
    let blocks = field(&out, "/messages/0/content")
        .as_array()
        .expect("content array");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
}

#[test]
fn no_cache_control_when_absent() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "Normal message"}]
            }]
        }),
    );

    assert_eq!(req.messages[0].content.to_text(), "Normal message");
    // No cache breakpoint was requested, so the wire round-trip must not
    // invent one.
    let out = encode_request(P::AnthropicMessages, &req);
    let blocks = field(&out, "/messages/0/content")
        .as_array()
        .expect("content array");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "Normal message");
    assert!(
        blocks[0].get("cache_control").is_none(),
        "no cache_control expected, got {:?}",
        blocks[0]
    );
}

// ── provider params ──────────────────────────────────────────────────────────

#[test]
fn temperature_top_p_and_stream_are_preserved() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 2048,
            "temperature": 0.7,
            "top_p": 0.9,
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}]
        }),
    );

    assert_eq!(req.generation.temperature, Some(0.7));
    assert_eq!(req.generation.top_p, Some(0.9));
    assert!(req.stream.enabled);
}

#[test]
fn stop_sequences_map_to_generation_stop() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "stop_sequences": ["\n\nHuman:", "\n\nAssistant:"],
            "messages": [{"role": "user", "content": "Hello"}]
        }),
    );

    assert_eq!(
        req.generation.stop,
        Some(vec!["\n\nHuman:".to_string(), "\n\nAssistant:".to_string()])
    );
}

// ── round-trip: anthropic → universal → anthropic ────────────────────────────

#[test]
fn round_trip_basic_text_messages() {
    let out = round_trip_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Hello, Claude!"},
                {"role": "assistant", "content": "Hello! How can I help you?"},
                {"role": "user", "content": "Tell me about TypeScript."}
            ]
        }),
    );

    field_str_eq(&out, "/model", "claude-sonnet-4-20250514");
    assert_eq!(field(&out, "/max_tokens"), &json!(1024));
    assert_eq!(field(&out, "/messages").as_array().map(Vec::len), Some(3));
}

#[test]
fn round_trip_system_prompt() {
    let out = round_trip_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": "You are a pirate.",
            "messages": [{"role": "user", "content": "Hello!"}]
        }),
    );

    field_str_eq(&out, "/system", "You are a pirate.");
}

#[test]
fn round_trip_tools() {
    let out = round_trip_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Get weather"}],
            "tools": [{
                "name": "get_weather",
                "description": "Get weather for a location",
                "input_schema": {
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }
            }]
        }),
    );

    let tools = field(&out, "/tools").as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(
        tools[0]["input_schema"],
        json!({
            "type": "object",
            "properties": {"location": {"type": "string"}},
            "required": ["location"]
        })
    );
}

#[test]
fn round_trip_thinking_config() {
    let out = round_trip_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 16000,
            "thinking": {"type": "enabled", "budget_tokens": 8000},
            "messages": [{"role": "user", "content": "Think deeply."}]
        }),
    );

    assert_eq!(field(&out, "/thinking/type"), "enabled");
    assert_eq!(field(&out, "/thinking/budget_tokens"), &json!(8000));
}

#[test]
fn round_trip_thinking_block_with_signature() {
    let out = round_trip_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 16000,
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "messages": [
                {"role": "user", "content": "Solve this problem."},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "Let me reason step by step...",
                            "signature": "ErUBCkYIAxgCIkD3sMj2example_signature_base64"
                        },
                        {"type": "text", "text": "The answer is 42."}
                    ]
                },
                {"role": "user", "content": "Can you explain further?"}
            ]
        }),
    );

    let blocks = field(&out, "/messages/1/content")
        .as_array()
        .expect("content array");
    let thinking = blocks
        .iter()
        .find(|b| b["type"] == "thinking")
        .unwrap_or_else(|| panic!("thinking block missing: {blocks:?}"));
    assert_eq!(thinking["thinking"], "Let me reason step by step...");
    assert_eq!(
        thinking["signature"],
        "ErUBCkYIAxgCIkD3sMj2example_signature_base64"
    );
}

#[test]
fn thinking_signature_survives_direct_ir_construction() {
    // llm-bridge "slow path": a universal built without `_original` still
    // carries the thinking signature through `fromUniversal`. Here the IR is
    // built directly, which is Nyro's slow path.
    let req = request(
        "claude-sonnet-4-20250514",
        vec![
            user_msg("Explain."),
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Thinking {
                        thinking: "Cross-provider thinking content".to_string(),
                        signature: Some("sig_from_cross_provider_roundtrip".to_string()),
                    },
                    ContentBlock::Text {
                        text: "Here is my answer.".to_string(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            user_msg("Follow up?"),
        ],
    );

    let out = encode_request(P::AnthropicMessages, &req);
    let blocks = field(&out, "/messages/1/content")
        .as_array()
        .expect("content array");
    let thinking = blocks
        .iter()
        .find(|b| b["type"] == "thinking")
        .unwrap_or_else(|| panic!("thinking block missing: {blocks:?}"));
    assert_eq!(thinking["thinking"], "Cross-provider thinking content");
    assert_eq!(thinking["signature"], "sig_from_cross_provider_roundtrip");
}

#[test]
fn round_trip_temperature_and_stop_sequences() {
    let out = round_trip_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "temperature": 0.5,
            "top_p": 0.95,
            "stop_sequences": ["STOP"],
            "messages": [{"role": "user", "content": "Hello"}]
        }),
    );

    assert_eq!(field(&out, "/temperature"), &json!(0.5));
    assert_eq!(field(&out, "/top_p"), &json!(0.95));
    assert_eq!(field(&out, "/stop_sequences"), &json!(["STOP"]));
}

// ── fromUniversal: anthropic output ──────────────────────────────────────────

#[test]
fn tool_choice_auto_emits_object_wire_format() {
    // llm-bridge: string `"auto"` in universal → `{ "type": "auto" }` on wire.
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "name": "greet",
                "description": "Greet user",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": "auto"
        }),
    );

    let out = encode_request(P::AnthropicMessages, &req);
    assert_eq!(field(&out, "/tool_choice"), &json!({"type": "auto"}));
}

#[test]
fn tool_choice_named_emits_tool_wire_format() {
    let req = request(
        "claude-sonnet-4-20250514",
        vec![user_msg("Use the calculator")],
    );
    let mut req = req;
    req.tools = Some(vec![ToolSpec {
        name: "calculator".to_string(),
        description: Some("Do math".to_string()),
        kind: Default::default(),
        namespace: None,
        parameters: json!({"type": "object", "properties": {}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    req.tool_choice = Some(ToolChoice::Named {
        name: "calculator".to_string(),
        namespace: None,
    });

    let out = encode_request(P::AnthropicMessages, &req);
    assert_eq!(
        field(&out, "/tool_choice"),
        &json!({"type": "tool", "name": "calculator"})
    );
}

#[test]
fn universal_base64_image_to_anthropic_wire() {
    let req = request(
        "claude-sonnet-4-20250514",
        vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::Base64 {
                    media_type: "image/jpeg".to_string(),
                    data: "base64data".to_string(),
                },
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let out = encode_request(P::AnthropicMessages, &req);
    let block = field(&out, "/messages/0/content/0");
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "base64");
    assert_eq!(block["source"]["data"], "base64data");
    assert_eq!(block["source"]["media_type"], "image/jpeg");
}

#[test]
fn universal_url_image_to_anthropic_wire() {
    let req = request(
        "claude-sonnet-4-20250514",
        vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::Url("https://example.com/image.png".to_string()),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let out = encode_request(P::AnthropicMessages, &req);
    let block = field(&out, "/messages/0/content/0");
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "url");
    assert_eq!(block["source"]["url"], "https://example.com/image.png");
}

#[test]
fn universal_thinking_content_back_to_anthropic() {
    let req = request(
        "claude-sonnet-4-20250514",
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

    let out = encode_request(P::AnthropicMessages, &req);
    let blocks = field(&out, "/messages/0/content")
        .as_array()
        .expect("content array");
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "Let me think...");
    assert_eq!(blocks[1]["type"], "text");
    assert_eq!(blocks[1]["text"], "The answer is 42.");
}

#[test]
fn universal_tool_result_with_is_error_back_to_anthropic() {
    let req = request(
        "claude-sonnet-4-20250514",
        vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_err456".to_string(),
                content: json!("Error: service unavailable"),
                is_error: Some(true),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let out = encode_request(P::AnthropicMessages, &req);
    let block = field(&out, "/messages/0/content/0");
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "toolu_err456");
    assert_eq!(block["is_error"], true);
    assert_eq!(block["content"], "Error: service unavailable");
}

// ── provider-formats.test.ts: Anthropic section ──────────────────────────────

#[test]
fn anthropic_request_to_universal_basic() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-3-sonnet-20240229",
            "messages": [{"role": "user", "content": "Hello Claude"}],
            "system": "You are Claude",
            "max_tokens": 200,
            "temperature": 0.5
        }),
    );

    assert_eq!(req.model, "claude-3-sonnet-20240229");
    assert_roles(&req, &[Role::System, Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "You are Claude");
    assert_eq!(req.messages[1].content.to_text(), "Hello Claude");
    assert_eq!(req.generation.temperature, Some(0.5));
    assert_eq!(req.generation.max_tokens, Some(200));
}

#[test]
fn anthropic_multimodal_content() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-3-sonnet-20240229",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": "xyz"
                        }
                    }
                ]
            }],
            "max_tokens": 100
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
fn anthropic_tool_use() {
    let req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "claude-3-sonnet-20240229",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "get_weather",
                    "input": {"location": "NYC"}
                }]
            }],
            "max_tokens": 100
        }),
    );

    let calls = tool_calls(&req);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_123");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, "{\"location\":\"NYC\"}");
}

#[test]
fn universal_to_anthropic_format() {
    // fromUniversal("anthropic", universal) — built straight from the IR.
    let mut req = request(
        "claude-3-sonnet-20240229",
        vec![system_msg("You are helpful"), user_msg("Hello Claude")],
    );
    req.generation.temperature = Some(0.5);
    req.generation.max_tokens = Some(200);

    let out = encode_request(P::AnthropicMessages, &req);

    field_str_eq(&out, "/model", "claude-3-sonnet-20240229");
    field_str_eq(&out, "/system", "You are helpful");
    let messages = field(&out, "/messages").as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"],
        json!([{"type": "text", "text": "Hello Claude"}])
    );
    assert_eq!(field(&out, "/temperature"), &json!(0.5));
    assert_eq!(field(&out, "/max_tokens"), &json!(200));
}

// ── cross-provider: Anthropic → Google ───────────────────────────────────────

#[test]
fn anthropic_to_google_cross_provider() {
    // translateBetweenProviders("anthropic", "google", body)
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "claude-3-sonnet-20240229",
            "messages": [{"role": "user", "content": "Hello Claude"}],
            "system": "You are Claude",
            "max_tokens": 200,
            "temperature": 0.5
        }),
        P::GoogleGemini,
    );

    let contents = field(&out, "/contents").as_array().expect("contents array");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Hello Claude");
    assert_eq!(
        field(&out, "/systemInstruction/parts/0/text"),
        "You are Claude"
    );
    assert_eq!(field(&out, "/generationConfig/temperature"), &json!(0.5));
    assert_eq!(
        field(&out, "/generationConfig/maxOutputTokens"),
        &json!(200)
    );
}

// ── fix-verification ported cases ────────────────────────────────────────────

#[test]
fn max_tokens_defaults_to_4096_when_undefined() {
    // fix-verification "should default to 1024 when max_tokens is undefined":
    // Nyro's Anthropic encoder defaults to 4096, not 1024.
    let req = request("claude-3-sonnet-20240229", vec![user_msg("Hello")]);

    let out = encode_request(P::AnthropicMessages, &req);
    assert_eq!(field(&out, "/max_tokens"), &json!(4096));
}

#[test]
fn developer_message_missing_text_does_not_produce_undefined() {
    // fix-verification "should not produce 'undefined' text from developer
    // messages with missing text": with IR text blocks the text is always
    // present (possibly empty); the encoded system prompt must never contain
    // the literal "undefined".
    let req = request(
        "claude-3-sonnet-20240229",
        vec![
            Message {
                role: Role::System,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Be helpful".to_string(),
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: String::new(),
                        cache_control: None,
                    },
                    ContentBlock::Image {
                        source: MediaSource::Url("https://example.com/img.png".to_string()),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            user_msg("Hello"),
        ],
    );

    let out = encode_request(P::AnthropicMessages, &req);
    let system = field_str(&out, "/system");
    assert!(system.contains("Be helpful"), "system: {system:?}");
    assert!(
        !system.contains("undefined"),
        "no undefined text: {system:?}"
    );
}
