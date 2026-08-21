use nyro_core::protocol::codec::anthropic::messages::decoder::AnthropicDecoder;
use nyro_core::protocol::codec::anthropic::messages::encoder::AnthropicEncoder;
use nyro_core::protocol::codec::anthropic::messages::stream::AnthropicResponseFormatter;
use nyro_core::protocol::codec::google::gemini::decoder::GoogleDecoder;
use nyro_core::protocol::codec::google::gemini::encoder::GoogleEncoder;
use nyro_core::protocol::codec::google::gemini::stream::GoogleStreamFormatter;
use nyro_core::protocol::codec::openai::compatible::decoder::OpenAIDecoder;
use nyro_core::protocol::codec::openai::compatible::encoder::OpenAIEncoder;
use nyro_core::protocol::codec::openai::compatible::stream::OpenAIStreamFormatter;
use nyro_core::protocol::codec::openai::responses::decoder::ResponsesDecoder;
use nyro_core::protocol::codec::openai::responses::encoder::ResponsesEncoder;
use nyro_core::protocol::codec::openai::responses::formatter::ResponsesResponseFormatter;
use nyro_core::protocol::codec::openai::responses::parser::{
    ResponsesResponseParser, ResponsesStreamParser,
};
use nyro_core::protocol::codec::openai::responses::stream::ResponsesStreamFormatter;
use nyro_core::protocol::codec::reasoning::normalize_response_reasoning;
use nyro_core::protocol::codec::tool_bridge::ToolRoutePlan;
use nyro_core::protocol::codec::tool_correlation::normalize_request_tool_results;
use nyro_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
};
use nyro_core::protocol::ir::usage::Usage;
use nyro_core::protocol::ir::{
    AiRequest, AiResponse as IrAiResponse, AiStreamDelta as IrStreamDelta,
    ContentBlock as IrContentBlock, MediaSource, Message, MessageContent as IrMessageContent,
    ReasoningConfig, ReasoningEffort, Role as IrRole, StreamConfig, ToolCall, ToolCallKind,
    ToolSpec, ToolSpecKind,
};
use nyro_core::protocol::{
    RequestDecoder, RequestEncoder, ResponseDecoder, ResponseEncoder, StreamResponseDecoder,
    StreamResponseEncoder,
};

#[test]
fn openai_to_anthropic_thinking_blocks() {
    let mut resp = IrAiResponse::new("msg_1", "minimax-m2.7");
    resp.content = "hello".to_string();
    resp.reasoning_content = Some("reasoning summary".to_string());
    resp.stop_reason = Some("stop".to_string());
    resp.usage = Usage {
        prompt_tokens: 10,
        completion_tokens: 20,
        ..Usage::default()
    };

    let out = AnthropicResponseFormatter.format_response(&resp);
    let content = out
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content should be array");
    assert_eq!(
        content[0].get("type").and_then(|v| v.as_str()),
        Some("thinking")
    );
    assert_eq!(
        content[0].get("thinking").and_then(|v| v.as_str()),
        Some("reasoning summary")
    );
}

#[test]
fn anthropic_encoder_replays_reasoning_extra_as_thinking_block() {
    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "reasoning_content".to_string(),
        serde_json::Value::String("I should run a shell command.".to_string()),
    );

    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("".to_string()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_1".to_string(),
                kind: ToolCallKind::Function,
                name: "exec_command".to_string(),
                arguments: "{\"cmd\":\"echo hello\"}".to_string(),
            }]),
            tool_call_id: None,
            meta: Some(serde_json::Value::Object(extra.into_iter().collect())),
        },
        // The tool call needs its matching result: cross-protocol conversions
        // drop unpaired tool_use blocks (Anthropic rejects them with a 400).
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("hello".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("deepseek-v4-flash", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let blocks = body["messages"][0]["content"]
        .as_array()
        .expect("assistant content blocks");

    assert_eq!(blocks[0]["type"].as_str(), Some("thinking"));
    assert_eq!(
        blocks[0]["thinking"].as_str(),
        Some("I should run a shell command.")
    );
    assert_eq!(blocks[1]["type"].as_str(), Some("tool_use"));
}

#[test]
fn openai_to_responses_reasoning_and_function_call_items() {
    let mut resp = IrAiResponse::new("resp_1", "minimax-m2.7");
    resp.content = "done".to_string();
    resp.reasoning_content = Some("chain".to_string());
    resp.tool_calls = vec![ToolCall {
        namespace: None,
        id: "call_123".to_string(),
        kind: ToolCallKind::Function,
        name: "ls".to_string(),
        arguments: "{\"path\":\".\"}".to_string(),
    }];
    resp.stop_reason = Some("stop".to_string());

    let out = ResponsesResponseFormatter.format_response(&resp);
    let output = out
        .get("output")
        .and_then(|v| v.as_array())
        .expect("output should be array");
    assert!(
        output
            .iter()
            .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("reasoning"))
    );
    assert!(
        output
            .iter()
            .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("function_call"))
    );
    assert!(
        output
            .iter()
            .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("message"))
    );
}

#[test]
fn openai_formatter_sets_tool_calls_finish_reason_when_tool_calls_present() {
    let mut resp = IrAiResponse::new("gen_1", "gemini-2.5-flash");
    resp.tool_calls = vec![ToolCall {
        namespace: None,
        id: "call_1".to_string(),
        kind: ToolCallKind::Function,
        name: "bash".to_string(),
        arguments: "{\"command\":\"ls\"}".to_string(),
    }];
    resp.stop_reason = Some("stop".to_string());
    resp.usage = Usage {
        prompt_tokens: 44,
        completion_tokens: 13,
        ..Usage::default()
    };

    let out = nyro_core::protocol::codec::openai::compatible::stream::OpenAIResponseFormatter
        .format_response(&resp);
    let finish_reason = out
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str());
    assert_eq!(finish_reason, Some("tool_calls"));
}

#[test]
fn openai_stream_formatter_sets_tool_calls_finish_reason_when_tool_calls_seen() {
    let mut fmt = OpenAIStreamFormatter::new();
    let ai_deltas = vec![
        IrStreamDelta::MessageStart {
            id: "gen_1".to_string(),
            model: "gemini-2.5-flash".to_string(),
        },
        IrStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".to_string(),
            namespace: None,
            kind: ToolCallKind::Function,
            name: "bash".to_string(),
        },
        IrStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"command\":\"ls\"}".to_string(),
        },
        IrStreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ];
    let events = fmt.format_deltas(&ai_deltas);
    let last_json = events
        .iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .next_back()
        .expect("has final json");
    let finish_reason = last_json
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str());
    assert_eq!(finish_reason, Some("tool_calls"));
}

#[test]
fn gemini_tool_result_correlation_success() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_abc".to_string(),
                kind: ToolCallKind::Function,
                name: "read_file".to_string(),
                arguments: "{\"path\":\"src/main.rs\"}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "read_file".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
    ];
    let mut ai_req = AiRequest::new("minimax-m2.7", messages);
    ai_req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    ai_req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    normalize_request_tool_results(&mut ai_req);
    assert_eq!(
        ai_req.messages[1].tool_call_id.as_deref(),
        Some("call_abc"),
        "tool result should be correlated to previous assistant tool_call id"
    );
}

#[test]
fn gemini_tool_result_id_hint_matches_out_of_order_calls() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![
                ToolCall {
                    namespace: None,
                    id: "call_a".to_string(),
                    kind: ToolCallKind::Function,
                    name: "Glob".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    namespace: None,
                    id: "call_b".to_string(),
                    kind: ToolCallKind::Function,
                    name: "Bash".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "call_b".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "call_a".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
    ];
    let mut ai_req = AiRequest::new("minimax-m2.7", messages);
    ai_req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    ai_req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    normalize_request_tool_results(&mut ai_req);
    assert_eq!(ai_req.messages[1].tool_call_id.as_deref(), Some("call_b"));
    assert_eq!(ai_req.messages[2].tool_call_id.as_deref(), Some("call_a"));
}

#[test]
fn minimax_reasoning_split_fallback_think_tag() {
    let mut ai_resp = IrAiResponse::new("resp_2", "minimax-m2.7");
    ai_resp.content = "<think>plan first</think>run ls".to_string();
    ai_resp.stop_reason = Some("stop".to_string());

    normalize_response_reasoning(&mut ai_resp);
    assert_eq!(ai_resp.reasoning_content.as_deref(), Some("plan first"));
    assert_eq!(ai_resp.content, "run ls");
}

#[test]
fn non_reasoning_model_no_regression() {
    let mut ai_resp = IrAiResponse::new("resp_3", "plain-model");
    ai_resp.content = "hello world".to_string();
    ai_resp.stop_reason = Some("stop".to_string());

    normalize_response_reasoning(&mut ai_resp);
    assert!(ai_resp.reasoning_content.is_none());
    assert_eq!(ai_resp.content, "hello world");
}

#[test]
fn anthropic_tool_result_decodes_to_tool_role() {
    let body = serde_json::json!({
        "model": "claude-sonnet",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "call_abc",
                        "name": "read_file",
                        "input": {"path": "Cargo.toml"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "call_abc",
                        "content": {"ok": true}
                    }
                ]
            }
        ]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[1].role, IrRole::Tool);
    assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_abc"));
}

#[test]
fn anthropic_multi_tool_result_decodes_to_multiple_tool_messages() {
    let body = serde_json::json!({
        "model": "claude-sonnet",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "call_a", "name": "read_file", "input": {"path":"a"} },
                    { "type": "tool_use", "id": "call_b", "name": "read_file", "input": {"path":"b"} }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "call_a", "content": {"ok": true} },
                    { "type": "tool_result", "tool_use_id": "call_b", "content": {"ok": true} }
                ]
            }
        ]
    });
    let req = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[1].role, IrRole::Tool);
    assert_eq!(req.messages[2].role, IrRole::Tool);
    assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_a"));
    assert_eq!(req.messages[2].tool_call_id.as_deref(), Some("call_b"));
}

