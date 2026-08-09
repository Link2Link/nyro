//! Cross-protocol request and response matrix for the three tool-capable HTTP APIs.
//! A route is named client protocol -> upstream protocol; responses travel back
//! through the inverse parser/formatter path under the same route name.

use std::collections::HashMap;

use nyro_core::protocol::codec::anthropic::messages::decoder::AnthropicDecoder;
use nyro_core::protocol::codec::anthropic::messages::encoder::AnthropicEncoder;
use nyro_core::protocol::codec::anthropic::messages::stream::{
    AnthropicResponseFormatter, AnthropicResponseParser, AnthropicStreamFormatter,
    AnthropicStreamParser,
};
use nyro_core::protocol::codec::openai::compatible::decoder::OpenAIDecoder;
use nyro_core::protocol::codec::openai::compatible::encoder::OpenAIEncoder;
use nyro_core::protocol::codec::openai::compatible::stream::{
    OpenAIResponseFormatter, OpenAIResponseParser, OpenAIStreamFormatter, OpenAIStreamParser,
};
use nyro_core::protocol::codec::openai::responses::decoder::ResponsesDecoder;
use nyro_core::protocol::codec::openai::responses::encoder::ResponsesEncoder;
use nyro_core::protocol::codec::openai::responses::formatter::ResponsesResponseFormatter;
use nyro_core::protocol::codec::openai::responses::parser::{
    ResponsesResponseParser, ResponsesStreamParser,
};
use nyro_core::protocol::codec::openai::responses::stream::ResponsesStreamFormatter;
use nyro_core::protocol::codec::tool_bridge::ToolRoutePlan;
use nyro_core::protocol::codec::tool_correlation::normalize_request_tool_results;
use nyro_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
    ProtocolEndpoint,
};
use nyro_core::protocol::ir::{
    AiRequest, AiResponse, AiStreamDelta, Role, ToolCallKind, ToolChoice, Usage,
};
use nyro_core::protocol::{
    RequestDecoder, RequestEncoder, ResponseDecoder, ResponseEncoder, SseEvent,
    StreamResponseDecoder, StreamResponseEncoder,
};
use serde_json::{Value, json};

const MODEL: &str = "matrix-model";
const SYSTEM_PROMPT: &str = "Answer with weather data.";
const USER_PROMPT: &str = "What is the weather in Paris?";
const TOOL_NAME: &str = "get_weather";
const TOOL_CALL_ID: &str = "call_1";
const TOOL_OUTPUT: &str = "21 C";
const RESPONSE_TEXT: &str = "Weather data is ready.";
const PROMPT_TOKENS: u32 = 12;
const COMPLETION_TOKENS: u32 = 7;
const CUSTOM_TOOL_NAME: &str = "exec";
const CUSTOM_TOOL_CALL_ID: &str = "call_exec";
const CUSTOM_TOOL_INPUT: &str = "const value = \"quoted\";\nreturn value;";
const FUNCTION_TOOL_NAME: &str = "wait";
const FUNCTION_TOOL_CALL_ID: &str = "call_wait";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestProtocol {
    AnthropicMessages,
    OpenAiCompatible,
    OpenAiResponses,
}

impl TestProtocol {
    fn name(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "Anthropic Messages",
            Self::OpenAiCompatible => "OpenAI Compatible",
            Self::OpenAiResponses => "OpenAI Responses",
        }
    }

    fn endpoint(self) -> ProtocolEndpoint {
        match self {
            Self::AnthropicMessages => ANTHROPIC_MESSAGES_2023_06_01,
            Self::OpenAiCompatible => OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Self::OpenAiResponses => OPENAI_RESPONSES_V1,
        }
    }
}

fn route_name(source: TestProtocol, target: TestProtocol) -> String {
    format!("{} -> {}", source.name(), target.name())
}

fn request_fixture(protocol: TestProtocol) -> Value {
    let parameters = json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
        "additionalProperties": false
    });

    match protocol {
        TestProtocol::AnthropicMessages => json!({
            "model": MODEL,
            "max_tokens": 128,
            "stream": false,
            "system": SYSTEM_PROMPT,
            "messages": [
                {"role": "user", "content": USER_PROMPT},
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": TOOL_CALL_ID,
                        "name": TOOL_NAME,
                        "input": {"city": "Paris"}
                    }]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": TOOL_CALL_ID,
                        "content": TOOL_OUTPUT
                    }]
                }
            ],
            "tools": [{
                "name": TOOL_NAME,
                "description": "Get weather by city",
                "input_schema": parameters
            }],
            "tool_choice": {"type": "auto"}
        }),
        TestProtocol::OpenAiCompatible => json!({
            "model": MODEL,
            "max_tokens": 128,
            "stream": false,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": USER_PROMPT},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": TOOL_CALL_ID,
                        "type": "function",
                        "function": {
                            "name": TOOL_NAME,
                            "arguments": "{\"city\":\"Paris\"}"
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": TOOL_CALL_ID, "content": TOOL_OUTPUT}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": TOOL_NAME,
                    "description": "Get weather by city",
                    "parameters": parameters,
                    "strict": false
                }
            }],
            "tool_choice": "auto"
        }),
        TestProtocol::OpenAiResponses => json!({
            "model": MODEL,
            "max_output_tokens": 128,
            "stream": false,
            "instructions": SYSTEM_PROMPT,
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": USER_PROMPT}]
                },
                {
                    "type": "function_call",
                    "call_id": TOOL_CALL_ID,
                    "name": TOOL_NAME,
                    "arguments": "{\"city\":\"Paris\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": TOOL_CALL_ID,
                    "output": TOOL_OUTPUT
                }
            ],
            "tools": [{
                "type": "function",
                "name": TOOL_NAME,
                "description": "Get weather by city",
                "parameters": parameters,
                "strict": false
            }],
            "tool_choice": "auto"
        }),
    }
}

fn decode_request(protocol: TestProtocol, body: Value) -> AiRequest {
    match protocol {
        TestProtocol::AnthropicMessages => AnthropicDecoder.decode_request(body),
        TestProtocol::OpenAiCompatible => OpenAIDecoder.decode_request(body),
        TestProtocol::OpenAiResponses => ResponsesDecoder.decode_request(body),
    }
    .unwrap_or_else(|error| panic!("failed to decode {} request: {error:#}", protocol.name()))
}

