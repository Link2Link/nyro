//! Shared helpers for the llm-bridge-derived protocol conversion test suites.
//!
//! These suites port the conversion-correctness tests from the TypeScript
//! `llm-bridge` project (https://github.com/Dhravya/llm-bridge) onto Nyro's
//! IR + codec architecture. The mapping is:
//!
//! | llm-bridge concept                  | Nyro equivalent                        |
//! |-------------------------------------|----------------------------------------|
//! | `toUniversal(provider, body)`       | `<Provider>Decoder::decode_request`    |
//! | `fromUniversal(provider, universal)`| `<Provider>Encoder::encode_request`    |
//! | `translateBetweenProviders(a,b,x)`  | decode with `a`, encode with `b`       |
//! | `parseStream(provider, sse)`        | `<Provider>StreamParser::parse_chunk`  |
//! | `emitStream(provider, deltas)`      | `<Provider>StreamFormatter::format_deltas` |
//!
//! Assertions target the IR contract (decode side) and the protocol wire
//! format (encode side), mirroring the original test semantics.

// Each test binary uses only a subset of the shared helpers.
#![allow(dead_code)]
// Re-exported items are used by different binaries in different combinations.
#![allow(unused_imports)]

use nyro_core::protocol::codec::anthropic::messages::decoder::AnthropicDecoder;
use nyro_core::protocol::codec::anthropic::messages::encoder::AnthropicEncoder;
use nyro_core::protocol::codec::anthropic::messages::stream::{
    AnthropicResponseFormatter, AnthropicResponseParser, AnthropicStreamFormatter,
    AnthropicStreamParser,
};
use nyro_core::protocol::codec::google::gemini::decoder::GoogleDecoder;
use nyro_core::protocol::codec::google::gemini::encoder::GoogleEncoder;
use nyro_core::protocol::codec::google::gemini::stream::{
    GoogleResponseFormatter, GoogleResponseParser, GoogleStreamFormatter, GoogleStreamParser,
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
pub use nyro_core::protocol::ir::{
    AiRequest, AiResponse, ContentBlock, Message, MessageContent, Role, ToolCall, Usage,
};
use nyro_core::protocol::{
    RequestDecoder, RequestEncoder, ResponseDecoder, ResponseEncoder, SseEvent,
    StreamResponseDecoder, StreamResponseEncoder,
};
pub use serde_json::Value;
use std::fmt;

/// The four protocols exercised by the conversion suites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P {
    AnthropicMessages,
    OpenAiChat,
    OpenAiResponses,
    GoogleGemini,
}

impl fmt::Display for P {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::GoogleGemini => "google-gemini",
        })
    }
}

// ── Request conversion (toUniversal / fromUniversal) ─────────────────────────

pub fn decode_request(p: P, body: Value) -> AiRequest {
    let result = match p {
        P::AnthropicMessages => AnthropicDecoder.decode_request(body),
        P::OpenAiChat => OpenAIDecoder.decode_request(body),
        P::OpenAiResponses => ResponsesDecoder.decode_request(body),
        P::GoogleGemini => GoogleDecoder.decode_request(body),
    };
    result.unwrap_or_else(|error| panic!("{p}: decode_request failed: {error:#}"))
}

/// Google embeds the model in the URL path; `decode_with_model` mirrors that.
pub fn decode_google_request(body: Value, model: &str) -> AiRequest {
    GoogleDecoder
        .decode_with_model(body, model, false)
        .unwrap_or_else(|error| panic!("google-gemini: decode_request failed: {error:#}"))
}

pub fn encode_request(p: P, req: &AiRequest) -> Value {
    encode_request_full(p, req).0
}

pub fn encode_request_full(p: P, req: &AiRequest) -> (Value, reqwest::header::HeaderMap) {
    let result = match p {
        P::AnthropicMessages => AnthropicEncoder.encode_request(req),
        P::OpenAiChat => OpenAIEncoder.encode_request(req),
        P::OpenAiResponses => ResponsesEncoder.encode_request(req),
        P::GoogleGemini => GoogleEncoder.encode_request(req),
    };
    result.unwrap_or_else(|error| panic!("{p}: encode_request failed: {error:#}"))
}