#[test]
fn anthropic_thinking_block_round_trips_with_signature() {
    let body = serde_json::json!({
        "model": "claude-sonnet",
        "max_tokens": 1024,
        "messages": [{
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "review prior tool output",
                    "signature": "sig_123"
                },
                {
                    "type": "text",
                    "text": "Ready."
                }
            ]
        }]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");
    let IrMessageContent::Blocks(blocks) = &req.messages[0].content else {
        panic!("thinking must remain a structured block");
    };
    assert!(matches!(
        &blocks[0],
        IrContentBlock::Thinking { thinking, signature }
            if thinking == "review prior tool output" && signature.as_deref() == Some("sig_123")
    ));

    let (encoded, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic request");
    let block = encoded
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|messages| messages.first())
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
        .and_then(|content| content.first())
        .expect("first content block");
    assert_eq!(block.get("type").and_then(|v| v.as_str()), Some("thinking"));
    assert_eq!(
        block.get("thinking").and_then(|v| v.as_str()),
        Some("review prior tool output")
    );
    assert_eq!(
        block.get("signature").and_then(|v| v.as_str()),
        Some("sig_123")
    );
}

#[test]
fn openai_encoder_injects_synthetic_tool_call_before_orphan_tool_result() {
    let messages = vec![Message {
        role: IrRole::Tool,
        content: IrMessageContent::Text("{\"ok\":true}".to_string()),
        tool_calls: None,
        tool_call_id: Some("call_orphan_1".to_string()),
        meta: None,
    }];
    let mut req = AiRequest::new("minimax-m2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        messages[1].get("role").and_then(|v| v.as_str()),
        Some("tool")
    );
    assert_eq!(
        messages[1].get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_orphan_1")
    );
}

#[test]
fn openai_encoder_injects_adjacent_tool_call_for_non_adjacent_match() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("will call".to_string()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_x".to_string(),
                kind: ToolCallKind::Function,
                name: "ls".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::User,
            content: IrMessageContent::Text("intermediate".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_x".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("minimax-m2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[2].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        messages[3].get("role").and_then(|v| v.as_str()),
        Some("tool")
    );
    let tool_id = messages[3]
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(!tool_id.is_empty());
    let assistant_call_id = messages[2]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(assistant_call_id, tool_id);
}

#[test]
fn openai_encoder_drops_intermediate_assistant_text_before_tool_result() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("plan".to_string()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_keep".to_string(),
                kind: ToolCallKind::Function,
                name: "exec_command".to_string(),
                arguments: "{\"command\":\"ls -la\"}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("extra text".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"stdout\":\"...\"}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_keep".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // intermediate assistant text should be dropped to keep tool_result adjacent
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        messages[1]
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|tc| tc.get("id"))
            .and_then(|v| v.as_str()),
        Some("call_keep")
    );
    assert_eq!(
        messages[2].get("role").and_then(|v| v.as_str()),
        Some("tool")
    );
    assert_eq!(
        messages[2].get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_keep")
    );
}

#[test]
fn openai_encoder_remaps_duplicate_tool_call_ids() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_dup".to_string(),
                kind: ToolCallKind::Function,
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_dup".to_string(),
                kind: ToolCallKind::Function,
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_dup".to_string()),
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_dup".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    let ids: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            m.get("tool_calls")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
        })
        .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);

    let tool_ids: Vec<String> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        .filter_map(|m| {
            m.get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    assert_eq!(tool_ids.len(), 2);
    assert!(ids.contains(&tool_ids[0]));
    assert!(ids.contains(&tool_ids[1]));
}

#[test]
fn anthropic_encoder_maps_required_tool_choice_to_any() {
    let messages = vec![Message {
        role: IrRole::User,
        content: IrMessageContent::Text("hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "exec_command".to_string(),
        kind: ToolSpecKind::Function,
        description: Some("Execute command".to_string()),
        parameters: serde_json::json!({"type":"object","properties":{"command":{"type":"string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.tools = tools;
    req.tool_choice = Some(nyro_core::protocol::ir::ToolChoice::Raw(serde_json::json!(
        "required"
    )));
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    assert_eq!(
        body.get("tool_choice")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("any")
    );
}

#[test]
fn anthropic_encoder_maps_function_tool_choice_to_tool_name() {
    let messages = vec![Message {
        role: IrRole::User,
        content: IrMessageContent::Text("hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "exec_command".to_string(),
        kind: ToolSpecKind::Function,
        description: Some("Execute command".to_string()),
        parameters: serde_json::json!({"type":"object","properties":{"command":{"type":"string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.tools = tools;
    req.tool_choice = Some(nyro_core::protocol::ir::ToolChoice::Raw(
        serde_json::json!({
            "type":"function",
            "function":{"name":"exec_command"}
        }),
    ));
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    assert_eq!(
        body.get("tool_choice")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("tool")
    );
    assert_eq!(
        body.get("tool_choice")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
        Some("exec_command")
    );
}

#[test]
fn anthropic_encoder_merges_consecutive_roles_and_drops_empty_text() {
    let messages = vec![
        Message {
            role: IrRole::User,
            content: IrMessageContent::Text("first".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::User,
            content: IrMessageContent::Text("second".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("tool".to_string()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_1".to_string(),
                kind: ToolCallKind::Function,
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("result".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].get("role").and_then(|v| v.as_str()), Some("user"));
    assert_eq!(
        msgs[1].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[2].get("role").and_then(|v| v.as_str()), Some("user"));

    let first_blocks = msgs[0]
        .get("content")
        .and_then(|v| v.as_array())
        .expect("first content blocks");
    assert_eq!(first_blocks.len(), 2);
    assert_eq!(
        first_blocks[0].get("text").and_then(|v| v.as_str()),
        Some("first")
    );
    assert_eq!(
        first_blocks[1].get("text").and_then(|v| v.as_str()),
        Some("second")
    );
}

#[test]
fn anthropic_encoder_normalizes_tool_use_ids_for_tool_and_result() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_function_abc_1".to_string(),
                kind: ToolCallKind::Function,
                name: "glob".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "call_function_abc_1".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_function_abc_1".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "glob".to_string(),
        kind: ToolSpecKind::Function,
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.tools = tools;
    req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    let tool_use_id = msgs[0]
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|b| b.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_result_id = msgs[1]
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|b| b.get("tool_use_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(tool_use_id.starts_with("toolu_"));
    assert_eq!(tool_use_id, tool_result_id);
}

#[test]
fn responses_decoder_ignores_empty_message_content_item() {
    let body = serde_json::json!({
        "model": "MiniMax-M2.7-Code-Claude",
        "input": [
            { "type": "message", "role": "user", "content": [] },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "帮我查看当前目录下有哪些文件" }]
            }
        ]
    });

    let req = ResponsesDecoder
        .decode_request(body)
        .expect("decode request should succeed");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, IrRole::User);
    assert_eq!(
        req.messages[0].content.to_text(),
        "帮我查看当前目录下有哪些文件"
    );
}

fn transcode_responses_to_openai_compatible(body: serde_json::Value) -> serde_json::Value {
    let mut request = ResponsesDecoder
        .decode_request(body)
        .expect("decode Responses request");
    let route_plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    route_plan.prepare_upstream_request(&mut request);
    OpenAIEncoder
        .encode_request(&request)
        .expect("encode OpenAI-compatible request")
        .0
}

fn encoded_openai_tool_name(tool: &serde_json::Value) -> Option<&str> {
    tool.pointer("/function/name")
        .or_else(|| tool.pointer("/custom/name"))
        .or_else(|| tool.get("name"))
        .and_then(|value| value.as_str())
}

#[test]
fn responses_top_level_function_tool_survives_openai_compatible_transcode() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "wait for the task"}]
        }],
        "tools": [{
            "type": "function",
            "name": "wait",
            "description": "Wait for a running task",
            "parameters": {
                "type": "object",
                "properties": {"task_id": {"type": "string"}},
                "required": ["task_id"],
                "additionalProperties": false
            },
            "strict": false
        }],
        "tool_choice": "auto"
    });

    let encoded = transcode_responses_to_openai_compatible(body);
    let tools = encoded["tools"].as_array().expect("tools array");

    assert_eq!(tools.len(), 1);
    assert_eq!(encoded_openai_tool_name(&tools[0]), Some("wait"));
}

// Regression reproduced from Codex Desktop requests: dynamically available
// tools are carried by an `additional_tools` input item instead of top-level
// `tools`. The Responses -> OpenAI-compatible transcode must not drop them.
#[test]
fn responses_additional_function_tool_survives_openai_compatible_transcode() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [{
                    "type": "function",
                    "name": "wait",
                    "description": "Wait for a running task",
                    "parameters": {
                        "type": "object",
                        "properties": {"task_id": {"type": "string"}},
                        "required": ["task_id"],
                        "additionalProperties": false
                    },
                    "strict": false
                }]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "wait for the task"}]
            }
        ],
        "tool_choice": "auto"
    });

    let encoded = transcode_responses_to_openai_compatible(body);
    let tools = encoded["tools"]
        .as_array()
        .expect("additional function tool must reach the upstream request");

    assert_eq!(tools.len(), 1);
    assert_eq!(encoded_openai_tool_name(&tools[0]), Some("wait"));
}

#[test]
fn responses_additional_mixed_tools_survive_openai_compatible_transcode() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [
                    {
                        "type": "custom",
                        "name": "exec",
                        "description": "Run JavaScript code",
                        "format": {
                            "type": "grammar",
                            "syntax": "lark",
                            "definition": "start: source\nsource: /.+/"
                        }
                    },
                    {
                        "type": "function",
                        "name": "wait",
                        "description": "Wait for a running task",
                        "parameters": {
                            "type": "object",
                            "properties": {"task_id": {"type": "string"}},
                            "required": ["task_id"],
                            "additionalProperties": false
                        },
                        "strict": false
                    },
                    {
                        "type": "function",
                        "name": "request_user_input",
                        "description": "Request a decision from the user",
                        "parameters": {
                            "type": "object",
                            "properties": {"question": {"type": "string"}},
                            "required": ["question"],
                            "additionalProperties": false
                        },
                        "strict": false
                    }
                ]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "inspect the project"}]
            }
        ],
        "parallel_tool_calls": false,
        "tool_choice": "auto"
    });

    let encoded = transcode_responses_to_openai_compatible(body);
    let names: Vec<&str> = encoded["tools"]
        .as_array()
        .expect("all additional tools must reach the upstream request")
        .iter()
        .filter_map(encoded_openai_tool_name)
        .collect();

    assert_eq!(names, ["exec", "wait", "request_user_input"]);
}