fn encode_request(protocol: TestProtocol, request: &AiRequest) -> Value {
    match protocol {
        TestProtocol::AnthropicMessages => AnthropicEncoder.encode_request(request),
        TestProtocol::OpenAiCompatible => OpenAIEncoder.encode_request(request),
        TestProtocol::OpenAiResponses => ResponsesEncoder.encode_request(request),
    }
    .unwrap_or_else(|error| panic!("failed to encode {} request: {error:#}", protocol.name()))
    .0
}

fn request_tool_call_id(protocol: TestProtocol) -> &'static str {
    match protocol {
        TestProtocol::AnthropicMessages => "toolu_call_1",
        TestProtocol::OpenAiCompatible | TestProtocol::OpenAiResponses => TOOL_CALL_ID,
    }
}

fn assert_request_semantics(request: &AiRequest, expected_tool_call_id: &str, context: &str) {
    assert_eq!(request.model, MODEL, "{context}: model");
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.role == Role::System
                && message.content.to_text() == SYSTEM_PROMPT),
        "{context}: system message was not preserved"
    );
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.role == Role::User && message.content.to_text() == USER_PROMPT),
        "{context}: user message was not preserved"
    );

    let tool_call = request
        .messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .find(|tool_call| tool_call.id == expected_tool_call_id)
        .unwrap_or_else(|| panic!("{context}: assistant tool call was not preserved"));
    assert_eq!(tool_call.name, TOOL_NAME, "{context}: tool call name");
    assert_eq!(
        serde_json::from_str::<Value>(&tool_call.arguments)
            .unwrap_or_else(|error| panic!("{context}: invalid tool arguments: {error}")),
        json!({"city": "Paris"}),
        "{context}: tool call arguments"
    );

    let tool_result = request
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap_or_else(|| panic!("{context}: tool result was not preserved"));
    assert_eq!(
        tool_result.tool_call_id.as_deref(),
        Some(expected_tool_call_id),
        "{context}: tool result correlation"
    );
    assert_eq!(
        tool_result.content.to_text(),
        TOOL_OUTPUT,
        "{context}: tool result content"
    );

    let tools = request
        .tools
        .as_ref()
        .unwrap_or_else(|| panic!("{context}: tool definition was not preserved"));
    let tool = tools
        .iter()
        .find(|tool| tool.name == TOOL_NAME)
        .unwrap_or_else(|| panic!("{context}: {TOOL_NAME} definition was not preserved"));
    assert_eq!(
        tool.description.as_deref(),
        Some("Get weather by city"),
        "{context}: tool description"
    );
    assert_eq!(
        tool.parameters.pointer("/properties/city/type"),
        Some(&Value::String("string".to_string())),
        "{context}: tool JSON schema"
    );
    assert!(
        matches!(request.tool_choice, Some(ToolChoice::Auto)),
        "{context}: automatic tool choice was not preserved"
    );
}

fn assert_request_wire(protocol: TestProtocol, body: &Value, context: &str) {
    match protocol {
        TestProtocol::AnthropicMessages => {
            assert_eq!(
                body["system"], SYSTEM_PROMPT,
                "{context}: system wire field"
            );
            assert_eq!(body["stream"], false, "{context}: stream wire field");
            assert_eq!(
                body["tools"][0]["name"], TOOL_NAME,
                "{context}: tool wire shape"
            );
            assert_eq!(
                body["tools"][0]["input_schema"]["properties"]["city"]["type"], "string",
                "{context}: tool schema wire shape"
            );
            assert_eq!(
                body["tool_choice"]["type"], "auto",
                "{context}: tool choice"
            );
            let messages = body["messages"]
                .as_array()
                .expect("Anthropic messages array");
            assert!(
                messages.iter().any(|message| {
                    message["role"] == "assistant"
                        && message["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block["type"] == "tool_use"
                                    && block["id"]
                                        == request_tool_call_id(TestProtocol::AnthropicMessages)
                                    && block["name"] == TOOL_NAME
                            })
                        })
                }),
                "{context}: Anthropic tool_use block"
            );
            assert!(
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block["type"] == "tool_result"
                                    && block["tool_use_id"]
                                        == request_tool_call_id(TestProtocol::AnthropicMessages)
                                    && block["content"] == TOOL_OUTPUT
                            })
                        })
                }),
                "{context}: Anthropic tool_result block"
            );
        }
        TestProtocol::OpenAiCompatible => {
            assert_eq!(body["stream"], false, "{context}: stream wire field");
            assert_eq!(
                body["tools"][0]["function"]["name"], TOOL_NAME,
                "{context}: tool wire shape"
            );
            assert_eq!(body["tool_choice"], "auto", "{context}: tool choice");
            let messages = body["messages"].as_array().expect("OpenAI messages array");
            assert!(
                messages.iter().any(|message| {
                    message["role"] == "assistant"
                        && message["tool_calls"][0]["id"] == TOOL_CALL_ID
                        && message["tool_calls"][0]["function"]["name"] == TOOL_NAME
                }),
                "{context}: OpenAI tool_calls entry"
            );
            assert!(
                messages.iter().any(|message| {
                    message["role"] == "tool"
                        && message["tool_call_id"] == TOOL_CALL_ID
                        && message["content"] == TOOL_OUTPUT
                }),
                "{context}: OpenAI tool result message"
            );
        }
        TestProtocol::OpenAiResponses => {
            assert_eq!(
                body["instructions"], SYSTEM_PROMPT,
                "{context}: instructions"
            );
            assert_eq!(body["stream"], true, "{context}: Responses egress is SSE");
            assert_eq!(
                body["tools"][0]["name"], TOOL_NAME,
                "{context}: tool wire shape"
            );
            assert_eq!(body["tool_choice"], "auto", "{context}: tool choice");
            let input = body["input"].as_array().expect("Responses input array");
            assert!(
                input.iter().any(|item| {
                    item["type"] == "function_call"
                        && item["call_id"] == TOOL_CALL_ID
                        && item["name"] == TOOL_NAME
                }),
                "{context}: Responses function_call item"
            );
            assert!(
                input.iter().any(|item| {
                    item["type"] == "function_call_output"
                        && item["call_id"] == TOOL_CALL_ID
                        && item["output"] == TOOL_OUTPUT
                }),
                "{context}: Responses function_call_output item"
            );
        }
    }
}

