//! TPS usage capture: reasoning-token extraction across all four wire
//! codecs, and the stream/non-stream parity that keeps compat logging from
//! drifting between the two paths.
//!
//! These tests go through the PUBLIC protocol API (`ProtocolEndpoint::handler()`
//! factories), mirroring how the dispatcher (and the compat raw-usage
//! bypass, which reuses the same codec extractors) observes usage.

use nyro_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
};
use nyro_core::protocol::ir::{AiStreamDelta, Usage};
// The trait methods (`parse_chunk`, `parse_response`) resolve on the boxed
// trait objects returned by the handler factories without extra imports.
use serde_json::Value;

/// Drive a protocol's stream decoder over SSE text and merge the emitted
/// usage snapshots the way the dispatch log accumulator does.
fn stream_usage(endpoint: nyro_core::protocol::ids::ProtocolEndpoint, sse: &str) -> Usage {
    let mut decoder = endpoint.handler().make_stream_response_decoder();
    let mut usage = Usage::default();
    let deltas = decoder.parse_chunk(sse).expect("stream parse");
    merge(&mut usage, deltas);
    let deltas = decoder.finish().expect("stream finish");
    merge(&mut usage, deltas);
    usage
}

fn merge(usage: &mut Usage, deltas: Vec<AiStreamDelta>) {
    for delta in deltas {
        if let AiStreamDelta::Usage(snapshot) = delta {
            usage.merge_partial(&snapshot);
        }
    }
}

fn nonstream_usage(endpoint: nyro_core::protocol::ids::ProtocolEndpoint, body: Value) -> Usage {
    endpoint
        .handler()
        .make_response_decoder()
        .parse_response(body)
        .expect("non-stream parse")
        .usage
}

// ── OpenAI Responses ──

#[test]
fn responses_stream_completed_and_incomplete_extract_reasoning() {
    let completed = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":64}}}}\n\n",
    );
    let usage = stream_usage(OPENAI_RESPONSES_V1, completed);
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (100, 80));
    assert_eq!(usage.reasoning_tokens, Some(64));

    let incomplete = concat!(
        "event: response.incomplete\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_2\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[],\"usage\":{\"input_tokens\":70,\"output_tokens\":50,\"output_tokens_details\":{\"reasoning_tokens\":40}}}}\n\n",
    );
    let usage = stream_usage(OPENAI_RESPONSES_V1, incomplete);
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (70, 50));
    assert_eq!(usage.reasoning_tokens, Some(40));
}

#[test]
fn responses_stream_and_nonstream_usage_stay_identical() {
    // Regression guard for the old drift: the stream terminal events used
    // to decode usage inline and silently dropped reasoning_tokens while
    // the non-stream parser kept it.
    let usage_json = serde_json::json!({
        "input_tokens": 13582,
        "input_tokens_details": {"cached_tokens": 13056, "cache_write_tokens": 0},
        "output_tokens": 692,
        "output_tokens_details": {"reasoning_tokens": 512},
        "total_tokens": 14274,
    });
    let mut body = serde_json::json!({
        "id": "resp_parity",
        "model": "gpt-5",
        "status": "completed",
        "output": [],
    });
    body["usage"] = usage_json.clone();
    let nonstream = nonstream_usage(OPENAI_RESPONSES_V1, body);

    let mut event = serde_json::json!({
        "type": "response.completed",
        "response": {"id": "resp_parity", "status": "completed", "output": []},
    });
    event["response"]["usage"] = usage_json;
    let sse = format!("data: {}\n\n", event);
    let streamed = stream_usage(OPENAI_RESPONSES_V1, &sse);

    assert_eq!(streamed.prompt_tokens, nonstream.prompt_tokens);
    assert_eq!(streamed.completion_tokens, nonstream.completion_tokens);
    assert_eq!(streamed.cache_read_tokens, nonstream.cache_read_tokens);
    assert_eq!(
        streamed.cache_creation_tokens,
        nonstream.cache_creation_tokens
    );
    assert_eq!(streamed.reasoning_tokens, nonstream.reasoning_tokens);
    assert_eq!(streamed.reasoning_tokens, Some(512));
}

