//! Public-entry contract tests for Gemini stream error handling in CompatEngine.
//!
//! These tests drive ONLY the public `CompatEngine` surface
//! (`prepare_request` + `convert_stream_response` + `CompatState`
//! inspection), the same path nyro-core's dispatcher uses. They pin the safety
//! contract for upstream failures on the Anthropic->Gemini streaming route:
//!
//! * invalid JSON chunks must surface an error, never a fake success;
//! * EOF after candidate content with no termination evidence (no finishReason
//!   chunk at all) must surface an error, never a normal `end_turn`
//!   completion;
//! * a mid-stream transport error must propagate and must not commit shared
//!   Gemini shadow session state derived from the failed stream;
//! * SSE comments / heartbeats between valid chunks are control traffic and
//!   must not disturb a fully successful conversion.
//!
//! The prohibition on `message_stop` in the failure cases is this task's
//! "must not claim normal completion" contract, not an SSE-syntax rule.
//! Red tests below document existing defects: they assert the expected safe
//! behaviour and are kept failing on purpose (no `ignore`, no logic changes).

use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use nyro_ccswitch_compat::{
    CompatEngine, CompatState, ConversionProfile, PreparedRequest, ResponseBody, ResponseMetadata,
    SessionIdentity, SessionSource,
};
use serde_json_ordered::{Value, json};

const PROVIDER_ID: &str = "gemini-stream-contract-provider";
const SESSION_ID: &str = "gemini-stream-contract-session";

/// One engine whose shared `CompatState` is inspected after the stream ends.
/// This mirrors the dispatcher, which passes the same long-lived gateway
/// engine (and therefore the same shadow store) to both entry points.
struct Harness {
    engine: CompatEngine,
    state: CompatState,
    prepared: PreparedRequest,
}

fn harness() -> Harness {
    let state = CompatState::new();
    let engine = CompatEngine::new(state.clone());
    let profile =
        ConversionProfile::anthropic_to_gemini(true).with_provider_id(PROVIDER_ID.to_string());
    // Client-provided identity so the engine wires the (provider, session)
    // key into the shared Gemini shadow store, like a real client header.
    let identity = SessionIdentity {
        value: SESSION_ID.to_string(),
        source: SessionSource::Header,
        client_provided: true,
    };
    let body = Bytes::from(
        serde_json_ordered::to_vec(&json!({
            "model": "gemini-stream-contract",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("serialize request"),
    );
    let prepared = futures::executor::block_on(async {
        engine.prepare_request(profile, body, identity).await
    })
    .expect("prepare_request must accept a minimal Anthropic request");
    Harness {
        engine,
        state,
        prepared,
    }
}

/// Everything one assertion needs to explain a failure: surfaced errors, the
/// emitted Anthropic SSE text, whether a success terminal was claimed, and
/// the shared shadow-session count after the stream drained.
struct StreamOutcome {
    errors: Vec<String>,
    /// Structured Anthropic `error` events (the application-level failure
    /// channel); a surfaced failure may legitimately arrive here instead of
    /// as a Rust stream error item.
    error_events: Vec<String>,
    events: Vec<Value>,
    raw_events: String,
    message_stop_count: usize,
    shadow_sessions: usize,
}

impl StreamOutcome {
    fn diagnostics(&self, label: &str) -> String {
        format!(
            "{label}: errors={:?}, error_events={:?}, message_stop_count={}, shadow_sessions={}, events={}",
            self.errors,
            self.error_events,
            self.message_stop_count,
            self.shadow_sessions,
            self.raw_events
        )
    }

    /// A failure surfaced through either carrier: a Rust stream error item
    /// or a structured Anthropic `error` event.
    fn has_error(&self) -> bool {
        !self.errors.is_empty() || !self.error_events.is_empty()
    }

    /// Concatenated payloads of every `text_delta` event, in emission order.
    fn text(&self) -> String {
        self.events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
            })
            .filter_map(|event| event["delta"]["text"].as_str())
            .collect()
    }

    fn stop_reason(&self) -> Option<&str> {
        self.events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .and_then(|event| event["delta"]["stop_reason"].as_str())
    }
}

/// Drive the converted stream to exhaustion with a plain futures executor.
/// The input is a finite `stream::iter`, so collection cannot hang and no
/// timer-based timeout dependency is required.
fn run_stream(harness: &Harness, chunks: Vec<Result<String, std::io::Error>>) -> StreamOutcome {
    let input = stream::iter(
        chunks
            .into_iter()
            .map(|chunk| chunk.map(|text| Bytes::from(text.into_bytes()))),
    );
    let converted = harness
        .engine
        .convert_stream_response(&harness.prepared.session, ResponseMetadata::new(200), input)
        .expect("2xx Gemini stream must enter conversion");
    let ResponseBody::Stream(converted) = converted.body else {
        panic!("streaming conversion must return a stream body");
    };
    // Fail fast: with a finite `stream::iter` input the converted stream must
    // run to completion on first poll; `now_or_never` turns any hidden
    // pending (a would-be hang) into an immediate test failure.
    let items = converted
        .collect::<Vec<_>>()
        .now_or_never()
        .expect("converted stream must not hang on finite input");
    let mut errors = Vec::new();
    let mut collected: Vec<u8> = Vec::new();
    for item in items {
        match item {
            Ok(bytes) => collected.extend_from_slice(&bytes),
            Err(error) => errors.push(error.to_string()),
        }
    }
    let raw_events =
        String::from_utf8(collected).expect("emitted Anthropic SSE bytes must be valid UTF-8");
    let events = raw_events
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json_ordered::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| {
            panic!("emitted data line is not valid JSON ({error}): {raw_events}")
        });
    let message_stop_count = events
        .iter()
        .filter(|event| event["type"] == "message_stop")
        .count();
    let error_events = events
        .iter()
        .filter(|event| event["type"] == "error")
        .map(|event| event.to_string())
        .collect();
    let shadow_sessions = harness.state.gemini_session_count();
    StreamOutcome {
        errors,
        error_events,
        events,
        raw_events: raw_events.clone(),
        message_stop_count,
        shadow_sessions,
    }
}