// Codex Desktop groups dynamically available tools inside a namespace. The
// Responses -> OpenAI-compatible transcode must flatten the namespace instead
// of silently dropping all of its child tools.
#[test]
fn responses_namespaced_additional_tools_survive_openai_compatible_transcode() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [{
                    "type": "namespace",
                    "name": "functions",
                    "description": "",
                    "tools": [
                        {
                            "type": "custom",
                            "name": "exec",
                            "description": "Run JavaScript code",
                            "format": {
                                "type": "grammar",
                                "syntax": "lark",
                                "definition": "start: source\nsource: /.+/"
                            }
                        },
                        {
                            "type": "function",
                            "name": "wait",
                            "description": "Wait for a running task",
                            "parameters": {
                                "type": "object",
                                "properties": {"task_id": {"type": "string"}},
                                "required": ["task_id"],
                                "additionalProperties": false
                            },
                            "strict": false
                        },
                        {
                            "type": "function",
                            "name": "request_user_input",
                            "description": "Request a decision from the user",
                            "parameters": {
                                "type": "object",
                                "properties": {"question": {"type": "string"}},
                                "required": ["question"],
                                "additionalProperties": false
                            },
                            "strict": false
                        }
                    ]
                }]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "inspect the project"}]
            }
        ],
        "parallel_tool_calls": false,
        "tool_choice": "auto"
    });

    let encoded = transcode_responses_to_openai_compatible(body);
    let tools = encoded["tools"]
        .as_array()
        .expect("namespaced additional tools must reach the upstream request");
    let names: Vec<&str> = tools.iter().filter_map(encoded_openai_tool_name).collect();

    assert_eq!(names, ["exec", "wait", "request_user_input"]);
    assert!(tools.iter().all(|tool| tool["type"] == "function"));

    let exec = tools
        .iter()
        .find(|tool| tool["function"]["name"] == "exec")
        .expect("bridged exec tool");
    assert_eq!(exec["function"]["description"], "Run JavaScript code");
    assert_eq!(
        exec["function"]["parameters"],
        serde_json::json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"],
            "additionalProperties": false
        })
    );

    let wait = tools
        .iter()
        .find(|tool| tool["function"]["name"] == "wait")
        .expect("flattened wait tool");
    assert_eq!(
        wait["function"]["parameters"]["properties"]["task_id"]["type"],
        "string"
    );
    assert!(tools.iter().all(|tool| {
        tool.get("namespace").is_none() && tool["function"].get("namespace").is_none()
    }));
}

// Mirrors the namespace and deferred-function example in the official OpenAI
// function-calling and hosted tool-search guides. Chat Completions has no
// namespace/tool-search surface, so every declared child must be made eagerly
// available as a flat function tool.
#[test]
fn responses_official_namespace_eagerly_flattens_to_openai_compatible_tools() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "input": "List open orders for customer CUST-12345.",
        "tools": [
            {
                "type": "namespace",
                "name": "crm",
                "description": "CRM tools for customer lookup and order management.",
                "tools": [
                    {
                        "type": "function",
                        "name": "get_customer_profile",
                        "description": "Fetch a customer profile by customer ID.",
                        "parameters": {
                            "type": "object",
                            "properties": {"customer_id": {"type": "string"}},
                            "required": ["customer_id"],
                            "additionalProperties": false
                        }
                    },
                    {
                        "type": "function",
                        "name": "list_open_orders",
                        "description": "List open orders for a customer ID.",
                        "defer_loading": true,
                        "parameters": {
                            "type": "object",
                            "properties": {"customer_id": {"type": "string"}},
                            "required": ["customer_id"],
                            "additionalProperties": false
                        }
                    }
                ]
            },
            {"type": "tool_search"}
        ],
        "parallel_tool_calls": false
    });

    let encoded = transcode_responses_to_openai_compatible(body);
    let tools = encoded["tools"]
        .as_array()
        .expect("namespace children must become Chat Completions tools");
    let names: Vec<&str> = tools.iter().filter_map(encoded_openai_tool_name).collect();

    assert_eq!(names, ["get_customer_profile", "list_open_orders"]);
    for tool in tools {
        let function = &tool["function"];
        assert_eq!(tool["type"], "function");
        assert!(
            function["description"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
        );
        assert_eq!(
            function["parameters"]["properties"]["customer_id"]["type"],
            "string"
        );
        assert!(
            function.get("defer_loading").is_none(),
            "Responses-only defer_loading must not leak to Chat Completions"
        );
    }
}

#[test]
fn responses_namespace_requires_name_and_tools() {
    let invalid_namespaces = [
        (
            "missing name",
            serde_json::json!({
                "type": "namespace",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object", "properties": {}}
                }]
            }),
        ),
        (
            "missing tools",
            serde_json::json!({
                "type": "namespace",
                "name": "crm"
            }),
        ),
    ];

    for (case, namespace) in invalid_namespaces {
        let body = serde_json::json!({
            "model": "gpt-5.6",
            "input": "Look up the customer.",
            "tools": [namespace]
        });
        let error = ResponsesDecoder.decode_request(body).expect_err(case);
        assert!(
            error.to_string().contains("namespace"),
            "{case}: error should identify the invalid namespace, got {error:#}"
        );
    }
}

#[test]
fn responses_namespace_survives_request_decode_encode_round_trip() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_history",
                "name": "lookup_customer",
                "namespace": "crm",
                "arguments": "{\"customer_id\":\"CUST-12345\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_history",
                "output": "found"
            },
            {
                "type": "message",
                "role": "user",
                "content": "Look it up again."
            }
        ],
        "tools": [{
            "type": "namespace",
            "name": "crm",
            "description": "CRM tools.",
            "tools": [{
                "type": "function",
                "name": "lookup_customer",
                "description": "Look up a CRM customer.",
                "parameters": {
                    "type": "object",
                    "properties": {"customer_id": {"type": "string"}},
                    "required": ["customer_id"],
                    "additionalProperties": false
                }
            }]
        }]
    });

    let request = ResponsesDecoder
        .decode_request(body)
        .expect("decode namespace");
    let (encoded, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("re-encode namespace");
    let namespace = encoded["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["type"] == "namespace"))
        .expect("namespace tool container");
    assert_eq!(namespace["name"], "crm");
    assert_eq!(namespace["tools"][0]["name"], "lookup_customer");

    let history_call = encoded["input"]
        .as_array()
        .and_then(|input| input.iter().find(|item| item["type"] == "function_call"))
        .expect("history function call");
    assert_eq!(history_call["name"], "lookup_customer");
    assert_eq!(history_call["namespace"], "crm");
}

#[test]
fn responses_identical_top_level_and_additional_tools_are_deduplicated() {
    let tool = serde_json::json!({
        "type": "function",
        "name": "wait",
        "description": "Wait for a running task",
        "parameters": {
            "type": "object",
            "properties": {"task_id": {"type": "string"}},
            "required": ["task_id"],
            "additionalProperties": false
        },
        "strict": false
    });
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "tools": [tool.clone()],
        "input": [
            {"type": "additional_tools", "role": "developer", "tools": [tool]},
            {"type": "message", "role": "user", "content": "wait for the task"}
        ]
    });

    let request = ResponsesDecoder
        .decode_request(body)
        .expect("identical tool definitions should merge");
    let tools = request.tools.expect("merged tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "wait");
}

#[test]
fn responses_namespaced_tools_merge_by_qualified_identity() {
    let leaf = |description: &str| {
        serde_json::json!({
            "type": "function",
            "name": "lookup_customer",
            "description": description,
            "parameters": {"type": "object", "properties": {}}
        })
    };
    let crm = serde_json::json!({
        "type": "namespace",
        "name": "crm",
        "tools": [leaf("CRM lookup")]
    });
    let support = serde_json::json!({
        "type": "namespace",
        "name": "support",
        "tools": [leaf("Support lookup")]
    });
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "tools": [crm.clone(), support],
        "input": [
            {"type": "additional_tools", "role": "developer", "tools": [crm]},
            {"type": "message", "role": "user", "content": "lookup"}
        ]
    });

    let request = ResponsesDecoder
        .decode_request(body)
        .expect("qualified tool identities should merge");
    let tools = request.tools.expect("namespaced tools");
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().any(|tool| {
        tool.namespace.as_deref() == Some("crm") && tool.name == "lookup_customer"
    }));
    assert!(tools.iter().any(|tool| {
        tool.namespace.as_deref() == Some("support") && tool.name == "lookup_customer"
    }));
}

