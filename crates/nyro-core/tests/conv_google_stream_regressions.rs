//! Native Gemini streaming tool-call regressions.
//!
//! Desired-contract assertions for native complete functionCall events. Synthetic
//! inputs are not real-provider recordings; no network is used. Explicit-ID
//! identical complete replays are idempotent, but conflicting snapshots and
//! partial argument dialects are rejected, not concatenated or inferred.
//!
//! Public codecs and the production ToolRoutePlan are exercised here. Private
//! forced-stream aggregation is tested inside proxy/dispatcher/accumulator.rs.
//! Raw-wire cc-switch conversion and dispatcher selection are separate paths.

mod conv_common;

use std::collections::{BTreeMap, HashSet};

use conv_common::*;
use nyro_core::protocol::StreamResponseDecoder;
use nyro_core::protocol::codec::google::gemini::stream::GoogleStreamParser;
use nyro_core::protocol::codec::tool_bridge::ToolRoutePlan;
use nyro_core::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use nyro_core::protocol::ir::error::AiErrorKind;
use nyro_core::protocol::ir::response::ResponseItem;
use serde_json::json;

fn function_part(name: &str, args: Value) -> Value {
    json!({"functionCall": {"name": name, "args": args}})
}

/// One complete SSE event; finishReason belongs only on the terminal event.
fn google_event(parts: Vec<Value>, terminal: bool) -> String {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}});
    if terminal {
        candidate["finishReason"] = json!("STOP");
    }
    format!("data: {}", json!({"candidates": [candidate]}))
}

fn parallel_parts() -> Vec<Value> {
    ["Beijing", "Shanghai", "Guangzhou"]
        .into_iter()
        .map(|city| function_part("get_weather", json!({"city": city})))
        .collect()
}

fn expected_arguments() -> Vec<Value> {
    ["Beijing", "Shanghai", "Guangzhou"]
        .into_iter()
        .map(|city| json!({"city": city}))
        .collect()
}

fn parallel_deltas() -> Vec<StreamDelta> {
    parse_stream(P::GoogleGemini, &google_event(parallel_parts(), true))
}

fn start_indices(deltas: &[StreamDelta]) -> Vec<usize> {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::ToolCallStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect()
}