/// `translateBetweenProviders(source, target, body)`.
pub fn translate(source: P, body: Value, target: P) -> Value {
    encode_request(target, &decode_request(source, body))
}

/// Round-trip: decode then encode back to the same protocol.
pub fn round_trip_request(p: P, body: Value) -> Value {
    encode_request(p, &decode_request(p, body))
}

// ── Response conversion (parse / format) ────────────────────────────────────

pub fn parse_response(p: P, body: Value) -> AiResponse {
    let result = match p {
        P::AnthropicMessages => AnthropicResponseParser.parse_response(body),
        P::OpenAiChat => OpenAIResponseParser.parse_response(body),
        P::OpenAiResponses => ResponsesResponseParser.parse_response(body),
        P::GoogleGemini => GoogleResponseParser.parse_response(body),
    };
    result.unwrap_or_else(|error| panic!("{p}: parse_response failed: {error:#}"))
}

pub fn format_response(p: P, resp: &AiResponse) -> Value {
    match p {
        P::AnthropicMessages => AnthropicResponseFormatter.format_response(resp),
        P::OpenAiChat => OpenAIResponseFormatter.format_response(resp),
        P::OpenAiResponses => ResponsesResponseFormatter.format_response(resp),
        P::GoogleGemini => GoogleResponseFormatter.format_response(resp),
    }
}

// ── Streaming conversion (parse / emit) ──────────────────────────────────────

pub fn parse_stream(p: P, raw: &str) -> Vec<StreamDelta> {
    parse_stream_chunks(p, &[raw])
}

/// Feed each chunk to a fresh parser (`parse_chunk` per chunk), then `finish`.
///
/// Chunks are framed with a trailing `\n\n` before being handed to
/// `parse_chunk`, mirroring real SSE wire framing. All nyro stream parsers
/// split their input on `\n\n`; without the frame a chunk is buffered until
/// `finish()`, and several chunks concatenate into a single unparseable block.
pub fn parse_stream_chunks(p: P, chunks: &[&str]) -> Vec<StreamDelta> {
    let mut parser: Box<dyn StreamResponseDecoder> = match p {
        P::AnthropicMessages => Box::new(AnthropicStreamParser::new()),
        P::OpenAiChat => Box::new(OpenAIStreamParser::new()),
        P::OpenAiResponses => Box::new(ResponsesStreamParser::new()),
        P::GoogleGemini => Box::new(GoogleStreamParser::new()),
    };
    let mut deltas: Vec<StreamDelta> = Vec::new();
    for chunk in chunks {
        let framed = format!("{chunk}\n\n");
        let parsed = parser
            .parse_chunk(&framed)
            .unwrap_or_else(|error| panic!("{p}: parse_chunk failed: {error:#}"));
        deltas.extend(parsed);
    }
    deltas.extend(
        parser
            .finish()
            .unwrap_or_else(|error| panic!("{p}: parse_stream finish failed: {error:#}")),
    );
    deltas
}

pub fn format_stream(p: P, deltas: &[StreamDelta]) -> Vec<SseEvent> {
    let mut formatter: Box<dyn StreamResponseEncoder> = match p {
        P::AnthropicMessages => Box::new(AnthropicStreamFormatter::new()),
        P::OpenAiChat => Box::new(OpenAIStreamFormatter::new()),
        P::OpenAiResponses => Box::new(ResponsesStreamFormatter::new()),
        P::GoogleGemini => Box::new(GoogleStreamFormatter::new()),
    };
    let mut events = formatter.format_deltas(deltas);
    events.extend(formatter.format_done());
    events
}

/// Parse every `data:` payload of the emitted SSE events as JSON.
pub fn sse_jsons(events: &[SseEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| serde_json::from_str(&event.data).ok())
        .collect()
}

/// Concatenated wire form of the emitted SSE events (`event:` + `data:` lines).
pub fn sse_string(events: &[SseEvent]) -> String {
    events.iter().map(|event| event.to_sse_string()).collect()
}

// ── IR construction helpers ──────────────────────────────────────────────────

pub fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

pub fn user_msg(text: &str) -> Message {
    text_message(Role::User, text)
}