fn text_chunk(text: &str, terminal: bool) -> String {
    let mut chunk = json!({
        "responseId": "gemini-stream-contract-reply",
        "modelVersion": "gemini-2.0-flash",
        "candidates": [{
            "index": 0,
            "content": {"role": "model", "parts": [{"text": text}]},
        }],
    });
    if terminal {
        chunk["candidates"][0]["finishReason"] = json!("STOP");
    }
    format!(
        "data: {}\n\n",
        serde_json_ordered::to_string(&chunk).expect("serialize chunk")
    )
}

#[test]
fn gemini_stream_control_case_comments_and_heartbeat_succeed() {
    let harness = harness();
    let outcome = run_stream(
        &harness,
        vec![
            Ok(text_chunk("Hel", false)),
            Ok(": keepalive\n\n".to_string()),
            Ok(": heartbeat ping\n\n".to_string()),
            Ok(text_chunk("Hello", true)),
        ],
    );
    // One combined assertion: a comment/heartbeat-interleaved valid stream is
    // fully successful — the concatenated text deltas rebuild the full text,
    // exactly one message_stop terminal is emitted, and the shared shadow
    // store holds exactly one session (created by the recorded turn).
    assert!(
        !outcome.has_error()
            && outcome.text() == "Hello"
            && outcome.stop_reason() == Some("end_turn")
            && outcome.message_stop_count == 1
            && outcome.shadow_sessions == 1,
        "control case must fully succeed: {}",
        outcome.diagnostics("comments/heartbeat stream")
    );
}

#[test]
fn gemini_stream_invalid_json_mid_stream_surfaces_error_not_success() {
    let harness = harness();
    let outcome = run_stream(
        &harness,
        vec![
            Ok(text_chunk("Hel", false)),
            // Syntactically broken SSE data payload in the middle of the stream.
            Ok(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"LOST\"}]}\n\n"
                    .to_string(),
            ),
            Ok(text_chunk("Hello", true)),
        ],
    );
    // Expected safe behaviour: the invalid chunk is reported as an error, the
    // stream does not claim normal completion, and the shared shadow state is
    // not polluted with a turn assembled while a chunk was silently dropped.
    // All facts asserted together so a failure shows the full picture.
    assert!(
        outcome.has_error() && outcome.message_stop_count == 0 && outcome.shadow_sessions == 0,
        "invalid JSON must be surfaced as an error without fake success or shadow pollution: {}",
        outcome.diagnostics("mid-stream invalid JSON")
    );
}

#[test]
fn gemini_stream_eof_after_content_without_terminal_surfaces_error_not_success() {
    let harness = harness();
    let outcome = run_stream(
        &harness,
        vec![
            // Candidate content starts flowing, then the upstream simply ends:
            // no finishReason chunk and no other termination evidence ever
            // arrives.
            Ok(text_chunk("Hel", false)),
        ],
    );
    // Expected safe behaviour: the truncated stream is reported as an error;
    // the client must not receive a normal end_turn completion, and the
    // partial text is not committed to shared shadow session state as if it
    // were a clean assistant turn.
    assert!(
        outcome.has_error()
            && outcome.message_stop_count == 0
            && outcome.stop_reason() != Some("end_turn")
            && outcome.shadow_sessions == 0,
        "truncated stream must be surfaced as an error without fake end_turn or shadow pollution: {}",
        outcome.diagnostics("EOF after content without terminal")
    );
}

#[test]
fn gemini_stream_transport_error_propagates_without_shadow_pollution() {
    let harness = harness();
    let outcome = run_stream(
        &harness,
        vec![
            Ok(text_chunk("Hel", false)),
            Err(std::io::Error::other(
                "upstream connection reset mid-stream",
            )),
        ],
    );
    // Expected safe behaviour (the positive contrast to the cases above): the
    // transport error propagates to the client, no success terminal is
    // emitted, and the shared shadow state stays clean.
    assert!(
        outcome.has_error()
            && outcome
                .errors
                .iter()
                .chain(outcome.error_events.iter())
                .any(|error| error.contains("upstream connection reset mid-stream"))
            && outcome.message_stop_count == 0
            && outcome.shadow_sessions == 0,
        "transport error must propagate without shadow pollution: {}",
        outcome.diagnostics("mid-stream transport error")
    );
}
