//! Streaming response handler.
//!
//! Two internal paths:
//! - PassThrough: ingress == egress protocol, no vendor mutations → forward raw
//!   SSE bytes; side-channel parser accumulates stats for logging.
//! - IR round-trip: parse → accumulate → format → re-emit as target-protocol SSE.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::protocol::codec::tool_bridge::ToolRoutePlan;
use crate::protocol::ids::ProtocolEndpoint;
use crate::protocol::ir::AiRequest;
use crate::proxy::client::ProxyClient;
use crate::proxy::context::RequestContext;
use crate::proxy::observability::headers_to_json;

use super::stream_probe::{ProbeOutcome, probe_stream};
use super::{CallCtx, LogBuilder, RequestExtras, ai_response_to_deltas, error_response};

// ── Streaming response handler ────────────────────────────────────────────────

/// Owned raw-chunk hook state for the spawned streaming task: applies the
/// vendor's `on_stream_raw_chunk` (e.g. google/antigravity unwrapping the
/// v1internal `{"response": …}` envelope from every SSE data line) before
/// the egress stream decoder parses the chunk.
///
/// Only captured when the vendor declares response mutations for the
/// provider, so mutation-free vendors pay zero overhead.
#[derive(Clone)]
pub(super) struct StreamRawChunkHook {
    vendor: Arc<dyn crate::provider::vendor::Vendor>,
    provider: crate::db::models::Provider,
    protocol_id: ProtocolEndpoint,
    api_key: String,
    actual_model: String,
    credential: Option<crate::auth::types::StoredCredential>,
}

impl StreamRawChunkHook {
    pub(super) fn capture(
        adapter: &Arc<dyn crate::provider::vendor::Vendor>,
        provider: &crate::db::models::Provider,
        provider_ctx: &crate::provider::vendor::ProviderCtx<'_>,
    ) -> Option<Self> {
        if !adapter.declared_response_mutations_for(provider) {
            return None;
        }
        Some(Self {
            vendor: adapter.clone(),
            provider: provider.clone(),
            protocol_id: provider_ctx.protocol,
            api_key: provider_ctx.api_key.to_string(),
            actual_model: provider_ctx.actual_model.to_string(),
            credential: provider_ctx.credential.cloned(),
        })
    }

