//! Response conversion for provider-owned native IR sources. No synthetic SSE
//! decoder sits between the native wire worker and the ingress formatter.

use std::convert::Infallible;
use std::time::Instant;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::conversion::{ConversionAttempt, HealthDisposition, RetryDisposition};
use crate::error::{GatewayError, TimeoutPhase};
use crate::plugin::phase::HostContext;
use crate::protocol::StreamResponseEncoder;
use crate::protocol::codec::tool_bridge::ToolRoutePlan;
use crate::protocol::ir::{AiRequest, AiStreamDelta, Usage};
use crate::provider::native::{IrEventStream, IrUpstream};
use crate::proxy::context::RequestContext;

use super::buffered::{BufferedFinalize, finalize_buffered_response};
use super::stream_probe::stream_delta_is_payload;
use super::streaming::{IrTerminalState, StreamHookState, validate_decoded_batch};
use super::{CallCtx, LogBuilder, RequestExtras, StreamResponseAccumulator};

const MAX_PRECOMMIT_DELTAS: usize = 2048;
const LIMITATIONS: &str = "system_best_effort,inline_images_only,tools_buffered";
type FrameSender = mpsc::Sender<Result<String, Infallible>>;

#[derive(Default)]
struct Progress {
    terminal: IrTerminalState,
    usage: Usage,
    batches: i32,
    first_batch_ms: Option<i64>,
}

impl Progress {
    fn accept(
        &mut self,
        deltas: Vec<AiStreamDelta>,
        started: Instant,
    ) -> Result<Vec<AiStreamDelta>, GatewayError> {
        self.batches = self.batches.saturating_add(1);
        self.first_batch_ms
            .get_or_insert_with(|| started.elapsed().as_millis() as i64);
        self.observe_usage(&deltas);
        self.terminal.accept(deltas).map_err(invalid_ir)
    }

    fn observe_usage(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            if let AiStreamDelta::Usage(usage) = delta {
                self.usage.merge_partial(usage);
            }
        }
    }

    fn log(self, log: LogBuilder, status: u16, started: Instant) {
        let mut log = log
            .status(status)
            .usage(self.usage)
            .stream_metrics(self.batches, self.first_batch_ms);
        log.extras.latency_upstream_ms = Some(started.elapsed().as_millis() as i64);
        log.emit();
    }
}

fn invalid_ir(error: impl std::fmt::Display) -> GatewayError {
    GatewayError::upstream_status("native", 502, Some(error.to_string()))
}

fn timeout_error() -> GatewayError {
    GatewayError::UpstreamTimeout {
        phase: TimeoutPhase::Total,
    }
}

fn with_limitations(mut response: Response) -> Response {
    response.headers_mut().insert(
        "x-nyro-provider-limitations",
        HeaderValue::from_static(LIMITATIONS),
    );
    response
}

/// Read failures are authoritative even after HTTP 200 or a verified Done. The
/// performance observer owns wire completion/capture; this consumer only records
/// failures and client delivery facts, never synthesizes terminal evidence.
fn record_error(log: &mut LogBuilder, error: &GatewayError) -> &'static str {
    let (outcome, kind, stage) = match error {
        GatewayError::ClientCancelled => ("cancelled", "client_cancelled", "client_delivery"),
        GatewayError::UpstreamTimeout { .. } => ("timed_out", "timeout", "upstream_read"),
        GatewayError::StreamParseError { .. } => {
            ("failed", "conversion_parse_error", "response_conversion")
        }
        _ => ("failed", "upstream_error", "upstream_response"),
    };
    if let Some(attempt) = &log.performance {
        attempt.record_failure(outcome, kind, stage, error);
    } else {
        log.diagnostic.record_failure(outcome, kind, stage, error);
    }
    kind
}

