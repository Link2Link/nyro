//! Ported from sub2api `apicompat/chatcompletions_responses_stream_lifecycle_test.go`
//! and `streaming_stop_reason_test.go`.
//!
//! Guards the Responses stream lifecycle invariants Codex clients rely on:
//!
//! * reasoning deltas are emitted only after their `reasoning` output item is
//!   opened (never before);
//! * a reasoning-only turn synthesizes visible text so the client sees a
//!   message item (blank reasoning does not synthesize);
//! * a turn with reasoning then real content must not duplicate the fallback
//!   text; a turn with reasoning then a tool call must not synthesize text;
//! * tool calls are fully closed: `output_item.added` → argument deltas →
//!   `output_item.done` carrying the complete arguments (no duplication);
//! * `max_tokens` truncation surfaces as `response.incomplete` with
//!   `incomplete_details.reason = "max_output_tokens"`; normal end surfaces as
//!   `response.completed` without details.

mod conv_common;

use conv_common::*;
use nyro_core::protocol::codec::openai::responses::stream::ResponsesStreamFormatter;
use nyro_core::protocol::{SseEvent, StreamResponseEncoder};

fn event_jsons(events: &[SseEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| serde_json::from_str(&e.data).ok())
        .collect()
}

fn emit(deltas: &[StreamDelta]) -> Vec<Value> {
    let mut fmt = ResponsesStreamFormatter::new();
    let mut events = fmt.format_deltas(deltas);
    events.extend(fmt.format_done());
    event_jsons(&events)
}

fn reasoning_start() -> StreamDelta {
    StreamDelta::MessageStart {
        id: "resp_life".to_string(),
        model: "gpt-5.2".to_string(),
    }
}

/// Collect the `output_index → item type` map as opened by `output_item.added`.
fn opened_types(jsons: &[Value]) -> std::collections::HashMap<usize, String> {
    jsons
        .iter()
        .filter(|j| j.get("type").and_then(Value::as_str) == Some("response.output_item.added"))
        .filter_map(|j| {
            let idx = j.get("output_index").and_then(Value::as_u64)? as usize;
            let ty = j.pointer("/item/type").and_then(Value::as_str)?.to_string();
            Some((idx, ty))
        })
        .collect()
}

// ── reasoning item opened before its deltas ───────────────────────────────────

#[test]
fn stream_reasoning_opens_item_before_delta() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ThinkingDelta("think".to_string()),
        StreamDelta::TextDelta("hello".to_string()),
        StreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ]);

    let open = opened_types(&jsons);
    for j in &jsons {
        match j.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.reasoning_summary_text.delta" => {
                let idx = j.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                assert_eq!(
                    open.get(&idx).map(String::as_str),
                    Some("reasoning"),
                    "reasoning delta before its item was opened: {jsons:?}"
                );
            }
            "response.output_text.delta" => {
                let idx = j.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                assert_eq!(
                    open.get(&idx).map(String::as_str),
                    Some("message"),
                    "text delta before its item was opened: {jsons:?}"
                );
            }
            _ => {}
        }
    }
}

// ── reasoning-only synthesis ─────────────────────────────────────────────────

#[test]
fn stream_reasoning_only_synthesizes_visible_text() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ThinkingDelta("thinking before final".to_string()),
        StreamDelta::Done {
            stop_reason: "length".to_string(),
        },
    ]);

    let text_deltas: Vec<&str> = jsons
        .iter()
        .filter(|j| j.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
        .filter_map(|j| j.get("delta").and_then(Value::as_str))
        .collect();
    assert_eq!(
        text_deltas,
        vec!["thinking before final"],
        "reasoning-only stream must synthesize a visible text delta: {jsons:?}"
    );

    let completed = jsons
        .iter()
        .find(|j| j.get("type").and_then(Value::as_str) == Some("response.incomplete"))
        .expect("max_tokens truncation must surface as response.incomplete");
    assert_eq!(
        completed
            .pointer("/response/status")
            .and_then(Value::as_str),
        Some("incomplete")
    );
    assert_eq!(
        completed
            .pointer("/response/incomplete_details/reason")
            .and_then(Value::as_str),
        Some("max_output_tokens")
    );
    let output = completed
        .pointer("/response/output")
        .and_then(Value::as_array)
        .expect("output array");
    assert_eq!(output.len(), 2, "{jsons:?}");
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "thinking before final");
}