#[test]
fn responses_conflicting_top_level_and_additional_tools_are_rejected() {
    let body = serde_json::json!({
        "model": "gpt-5.6",
        "tools": [{
            "type": "function",
            "name": "wait",
            "parameters": {"type": "object", "properties": {"task_id": {"type": "string"}}}
        }],
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [{
                    "type": "function",
                    "name": "wait",
                    "parameters": {"type": "object", "properties": {"seconds": {"type": "number"}}}
                }]
            },
            {"type": "message", "role": "user", "content": "wait for the task"}
        ]
    });

    let error = ResponsesDecoder
        .decode_request(body)
        .expect_err("conflicting definitions must not be silently overwritten");
    assert!(
        error
            .to_string()
            .contains("conflicting tool definitions for 'wait'"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn openai_encoder_remaps_reused_tool_result_id_with_synthetic_adjacent_call() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_same".to_string(),
                kind: ToolCallKind::Function,
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("ok1".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_same".to_string()),
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("intermediate".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("ok2".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_same".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "exec_command".to_string(),
        kind: ToolSpecKind::Function,
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("gpt-4o-mini", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let (body, _) = OpenAIEncoder.encode_request(&req).expect("encode");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    let mut tool_ids: Vec<String> = Vec::new();
    for msg in msgs {
        if msg.get("role").and_then(|v| v.as_str()) == Some("tool") {
            let id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            assert!(!id.is_empty());
            tool_ids.push(id);
        }
    }
    assert_eq!(tool_ids.len(), 2);
    assert_ne!(tool_ids[0], tool_ids[1]);
}

#[test]
fn openai_encoder_rewrites_multi_tool_call_history_to_adjacent_pairs() {
    let messages = vec![
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("".to_string()),
            tool_calls: Some(vec![
                ToolCall {
                    namespace: None,
                    id: "call_a".to_string(),
                    kind: ToolCallKind::Function,
                    name: "Glob".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    namespace: None,
                    id: "call_b".to_string(),
                    kind: ToolCallKind::Function,
                    name: "Bash".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("r1".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_a".to_string()),
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("r2".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_b".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "Glob".to_string(),
        kind: ToolSpecKind::Function,
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(ANTHROPIC_MESSAGES_2023_06_01);

    let (body, _) = OpenAIEncoder.encode_request(&req).expect("encode");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    assert_eq!(msgs.len(), 4);
    assert_eq!(
        msgs[0].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[1].get("role").and_then(|v| v.as_str()), Some("tool"));
    assert_eq!(
        msgs[2].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[3].get("role").and_then(|v| v.as_str()), Some("tool"));
    let id1 = msgs[1]
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let id2 = msgs[3]
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let prev1 = msgs[0]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let prev2 = msgs[2]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(id1, prev1);
    assert_eq!(id2, prev2);
}

#[test]
fn openai_encoder_preserves_reasoning_content_across_parallel_tool_calls() {
    // Regression: when an assistant message has multiple parallel tool calls
    // AND extra fields (e.g. reasoning_content from DeepSeek thinking mode),
    // each synthetic assistant message created by normalize_messages_for_openai
    // must carry forward the extra fields. std::mem::take() only works for the
    // first extraction — subsequent extractions get HashMap::new(), dropping
    // reasoning_content and causing HTTP 400 from DeepSeek.
    use std::collections::HashMap;
    let mut extra = HashMap::new();
    extra.insert(
        "reasoning_content".to_string(),
        serde_json::Value::String("I need to check the time in Tokyo and Paris.".to_string()),
    );

    let messages = vec![
        Message {
            role: IrRole::User,
            content: IrMessageContent::Text("What time is it in Tokyo and Paris?".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        // Single assistant message with TWO parallel tool calls + reasoning_content
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("".to_string()),
            tool_calls: Some(vec![
                ToolCall {
                    namespace: None,
                    id: "call_tokyo".to_string(),
                    kind: ToolCallKind::Function,
                    name: "get_time".to_string(),
                    arguments: "{\"location\":\"Tokyo\"}".to_string(),
                },
                ToolCall {
                    namespace: None,
                    id: "call_paris".to_string(),
                    kind: ToolCallKind::Function,
                    name: "get_time".to_string(),
                    arguments: "{\"location\":\"Paris\"}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: Some(serde_json::Value::Object(extra.into_iter().collect())),
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("10:30 JST".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_tokyo".to_string()),
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("03:30 CEST".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_paris".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "get_time".to_string(),
        kind: ToolSpecKind::Function,
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{"location":{"type":"string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("deepseek-v4-flash", messages);
    req.stream = StreamConfig {
        enabled: true,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // We expect: [user, assistant(call_tokyo, reasoning_content), tool(call_tokyo),
    //             assistant(call_paris, reasoning_content), tool(call_paris)]
    // The original assistant with both calls gets pruned (empty content, no calls left).
    assert_eq!(
        msgs.len(),
        5,
        "expected 5 messages: user + 2 assistant+tool pairs"
    );

    // Every assistant message must carry reasoning_content
    for (i, msg) in msgs.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "assistant" {
            let rc = msg.get("reasoning_content").and_then(|v| v.as_str());
            assert!(
                rc.is_some(),
                "assistant message at index {} is missing reasoning_content. \
                 Bug: std::mem::take() on source.extra drops it after first extraction. \
                 Full msg: {:?}",
                i,
                msg
            );
            assert_eq!(
                rc,
                Some("I need to check the time in Tokyo and Paris."),
                "assistant[{}] has wrong reasoning_content value",
                i
            );
        }
    }
}

#[test]
fn anthropic_to_openai_thinking_round_trip_carries_reasoning_content() {
    // Regression for cross-protocol Anthropic Messages → OpenAI chat/completions:
    // when the client (Claude Code) re-sends an assistant turn containing
    // `thinking` + parallel `tool_use` blocks followed by `tool_result`s,
    // upstreams in thinking mode (Xiaomi Mimo / DeepSeek / etc.) require the
    // assistant message that carries `tool_calls` to also carry the original
    // `reasoning_content`. Otherwise they return:
    //   400 "The reasoning_content in the thinking mode must be passed back."
    //
    // The thinking text must be bridged through `meta.reasoning_content` so the
    // OpenAI encoder emits it on every split assistant message produced by
    // `normalize_messages_for_openai`.
    let raw = serde_json::json!({
        "model": "mimo-v2.5-pro",
        "max_tokens": 1024,
        "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "ls the project"}]},
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": "The user wants me to list the project.",
                        "signature": ""
                    },
                    {
                        "type": "tool_use",
                        "id": "call_a",
                        "name": "Bash",
                        "input": {"command": "ls -la"}
                    },
                    {
                        "type": "tool_use",
                        "id": "call_b",
                        "name": "Bash",
                        "input": {"command": "find . -maxdepth 2"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_a", "content": "out-a"},
                    {"type": "tool_result", "tool_use_id": "call_b", "content": "out-b"}
                ]
            }
        ]
    });

    let ir = AnthropicDecoder
        .decode_request(raw)
        .expect("decode anthropic request");

    let asst_idx = ir
        .messages
        .iter()
        .position(|m| m.role == IrRole::Assistant)
        .expect("assistant message present");
    let asst_meta = ir.messages[asst_idx]
        .meta
        .as_ref()
        .and_then(|v| v.get("reasoning_content"))
        .and_then(|v| v.as_str());
    assert_eq!(
        asst_meta,
        Some("The user wants me to list the project."),
        "anthropic decoder must surface thinking text as meta.reasoning_content; \
         got meta={:?}",
        ir.messages[asst_idx].meta,
    );

    let (body, _) = OpenAIEncoder
        .encode_request(&ir)
        .expect("encode openai body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    let assistant_msgs: Vec<&serde_json::Value> = msgs
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .collect();
    assert!(
        !assistant_msgs.is_empty(),
        "expected at least one assistant message in encoded body, got: {:?}",
        msgs
    );

    for (i, m) in assistant_msgs.iter().enumerate() {
        let rc = m.get("reasoning_content").and_then(|v| v.as_str());
        assert_eq!(
            rc,
            Some("The user wants me to list the project."),
            "assistant[{}] missing or wrong reasoning_content: {:?}",
            i,
            m
        );

        // Thinking block must NOT also leak into content as plain text — that
        // would duplicate reasoning across two channels.
        if let Some(arr) = m.get("content").and_then(|v| v.as_array()) {
            for part in arr {
                let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                assert!(
                    !text.contains("The user wants me to list the project."),
                    "thinking text leaked into content array: {:?}",
                    m
                );
            }
        }
    }
}

#[test]
fn openai_encoder_drops_orphan_assistant_tool_calls_without_results() {
    let messages = vec![
        Message {
            role: IrRole::System,
            content: IrMessageContent::Text("sys".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![
                ToolCall {
                    namespace: None,
                    id: "call_old_1".to_string(),
                    kind: ToolCallKind::Function,
                    name: String::new(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    namespace: None,
                    id: "call_old_2".to_string(),
                    kind: ToolCallKind::Function,
                    name: "list_directory".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                namespace: None,
                id: "call_new".to_string(),
                kind: ToolCallKind::Function,
                name: "glob".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        Message {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_new".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "glob".to_string(),
        kind: ToolSpecKind::Function,
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    let (body, _) = OpenAIEncoder.encode_request(&req).expect("encode");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].get("role").and_then(|v| v.as_str()), Some("system"));
    assert_eq!(
        msgs[1].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[2].get("role").and_then(|v| v.as_str()), Some("tool"));
    let call_id = msgs[1]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(call_id, "call_new");
}

#[test]
fn gemini_stream_formatter_keeps_tool_name_for_argument_deltas() {
    let mut fmt = GoogleStreamFormatter::new();
    let deltas = vec![
        IrStreamDelta::MessageStart {
            id: "x".to_string(),
            model: "m".to_string(),
        },
        IrStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".to_string(),
            name: "run_shell_command".to_string(),
            namespace: None,
            kind: ToolCallKind::Function,
        },
        IrStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"command\":\"ls -la\"}".to_string(),
        },
    ];
    let events = fmt.format_deltas(&deltas);
    let mut saw_named_call = false;
    let mut saw_command_arg = false;
    for ev in events {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&ev.data) else {
            continue;
        };
        let part = v
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|arr| arr.first())
            .and_then(|p| p.get("functionCall"));
        if let Some(fc) = part {
            if fc.get("name").and_then(|n| n.as_str()) == Some("run_shell_command") {
                saw_named_call = true;
            }
            if fc
                .get("args")
                .and_then(|a| a.get("command"))
                .and_then(|c| c.as_str())
                == Some("ls -la")
            {
                saw_command_arg = true;
            }
        }
    }
    assert!(saw_named_call);
    assert!(saw_command_arg);
}

#[test]
fn gemini_stream_formatter_normalizes_common_tool_argument_aliases() {
    let mut fmt = GoogleStreamFormatter::new();
    let deltas = vec![
        IrStreamDelta::MessageStart {
            id: "x".to_string(),
            model: "m".to_string(),
        },
        IrStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".to_string(),
            name: "glob".to_string(),
            namespace: None,
            kind: ToolCallKind::Function,
        },
        IrStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"include_pattern\":\"**/*.py\",\"search_root\":\"/tmp/work\",\"exclude_pattern\":\"**/.venv/**\"}".to_string(),
        },
    ];
    let events = fmt.format_deltas(&deltas);
    let payload = events
        .iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .find_map(|v| {
            v.get("candidates")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
                .and_then(|arr| arr.first())
                .and_then(|p| p.get("functionCall"))
                .cloned()
        })
        .expect("functionCall payload");

    assert_eq!(payload.get("name").and_then(|v| v.as_str()), Some("glob"));
    let args = payload.get("args").expect("args object");
    assert_eq!(
        args.get("pattern").and_then(|v| v.as_str()),
        Some("**/*.py")
    );
    assert_eq!(
        args.get("root_dir").and_then(|v| v.as_str()),
        Some("/tmp/work")
    );
    assert_eq!(
        args.get("exclude_patterns")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str()),
        Some("**/.venv/**")
    );
}

#[test]
fn gemini_encoder_sanitizes_unsupported_json_schema_fields() {
    let messages = vec![Message {
        role: IrRole::User,
        content: IrMessageContent::Text("hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let tools = Some(vec![ToolSpec {
        namespace: None,
        name: "glob".to_string(),
        kind: ToolSpecKind::Function,
        description: Some("glob files".to_string()),
        parameters: serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string"},
                "items": {
                    "type": "array",
                    "items": {
                        "$ref": "#/$defs/entry",
                        "ref": "legacy"
                    }
                }
            },
            "$defs": {
                "entry": {"type":"string"}
            }
        }),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("gemini-2.5-flash", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let (body, _) = GoogleEncoder.encode_request(&req).expect("encode");
    let params = body
        .get("tools")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("functionDeclarations"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("parameters"))
        .cloned()
        .expect("parameters");

    let rendered = params.to_string();
    assert!(!rendered.contains("$schema"));
    assert!(!rendered.contains("additionalProperties"));
    assert!(!rendered.contains("$ref"));
    assert!(!rendered.contains("\"ref\""));
    assert!(!rendered.contains("$defs"));
}

fn responses_request(messages: Vec<Message>, stream: bool) -> AiRequest {
    let mut req = AiRequest::new("gpt-5.4", messages);
    req.stream = StreamConfig {
        enabled: stream,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);
    req
}

#[test]
fn responses_encoder_targets_slash_v1_responses_and_forces_stream() {
    let req = responses_request(
        vec![Message {
            role: IrRole::User,
            content: IrMessageContent::Text("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
        false,
    );

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    assert_eq!(
        body.get("stream").and_then(|v| v.as_bool()),
        Some(true),
        "responses backends only accept stream:true"
    );
    assert_eq!(
        body.get("store").and_then(|v| v.as_bool()),
        Some(false),
        "gateway never persists server-side state"
    );
    assert_eq!(
        ResponsesEncoder.egress_path("gpt-5.4", false),
        "/v1/responses"
    );
}

#[test]
fn responses_encoder_splits_system_to_instructions_and_user_to_input_text() {
    let req = responses_request(
        vec![
            Message {
                role: IrRole::System,
                content: IrMessageContent::Text("be terse".to_string()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            Message {
                role: IrRole::User,
                content: IrMessageContent::Text("hi".to_string()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
        false,
    );

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    assert_eq!(
        body.get("instructions").and_then(|v| v.as_str()),
        Some("be terse")
    );
    let input = body.get("input").and_then(|v| v.as_array()).expect("input");
    assert_eq!(input.len(), 1);
    assert_eq!(
        input[0].get("type").and_then(|v| v.as_str()),
        Some("message")
    );
    assert_eq!(input[0].get("role").and_then(|v| v.as_str()), Some("user"));
    let first_block = input[0]
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .expect("first content block");
    assert_eq!(
        first_block.get("type").and_then(|v| v.as_str()),
        Some("input_text")
    );
    assert_eq!(first_block.get("text").and_then(|v| v.as_str()), Some("hi"));
}

#[test]
fn responses_encoder_emits_function_call_and_function_call_output_items() {
    let req = responses_request(
        vec![
            Message {
                role: IrRole::Assistant,
                content: IrMessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall {
                    namespace: None,
                    id: "call_abc".to_string(),
                    kind: ToolCallKind::Function,
                    name: "list_dir".to_string(),
                    arguments: "{\"path\":\".\"}".to_string(),
                }]),
                tool_call_id: None,
                meta: None,
            },
            Message {
                role: IrRole::Tool,
                content: IrMessageContent::Text("file1\nfile2".to_string()),
                tool_calls: None,
                tool_call_id: Some("call_abc".to_string()),
                meta: None,
            },
        ],
        false,
    );

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    let input = body.get("input").and_then(|v| v.as_array()).expect("input");
    assert_eq!(
        input.len(),
        2,
        "one function_call + one function_call_output"
    );

    assert_eq!(
        input[0].get("type").and_then(|v| v.as_str()),
        Some("function_call")
    );
    assert_eq!(
        input[0].get("call_id").and_then(|v| v.as_str()),
        Some("call_abc")
    );
    assert_eq!(
        input[0].get("name").and_then(|v| v.as_str()),
        Some("list_dir")
    );
    assert_eq!(
        input[0].get("arguments").and_then(|v| v.as_str()),
        Some("{\"path\":\".\"}"),
    );

    assert_eq!(
        input[1].get("type").and_then(|v| v.as_str()),
        Some("function_call_output")
    );
    assert_eq!(
        input[1].get("call_id").and_then(|v| v.as_str()),
        Some("call_abc")
    );
    assert_eq!(
        input[1].get("output").and_then(|v| v.as_str()),
        Some("file1\nfile2")
    );
}

#[test]
fn responses_encoder_drops_max_output_tokens_for_codex_compat() {
    let mut req = responses_request(
        vec![Message {
            role: IrRole::User,
            content: IrMessageContent::Text("hi".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
        false,
    );
    req.generation.max_tokens = Some(128);

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    assert!(
        body.get("max_output_tokens").is_none(),
        "codex backend rejects max_output_tokens; callers needing a cap must use extra"
    );
}

#[test]
fn responses_encoder_drops_chat_include_usage_stream_options() {
    let ir = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "gpt-5.6-sol",
            "stream": true,
            "stream_options": {"include_usage": true},
            "reasoning_effort": "medium",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .expect("decode chat request");

    let (body, _) = ResponsesEncoder
        .encode_request(&ir)
        .expect("encode responses request");
    assert!(
        body.get("stream_options").is_none(),
        "codex responses rejects Chat Completions stream_options.include_usage"
    );
}

#[test]
fn responses_encoder_keeps_native_stream_options_without_include_usage() {
    let ir = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt-5.6-sol",
            "stream": true,
            "stream_options": {
                "include_usage": true,
                "include_obfuscation": true
            },
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        }))
        .expect("decode responses request");

    let (body, _) = ResponsesEncoder
        .encode_request(&ir)
        .expect("encode responses request");
    assert_eq!(
        body.get("stream_options"),
        Some(&serde_json::json!({"include_obfuscation": true})),
        "native Responses stream_options must round-trip minus include_usage"
    );
}

#[test]
fn responses_stream_parser_extracts_text_and_usage() {
    let sse = "event: response.created\n\
data: {\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.4\"}}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Hel\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"lo\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\
\n";

    let mut parser = ResponsesStreamParser::new();
    let deltas = parser.parse_chunk(sse).expect("parse");

    let mut saw_start = false;
    let mut text_concat = String::new();
    let mut usage_input = 0;
    let mut usage_output = 0;
    let mut done_reason: Option<String> = None;

    for delta in &deltas {
        match delta {
            IrStreamDelta::MessageStart { id, model } => {
                saw_start = true;
                assert_eq!(id, "resp_1");
                assert_eq!(model, "gpt-5.4");
            }
            IrStreamDelta::TextDelta(t) => text_concat.push_str(t),
            IrStreamDelta::Usage(u) => {
                usage_input = u.prompt_tokens;
                usage_output = u.completion_tokens;
            }
            IrStreamDelta::Done { stop_reason } => done_reason = Some(stop_reason.clone()),
            _ => {}
        }
    }

    assert!(saw_start);
    assert_eq!(text_concat, "Hello");
    assert_eq!(usage_input, 7);
    assert_eq!(usage_output, 2);
    assert_eq!(done_reason.as_deref(), Some("stop"));
}

#[test]
fn responses_stream_parser_extracts_function_call() {
    let sse = "event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_xyz\",\"name\":\"ls\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"output_index\":0,\"delta\":\"{\\\"a\\\":1\"}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"output_index\":0,\"delta\":\"}\"}\n\
\n";

    let mut parser = ResponsesStreamParser::new();
    let deltas = parser.parse_chunk(sse).expect("parse");

    let mut got_start = false;
    let mut arg_concat = String::new();
    for delta in &deltas {
        match delta {
            IrStreamDelta::ToolCallStart { id, name, .. } => {
                got_start = true;
                assert_eq!(id, "call_xyz");
                assert_eq!(name, "ls");
            }
            IrStreamDelta::ToolCallDelta { arguments, .. } => arg_concat.push_str(arguments),
            _ => {}
        }
    }
    assert!(got_start);
    assert_eq!(arg_concat, "{\"a\":1}");
}

#[test]
fn responses_response_parser_extracts_text_tool_calls_and_usage() {
    let body = serde_json::json!({
        "id": "resp_42",
        "model": "gpt-5.4",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "content": [
                    {"type": "output_text", "text": "Hi "},
                    {"type": "output_text", "text": "there"}
                ]
            },
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "search",
                "arguments": "{\"q\":\"rust\"}"
            }
        ],
        "usage": {"input_tokens": 11, "output_tokens": 3}
    });

    let resp = ResponsesResponseParser.parse_response(body).expect("parse");

    assert_eq!(resp.id, "resp_42");
    assert_eq!(resp.model, "gpt-5.4");
    assert_eq!(resp.content, "Hi there");
    assert_eq!(resp.stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(resp.usage.prompt_tokens, 11);
    assert_eq!(resp.usage.completion_tokens, 3);
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "call_1");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(resp.tool_calls[0].arguments, "{\"q\":\"rust\"}");
}

// The official hosted tool-search response carries the leaf function name and
// its namespace as separate fields. A native Responses parse/format cycle must
// preserve both fields even though the generic tool-call IR is involved.
#[test]
fn responses_function_call_namespace_survives_non_stream_round_trip() {
    let body = serde_json::json!({
        "id": "resp_namespace",
        "object": "response",
        "status": "completed",
        "model": "gpt-5.6",
        "output": [{
            "type": "function_call",
            "id": "fc_namespace",
            "call_id": "call_namespace",
            "name": "list_open_orders",
            "namespace": "crm",
            "arguments": "{\"customer_id\":\"CUST-12345\"}",
            "status": "completed"
        }],
        "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14}
    });

    let response = ResponsesResponseParser
        .parse_response(body)
        .expect("parse namespaced Responses function call");
    let formatted = ResponsesResponseFormatter.format_response(&response);
    let call = formatted["output"]
        .as_array()
        .expect("Responses output array")
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("formatted function_call");

    assert_eq!(call["name"], "list_open_orders");
    assert_eq!(call["namespace"], "crm");
    assert_eq!(call["call_id"], "call_namespace");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(call["arguments"].as_str().unwrap()).unwrap(),
        serde_json::json!({"customer_id": "CUST-12345"})
    );
}

#[test]
fn responses_function_call_namespace_survives_stream_round_trip() {
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_namespace\",\"model\":\"gpt-5.6\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_namespace\",\"call_id\":\"call_namespace\",\"name\":\"list_open_orders\",\"namespace\":\"crm\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_namespace\",\"output_index\":0,\"delta\":\"{\\\"customer_id\\\":\\\"CUST-12345\\\"}\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_namespace\",\"call_id\":\"call_namespace\",\"name\":\"list_open_orders\",\"namespace\":\"crm\",\"arguments\":\"{\\\"customer_id\\\":\\\"CUST-12345\\\"}\",\"status\":\"completed\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_namespace\",\"model\":\"gpt-5.6\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"total_tokens\":14}}}\n\n"
    );

    let mut parser = ResponsesStreamParser::new();
    let deltas = parser
        .parse_chunk(sse)
        .expect("parse namespaced Responses stream");
    let mut formatter = ResponsesStreamFormatter::new();
    let mut events = formatter.format_deltas(&deltas);
    events.extend(formatter.format_done());

    let payloads: Vec<serde_json::Value> = events
        .iter()
        .filter_map(|event| serde_json::from_str(&event.data).ok())
        .collect();
    let call_items: Vec<&serde_json::Value> = payloads
        .iter()
        .filter_map(|payload| payload.get("item"))
        .filter(|item| item["type"] == "function_call")
        .collect();

    assert!(
        !call_items.is_empty(),
        "formatter emitted no function_call items"
    );
    assert!(
        call_items
            .iter()
            .all(|item| item["name"] == "list_open_orders" && item["namespace"] == "crm"),
        "every streamed function_call item must preserve its namespace"
    );

    let completed_call = payloads
        .iter()
        .find(|payload| payload["type"] == "response.completed")
        .and_then(|payload| payload["response"]["output"].as_array())
        .and_then(|output| output.iter().find(|item| item["type"] == "function_call"))
        .expect("response.completed function_call");
    assert_eq!(completed_call["name"], "list_open_orders");
    assert_eq!(completed_call["namespace"], "crm");
}

#[test]
fn codex_parallel_calls_with_intermediate_text_anthropic_egress() {
    let body = serde_json::json!({
        "model": "deepseek-v4-flash",
        "input": [
            {"type": "message", "role": "user",
                "content": [{"type":"input_text","text":"do parallel work"}]},
            {"type": "function_call", "call_id": "call_00_A",
                "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call", "call_id": "call_00_B",
                "name": "exec_command", "arguments": "{\"cmd\":\"pwd\"}"},
            {"type": "message", "role": "assistant",
                "content": [{"type":"output_text","text":"running both"}]},
            {"type": "function_call_output", "call_id": "call_00_A",
                "output": "{\"stdout\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_00_B",
                "output": "{\"stdout\":\"b\"}"},
        ]
    });
    let mut req: AiRequest = ResponsesDecoder.decode_request(body).expect("decode");
    normalize_request_tool_results(&mut req);

    let (encoded, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let msgs = encoded
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    for (i, m) in msgs.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let blocks = m
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let tool_use_ids: Vec<String> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        if tool_use_ids.is_empty() {
            continue;
        }

        assert_eq!(
            blocks
                .last()
                .and_then(|b| b.get("type"))
                .and_then(|v| v.as_str()),
            Some("tool_use"),
            "assistant message {i} must end with tool_use; got blocks={blocks:?}",
        );

        let next = msgs.get(i + 1).expect("must have next user msg");
        assert_eq!(next.get("role").and_then(|v| v.as_str()), Some("user"));
        let next_blocks = next
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let result_ids: Vec<String> = next_blocks
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
            .filter_map(|b| {
                b.get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();
        for id in &tool_use_ids {
            assert!(
                result_ids.contains(id),
                "tool_use {id} has no matching tool_result in next user message; got {next_blocks:?}",
            );
        }
    }
}

#[test]
fn gemini_file_data_round_trip_preserves_uri_and_mime_type() {
    use nyro_core::protocol::codec::google::gemini::decoder::GoogleDecoder;

    // Simulate an inbound request with a PDF fileData part.
    let inbound = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "fileData": {
                    "fileUri": "https://example.com/doc.pdf",
                    "mimeType": "application/pdf"
                }
            }]
        }]
    });

    // Decode to IR, then re-encode.
    let mut req = GoogleDecoder.decode_request(inbound).expect("decode");
    req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    let (outbound, _) = GoogleEncoder.encode_request(&req).expect("encode");

    let parts = outbound["contents"][0]["parts"].as_array().expect("parts");
    let fd = &parts[0]["fileData"];
    assert_eq!(
        fd["fileUri"].as_str(),
        Some("https://example.com/doc.pdf"),
        "fileUri must survive round-trip"
    );
    assert_eq!(
        fd["mimeType"].as_str(),
        Some("application/pdf"),
        "mimeType must survive round-trip"
    );
}

#[test]
fn gemini_decoder_file_data_routes_image_to_image_block() {
    use nyro_core::protocol::codec::google::gemini::decoder::GoogleDecoder;

    let body = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "fileData": {
                    "fileUri": "https://example.com/photo.jpg",
                    "mimeType": "image/jpeg"
                }
            }]
        }]
    });

    let decoder = GoogleDecoder;
    let req = decoder.decode_request(body).expect("decode");

    let msg = &req.messages[0];
    match &msg.content {
        IrMessageContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 1);
            match &blocks[0] {
                IrContentBlock::Image { source, .. } => match source {
                    MediaSource::Url(url) => {
                        assert_eq!(url, "https://example.com/photo.jpg");
                    }
                    _ => panic!("expected MediaSource::Url"),
                },
                other => panic!("expected ContentBlock::Image for image/ mimeType, got {other:?}"),
            }
        }
        other => panic!("expected Blocks, got {other:?}"),
    }
}

#[test]
fn gemini_encoder_file_data_without_mime_type_omits_mime_type() {
    let messages = vec![Message {
        role: IrRole::User,
        content: IrMessageContent::Blocks(vec![IrContentBlock::File {
            source: MediaSource::Url("https://example.com/unknown.bin".into()),
            media_type: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let req = AiRequest::new("gemini-2.5-flash", messages);

    let (body, _) = GoogleEncoder.encode_request(&req).expect("encode");

    let parts = body["contents"][0]["parts"]
        .as_array()
        .expect("parts array");
    let fd = &parts[0]["fileData"];
    assert_eq!(
        fd["fileUri"].as_str(),
        Some("https://example.com/unknown.bin")
    );
    assert!(
        fd.get("mimeType").is_none(),
        "mimeType must be absent when media_type is None"
    );
}

// ── Claude Code >=2.1.154 mid-conversation system messages ────────────────────

/// Basic: inline system role is decoded as Role::System and kept at its position.
#[test]
fn anthropic_inline_system_role_decodes_without_error() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 32000,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "帮我查下当前目录结构"}
                ]
            },
            {
                "role": "system",
                "content": "SessionStart hook additional context: you have superpowers."
            },
            {
                "role": "user",
                "content": "follow up question"
            }
        ]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("should not fail on inline system role");

    // Inline system decoded to Role::System at original position (index 1).
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[0].role, IrRole::User);
    assert_eq!(req.messages[1].role, IrRole::System);
    assert_eq!(req.messages[2].role, IrRole::User);

    // System content is preserved.
    let sys_text = req.messages[1].content.to_text();
    assert!(
        sys_text.contains("superpowers"),
        "system content must be preserved, got: {sys_text}"
    );
}

/// Inline system with content blocks (cache_control present, mirroring the real log).
#[test]
fn anthropic_inline_system_role_with_blocks_decodes() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 32000,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "first question"},
                    {"type": "text", "text": "cached part", "cache_control": {"type": "ephemeral"}}
                ]
            },
            {
                "role": "system",
                "content": [
                    {"type": "text", "text": "injected system context from skill"}
                ]
            },
            {
                "role": "assistant",
                "content": "sure"
            }
        ]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("should handle inline system with surrounding cache_control blocks");

    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[1].role, IrRole::System);
    let sys_text = req.messages[1].content.to_text();
    assert!(sys_text.contains("injected system context"));
}

/// Anthropic encoder re-merges inline system into top-level system field,
/// keeping the messages array clean for strict downstream endpoints.
#[test]
fn anthropic_inline_system_role_encodes_into_top_level_system() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "system": "base system prompt",
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "system", "content": "mid-conversation system injection"},
            {"role": "assistant", "content": "hi there"},
            {"role": "user", "content": "next turn"}
        ]
    });

    let ir = AnthropicDecoder.decode_request(body).expect("decode");

    let (encoded, _) = AnthropicEncoder.encode_request(&ir).expect("encode");

    // Top-level system should contain both base and injected text.
    let system_val = encoded.get("system").expect("system field must exist");
    let system_str = system_val.as_str().expect("system must be string");
    assert!(
        system_str.contains("base system prompt"),
        "base system missing"
    );
    assert!(
        system_str.contains("mid-conversation system injection"),
        "injected system missing"
    );

    // messages must not contain any system role (strict endpoint safe).
    let msgs = encoded["messages"].as_array().expect("messages array");
    for m in msgs {
        assert_ne!(
            m["role"].as_str(),
            Some("system"),
            "re-encoded messages must not contain system role"
        );
    }
    // user and assistant turns are preserved.
    assert_eq!(msgs.len(), 3, "user + assistant + user");
}