fn assert_request_conversion(source: TestProtocol, target: TestProtocol) {
    let route = route_name(source, target);
    let mut request = decode_request(source, request_fixture(source));
    normalize_request_tool_results(&mut request);
    assert_request_semantics(&request, TOOL_CALL_ID, &format!("{route} source decode"));

    let encoded = encode_request(target, &request);
    assert_request_wire(target, &encoded, &format!("{route} target encode"));

    let mut round_trip = decode_request(target, encoded);
    normalize_request_tool_results(&mut round_trip);
    assert_request_semantics(
        &round_trip,
        request_tool_call_id(target),
        &format!("{route} target wire decode"),
    );
}

fn custom_request_fixture() -> Value {
    json!({
        "model": MODEL,
        "stream": false,
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [
                    {
                        "type": "custom",
                        "name": CUSTOM_TOOL_NAME,
                        "description": "Run source code",
                        "format": {
                            "type": "grammar",
                            "syntax": "lark",
                            "definition": "start: source\nsource: /.+/"
                        }
                    },
                    {
                        "type": "function",
                        "name": FUNCTION_TOOL_NAME,
                        "description": "Wait for a task",
                        "parameters": {
                            "type": "object",
                            "properties": {"task_id": {"type": "string"}},
                            "required": ["task_id"],
                            "additionalProperties": false
                        },
                        "strict": false
                    }
                ]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Run the source"}]
            },
            {
                "type": "custom_tool_call",
                "call_id": CUSTOM_TOOL_CALL_ID,
                "name": CUSTOM_TOOL_NAME,
                "input": CUSTOM_TOOL_INPUT
            },
            {
                "type": "custom_tool_call_output",
                "call_id": CUSTOM_TOOL_CALL_ID,
                "output": "ok"
            }
        ],
        "tool_choice": "auto"
    })
}

fn encoded_tool_by_name<'a>(body: &'a Value, protocol: TestProtocol, name: &str) -> &'a Value {
    body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|tool| match protocol {
            TestProtocol::AnthropicMessages => tool["name"] == name,
            TestProtocol::OpenAiCompatible => tool["function"]["name"] == name,
            TestProtocol::OpenAiResponses => tool["name"] == name,
        })
        .unwrap_or_else(|| panic!("missing encoded tool {name}"))
}

fn assert_custom_request_conversion(target: TestProtocol) {
    let mut request = decode_request(TestProtocol::OpenAiResponses, custom_request_fixture());
    normalize_request_tool_results(&mut request);

    let tools = request.tools.as_ref().expect("decoded tools");
    let custom = tools
        .iter()
        .find(|tool| tool.name == CUSTOM_TOOL_NAME)
        .expect("custom tool");
    assert!(custom.is_custom());
    assert_eq!(
        custom
            .custom_format()
            .and_then(|format| format["syntax"].as_str()),
        Some("lark")
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.name == FUNCTION_TOOL_NAME && !tool.is_custom())
    );

    let history_call = request
        .messages
        .iter()
        .filter_map(|message| message.tool_calls.as_deref())
        .flatten()
        .find(|call| call.name == CUSTOM_TOOL_NAME)
        .expect("custom history call");
    assert_eq!(history_call.kind, ToolCallKind::Custom);
    assert_eq!(history_call.arguments, CUSTOM_TOOL_INPUT);

    let plan = ToolRoutePlan::for_request(&request, target.endpoint());
    assert!(plan.is_active());
    let mut upstream_request = request.clone();
    plan.prepare_upstream_request(&mut upstream_request);
    let body = encode_request(target, &upstream_request);

    let encoded_custom = encoded_tool_by_name(&body, target, CUSTOM_TOOL_NAME);
    let custom_schema = match target {
        TestProtocol::AnthropicMessages => &encoded_custom["input_schema"],
        TestProtocol::OpenAiCompatible => &encoded_custom["function"]["parameters"],
        TestProtocol::OpenAiResponses => unreachable!(),
    };
    assert_eq!(custom_schema["properties"]["input"]["type"], "string");
    assert_eq!(custom_schema["required"], json!(["input"]));
    assert_eq!(custom_schema["additionalProperties"], false);

    let encoded_function = encoded_tool_by_name(&body, target, FUNCTION_TOOL_NAME);
    let function_schema = match target {
        TestProtocol::AnthropicMessages => &encoded_function["input_schema"],
        TestProtocol::OpenAiCompatible => &encoded_function["function"]["parameters"],
        TestProtocol::OpenAiResponses => unreachable!(),
    };
    assert_eq!(function_schema["properties"]["task_id"]["type"], "string");
    assert!(function_schema["properties"].get("input").is_none());

    match target {
        TestProtocol::OpenAiCompatible => {
            let messages = body["messages"].as_array().expect("messages");
            let call = messages
                .iter()
                .filter_map(|message| message["tool_calls"].as_array())
                .flatten()
                .find(|call| call["function"]["name"] == CUSTOM_TOOL_NAME)
                .expect("bridged custom history call");
            let arguments =
                serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap())
                    .expect("wrapped custom arguments");
            assert_eq!(arguments, json!({"input": CUSTOM_TOOL_INPUT}));
            assert!(
                messages
                    .iter()
                    .all(|message| message.get("__nyro_tool_call_kind").is_none()),
                "internal custom output metadata leaked to OpenAI Compatible"
            );
        }
        TestProtocol::AnthropicMessages => {
            let blocks = body["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .collect::<Vec<_>>();
            let call = blocks
                .iter()
                .find(|block| block["type"] == "tool_use" && block["name"] == CUSTOM_TOOL_NAME)
                .expect("bridged custom tool_use");
            assert_eq!(call["input"], json!({"input": CUSTOM_TOOL_INPUT}));
            assert!(
                blocks
                    .iter()
                    .any(|block| { block["type"] == "tool_result" && block["content"] == "ok" }),
                "custom tool output was not preserved"
            );
        }
        TestProtocol::OpenAiResponses => unreachable!(),
    }
}

