//! Public CompatEngine contract for ordinary Gemini complete functionCall SSE.
//!
//! Synthetic fixtures, not provider recordings. A shared responseId, array
//! position, function name, or growing argument object is not call identity.
//! Only an explicit ID plus an identical complete name/args payload proves a
//! replay. Partial argument dialects and conflicting identified calls fail
//! rather than silently replacing content or fabricating executable arguments.

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use futures::{StreamExt, stream};
use nyro_ccswitch_compat::{
    CompatEngine, ConversionProfile, ResponseBody, ResponseMetadata, SessionIdentity, SessionSource,
};
use serde_json_ordered::{Value, json};

const PROVIDER: &str = "gemini-tool-contract";
const SESSION: &str = "gemini-tool-session";

fn identity() -> SessionIdentity {
    SessionIdentity {
        value: SESSION.to_string(),
        source: SessionSource::Header,
        client_provided: true,
    }
}

fn profile() -> ConversionProfile {
    ConversionProfile::anthropic_to_gemini(true).with_provider_id(PROVIDER.to_string())
}

fn function(name: &str, args: Value) -> Value {
    json!({"functionCall": {"name": name, "args": args}})
}

fn identified(id: &str, name: &str, args: Value) -> Value {
    json!({"functionCall": {"id": id, "name": name, "args": args}})
}

fn event(parts: Vec<Value>) -> String {
    format!(
        "data: {}\n\n",
        json!({"responseId": "shared-response", "candidates": [{"content": {"parts": parts}}]})
    )
}

fn wire(events: Vec<Vec<Value>>) -> String {
    let mut wire: String = events.into_iter().map(event).collect();
    wire.push_str("data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n");
    wire
}

struct Outcome {
    engine: CompatEngine,
    events: Vec<Value>,
    errors: Vec<String>,
}

impl Outcome {
    fn calls(&self) -> Vec<Value> {
        let mut calls = BTreeMap::new();
        for event in &self.events {
            let index = event["index"].as_u64().unwrap_or_default();
            if event["type"] == "content_block_start"
                && event["content_block"]["type"] == "tool_use"
            {
                assert!(
                    calls
                        .insert(index, event["content_block"].clone())
                        .is_none()
                );
            } else if event["delta"]["type"] == "input_json_delta" {
                calls.get_mut(&index).expect("started tool block")["input"] =
                    serde_json_ordered::from_str(event["delta"]["partial_json"].as_str().unwrap())
                        .expect("complete argument object");
            }
        }
        calls.into_values().collect()
    }

    fn assert_success(&self, expected: &[(&str, Value)]) -> Vec<Value> {
        assert!(self.errors.is_empty(), "{:?}", self.errors);
        let calls = self.calls();
        assert_eq!(calls.len(), expected.len(), "{:?}", self.events);
        for (call, (name, args)) in calls.iter().zip(expected) {
            assert_eq!(call["name"], *name);
            assert_eq!(call["input"], *args);
            assert!(!call["id"].as_str().unwrap().is_empty());
        }
        assert_eq!(
            calls
                .iter()
                .map(|call| call["id"].as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            calls.len()
        );
        assert_eq!(
            self.events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1
        );
        assert!(
            self.events
                .iter()
                .any(|event| event["delta"]["stop_reason"] == "tool_use")
        );
        calls
    }

    fn assert_failure(&self, diagnostic: &str) {
        assert_eq!(self.errors.len(), 1, "{:?}", self.errors);
        assert!(self.errors[0].contains(diagnostic), "{:?}", self.errors);
        assert!(
            self.calls().is_empty(),
            "failed calls must not become executable"
        );
        assert!(
            !self
                .events
                .iter()
                .any(|event| event["type"] == "message_stop")
        );
        assert_eq!(self.engine.state().gemini_session_count(), 0);
    }

    // Follow-up preparation is the public way to observe shadow signature
    // ownership; no private store API is exposed just for these assertions.
    fn replay_parts(&self) -> Vec<Value> {
        let calls = self.calls();
        let results: Vec<_> = calls
            .iter()
            .map(|call| {
                json!({
                    "type": "tool_result", "tool_use_id": call["id"], "content": "ok"
                })
            })
            .collect();
        let body = json!({
            "model": "gemini-3-flash", "max_tokens": 100, "stream": true,
            "messages": [
                {"role": "user", "content": "do both"},
                {"role": "assistant", "content": calls},
                {"role": "user", "content": results}
            ]
        });
        let request = futures::executor::block_on(self.engine.prepare_request(
            profile(),
            Bytes::from(serde_json_ordered::to_vec(&body).unwrap()),
            identity(),
        ))
        .unwrap();
        let body: Value = serde_json_ordered::from_slice(&request.body).unwrap();
        body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|content| content["role"] == "model")
            .unwrap()["parts"]
            .as_array()
            .unwrap()
            .clone()
    }
}