/// A hook or wire observer may have recorded an error independently of the
/// yielded IR. Do not let a formatter manufacture success over that evidence.
fn check_performance(
    performance: Option<&crate::performance::Attempt>,
) -> Result<(), GatewayError> {
    if let Some(attempt) = performance {
        match attempt.diagnostic().attempt_outcome.as_str() {
            "timed_out" => return Err(timeout_error()),
            "failed" => return Err(invalid_ir("Native upstream attempt failed")),
            "cancelled" => return Err(GatewayError::ClientCancelled),
            _ => {}
        }
    }
    Ok(())
}

fn failed_attempt(
    mut log: LogBuilder,
    progress: Progress,
    error: GatewayError,
    request_id: &str,
    started: Instant,
) -> ConversionAttempt {
    record_error(&mut log, &error);
    let response = with_limitations(error.render(Some(request_id)));
    let health = super::health_outcome_from_status(response.status().as_u16());
    progress.log(log, response.status().as_u16(), started);
    let retry = if error
        .account_failure()
        .is_some_and(|failure| failure.rejected_before_execution)
    {
        RetryDisposition::DefaultStatusPolicy
    } else {
        RetryDisposition::Never
    };
    ConversionAttempt::new(response, retry, health)
}

/// Buffer framing/usage until model output, a verified terminal, or the bounded
/// delta budget. No tool restoration or response hooks run during the probe.
async fn precommit(
    events: &mut IrEventStream,
    progress: &mut Progress,
    performance: Option<&crate::performance::Attempt>,
    started: Instant,
) -> Result<Vec<AiStreamDelta>, GatewayError> {
    let mut buffered = Vec::new();
    loop {
        let batch = events
            .next()
            .await
            .ok_or_else(|| invalid_ir("Native IR stream ended before payload or Done"))??;
        check_performance(performance)?;
        let deltas = progress.accept(batch, started)?;
        let has_payload = deltas.iter().any(stream_delta_is_payload);
        buffered.extend(deltas);
        if has_payload || progress.terminal.has_done() || buffered.len() >= MAX_PRECOMMIT_DELTAS {
            return Ok(buffered);
        }
    }
}

