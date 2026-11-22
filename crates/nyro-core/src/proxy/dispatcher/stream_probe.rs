//! Pre-commit stream probe: transient zero-payload terminal handling.
//!
//! Production incident (Gemini 3.8 via the Code Assist v1internal surface):
//! the upstream answers HTTP 200 with a single SSE event carrying
//! `finishReason: "MALFORMED_FUNCTION_CALL"` and no text or tool-call
//! output — Google's internal function-call parser rejected an empty
//! payload. The dispatcher retry loop only sees HTTP status codes, so that
//! sampling glitch was committed to the client verbatim as a "successful"
//! stream whose only meaningful frame was the failure itself.
//!
//! This module centralizes the pure classification helpers plus the probe
//! driver that runs before a streaming response is committed: upstream chunks
//! are decoded without consuming any downstream conversion state (tool route
//! plan, OnResponse hooks, ingress formatter), so a transient zero-payload
//! terminal fails the current attempt before anything reaches the client.
//! The failure surfaces as a typed, health-neutral 502. Before committing
//! output, the dispatcher may replay the same prepared target once while the
//! request is active; a second failure resumes ordinary target failover.
//! Sampling failures remain neutral to provider health scoring.

use std::time::Instant;

use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::protocol::StreamResponseDecoder;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiStreamDelta, Usage};

use super::stream::{StreamInput, StreamRawChunkHook};
use super::{LogBuilder, LogUsageAccumulator, stream_error_event};

/// Upper bound on the upstream bytes retained for diagnostics (mirrors the
/// compat `prime_stream` budget): enough to capture the failure evidence for
/// request logs and finish-message extraction, never enough to stall.
const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// Upper bound on buffered pre-restore deltas before the probe forces a
/// commit. An upstream that streams endless non-payload frames (signatures,
/// usage, opaque vendor events) must not grow the probe buffer without limit;
/// past the cap the stream commits with whatever was buffered and the buffer
/// replays through the normal pipeline unchanged.
const MAX_BUFFERED_DELTAS: usize = 2048;

/// Whitelist of terminations that are known transient sampling glitches when
/// they arrive with zero payload: one replay may recover before target
/// failover. Besides MALFORMED_FUNCTION_CALL, Gemini 4 (Hydrogen) traffic on
/// the Code Assist surface also ends zero-output turns with
/// UNEXPECTED_TOOL_CALL / MALFORMED_RESPONSE (CLIProxyAPI #7513 handles the
/// same three). Deliberately narrow — SAFETY, RECITATION and every other
/// finish reason keep today's pass-through behavior, and a terminal that
/// carries any payload is never replayed.
pub(super) fn is_transient_stream_terminal(stop_reason: &str) -> bool {
    [
        "malformed_function_call",
        "unexpected_tool_call",
        "malformed_response",
    ]
    .iter()
    .any(|reason| stop_reason.eq_ignore_ascii_case(reason))
}

/// Whether one decoded delta carries actual model output. Message framing,
/// thinking signatures, usage and opaque vendor events do not count: a stream
/// that produced only those and then terminated has delivered nothing the
/// client can act on.
pub(super) fn stream_delta_is_payload(delta: &AiStreamDelta) -> bool {
    match delta {
        AiStreamDelta::TextDelta(text) => !text.is_empty(),
        AiStreamDelta::ThinkingDelta(text) => !text.is_empty(),
        AiStreamDelta::ToolCallStart { .. }
        | AiStreamDelta::ToolCallDelta { .. }
        | AiStreamDelta::ToolCallComplete { .. } => true,
        AiStreamDelta::MessageStart { .. }
        | AiStreamDelta::ThinkingSignature(_)
        | AiStreamDelta::Usage(_)
        | AiStreamDelta::Done { .. }
        | AiStreamDelta::StreamError { .. }
        | AiStreamDelta::UnexpectedEof
        | AiStreamDelta::Unknown { .. } => false,
    }
}

