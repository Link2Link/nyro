//! Ported from sub2api `apicompat/responses_anthropic_cache_creation_test.go`
//! and the cache-token sections of `anthropic_responses_test.go` /
//! `chatcompletions_responses_test.go`.
//!
//! Pins the cross-protocol usage semantics on Nyro's IR `Usage`:
//!
//! * IR `prompt_tokens` is GROSS — the total prompt tokens billed. Anthropic
//!   wire `input_tokens` is NET and reports cache components separately; the
//!   decoder folds them in, the formatter reverses the fold on egress.
//! * Responses API reports cache stats under `usage.input_tokens_details`
//!   (`cached_tokens` / `cache_write_tokens`); the parser surfaces them as
//!   `cache_read_tokens` / `cache_creation_tokens`.
//! * OpenAI-compatible usage carries `prompt_tokens_details.cached_tokens`
//!   (or DeepSeek `prompt_cache_hit_tokens`), also surfaced as cache_read.
//! * Zero-valued cache counters stay `None` (they would skew analytics).

mod conv_common;

use conv_common::*;

// ── Anthropic: NET input + cache components → IR GROSS prompt_tokens ─────────

#[test]
fn anthropic_usage_folds_cache_components_into_gross_prompt_tokens() {
    let resp = parse_response(
        P::AnthropicMessages,
        json!({
            "id": "msg_cache",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 3318,
                "cache_read_input_tokens": 3100,
                "cache_creation_input_tokens": 96,
                "output_tokens": 512
            }
        }),
    );

    // IR prompt_tokens = net input + cache_read + cache_creation (gross).
    assert_eq!(resp.usage.prompt_tokens, 3318 + 3100 + 96);
    assert_eq!(resp.usage.completion_tokens, 512);
    assert_eq!(resp.usage.cache_read_tokens, Some(3100));
    assert_eq!(resp.usage.cache_creation_tokens, Some(96));
}

#[test]
fn anthropic_usage_without_cache_stays_net() {
    let resp = parse_response(
        P::AnthropicMessages,
        json!({
            "id": "msg_plain",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }),
    );

    assert_eq!(resp.usage.prompt_tokens, 10);
    assert_eq!(resp.usage.cache_read_tokens, None);
    assert_eq!(resp.usage.cache_creation_tokens, None);
}

#[test]
fn anthropic_formatter_reverses_gross_to_net_on_egress() {
    // Build an IR response with cache components and format back to Anthropic:
    // input_tokens must be NET (gross minus cache components), with the cache
    // fields emitted separately — the wire shape Anthropic SDKs expect.
    let mut resp = AiResponse::new("msg_rt", "claude-sonnet-4-5");
    resp.content = "ok".to_string();
    resp.usage.prompt_tokens = 3318 + 3100 + 96;
    resp.usage.completion_tokens = 512;
    resp.usage.cache_read_tokens = Some(3100);
    resp.usage.cache_creation_tokens = Some(96);

    let out = format_response(P::AnthropicMessages, &resp);
    assert_eq!(field(&out, "/usage/input_tokens"), &json!(3318));
    assert_eq!(field(&out, "/usage/output_tokens"), &json!(512));
    assert_eq!(
        field(&out, "/usage/cache_read_input_tokens"),
        &json!(3100)
    );
    assert_eq!(
        field(&out, "/usage/cache_creation_input_tokens"),
        &json!(96)
    );
}

// ── Responses: input_tokens_details → cache_read / cache_creation ────────────

#[test]
fn responses_usage_surfaces_cache_write_tokens() {
    let resp = parse_response(
        P::OpenAiResponses,
        json!({
            "id": "resp_write",
            "model": "gpt-5.6",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }],
            "usage": {
                "input_tokens": 5000,
                "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 4800},
                "output_tokens": 100
            }
        }),
    );

    assert_eq!(resp.usage.prompt_tokens, 5000);
    assert_eq!(resp.usage.cache_creation_tokens, Some(4800));
    // Zero-valued cached_tokens must stay None (analytics would misread 0 as a hit).
    assert_eq!(resp.usage.cache_read_tokens, None);
}

