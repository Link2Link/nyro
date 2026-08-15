//! Ported from sub2api `apicompat/responses_to_anthropic_tool_pairing_test.go`.
//!
//! Enforces the Anthropic Messages tool-pairing invariants on the egress wire
//! after converting a Responses API request: every `tool_use` must be answered
//! by a `tool_result` in the immediately following message, and every
//! `tool_result` must be preceded by a matching `tool_use`. Violations surface
//! as upstream 400s, so the converter must repair (drop) unpaired items.
//!
//! Pairing is verified on the encoded Anthropic request body (the actual wire
//! Anthropic validates), mirroring sub2api's `assertAnthropicPairing`.

mod conv_common;

use conv_common::*;
use serde_json::Value;

/// The Anthropic encoder rewrites client `call_*` ids to `toolu_<id>` on the
/// wire; accept both forms when matching.
fn matches_tool_id(wire_id: &str, expected: &str) -> bool {
    wire_id == expected || wire_id == format!("toolu_{expected}")
}

fn tool_use_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| b.get("id").and_then(Value::as_str).map(String::from))
        .collect()
}

fn tool_result_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| {
            b.get("tool_use_id")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect()
}

fn blocks_of(msg: &Value) -> Vec<Value> {
    msg.get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Enforce the Anthropic tool-pairing invariants on an encoded request body.
/// Panics on violation, mirroring the upstream 400 rejection.
fn assert_anthropic_pairing(body: &Value) {
    let messages = body
        .pointer("/messages")
        .and_then(Value::as_array)
        .expect("request must have a messages array");

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let blocks = blocks_of(msg);

        // No two consecutive same-role messages.
        if i > 0 {
            let prev_role = messages[i - 1]
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("");
            assert_ne!(
                prev_role, role,
                "consecutive {role} messages at index {i}: {messages:?}"
            );
        }

        for b in &blocks {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "tool_result" => {
                    let id = b
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .expect("tool_result missing tool_use_id");
                    assert!(i > 0, "tool_result {id} has no previous message");
                    let prev_uses = tool_use_ids(&blocks_of(&messages[i - 1]));
                    assert!(
                        prev_uses.iter().any(|u| matches_tool_id(u, id)),
                        "tool_result {id} has no corresponding tool_use in the previous message: {messages:?}"
                    );
                }
                "tool_use" => {
                    let id = b
                        .get("id")
                        .and_then(Value::as_str)
                        .expect("tool_use missing id");
                    assert!(
                        i + 1 < messages.len(),
                        "tool_use {id} has no following message: {messages:?}"
                    );
                    let next_ids = tool_result_ids(&blocks_of(&messages[i + 1]));
                    assert!(
                        next_ids.iter().any(|r| matches_tool_id(r, id)),
                        "tool_use {id} is not answered in the next message: {messages:?}"
                    );
                }
                _ => {}
            }
        }
    }
}

fn convert_to_anthropic(input: Value) -> Value {
    let req = decode_request(P::OpenAiResponses, input);
    let body = encode_request(P::AnthropicMessages, &req);
    assert_anthropic_pairing(&body);
    body
}

// ── A developer/approval message injected between a function_call and its
// output must be moved out of the tool_use→tool_result adjacency. This is the
// shape that produced the production 400 "tool_result ... must have a
// corresponding tool_use block in the previous message".
#[test]
fn anthropic_pairing_developer_message_between() {
    let body = convert_to_anthropic(json!({
        "model": "gpt-5.2",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"do it"}]},
            {"type":"function_call","call_id":"call_A","name":"exec","arguments":"{}"},
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"Approved command prefix saved"}]},
            {"type":"function_call_output","call_id":"call_A","output":"ok"}
        ]
    }));

    let messages = body.pointer("/messages").unwrap().as_array().unwrap();
    for (i, m) in messages.iter().enumerate() {
        let uses = tool_use_ids(&blocks_of(m));
        if uses.iter().any(|u| matches_tool_id(u, "call_A")) {
            assert_eq!(messages[i + 1]["role"].as_str(), Some("user"));
            let results = tool_result_ids(&blocks_of(&messages[i + 1]));
            assert!(
                results.iter().any(|r| matches_tool_id(r, "call_A")),
                "tool_use call_A must be immediately followed by its tool_result: {messages:?}"
            );
        }
    }
}