/// Unknown roles (not system/user/assistant) still produce a hard error.
#[test]
fn anthropic_truly_unknown_role_still_errors() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "garbage_role", "content": "unexpected"}
        ]
    });

    let result = AnthropicDecoder.decode_request(body);
    assert!(
        result.is_err(),
        "truly unknown role must still be rejected with an error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unknown Anthropic role: garbage_role"),
        "error message must identify the bad role, got: {err}"
    );
}

#[test]
fn gemini_encoder_file_data_with_mime_type_emits_mime_type() {
    let messages = vec![Message {
        role: IrRole::User,
        content: IrMessageContent::Blocks(vec![IrContentBlock::File {
            source: MediaSource::Url("https://example.com/report.pdf".into()),
            media_type: Some("application/pdf".into()),
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let req = AiRequest::new("gemini-2.5-flash", messages);

    let (body, _) = GoogleEncoder.encode_request(&req).expect("encode");

    let parts = body["contents"][0]["parts"]
        .as_array()
        .expect("parts array");
    assert_eq!(parts.len(), 1);
    let fd = &parts[0]["fileData"];
    assert_eq!(
        fd["fileUri"].as_str(),
        Some("https://example.com/report.pdf")
    );
    assert_eq!(fd["mimeType"].as_str(), Some("application/pdf"));
}

#[test]
fn anthropic_to_openai_strips_tool_use_from_content_array() {
    // Regression for Anthropic Messages → OpenAI Chat Completions cross-protocol
    // conversion: the Anthropic decoder carries an assistant `tool_use` BOTH in
    // `content` blocks AND in `tool_calls`. The OpenAI encoder must NOT emit the
    // ToolUse block into the `content` array (OpenAI only accepts text/image/...
    // part types there) — otherwise strict upstreams reject with:
    //   400 "messages[N]: unknown variant `function`, expected `text`".
    // The tool call must instead live solely in the `tool_calls` array.
    let raw = serde_json::json!({
        "model": "deepseek-v4-flash",
        "max_tokens": 1024,
        "tools": [{
            "name": "Bash",
            "description": "run a shell command",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}}
            }
        }],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "list the project files"}]},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Sure, listing files."},
                    {"type": "tool_use", "id": "call_a", "name": "Bash", "input": {"command": "ls -la"}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_a", "content": "file1\nfile2"}
                ]
            }
        ]
    });

    let ir = AnthropicDecoder
        .decode_request(raw)
        .expect("decode anthropic request");

    let (body, _) = OpenAIEncoder
        .encode_request(&ir)
        .expect("encode openai body");

    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // (a) No assistant content part may carry type:"function" (the bug).
    for (i, m) in msgs.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(arr) = m.get("content").and_then(|v| v.as_array()) {
            for part in arr {
                let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                assert_ne!(
                    ty, "function",
                    "assistant[{i}] content leaked a `function` part into content array: {part:?}"
                );
            }
        }
    }

    // (b) The tool call survives intact in tool_calls (id / name / arguments).
    let call = msgs
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(|v| v.as_array()))
        .flatten()
        .find(|tc| tc.get("id").and_then(|v| v.as_str()) == Some("call_a"))
        .expect("tool_call call_a must be preserved in tool_calls");
    assert_eq!(call.get("type").and_then(|v| v.as_str()), Some("function"));
    assert_eq!(
        call.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("Bash")
    );
    let args = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        args.contains("ls -la"),
        "tool call arguments must survive, got: {args}"
    );

    // (c) tool_result message is correlated back to the same id.
    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        .expect("tool message present");
    assert_eq!(
        tool_msg.get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_a")
    );

    // (d) tool definitions survive.
    let tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    assert_eq!(
        tools[0].get("type").and_then(|v| v.as_str()),
        Some("function")
    );
    assert_eq!(
        tools[0]
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("Bash")
    );
}