#[test]
fn stream_reasoning_only_blank_does_not_synthesize() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ThinkingDelta("   ".to_string()),
        StreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ]);

    assert!(
        !jsons
            .iter()
            .any(|j| j.get("type").and_then(Value::as_str) == Some("response.output_text.delta")),
        "blank reasoning must not synthesize visible text: {jsons:?}"
    );
    // Normal end stays completed, still with a (empty) message item so the
    // client can close the turn.
    let completed = jsons
        .iter()
        .find(|j| j.get("type").and_then(Value::as_str) == Some("response.completed"))
        .expect("response.completed");
    assert_eq!(
        completed
            .pointer("/response/status")
            .and_then(Value::as_str),
        Some("completed")
    );
}

#[test]
fn stream_reasoning_then_content_does_not_duplicate_fallback_text() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ThinkingDelta("private plan".to_string()),
        StreamDelta::TextDelta("final answer".to_string()),
        StreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ]);

    let text_deltas: Vec<&str> = jsons
        .iter()
        .filter(|j| j.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
        .filter_map(|j| j.get("delta").and_then(Value::as_str))
        .collect();
    assert_eq!(
        text_deltas,
        vec!["final answer"],
        "real content must not be duplicated by the reasoning fallback: {jsons:?}"
    );

    let completed = jsons
        .iter()
        .find(|j| j.get("type").and_then(Value::as_str) == Some("response.completed"))
        .expect("response.completed");
    let output = completed
        .pointer("/response/output")
        .and_then(Value::as_array)
        .expect("output array");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["text"], "private plan");
    assert_eq!(output[1]["content"][0]["text"], "final answer");
}

#[test]
fn stream_reasoning_then_tool_call_does_not_synthesize_visible_text() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ThinkingDelta("call a tool".to_string()),
        StreamDelta::ToolCallStart {
            index: 0,
            id: "call_a".to_string(),
            name: "exec".to_string(),
            namespace: None,
            kind: ToolKind::Function,
        },
        StreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{}".to_string(),
        },
        StreamDelta::Done {
            stop_reason: "tool_calls".to_string(),
        },
    ]);

    assert!(
        !jsons
            .iter()
            .any(|j| j.get("type").and_then(Value::as_str) == Some("response.output_text.delta")),
        "tool-call turn must not synthesize visible text: {jsons:?}"
    );
    let completed = jsons
        .iter()
        .find(|j| j.get("type").and_then(Value::as_str) == Some("response.completed"))
        .expect("response.completed");
    let output = completed
        .pointer("/response/output")
        .and_then(Value::as_array)
        .expect("output array");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "function_call");
}

// ── tool call lifecycle ──────────────────────────────────────────────────────

#[test]
fn stream_tool_call_lifecycle_complete() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ToolCallStart {
            index: 0,
            id: "call_a".to_string(),
            name: "exec".to_string(),
            namespace: None,
            kind: ToolKind::Function,
        },
        StreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"cmd\":\"ls\"}".to_string(),
        },
        StreamDelta::Done {
            stop_reason: "tool_calls".to_string(),
        },
    ]);

    let added = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.output_item.added")
                && j.pointer("/item/type").and_then(Value::as_str) == Some("function_call")
        })
        .expect("function_call output_item.added missing");
    assert_eq!(added["item"]["call_id"], "call_a");
    assert_eq!(added["item"]["arguments"], "");

    let args_delta = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.function_call_arguments.delta")
        })
        .expect("function_call_arguments.delta missing");
    assert_eq!(args_delta["delta"], "{\"cmd\":\"ls\"}");

    let done = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.output_item.done")
                && j.pointer("/item/type").and_then(Value::as_str) == Some("function_call")
        })
        .expect("function_call output_item.done missing");
    assert_eq!(done["item"]["call_id"], "call_a");
    assert_eq!(done["item"]["arguments"], "{\"cmd\":\"ls\"}");
    assert_eq!(done["item"]["status"], "completed");
}