// ── OpenAI-compatible chat ──

#[test]
fn chat_stream_and_nonstream_extract_reasoning() {
    let sse = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":30}}}\n\n",
        "data: [DONE]\n\n",
    );
    let usage = stream_usage(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, sse);
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (100, 80));
    assert_eq!(usage.reasoning_tokens, Some(30));

    let body = serde_json::json!({
        "id": "c2",
        "object": "chat.completion",
        "model": "m",
        "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 80,
            "completion_tokens_details": {"reasoning_tokens": 30},
        },
    });
    let usage = nonstream_usage(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, body);
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (100, 80));
    assert_eq!(usage.reasoning_tokens, Some(30));
}

// ── Anthropic messages ──

#[test]
fn anthropic_stream_and_nonstream_extract_reasoning() {
    // Anthropic reports input NET of cache; IR prompt_tokens is the GROSS
    // figure, so input + cache_read must be folded (100 = 80 + 20).
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":80,\"cache_read_input_tokens\":20,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":80,\"cache_read_input_tokens\":20,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":16}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let usage = stream_usage(ANTHROPIC_MESSAGES_2023_06_01, sse);
    assert_eq!(
        usage.prompt_tokens, 100,
        "net input must fold cache to gross"
    );
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.reasoning_tokens, Some(16));

    let body = serde_json::json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "claude-x",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 80,
            "cache_read_input_tokens": 20,
            "output_tokens": 80,
            "output_tokens_details": {"reasoning_tokens": 16},
        },
    });
    let usage = nonstream_usage(ANTHROPIC_MESSAGES_2023_06_01, body);
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.reasoning_tokens, Some(16));
}

// ── Google Gemini ──

#[test]
fn gemini_stream_and_nonstream_extract_reasoning() {
    // Gemini totalTokenCount includes thoughts; IR completion is
    // total - prompt (80), and thoughts surface as reasoning_tokens.
    let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":20,\"thoughtsTokenCount\":60,\"totalTokenCount\":180}}\n\n";
    let usage = stream_usage(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, sse);
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.reasoning_tokens, Some(60));

    let body = serde_json::json!({
        "candidates": [{
            "content": {"parts": [{"text": "hi"}], "role": "model"},
            "finishReason": "STOP",
        }],
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 20,
            "thoughtsTokenCount": 60,
            "totalTokenCount": 180,
        },
    });
    let usage = nonstream_usage(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, body);
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.reasoning_tokens, Some(60));
}

// ── Usage merge semantics (drives every accumulator above) ──

#[test]
fn merge_partial_keeps_reasoning_from_latest_snapshot_only() {
    let mut usage = Usage::default();
    usage.merge_partial(&Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        reasoning_tokens: Some(10),
        ..Usage::default()
    });
    // Terminal snapshot replaces the pair.
    usage.merge_partial(&Usage {
        prompt_tokens: 100,
        completion_tokens: 80,
        reasoning_tokens: Some(60),
        ..Usage::default()
    });
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (100, 80));
    assert_eq!(usage.reasoning_tokens, Some(60));
}

#[test]
fn merge_partial_does_not_clear_fields_missing_from_next_snapshot() {
    let mut usage = Usage {
        prompt_tokens: 100,
        completion_tokens: 80,
        reasoning_tokens: Some(60),
        cache_read_tokens: Some(20),
        ..Usage::default()
    };
    // A later snapshot without reasoning (presence loss) must not erase the
    // known value, and zero counts must not overwrite positive ones.
    usage.merge_partial(&Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        ..Usage::default()
    });
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.reasoning_tokens, Some(60));
    assert_eq!(usage.cache_read_tokens, Some(20));
}