/// Native attempts never replay automatically: the provider may have accepted
/// Run before a failure was observed. The dispatcher owns health settlement and
/// wraps the final response in its existing performance delivery observer.
pub(super) async fn handle_ir_response(
    upstream: IrUpstream,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    tool_route_plan: ToolRoutePlan,
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &HostContext<'_>,
) -> ConversionAttempt {
    let IrUpstream {
        status,
        headers,
        url,
        actual_model,
        model_metadata,
        mut events,
        guard,
    } = upstream;
    let started = Instant::now();
    let mut log = LogBuilder::from_ctx(call_ctx)
        .with_req_extras(req_extras)
        .upstream_url(&url)
        .upstream_status(i32::from(status))
        .upstream_resp_headers(crate::proxy::observability::headers_to_json(&headers));
    log.upstream_model = actual_model;
    log.diagnostic.payload_metadata[super::native_model_route_key(call_ctx.provider)] =
        model_metadata;
    log.diagnostic.client_request_id = Some(req_ctx.request_id.clone());
    let mut progress = Progress::default();
    if !(200..300).contains(&status) {
        drop(events);
        drop(guard);
        return failed_attempt(
            log,
            progress,
            GatewayError::upstream_status(
                &call_ctx.provider.name,
                if status >= 400 { status } else { 502 },
                None,
            ),
            &req_ctx.request_id,
            started,
        );
    }

    if !call_ctx.is_stream {
        let collected = tokio::select! {
            biased;
            _ = req_ctx.cancellation.cancelled() => Err(GatewayError::ClientCancelled),
            _ = tokio::time::sleep(req_ctx.deadline.remaining()) => Err(timeout_error()),
            result = collect_response(&mut events, &mut progress, log.performance.as_ref(), started) => result,
        };
        // EOF means the source worker has closed its event stream. On any other
        // exit these drops cancel/abort it, including cancellation while awaiting.
        drop(events);
        drop(guard);
        let mut response = match collected {
            Ok(response) => response,
            Err(error) => {
                return failed_attempt(log, progress, error, &req_ctx.request_id, started);
            }
        };
        tool_route_plan.restore_response(&mut response);
        if response.id.is_empty() {
            response.id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        }
        let finalized = finalize_buffered_response(
            call_ctx,
            req_ctx,
            req_ir,
            host,
            response,
            Some(call_ctx.actual_model),
        )
        .await;
        let response = match finalized {
            BufferedFinalize::Continue {
                response, usage, ..
            } => {
                progress.usage = usage;
                let output = call_ctx
                    .ingress
                    .handler()
                    .make_response_encoder()
                    .format_response(&response);
                Json(output).into_response()
            }
            BufferedFinalize::Override(response) => {
                let health = super::health_outcome_from_response(&response);
                progress.log(log, response.status().as_u16(), started);
                return ConversionAttempt::new(
                    with_limitations(response),
                    RetryDisposition::Never,
                    health,
                );
            }
        };
        progress.log(log, response.status().as_u16(), started);
        return ConversionAttempt::new(
            with_limitations(response),
            RetryDisposition::Never,
            HealthDisposition::Success,
        );
    }

    let buffered = tokio::select! {
        biased;
        _ = req_ctx.cancellation.cancelled() => Err(GatewayError::ClientCancelled),
        _ = tokio::time::sleep(req_ctx.deadline.remaining()) => Err(timeout_error()),
        result = precommit(&mut events, &mut progress, log.performance.as_ref(), started) => result,
    };
    let buffered = match buffered {
        Ok(buffered) => buffered,
        Err(error) => {
            drop(events);
            drop(guard);
            return failed_attempt(log, progress, error, &req_ctx.request_id, started);
        }
    };
    let ingress = call_ctx.ingress;
    let request_id = req_ctx.request_id.clone();
    let cancellation = req_ctx.cancellation.clone();
    let deadline = req_ctx.deadline.clone();
    let mut hooks = StreamHookState::capture(req_ctx, req_ir, &call_ctx.gw);
    let mut formatter = ingress.handler().make_stream_response_encoder();
    let mut tool_route_plan = tool_route_plan;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        // The outer select covers every await in the relay, including response
        // hooks and channel backpressure, not just source.next(). Dropping a
        // client body must never leave a blocked native worker in the background.
        let result = tokio::select! {
            biased;
            _ = tx.closed() => Err(GatewayError::ClientCancelled),
            _ = cancellation.cancelled() => Err(GatewayError::ClientCancelled),
            _ = tokio::time::sleep(deadline.remaining()) => Err(timeout_error()),
            result = relay(
                &mut events, buffered, &mut progress, &mut tool_route_plan,
                &mut hooks, formatter.as_mut(), &tx, log.performance.as_ref(), started,
            ) => result,
        };
        drop(events);
        drop(guard);
        if let Err(error) = result {
            let kind = record_error(&mut log, &error);
            if !matches!(error, GatewayError::ClientCancelled) {
                let event = super::stream_error_event(ingress, &request_id, kind);
                // Do not wait past the deadline on a saturated client channel.
                if deadline.is_exceeded() {
                    if tx.try_send(Ok(event)).is_ok()
                        && let Some(attempt) = &log.performance
                    {
                        attempt.note_client_frame_sent();
                    }
                } else {
                    tokio::select! {
                        biased;
                        _ = tx.closed() => {}
                        _ = cancellation.cancelled() => {}
                        _ = tokio::time::sleep(deadline.remaining()) => {}
                        _ = send_frame(&tx, event, log.performance.as_ref()) => {}
                    }
                }
            }
        }
        progress.log(log, 200, started);
    });

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("static native stream response headers");
    ConversionAttempt::new(
        with_limitations(response),
        RetryDisposition::Never,
        HealthDisposition::Deferred,
    )
}