fn response_fixture(protocol: TestProtocol) -> Value {
    match protocol {
        TestProtocol::AnthropicMessages => json!({
            "id": "resp_matrix",
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [
                {"type": "text", "text": RESPONSE_TEXT},
                {
                    "type": "tool_use",
                    "id": TOOL_CALL_ID,
                    "name": TOOL_NAME,
                    "input": {"city": "Paris"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": PROMPT_TOKENS,
                "output_tokens": COMPLETION_TOKENS
            }
        }),
        TestProtocol::OpenAiCompatible => json!({
            "id": "resp_matrix",
            "object": "chat.completion",
            "model": MODEL,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": RESPONSE_TEXT,
                    "tool_calls": [{
                        "id": TOOL_CALL_ID,
                        "type": "function",
                        "function": {
                            "name": TOOL_NAME,
                            "arguments": "{\"city\":\"Paris\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": PROMPT_TOKENS,
                "completion_tokens": COMPLETION_TOKENS,
                "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS
            }
        }),
        TestProtocol::OpenAiResponses => json!({
            "id": "resp_matrix",
            "object": "response",
            "status": "completed",
            "model": MODEL,
            "output": [
                {
                    "type": "function_call",
                    "id": "fc_matrix",
                    "call_id": TOOL_CALL_ID,
                    "name": TOOL_NAME,
                    "arguments": "{\"city\":\"Paris\"}",
                    "status": "completed"
                },
                {
                    "type": "message",
                    "id": "msg_matrix",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": RESPONSE_TEXT}]
                }
            ],
            "usage": {
                "input_tokens": PROMPT_TOKENS,
                "output_tokens": COMPLETION_TOKENS,
                "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS
            }
        }),
    }
}

fn parse_response(protocol: TestProtocol, body: Value) -> AiResponse {
    match protocol {
        TestProtocol::AnthropicMessages => AnthropicResponseParser.parse_response(body),
        TestProtocol::OpenAiCompatible => OpenAIResponseParser.parse_response(body),
        TestProtocol::OpenAiResponses => ResponsesResponseParser.parse_response(body),
    }
    .unwrap_or_else(|error| panic!("failed to parse {} response: {error:#}", protocol.name()))
}

fn format_response(protocol: TestProtocol, response: &AiResponse) -> Value {
    match protocol {
        TestProtocol::AnthropicMessages => AnthropicResponseFormatter.format_response(response),
        TestProtocol::OpenAiCompatible => OpenAIResponseFormatter.format_response(response),
        TestProtocol::OpenAiResponses => ResponsesResponseFormatter.format_response(response),
    }
}

fn canonical_stop_reason(_protocol: TestProtocol) -> &'static str {
    "tool_calls"
}

fn assert_response_semantics(response: &AiResponse, protocol: TestProtocol, context: &str) {
    assert_eq!(response.id, "resp_matrix", "{context}: response id");
    assert_eq!(response.model, MODEL, "{context}: model");
    assert_eq!(response.content, RESPONSE_TEXT, "{context}: response text");
    assert_eq!(response.tool_calls.len(), 1, "{context}: tool call count");
    let tool_call = &response.tool_calls[0];
    assert_eq!(tool_call.id, TOOL_CALL_ID, "{context}: tool call id");
    assert_eq!(tool_call.name, TOOL_NAME, "{context}: tool call name");
    assert_eq!(
        serde_json::from_str::<Value>(&tool_call.arguments)
            .unwrap_or_else(|error| panic!("{context}: invalid tool arguments: {error}")),
        json!({"city": "Paris"}),
        "{context}: tool arguments"
    );
    assert_eq!(
        response.usage.prompt_tokens, PROMPT_TOKENS,
        "{context}: prompt usage"
    );
    assert_eq!(
        response.usage.completion_tokens, COMPLETION_TOKENS,
        "{context}: completion usage"
    );
    assert_eq!(
        response.stop_reason.as_deref(),
        Some(canonical_stop_reason(protocol)),
        "{context}: stop reason"
    );
}

fn assert_response_wire(protocol: TestProtocol, body: &Value, context: &str) {
    match protocol {
        TestProtocol::AnthropicMessages => {
            let content = body["content"].as_array().expect("Anthropic content array");
            assert!(
                content
                    .iter()
                    .any(|block| block["type"] == "text" && block["text"] == RESPONSE_TEXT),
                "{context}: Anthropic text block"
            );
            assert!(
                content.iter().any(|block| {
                    block["type"] == "tool_use"
                        && block["id"] == TOOL_CALL_ID
                        && block["name"] == TOOL_NAME
                }),
                "{context}: Anthropic tool_use block"
            );
            assert_eq!(body["stop_reason"], "tool_use", "{context}: stop reason");
            assert_eq!(
                body["usage"]["input_tokens"], PROMPT_TOKENS,
                "{context}: input usage"
            );
            assert_eq!(
                body["usage"]["output_tokens"], COMPLETION_TOKENS,
                "{context}: output usage"
            );
        }
        TestProtocol::OpenAiCompatible => {
            assert_eq!(
                body["choices"][0]["message"]["content"], RESPONSE_TEXT,
                "{context}: response text"
            );
            assert_eq!(
                body["choices"][0]["message"]["tool_calls"][0]["id"], TOOL_CALL_ID,
                "{context}: tool call id"
            );
            assert_eq!(
                body["choices"][0]["message"]["tool_calls"][0]["function"]["name"], TOOL_NAME,
                "{context}: tool call name"
            );
            assert_eq!(
                body["choices"][0]["finish_reason"], "tool_calls",
                "{context}: finish reason"
            );
            assert_eq!(
                body["usage"]["prompt_tokens"], PROMPT_TOKENS,
                "{context}: prompt usage"
            );
            assert_eq!(
                body["usage"]["completion_tokens"], COMPLETION_TOKENS,
                "{context}: completion usage"
            );
        }
        TestProtocol::OpenAiResponses => {
            assert_eq!(body["status"], "completed", "{context}: status");
            let output = body["output"].as_array().expect("Responses output array");
            assert!(
                output.iter().any(|item| {
                    item["type"] == "function_call"
                        && item["call_id"] == TOOL_CALL_ID
                        && item["name"] == TOOL_NAME
                }),
                "{context}: Responses function_call item"
            );
            assert!(
                output.iter().any(|item| {
                    item["type"] == "message"
                        && item["content"][0]["type"] == "output_text"
                        && item["content"][0]["text"] == RESPONSE_TEXT
                }),
                "{context}: Responses output_text item"
            );
            assert_eq!(
                body["usage"]["input_tokens"], PROMPT_TOKENS,
                "{context}: input usage"
            );
            assert_eq!(
                body["usage"]["output_tokens"], COMPLETION_TOKENS,
                "{context}: output usage"
            );
        }
    }
}

fn assert_non_stream_response_conversion(source: TestProtocol, target: TestProtocol) {
    let route = route_name(source, target);
    let provider_response = parse_response(target, response_fixture(target));
    assert_response_semantics(
        &provider_response,
        target,
        &format!("{route} provider response parse"),
    );

    let client_body = format_response(source, &provider_response);
    assert_response_wire(
        source,
        &client_body,
        &format!("{route} client response format"),
    );

    let client_response = parse_response(source, client_body);
    assert_response_semantics(
        &client_response,
        source,
        &format!("{route} formatted response decode"),
    );
}

fn custom_response_fixture(protocol: TestProtocol) -> Value {
    let wrapped_input = json!({"input": CUSTOM_TOOL_INPUT});
    match protocol {
        TestProtocol::OpenAiCompatible => json!({
            "id": "resp_custom",
            "object": "chat.completion",
            "model": MODEL,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": CUSTOM_TOOL_CALL_ID,
                            "type": "function",
                            "function": {
                                "name": CUSTOM_TOOL_NAME,
                                "arguments": wrapped_input.to_string()
                            }
                        },
                        {
                            "id": FUNCTION_TOOL_CALL_ID,
                            "type": "function",
                            "function": {
                                "name": FUNCTION_TOOL_NAME,
                                "arguments": "{\"task_id\":\"job-1\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": PROMPT_TOKENS,
                "completion_tokens": COMPLETION_TOKENS
            }
        }),
        TestProtocol::AnthropicMessages => json!({
            "id": "resp_custom",
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [
                {
                    "type": "tool_use",
                    "id": CUSTOM_TOOL_CALL_ID,
                    "name": CUSTOM_TOOL_NAME,
                    "input": wrapped_input
                },
                {
                    "type": "tool_use",
                    "id": FUNCTION_TOOL_CALL_ID,
                    "name": FUNCTION_TOOL_NAME,
                    "input": {"task_id": "job-1"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": PROMPT_TOKENS,
                "output_tokens": COMPLETION_TOKENS
            }
        }),
        TestProtocol::OpenAiResponses => unreachable!(),
    }
}

fn assert_custom_non_stream_response_conversion(target: TestProtocol) {
    let request = decode_request(TestProtocol::OpenAiResponses, custom_request_fixture());
    let plan = ToolRoutePlan::for_request(&request, target.endpoint());
    let mut response = parse_response(target, custom_response_fixture(target));
    plan.restore_response(&mut response);

    let custom = response
        .tool_calls
        .iter()
        .find(|call| call.name == CUSTOM_TOOL_NAME)
        .expect("restored custom call");
    assert_eq!(custom.kind, ToolCallKind::Custom);
    assert_eq!(custom.arguments, CUSTOM_TOOL_INPUT);

    let function = response
        .tool_calls
        .iter()
        .find(|call| call.name == FUNCTION_TOOL_NAME)
        .expect("regular function call");
    assert_eq!(function.kind, ToolCallKind::Function);
    assert_eq!(
        serde_json::from_str::<Value>(&function.arguments).unwrap(),
        json!({"task_id": "job-1"})
    );

    let body = format_response(TestProtocol::OpenAiResponses, &response);
    let output = body["output"].as_array().expect("Responses output");
    let custom_item = output
        .iter()
        .find(|item| item["name"] == CUSTOM_TOOL_NAME)
        .expect("custom_tool_call output item");
    assert_eq!(custom_item["type"], "custom_tool_call");
    assert_eq!(custom_item["call_id"], CUSTOM_TOOL_CALL_ID);
    assert_eq!(custom_item["input"], CUSTOM_TOOL_INPUT);

    let function_item = output
        .iter()
        .find(|item| item["name"] == FUNCTION_TOOL_NAME)
        .expect("function_call output item");
    assert_eq!(function_item["type"], "function_call");
    assert_eq!(function_item["call_id"], FUNCTION_TOOL_CALL_ID);

    let round_trip = parse_response(TestProtocol::OpenAiResponses, body);
    let round_trip_custom = round_trip
        .tool_calls
        .iter()
        .find(|call| call.name == CUSTOM_TOOL_NAME)
        .expect("round-trip custom call");
    assert_eq!(round_trip_custom.kind, ToolCallKind::Custom);
    assert_eq!(round_trip_custom.arguments, CUSTOM_TOOL_INPUT);
}

fn sse_event(event: Option<&str>, data: Value) -> String {
    match event {
        Some(event) => format!("event: {event}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

fn stream_fixture(protocol: TestProtocol) -> String {
    match protocol {
        TestProtocol::AnthropicMessages => [
            sse_event(
                Some("message_start"),
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "resp_matrix",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": MODEL,
                        "stop_reason": null,
                        "usage": {"input_tokens": PROMPT_TOKENS, "output_tokens": 0}
                    }
                }),
            ),
            sse_event(
                Some("content_block_start"),
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
            ),
            sse_event(
                Some("content_block_delta"),
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": RESPONSE_TEXT}
                }),
            ),
            sse_event(
                Some("content_block_stop"),
                json!({"type": "content_block_stop", "index": 0}),
            ),
            sse_event(
                Some("content_block_start"),
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": TOOL_CALL_ID,
                        "name": TOOL_NAME,
                        "input": {}
                    }
                }),
            ),
            sse_event(
                Some("content_block_delta"),
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"city\":\"Paris\"}"
                    }
                }),
            ),
            sse_event(
                Some("content_block_stop"),
                json!({"type": "content_block_stop", "index": 1}),
            ),
            sse_event(
                Some("message_delta"),
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": COMPLETION_TOKENS}
                }),
            ),
            sse_event(Some("message_stop"), json!({"type": "message_stop"})),
        ]
        .concat(),
        TestProtocol::OpenAiCompatible => [
            sse_event(
                None,
                json!({
                    "id": "resp_matrix",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant"},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_matrix",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"content": RESPONSE_TEXT},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_matrix",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": TOOL_CALL_ID,
                                "type": "function",
                                "function": {"name": TOOL_NAME, "arguments": ""}
                            }]
                        },
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_matrix",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "function": {"arguments": "{\"city\":\"Paris\"}"}
                            }]
                        },
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_matrix",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_matrix",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": PROMPT_TOKENS,
                        "completion_tokens": COMPLETION_TOKENS,
                        "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS
                    }
                }),
            ),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat(),
        TestProtocol::OpenAiResponses => [
            sse_event(
                Some("response.created"),
                json!({
                    "type": "response.created",
                    "response": {
                        "id": "resp_matrix",
                        "object": "response",
                        "status": "in_progress",
                        "model": MODEL,
                        "output": []
                    }
                }),
            ),
            sse_event(
                Some("response.output_text.delta"),
                json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": RESPONSE_TEXT
                }),
            ),
            sse_event(
                Some("response.output_item.added"),
                json!({
                    "type": "response.output_item.added",
                    "output_index": 1,
                    "item": {
                        "type": "function_call",
                        "id": "fc_matrix",
                        "call_id": TOOL_CALL_ID,
                        "name": TOOL_NAME,
                        "arguments": "",
                        "status": "in_progress"
                    }
                }),
            ),
            sse_event(
                Some("response.function_call_arguments.delta"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": 1,
                    "delta": "{\"city\":\"Paris\"}"
                }),
            ),
            sse_event(
                Some("response.completed"),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_matrix",
                        "object": "response",
                        "status": "completed",
                        "model": MODEL,
                        "output": [],
                        "usage": {
                            "input_tokens": PROMPT_TOKENS,
                            "output_tokens": COMPLETION_TOKENS,
                            "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS
                        }
                    }
                }),
            ),
        ]
        .concat(),
    }
}