fn argument_values(deltas: &[StreamDelta]) -> Vec<Value> {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::ToolCallDelta { arguments, .. } => {
                Some(serde_json::from_str(arguments).expect("complete function arguments"))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn parallel_same_name_calls_in_one_event_have_distinct_indices() {
    let deltas = parallel_deltas();
    assert_eq!(start_indices(&deltas), vec![0, 1, 2], "{deltas:?}");
    let argument_indices: Vec<_> = deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::ToolCallDelta { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(argument_indices, vec![0, 1, 2]);
    assert_eq!(argument_values(&deltas), expected_arguments());
    let ids: Vec<_> = deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::ToolCallStart { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 3);
}

#[test]
fn independent_calls_in_separate_events_keep_increasing_indices() {
    let first = google_event(
        vec![function_part("get_weather", json!({"city":"Beijing"}))],
        false,
    );
    let second = google_event(
        vec![function_part("get_weather", json!({"city":"Shanghai"}))],
        false,
    );
    let third = google_event(
        vec![function_part("get_weather", json!({"city":"Guangzhou"}))],
        false,
    );
    let stop = google_event(vec![], true);
    let deltas = parse_stream_chunks(P::GoogleGemini, &[&first, &second, &third, &stop]);
    assert_eq!(start_indices(&deltas), vec![0, 1, 2], "{deltas:?}");
    assert_eq!(argument_values(&deltas), expected_arguments());
    assert_eq!(
        deltas
            .iter()
            .filter(|d| matches!(d, StreamDelta::Done { .. }))
            .count(),
        1
    );
}

#[test]
fn interleaved_parser_instances_do_not_share_tool_indices() {
    let mut a = GoogleStreamParser::new();
    let mut b = GoogleStreamParser::new();
    let event = format!(
        "{}\n\n",
        google_event(vec![function_part("ping", json!({"value":1}))], false)
    );
    assert_eq!(start_indices(&a.parse_chunk(&event).unwrap()), vec![0]);
    assert_eq!(start_indices(&b.parse_chunk(&event).unwrap()), vec![0]);
    assert_eq!(start_indices(&a.parse_chunk(&event).unwrap()), vec![1]);
    assert_eq!(start_indices(&b.parse_chunk(&event).unwrap()), vec![1]);
}

#[test]
fn chat_sse_reassembles_three_independent_valid_argument_objects() {
    let events = sse_jsons(&format_stream(P::OpenAiChat, &parallel_deltas()));
    let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    for event in &events {
        let Some(tool_calls) = event["choices"][0]["delta"]["tool_calls"].as_array() else {
            continue;
        };
        for tc in tool_calls {
            let index = tc["index"].as_u64().expect("wire index") as usize;
            let call = calls.entry(index).or_default();
            if let Some(id) = tc["id"].as_str() {
                assert!(
                    call.0.is_empty(),
                    "second call starts on index {index}: {events:?}"
                );
                call.0 = id.to_string();
            }
            if let Some(name) = tc["function"]["name"].as_str() {
                call.1.push_str(name);
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                call.2.push_str(args);
            }
        }
    }
    assert_eq!(calls.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2]);
    assert!(
        calls
            .values()
            .all(|(id, name, _)| !id.is_empty() && name == "get_weather")
    );
    let args: Vec<Value> = calls
        .values()
        .map(|(_, _, args)| serde_json::from_str(args).expect("valid call JSON"))
        .collect();
    assert_eq!(args, expected_arguments());
}

// Negative controls guard against the earlier overclaim that every formatter
// drops complete calls on repeated input indices. Assert semantics, not today's
// bad indices: these controls must continue to pass after parser repair.
#[test]
fn complete_parallel_calls_survive_gemini_stream_formatting() {
    let events = sse_jsons(&format_stream(P::GoogleGemini, &parallel_deltas()));
    let calls: Vec<_> = events
        .iter()
        .flat_map(|event| {
            event["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|part| part.get("functionCall"))
        .collect();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call["name"] == "get_weather"));
    assert_eq!(
        calls
            .iter()
            .map(|call| call["args"].clone())
            .collect::<Vec<_>>(),
        expected_arguments()
    );
}

#[test]
fn complete_parallel_calls_survive_anthropic_stream_formatting() {
    let events = sse_jsons(&format_stream(P::AnthropicMessages, &parallel_deltas()));
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    for event in &events {
        if event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use" {
            let index = event["index"].as_u64().unwrap();
            assert!(
                calls
                    .insert(
                        index,
                        (
                            event["content_block"]["id"].as_str().unwrap().to_string(),
                            event["content_block"]["name"].as_str().unwrap().to_string(),
                            String::new(),
                        )
                    )
                    .is_none(),
                "duplicate content block: {events:?}"
            );
        } else if event["type"] == "content_block_delta"
            && event["delta"]["type"] == "input_json_delta"
        {
            calls
                .get_mut(&event["index"].as_u64().unwrap())
                .expect("started tool block")
                .2
                .push_str(event["delta"]["partial_json"].as_str().unwrap());
        }
    }
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .values()
            .all(|(id, name, _)| !id.is_empty() && name == "get_weather")
    );
    let args: Vec<Value> = calls
        .values()
        .map(|(_, _, args)| serde_json::from_str(args).unwrap())
        .collect();
    assert_eq!(args, expected_arguments());
}

#[test]
fn complete_parallel_functions_survive_responses_stream_formatting() {
    let events = sse_jsons(&format_stream(P::OpenAiResponses, &parallel_deltas()));
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("terminal response");
    let calls: Vec<_> = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    assert_eq!(calls.len(), 3);
    let ids: HashSet<_> = calls
        .iter()
        .map(|call| call["call_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(calls.iter().all(|call| call["name"] == "get_weather"));
    let args: Vec<Value> = calls
        .iter()
        .map(|call| serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(args, expected_arguments());
}

#[test]
fn custom_tool_bridge_keeps_each_parallel_input_with_its_call() {
    let mut request = decode_request(
        P::OpenAiResponses,
        json!({
            "model":"gemini-review", "input":"run two commands", "stream":true,
            "tools":[{"type":"custom","name":"exec","description":"Run text"}]
        }),
    );
    let mut plan = ToolRoutePlan::for_request(&request, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    assert!(plan.is_active());
    plan.prepare_upstream_request(&mut request);
    let wire_tool = &request.tools.as_ref().unwrap()[0];
    assert!(!wire_tool.is_custom());
    let wire_name = wire_tool.name.clone();
    let event = google_event(
        vec![
            function_part(&wire_name, json!({"input":"echo A"})),
            function_part(&wire_name, json!({"input":"echo B"})),
        ],
        true,
    );
    let mut deltas = plan.restore_stream_deltas(parse_stream(P::GoogleGemini, &event));
    deltas.extend(plan.finish_stream());
    let events = sse_jsons(&format_stream(P::OpenAiResponses, &deltas));
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("terminal response");
    let calls: Vec<_> = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "custom_tool_call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call["name"] == "exec"));
    assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
    assert_eq!(
        calls
            .iter()
            .map(|call| call["input"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["echo A", "echo B"],
        "{completed}"
    );
}

#[test]
fn zero_argument_function_survives_gemini_stream_reencoding_once() {
    let event = google_event(vec![function_part("ping", json!({}))], true);
    let deltas = parse_stream(P::GoogleGemini, &event);
    assert_eq!(start_indices(&deltas), vec![0]);
    let args: Vec<_> = deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::ToolCallDelta { index, arguments } => Some((*index, arguments.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(args, vec![(0, "{}")]);
    let events = sse_jsons(&format_stream(P::GoogleGemini, &deltas));
    let calls: Vec<_> = events
        .iter()
        .flat_map(|event| {
            event["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|part| part.get("functionCall"))
        .collect();
    assert_eq!(calls.len(), 1, "zero-argument call disappeared: {events:?}");
    assert_eq!(calls[0]["name"], "ping");
    assert_eq!(calls[0]["args"], json!({}));
}

fn identified_part(id: &str, name: &str, args: Value) -> Value {
    json!({"functionCall": {"id": id, "name": name, "args": args}})
}

fn started_calls(deltas: &[StreamDelta]) -> Vec<(usize, &str, &str)> {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            StreamDelta::ToolCallStart {
                index, id, name, ..
            } => Some((*index, id.as_str(), name.as_str())),
            _ => None,
        })
        .collect()
}

fn assert_no_executable_call(deltas: &[StreamDelta]) {
    assert!(
        !deltas.iter().any(|delta| matches!(
            delta,
            StreamDelta::ToolCallStart { .. }
                | StreamDelta::ToolCallDelta { .. }
                | StreamDelta::ToolCallComplete { .. }
        )),
        "malformed input became a tool call: {deltas:?}"
    );
    let events = sse_jsons(&format_stream(P::GoogleGemini, deltas));
    assert!(
        events.iter().all(|event| {
            event["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|part| part.get("functionCall").is_none())
        }),
        "malformed input was reencoded as a functionCall: {events:?}"
    );
}

#[test]
fn raw_sse_transport_splits_preserve_call_identity_and_delta_indices() {
    // Use the direct parser, not parse_stream_chunks (which adds event framing).
    // Every split includes splits inside data:, args JSON, and the \n\n delimiter.
    let raw = format!(
        "{}\n\n{}\n\n{}\n\n",
        google_event(
            vec![identified_part("provided-a", "ping", json!({}))],
            false
        ),
        google_event(
            vec![identified_part("provided-b", "ping", json!({"n":2}))],
            false
        ),
        google_event(vec![], true),
    );
    assert!(raw.is_ascii());
    for split in 0..=raw.len() {
        let mut parser = GoogleStreamParser::new();
        let mut deltas = parser.parse_chunk(&raw[..split]).unwrap();
        deltas.extend(parser.parse_chunk(&raw[split..]).unwrap());
        deltas.extend(parser.finish().unwrap());
        assert_eq!(
            started_calls(&deltas),
            vec![(0, "provided-a", "ping"), (1, "provided-b", "ping")],
            "split={split}: {deltas:?}"
        );
        let args: Vec<_> = deltas
            .iter()
            .filter_map(|delta| match delta {
                StreamDelta::ToolCallDelta { index, arguments } => {
                    Some((*index, arguments.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(args, vec![(0, "{}"), (1, "{\"n\":2}")]);
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, StreamDelta::StreamError { .. }))
        );
    }
    let mut parser = GoogleStreamParser::new();
    let mut deltas = Vec::new();
    for byte in raw.as_bytes().chunks(1) {
        deltas.extend(
            parser
                .parse_chunk(std::str::from_utf8(byte).unwrap())
                .unwrap(),
        );
    }
    deltas.extend(parser.finish().unwrap());
    assert_eq!(start_indices(&deltas), vec![0, 1]);
    assert_eq!(argument_values(&deltas), vec![json!({}), json!({"n":2})]);
}

#[test]
fn upstream_ids_are_preserved_in_stream_and_nonstream_ir() {
    // Whitespace in a nonempty upstream ID is not silently trimmed or replaced.
    let parts = vec![
        identified_part("upstream-1", "ping", json!({})),
        identified_part(" opaque-id ", "ping", json!({"n":2})),
        identified_part("", "ping", json!({"n":3})),
        function_part("ping", json!({"n":4})),
    ];
    let deltas = parse_stream(P::GoogleGemini, &google_event(parts.clone(), true));
    let starts = started_calls(&deltas);
    assert_eq!(starts[0], (0, "upstream-1", "ping"));
    assert_eq!(starts[1], (1, " opaque-id ", "ping"));
    assert!(starts[2].1.starts_with("call_"));
    assert!(starts[3].1.starts_with("call_"));
    assert_ne!(starts[2].1, starts[3].1);
    let response = parse_response(
        P::GoogleGemini,
        json!({"candidates":[{"content":{"parts":parts}, "finishReason":"STOP"}]}),
    );
    assert_eq!(response.tool_calls.len(), 4);
    assert_eq!(response.tool_calls[0].id, "upstream-1");
    assert_eq!(response.tool_calls[1].id, " opaque-id ");
    assert!(response.tool_calls[2].id.starts_with("call_"));
    assert!(response.tool_calls[3].id.starts_with("call_"));
    let item_ids: Vec<_> = response
        .items
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        item_ids,
        response
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn identical_no_id_complete_calls_remain_independent() {
    let part = function_part("ping", json!({}));
    let first = google_event(vec![part.clone(), part.clone()], false);
    let second = google_event(vec![part], true);
    let deltas = parse_stream_chunks(P::GoogleGemini, &[&first, &second]);
    assert_eq!(start_indices(&deltas), vec![0, 1, 2]);
    assert_eq!(argument_values(&deltas), vec![json!({}); 3]);
    assert_eq!(
        started_calls(&deltas)
            .iter()
            .map(|(_, id, _)| id)
            .collect::<HashSet<_>>()
            .len(),
        3
    );
}

#[test]
fn exact_explicit_id_replays_do_not_duplicate_calls_or_arguments() {
    let mut part = identified_part("repeat-id", "ping", json!({"n":1}));
    part["thoughtSignature"] = json!("SIGNED-CALL");
    let first = google_event(vec![part.clone(), part.clone()], false);
    let replay = google_event(
        vec![part, identified_part("next-id", "ping", json!({}))],
        true,
    );
    let deltas = parse_stream_chunks(P::GoogleGemini, &[&first, &replay]);
    assert_eq!(
        started_calls(&deltas),
        vec![(0, "repeat-id", "ping"), (1, "next-id", "ping")]
    );
    assert_eq!(argument_values(&deltas), vec![json!({"n":1}), json!({})]);
    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, StreamDelta::StreamError { .. }))
    );
    assert!(
        deltas.iter().any(
            |delta| matches!(delta, StreamDelta::ThinkingSignature(sig) if sig == "SIGNED-CALL")
        )
    );
    let responses = sse_jsons(&format_stream(P::OpenAiResponses, &deltas));
    let completed = responses
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let calls = completed["response"]["output"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{completed}");
    assert_eq!(calls[0]["call_id"], "repeat-id");
    assert_eq!(calls[0]["arguments"], "{\"n\":1}");
    assert_eq!(calls[1]["call_id"], "next-id");
    assert_eq!(calls[1]["arguments"], "{}");
}

#[test]
fn explicit_id_snapshot_tracking_is_local_to_each_parser() {
    let mut a = GoogleStreamParser::new();
    let mut b = GoogleStreamParser::new();
    let event_a = format!(
        "{}\n\n",
        google_event(
            vec![identified_part("shared-id", "ping", json!({"n":1}))],
            false
        )
    );
    let event_b = format!(
        "{}\n\n",
        google_event(
            vec![identified_part("shared-id", "ping", json!({"n":2}))],
            false
        )
    );
    assert_eq!(start_indices(&a.parse_chunk(&event_a).unwrap()), vec![0]);
    let deltas_b = b.parse_chunk(&event_b).unwrap();
    assert_eq!(start_indices(&deltas_b), vec![0]);
    assert_eq!(argument_values(&deltas_b), vec![json!({"n":2})]);
    assert!(
        !deltas_b
            .iter()
            .any(|delta| matches!(delta, StreamDelta::StreamError { .. }))
    );
    assert!(started_calls(&a.parse_chunk(&event_a).unwrap()).is_empty());
    assert!(started_calls(&b.parse_chunk(&event_b).unwrap()).is_empty());
}

#[test]
fn conflicting_same_id_complete_payload_is_a_terminal_stream_error() {
    for conflict in [
        identified_part("same-id", "ping", json!({"n":2})),
        identified_part("same-id", "different_name", json!({"n":1})),
    ] {
        let mut parser = GoogleStreamParser::new();
        let first = format!(
            "{}\n\n",
            google_event(
                vec![identified_part("same-id", "ping", json!({"n":1}))],
                false
            )
        );
        let accepted = parser.parse_chunk(&first).unwrap();
        assert_eq!(started_calls(&accepted), vec![(0, "same-id", "ping")]);
        assert_eq!(argument_values(&accepted), vec![json!({"n":1})]);
        let conflicting = format!(
            "{}\n\n",
            google_event(
                vec![conflict, identified_part("later-id", "ping", json!({}))],
                true
            )
        );
        let rejected = parser.parse_chunk(&conflicting).unwrap();
        assert_no_executable_call(&rejected);
        let errors: Vec<_> = rejected
            .iter()
            .filter_map(|delta| match delta {
                StreamDelta::StreamError { error } => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "{rejected:?}");
        assert_eq!(errors[0].kind, AiErrorKind::StreamMidError);
        assert!(
            errors[0]
                .message
                .contains("Conflicting complete Gemini functionCall")
        );
        assert!(errors[0].message.contains("same-id"));
        assert!(
            !rejected
                .iter()
                .any(|delta| matches!(delta, StreamDelta::Done { .. }))
        );
        assert!(parser.parse_chunk(&first).unwrap().is_empty());
        assert!(parser.finish().unwrap().is_empty());
    }
}

#[test]
fn absent_or_nonobject_args_are_errors_not_executable_zero_argument_calls() {
    let invalid = [
        json!({"functionCall":{"name":"ping"}}),
        function_part("ping", Value::Null),
        function_part("ping", json!("{}")),
        function_part("ping", json!("{\"n\":")),
        function_part("ping", json!([])),
        function_part("ping", json!(7)),
        function_part("ping", json!(true)),
    ];
    for part in invalid {
        let mut parser = GoogleStreamParser::new();
        let deltas = parser
            .parse_chunk(&format!("{}\n\n", google_event(vec![part.clone()], true)))
            .unwrap();
        assert_no_executable_call(&deltas);
        assert!(deltas.iter().any(|delta| matches!(delta, StreamDelta::StreamError { error }
            if error.kind == AiErrorKind::StreamMidError && error.message.contains("functionCall.args"))), "{part}: {deltas:?}");
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, StreamDelta::Done { .. }))
        );
        assert!(
            parser
                .parse_chunk(&format!(
                    "{}\n\n",
                    google_event(vec![function_part("ping", json!({}))], true)
                ))
                .unwrap()
                .is_empty()
        );
        assert!(parser.finish().unwrap().is_empty());
    }
}

#[test]
fn truncated_or_malformed_json_never_synthesizes_a_call_at_finish() {
    let truncated = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"ping\",\"args\":{\"n\":";
    for raw in [truncated.to_string(), format!("{truncated}\n\n")] {
        let mut parser = GoogleStreamParser::new();
        let mut deltas = parser.parse_chunk(&raw).unwrap();
        deltas.extend(parser.finish().unwrap());
        assert_no_executable_call(&deltas);
        assert!(
            deltas
                .iter()
                .any(|delta| matches!(delta, StreamDelta::StreamError { error }
            if error.kind == AiErrorKind::UnexpectedEof)),
            "{deltas:?}"
        );
        assert!(parser.finish().unwrap().is_empty());
    }
    let mut parser = GoogleStreamParser::new();
    let malformed = format!("{truncated}!}}}}}}}}]}}}}]}}\n\n");
    let deltas = parser.parse_chunk(&malformed).unwrap();
    assert_no_executable_call(&deltas);
    assert!(
        deltas
            .iter()
            .any(|delta| matches!(delta, StreamDelta::StreamError { error }
        if error.kind == AiErrorKind::StreamMidError)),
        "{deltas:?}"
    );
    assert!(parser.finish().unwrap().is_empty());
}

#[test]
fn complete_json_without_final_sse_delimiter_is_not_truncated() {
    let mut parser = GoogleStreamParser::new();
    let event = google_event(vec![function_part("ping", json!({}))], true);
    assert!(parser.parse_chunk(&event).unwrap().is_empty());
    let deltas = parser.finish().unwrap();
    assert_eq!(start_indices(&deltas), vec![0]);
    assert_eq!(argument_values(&deltas), vec![json!({})]);
    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, StreamDelta::StreamError { .. }))
    );
}

#[test]
fn invalid_function_name_never_starts_an_executable_call() {
    for fc in [
        json!({"args":{}}),
        json!({"name":"", "args":{}}),
        json!({"name":42, "args":{}}),
        Value::Null,
    ] {
        let event = google_event(vec![json!({"functionCall":fc})], true);
        let deltas = parse_stream(P::GoogleGemini, &event);
        assert_no_executable_call(&deltas);
        assert!(deltas.iter().any(|delta| matches!(delta,
            StreamDelta::StreamError { error } if error.message.contains("functionCall.name"))));
    }
}

#[test]
fn explicit_empty_arguments_survive_chat_anthropic_and_responses() {
    let deltas = parse_stream(
        P::GoogleGemini,
        &google_event(
            vec![identified_part("zero-arg-id", "ping", json!({}))],
            true,
        ),
    );
    let chat = sse_jsons(&format_stream(P::OpenAiChat, &deltas));
    let calls: Vec<_> = chat
        .iter()
        .flat_map(|event| {
            event["choices"][0]["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["id"] == "zero-arg-id")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter_map(|call| call["function"]["arguments"].as_str())
            .collect::<String>(),
        "{}"
    );

    let anthropic = sse_jsons(&format_stream(P::AnthropicMessages, &deltas));
    assert_eq!(
        anthropic
            .iter()
            .filter(|event| event["content_block"]["type"] == "tool_use")
            .count(),
        1
    );
    assert_eq!(
        anthropic
            .iter()
            .filter_map(|event| event["delta"]["partial_json"].as_str())
            .collect::<String>(),
        "{}"
    );

    let responses = sse_jsons(&format_stream(P::OpenAiResponses, &deltas));
    let completed = responses
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let calls = completed["response"]["output"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{completed}");
    assert_eq!(calls[0]["call_id"], "zero-arg-id");
    assert_eq!(calls[0]["name"], "ping");
    assert_eq!(calls[0]["arguments"], "{}");
}

#[test]
fn normal_text_deltas_are_not_mistaken_for_cumulative_snapshots() {
    let first = google_event(vec![json!({"text":"same"})], false);
    let second = google_event(vec![json!({"text":"same"})], true);
    let deltas = parse_stream_chunks(P::GoogleGemini, &[&first, &second]);
    assert_eq!(delta_text(&deltas), "samesame");
}
