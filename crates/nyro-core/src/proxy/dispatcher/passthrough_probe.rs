//! Pre-commit hold for the byte-level SSE passthrough path.
//!
//! Production incident (request dc053439, Codex `gpt-6-astra`, 2026-10-18):
//! the upstream answered HTTP 200 and streamed only framing (`response.created`,
//! `response.in_progress`, keep-alives, `response.metadata`). After ~172 s it
//! emitted `event: error` / `server_error` and closed. The passthrough relay
//! had already committed the 200 on the first byte, so the dispatcher saw a
//! "successful" attempt and could not fail over to the next route target even
//! though a healthy one (Devin Pro 2) was available.
//!
//! The IR round-trip path already avoids this with `stream_probe`; byte-level
//! passthrough never decoded anything before forwarding. This module adds the
//! equivalent gate: upstream bytes are held (and kept verbatim for replay)
//! until the stream is *committable*:
//!
//! - first payload event (anything that is not known framing) → commit;
//! - a non-retryable error event → commit (the client sees the real error);
//! - a retryable error event before any payload → fail the attempt with a 502
//!   so the dispatcher's ordinary ordered failover moves to the next target;
//! - upstream EOF / read error before any payload → fail the attempt;
//! - hold window, byte cap, cancellation or request deadline → commit with
//!   what was buffered (the relay's own idle/first-byte handling takes over).
//!
//! Non-SSE bodies (JSON answers to a streaming request) and anything that does
//! not start like an SSE field commit immediately, exactly as before.
//!
//! Trade-off: while holding, the client receives no response headers. The hold
//! window is therefore bounded by `PRECOMMIT_HOLD_MAX` and by the configured
//! `first_byte` timeout, whichever is shorter. Failures that surface later than
//! that, or after payload has been sent, cannot be failed over transparently.

use std::time::{Duration, Instant};

use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::proxy::client::UpstreamReadError;
use crate::proxy::context::RequestContext;

use super::stream_probe::append_capped;
use super::{LogBuilder, error_response};

/// Longest the passthrough relay withholds response headers while waiting for
/// the first payload event. Chosen to cover the observed 172 s silent failure
/// while staying under common client header timeouts (undici: 300 s). The
/// effective cap is `min(this, upstream_timeouts.first_byte)`.
pub(super) const PRECOMMIT_HOLD_MAX: Duration = Duration::from_secs(240);

/// Upper bound on bytes withheld before a forced commit. `response.created`
/// echoes the full tool list, so it alone can reach hundreds of KB.
const MAX_HELD_BYTES: usize = 4 * 1024 * 1024;

// ── SSE scanning ──────────────────────────────────────────────────────────────

struct SseEvent {
    name: String,
    data: String,
}

/// Incremental SSE line scanner (LF, CRLF and bare CR terminators, chunk
/// boundaries anywhere). It only observes bytes; the raw stream is replayed
/// untouched.
#[derive(Default)]
struct SseScanner {
    line: Vec<u8>,
    pending_cr: bool,
    event: String,
    data: Vec<String>,
}

impl SseScanner {
    fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &byte in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut out);
                }
                b'\n' => self.end_line(&mut out),
                _ => self.line.push(byte),
            }
        }
        out
    }

    /// Flush a final unterminated line and event at upstream EOF.
    fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        let text = String::from_utf8_lossy(&line);
        if text.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match text.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (&*text, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.event.is_empty() && self.data.is_empty() {
            return;
        }
        out.push(SseEvent {
            name: std::mem::take(&mut self.event),
            data: std::mem::take(&mut self.data).join("\n"),
        });
    }
}

/// Whether a body starts like an SSE stream. `None` while only whitespace has
/// been seen. Anything else (notably `{` / `[`) is a non-SSE body and commits
/// immediately so the existing JSON-to-SSE conversion keeps working.
fn sniff_sse(bytes: &[u8]) -> Option<bool> {
    let first = bytes.iter().find(|byte| !byte.is_ascii_whitespace())?;
    Some(matches!(first, b'd' | b'e' | b'i' | b'r' | b':'))
}

// ── Event classification ──────────────────────────────────────────────────────