fn custom_stream_fixture(protocol: TestProtocol) -> String {
    let wrapped = serde_json::to_string(&json!({"input": CUSTOM_TOOL_INPUT})).unwrap();
    let split_at = wrapped.len() / 2;
    let (wrapped_first, wrapped_second) = wrapped.split_at(split_at);

    match protocol {
        TestProtocol::OpenAiCompatible => [
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant"},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": 0,
                            "id": CUSTOM_TOOL_CALL_ID,
                            "type": "function",
                            "function": {"name": CUSTOM_TOOL_NAME, "arguments": ""}
                        }]},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": 0,
                            "function": {"arguments": wrapped_first}
                        }]},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": 0,
                            "function": {"arguments": wrapped_second}
                        }]},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": 1,
                            "id": FUNCTION_TOOL_CALL_ID,
                            "type": "function",
                            "function": {
                                "name": FUNCTION_TOOL_NAME,
                                "arguments": "{\"task_id\":\"job-1\"}"
                            }
                        }]},
                        "finish_reason": null
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "tool_calls"
                    }]
                }),
            ),
            sse_event(
                None,
                json!({
                    "id": "resp_custom",
                    "object": "chat.completion.chunk",
                    "model": MODEL,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": PROMPT_TOKENS,
                        "completion_tokens": COMPLETION_TOKENS
                    }
                }),
            ),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat(),
        TestProtocol::AnthropicMessages => [
            sse_event(
                Some("message_start"),
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "resp_custom",
                        "model": MODEL,
                        "content": [],
                        "usage": {"input_tokens": PROMPT_TOKENS, "output_tokens": 0}
                    }
                }),
            ),
            sse_event(
                Some("content_block_start"),
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": CUSTOM_TOOL_CALL_ID,
                        "name": CUSTOM_TOOL_NAME,
                        "input": {}
                    }
                }),
            ),
            sse_event(
                Some("content_block_delta"),
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": wrapped_first}
                }),
            ),
            sse_event(
                Some("content_block_delta"),
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": wrapped_second}
                }),
            ),
            sse_event(
                Some("content_block_stop"),
                json!({"type": "content_block_stop", "index": 0}),
            ),
            sse_event(
                Some("content_block_start"),
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": FUNCTION_TOOL_CALL_ID,
                        "name": FUNCTION_TOOL_NAME,
                        "input": {}
                    }
                }),
            ),
            sse_event(
                Some("content_block_delta"),
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"task_id\":\"job-1\"}"
                    }
                }),
            ),
            sse_event(
                Some("content_block_stop"),
                json!({"type": "content_block_stop", "index": 1}),
            ),
            sse_event(
                Some("message_delta"),
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": COMPLETION_TOKENS}
                }),
            ),
            sse_event(Some("message_stop"), json!({"type": "message_stop"})),
        ]
        .concat(),
        TestProtocol::OpenAiResponses => unreachable!(),
    }
}