    pub(super) async fn apply(
        &self,
        chunk: &str,
        performance: Option<&crate::performance::Attempt>,
    ) -> String {
        let vendor_ctx = crate::provider::vendor_ext::VendorCtx {
            provider: &self.provider,
            protocol_id: self.protocol_id,
            api_key: &self.api_key,
            actual_model: &self.actual_model,
            credential: self.credential.as_ref(),
        };
        let mut text = chunk.to_string();
        match self
            .vendor
            .on_stream_raw_chunk(&vendor_ctx, &mut text)
            .await
        {
            Ok(()) => text,
            Err(error) => {
                if let Some(performance) = performance {
                    performance.record_failure(
                        "failed",
                        "hook_error",
                        "response_hook",
                        error.as_ref(),
                    );
                }
                tracing::warn!(
                    %error,
                    vendor = self.vendor.vendor_id(),
                    "vendor on_stream_raw_chunk failed; parsing the raw chunk instead"
                );
                chunk.to_string()
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_stream(
    client: ProxyClient,
    url: &str,
    headers: ReqwestHeaderMap,
    body: Value,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    passthrough_resp: bool,
    mut tool_route_plan: ToolRoutePlan,
    // Cloned into the streaming task for the per-chunk OnResponse phase; the
    // spawned task outlives the borrow, so owned copies (not borrows) cross in.
    req_ctx: &RequestContext,
    req_ir: &AiRequest,
    raw_chunk_hook: Option<StreamRawChunkHook>,
) -> Response {
    let egress = call_ctx.egress;
    let ingress = call_ctx.ingress;
    // Shared log builder: identity + request-side extras pre-filled.
    let log = LogBuilder::from_ctx(call_ctx)
        .with_req_extras(req_extras)
        .upstream_url(url);

    let upstream_start = std::time::Instant::now();
    let call_result = match client.call_stream(url, headers.clone(), body.clone()).await {
        Ok(r) => r,
        Err(e) => {
            log.status(502)
                .resp_body(Some(
                    serde_json::json!({ "error": { "message": format!("upstream error: {e:#}") } })
                        .to_string(),
                ))
                .emit();
            return error_response(502, &format!("upstream error: {e:#}"));
        }
    };
    let upstream_req_hdrs_str = crate::proxy::observability::reqwest_headers_to_json(&headers);
    let upstream_req_body_str = crate::logging::payload::capture_json(&body, true).body;

    let (resp, status) = call_result;
    let upstream_hdrs_str = headers_to_json(resp.headers());

    if status >= 400 {
        let err_body: Value = resp
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({"error": {"message": "upstream error"}}));
        let err_body_str = crate::logging::payload::capture_json(&err_body, true).body;
        log.status(status)
            .upstream_status(status as i32)
            .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
            .upstream_resp_headers(upstream_hdrs_str.clone())
            .upstream_resp_body(err_body_str.clone())
            .resp_body(err_body_str)
            .emit();
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(err_body),
        )
            .into_response();
    }

    // ── Byte-level SSE passthrough ────────────────────────────────────────────
    // Used when ingress == egress protocol and the vendor declares no response
    // mutations (passthrough_resp=true). Upstream bytes are forwarded verbatim;
    // a side-channel parser accumulates usage stats for logging only.
    if passthrough_resp {
        let (pt_tx, pt_rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);

        // Clone the log builder into the spawn: all identity + request-side
        // fields are already owned inside the builder, so no individual variable
        // cloning is needed.
        let log_pt = log.clone();
        let upstream_hdrs_pt = upstream_hdrs_str.clone();
        let upstream_req_hdrs_pt = upstream_req_hdrs_str.clone();
        let upstream_req_body_pt = upstream_req_body_str.clone();
        let upstream_start_pt = upstream_start;

        tokio::spawn(async move {
            // Full buffering is only required when upstream returns JSON to a
            // streaming request. SSE stats are decoded incrementally.
            let mut json_buf: Vec<u8> = Vec::new();
            let mut log_parser = egress.handler().make_stream_response_decoder();
            let mut accumulator = super::LogUsageAccumulator::default();
            let mut undecided_buf: Vec<u8> = Vec::new();
            let mut byte_stream = resp.bytes_stream();
            let mut stream_error: Option<String> = None;
            let mut chunks_count: i32 = 0;
            let mut first_chunk_ms: Option<i64> = None;
            let mut passthrough_mode = PassthroughBodyMode::Undecided;
            let mut converted_client_sse: Option<String> = None;
            let mut converted_ai_resp = None;

            loop {
                let result = tokio::select! {
                    biased;
                    _ = pt_tx.closed() => break,
                    result = byte_stream.next() => result,
                };
                let Some(result) = result else {
                    break;
                };
                match result {
                    Ok(b) => {
                        if first_chunk_ms.is_none() {
                            first_chunk_ms = Some(upstream_start_pt.elapsed().as_millis() as i64);
                        }
                        chunks_count += 1;
                        if let Ok(deltas) = log_parser.parse_chunk(&String::from_utf8_lossy(&b)) {
                            accumulator.apply_all(&deltas);
                        }
                        match passthrough_mode {
                            PassthroughBodyMode::Undecided => {
                                undecided_buf.extend_from_slice(&b);
                                match classify_passthrough_body(&undecided_buf) {
                                    Some(PassthroughBodyMode::RawSse) => {
                                        passthrough_mode = PassthroughBodyMode::RawSse;
                                        let pending = std::mem::take(&mut undecided_buf);
                                        if pt_tx.send(Ok(Bytes::from(pending))).await.is_err() {
                                            break; // client disconnected
                                        }
                                        if let Some(p) = &log_pt.performance {
                                            p.note_client_frame_sent();
                                        }
                                    }
                                    Some(PassthroughBodyMode::NonSseJson) => {
                                        passthrough_mode = PassthroughBodyMode::NonSseJson;
                                        json_buf = std::mem::take(&mut undecided_buf);
                                    }
                                    _ => {}
                                }
                            }
                            PassthroughBodyMode::RawSse => {
                                if pt_tx.send(Ok(b)).await.is_err() {
                                    break; // client disconnected
                                }
                                if let Some(p) = &log_pt.performance {
                                    p.note_client_frame_sent();
                                }
                            }
                            PassthroughBodyMode::NonSseJson => {
                                // Upstream returned a complete JSON response to a stream endpoint.
                                // Buffer until EOF, then convert it to the downstream SSE shape.
                                json_buf.extend_from_slice(&b);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "upstream stream error during passthrough");
                        stream_error = Some(e.to_string());
                        let request_id = log_pt
                            .diagnostic
                            .client_request_id
                            .as_deref()
                            .unwrap_or_default();
                        let err_sse = super::stream_error_event(
                            ingress,
                            request_id,
                            if e.is_timeout() {
                                "timeout"
                            } else {
                                "upstream_read_error"
                            },
                        );
                        if pt_tx.send(Ok(Bytes::from(err_sse))).await.is_ok()
                            && let Some(p) = &log_pt.performance
                        {
                            p.note_client_frame_sent();
                        }
                        break;
                    }
                }
            }

            let upstream_latency_ms = upstream_start_pt.elapsed().as_millis() as i64;
            let raw_sse = String::from_utf8_lossy(&json_buf).into_owned();

            if matches!(
                passthrough_mode,
                PassthroughBodyMode::NonSseJson | PassthroughBodyMode::Undecided
            ) && let Some((client_sse, ai_resp)) =
                format_non_sse_stream_response(&raw_sse, egress, ingress)
            {
                if pt_tx
                    .send(Ok(Bytes::from(client_sse.clone())))
                    .await
                    .is_ok()
                    && let Some(p) = &log_pt.performance
                {
                    p.note_client_frame_sent();
                }
                converted_client_sse = Some(client_sse);
                converted_ai_resp = Some(ai_resp);
            }

            // Optional usage parsing never changes authoritative outcomes.
            if let Ok(ai_deltas) = log_parser.finish() {
                accumulator.apply_all(&ai_deltas);
            }

            let mut ai_resp = converted_ai_resp.unwrap_or_else(|| accumulator.into_ai_response());
            if ai_resp.id.is_empty() {
                ai_resp.id = format!("msg_{}", uuid::Uuid::new_v4().simple());
            }
            if ai_resp.model.is_empty() {
                ai_resp.model = log_pt.upstream_model.clone();
            }

            log_pt
                .status(200)
                .upstream_status(200)
                .usage(ai_resp.usage.clone())
                .maybe_error(stream_error)
                .with_upstream_request(upstream_req_hdrs_pt, upstream_req_body_pt)
                .with_upstream_response(
                    200,
                    upstream_hdrs_pt,
                    Some(raw_sse.clone()),
                    Some(upstream_latency_ms),
                )
                .with_client_response(None, Some(converted_client_sse.unwrap_or(raw_sse)))
                .stream_metrics(chunks_count, first_chunk_ms)
                .emit();
        });

        let stream = ReceiverStream::new(pt_rx);
        let body = Body::from_stream(stream);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(body)
            .unwrap();
        return response;
    }

    // ── IR round-trip path ────────────────────────────────────────────────────
    // Pre-commit probe (see stream_probe): decode upstream chunks without
    // consuming any downstream conversion state (tool route plan, OnResponse
    // hooks, ingress formatter) until the stream is committable, so a
    // transient zero-payload terminal — the Gemini MALFORMED_FUNCTION_CALL
    // sampling glitch — fails the attempt before anything reaches the client
    // and the dispatcher can fail over to the next target.
    let mut stream_parser = egress.handler().make_stream_response_decoder();
    let mut byte_stream = resp.bytes_stream();
    let probe_request_id = log.diagnostic.client_request_id.clone().unwrap_or_default();
    let probe = probe_stream(
        &mut byte_stream,
        stream_parser.as_mut(),
        raw_chunk_hook.as_ref(),
        log.performance.as_ref(),
        ingress,
        &probe_request_id,
        upstream_start,
    )
    .await;

    if let ProbeOutcome::TransientZeroPayload(failure) = probe {
        // The attempt has terminally failed and nothing has reached the
        // client yet: record the failure on this attempt, emit its
        // request-log row (upstream 200 + finishReason/finishMessage + usage
        // preserved), and answer with the health-neutral synthetic 502. The
        // dispatcher's ordinary retry policy then advances to the next
        // target of the same model's ordered candidates — selector-appended
        // fallback rows included — never a same-target replay and never a
        // hop into another route.
        //
        // Known limitation (accepted): the original-wire Terminal observer is
        // one-shot per upstream connection and MALFORMED_FUNCTION_CALL is not
        // in its reason whitelist, so confirmed_completed stays false for
        // these attempts. The explicit record_failure below is what keeps the
        // row classified failed/upstream_error instead of ambiguous_terminal;
        // extending that whitelist would change classification for every
        // Google upstream — out of scope.
        tracing::warn!(
            stop_reason = %failure.stop_reason,
            "transient zero-payload stream terminal; failing the attempt for dispatcher failover"
        );
        if let Some(attempt) = &log.performance {
            let error = anyhow::anyhow!(super::stream_probe::transient_failure_reason(&failure));
            attempt.record_failure(
                "failed",
                "upstream_error",
                "upstream_response",
                error.as_ref(),
            );
        }
        super::stream_probe::emit_transient_failure(
            &log,
            &failure,
            upstream_req_hdrs_str,
            upstream_req_body_str,
            upstream_hdrs_str,
        );
        return super::stream_probe::transient_terminal_error_response(&failure);
    }

    let ProbeOutcome::Commit {
        buffered: probe_buffered,
        terminal_error: probe_terminal_error,
        chunks_count: probe_chunks_count,
        first_chunk_ms: probe_first_chunk_ms,
    } = probe
    else {
        unreachable!("transient probe outcomes return in the failure branch above")
    };

    let mut stream_formatter = ingress.handler().make_stream_response_encoder();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(64);

    // Move the log builder into the spawn.  Extract the fields we need AFTER
    // emit() consumes the builder, before passing it to the spawn.
    let log_ir = log;
    let act_model_ir = log_ir.upstream_model.clone();
    let upstream_hdrs_owned = upstream_hdrs_str;

    // Owned OnResponse hook state for the spawned task: clones the request
    // context / IR / gateway only when at least one hook is registered —
    // otherwise the per-delta application is a zero-overhead no-op.
    let mut hook_state = super::streaming::StreamHookState::capture(req_ctx, req_ir, &call_ctx.gw);

    tokio::spawn(async move {
        let mut accumulator = super::LogUsageAccumulator::default();
        // Wire capture is shared in Attempt; no duplicate full log buffers.
        // Upstream-side counters start from the pre-commit probe so stream
        // metrics keep covering the whole upstream exchange.
        let mut chunks_count: i32 = probe_chunks_count;
        let mut first_chunk_ms: Option<i64> = probe_first_chunk_ms;
        let mut terminal_error_sent = false;

        // Pre-commit replay: the probe buffered pre-restore deltas without
        // touching the tool route plan / OnResponse hooks / formatter, so the
        // identical per-batch pipeline runs here, in order, before the live
        // upstream loop continues.
        {
            let mut ai_deltas = tool_route_plan.restore_stream_deltas(probe_buffered);
            hook_state.apply(&mut ai_deltas).await;
            accumulator.apply_all(&ai_deltas);
            let events = stream_formatter.format_deltas(&ai_deltas);
            for ev in events {
                let sse = ev.to_sse_string();
                if tx.send(Ok(sse)).await.is_err() {
                    return;
                }
                if let Some(p) = &log_ir.performance {
                    p.note_client_frame_sent();
                }
            }
        }
        if let Some(event) = probe_terminal_error {
            if tx.send(Ok(event)).await.is_ok()
                && let Some(p) = &log_ir.performance
            {
                p.note_client_frame_sent();
            }
            terminal_error_sent = true;
        }

        while !terminal_error_sent {
            let chunk = tokio::select! {
                biased;
                _ = tx.closed() => break,
                chunk = byte_stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    // P1: emit an explicit terminal event instead of silently breaking,
                    // so the client receives a defined stop_reason and does not hang.
                    tracing::warn!(error = %e, "upstream stream error; emitting terminal event");
                    let request_id = log_ir
                        .diagnostic
                        .client_request_id
                        .as_deref()
                        .unwrap_or_default();
                    let event = super::stream_error_event(
                        ingress,
                        request_id,
                        if e.is_timeout() {
                            "timeout"
                        } else {
                            "upstream_read_error"
                        },
                    );
                    if tx.send(Ok(event)).await.is_ok()
                        && let Some(p) = &log_ir.performance
                    {
                        p.note_client_frame_sent();
                    }
                    terminal_error_sent = true;
                    break;
                }
            };
            if first_chunk_ms.is_none() {
                first_chunk_ms = Some(upstream_start.elapsed().as_millis() as i64);
            }
            chunks_count += 1;
            let text = String::from_utf8_lossy(&bytes);
            // Vendor raw-chunk normalization (see StreamRawChunkHook) runs
            // before the decoder; the raw buffer keeps the verbatim upstream
            // bytes for request logs.
            let hooked_text;
            let parse_src: &str = if let Some(hook) = raw_chunk_hook.as_ref() {
                hooked_text = hook.apply(&text, log_ir.performance.as_ref()).await;
                hooked_text.as_str()
            } else {
                text.as_ref()
            };
            let ai_deltas = match super::streaming::validate_decoded_batch(
                stream_parser.parse_chunk(parse_src),
            ) {
                Ok(deltas) => deltas,
                Err(error) => {
                    if let Some(attempt) = &log_ir.performance {
                        attempt.record_failure(
                            "failed",
                            "conversion_parse_error",
                            "response_conversion",
                            error.as_ref(),
                        );
                    }
                    let event = super::stream_error_event(
                        ingress,
                        log_ir
                            .diagnostic
                            .client_request_id
                            .as_deref()
                            .unwrap_or_default(),
                        "conversion_parse_error",
                    );
                    if tx.send(Ok(event)).await.is_ok()
                        && let Some(attempt) = &log_ir.performance
                    {
                        attempt.note_client_frame_sent();
                    }
                    terminal_error_sent = true;
                    break;
                }
            };
            {
                let mut ai_deltas = tool_route_plan.restore_stream_deltas(ai_deltas);
                hook_state.apply(&mut ai_deltas).await;
                accumulator.apply_all(&ai_deltas);
                let events = stream_formatter.format_deltas(&ai_deltas);
                for ev in events {
                    let sse = ev.to_sse_string();
                    if tx.send(Ok(sse)).await.is_err() {
                        return;
                    }
                    if let Some(p) = &log_ir.performance {
                        p.note_client_frame_sent();
                    }
                }
            }
        }

        if !terminal_error_sent {
            match super::streaming::validate_decoded_batch(stream_parser.finish()) {
                Ok(ai_deltas) => {
                    let mut ai_deltas = tool_route_plan.restore_stream_deltas(ai_deltas);
                    hook_state.apply(&mut ai_deltas).await;
                    accumulator.apply_all(&ai_deltas);
                    let events = stream_formatter.format_deltas(&ai_deltas);
                    for ev in events {
                        let sse = ev.to_sse_string();
                        if tx.send(Ok(sse)).await.is_ok()
                            && let Some(p) = &log_ir.performance
                        {
                            p.note_client_frame_sent();
                        }
                    }
                }
                Err(error) => {
                    if let Some(attempt) = &log_ir.performance {
                        attempt.record_failure(
                            "failed",
                            "conversion_parse_error",
                            "response_conversion",
                            error.as_ref(),
                        );
                    }
                    let event = super::stream_error_event(
                        ingress,
                        log_ir
                            .diagnostic
                            .client_request_id
                            .as_deref()
                            .unwrap_or_default(),
                        "conversion_parse_error",
                    );
                    if tx.send(Ok(event)).await.is_ok()
                        && let Some(attempt) = &log_ir.performance
                    {
                        attempt.note_client_frame_sent();
                    }
                    terminal_error_sent = true;
                }
            }
        }

        // Never flush buffered custom inputs after a decoder failure; that would
        // expose incomplete arguments or manufacture a successful final item.
        let mut bridge_deltas = if terminal_error_sent {
            Vec::new()
        } else {
            tool_route_plan.finish_stream()
        };
        if !bridge_deltas.is_empty() {
            hook_state.apply(&mut bridge_deltas).await;
            accumulator.apply_all(&bridge_deltas);
            for ev in stream_formatter.format_deltas(&bridge_deltas) {
                let sse = ev.to_sse_string();
                if tx.send(Ok(sse)).await.is_ok()
                    && let Some(p) = &log_ir.performance
                {
                    p.note_client_frame_sent();
                }
            }
        }

        if !terminal_error_sent {
            if let Some(attempt) = &log_ir.performance {
                let diagnostic = attempt.diagnostic();
                if matches!(diagnostic.attempt_outcome.as_str(), "failed" | "timed_out") {
                    let event = super::stream_error_event(
                        ingress,
                        diagnostic.client_request_id.as_deref().unwrap_or_default(),
                        diagnostic.failure_kind.as_deref().unwrap_or("stream_error"),
                    );
                    if tx.send(Ok(event)).await.is_ok()
                        && let Some(p) = &log_ir.performance
                    {
                        p.note_client_frame_sent();
                    }
                    terminal_error_sent = true;
                }
            }
        }
        let done_events = if terminal_error_sent {
            Vec::new()
        } else {
            stream_formatter.format_done()
        };
        for ev in done_events {
            let sse = ev.to_sse_string();
            if tx.send(Ok(sse)).await.is_ok()
                && let Some(p) = &log_ir.performance
            {
                p.note_client_frame_sent();
            }
        }

        let upstream_latency_ms = upstream_start.elapsed().as_millis() as i64;

        let usage = stream_formatter.usage();
        let mut ai_resp = accumulator.into_ai_response();
        if ai_resp.usage.prompt_tokens == 0 && ai_resp.usage.completion_tokens == 0 {
            ai_resp.usage = usage.clone();
        }
        if ai_resp.id.is_empty() {
            ai_resp.id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
        }
        if ai_resp.model.is_empty() {
            ai_resp.model = act_model_ir.clone();
        }
        if ai_resp.stop_reason.is_none() {
            ai_resp.stop_reason = Some("stop".to_string());
        }

        log_ir
            .status(200)
            .upstream_status(200)
            .usage(ai_resp.usage.clone())
            .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
            .with_upstream_response(200, upstream_hdrs_owned, None, Some(upstream_latency_ms))
            .with_client_response(None, None)
            .stream_metrics(chunks_count, first_chunk_ms)
            .emit();
    });

    let stream = ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassthroughBodyMode {
    Undecided,
    RawSse,
    NonSseJson,
}

fn classify_passthrough_body(bytes: &[u8]) -> Option<PassthroughBodyMode> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("data:")
        || trimmed.starts_with("event:")
        || trimmed.starts_with("id:")
        || trimmed.starts_with("retry:")
        || trimmed.starts_with(':')
    {
        return Some(PassthroughBodyMode::RawSse);
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(PassthroughBodyMode::NonSseJson);
    }
    Some(PassthroughBodyMode::RawSse)
}

fn format_non_sse_stream_response(
    raw: &str,
    egress: ProtocolEndpoint,
    ingress: ProtocolEndpoint,
) -> Option<(String, crate::protocol::ir::AiResponse)> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let ai_resp = egress
        .handler()
        .make_response_decoder()
        .parse_response(value)
        .ok()?;
    let deltas = ai_response_to_deltas(&ai_resp);
    let mut stream_formatter = ingress.handler().make_stream_response_encoder();
    let mut client_sse_parts = Vec::new();

    for ev in stream_formatter.format_deltas(&deltas) {
        client_sse_parts.push(ev.to_sse_string());
    }
    for ev in stream_formatter.format_done() {
        client_sse_parts.push(ev.to_sse_string());
    }

    Some((client_sse_parts.join(""), ai_resp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;

    #[test]
    fn non_sse_gemini_stream_response_is_formatted_as_sse() {
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "hello"}],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0
            }],
            "modelVersion": "gemini-3.5-flash",
            "responseId": "resp-json-stream",
            "usageMetadata": {
                "candidatesTokenCount": 3,
                "promptTokenCount": 5,
                "totalTokenCount": 8
            }
        })
        .to_string();

        let (sse, ai_resp) = format_non_sse_stream_response(
            &raw,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("complete JSON stream response should format as SSE");

        assert!(sse.starts_with("data: "), "SSE must use data frames: {sse}");
        assert!(
            sse.contains("\"usageMetadata\""),
            "terminal SSE must include Gemini usage metadata: {sse}"
        );
        assert_eq!(ai_resp.content, "hello");
        assert_eq!(ai_resp.usage.prompt_tokens, 5);
        assert_eq!(ai_resp.usage.completion_tokens, 3);
        assert_eq!(ai_resp.usage.total_tokens, 8);
    }
}