// Regression: Codex Desktop sometimes emits a `function_call` input item
// whose `name` is empty but whose `arguments` carry the full call payload
// (paired with a separate item that has the real name). The decoder must
// tolerate the empty-name item rather than bail with 400.
#[test]
fn responses_decoder_tolerates_empty_name_function_call_item() {
    let body = serde_json::json!({
        "model": "gpt-5.4",
        "input": [
            {"type": "message", "role": "user",
                "content": [{"type":"input_text","text":"run a command"}]},
            // well-formed call (real name, empty args placeholder)
            {"type": "function_call", "call_id": "call_w3HMgoP4RtEnGpxdTe2eQVEZ",
                "name": "exec_command", "arguments": ""},
            // malformed duplicate: full args, empty name, fresh hex call_id
            {"type": "function_call", "call_id": "call_aec52c641b094ce0aae3ce3cc526068c",
                "name": "",
                "arguments": "{\"cmd\":\"git status --short\"}"},
            {"type": "function_call_output", "call_id": "call_w3HMgoP4RtEnGpxdTe2eQVEZ",
                "output": "On branch master"},
            // orphaned output for the skipped empty-name call; normalize step
            // will synthesize a matching assistant tool_call.
            {"type": "function_call_output", "call_id": "call_aec52c641b094ce0aae3ce3cc526068c",
                "output": "unsupported call: "}
        ]
    });

    let mut req: AiRequest = ResponsesDecoder
        .decode_request(body)
        .expect("decoder must tolerate empty-name function_call item");

    // Exactly one assistant tool_call should survive (the well-formed one).
    let assistant_calls: Vec<&ToolCall> = req
        .messages
        .iter()
        .filter(|m| m.role == IrRole::Assistant)
        .flat_map(|m| m.tool_calls.iter().flatten())
        .collect();
    assert_eq!(
        assistant_calls.len(),
        1,
        "empty-name function_call item must be skipped, not duplicated"
    );
    assert_eq!(assistant_calls[0].id, "call_w3HMgoP4RtEnGpxdTe2eQVEZ");
    assert_eq!(assistant_calls[0].name, "exec_command");

    // normalize_request_tool_results must reconcile the orphaned output
    // without panicking.
    normalize_request_tool_results(&mut req);

    // Every tool result must end up linked to a non-empty tool_call_id.
    let orphan_tool_msgs: Vec<_> = req
        .messages
        .iter()
        .filter(|m| m.role == IrRole::Tool)
        .filter(|m| {
            m.tool_call_id
                .as_ref()
                .map(|id| id.trim().is_empty())
                .unwrap_or(true)
        })
        .collect();
    assert!(
        orphan_tool_msgs.is_empty(),
        "all tool results should be correlated after normalize"
    );
}