// A single tool_call delta chunk carrying id+name+arguments together (GLM/Zhipu
// shape) must not double the accumulated arguments.
#[test]
fn stream_tool_call_arguments_in_first_chunk_not_doubled() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::ToolCallStart {
            index: 0,
            id: "call_a".to_string(),
            name: "exec".to_string(),
            namespace: None,
            kind: ToolKind::Function,
        },
        StreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"cmd\":\"ls\"}".to_string(),
        },
        StreamDelta::Done {
            stop_reason: "tool_calls".to_string(),
        },
    ]);

    let accumulated: String = jsons
        .iter()
        .filter(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.function_call_arguments.delta")
        })
        .filter_map(|j| j.get("delta").and_then(Value::as_str))
        .collect();
    assert_eq!(
        accumulated, "{\"cmd\":\"ls\"}",
        "accumulated deltas must equal the final arguments exactly (no duplication): {jsons:?}"
    );
}

// ── terminal state mapping (sub2api streaming_stop_reason_test.go) ───────────

#[test]
fn stream_max_tokens_maps_to_incomplete() {
    // IR vocabulary: the Responses parser maps incomplete/max_output_tokens to
    // `length`; the formatter restores `response.incomplete` on egress.
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::TextDelta("partial".to_string()),
        StreamDelta::Done {
            stop_reason: "length".to_string(),
        },
    ]);

    let incomplete = jsons
        .iter()
        .find(|j| j.get("type").and_then(Value::as_str) == Some("response.incomplete"))
        .expect("max_tokens must map to response.incomplete");
    assert_eq!(
        incomplete
            .pointer("/response/status")
            .and_then(Value::as_str),
        Some("incomplete")
    );
    assert_eq!(
        incomplete
            .pointer("/response/incomplete_details/reason")
            .and_then(Value::as_str),
        Some("max_output_tokens")
    );
    // The partial text must still be carried in the terminal output.
    let output = incomplete
        .pointer("/response/output")
        .and_then(Value::as_array)
        .expect("output array");
    assert!(
        output
            .iter()
            .any(|item| item["content"][0]["text"] == "partial"),
        "{jsons:?}"
    );
}

#[test]
fn stream_end_turn_maps_to_completed() {
    let jsons = emit(&[
        reasoning_start(),
        StreamDelta::TextDelta("done".to_string()),
        StreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ]);

    let completed = jsons
        .iter()
        .find(|j| j.get("type").and_then(Value::as_str) == Some("response.completed"))
        .expect("end_turn must map to response.completed");
    assert_eq!(
        completed
            .pointer("/response/status")
            .and_then(Value::as_str),
        Some("completed")
    );
    assert!(
        completed.pointer("/response/incomplete_details").is_none(),
        "completed must not carry incomplete_details: {jsons:?}"
    );
}

// ── finalize idempotency (sub2api FinalizeAnthropicResponsesStream parity) ───

#[test]
fn format_done_is_idempotent() {
    let mut fmt = ResponsesStreamFormatter::new();
    let events = fmt.format_deltas(&[
        reasoning_start(),
        StreamDelta::TextDelta("x".to_string()),
        StreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ]);
    let done_once = fmt.format_done();
    let done_twice = fmt.format_done();

    let first: Vec<Value> = event_jsons(&events)
        .into_iter()
        .filter(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.completed")
                || j.get("type").and_then(Value::as_str) == Some("response.incomplete")
        })
        .collect();
    let second: Vec<Value> = event_jsons(&done_once)
        .into_iter()
        .filter(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.completed")
                || j.get("type").and_then(Value::as_str) == Some("response.incomplete")
        })
        .collect();
    let third: Vec<Value> = event_jsons(&done_twice)
        .into_iter()
        .filter(|j| {
            j.get("type").and_then(Value::as_str) == Some("response.completed")
                || j.get("type").and_then(Value::as_str) == Some("response.incomplete")
        })
        .collect();

    assert_eq!(first.len(), 1, "terminal event emitted once: {first:?}");
    assert_eq!(
        second.len(),
        0,
        "repeated finalization must be idempotent: {second:?}"
    );
    assert_eq!(third.len(), 0, "repeated finalization must be idempotent");
}