#[derive(Debug)]
struct UpstreamFailure {
    code: String,
    message: String,
    retryable: bool,
}

#[derive(Debug)]
enum EventClass {
    /// Lifecycle / keep-alive event that carries no model output.
    Framing,
    /// Anything else: model output, a terminal event, or unknown content.
    /// Unknown events deliberately commit so the hold never changes behavior
    /// for protocols it does not understand.
    Payload,
    Failure(UpstreamFailure),
}

fn classify_event(name: &str, data: &str) -> EventClass {
    let data = data.trim();
    if data.is_empty() {
        // A named `error` event with no body is still an error the client must
        // see; any other empty event is framing.
        return if name == "error" {
            EventClass::Payload
        } else {
            EventClass::Framing
        };
    }
    if data == "[DONE]" {
        return EventClass::Payload;
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return EventClass::Payload;
    };
    let kind = value.get("type").and_then(Value::as_str).unwrap_or(name);
    match kind {
        "response.created"
        | "response.in_progress"
        | "response.queued"
        | "response.metadata"
        | "keepalive"
        | "ping"
        | "message_start" => EventClass::Framing,
        "error" | "response.failed" => extract_failure(&value)
            .map(EventClass::Failure)
            .unwrap_or(EventClass::Payload),
        // Chat Completions error: top-level `error` object, no `choices`.
        _ if value.get("type").is_none()
            && value.get("choices").is_none()
            && value.get("error").is_some_and(Value::is_object) =>
        {
            extract_failure(&value)
                .map(EventClass::Failure)
                .unwrap_or(EventClass::Payload)
        }
        _ => EventClass::Payload,
    }
}