pub fn assistant_msg(text: &str) -> Message {
    text_message(Role::Assistant, text)
}

pub fn system_msg(text: &str) -> Message {
    text_message(Role::System, text)
}

/// Assistant message carrying one tool call, arguments as raw JSON text.
pub fn assistant_tool_call_msg(id: &str, name: &str, arguments_json: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text(String::new()),
        tool_calls: Some(vec![ToolCall::function(id, name, arguments_json)]),
        tool_call_id: None,
        meta: None,
    }
}

/// Tool result message (block form) correlated to `tool_use_id`.
pub fn tool_result_msg(tool_use_id: &str, content: Value) -> Message {
    Message {
        role: Role::Tool,
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content,
            is_error: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

pub fn request(model: &str, messages: Vec<Message>) -> AiRequest {
    AiRequest::new(model.to_string(), messages)
}

// ── Assertion helpers ────────────────────────────────────────────────────────

/// Navigate a JSON value via a JSON-pointer-ish `/a/b/c` path, panicking with
/// the path and the actual value on a miss.
pub fn field<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path)
        .unwrap_or_else(|| panic!("missing JSON field `{path}` in {v}"))
}

pub fn field_str<'a>(v: &'a Value, path: &str) -> &'a str {
    field(v, path)
        .as_str()
        .unwrap_or_else(|| panic!("field `{path}` is not a string in {v}"))
}

pub fn field_str_eq(v: &Value, path: &str, expected: &str) {
    assert_eq!(field_str(v, path), expected, "JSON field `{path}`");
}

/// All tool calls on the assistant messages of a request.
pub fn tool_calls(req: &AiRequest) -> Vec<&ToolCall> {
    req.messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .collect()
}

pub fn all_text(messages: &[Message]) -> Vec<String> {
    messages.iter().map(|m| m.content.to_text()).collect()
}

/// Assert the message `role` sequence of a request.
pub fn assert_roles(req: &AiRequest, expected: &[Role]) {
    let actual: Vec<Role> = req.messages.iter().map(|m| m.role).collect();
    assert_eq!(actual, expected, "message role sequence");
}