fn run_chunks(chunks: Vec<Bytes>) -> Outcome {
    let engine = CompatEngine::default();
    let prepared = futures::executor::block_on(engine.prepare_request(
        profile(),
        Bytes::from_static(br#"{"model":"gemini-3-flash","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"do both"}]}"#),
        identity(),
    )).unwrap();
    let response = engine
        .convert_stream_response(
            &prepared.session,
            ResponseMetadata::new(200),
            stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)),
        )
        .unwrap();
    let ResponseBody::Stream(converted) = response.body else {
        panic!("expected stream")
    };
    let mut errors = Vec::new();
    let mut output = Vec::new();
    for item in futures::executor::block_on(converted.collect::<Vec<_>>()) {
        match item {
            Ok(bytes) => output.extend_from_slice(&bytes),
            Err(error) => errors.push(error.to_string()),
        }
    }
    let output = String::from_utf8(output).unwrap();
    let events = output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json_ordered::from_str(data).unwrap())
        .collect();
    Outcome {
        engine,
        events,
        errors,
    }
}

fn run(events: Vec<Vec<Value>>) -> Outcome {
    run_chunks(vec![Bytes::from(wire(events))])
}

#[test]
fn independent_same_name_calls_in_separate_events_keep_both_arguments() {
    run(vec![
        vec![function("read", json!({"file":"a"}))],
        vec![function("read", json!({"file":"b"}))],
    ])
    .assert_success(&[("read", json!({"file":"a"})), ("read", json!({"file":"b"}))]);
}

#[test]
fn independent_different_names_and_empty_ids_are_not_positional_updates() {
    run(vec![
        vec![identified("", "read", json!({"file":"a"}))],
        vec![function("write", json!({"file":"b"}))],
    ])
    .assert_success(&[
        ("read", json!({"file":"a"})),
        ("write", json!({"file":"b"})),
    ]);
}

#[test]
fn identical_no_id_calls_are_independent_within_and_across_events() {
    let part = function("ping", json!({}));
    run(vec![vec![part.clone(), part.clone()], vec![part]]).assert_success(&[
        ("ping", json!({})),
        ("ping", json!({})),
        ("ping", json!({})),
    ]);
}

#[test]
fn parallel_no_id_calls_and_later_call_preserve_order_and_signatures() {
    let mut first = function("read", json!({"file":"a"}));
    first["thoughtSignature"] = json!("signature-a");
    let mut third = function("write", json!({"file":"c"}));
    third["thought_signature"] = json!("signature-c");
    let outcome = run(vec![
        vec![first, function("read", json!({"file":"b"}))],
        vec![third],
    ]);
    outcome.assert_success(&[
        ("read", json!({"file":"a"})),
        ("read", json!({"file":"b"})),
        ("write", json!({"file":"c"})),
    ]);
    let parts = outcome.replay_parts();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["thoughtSignature"], "signature-a");
    assert!(parts[1].get("thoughtSignature").is_none());
    assert_eq!(parts[2]["thoughtSignature"], "signature-c");
}

#[test]
fn later_independent_no_id_call_does_not_inherit_a_signature() {
    let mut first = function("read", json!({"file":"a"}));
    first["thoughtSignature"] = json!("signature-a");
    let outcome = run(vec![
        vec![first],
        vec![function("read", json!({"file":"b"}))],
    ]);
    outcome.assert_success(&[("read", json!({"file":"a"})), ("read", json!({"file":"b"}))]);
    let parts = outcome.replay_parts();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["functionCall"]["args"], json!({"file":"a"}));
    assert_eq!(parts[0]["thoughtSignature"], "signature-a");
    assert_eq!(parts[1]["functionCall"]["args"], json!({"file":"b"}));
    assert!(parts[1].get("thoughtSignature").is_none());
}

#[test]
fn new_explicit_id_is_not_an_upgrade_of_a_previous_no_id_call() {
    let mut first = function("read", json!({"file":"a"}));
    first["thoughtSignature"] = json!("private-first-signature");
    let outcome = run(vec![
        vec![first],
        vec![identified("real-id", "read", json!({"file":"a"}))],
    ]);
    let calls =
        outcome.assert_success(&[("read", json!({"file":"a"})), ("read", json!({"file":"a"}))]);
    assert_eq!(calls[1]["id"], "real-id");
    let parts = outcome.replay_parts();
    assert_eq!(parts[0]["thoughtSignature"], "private-first-signature");
    assert!(parts[1].get("thoughtSignature").is_none());
}