#[test]
fn responses_usage_surfaces_cached_tokens() {
    let resp = parse_response(
        P::OpenAiResponses,
        json!({
            "id": "resp_read",
            "model": "gpt-5.6",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }],
            "usage": {
                "input_tokens": 60840,
                "input_tokens_details": {"cached_tokens": 59136, "cache_write_tokens": 0},
                "output_tokens": 692
            }
        }),
    );

    assert_eq!(resp.usage.cache_read_tokens, Some(59136));
    assert_eq!(resp.usage.cache_creation_tokens, None);
}

// ── OpenAI-compatible: prompt_tokens_details / prompt_cache_hit_tokens ───────

#[test]
fn openai_chat_usage_surfaces_cached_tokens() {
    let resp = parse_response(
        P::OpenAiChat,
        json!({
            "id": "chatcmpl-cache",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1500,
                "completion_tokens": 100,
                "prompt_tokens_details": {"cached_tokens": 1200}
            }
        }),
    );

    assert_eq!(resp.usage.prompt_tokens, 1500);
    assert_eq!(resp.usage.cache_read_tokens, Some(1200));
}

#[test]
fn openai_chat_usage_surfaces_deepseek_cache_hit_tokens() {
    let resp = parse_response(
        P::OpenAiChat,
        json!({
            "id": "chatcmpl-ds",
            "model": "deepseek-v4-flash",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 50,
                "prompt_cache_hit_tokens": 800,
                "prompt_cache_miss_tokens": 200
            }
        }),
    );

    assert_eq!(resp.usage.cache_read_tokens, Some(800));
}

// ── Streaming: cache stats survive parse → emit → re-parse round trips ───────

#[test]
fn anthropic_stream_cache_usage_round_trips() {
    let raw = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3318,\"cache_read_input_tokens\":3100,\"cache_creation_input_tokens\":96,\"output_tokens\":0}}}\n\
         \nevent: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":512}}\n\
         \nevent: message_stop\n\
         data: {\"type\":\"message_stop\"}";

    let deltas = parse_stream(P::AnthropicMessages, raw);
    let usage = deltas
        .iter()
        .find_map(|d| match d {
            StreamDelta::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("usage delta");
    assert_eq!(usage.prompt_tokens, 3318 + 3100 + 96);
    assert_eq!(usage.cache_read_tokens, Some(3100));
    assert_eq!(usage.cache_creation_tokens, Some(96));

    // Re-emit to Anthropic wire: net input restored, cache fields separate.
    let out = sse_string(&format_stream(P::AnthropicMessages, &deltas));
    assert!(
        out.contains("\"input_tokens\":3318"),
        "gross→net fold must be reversed on emit: {out}"
    );
    assert!(out.contains("\"cache_read_input_tokens\":3100"), "{out}");
    assert!(out.contains("\"cache_creation_input_tokens\":96"), "{out}");
}

#[test]
fn responses_stream_cache_usage_round_trips() {
    let raw = "event: response.created\n\
         data: {\"response\":{\"id\":\"resp_c\",\"model\":\"gpt-5.6\"}}\n\
         \nevent: response.output_text.delta\n\
         data: {\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\
         \nevent: response.completed\n\
         data: {\"response\":{\"id\":\"resp_c\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5000,\"input_tokens_details\":{\"cached_tokens\":100,\"cache_write_tokens\":4800},\"output_tokens\":100}}}";

    let deltas = parse_stream(P::OpenAiResponses, raw);
    let usage = deltas
        .iter()
        .find_map(|d| match d {
            StreamDelta::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("usage delta");
    assert_eq!(usage.prompt_tokens, 5000);
    assert_eq!(usage.cache_read_tokens, Some(100));
    assert_eq!(usage.cache_creation_tokens, Some(4800));
}