fn field_string(object: &Value, key: &str) -> Option<String> {
    match object.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Pull `(code, message)` out of the error shapes seen on the wire:
/// - Responses nested: `{"type":"error","error":{"code","message"}}`
/// - Responses `response.failed`: `{"response":{"error":{...}}}`
/// - Responses flat: `{"type":"error","code","message"}`
/// - Anthropic: `{"type":"error","error":{"type":"overloaded_error","message"}}`
/// - Chat Completions: `{"error":{"message","type","code"}}`
fn extract_failure(value: &Value) -> Option<UpstreamFailure> {
    let nested = value
        .pointer("/response/error")
        .filter(|error| error.is_object())
        .or_else(|| value.get("error").filter(|error| error.is_object()));
    let (object, is_nested) = match nested {
        Some(object) => (object, true),
        None => (value, false),
    };
    let code = field_string(object, "code");
    // Only a nested error object's own `type` names the failure; on the flat
    // shape `type` is just the event discriminator ("error").
    let type_hint = if is_nested {
        field_string(object, "type")
    } else {
        None
    };
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if code.is_none() && type_hint.is_none() && message.is_none() {
        return None;
    }
    let retryable = code.as_deref().is_some_and(is_retryable_code)
        || type_hint.as_deref().is_some_and(is_retryable_code);
    Some(UpstreamFailure {
        code: code.or(type_hint).unwrap_or_else(|| "unknown".into()),
        message: message.unwrap_or_default(),
        retryable,
    })
}

/// Errors that describe the upstream or this credential's capacity rather than
/// the request, so another route target may succeed. Request-shape errors
/// (`invalid_prompt`, `context_length_exceeded`, content filters, ...) are not
/// listed: replaying them elsewhere only wastes time.
fn is_retryable_code(code: &str) -> bool {
    let code = code.trim().to_ascii_lowercase();
    if let Ok(status) = code.parse::<u16>() {
        return super::util::is_retryable(status);
    }
    matches!(
        code.as_str(),
        "server_error"
            | "internal_server_error"
            | "internal_error"
            | "api_error"
            | "overloaded_error"
            | "overloaded"
            | "model_overloaded"
            | "engine_overloaded"
            | "service_unavailable"
            | "service_unavailable_error"
            | "bad_gateway"
            | "gateway_timeout"
            | "timeout"
            | "request_timeout"
            | "rate_limit_exceeded"
            | "rate_limit_error"
            | "too_many_requests"
            | "insufficient_quota"
            | "usage_limit_reached"
    )
}

enum Verdict {
    /// Nothing decisive yet.
    Hold,
    Commit,
    Fail(UpstreamFailure),
}

fn judge(events: &[SseEvent]) -> Verdict {
    for event in events {
        match classify_event(&event.name, &event.data) {
            EventClass::Framing => continue,
            EventClass::Payload => return Verdict::Commit,
            EventClass::Failure(failure) if failure.retryable => return Verdict::Fail(failure),
            // Deterministic request errors reach the client verbatim.
            EventClass::Failure(_) => return Verdict::Commit,
        }
    }
    Verdict::Hold
}

// ── Hold driver ───────────────────────────────────────────────────────────────

/// Evidence about an attempt that failed before committing any output.
pub(super) struct HeldFailure {
    /// Performance outcome: `failed` or `timed_out`.
    pub outcome: &'static str,
    pub kind: &'static str,
    pub stage: &'static str,
    /// Internal diagnostic (request log, tracing).
    pub message: String,
    /// What the client sees in the 502 body; never carries transport detail.
    pub client_message: String,
    /// Verbatim upstream bytes (bounded) for the request-log row.
    pub raw_text: String,
    pub chunks_count: i32,
    pub latency_ms: i64,
}

pub(super) enum HoldOutcome {
    /// Forward `replay` first, then continue with the live upstream stream.
    Commit {
        replay: Vec<Bytes>,
        first_chunk_ms: Option<i64>,
    },
    Fail(Box<HeldFailure>),
}

/// Read upstream chunks until the stream is committable. See the module docs.
pub(super) async fn hold_until_committable<S>(
    stream: &mut S,
    req_ctx: &RequestContext,
    started_at: Instant,
    max_hold: Duration,
) -> HoldOutcome
where
    S: Stream<Item = Result<Bytes, UpstreamReadError>> + Unpin,
{
    let hold_started = tokio::time::Instant::now();
    let mut scanner = SseScanner::default();
    let mut replay: Vec<Bytes> = Vec::new();
    let mut held_bytes = 0_usize;
    let mut raw_text = String::new();
    let mut chunks_count = 0_i32;
    let mut first_chunk_ms: Option<i64> = None;
    let mut sniffed = false;

    let fail = |outcome: &'static str,
                kind: &'static str,
                stage: &'static str,
                message: String,
                client_message: String,
                raw_text: String,
                chunks_count: i32| {
        HoldOutcome::Fail(Box::new(HeldFailure {
            outcome,
            kind,
            stage,
            message,
            client_message,
            raw_text,
            chunks_count,
            latency_ms: started_at.elapsed().as_millis() as i64,
        }))
    };
    let failure_for_event = |failure: UpstreamFailure, raw_text: String, chunks_count: i32| {
        let detail = if failure.message.is_empty() {
            failure.code.clone()
        } else {
            format!("{}: {}", failure.code, failure.message)
        };
        let message = format!("upstream stream failed before producing any output: {detail}");
        fail(
            "failed",
            "upstream_error",
            "upstream_response",
            message.clone(),
            message,
            raw_text,
            chunks_count,
        )
    };

    loop {
        let budget = max_hold
            .saturating_sub(hold_started.elapsed())
            .min(req_ctx.deadline.remaining());
        if budget.is_zero() {
            break;
        }
        let next = tokio::select! {
            biased;
            _ = req_ctx.cancellation.cancelled() => break,
            result = tokio::time::timeout(budget, stream.next()) => match result {
                Ok(item) => item,
                Err(_) => break,
            },
        };
        match next {
            None => {
                let events = scanner.finish();
                return match judge(&events) {
                    Verdict::Fail(failure) => failure_for_event(failure, raw_text, chunks_count),
                    Verdict::Commit => HoldOutcome::Commit {
                        replay,
                        first_chunk_ms,
                    },
                    Verdict::Hold => fail(
                        "failed",
                        "upstream_error",
                        "upstream_response",
                        "upstream stream ended before producing any output".to_string(),
                        "upstream stream ended before producing any output".to_string(),
                        raw_text,
                        chunks_count,
                    ),
                };
            }
            Some(Err(error)) => {
                let timeout = error.is_timeout();
                return fail(
                    if timeout { "timed_out" } else { "failed" },
                    if timeout {
                        "timeout"
                    } else {
                        "upstream_read_error"
                    },
                    error.stage(),
                    format!("upstream stream failed before producing any output: {error}"),
                    "upstream stream failed before producing any output".to_string(),
                    raw_text,
                    chunks_count,
                );
            }
            Some(Ok(bytes)) => {
                if bytes.is_empty() {
                    continue;
                }
                if first_chunk_ms.is_none() {
                    first_chunk_ms = Some(started_at.elapsed().as_millis() as i64);
                }
                chunks_count += 1;
                held_bytes += bytes.len();
                append_capped(&mut raw_text, &String::from_utf8_lossy(&bytes));
                if !sniffed {
                    match sniff_sse(&bytes) {
                        Some(true) => sniffed = true,
                        Some(false) => {
                            replay.push(bytes);
                            break;
                        }
                        None => {}
                    }
                }
                let events = scanner.feed(&bytes);
                replay.push(bytes);
                match judge(&events) {
                    Verdict::Hold => {}
                    Verdict::Commit => break,
                    Verdict::Fail(failure) => {
                        return failure_for_event(failure, raw_text, chunks_count);
                    }
                }
                if held_bytes >= MAX_HELD_BYTES {
                    break;
                }
            }
        }
    }
    HoldOutcome::Commit {
        replay,
        first_chunk_ms,
    }
}

// ── Failure surfacing ─────────────────────────────────────────────────────────

/// Record the failed attempt, emit its request-log row (upstream 200 + the
/// captured evidence) and answer with a retryable 502 so the dispatcher fails
/// over to the next target. The attempt counts against provider health like any
/// other upstream 5xx: the upstream really did fail.
pub(super) fn fail_attempt(
    log: LogBuilder,
    failure: HeldFailure,
    upstream_request_headers: Option<String>,
    upstream_request_body: Option<String>,
    upstream_headers: Option<String>,
) -> Response {
    if let Some(attempt) = &log.performance {
        let error = anyhow::anyhow!(failure.message.clone());
        attempt.record_failure(failure.outcome, failure.kind, failure.stage, error.as_ref());
    }
    let mut row = log
        .status(502)
        .upstream_status(200)
        .with_upstream_request(upstream_request_headers, upstream_request_body)
        .with_upstream_response(
            200,
            upstream_headers,
            Some(failure.raw_text.clone()),
            Some(failure.latency_ms),
        )
        // No TTFB sample: a failed attempt must not skew latency ranking.
        .stream_metrics(failure.chunks_count, None);
    if row.performance.is_none() {
        row.diagnostic.record_message(
            failure.outcome,
            failure.kind,
            failure.stage,
            &failure.message,
        );
    }
    row.emit();
    error_response(502, &failure.client_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use crate::proxy::client::UpstreamTimeoutPhase;

    fn ctx() -> RequestContext {
        RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Duration::from_secs(600),
        )
    }

    type Item = Result<Bytes, UpstreamReadError>;

    fn ok(text: &str) -> Item {
        Ok(Bytes::from(text.to_string()))
    }

    fn event(name: &str, data: serde_json::Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    /// The production incident, event for event (tool list elided).
    fn incident_prefix() -> Vec<String> {
        vec![
            event(
                "response.created",
                serde_json::json!({"type":"response.created","sequence_number":0,
                    "response":{"id":"resp_x","status":"in_progress","output":[],"tools":[]}}),
            ),
            event(
                "response.in_progress",
                serde_json::json!({"type":"response.in_progress","sequence_number":1}),
            ),
            ": keep-alive\n\n".repeat(5),
            event(
                "keepalive",
                serde_json::json!({"type":"keepalive","sequence_number":2}),
            ),
            event(
                "response.metadata",
                serde_json::json!({"type":"response.metadata","sequence_number":7,
                    "response_id":"resp_x","metadata":{}}),
            ),
        ]
    }

    fn incident_error() -> String {
        event(
            "error",
            serde_json::json!({"type":"error","sequence_number":8,"error":{
                "message":"An error occurred while processing your request. Please include the request ID ea2be9d4 in your message.",
                "type":"server_error","param":null,"code":"server_error"}}),
        )
    }

    async fn hold(items: Vec<Item>) -> HoldOutcome {
        let mut stream = futures::stream::iter(items);
        hold_until_committable(&mut stream, &ctx(), Instant::now(), Duration::from_secs(30)).await
    }

    fn replay_text(replay: &[Bytes]) -> String {
        replay
            .iter()
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect()
    }

    // ── scanner ──────────────────────────────────────────────────────────────

    #[test]
    fn scanner_handles_every_line_terminator_and_split_boundaries() {
        for newline in ["\n", "\r\n", "\r"] {
            let wire = format!(
                ": hello{newline}event: a{newline}data: 1{newline}data: 2{newline}{newline}data: 3{newline}{newline}"
            );
            // One chunk, then every possible two-way split (covers CR|LF).
            let mut whole = SseScanner::default();
            let events = whole.feed(wire.as_bytes());
            assert_eq!(events.len(), 2, "terminator {newline:?}");
            assert_eq!(events[0].name, "a");
            assert_eq!(events[0].data, "1\n2");
            assert_eq!(events[1].name, "");
            assert_eq!(events[1].data, "3");
            for at in 0..wire.len() {
                let mut split = SseScanner::default();
                let mut got = split.feed(&wire.as_bytes()[..at]);
                got.extend(split.feed(&wire.as_bytes()[at..]));
                assert_eq!(got.len(), 2, "split {at} of {newline:?}");
                assert_eq!(got[0].data, "1\n2");
            }
        }
    }

    #[test]
    fn scanner_flushes_unterminated_final_event_at_eof() {
        let mut scanner = SseScanner::default();
        assert!(scanner.feed(b"event: error\ndata: {\"a\":1}").is_empty());
        let events = scanner.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "error");
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn comment_only_streams_produce_no_events() {
        let mut scanner = SseScanner::default();
        assert!(scanner.feed(b": keep-alive\n\n: keep-alive\n\n").is_empty());
        assert!(scanner.finish().is_empty());
    }

    // ── classification ───────────────────────────────────────────────────────

    fn class(name: &str, data: serde_json::Value) -> EventClass {
        classify_event(name, &data.to_string())
    }

    #[test]
    fn lifecycle_and_keepalive_events_are_framing() {
        for kind in [
            "response.created",
            "response.in_progress",
            "response.queued",
            "response.metadata",
            "keepalive",
            "ping",
            "message_start",
        ] {
            assert!(
                matches!(
                    class(kind, serde_json::json!({"type": kind})),
                    EventClass::Framing
                ),
                "{kind}"
            );
        }
        assert!(matches!(classify_event("", ""), EventClass::Framing));
        assert!(matches!(classify_event("ping", "  "), EventClass::Framing));
    }

    #[test]
    fn output_terminal_and_unknown_events_commit() {
        for value in [
            serde_json::json!({"type":"response.output_item.added","item":{}}),
            serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"x"}),
            serde_json::json!({"type":"response.output_text.delta","delta":"x"}),
            serde_json::json!({"type":"response.completed","response":{}}),
            serde_json::json!({"type":"response.incomplete","response":{}}),
            serde_json::json!({"type":"content_block_start","index":0}),
            serde_json::json!({"id":"c1","choices":[{"delta":{"role":"assistant"}}]}),
            serde_json::json!({"type":"something.new"}),
        ] {
            assert!(
                matches!(class("", value.clone()), EventClass::Payload),
                "{value}"
            );
        }
        assert!(matches!(classify_event("", "[DONE]"), EventClass::Payload));
        assert!(matches!(
            classify_event("", "not json"),
            EventClass::Payload
        ));
        assert!(matches!(classify_event("error", ""), EventClass::Payload));
    }

    #[test]
    fn retryable_error_shapes_are_failures() {
        let cases = [
            // Incident shape: nested object with code + type.
            serde_json::json!({"type":"error","error":{"type":"server_error","code":"server_error","message":"m"}}),
            // Responses flat shape.
            serde_json::json!({"type":"error","code":"server_error","message":"m"}),
            // response.failed nesting.
            serde_json::json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"m"}}}),
            // Anthropic.
            serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
            // Chat Completions (no discriminator).
            serde_json::json!({"error":{"message":"m","type":"server_error","code":null}}),
            // Numeric HTTP-style code.
            serde_json::json!({"type":"error","error":{"code":503,"message":"m"}}),
            serde_json::json!({"type":"error","error":{"code":"rate_limit_exceeded","message":"m"}}),
        ];
        for value in cases {
            match class("", value.clone()) {
                EventClass::Failure(failure) => assert!(failure.retryable, "{value}"),
                other => panic!("{value} classified as {other:?}"),
            }
        }
    }

    #[test]
    fn request_shape_errors_are_failures_but_not_retryable() {
        for code in [
            "invalid_prompt",
            "context_length_exceeded",
            "content_filter",
        ] {
            match class(
                "",
                serde_json::json!({"type":"error","error":{"code":code,"message":"m"}}),
            ) {
                EventClass::Failure(failure) => assert!(!failure.retryable, "{code}"),
                other => panic!("{code} classified as {other:?}"),
            }
        }
        // The flat shape's `type: "error"` discriminator must not be read as a
        // failure code.
        match class("", serde_json::json!({"type":"error","message":"m"})) {
            EventClass::Failure(failure) => {
                assert!(!failure.retryable);
                assert_eq!(failure.code, "unknown");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn failed_event_without_error_detail_is_forwarded_not_retried() {
        let value = serde_json::json!({"type":"response.failed","response":{"error":null}});
        assert!(matches!(class("", value), EventClass::Payload));
    }

    // ── hold driver ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn incident_stream_fails_before_commit_with_upstream_detail() {
        let mut items: Vec<Item> = incident_prefix().iter().map(|s| ok(s)).collect();
        items.push(ok(&incident_error()));
        let HoldOutcome::Fail(failure) = hold(items).await else {
            panic!("a retryable error before any payload must fail the attempt");
        };
        assert_eq!(failure.outcome, "failed");
        assert_eq!(failure.kind, "upstream_error");
        assert_eq!(failure.stage, "upstream_response");
        assert!(
            failure.message.contains("server_error"),
            "{}",
            failure.message
        );
        assert!(failure.message.contains("ea2be9d4"), "{}", failure.message);
        assert!(failure.raw_text.contains("response.created"));
        assert!(failure.raw_text.contains("\"code\":\"server_error\""));
        assert_eq!(failure.chunks_count as usize, incident_prefix().len() + 1);
    }

    #[tokio::test]
    async fn events_split_across_chunks_are_still_classified() {
        let wire = incident_prefix().concat() + &incident_error();
        // Cut at arbitrary byte offsets, including inside the error event.
        let bytes = wire.as_bytes();
        let items: Vec<Item> = bytes
            .chunks(37)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        assert!(matches!(hold(items).await, HoldOutcome::Fail(_)));
    }

    #[tokio::test]
    async fn payload_after_framing_commits_and_replays_every_byte_in_order() {
        let mut parts = incident_prefix();
        parts.push(event(
            "response.output_text.delta",
            serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
        ));
        // Bytes after the commit point must stay with the live stream.
        parts.push(incident_error());
        let expected_prefix: String = parts[..parts.len() - 1].concat();
        let items: Vec<Item> = parts.iter().map(|s| ok(s)).collect();
        let HoldOutcome::Commit {
            replay,
            first_chunk_ms,
        } = hold(items).await
        else {
            panic!("payload must commit");
        };
        assert_eq!(replay_text(&replay), expected_prefix);
        assert!(first_chunk_ms.is_some());
    }

    #[tokio::test]
    async fn error_after_payload_is_never_held() {
        let items = vec![
            ok(&event(
                "response.output_text.delta",
                serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
            )),
            ok(&incident_error()),
        ];
        let HoldOutcome::Commit { replay, .. } = hold(items).await else {
            panic!("committed streams stay committed");
        };
        assert_eq!(replay.len(), 1, "the error chunk belongs to the live tail");
    }

    #[tokio::test]
    async fn non_retryable_error_commits_so_the_client_sees_it() {
        let wire = event(
            "error",
            serde_json::json!({"type":"error","error":{"code":"context_length_exceeded","message":"too long"}}),
        );
        let HoldOutcome::Commit { replay, .. } = hold(vec![ok(&wire)]).await else {
            panic!("request-shape errors must not trigger failover");
        };
        assert_eq!(replay_text(&replay), wire);
    }

    #[tokio::test]
    async fn json_body_commits_immediately_without_waiting_for_eof() {
        let mut stream = futures::stream::iter(vec![ok("{\"id\":\"resp\"}")])
            .chain(futures::stream::pending::<Item>());
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            hold_until_committable(
                &mut stream,
                &ctx(),
                Instant::now(),
                Duration::from_secs(300),
            ),
        )
        .await
        .expect("a non-SSE body must not be held");
        let HoldOutcome::Commit { replay, .. } = outcome else {
            panic!("JSON bodies commit");
        };
        assert_eq!(replay_text(&replay), "{\"id\":\"resp\"}");
    }

    #[tokio::test]
    async fn eof_after_framing_only_fails_and_empty_body_fails() {
        let framing: Vec<Item> = incident_prefix().iter().map(|s| ok(s)).collect();
        let HoldOutcome::Fail(failure) = hold(framing).await else {
            panic!("framing-only EOF is a failed attempt");
        };
        assert!(
            failure
                .message
                .contains("ended before producing any output")
        );
        assert!(matches!(hold(vec![]).await, HoldOutcome::Fail(_)));
    }

    #[tokio::test]
    async fn unterminated_error_at_eof_still_fails_over() {
        let wire = incident_error().trim_end().to_string();
        let mut items: Vec<Item> = incident_prefix().iter().map(|s| ok(s)).collect();
        items.push(ok(&wire));
        assert!(matches!(hold(items).await, HoldOutcome::Fail(_)));
    }

    #[tokio::test]
    async fn read_errors_before_payload_fail_with_timeout_classification() {
        let mut items: Vec<Item> = vec![ok(&incident_prefix()[0])];
        items.push(Err(UpstreamReadError::Timeout {
            phase: UpstreamTimeoutPhase::StreamIdle,
        }));
        let HoldOutcome::Fail(failure) = hold(items).await else {
            panic!("a read error before payload fails the attempt");
        };
        assert_eq!(failure.outcome, "timed_out");
        assert_eq!(failure.kind, "timeout");
        assert_eq!(failure.stage, "upstream_idle");
        assert!(!failure.client_message.contains("StreamIdle"));
    }

    #[tokio::test(start_paused = true)]
    async fn hold_window_expiry_commits_what_was_buffered() {
        let mut stream = futures::stream::iter(vec![ok(&incident_prefix()[0])])
            .chain(futures::stream::pending::<Item>());
        let HoldOutcome::Commit { replay, .. } =
            hold_until_committable(&mut stream, &ctx(), Instant::now(), Duration::from_secs(5))
                .await
        else {
            panic!("an expired hold window commits");
        };
        assert_eq!(replay.len(), 1);
    }

    #[tokio::test]
    async fn cancellation_commits_instead_of_inventing_a_failure() {
        let ctx = ctx();
        ctx.cancellation.cancel();
        let mut stream = futures::stream::iter(vec![ok(&incident_prefix()[0])])
            .chain(futures::stream::pending::<Item>());
        let outcome =
            hold_until_committable(&mut stream, &ctx, Instant::now(), Duration::from_secs(30))
                .await;
        assert!(matches!(outcome, HoldOutcome::Commit { .. }));
    }

    #[tokio::test]
    async fn byte_cap_forces_a_commit() {
        let filler = format!(": {}\n\n", "x".repeat(64 * 1024));
        let items: Vec<Item> = (0..80).map(|_| ok(&filler)).collect();
        let HoldOutcome::Commit { replay, .. } = hold(items).await else {
            panic!("the byte cap commits");
        };
        let held: usize = replay.iter().map(Bytes::len).sum();
        assert!((MAX_HELD_BYTES..MAX_HELD_BYTES + 128 * 1024).contains(&held));
    }
}