fn parse_stream(protocol: TestProtocol, raw: &str) -> Vec<AiStreamDelta> {
    macro_rules! parse_with {
        ($parser:expr) => {{
            let mut parser = $parser;
            let mut deltas = parser.parse_chunk(raw).unwrap_or_else(|error| {
                panic!("failed to parse {} stream: {error:#}", protocol.name())
            });
            deltas.extend(parser.finish().unwrap_or_else(|error| {
                panic!("failed to finish {} stream: {error:#}", protocol.name())
            }));
            deltas
        }};
    }

    match protocol {
        TestProtocol::AnthropicMessages => parse_with!(AnthropicStreamParser::new()),
        TestProtocol::OpenAiCompatible => parse_with!(OpenAIStreamParser::new()),
        TestProtocol::OpenAiResponses => parse_with!(ResponsesStreamParser::new()),
    }
}

fn format_stream(protocol: TestProtocol, deltas: &[AiStreamDelta]) -> (Vec<SseEvent>, Usage) {
    macro_rules! format_with {
        ($formatter:expr) => {{
            let mut formatter = $formatter;
            let mut events = formatter.format_deltas(deltas);
            events.extend(formatter.format_done());
            let usage = formatter.usage();
            (events, usage)
        }};
    }

    match protocol {
        TestProtocol::AnthropicMessages => format_with!(AnthropicStreamFormatter::new()),
        TestProtocol::OpenAiCompatible => format_with!(OpenAIStreamFormatter::new()),
        TestProtocol::OpenAiResponses => format_with!(ResponsesStreamFormatter::new()),
    }
}

#[derive(Debug, Default)]
struct StreamSummary {
    id: Option<String>,
    model: Option<String>,
    text: String,
    tool_calls: Vec<(usize, String, String)>,
    tool_arguments: HashMap<usize, String>,
    usage: Usage,
    stop_reasons: Vec<String>,
}