/// Format a stream delta list as a compact human-readable trace for diagnostics.
pub fn delta_trace(deltas: &[StreamDelta]) -> String {
    deltas
        .iter()
        .map(|d| match d {
            StreamDelta::MessageStart { id, model } => {
                format!("start(id={id},model={model})")
            }
            StreamDelta::TextDelta(t) => format!("text({t:?})"),
            StreamDelta::ThinkingDelta(t) => format!("thinking({t:?})"),
            StreamDelta::ThinkingSignature(s) => format!("sig({s:?})"),
            StreamDelta::ToolCallStart {
                index,
                id,
                name,
                kind,
            } => format!("tool_start({index},{id},{name},{kind:?})"),
            StreamDelta::ToolCallDelta { index, arguments } => {
                format!("tool_delta({index},{arguments:?})")
            }
            StreamDelta::ToolCallComplete { index, tool_call } => {
                format!("tool_complete({index},{}={})", tool_call.name, tool_call.arguments)
            }
            StreamDelta::Usage(u) => format!(
                "usage(prompt={},completion={})",
                u.prompt_tokens, u.completion_tokens
            ),
            StreamDelta::Done { stop_reason } => format!("done({stop_reason})"),
            StreamDelta::StreamError { error } => format!("error({:?})", error.kind),
            StreamDelta::UnexpectedEof => "eof".to_string(),
            StreamDelta::Unknown { raw } => format!("unknown({raw:?})"),
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// Concatenated text of a delta trace (for `anchor in visible text` style checks).
pub fn delta_text(deltas: &[StreamDelta]) -> String {
    deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::TextDelta(t) => Some(t.as_str()),
            StreamDelta::ThinkingDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

// ── Stream delta structural comparison ───────────────────────────────────────
// `AiStreamDelta` intentionally does not derive `PartialEq` in the IR; these
// helpers give the conversion suites structural equality on the delta model.

fn tool_call_eq(a: &ToolCall, b: &ToolCall) -> bool {
    a.id == b.id && a.name == b.name && a.kind == b.kind && a.arguments == b.arguments
}

fn usage_eq(a: &Usage, b: &Usage) -> bool {
    a.prompt_tokens == b.prompt_tokens
        && a.completion_tokens == b.completion_tokens
        && a.total_tokens == b.total_tokens
        && a.cache_read_tokens == b.cache_read_tokens
        && a.cache_creation_tokens == b.cache_creation_tokens
        && a.server_tool_use.as_ref().map(|s| (&s.web_search_requests, &s.web_fetch_requests))
            == b.server_tool_use.as_ref().map(|s| (&s.web_search_requests, &s.web_fetch_requests))
}

/// Structural equality for stream deltas.
pub fn delta_eq(a: &StreamDelta, b: &StreamDelta) -> bool {
    match (a, b) {
        (StreamDelta::MessageStart { id: ai, model: am }, StreamDelta::MessageStart { id: bi, model: bm }) => {
            ai == bi && am == bm
        }
        (StreamDelta::TextDelta(a), StreamDelta::TextDelta(b)) => a == b,
        (StreamDelta::ThinkingDelta(a), StreamDelta::ThinkingDelta(b)) => a == b,
        (StreamDelta::ThinkingSignature(a), StreamDelta::ThinkingSignature(b)) => a == b,
        (
            StreamDelta::ToolCallStart { index: ai, id: aid, name: an, kind: ak },
            StreamDelta::ToolCallStart { index: bi, id: bid, name: bn, kind: bk },
        ) => ai == bi && aid == bid && an == bn && ak == bk,
        (StreamDelta::ToolCallDelta { index: ai, arguments: aa }, StreamDelta::ToolCallDelta { index: bi, arguments: ba }) => {
            ai == bi && aa == ba
        }
        (StreamDelta::ToolCallComplete { index: ai, tool_call: at }, StreamDelta::ToolCallComplete { index: bi, tool_call: bt }) => {
            ai == bi && tool_call_eq(at, bt)
        }
        (StreamDelta::Usage(a), StreamDelta::Usage(b)) => usage_eq(a, b),
        (StreamDelta::Done { stop_reason: a }, StreamDelta::Done { stop_reason: b }) => a == b,
        (
            StreamDelta::StreamError { error: a },
            StreamDelta::StreamError { error: b },
        ) => a.kind == b.kind && a.message == b.message && a.status_code == b.status_code,
        (StreamDelta::UnexpectedEof, StreamDelta::UnexpectedEof) => true,
        (StreamDelta::Unknown { raw: a }, StreamDelta::Unknown { raw: b }) => a == b,
        _ => false,
    }
}

/// Assert two stream deltas are structurally equal, printing both on mismatch.
pub fn assert_delta_eq(actual: &StreamDelta, expected: &StreamDelta) {
    assert_delta_eq_msg(actual, expected, "");
}

/// `assert_delta_eq` with an extra context message (e.g. a delta trace).
pub fn assert_delta_eq_msg(actual: &StreamDelta, expected: &StreamDelta, msg: &str) {
    assert!(
        delta_eq(actual, expected),
        "stream delta mismatch{}{}\n  actual:   {actual:?}\n  expected: {expected:?}",
        if msg.is_empty() { "" } else { ": " },
        msg
    );
}

/// Assert the last delta of a stream is structurally equal to `expected`.
pub fn assert_last_delta(deltas: &[StreamDelta], expected: &StreamDelta) {
    let Some(actual) = deltas.last() else {
        panic!("stream has no deltas, expected {expected:?}");
    };
    assert!(
        delta_eq(actual, expected),
        "stream delta mismatch (last):\n  trace:\n  {}\n  actual:   {actual:?}\n  expected: {expected:?}",
        delta_trace(deltas)
    );
}

// Re-export commonly used items so test files only need `use conv_common::*;`.
pub use serde_json::json;
pub use nyro_core::protocol::ir::{
    AiStreamDelta as StreamDelta, CacheControl, GenerationConfig, MediaSource, ReasoningConfig,
    ReasoningEffort, ResponseFormat, StreamConfig, ToolCallKind as ToolKind, ToolChoice, ToolSpec,
};