async fn collect_response(
    events: &mut IrEventStream,
    progress: &mut Progress,
    performance: Option<&crate::performance::Attempt>,
    started: Instant,
) -> Result<crate::protocol::ir::AiResponse, GatewayError> {
    let mut accumulator = StreamResponseAccumulator::default();
    while let Some(batch) = events.next().await {
        let deltas = progress.accept(batch?, started)?;
        check_performance(performance)?;
        accumulator.apply_all(&deltas);
    }
    check_performance(performance)?;
    accumulator.apply_all(&progress.terminal.finish().map_err(invalid_ir)?);
    Ok(accumulator.into_ai_response())
}

#[allow(clippy::too_many_arguments)]
async fn relay(
    events: &mut IrEventStream,
    buffered: Vec<AiStreamDelta>,
    progress: &mut Progress,
    tool_route_plan: &mut ToolRoutePlan,
    hooks: &mut StreamHookState,
    formatter: &mut dyn StreamResponseEncoder,
    tx: &FrameSender,
    performance: Option<&crate::performance::Attempt>,
    started: Instant,
) -> Result<(), GatewayError> {
    forward_batch(
        buffered,
        progress,
        tool_route_plan,
        hooks,
        formatter,
        tx,
        performance,
    )
    .await?;
    while let Some(batch) = events.next().await {
        let deltas = progress.accept(batch?, started)?;
        forward_batch(
            deltas,
            progress,
            tool_route_plan,
            hooks,
            formatter,
            tx,
            performance,
        )
        .await?;
    }
    check_performance(performance)?;
    let done = progress.terminal.finish().map_err(invalid_ir)?;
    // Tool bridge flush and the terminal hook happen only after verified Done
    // AND source EOF. On an error neither partial custom inputs nor success
    // terminals are flushed. Done itself makes ToolRoutePlan flush pending input.
    forward_batch(
        done,
        progress,
        tool_route_plan,
        hooks,
        formatter,
        tx,
        performance,
    )
    .await?;
    check_performance(performance)?;
    let mut tail = tool_route_plan.finish_stream();
    hooks.apply(&mut tail).await;
    let tail = validate_decoded_batch(Ok(tail)).map_err(invalid_ir)?;
    check_performance(performance)?;
    progress.observe_usage(&tail);
    for event in formatter.format_deltas(&tail) {
        send_frame(tx, event.to_sse_string(), performance).await?;
    }
    for event in formatter.format_done() {
        send_frame(tx, event.to_sse_string(), performance).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn forward_batch(
    deltas: Vec<AiStreamDelta>,
    progress: &mut Progress,
    tool_route_plan: &mut ToolRoutePlan,
    hooks: &mut StreamHookState,
    formatter: &mut dyn StreamResponseEncoder,
    tx: &FrameSender,
    performance: Option<&crate::performance::Attempt>,
) -> Result<(), GatewayError> {
    check_performance(performance)?;
    let terminal_allowed = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::Done { .. }));
    let mut deltas = tool_route_plan.restore_stream_deltas(deltas);
    hooks.apply(&mut deltas).await;
    let deltas = validate_decoded_batch(Ok(deltas)).map_err(invalid_ir)?;
    if deltas.iter().any(|d| matches!(d, AiStreamDelta::Done { stop_reason } if !terminal_allowed || stop_reason.eq_ignore_ascii_case("error"))) {
        return Err(invalid_ir("Response hook produced an error terminal"));
    }
    check_performance(performance)?;
    progress.observe_usage(&deltas);
    for event in formatter.format_deltas(&deltas) {
        send_frame(tx, event.to_sse_string(), performance).await?;
    }
    Ok(())
}

async fn send_frame(
    tx: &FrameSender,
    frame: String,
    performance: Option<&crate::performance::Attempt>,
) -> Result<(), GatewayError> {
    tx.send(Ok(frame))
        .await
        .map_err(|_| GatewayError::ClientCancelled)?;
    if let Some(attempt) = performance {
        attempt.note_client_frame_sent();
    }
    Ok(())
}

#[cfg(test)]
mod tests;