#[test]
fn anthropic_adaptive_effort_is_forwarded_to_openai_responses() {
    let body = serde_json::json!({
        "model": "gpt-5.6-sol",
        "max_tokens": 32000,
        "messages": [{"role": "user", "content": "hello"}],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"},
        "stream": true
    });

    let ir = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");

    assert!(ir.reasoning.enabled);
    assert_eq!(ir.reasoning.effort, Some(ReasoningEffort::High));

    let (responses_body, _) = ResponsesEncoder
        .encode_request(&ir)
        .expect("encode responses request");
    assert_eq!(responses_body["reasoning"]["effort"], "high");

    let (anthropic_body, _) = AnthropicEncoder
        .encode_request(&ir)
        .expect("re-encode anthropic request");
    assert_eq!(anthropic_body["thinking"]["type"], "adaptive");
    assert_eq!(anthropic_body["output_config"]["effort"], "high");
}

fn simple_reasoning_request(effort: ReasoningEffort) -> AiRequest {
    let mut req = AiRequest::new(
        "reasoning-model",
        vec![Message {
            role: IrRole::User,
            content: IrMessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    req.generation.max_tokens = Some(10_000);
    req.reasoning = ReasoningConfig {
        enabled: effort != ReasoningEffort::None,
        effort: Some(effort),
        ..Default::default()
    };
    req
}

fn decode_chat_effort(effort: &str) -> AiRequest {
    OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "reasoning-model",
            "messages": [{"role": "user", "content": "hello"}],
            "max_completion_tokens": 10000,
            "reasoning_effort": effort
        }))
        .expect("decode chat reasoning effort")
}