/// Evidence about one transient zero-payload upstream attempt.
pub(super) struct TransientStreamFailure {
    /// None only for a clean Antigravity EOF proven by its shell-aware decoder.
    pub stop_reason: Option<String>,
    /// Upstream `finishMessage` extracted from the raw SSE text, when present.
    pub finish_message: Option<String>,
    pub usage: Usage,
    pub chunks_count: i32,
    /// Verbatim upstream bytes (bounded by `MAX_CAPTURE_BYTES`).
    pub raw_text: String,
    pub latency_ms: i64,
}

impl TransientStreamFailure {
    pub(super) fn shell_eof(
        shell: &crate::protocol::codec::google::gemini::stream::AntigravityZeroOutput,
        chunks_count: i32,
        raw_text: String,
        started_at: Instant,
    ) -> Self {
        Self {
            stop_reason: None,
            finish_message: None,
            usage: shell.0.clone(),
            chunks_count,
            raw_text,
            latency_ms: started_at.elapsed().as_millis() as i64,
        }
    }

    fn kind(&self) -> &'static str {
        if self.stop_reason.is_none() {
            "upstream_zero_output"
        } else {
            "upstream_error"
        }
    }
}

/// Result of the pre-commit probe over one upstream attempt.
pub(super) enum ProbeOutcome {
    /// The stream is safe to commit. `buffered` holds the pre-restore deltas
    /// observed so far; the forwarding task replays them through the normal
    /// restore → hook → accumulate → format pipeline before continuing the
    /// live loop. `terminal_error` carries a pre-rendered ingress error event
    /// for the upstream read / decode failure paths, which keep their current
    /// on-the-wire behavior.
    Commit {
        buffered: Vec<AiStreamDelta>,
        terminal_error: Option<String>,
        /// EOF was decoded during the probe; consumers must not finish twice.
        parser_finished: bool,
        chunks_count: i32,
        first_chunk_ms: Option<i64>,
    },
    /// No output was committed; the decoder proved a retryable zero-output end.
    TransientZeroPayload(Box<TransientStreamFailure>),
    /// No bytes were sent downstream. Do not replay an in-band failure as a
    /// zero-output sampling glitch; upstream execution may already have begun.
    UpstreamStatus(Box<crate::protocol::ir::error::AiError>),
}