#[test]
fn explicit_id_replays_are_deduplicated_without_losing_signature() {
    let mut signed = identified(
        "same-id",
        "read",
        json!({"file":"a", "nested":{"a":1,"b":2}}),
    );
    signed["thoughtSignature"] = json!("signature-a");
    // Object key order is not argument identity. Replays can change position,
    // omit a one-shot signature, and occur in the same or subsequent event.
    let replay = identified(
        "same-id",
        "read",
        json!({"nested":{"b":2,"a":1}, "file":"a"}),
    );
    let outcome = run(vec![
        vec![signed.clone(), signed],
        vec![identified("other-id", "read", json!({"file":"b"})), replay],
    ]);
    let calls = outcome.assert_success(&[
        ("read", json!({"file":"a", "nested":{"a":1,"b":2}})),
        ("read", json!({"file":"b"})),
    ]);
    assert_eq!(calls[0]["id"], "same-id");
    assert_eq!(calls[1]["id"], "other-id");
    let parts = outcome.replay_parts();
    assert_eq!(parts[0]["thoughtSignature"], "signature-a");
    assert!(parts[1].get("thoughtSignature").is_none());
}

#[test]
fn signature_can_arrive_on_an_identical_explicit_id_replay() {
    let original = identified("same-id", "read", json!({"file":"a"}));
    let mut replay = original.clone();
    replay["thoughtSignature"] = json!("late-signature");
    let outcome = run(vec![vec![original], vec![replay]]);
    outcome.assert_success(&[("read", json!({"file":"a"}))]);
    assert_eq!(
        outcome.replay_parts()[0]["thoughtSignature"],
        "late-signature"
    );
}

#[test]
fn same_explicit_id_conflicting_name_or_args_errors_without_shadow_commit() {
    for conflict in [
        identified("same-id", "write", json!({"file":"a"})),
        identified("same-id", "read", json!({"file":"b"})),
        identified("same-id", "read", json!({"file":"a", "limit":1})),
        identified("same-id", "read", json!({})),
    ] {
        run(vec![
            vec![identified("same-id", "read", json!({"file":"a"}))],
            vec![conflict],
        ])
        .assert_failure("conflicting complete Gemini functionCall");
    }
}

#[test]
fn conflicting_signatures_for_same_id_are_errors() {
    let mut first = identified("same-id", "ping", json!({}));
    first["thoughtSignature"] = json!("first-secret");
    let mut second = first.clone();
    second["thoughtSignature"] = json!("second-secret");
    let outcome = run(vec![vec![first], vec![second]]);
    outcome.assert_failure("conflicting Gemini functionCall thoughtSignature");
    assert!(!outcome.errors[0].contains("secret"));
}

#[test]
fn partial_or_invalid_function_calls_are_errors_not_executable_calls() {
    for call in [
        json!({"name":"read"}),
        json!({"name":"read", "args":null}),
        json!({"name":"read", "args":"{\"file\":"}),
        json!({"name":"read", "args":[]}),
        json!({"args":{}}),
        json!({"name":"", "args":{}}),
        json!({"name":42, "args":{}}),
        json!({"name":"read", "args":{}, "partialArgs":[{"jsonPath":"$.file", "stringValue":"a"}]}),
        json!({"name":"read", "args":{}, "willContinue":true}),
        json!({"name":"read", "args":{}, "willContinue":false}),
    ] {
        for id in [None, Some("partial-id")] {
            let mut call = call.clone();
            if let Some(id) = id {
                call["id"] = json!(id);
            }
            run(vec![vec![json!({"functionCall":call})]]).assert_failure("Gemini functionCall");
        }
    }
}

#[test]
fn ordinary_gemini_call_identity_is_independent_of_transport_chunk_boundaries() {
    let wire = wire(vec![
        vec![function("read", json!({"file":"a"}))],
        vec![function("read", json!({"file":"b"}))],
    ]);
    for split in 0..=wire.len() {
        run_chunks(vec![
            Bytes::copy_from_slice(&wire.as_bytes()[..split]),
            Bytes::copy_from_slice(&wire.as_bytes()[split..]),
        ])
        .assert_success(&[("read", json!({"file":"a"})), ("read", json!({"file":"b"}))]);
    }
}

#[test]
fn explicit_call_id_tracking_is_local_to_each_response() {
    for file in ["a", "b"] {
        run(vec![vec![identified(
            "reused-id",
            "read",
            json!({"file":file}),
        )]])
        .assert_success(&[("read", json!({"file":file}))]);
    }
}