fn summarize_stream(deltas: &[AiStreamDelta]) -> StreamSummary {
    let mut summary = StreamSummary::default();
    for delta in deltas {
        match delta {
            AiStreamDelta::MessageStart { id, model } => {
                summary.id = Some(id.clone());
                summary.model = Some(model.clone());
            }
            AiStreamDelta::TextDelta(text) => summary.text.push_str(text),
            AiStreamDelta::ToolCallStart {
                index, id, name, ..
            } => {
                summary.tool_calls.push((*index, id.clone(), name.clone()));
            }
            AiStreamDelta::ToolCallDelta { index, arguments } => {
                summary
                    .tool_arguments
                    .entry(*index)
                    .or_default()
                    .push_str(arguments);
            }
            AiStreamDelta::Usage(usage) => merge_usage(&mut summary.usage, usage),
            AiStreamDelta::Done { stop_reason } => {
                summary.stop_reasons.push(stop_reason.clone());
            }
            _ => {}
        }
    }
    summary
}

fn merge_usage(current: &mut Usage, next: &Usage) {
    if next.prompt_tokens > 0 {
        current.prompt_tokens = next.prompt_tokens;
    }
    if next.completion_tokens > 0 {
        current.completion_tokens = next.completion_tokens;
    }
}

fn assert_usage(usage: &Usage, context: &str) {
    assert_eq!(
        usage.prompt_tokens, PROMPT_TOKENS,
        "{context}: prompt usage"
    );
    assert_eq!(
        usage.completion_tokens, COMPLETION_TOKENS,
        "{context}: completion usage"
    );
}

fn assert_stream_summary(summary: &StreamSummary, protocol: TestProtocol, context: &str) {
    assert_eq!(summary.id.as_deref(), Some("resp_matrix"), "{context}: id");
    assert_eq!(summary.model.as_deref(), Some(MODEL), "{context}: model");
    assert_eq!(summary.text, RESPONSE_TEXT, "{context}: text");

    let (tool_index, tool_id, tool_name) = summary
        .tool_calls
        .iter()
        .find(|(_, _, name)| name == TOOL_NAME)
        .unwrap_or_else(|| panic!("{context}: tool call start was not preserved"));
    assert_eq!(tool_id, TOOL_CALL_ID, "{context}: tool call id");
    assert_eq!(tool_name, TOOL_NAME, "{context}: tool call name");
    let arguments = summary
        .tool_arguments
        .get(tool_index)
        .unwrap_or_else(|| panic!("{context}: tool arguments were not preserved"));
    assert_eq!(
        serde_json::from_str::<Value>(arguments)
            .unwrap_or_else(|error| panic!("{context}: invalid tool arguments: {error}")),
        json!({"city": "Paris"}),
        "{context}: tool call arguments"
    );

    assert_usage(&summary.usage, context);
    assert!(
        summary
            .stop_reasons
            .iter()
            .any(|reason| reason == canonical_stop_reason(protocol)),
        "{context}: expected {} stop reason, got {:?}",
        canonical_stop_reason(protocol),
        summary.stop_reasons
    );
}

fn assert_stream_response_conversion(source: TestProtocol, target: TestProtocol) {
    let route = route_name(source, target);
    let provider_deltas = parse_stream(target, &stream_fixture(target));
    let provider_summary = summarize_stream(&provider_deltas);
    assert_stream_summary(
        &provider_summary,
        target,
        &format!("{route} provider stream parse"),
    );

    let (events, formatter_usage) = format_stream(source, &provider_deltas);
    assert_usage(
        &formatter_usage,
        &format!("{route} client stream formatter state"),
    );
    assert!(
        !events.is_empty(),
        "{route}: formatter emitted no SSE events"
    );

    let client_sse = events
        .iter()
        .map(SseEvent::to_sse_string)
        .collect::<String>();
    let client_deltas = parse_stream(source, &client_sse);
    let client_summary = summarize_stream(&client_deltas);
    assert_stream_summary(
        &client_summary,
        source,
        &format!("{route} formatted client stream decode"),
    );
}

fn assert_custom_stream_response_conversion(target: TestProtocol) {
    let request = decode_request(TestProtocol::OpenAiResponses, custom_request_fixture());
    let mut plan = ToolRoutePlan::for_request(&request, target.endpoint());
    let mut restored =
        plan.restore_stream_deltas(parse_stream(target, &custom_stream_fixture(target)));
    restored.extend(plan.finish_stream());

    let mut starts = HashMap::new();
    let mut arguments: HashMap<usize, String> = HashMap::new();
    let mut usage = Usage::default();
    let mut done = Vec::new();
    for delta in &restored {
        match delta {
            AiStreamDelta::ToolCallStart {
                index, name, kind, ..
            } => {
                starts.insert(*index, (name.clone(), *kind));
            }
            AiStreamDelta::ToolCallDelta {
                index,
                arguments: fragment,
            } => arguments.entry(*index).or_default().push_str(fragment),
            AiStreamDelta::Usage(partial) => usage.merge_partial(partial),
            AiStreamDelta::Done { stop_reason } => done.push(stop_reason.clone()),
            _ => {}
        }
    }

    let (custom_index, _) = starts
        .iter()
        .find(|(_, (name, _))| name == CUSTOM_TOOL_NAME)
        .expect("custom stream start");
    assert_eq!(
        starts[custom_index].1,
        ToolCallKind::Custom,
        "bridged custom stream kind"
    );
    assert_eq!(arguments[custom_index], CUSTOM_TOOL_INPUT);

    let (function_index, _) = starts
        .iter()
        .find(|(_, (name, _))| name == FUNCTION_TOOL_NAME)
        .expect("function stream start");
    assert_eq!(starts[function_index].1, ToolCallKind::Function);
    assert_eq!(
        serde_json::from_str::<Value>(&arguments[function_index]).unwrap(),
        json!({"task_id": "job-1"})
    );
    assert_usage(&usage, "bridged upstream custom stream");
    assert_eq!(done, ["tool_calls"]);

    let (events, formatter_usage) = format_stream(TestProtocol::OpenAiResponses, &restored);
    assert_usage(&formatter_usage, "Responses custom stream formatter");
    assert!(
        events.iter().any(|event| {
            event.event.as_deref() == Some("response.custom_tool_call_input.delta")
        })
    );
    assert!(events.iter().any(|event| {
        serde_json::from_str::<Value>(&event.data)
            .ok()
            .and_then(|value| value["item"]["type"].as_str().map(str::to_string))
            .as_deref()
            == Some("custom_tool_call")
    }));

    let client_sse = events
        .iter()
        .map(SseEvent::to_sse_string)
        .collect::<String>();
    let client_deltas = parse_stream(TestProtocol::OpenAiResponses, &client_sse);
    let mut client_starts = HashMap::new();
    let mut client_arguments: HashMap<usize, String> = HashMap::new();
    let mut client_usage = Usage::default();
    let mut client_done = Vec::new();
    for delta in client_deltas {
        match delta {
            AiStreamDelta::ToolCallStart {
                index, name, kind, ..
            } => {
                client_starts.insert(index, (name, kind));
            }
            AiStreamDelta::ToolCallDelta {
                index,
                arguments: fragment,
            } => client_arguments
                .entry(index)
                .or_default()
                .push_str(&fragment),
            AiStreamDelta::Usage(partial) => client_usage.merge_partial(&partial),
            AiStreamDelta::Done { stop_reason } => client_done.push(stop_reason),
            _ => {}
        }
    }

    let (client_custom_index, (_, client_custom_kind)) = client_starts
        .iter()
        .find(|(_, (name, _))| name == CUSTOM_TOOL_NAME)
        .expect("Responses custom stream round-trip start");
    assert_eq!(*client_custom_kind, ToolCallKind::Custom);
    assert_eq!(client_arguments[client_custom_index], CUSTOM_TOOL_INPUT);

    let (client_function_index, (_, client_function_kind)) = client_starts
        .iter()
        .find(|(_, (name, _))| name == FUNCTION_TOOL_NAME)
        .expect("Responses function stream round-trip start");
    assert_eq!(*client_function_kind, ToolCallKind::Function);
    assert_eq!(
        serde_json::from_str::<Value>(&client_arguments[client_function_index]).unwrap(),
        json!({"task_id": "job-1"})
    );
    assert_usage(&client_usage, "Responses custom stream round trip");
    assert_eq!(client_done, ["tool_calls"]);
}