fn decode_responses_effort(effort: &str) -> AiRequest {
    ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "reasoning-model",
            "input": "hello",
            "max_output_tokens": 10000,
            "reasoning": {"effort": effort}
        }))
        .expect("decode responses reasoning effort")
}

fn decode_anthropic_effort(effort: &str) -> AiRequest {
    AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "reasoning-model",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 10000,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": effort}
        }))
        .expect("decode anthropic reasoning effort")
}

fn decode_google_effort(effort: &str) -> AiRequest {
    GoogleDecoder
        .decode_with_model(
            serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                "generationConfig": {
                    "maxOutputTokens": 10000,
                    "thinkingConfig": {"thinkingLevel": effort}
                }
            }),
            "gemini-reasoning-model",
            false,
        )
        .expect("decode google reasoning effort")
}

#[test]
fn reasoning_decoders_preserve_every_supported_effort_level() {
    let openai_levels = [
        ("none", ReasoningEffort::None),
        ("minimal", ReasoningEffort::Minimal),
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("xhigh", ReasoningEffort::Xhigh),
        ("max", ReasoningEffort::Max),
    ];
    for (name, expected) in openai_levels {
        let chat = decode_chat_effort(name);
        let responses = decode_responses_effort(name);
        assert_eq!(chat.reasoning.effort.as_ref(), Some(&expected));
        assert_eq!(responses.reasoning.effort.as_ref(), Some(&expected));
    }

    let anthropic_levels = [
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("xhigh", ReasoningEffort::Xhigh),
        ("max", ReasoningEffort::Max),
    ];
    for (name, expected) in anthropic_levels {
        let request = decode_anthropic_effort(name);
        assert_eq!(request.reasoning.effort, Some(expected));
    }

    let google_levels = [
        ("MINIMAL", ReasoningEffort::Minimal),
        ("LOW", ReasoningEffort::Low),
        ("MEDIUM", ReasoningEffort::Medium),
        ("HIGH", ReasoningEffort::High),
    ];
    for (name, expected) in google_levels {
        let request = decode_google_effort(name);
        assert_eq!(request.reasoning.effort, Some(expected));
    }
}

#[test]
fn reasoning_encoders_emit_target_specific_effort_levels() {
    let levels = [
        (ReasoningEffort::None, "none", None, None, Some(0)),
        (
            ReasoningEffort::Minimal,
            "minimal",
            Some("low"),
            Some("low"),
            None,
        ),
        (ReasoningEffort::Low, "low", Some("low"), Some("low"), None),
        (
            ReasoningEffort::Medium,
            "medium",
            Some("medium"),
            Some("medium"),
            None,
        ),
        (
            ReasoningEffort::High,
            "high",
            Some("high"),
            Some("high"),
            None,
        ),
        (
            ReasoningEffort::Xhigh,
            "xhigh",
            Some("xhigh"),
            Some("high"),
            None,
        ),
        (ReasoningEffort::Max, "max", Some("max"), Some("high"), None),
    ];

    for (effort, openai, anthropic, google_level, google_budget) in levels {
        let request = simple_reasoning_request(effort);
        let (chat, _) = OpenAIEncoder.encode_request(&request).expect("encode chat");
        let (responses, _) = ResponsesEncoder
            .encode_request(&request)
            .expect("encode responses");
        let (anthropic_body, _) = AnthropicEncoder
            .encode_request(&request)
            .expect("encode anthropic");
        let (google, _) = GoogleEncoder
            .encode_request(&request)
            .expect("encode google");

        assert_eq!(chat["reasoning_effort"], openai);
        assert_eq!(responses["reasoning"]["effort"], openai);
        match anthropic {
            Some(expected) => {
                assert_eq!(anthropic_body["thinking"]["type"], "adaptive");
                assert_eq!(anthropic_body["output_config"]["effort"], expected);
            }
            None => {
                assert_eq!(anthropic_body["thinking"]["type"], "disabled");
                assert!(anthropic_body.get("output_config").is_none());
            }
        }
        if let Some(expected) = google_level {
            assert_eq!(
                google["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                expected
            );
        }
        if let Some(expected) = google_budget {
            assert_eq!(
                google["generationConfig"]["thinkingConfig"]["thinkingBudget"],
                expected
            );
        }
    }
}

fn assert_high_effort_in_every_egress(request: &AiRequest) {
    let (chat, _) = OpenAIEncoder.encode_request(request).expect("encode chat");
    assert_eq!(chat["reasoning_effort"], "high");
    assert!(chat.get("reasoning").is_none());

    let (responses, _) = ResponsesEncoder
        .encode_request(request)
        .expect("encode responses");
    assert_eq!(responses["reasoning"]["effort"], "high");
    assert!(responses.get("reasoning_effort").is_none());

    let (anthropic, _) = AnthropicEncoder
        .encode_request(request)
        .expect("encode anthropic");
    assert_eq!(anthropic["thinking"]["type"], "adaptive");
    assert_eq!(anthropic["output_config"]["effort"], "high");

    let (google, _) = GoogleEncoder
        .encode_request(request)
        .expect("encode google");
    // The Google egress keeps the effort either via the IR reasoning config
    // (lowercase, API-spec) or the raw generation-config passthrough (verbatim
    // from the upstream request), so compare case-insensitively.
    let google_level = google["generationConfig"]["thinkingConfig"]["thinkingLevel"]
        .as_str()
        .unwrap_or_else(|| panic!("missing google thinkingLevel: {google}"));
    assert!(
        google_level.eq_ignore_ascii_case("high"),
        "high effort must survive the Google egress, got {google_level:?}"
    );
}

#[test]
fn high_effort_survives_every_protocol_pair() {
    assert_high_effort_in_every_egress(&decode_chat_effort("high"));
    assert_high_effort_in_every_egress(&decode_responses_effort("high"));
    assert_high_effort_in_every_egress(&decode_anthropic_effort("high"));
    assert_high_effort_in_every_egress(&decode_google_effort("HIGH"));
}

#[test]
fn native_reasoning_fields_take_priority_over_normalized_ir() {
    let mut chat = decode_chat_effort("high");
    chat.reasoning.effort = Some(ReasoningEffort::Low);
    assert_eq!(
        OpenAIEncoder.encode_request(&chat).unwrap().0["reasoning_effort"],
        "high"
    );

    let mut responses = decode_responses_effort("high");
    responses.reasoning.effort = Some(ReasoningEffort::Low);
    assert_eq!(
        ResponsesEncoder.encode_request(&responses).unwrap().0["reasoning"]["effort"],
        "high"
    );

    let mut anthropic = decode_anthropic_effort("high");
    anthropic.reasoning.effort = Some(ReasoningEffort::Low);
    assert_eq!(
        AnthropicEncoder.encode_request(&anthropic).unwrap().0["output_config"]["effort"],
        "high"
    );

    let mut google = decode_google_effort("HIGH");
    google.reasoning.effort = Some(ReasoningEffort::Low);
    assert_eq!(
        GoogleEncoder.encode_request(&google).unwrap().0["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
}

#[test]
fn token_budgets_are_preserved_or_mapped_without_being_dropped() {
    let anthropic = AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "reasoning-model",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 10000,
            "thinking": {"type": "enabled", "budget_tokens": 8000}
        }))
        .expect("decode anthropic budget");
    assert_eq!(anthropic.reasoning.budget_tokens, Some(8000));
    assert_eq!(
        OpenAIEncoder.encode_request(&anthropic).unwrap().0["reasoning_effort"],
        "high"
    );
    assert_eq!(
        ResponsesEncoder.encode_request(&anthropic).unwrap().0["reasoning"]["effort"],
        "high"
    );
    assert_eq!(
        GoogleEncoder.encode_request(&anthropic).unwrap().0["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        8000
    );

    let google = GoogleDecoder
        .decode_with_model(
            serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                "generationConfig": {
                    "maxOutputTokens": 10000,
                    "thinkingConfig": {"thinkingBudget": 2000}
                }
            }),
            "gemini-2.5-flash",
            false,
        )
        .expect("decode google budget");
    assert_eq!(google.reasoning.budget_tokens, Some(2000));
    assert_eq!(
        AnthropicEncoder.encode_request(&google).unwrap().0["thinking"]["budget_tokens"],
        2000
    );
    assert_eq!(
        OpenAIEncoder.encode_request(&google).unwrap().0["reasoning_effort"],
        "low"
    );
}

#[test]
fn gemini_disabled_and_dynamic_budgets_map_to_explicit_intent() {
    let decode_budget = |budget| {
        GoogleDecoder
            .decode_with_model(
                serde_json::json!({
                    "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                    "generationConfig": {
                        "maxOutputTokens": 10000,
                        "thinkingConfig": {"thinkingBudget": budget}
                    }
                }),
                "gemini-2.5-flash",
                false,
            )
            .expect("decode google budget")
    };

    let disabled = decode_budget(0);
    assert!(!disabled.reasoning.enabled);
    assert_eq!(disabled.reasoning.effort, Some(ReasoningEffort::None));
    assert_eq!(
        ResponsesEncoder.encode_request(&disabled).unwrap().0["reasoning"]["effort"],
        "none"
    );
    assert_eq!(
        AnthropicEncoder.encode_request(&disabled).unwrap().0["thinking"]["type"],
        "disabled"
    );

    let dynamic = decode_budget(-1);
    assert!(dynamic.reasoning.enabled);
    assert_eq!(dynamic.reasoning.budget_tokens, None);
    assert_eq!(dynamic.reasoning.effort, None);
    assert_eq!(
        ResponsesEncoder.encode_request(&dynamic).unwrap().0["reasoning"]["effort"],
        "medium"
    );
    assert_eq!(
        AnthropicEncoder.encode_request(&dynamic).unwrap().0["thinking"]["type"],
        "adaptive"
    );
}
