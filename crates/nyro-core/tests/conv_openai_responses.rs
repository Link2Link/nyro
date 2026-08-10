//! OpenAI Responses API conversion-correctness suite.
//!
//! Ported from `llm-bridge/test/openai-responses.test.ts`, adapted to Nyro's
//! IR + codec architecture:
//!
//! * `toUniversal("openai-responses", body)` → `ResponsesDecoder::decode_request`
//! * `fromUniversal("openai-responses", universal)` → `ResponsesEncoder::encode_request`
//! * `instructions` → leading `Role::System` message
//! * `function_call_output` items → `Role::Tool` messages
//! * built-in tools (`web_search_preview` …) → tools named `__builtin__<type>`
//! * the encoder forces `stream: true` (Responses API is streaming-only)
//!
//! Known IR deviations from the llm-bridge universal format are annotated
//! inline with `KNOWN GAP`.

mod conv_common;

use conv_common::*;

// ── basic text input ─────────────────────────────────────────────────────────

#[test]
fn string_input_becomes_single_user_message() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": "What is the capital of France?"
        }),
    );

    assert_eq!(req.model, "gpt-4o");
    assert_roles(&req, &[Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "What is the capital of France?");
}

#[test]
fn input_text_items_to_universal() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "Hello, how are you?"}]
            }]
        }),
    );

    assert_roles(&req, &[Role::User]);
    assert_eq!(req.messages[0].content.to_text(), "Hello, how are you?");
}

#[test]
fn multi_turn_input_with_roles() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [
                {"role": "user", "content": "Hi"},
                {"role": "assistant", "content": "Hello! How can I help?"},
                {"role": "user", "content": "Tell me a joke."}
            ]
        }),
    );

    assert_roles(&req, &[Role::User, Role::Assistant, Role::User]);
    assert_eq!(req.messages[2].content.to_text(), "Tell me a joke.");
}

// ── input_image content type ─────────────────────────────────────────────────

#[test]
fn input_image_decodes_to_image_block() {
    // KNOWN GAP: `input_image` content blocks are dropped by the Responses
    // decoder (only `input_text`-style blocks are kept). The text part
    // survives; the image does not.
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Describe this image."},
                    {"type": "input_image", "image_url": "https://example.com/photo.jpg"}
                ]
            }]
        }),
    );

    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].content.to_text(), "Describe this image.");
    let MessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!(
            "KNOWN GAP: input_image should decode to an image block, content collapsed to {:?}",
            req.messages[0].content
        );
    };
    assert!(
        blocks.iter().any(|b| matches!(b, ContentBlock::Image { .. })),
        "KNOWN GAP: input_image should decode to an image block: {blocks:?}"
    );
}

// ── function_call_output items → tool results ────────────────────────────────

#[test]
fn function_call_output_becomes_tool_message() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [
                {"role": "user", "content": "What's the weather?"},
                {
                    "type": "function_call_output",
                    "call_id": "call_xyz789",
                    "output": "{\"temp\": 20, \"condition\": \"sunny\"}"
                }
            ]
        }),
    );

    assert_roles(&req, &[Role::User, Role::Tool]);
    assert_eq!(req.messages[1].role, Role::Tool);
    assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_xyz789"));
    assert_eq!(
        req.messages[1].content.to_text(),
        "{\"temp\": 20, \"condition\": \"sunny\"}"
    );
}

// ── tool definitions (flattened format) ──────────────────────────────────────

#[test]
fn flattened_tool_format_to_universal() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Get weather"}],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get the weather for a city",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }]
        }),
    );

    let tools = req.tools.expect("tools preserved");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "get_weather");
    assert_eq!(tools[0].description.as_deref(), Some("Get the weather for a city"));
    assert_eq!(tools[0].strict, Some(true));
    assert_eq!(
        tools[0].parameters,
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        })
    );
}

// ── generation params ────────────────────────────────────────────────────────

#[test]
fn max_output_tokens_maps_to_max_tokens() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Hello"}],
            "max_output_tokens": 2048
        }),
    );

    assert_eq!(req.generation.max_tokens, Some(2048));
}

#[test]
fn reasoning_config_maps_to_reasoning() {
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "o1",
            "input": [{"role": "user", "content": "Solve this complex problem step by step."}],
            "reasoning": {"effort": "high", "summary": "auto"}
        }),
    );

    assert!(req.reasoning.enabled, "reasoning must be enabled");
    assert_eq!(req.reasoning.effort, Some(ReasoningEffort::High));
    assert_eq!(req.reasoning.display.as_deref(), Some("auto"));
}

// ── structured output ────────────────────────────────────────────────────────

#[test]
fn text_format_json_schema_maps_to_response_format() {
    // KNOWN GAP: `text.format` is only pass-through in the ingress bag; the
    // Responses decoder does not map it into `response_format`.
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Extract the person's details."}],
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "person_info",
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

    match req.response_format.expect(
        "KNOWN GAP: text.format json_schema should map to ResponseFormat::JsonSchema",
    ) {
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => {
            assert_eq!(name, "person_info");
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
fn text_format_json_object_maps_to_response_format() {
    // KNOWN GAP: as above, `text.format` json_object is not mapped into IR.
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Return JSON."}],
            "text": {"format": {"type": "json_object"}}
        }),
    );

    assert!(
        matches!(req.response_format, Some(ResponseFormat::JsonObject)),
        "KNOWN GAP: text.format json_object should map to ResponseFormat::JsonObject, got {:?}",
        req.response_format
    );
}