macro_rules! matrix_case {
    ($name:ident, $runner:ident, $source:ident => $target:ident) => {
        #[test]
        fn $name() {
            $runner(TestProtocol::$source, TestProtocol::$target);
        }
    };
}

matrix_case!(
    request_anthropic_to_openai_compatible,
    assert_request_conversion,
    AnthropicMessages => OpenAiCompatible
);
matrix_case!(
    request_anthropic_to_openai_responses,
    assert_request_conversion,
    AnthropicMessages => OpenAiResponses
);
matrix_case!(
    request_openai_compatible_to_anthropic,
    assert_request_conversion,
    OpenAiCompatible => AnthropicMessages
);
matrix_case!(
    request_openai_compatible_to_openai_responses,
    assert_request_conversion,
    OpenAiCompatible => OpenAiResponses
);
matrix_case!(
    request_openai_responses_to_anthropic,
    assert_request_conversion,
    OpenAiResponses => AnthropicMessages
);
matrix_case!(
    request_openai_responses_to_openai_compatible,
    assert_request_conversion,
    OpenAiResponses => OpenAiCompatible
);

matrix_case!(
    non_stream_anthropic_to_openai_compatible,
    assert_non_stream_response_conversion,
    AnthropicMessages => OpenAiCompatible
);

#[test]
fn non_stream_anthropic_to_openai_responses() {
    assert_non_stream_response_conversion(
        TestProtocol::AnthropicMessages,
        TestProtocol::OpenAiResponses,
    );
}

matrix_case!(
    non_stream_openai_compatible_to_anthropic,
    assert_non_stream_response_conversion,
    OpenAiCompatible => AnthropicMessages
);
matrix_case!(
    non_stream_openai_compatible_to_openai_responses,
    assert_non_stream_response_conversion,
    OpenAiCompatible => OpenAiResponses
);
matrix_case!(
    non_stream_openai_responses_to_anthropic,
    assert_non_stream_response_conversion,
    OpenAiResponses => AnthropicMessages
);
matrix_case!(
    non_stream_openai_responses_to_openai_compatible,
    assert_non_stream_response_conversion,
    OpenAiResponses => OpenAiCompatible
);

#[test]
fn stream_anthropic_to_openai_compatible() {
    assert_stream_response_conversion(
        TestProtocol::AnthropicMessages,
        TestProtocol::OpenAiCompatible,
    );
}

#[test]
fn stream_anthropic_to_openai_responses() {
    assert_stream_response_conversion(
        TestProtocol::AnthropicMessages,
        TestProtocol::OpenAiResponses,
    );
}

#[test]
fn stream_openai_compatible_to_anthropic() {
    assert_stream_response_conversion(
        TestProtocol::OpenAiCompatible,
        TestProtocol::AnthropicMessages,
    );
}

matrix_case!(
    stream_openai_compatible_to_openai_responses,
    assert_stream_response_conversion,
    OpenAiCompatible => OpenAiResponses
);
matrix_case!(
    stream_openai_responses_to_anthropic,
    assert_stream_response_conversion,
    OpenAiResponses => AnthropicMessages
);

#[test]
fn stream_openai_responses_to_openai_compatible() {
    assert_stream_response_conversion(
        TestProtocol::OpenAiResponses,
        TestProtocol::OpenAiCompatible,
    );
}

#[test]
fn request_openai_responses_custom_to_openai_compatible() {
    assert_custom_request_conversion(TestProtocol::OpenAiCompatible);
}

#[test]
fn request_openai_responses_custom_to_anthropic() {
    assert_custom_request_conversion(TestProtocol::AnthropicMessages);
}

#[test]
fn non_stream_openai_responses_custom_to_openai_compatible() {
    assert_custom_non_stream_response_conversion(TestProtocol::OpenAiCompatible);
}

#[test]
fn non_stream_openai_responses_custom_to_anthropic() {
    assert_custom_non_stream_response_conversion(TestProtocol::AnthropicMessages);
}

#[test]
fn stream_openai_responses_custom_to_openai_compatible() {
    assert_custom_stream_response_conversion(TestProtocol::OpenAiCompatible);
}

#[test]
fn stream_openai_responses_custom_to_anthropic() {
    assert_custom_stream_response_conversion(TestProtocol::AnthropicMessages);
}