// ── Parallel tool calls where both outputs arrive stay grouped: one assistant
// message with both tool_use blocks, the next user message with both results.
#[test]
fn anthropic_pairing_parallel_both_answered() {
    let body = convert_to_anthropic(json!({
        "model": "gpt-5.2",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"features?"}]},
            {"type":"function_call","call_id":"call_c0","name":"exec","arguments":"{}"},
            {"type":"function_call","call_id":"call_c1","name":"exec","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_c0","output":"log"},
            {"type":"function_call_output","call_id":"call_c1","output":"tags"}
        ]
    }));

    let messages = body.pointer("/messages").unwrap().as_array().unwrap();
    let saw_grouped = messages.iter().any(|m| {
        let uses = tool_use_ids(&blocks_of(m));
        uses.iter().any(|u| matches_tool_id(u, "call_c0"))
            && uses.iter().any(|u| matches_tool_id(u, "call_c1"))
    });
    assert!(
        saw_grouped,
        "parallel tool_use blocks should share one assistant message: {messages:?}"
    );
}

// ── A parallel call whose sibling output never arrived must be dropped so
// every remaining tool_use is answered.
#[test]
fn anthropic_pairing_parallel_one_unanswered() {
    let body = convert_to_anthropic(json!({
        "model": "gpt-5.2",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
            {"type":"function_call","call_id":"call_A","name":"exec","arguments":"{}"},
            {"type":"function_call","call_id":"call_B","name":"exec","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_A","output":"oa"}
        ]
    }));

    let messages = body.pointer("/messages").unwrap().as_array().unwrap();
    for m in messages {
        let uses = tool_use_ids(&blocks_of(m));
        assert!(
            !uses.iter().any(|u| matches_tool_id(u, "call_B")),
            "unanswered tool_use call_B should have been dropped: {messages:?}"
        );
    }
}

// ── An orphan tool_result whose tool_use was never announced must be dropped.
#[test]
fn anthropic_pairing_orphan_tool_result_dropped() {
    let body = convert_to_anthropic(json!({
        "model": "gpt-5.2",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
            {"type":"function_call_output","call_id":"call_ghost","output":"orphan"}
        ]
    }));

    let messages = body.pointer("/messages").unwrap().as_array().unwrap();
    for m in messages {
        let results = tool_result_ids(&blocks_of(m));
        assert!(
            !results.iter().any(|r| matches_tool_id(r, "call_ghost")),
            "orphan tool_result should have been dropped: {messages:?}"
        );
    }
}

// ── A dangling tool_call at the end of the history (no output yet) drops the
// assistant message holding only that call, leaving no tool_use behind.
#[test]
fn anthropic_pairing_dangling_call_dropped() {
    let body = convert_to_anthropic(json!({
        "model": "gpt-5.2",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
            {"type":"function_call","call_id":"call_A","name":"exec","arguments":"{}"}
        ]
    }));

    let messages = body.pointer("/messages").unwrap().as_array().unwrap();
    for m in messages {
        let uses = tool_use_ids(&blocks_of(m));
        assert!(
            !uses.iter().any(|u| matches_tool_id(u, "call_A")),
            "dangling tool_use call_A should have been dropped: {messages:?}"
        );
    }
}

// ── Baseline: a single answered call pairs correctly and preserves the
// surrounding turns.
#[test]
fn anthropic_pairing_single_call() {
    let body = convert_to_anthropic(json!({
        "model": "gpt-5.2",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"latest sha?"}]},
            {"type":"function_call","call_id":"call_A","name":"exec","arguments":"{\"cmd\":\"git rev-parse HEAD\"}"},
            {"type":"function_call_output","call_id":"call_A","output":"deadbeef"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"It is deadbeef."}]}
        ]
    }));

    let messages = body.pointer("/messages").unwrap().as_array().unwrap();
    // user, assistant(tool_use), user(tool_result), assistant(text)
    assert!(messages.len() >= 4, "expected at least 4 messages: {messages:?}");
    assert_eq!(messages[0]["role"].as_str(), Some("user"));
    assert!(
        tool_use_ids(&blocks_of(&messages[1]))
            .iter()
            .any(|u| matches_tool_id(u, "call_A"))
    );
    assert!(
        tool_result_ids(&blocks_of(&messages[2]))
            .iter()
            .any(|r| matches_tool_id(r, "call_A"))
    );
}