/// Decode upstream chunks without consuming any downstream state until the
/// stream is committable:
///
/// - first payload delta → commit (buffer replayed first);
/// - any `Done` (non-transient, or after payload) → commit;
/// - `MAX_BUFFERED_DELTAS` buffered deltas → forced commit (unbounded
///   non-payload streams must not grow the probe buffer);
/// - transient zero-payload `Done` → drain bounded evidence and report;
/// - upstream read error / decoder failure → commit with the same terminal
///   error event the spawned loop emits today (including the sticky
///   `record_failure` for decode failures).
pub(super) async fn probe_stream<S>(
    byte_stream: &mut S,
    parser: &mut dyn StreamResponseDecoder,
    stream_input: &mut StreamInput,
    raw_chunk_hook: Option<&StreamRawChunkHook>,
    performance: Option<&crate::performance::Attempt>,
    ingress: ProtocolId,
    request_id: &str,
    started_at: Instant,
) -> ProbeOutcome
where
    S: Stream<Item = Result<Bytes, crate::proxy::client::UpstreamReadError>> + Unpin,
{
    let mut buffered: Vec<AiStreamDelta> = Vec::new();
    let mut usage_accumulator = LogUsageAccumulator::default();
    let mut raw_text = String::new();
    let mut chunks_count: i32 = 0;
    let mut first_chunk_ms: Option<i64> = None;
    let mut saw_payload = false;

    loop {
        // EOF can release a terminal event buffered without a final SSE blank
        // line. Classify it before committing, exactly like a regular batch.
        let (decoded, parser_finished) = match byte_stream.next().await {
            None => (
                stream_input
                    .finish(parser, raw_chunk_hook, performance)
                    .await,
                true,
            ),
            Some(Err(error)) => {
                tracing::warn!(error = %error, "upstream stream error during pre-commit probe");
                let event = stream_error_event(
                    ingress,
                    request_id,
                    if error.is_timeout() {
                        "timeout"
                    } else {
                        "upstream_read_error"
                    },
                );
                return ProbeOutcome::Commit {
                    buffered,
                    terminal_error: Some(event),
                    parser_finished: false,
                    chunks_count,
                    first_chunk_ms,
                };
            }
            Some(Ok(bytes)) => {
                if first_chunk_ms.is_none() {
                    first_chunk_ms = Some(started_at.elapsed().as_millis() as i64);
                }
                chunks_count += 1;
                let text = String::from_utf8_lossy(&bytes);
                append_capped(&mut raw_text, &text);
                // Only the diagnostic capture is lossy. Conversion buffers bytes
                // into complete UTF-8 lines before vendor normalization.
                (
                    stream_input
                        .decode_chunk(&bytes, parser, raw_chunk_hook, performance)
                        .await,
                    false,
                )
            }
        };
        let ai_deltas = match super::streaming::validate_decoded_batch(decoded) {
            Ok(deltas) => deltas,
            Err(error) => {
                if parser_finished && !saw_payload
                    && let Some(shell) = error.downcast_ref::<crate::protocol::codec::google::gemini::stream::AntigravityZeroOutput>() {
                    return ProbeOutcome::TransientZeroPayload(Box::new(TransientStreamFailure::shell_eof(shell, chunks_count, raw_text, started_at)));
                }
                if let Some(frame) = super::streaming::upstream_status_frame(&error) {
                    return ProbeOutcome::UpstreamStatus(Box::new(frame.clone()));
                }
                if let Some(attempt) = performance {
                    attempt.record_failure(
                        "failed",
                        "conversion_parse_error",
                        "response_conversion",
                        error.as_ref(),
                    );
                }
                let event = stream_error_event(ingress, request_id, "conversion_parse_error");
                return ProbeOutcome::Commit {
                    buffered,
                    terminal_error: Some(event),
                    parser_finished,
                    chunks_count,
                    first_chunk_ms,
                };
            }
        };
        usage_accumulator.apply_all(&ai_deltas);
        let batch_has_payload = ai_deltas.iter().any(stream_delta_is_payload);
        let transient_reason = if saw_payload || batch_has_payload {
            None
        } else {
            ai_deltas.iter().find_map(|delta| match delta {
                AiStreamDelta::Done { stop_reason }
                    if is_transient_stream_terminal(stop_reason) =>
                {
                    Some(stop_reason.clone())
                }
                _ => None,
            })
        };
        saw_payload |= batch_has_payload;
        let done_reason = ai_deltas.iter().find_map(|delta| match delta {
            AiStreamDelta::Done { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        });
        buffered.extend(ai_deltas);
        if let Some(stop_reason) = transient_reason {
            // The attempt has terminally failed; capture bounded evidence
            // from the remaining bytes for the request-log row.
            if !parser_finished {
                drain_capped(byte_stream, &mut raw_text, &mut chunks_count).await;
            }
            return ProbeOutcome::TransientZeroPayload(Box::new(TransientStreamFailure {
                stop_reason: Some(stop_reason),
                finish_message: extract_finish_message(&raw_text),
                usage: usage_accumulator.into_ai_response().usage,
                chunks_count,
                raw_text,
                latency_ms: started_at.elapsed().as_millis() as i64,
            }));
        }
        if parser_finished
            || saw_payload
            || done_reason.is_some()
            || buffered.len() >= MAX_BUFFERED_DELTAS
        {
            return ProbeOutcome::Commit {
                buffered,
                terminal_error: None,
                parser_finished,
                chunks_count,
                first_chunk_ms,
            };
        }
    }
}

pub(super) fn append_capped(raw: &mut String, text: &str) {
    if raw.len() >= MAX_CAPTURE_BYTES {
        return;
    }
    let room = MAX_CAPTURE_BYTES - raw.len();
    if text.len() <= room {
        raw.push_str(text);
        return;
    }
    // Never split a UTF-8 code point.
    let mut end = room;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    raw.push_str(&text[..end]);
}

/// Extra upstream chunks read while collecting failure evidence.
const MAX_DRAIN_CHUNKS: usize = 8;
/// Wall-clock budget for the whole evidence drain: short enough to keep the
/// dispatcher failover responsive when an upstream parks the connection open
/// after its terminal frame instead of closing it cleanly.
const DRAIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// Bounded evidence drain after a transient zero-payload terminal: stops at
/// upstream EOF, a transport error, `MAX_CAPTURE_BYTES`, `MAX_DRAIN_CHUNKS`
/// further chunks, or `DRAIN_WINDOW` — whichever comes first.
///
/// Known limitation: when the drain exits early (window or chunk cap), the
/// attempt's wire observation (`ObservedStream`) is dropped mid-stream and
/// may leave a transport-flavoured sticky diagnostic on the shared
/// performance `Attempt` instead of the terminal finish reason. The
/// attempt has terminally failed either way, so the classification stays
/// truthful; only the recorded reason text can read as a mid-stream drop.
/// Clean EOF after the terminal frame is the norm.
async fn drain_capped<S>(byte_stream: &mut S, raw_text: &mut String, chunks_count: &mut i32)
where
    S: Stream<Item = Result<Bytes, crate::proxy::client::UpstreamReadError>> + Unpin,
{
    let mut drained_chunks = 0_usize;
    let drain = async {
        while raw_text.len() < MAX_CAPTURE_BYTES && drained_chunks < MAX_DRAIN_CHUNKS {
            match byte_stream.next().await {
                None | Some(Err(_)) => {
                    // The attempt already failed terminally; transport errors
                    // during the evidence drain are irrelevant.
                    break;
                }
                Some(Ok(bytes)) => {
                    *chunks_count += 1;
                    drained_chunks += 1;
                    append_capped(raw_text, &String::from_utf8_lossy(&bytes));
                }
            }
        }
    };
    let _ = tokio::time::timeout(DRAIN_WINDOW, drain).await;
}

/// Best-effort extraction of the upstream finish message from the raw SSE
/// text, supporting both the plain Gemini shape and the Code Assist
/// v1internal `{"response": …}` envelope. Purely diagnostic — never feeds
/// the IR.
pub(super) fn extract_finish_message(raw: &str) -> Option<String> {
    for line in raw.split(['\r', '\n']) {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        for pointer in [
            "/candidates/0/finishMessage",
            "/response/candidates/0/finishMessage",
        ] {
            if let Some(message) = value.pointer(pointer).and_then(Value::as_str) {
                return Some(message.to_string());
            }
        }
    }
    None
}

/// Human-readable failure cause for diagnostics and the client error body.
pub(super) fn transient_failure_reason(failure: &TransientStreamFailure) -> String {
    let Some(stop_reason) = &failure.stop_reason else {
        return "Antigravity upstream closed with zero output and no finishReason".to_string();
    };
    let mut reason = format!(
        "upstream stream produced no payload and terminated with finish_reason={}",
        stop_reason.to_ascii_lowercase()
    );
    if let Some(message) = &failure.finish_message {
        reason.push_str(": ");
        reason.push_str(message);
    }
    reason
}

fn transient_failure_row(
    log: &LogBuilder,
    failure: &TransientStreamFailure,
    upstream_request_headers: Option<String>,
    upstream_request_body: Option<String>,
    upstream_headers: Option<String>,
) -> LogBuilder {
    log.clone()
        .status(502)
        .upstream_status(200)
        .usage(failure.usage.clone())
        .with_upstream_request(upstream_request_headers, upstream_request_body)
        .with_upstream_response(
            200,
            upstream_headers,
            Some(failure.raw_text.clone()),
            Some(failure.latency_ms),
        )
        // No first-chunk TTFB on failure rows: emit() feeds a Some sample
        // into the latency EWMA, and a failed attempt must not skew backend
        // ranking (the diagnostic value of TTFB on a zero-payload failure
        // is marginal).
        .stream_metrics(failure.chunks_count, None)
}

/// Record the failed attempt (including upstream HTTP 200 and usage) before
/// returning the private replay marker. Both streaming and aggregation use it.
pub(super) fn transient_failure_response(
    log: &LogBuilder,
    failure: &TransientStreamFailure,
    upstream_request_headers: Option<String>,
    upstream_request_body: Option<String>,
    upstream_headers: Option<String>,
) -> Response {
    let mut row = transient_failure_row(
        log,
        failure,
        upstream_request_headers,
        upstream_request_body,
        upstream_headers,
    );
    if let Some(attempt) = &row.performance {
        let error = anyhow::anyhow!(transient_failure_reason(failure));
        attempt.record_failure(
            "failed",
            failure.kind(),
            "upstream_response",
            error.as_ref(),
        );
    } else {
        row.diagnostic.record_message(
            "failed",
            failure.kind(),
            "upstream_response",
            &transient_failure_reason(failure),
        );
    }
    row.emit();
    transient_terminal_error_response(failure)
}

/// Local-only proof that the upstream terminated with no usable payload and
/// nothing was committed to the client. Never inferred from a status or body.
#[derive(Clone, Copy)]
struct RetryableZeroOutput;

pub(super) fn is_retryable_zero_output(response: &Response) -> bool {
    response.extensions().get::<RetryableZeroOutput>().is_some()
}

pub(super) fn should_retry_same_target(
    response: &Response,
    already_retried: bool,
    ctx: &crate::proxy::context::RequestContext,
) -> bool {
    !already_retried
        && !ctx.cancellation.is_cancelled()
        && !ctx.deadline.remaining().is_zero()
        && is_retryable_zero_output(response)
}

/// Synthetic 502 for a target whose stream terminated transiently with zero
/// payload. The marker permits one same-target replay before normal failover;
/// LocalHealthNeutral keeps sampling failures out of provider health scoring.
pub(super) fn transient_terminal_error_response(failure: &TransientStreamFailure) -> Response {
    let body = match &failure.stop_reason {
        None => {
            serde_json::json!({"error":{"type":failure.kind(), "message":transient_failure_reason(failure)}})
        }
        Some(reason) => {
            // Preserve the existing public error shape for explicit finish reasons.
            let mut message = format!(
                "upstream stream produced no payload and terminated with finish_reason={}",
                reason.to_ascii_lowercase()
            );
            if let Some(detail) = &failure.finish_message {
                message.push_str("; upstream finish message: ");
                message.push_str(detail);
            }
            serde_json::json!({"error":{"message":message}})
        }
    };
    let mut response = (axum::http::StatusCode::BAD_GATEWAY, axum::Json(body)).into_response();
    response.extensions_mut().insert(super::LocalHealthNeutral);
    if failure
        .stop_reason
        .as_deref()
        .is_none_or(is_transient_stream_terminal)
    {
        response.extensions_mut().insert(RetryableZeroOutput);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(reason: &str) -> AiStreamDelta {
        AiStreamDelta::Done {
            stop_reason: reason.to_string(),
        }
    }

    #[test]
    fn same_target_retry_requires_local_marker_and_live_request_budget() {
        use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        use crate::proxy::context::{Deadline, RequestContext};
        use std::time::Duration;

        let mut ctx = RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Duration::from_secs(30),
        );
        let mut failure = TransientStreamFailure {
            stop_reason: Some("MALFORMED_FUNCTION_CALL".into()),
            finish_message: None,
            usage: Usage::default(),
            chunks_count: 1,
            raw_text: String::new(),
            latency_ms: 0,
        };
        let response = transient_terminal_error_response(&failure);
        assert!(should_retry_same_target(&response, false, &ctx));
        assert!(!should_retry_same_target(&response, true, &ctx));
        // An upstream 502 or another local health-neutral error cannot spoof
        // the pre-commit zero-payload proof with a matching message string.
        let mut ordinary = super::super::error_response(502, "MALFORMED_FUNCTION_CALL");
        ordinary
            .extensions_mut()
            .insert(super::super::LocalHealthNeutral);
        assert!(!should_retry_same_target(&ordinary, false, &ctx));
        failure.stop_reason = Some("SAFETY".into());
        assert!(!should_retry_same_target(
            &transient_terminal_error_response(&failure),
            false,
            &ctx
        ));
        ctx.deadline = Deadline::from_now(Duration::ZERO);
        assert!(!should_retry_same_target(&response, false, &ctx));
        ctx.deadline = Deadline::never();
        ctx.cancellation.cancel();
        assert!(!should_retry_same_target(&response, false, &ctx));
    }

    /// Model a decoder whose final batch is available only at transport EOF.
    struct EofDecoder {
        final_batch: Option<anyhow::Result<Vec<AiStreamDelta>>>,
        finish_calls: usize,
    }

    impl StreamResponseDecoder for EofDecoder {
        fn parse_chunk(&mut self, _raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
            Ok(vec![])
        }

        fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
            self.finish_calls += 1;
            self.final_batch
                .take()
                .expect("decoder must only finish once")
        }
    }

    async fn probe_eof(batch: anyhow::Result<Vec<AiStreamDelta>>) -> ProbeOutcome {
        let mut parser = EofDecoder {
            final_batch: Some(batch),
            finish_calls: 0,
        };
        // An EOF stream need not be fused. Reading again after None is a bug.
        let mut ended = false;
        let mut stream = futures::stream::poll_fn(move |_| {
            assert!(!ended, "probe must not drain an already exhausted stream");
            ended = true;
            std::task::Poll::Ready(None::<Result<Bytes, crate::proxy::client::UpstreamReadError>>)
        });
        let outcome = probe_stream(
            &mut stream,
            &mut parser,
            &mut StreamInput::default(),
            None,
            None,
            crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "eof-test",
            Instant::now(),
        )
        .await;
        assert_eq!(parser.finish_calls, 1);
        outcome
    }

    #[tokio::test]
    async fn eof_only_malformed_terminal_is_rejected_before_commit() {
        for reason in [
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
            "MALFORMED_RESPONSE",
        ] {
            let result = probe_eof(Ok(vec![
                AiStreamDelta::ThinkingSignature("signature-only".into()),
                AiStreamDelta::Usage(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 3,
                    ..Usage::default()
                }),
                done(reason),
            ]))
            .await;
            let ProbeOutcome::TransientZeroPayload(failure) = result else {
                panic!("EOF {reason} terminal must fail before committing");
            };
            assert_eq!(failure.stop_reason.as_deref(), Some(reason));
            assert_eq!(failure.usage.prompt_tokens, 10);
            assert_eq!(failure.usage.completion_tokens, 3);
        }
    }

    #[tokio::test]
    async fn eof_payload_and_normal_terminals_are_replayed_once() {
        for reason in [
            "stop",
            "SAFETY",
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
        ] {
            let result = probe_eof(Ok(vec![
                AiStreamDelta::TextDelta("answer".into()),
                done(reason),
            ]))
            .await;
            let ProbeOutcome::Commit {
                buffered,
                parser_finished,
                terminal_error,
                ..
            } = result
            else {
                panic!("an actual payload must not be retried");
            };
            assert!(parser_finished);
            assert!(terminal_error.is_none());
            assert_eq!(buffered.len(), 2);
            assert!(matches!(&buffered[0], AiStreamDelta::TextDelta(text) if text == "answer"));
            assert!(
                matches!(&buffered[1], AiStreamDelta::Done { stop_reason } if stop_reason == reason)
            );
        }
    }

    #[tokio::test]
    async fn eof_decode_error_is_an_explicit_terminal_error() {
        let result = probe_eof(Err(anyhow::anyhow!("invalid final event"))).await;
        let ProbeOutcome::Commit {
            buffered,
            parser_finished,
            terminal_error,
            ..
        } = result
        else {
            panic!("decode error must retain the explicit error-event path");
        };
        assert!(parser_finished);
        assert!(buffered.is_empty());
        assert!(terminal_error.unwrap().contains("conversion_parse_error"));
    }

    #[tokio::test]
    async fn empty_eof_finishes_without_inventing_a_transient_failure() {
        let result = probe_eof(Ok(vec![])).await;
        assert!(matches!(result, ProbeOutcome::Commit {
            parser_finished: true, terminal_error: None, buffered, ..
        } if buffered.is_empty()));
    }

    #[test]
    fn transient_terminal_whitelist_is_exact_and_case_insensitive() {
        assert!(is_transient_stream_terminal("malformed_function_call"));
        assert!(is_transient_stream_terminal("MALFORMED_FUNCTION_CALL"));
        assert!(is_transient_stream_terminal("Malformed_Function_Call"));
        assert!(is_transient_stream_terminal("UNEXPECTED_TOOL_CALL"));
        assert!(is_transient_stream_terminal("MALFORMED_RESPONSE"));
        // Everything else — including neighbouring Google finish reasons —
        // must keep the pass-through behavior.
        for reason in [
            "safety",
            "SAFETY",
            "recitation",
            "stop",
            "length",
            "error",
            "malformed_function",
            "malformed_function_calls",
            "unexpected_tool_calls",
            "malformed",
            "TOO_MANY_TOOL_CALLS",
            "",
        ] {
            assert!(
                !is_transient_stream_terminal(reason),
                "reason {reason:?} must not be transient"
            );
        }
    }

    #[test]
    fn payload_detection_covers_text_thinking_and_tool_calls() {
        assert!(stream_delta_is_payload(&AiStreamDelta::TextDelta(
            "hi".into()
        )));
        assert!(stream_delta_is_payload(&AiStreamDelta::ThinkingDelta(
            "thinking".into()
        )));
        assert!(stream_delta_is_payload(&AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "lookup".into(),
            namespace: None,
            kind: crate::protocol::ir::ToolCallKind::Function,
        }));
        assert!(stream_delta_is_payload(&AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{}".into(),
        }));
        assert!(stream_delta_is_payload(&AiStreamDelta::ToolCallComplete {
            index: 0,
            tool_call: crate::protocol::ir::ToolCall::function("call_1", "lookup", "{}",),
        }));
    }

    #[test]
    fn framing_signatures_usage_and_errors_are_not_payload() {
        // Empty text / empty thinking deliver nothing actionable.
        assert!(!stream_delta_is_payload(&AiStreamDelta::TextDelta(
            String::new()
        )));
        assert!(!stream_delta_is_payload(&AiStreamDelta::ThinkingDelta(
            String::new()
        )));
        // Signature-only part (the real incident shape) is framing, not output.
        assert!(!stream_delta_is_payload(&AiStreamDelta::ThinkingSignature(
            "EsMLCsALARFNMg9yaNHgdF2cRMl8ynZFpcf9EyEeMi63fPN1tymx".into()
        )));
        assert!(!stream_delta_is_payload(&AiStreamDelta::MessageStart {
            id: "gen-1".into(),
            model: "gemini-3.8-flash".into(),
        }));
        assert!(!stream_delta_is_payload(&AiStreamDelta::Usage(
            Usage::default()
        )));
        assert!(!stream_delta_is_payload(&AiStreamDelta::Unknown {
            raw: "{}".into()
        }));
        assert!(!stream_delta_is_payload(&AiStreamDelta::UnexpectedEof));
        assert!(!stream_delta_is_payload(&done("MALFORMED_FUNCTION_CALL")));
    }

    #[test]
    fn finish_message_accepts_all_sse_line_endings_and_compact_data() {
        for newline in ["\n", "\r\n", "\r"] {
            for prefix in ["data: ", "data:"] {
                let wire = format!(
                    ": keepalive{newline}{newline}{prefix}{}{newline}{newline}",
                    serde_json::json!({"response": {"candidates": [{"finishMessage": "empty function call"}]}}),
                );
                assert_eq!(
                    extract_finish_message(&wire).as_deref(),
                    Some("empty function call")
                );
            }
        }
    }

    #[test]
    fn finish_message_is_extracted_from_plain_and_enveloped_sse() {
        let plain = concat!(
            r#"data: {"candidates":[{"content":{"parts":[]},"#,
            r#""finishReason":"MALFORMED_FUNCTION_CALL","#,
            r#""finishMessage":"Function call is empty - no input to parse."}]}"#,
            "

"
        );
        assert_eq!(
            extract_finish_message(plain).as_deref(),
            Some("Function call is empty - no input to parse.")
        );

        // v1internal envelope shape from the production incident.
        let enveloped = concat!(
            r#"data: {"response":{"candidates":[{"finishMessage":"enveloped message"}]}}"#,
            "

"
        );
        assert_eq!(
            extract_finish_message(enveloped).as_deref(),
            Some("enveloped message")
        );

        assert!(
            extract_finish_message(
                "data: [DONE]

"
            )
            .is_none()
        );
        assert!(
            extract_finish_message(
                "data: not-json

"
            )
            .is_none()
        );
        assert!(extract_finish_message("").is_none());
    }
}