// ── built-in tools pass-through ──────────────────────────────────────────────

#[test]
fn builtin_tools_become_prefixed_tool_specs() {
    // llm-bridge keeps built-in tools in `provider_params.builtin_tools`;
    // Nyro folds them into `tools` as `__builtin__<type>` specs.
    let req = decode_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Search for the latest news."}],
            "tools": [
                {"type": "web_search_preview"},
                {
                    "type": "function",
                    "name": "get_info",
                    "description": "Get info",
                    "parameters": {"type": "object", "properties": {}}
                }
            ]
        }),
    );

    let tools = req.tools.expect("tools preserved");
    assert_eq!(tools.len(), 2);
    assert!(
        tools.iter().any(|t| t.name == "__builtin__web_search_preview"),
        "builtin tool should be prefixed: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert!(tools.iter().any(|t| t.name == "get_info"));
}

// ── round-trip: openai-responses → universal → openai-responses ──────────────

#[test]
fn round_trip_basic_conversation() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello!"}
            ],
            "temperature": 0.8,
            "max_output_tokens": 500,
            "top_p": 0.9
        }),
    );

    field_str_eq(&out, "/model", "gpt-4o");
    assert_eq!(field(&out, "/temperature"), &json!(0.8));
    assert_eq!(field(&out, "/top_p"), &json!(0.9));
    // System message is lifted to `instructions` on the wire (llm-bridge keeps
    // it in `input`; both round-trip the information).
    assert_eq!(
        field(&out, "/instructions"),
        "You are a helpful assistant."
    );
    let input = field(&out, "/input").as_array().expect("input array");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["text"], "Hello!");
}

#[test]
fn round_trip_max_output_tokens() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Hello!"}],
            "max_output_tokens": 500
        }),
    );

    assert_eq!(
        field(&out, "/max_output_tokens"),
        &json!(500),
        "KNOWN GAP: `max_output_tokens` is decoded to IR but the encoder never re-emits it"
    );
}

#[test]
fn round_trip_function_tools_flattened_format() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Look up the weather"}],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }]
        }),
    );

    let tools = field(&out, "/tools").as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["strict"], true);
    assert_eq!(
        tools[0]["parameters"],
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        })
    );
}

#[test]
fn round_trip_function_call_output_items() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [
                {"role": "user", "content": "Get the weather"},
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": "{\"temp\":25}"
                }
            ]
        }),
    );

    let input = field(&out, "/input").as_array().expect("input array");
    assert_eq!(input.len(), 2);
    let fc = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap_or_else(|| panic!("function_call_output missing: {input:?}"));
    assert_eq!(fc["call_id"], "call_abc");
    assert_eq!(fc["output"], "{\"temp\":25}");
}

#[test]
fn round_trip_reasoning_config() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "o1",
            "input": "Think carefully.",
            "reasoning": {"effort": "high", "summary": "auto"}
        }),
    );

    assert_eq!(field(&out, "/reasoning/effort"), "high");
    assert_eq!(field(&out, "/reasoning/summary"), "auto");
}

#[test]
fn round_trip_structured_output_via_text_format() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": "Extract data.",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "data",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {"key": {"type": "string"}},
                        "required": ["key"],
                        "additionalProperties": false
                    }
                }
            }
        }),
    );

    let text = field(&out, "/text");
    assert_eq!(text["format"]["type"], "json_schema");
    assert_eq!(text["format"]["name"], "data");
    assert_eq!(text["format"]["strict"], true);
    assert_eq!(
        text["format"]["schema"],
        json!({
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"],
            "additionalProperties": false
        })
    );
}

#[test]
fn round_trip_builtin_tools_alongside_function_tools() {
    let out = round_trip_request(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Search for news"}],
            "tools": [
                {"type": "web_search_preview"},
                {
                    "type": "function",
                    "name": "summarize",
                    "description": "Summarize text",
                    "parameters": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}}
                    }
                }
            ]
        }),
    );

    let tools = field(&out, "/tools").as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(
        tools.iter().any(|t| t["type"] == "web_search_preview"),
        "builtin tool should round-trip: {tools:?}"
    );
    let func = tools
        .iter()
        .find(|t| t["type"] == "function")
        .unwrap_or_else(|| panic!("function tool missing: {tools:?}"));
    assert_eq!(func["name"], "summarize");
}

// ── fix-verification ported cases ────────────────────────────────────────────

#[test]
fn multiple_tool_results_become_function_call_output_items() {
    // fix-verification "should convert multiple tool_results in user message to
    // function_call_output items": llm-bridge emits one item per tool_result.
    let mut req = request("gpt-4o", vec![Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_abc".to_string(),
                content: json!("Weather is sunny"),
                is_error: None,
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_def".to_string(),
                content: json!({"temperature": 72}),
                is_error: None,
                cache_control: None,
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }]);

    let out = encode_request(P::OpenAiResponses, &mut req);
    let input = field(&out, "/input").as_array().expect("input array");
    let outputs: Vec<_> = input
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 2, "{input:?}");
    assert_eq!(outputs[0]["call_id"], "call_abc");
    assert_eq!(outputs[0]["output"], "Weather is sunny");
    assert_eq!(outputs[1]["call_id"], "call_def");
    assert_eq!(outputs[1]["output"], "{\"temperature\":72}");
}
