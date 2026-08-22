use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::body::Body;
#[cfg(test)]
use axum::http::HeaderMap;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
#[cfg(test)]
use nyro_ccswitch_compat::ConversionProfile;
use nyro_ccswitch_compat::{
    CompatEngine, CompatError, ConversionSession, ConvertedResponse, DecompressError, Direction,
    Header, PreparedRequest, ResponseBody, ResponseMetadata, StreamStartDecision, UpstreamFlavor,
    decompress_body_with_limit, detect_semantic_failure, get_content_encoding,
    strip_hop_by_hop_headers,
};
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

use crate::conversion::{ConversionAttempt, RetryDisposition};
#[cfg(test)]
use crate::db::models::Provider;
use crate::protocol::ids::{ANTHROPIC_MESSAGES_2023_06_01, OPENAI_RESPONSES_V1, ProtocolId};
#[cfg(test)]
use crate::protocol::ids::{
    GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
};
use crate::protocol::ir::{AiRequest, Usage};
use crate::proxy::client::{MAX_UPSTREAM_RESPONSE_BODY_BYTES, ProxyClient, RawUpstreamResponse};
use crate::proxy::context::RequestContext;
use crate::router::health::HealthPermit;

use super::{
    CallCtx, HealthOutcome, LogBuilder, RequestExtras, error_response, health_outcome_from_status,
};

pub(super) type CompatAttempt = ConversionAttempt;

type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;

pub(super) fn normalize_compat_request_headers(
    headers: &mut ReqwestHeaderMap,
    selection: &crate::conversion::RawWireCompatSelection,
    endpoint: &str,
) -> Result<(), String> {
    // Streaming responses cannot be re-encoded mid-flight, so requests that
    // will produce an SSE stream opt out of automatic compression; buffered
    // requests keep it and the ported bounded decompressor decodes the reply.
    let accept = headers
        .get(reqwest::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let upstream_streams = selection.profile.force_upstream_stream();
    if nyro_ccswitch_compat::should_force_identity_encoding(
        endpoint,
        upstream_streams,
        accept.as_deref(),
    ) {
        headers.insert(
            reqwest::header::ACCEPT_ENCODING,
            reqwest::header::HeaderValue::from_static("identity"),
        );
    }

    if matches!(
        selection.profile.direction,
        Direction::CodexResponsesToAnthropic
    ) {
        let names = headers.keys().cloned().collect::<Vec<_>>();
        for name in names {
            if is_codex_client_fingerprint_header(name.as_str()) {
                headers.remove(name);
            }
        }
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers
            .entry(reqwest::header::HeaderName::from_static(
                "anthropic-version",
            ))
            .or_insert(reqwest::header::HeaderValue::from_static("2023-06-01"));
        if selection.context_1m {
            append_header_token(headers, "anthropic-beta", "context-1m-2025-08-07")?;
        }
    }

    if matches!(
        selection.profile.upstream_flavor,
        UpstreamFlavor::CodexOAuthResponses
    ) && selection.identity.client_provided
    {
        let session_id = selection.identity.value.trim();
        if !session_id.is_empty() {
            let value = reqwest::header::HeaderValue::from_str(session_id)
                .map_err(|error| error.to_string())?;
            headers.insert("session_id", value.clone());
            headers.insert("x-client-request-id", value);
            let window_id = reqwest::header::HeaderValue::from_str(&format!("{session_id}:0"))
                .map_err(|error| error.to_string())?;
            headers.insert("x-codex-window-id", window_id);
        }
    }

    Ok(())
}

fn append_header_token(
    headers: &mut ReqwestHeaderMap,
    name: &'static str,
    token: &str,
) -> Result<(), String> {
    let existing = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if existing
        .split(',')
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case(token))
    {
        return Ok(());
    }
    let value = if existing.trim().is_empty() {
        token.to_string()
    } else {
        format!("{existing},{token}")
    };
    headers.insert(
        name,
        reqwest::header::HeaderValue::from_str(&value).map_err(|error| error.to_string())?,
    );
    Ok(())
}

fn is_codex_client_fingerprint_header(name: &str) -> bool {
    matches!(
        name,
        "originator"
            | "session_id"
            | "session-id"
            | "thread-id"
            | "conversation_id"
            | "chatgpt-account-id"
            | "x-openai-subagent"
            | "x-client-request-id"
            | "openai-beta"
            | "openai-organization"
            | "openai-project"
    ) || name.starts_with("x-stainless-")
        || name.starts_with("x-codex-")
}

#[cfg(test)]
fn strip_one_m_suffix(model: &str) -> &str {
    crate::conversion::strip_one_m_suffix(model)
}

#[cfg(test)]
fn chat_prompt_cache_key_supported(base_url: &str) -> bool {
    crate::conversion::chat_prompt_cache_key_supported(base_url)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_compat(
    client: ProxyClient,
    url: &str,
    headers: ReqwestHeaderMap,
    prepared: PreparedRequest,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &crate::plugin::phase::HostContext<'_>,
    health_permit: HealthPermit,
) -> CompatAttempt {
    let log = LogBuilder::from_ctx(call_ctx)
        .with_req_extras(req_extras)
        .upstream_url(url);
    let upstream_req_headers = crate::proxy::observability::reqwest_headers_to_json(&headers);
    let upstream_req_body = Some(String::from_utf8_lossy(&prepared.body).into_owned());
    let upstream_start = std::time::Instant::now();

    let response = match client
        .call_stream_raw(url, headers.clone(), prepared.body.clone())
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let message = format!("upstream error: {error:#}");
            let response = codex_ingress_error_body(
                &prepared.session,
                call_ctx,
                req_extras,
                None,
                None,
                Some(&message),
            )
            .map(|body| {
                build_compat_response(
                    ResponseMetadata::new(502).rebuilt("application/json"),
                    Body::from(body),
                )
            })
            .unwrap_or_else(|| error_response(502, &message));
            log.status(502)
                .with_upstream_request(upstream_req_headers, upstream_req_body)
                .resp_body(Some(
                    serde_json::json!({"error": {"message": message.clone()}}).to_string(),
                ))
                .emit();
            return CompatAttempt {
                response,
                retry: RetryDisposition::ForceRetry,
                health: HealthOutcome::Failure,
            };
        }
    };

    let status = response.status().as_u16();
    let raw_headers = response.headers().clone();
    let upstream_headers = crate::proxy::observability::headers_to_json(&raw_headers);
    let metadata = response_metadata(status, &raw_headers);
    let live_stream =
        (200..300).contains(&status) && should_stream_response(&prepared.session, &metadata);

    if !live_stream {
        let raw = match ProxyClient::buffer_response(response).await {
            Ok(raw) => raw,
            Err(error) => {
                let message = format!("upstream response read error: {error:#}");
                log.status(502)
                    .with_upstream_request(upstream_req_headers, upstream_req_body)
                    .with_upstream_response(
                        status as i32,
                        upstream_headers,
                        None,
                        Some(upstream_start.elapsed().as_millis() as i64),
                    )
                    .resp_body(Some(
                        serde_json::json!({"error": {"message": message.clone()}}).to_string(),
                    ))
                    .emit();
                return CompatAttempt {
                    response: error_response(502, &message),
                    retry: RetryDisposition::ForceRetry,
                    health: HealthOutcome::Failure,
                };
            }
        };
        return handle_buffered_compat(
            call_ctx.gw.compat_engine.as_ref(),
            &prepared.session,
            raw,
            log,
            upstream_req_headers,
            upstream_req_body,
            upstream_start,
            call_ctx,
            req_extras,
            req_ctx,
            req_ir,
            host,
        )
        .await;
    }

    let primed = match prime_stream(
        call_ctx.gw.compat_engine.as_ref(),
        &prepared.session,
        response,
        req_ctx,
        upstream_start,
    )
    .await
    {
        Ok(primed) => primed,
        Err(error) => {
            let status = error.http_status();
            let message = error.to_string();
            let response = codex_compat_error_response(
                &prepared.session,
                call_ctx,
                req_extras,
                status,
                &message,
            );
            log.status(status)
                .with_upstream_request(upstream_req_headers, upstream_req_body)
                .with_upstream_response(
                    200,
                    upstream_headers,
                    None,
                    Some(upstream_start.elapsed().as_millis() as i64),
                )
                .resp_body(Some(proxy_error_body(&message)))
                .emit();
            return CompatAttempt {
                response,
                retry: RetryDisposition::ForceRetry,
                health: HealthOutcome::Failure,
            };
        }
    };

    let upstream_raw = Arc::new(Mutex::new(Vec::new()));
    let upstream_raw_observer = upstream_raw.clone();
    let observed = primed.stream.map(move |item| {
        if let Ok(bytes) = &item
            && let Ok(mut buffer) = upstream_raw_observer.lock()
        {
            buffer.extend_from_slice(bytes);
        }
        item
    });
    let converted = match call_ctx.gw.compat_engine.convert_stream_response(
        &prepared.session,
        primed.metadata,
        observed,
    ) {
        Ok(converted) => converted,
        Err(error) => {
            let status = error.http_status();
            let message = error.to_string();
            let response = codex_compat_error_response(
                &prepared.session,
                call_ctx,
                req_extras,
                status,
                &message,
            );
            log.status(status)
                .with_upstream_request(upstream_req_headers, upstream_req_body)
                .with_upstream_response(
                    200,
                    upstream_headers,
                    None,
                    Some(upstream_start.elapsed().as_millis() as i64),
                )
                .resp_body(Some(proxy_error_body(&message)))
                .emit();
            return CompatAttempt {
                response,
                retry: RetryDisposition::ForceRetry,
                health: HealthOutcome::Neutral,
            };
        }
    };

    build_streaming_compat_response(
        converted,
        upstream_raw,
        log,
        upstream_req_headers,
        upstream_req_body,
        upstream_headers,
        upstream_start,
        primed.first_chunk_ms,
        call_ctx,
        req_ctx,
        req_ir,
        health_permit,
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_buffered_compat(
    engine: &CompatEngine,
    session: &ConversionSession,
    raw: RawUpstreamResponse,
    log: LogBuilder,
    upstream_req_headers: Option<String>,
    upstream_req_body: Option<String>,
    upstream_start: std::time::Instant,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &crate::plugin::phase::HostContext<'_>,
) -> CompatAttempt {
    let upstream_latency_ms = upstream_start.elapsed().as_millis() as i64;
    let upstream_headers = crate::proxy::observability::headers_to_json(&raw.headers);
    let upstream_body = Some(String::from_utf8_lossy(&raw.body).into_owned());
    let (metadata, body) = match decode_response_body(raw.status, &raw.headers, raw.body) {
        Ok(decoded) => decoded,
        Err(error) => {
            let message = error.to_string();
            log.status(502)
                .with_upstream_request(upstream_req_headers, upstream_req_body)
                .with_upstream_response(
                    raw.status as i32,
                    upstream_headers,
                    upstream_body,
                    Some(upstream_latency_ms),
                )
                .resp_body(Some(proxy_error_body(&message)))
                .emit();
            return CompatAttempt {
                response: error_response(502, &message),
                retry: RetryDisposition::ForceRetry,
                health: HealthOutcome::Failure,
            };
        }
    };

    if (200..300).contains(&metadata.status)
        && let Some(message) = detect_semantic_failure(session, &body)
    {
        let client_body =
            codex_ingress_error_body(session, call_ctx, req_extras, None, None, Some(&message))
                .unwrap_or_else(|| Bytes::from(proxy_error_body(&message)));
        log.status(422)
            .with_upstream_request(upstream_req_headers, upstream_req_body)
            .with_upstream_response(
                metadata.status as i32,
                upstream_headers,
                upstream_body,
                Some(upstream_latency_ms),
            )
            .resp_body(Some(proxy_error_body(&message)))
            .emit();
        return CompatAttempt {
            response: build_compat_response(
                ResponseMetadata::new(422).rebuilt("application/json"),
                Body::from(client_body),
            ),
            retry: RetryDisposition::ForceRetry,
            health: HealthOutcome::Failure,
        };
    }

    if !(200..300).contains(&metadata.status) {
        // Codex-semantics clients get cc-switch's normalized error envelope
        // (413 upstream-size guidance, provider/model/endpoint context,
        // nonstandard upstream bodies normalized); other clients keep the
        // upstream body as-is. Retryability is decided by the outer loop's
        // `is_retryable(status)`.
        let health_outcome = health_outcome_from_status(metadata.status);
        if let Some(client_body) = codex_ingress_error_body(
            session,
            call_ctx,
            req_extras,
            Some(metadata.status),
            Some(&body),
            None,
        ) {
            log.status(metadata.status)
                .with_upstream_request(upstream_req_headers, upstream_req_body)
                .with_upstream_response(
                    raw.status as i32,
                    upstream_headers,
                    upstream_body,
                    Some(upstream_latency_ms),
                )
                .with_client_response(
                    None,
                    Some(String::from_utf8_lossy(&client_body).into_owned()),
                )
                .emit();
            return CompatAttempt {
                response: build_compat_response(
                    metadata.rebuilt("application/json"),
                    Body::from(client_body),
                ),
                retry: RetryDisposition::DefaultStatusPolicy,
                health: health_outcome,
            };
        }
        return CompatAttempt {
            response: build_compat_response(metadata, Body::from(body)),
            retry: RetryDisposition::DefaultStatusPolicy,
            health: health_outcome,
        };
    }

    let converted = match engine
        .convert_buffered_response(session, metadata, body)
        .await
    {
        Ok(converted) => converted,
        Err(error) => {
            let status = error.http_status();
            let message = error.to_string();
            log.status(status)
                .with_upstream_request(upstream_req_headers, upstream_req_body)
                .with_upstream_response(
                    raw.status as i32,
                    upstream_headers,
                    upstream_body,
                    Some(upstream_latency_ms),
                )
                .resp_body(Some(proxy_error_body(&message)))
                .emit();
            return CompatAttempt {
                response: compat_error_response(status, &message),
                retry: RetryDisposition::ForceRetry,
                health: HealthOutcome::Neutral,
            };
        }
    };

    let ConvertedResponse {
        metadata,
        body,
        usage: _,
    } = converted;
    let ResponseBody::Buffered(body) = body else {
        let message = "compat buffered conversion returned a stream";
        return CompatAttempt {
            response: compat_error_response(500, message),
            retry: RetryDisposition::ForceRetry,
            health: HealthOutcome::Neutral,
        };
    };

    let (body, usage) =
        match finalize_compat_buffered_body(call_ctx, req_ctx, req_ir, host, metadata.status, body)
            .await
        {
            BufferedWireFinalize::Continue { body, usage } => (body, usage),
            BufferedWireFinalize::Override(response) => {
                return CompatAttempt {
                    response,
                    retry: RetryDisposition::DefaultStatusPolicy,
                    health: HealthOutcome::Neutral,
                };
            }
        };

    let client_body = Some(String::from_utf8_lossy(&body).into_owned());
    log.status(metadata.status)
        .usage(usage)
        .with_upstream_request(upstream_req_headers, upstream_req_body)
        .with_upstream_response(
            raw.status as i32,
            upstream_headers,
            upstream_body,
            Some(upstream_latency_ms),
        )
        .with_client_response(None, client_body)
        .emit();

    CompatAttempt {
        response: build_compat_response(metadata, Body::from(body)),
        retry: RetryDisposition::DefaultStatusPolicy,
        health: HealthOutcome::Success,
    }
}

enum BufferedWireFinalize {
    Continue { body: Bytes, usage: Usage },
    Override(Response),
}

async fn finalize_compat_buffered_body(
    call_ctx: &CallCtx<'_>,
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &crate::plugin::phase::HostContext<'_>,
    status: u16,
    body: Bytes,
) -> BufferedWireFinalize {
    if !(200..300).contains(&status) {
        return BufferedWireFinalize::Continue {
            body,
            usage: Usage::default(),
        };
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return BufferedWireFinalize::Continue {
            body,
            usage: Usage::default(),
        };
    };
    let parser = call_ctx.ingress.handler().make_response_decoder();
    let Ok(response) = parser.parse_response(value) else {
        return BufferedWireFinalize::Continue {
            body,
            usage: Usage::default(),
        };
    };

    match super::buffered::finalize_buffered_response(
        call_ctx, req_ctx, req_ir, host, response, None,
    )
    .await
    {
        super::buffered::BufferedFinalize::Override(response) => {
            BufferedWireFinalize::Override(response)
        }
        super::buffered::BufferedFinalize::Continue {
            usage,
            mutated: false,
            ..
        } => BufferedWireFinalize::Continue { body, usage },
        super::buffered::BufferedFinalize::Continue {
            response,
            usage,
            mutated: true,
        } => {
            let value = call_ctx
                .ingress
                .handler()
                .make_response_encoder()
                .format_response(&response);
            let body = serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body);
            BufferedWireFinalize::Continue { body, usage }
        }
    }
}

struct PrimedStream {
    metadata: ResponseMetadata,
    stream: UpstreamByteStream,
    first_chunk_ms: Option<i64>,
}

async fn prime_stream(
    engine: &CompatEngine,
    session: &ConversionSession,
    response: reqwest::Response,
    req_ctx: &RequestContext,
    started_at: std::time::Instant,
) -> Result<PrimedStream, CompatError> {
    const MAX_PRIME_BYTES: usize = 256 * 1024;

    let status = response.status().as_u16();
    let metadata = response_metadata(status, response.headers());
    let mut stream = Box::pin(response.bytes_stream());
    let mut replay = Vec::new();
    let mut buffered = Vec::new();
    let mut first_chunk_ms = None;

    loop {
        let remaining = req_ctx.deadline.remaining();
        if remaining.is_zero() {
            return Err(CompatError::Stream(
                "upstream stream timed out before producing output".to_string(),
            ));
        }
        let next = tokio::time::timeout(remaining, stream.next())
            .await
            .map_err(|_| {
                CompatError::Stream("upstream stream timed out before producing output".to_string())
            })?;
        let Some(item) = next else {
            return match engine.inspect_stream_end(session, &buffered) {
                StreamStartDecision::Ready => {
                    let replay = futures::stream::iter(replay.into_iter().map(Ok));
                    Ok(PrimedStream {
                        metadata,
                        stream: Box::pin(replay),
                        first_chunk_ms,
                    })
                }
                StreamStartDecision::Failed(message) => Err(CompatError::Stream(message)),
                StreamStartDecision::Pending => Err(CompatError::Stream(
                    "upstream stream ended before producing output".to_string(),
                )),
            };
        };
        let chunk = item.map_err(|error| CompatError::Stream(error.to_string()))?;
        if first_chunk_ms.is_none() {
            first_chunk_ms = Some(started_at.elapsed().as_millis() as i64);
        }
        buffered.extend_from_slice(&chunk);
        replay.push(chunk);

        match engine.inspect_stream_start(session, &buffered) {
            StreamStartDecision::Ready => break,
            StreamStartDecision::Failed(message) => return Err(CompatError::Stream(message)),
            StreamStartDecision::Pending => {}
        }
        if buffered.len() >= MAX_PRIME_BYTES {
            break;
        }
    }

    let replay = futures::stream::iter(replay.into_iter().map(Ok));
    Ok(PrimedStream {
        metadata,
        stream: Box::pin(replay.chain(stream)),
        first_chunk_ms,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_streaming_compat_response(
    converted: ConvertedResponse,
    upstream_raw: Arc<Mutex<Vec<u8>>>,
    log: LogBuilder,
    upstream_req_headers: Option<String>,
    upstream_req_body: Option<String>,
    upstream_headers: Option<String>,
    upstream_start: std::time::Instant,
    first_chunk_ms: Option<i64>,
    call_ctx: &CallCtx<'_>,
    req_ctx: &RequestContext,
    req_ir: &AiRequest,
    health_permit: HealthPermit,
) -> CompatAttempt {
    let ConvertedResponse {
        metadata,
        body,
        usage: _,
    } = converted;
    let ResponseBody::Stream(mut stream) = body else {
        let message = "compat streaming conversion returned a buffered body";
        return CompatAttempt {
            response: compat_error_response(500, message),
            retry: RetryDisposition::ForceRetry,
            health: HealthOutcome::Neutral,
        };
    };

    let response_metadata = metadata.clone();
    let status = metadata.status;
    let client_protocol = call_ctx.ingress;
    let actual_model = call_ctx.actual_model.to_string();
    let request_context = req_ctx.clone();
    let mut hook_state = super::streaming::StreamHookState::capture(req_ctx, req_ir, &call_ctx.gw);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);

    tokio::spawn(async move {
        let mut bridge = crate::proxy::stream::StreamBridge::new(&request_context);
        bridge.on_connected();
        let mut parser = client_protocol.handler().make_stream_response_decoder();
        let mut formatter = client_protocol.handler().make_stream_response_encoder();
        let mut accumulator = super::StreamResponseAccumulator::default();
        let mut terminal = StreamTerminal::default();
        let mut client_body = Vec::new();
        let mut chunks = 0_i32;
        let mut stream_error = None;
        let mut upstream_ended = false;

        loop {
            let item = tokio::select! {
                biased;
                _ = tx.closed() => {
                    request_context.cancellation.cancel();
                    let _ = bridge.push_chunk(Ok(()));
                    stream_error = Some("client disconnected".to_string());
                    break;
                }
                _ = tokio::time::sleep(request_context.deadline.remaining()) => {
                    let message = "upstream stream deadline exceeded".to_string();
                    let _ = bridge.push_chunk(Err(crate::proxy::stream::StreamFailure::Timeout));
                    stream_error = Some(message);
                    break;
                }
                item = stream.next() => item,
            };
            let Some(item) = item else {
                upstream_ended = true;
                break;
            };
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(error) => {
                    stream_error = Some(error.to_string());
                    bridge.on_read_error(error.to_string());
                    break;
                }
            };
            observe_terminal(&mut terminal, &bytes, client_protocol);
            let text = String::from_utf8_lossy(&bytes);
            let outgoing = if let Ok(mut deltas) = parser.parse_chunk(&text) {
                if hook_state.is_empty() {
                    accumulator.apply_all(&deltas);
                    bytes
                } else {
                    let before = format!("{deltas:?}");
                    hook_state.apply(&mut deltas).await;
                    accumulator.apply_all(&deltas);
                    let events = formatter.format_deltas(&deltas);
                    if before == format!("{deltas:?}") {
                        bytes
                    } else {
                        Bytes::from(
                            events
                                .iter()
                                .map(crate::protocol::SseEvent::to_sse_string)
                                .collect::<String>(),
                        )
                    }
                }
            } else {
                bytes
            };
            client_body.extend_from_slice(&outgoing);
            let sent = tokio::select! {
                biased;
                _ = tx.closed() => false,
                _ = tokio::time::sleep(request_context.deadline.remaining()) => false,
                result = tx.send(Ok(outgoing)) => result.is_ok(),
            };
            if !sent {
                request_context.cancellation.cancel();
                let _ = bridge.push_chunk(Ok(()));
                stream_error = Some("client disconnected".to_string());
                break;
            }
            chunks += 1;
            if bridge.push_chunk(Ok(())).is_err() {
                break;
            }
        }

        terminal.finish(client_protocol);
        if let Ok(mut deltas) = parser.finish() {
            let before = format!("{deltas:?}");
            if !hook_state.is_empty() {
                hook_state.apply(&mut deltas).await;
            }
            accumulator.apply_all(&deltas);
            if !hook_state.is_empty() && before != format!("{deltas:?}") {
                let events = formatter.format_deltas(&deltas);
                let outgoing = Bytes::from(
                    events
                        .iter()
                        .map(crate::protocol::SseEvent::to_sse_string)
                        .collect::<String>(),
                );
                client_body.extend_from_slice(&outgoing);
                let sent = tokio::select! {
                    biased;
                    _ = tx.closed() => false,
                    _ = tokio::time::sleep(request_context.deadline.remaining()) => false,
                    result = tx.send(Ok(outgoing)) => result.is_ok(),
                };
                if sent {
                    chunks += 1;
                } else {
                    request_context.cancellation.cancel();
                    let _ = bridge.push_chunk(Ok(()));
                    stream_error = Some("client disconnected".to_string());
                }
            }
        }

        let stream_completed =
            stream_error.is_none() && terminal.error.is_none() && terminal.success;
        let provider_error = terminal.error.is_some() || (upstream_ended && !terminal.success);
        match stream_health_outcome(
            bridge.state(),
            stream_completed,
            provider_error,
            tx.is_closed(),
        ) {
            StreamHealthOutcome::Neutral => health_permit.neutral(),
            StreamHealthOutcome::Success => {
                bridge.finish();
                health_permit.success();
            }
            StreamHealthOutcome::Failure => {
                let message = terminal
                    .error
                    .clone()
                    .or_else(|| stream_error.clone())
                    .unwrap_or_else(|| "compat stream ended without a terminal event".to_string());
                if stream_error.is_none() {
                    let _ = bridge.push_chunk(Err(crate::proxy::stream::StreamFailure::parse(
                        Some(message.clone()),
                    )));
                }
                health_permit.failure();
                if stream_error.is_none() {
                    stream_error = Some(message);
                }
            }
        }

        let mut response = accumulator.into_ai_response();
        if response.model.is_empty() {
            response.model = actual_model;
        }
        let upstream_body = upstream_raw
            .lock()
            .ok()
            .map(|buffer| String::from_utf8_lossy(&buffer).into_owned());
        log.status(status)
            .upstream_status(status as i32)
            .usage(response.usage)
            .maybe_error(stream_error)
            .with_upstream_request(upstream_req_headers, upstream_req_body)
            .with_upstream_response(
                status as i32,
                upstream_headers,
                upstream_body,
                Some(upstream_start.elapsed().as_millis() as i64),
            )
            .with_client_response(
                None,
                Some(String::from_utf8_lossy(&client_body).into_owned()),
            )
            .stream_metrics(chunks, first_chunk_ms)
            .emit();
    });

    CompatAttempt {
        response: build_compat_response(
            response_metadata,
            Body::from_stream(ReceiverStream::new(rx)),
        ),
        retry: RetryDisposition::DefaultStatusPolicy,
        health: HealthOutcome::Deferred,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StreamHealthOutcome {
    Success,
    Failure,
    Neutral,
}

fn stream_health_outcome(
    state: &crate::proxy::stream::StreamState,
    completed: bool,
    provider_error: bool,
    client_disconnected: bool,
) -> StreamHealthOutcome {
    match state {
        crate::proxy::stream::StreamState::Failed(
            crate::proxy::stream::StreamFailure::ClientCancelled,
        ) if !provider_error => StreamHealthOutcome::Neutral,
        crate::proxy::stream::StreamState::Failed(_) => StreamHealthOutcome::Failure,
        _ if provider_error => StreamHealthOutcome::Failure,
        _ if client_disconnected => StreamHealthOutcome::Neutral,
        _ if completed => StreamHealthOutcome::Success,
        _ => StreamHealthOutcome::Failure,
    }
}

#[derive(Default)]
struct StreamTerminal {
    buffer: String,
    success: bool,
    error: Option<String>,
}

impl StreamTerminal {
    fn finish(&mut self, protocol: ProtocolId) {
        if !self.buffer.trim().is_empty() {
            let block = std::mem::take(&mut self.buffer);
            self.observe_block(&block, protocol);
        }
    }

    fn observe_block(&mut self, block: &str, protocol: ProtocolId) {
        let mut event_name = None;
        let mut data = Vec::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event_name = Some(value.trim());
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.trim_start());
            }
        }
        let value = serde_json::from_str::<Value>(&data.join("\n")).ok();
        let event = event_name
            .or_else(|| value.as_ref()?.get("type")?.as_str())
            .unwrap_or_default();
        if protocol == ANTHROPIC_MESSAGES_2023_06_01 {
            match event {
                "message_stop" => self.success = true,
                "error" => {
                    self.error = Some("Anthropic compat stream emitted an error".to_string())
                }
                _ => {}
            }
        } else if protocol == OPENAI_RESPONSES_V1 {
            match event {
                "response.completed" | "response.incomplete" => self.success = true,
                "response.failed" | "error" => {
                    self.error = Some("Responses compat stream emitted an error".to_string())
                }
                _ => {}
            }
        }
    }
}

fn observe_terminal(terminal: &mut StreamTerminal, bytes: &[u8], protocol: ProtocolId) {
    terminal.buffer.push_str(&String::from_utf8_lossy(bytes));
    while let Some((block, consumed)) = take_sse_block(&terminal.buffer) {
        terminal.observe_block(&block, protocol);
        terminal.buffer.drain(..consumed);
    }
}

fn take_sse_block(buffer: &str) -> Option<(String, usize)> {
    let lf = buffer.find("\n\n").map(|index| (index, 2));
    let crlf = buffer.find("\r\n\r\n").map(|index| (index, 4));
    let (index, delimiter) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(found), None) | (None, Some(found)) => found,
        (None, None) => return None,
    };
    Some((buffer[..index].to_string(), index + delimiter))
}

fn should_stream_response(session: &ConversionSession, metadata: &ResponseMetadata) -> bool {
    let is_sse = metadata.content_type.as_deref().is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
    });
    if matches!(session.profile.direction, Direction::XaiResponsesNative) {
        return is_sse;
    }
    if !session.profile.client_stream
        && matches!(
            session.profile.upstream_flavor,
            UpstreamFlavor::CodexOAuthResponses
        )
    {
        return false;
    }
    session.profile.client_stream || is_sse
}

fn response_metadata(status: u16, headers: &ReqwestHeaderMap) -> ResponseMetadata {
    let mut metadata = ResponseMetadata::new(status);
    metadata.content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    for name in headers.keys() {
        for value in headers.get_all(name).iter() {
            metadata.headers.push(Header::new(
                name.as_str(),
                Bytes::copy_from_slice(value.as_bytes()),
            ));
        }
    }
    strip_hop_by_hop_headers(&mut metadata.headers);
    metadata
}

fn decode_response_body(
    status: u16,
    headers: &ReqwestHeaderMap,
    body: Bytes,
) -> Result<(ResponseMetadata, Bytes), DecompressError> {
    let mut metadata = response_metadata(status, headers);
    let Some(encoding) = get_content_encoding(&metadata.headers) else {
        return Ok((metadata, body));
    };
    match decompress_body_with_limit(&encoding, &body, MAX_UPSTREAM_RESPONSE_BODY_BYTES)? {
        Some(decoded) => {
            metadata.headers.retain(|header| {
                !matches!(
                    header.name.to_ascii_lowercase().as_str(),
                    "content-length" | "content-encoding" | "transfer-encoding"
                )
            });
            Ok((metadata, Bytes::from(decoded)))
        }
        None => Ok((metadata, body)),
    }
}

fn build_compat_response(metadata: ResponseMetadata, body: Body) -> Response {
    let status = StatusCode::from_u16(metadata.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status).body(body).unwrap();
    let headers = response.headers_mut();
    for compat_header in metadata.headers {
        let Ok(name) = HeaderName::from_bytes(compat_header.name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_bytes(&compat_header.value) else {
            continue;
        };
        headers.append(name, value);
    }
    if !headers.contains_key(header::CONTENT_TYPE)
        && let Some(content_type) = metadata.content_type
        && let Ok(value) = HeaderValue::from_str(&content_type)
    {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
        && !headers.contains_key(header::CACHE_CONTROL)
    {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    response
}

fn proxy_error_body(message: &str) -> String {
    serde_json::json!({
        "error": {
            "message": message,
            "type": "proxy_error"
        }
    })
    .to_string()
}

/// cc-switch's Codex client error envelope for Codex-semantics ingresses.
/// Returns `None` for Anthropic-semantics clients, which keep raw upstream
/// error bodies.
fn codex_ingress_error_body(
    session: &ConversionSession,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    upstream_status: Option<u16>,
    upstream_body: Option<&[u8]>,
    local_cause: Option<&str>,
) -> Option<Bytes> {
    if !matches!(
        session.profile.client_semantics,
        nyro_ccswitch_compat::ClientSemantics::CodexResponses
    ) {
        return None;
    }
    Some(nyro_ccswitch_compat::codex_client_error_json(
        &call_ctx.provider.name,
        call_ctx.actual_model,
        &req_extras.path,
        upstream_status,
        upstream_body,
        local_cause,
    ))
}

fn compat_error_response(status: u16, message: &str) -> Response {
    let metadata = ResponseMetadata::new(status).rebuilt("application/json");
    build_compat_response(metadata, Body::from(proxy_error_body(message)))
}

/// cc-switch's Codex error envelope when the client speaks Codex Responses
/// semantics; the generic proxy error shape otherwise.
fn codex_compat_error_response(
    session: &ConversionSession,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    status: u16,
    message: &str,
) -> Response {
    if let Some(body) =
        codex_ingress_error_body(session, call_ctx, req_extras, None, None, Some(message))
    {
        return build_compat_response(
            ResponseMetadata::new(status).rebuilt("application/json"),
            Body::from(body),
        );
    }
    compat_error_response(status, message)
}

#[cfg(test)]
fn request_patch(baseline: &AiRequest, current: &AiRequest) -> Result<Option<Bytes>, String> {
    crate::conversion::request_patch(baseline, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::request::{Message, MessageContent, Role};
    use crate::proxy::context::RequestOutcome;

    #[test]
    fn cancelled_stream_is_neutral_for_provider_health() {
        let state = crate::proxy::stream::StreamState::Failed(
            crate::proxy::stream::StreamFailure::ClientCancelled,
        );

        assert_eq!(
            stream_health_outcome(&state, false, false, false),
            StreamHealthOutcome::Neutral
        );
        assert_eq!(
            stream_health_outcome(&state, true, false, false),
            StreamHealthOutcome::Neutral
        );
        assert_eq!(
            stream_health_outcome(&state, false, true, true),
            StreamHealthOutcome::Failure
        );
    }

    #[test]
    fn stream_failure_cannot_be_overridden_by_a_terminal_success_event() {
        let state = crate::proxy::stream::StreamState::Failed(
            crate::proxy::stream::StreamFailure::upstream("connection reset"),
        );

        assert_eq!(
            stream_health_outcome(&state, true, false, true),
            StreamHealthOutcome::Failure
        );
    }

    #[test]
    fn closed_response_channel_is_neutral_for_provider_health() {
        assert_eq!(
            stream_health_outcome(
                &crate::proxy::stream::StreamState::Streaming,
                true,
                false,
                true,
            ),
            StreamHealthOutcome::Neutral
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn select_via_resolver(
        ingress: ProtocolId,
        egress: ProtocolId,
        provider: &Provider,
        egress_base_url: &str,
        actual_model: &str,
        client_stream: bool,
        headers: &HeaderMap,
        raw_body: &[u8],
        baseline_request: &AiRequest,
        current_request: &AiRequest,
    ) -> Result<Option<crate::conversion::RawWireCompatSelection>, String> {
        crate::conversion::resolve_raw_wire_compat(crate::conversion::ResolveRawWireCompatInput {
            ingress,
            egress,
            provider,
            egress_base_url,
            actual_model,
            client_stream,
            headers,
            raw_body,
            baseline_request,
            current_request,
        })
    }

    fn provider(vendor: &str, channel: &str) -> Provider {
        Provider {
            id: "provider-test".into(),
            name: vendor.into(),
            vendor: Some(vendor.into()),
            protocol: "openai-compatible".into(),
            base_url: "https://example.com/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: Some(channel.into()),
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn request(model: &str, source: ProtocolId) -> AiRequest {
        let message = Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let mut request = AiRequest::new(model, vec![message]);
        request.meta.source_protocol = Some(source);
        request
    }

    #[test]
    fn selects_codex_oauth_responses_flavor() {
        let request = request("virtual", ANTHROPIC_MESSAGES_2023_06_01);
        let selected = select_via_resolver(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_RESPONSES_V1,
            &provider("openai", "codex"),
            "https://chatgpt.com/backend-api/codex",
            "gpt-5",
            false,
            &HeaderMap::new(),
            br#"{"model":"virtual","messages":[{"role":"user","content":"hello"}]}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::CodexOAuthResponses
        );
        assert!(selected.profile.force_upstream_stream());
    }

    #[test]
    fn enables_fast_mode_for_codex_oauth_responses() {
        let request = request("virtual", ANTHROPIC_MESSAGES_2023_06_01);
        let mut codex = provider("openai", "codex");
        codex.fast_mode = true;
        let selected = select_via_resolver(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_RESPONSES_V1,
            &codex,
            "https://chatgpt.com/backend-api/codex",
            "gpt-5",
            false,
            &HeaderMap::new(),
            br#"{"model":"virtual","messages":[{"role":"user","content":"hello"}]}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert!(selected.profile.codex_fast_mode);
    }

    #[test]
    fn selects_xai_native_only_for_responses_ingress() {
        let request = request("grok-4", OPENAI_RESPONSES_V1);
        let selected = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &provider("xai", "default"),
            "https://api.x.ai/v1",
            "grok-4",
            false,
            &HeaderMap::new(),
            br#"{"model":"grok-4","input":"hello"}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::XaiStrictResponses
        );
    }

    #[test]
    fn hook_patch_only_contains_changed_fields() {
        let baseline = request("virtual", OPENAI_RESPONSES_V1);
        let mut current = baseline.clone();
        current.generation.temperature = Some(0.25);
        let patch = request_patch(&baseline, &current).unwrap().unwrap();
        let patch: Value = serde_json::from_slice(&patch).unwrap();
        assert!(patch.as_array().unwrap().iter().any(|entry| {
            entry["op"] == "set"
                && entry["path"] == json!(["temperature"])
                && entry["value"] == json!(0.25)
        }));
    }

    #[test]
    fn prompt_cache_auto_detection_matches_source_rules() {
        assert!(chat_prompt_cache_key_supported("https://api.openai.com/v1"));
        assert!(chat_prompt_cache_key_supported(
            "https://api.kimi.com/coding/v1"
        ));
        assert!(!chat_prompt_cache_key_supported(
            "https://strict.example.com/v1"
        ));
    }

    #[test]
    fn strips_one_m_suffix_case_insensitively() {
        assert_eq!(strip_one_m_suffix("claude-sonnet-4[1m]"), "claude-sonnet-4");
        assert_eq!(
            strip_one_m_suffix("claude-sonnet-4 [1M]  "),
            "claude-sonnet-4"
        );
        assert_eq!(strip_one_m_suffix("claude-sonnet-4"), "claude-sonnet-4");
        assert_eq!(
            strip_one_m_suffix("claude-sonnet-4 [1m] extra"),
            "claude-sonnet-4 [1m] extra"
        );
    }

    async fn read_mock_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;

        let mut request = Vec::new();
        let mut expected_len = None;
        loop {
            let mut buf = [0_u8; 1024];
            let read = socket.read(&mut buf).await.expect("read mock request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
            if expected_len.is_none()
                && let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let content_length = std::str::from_utf8(&request[..header_end])
                    .unwrap()
                    .lines()
                    .skip(1)
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                    })
                    .flatten()
                    .unwrap_or_default();
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|len| request.len() >= len) {
                break;
            }
        }
        request
    }

    async fn serve_once(
        status: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind compatibility mock");
        let addr = listener.local_addr().expect("mock local addr");
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept mock request");
            let request = read_mock_request(&mut socket).await;
            let _ = request_tx.send(request);
            let response = format!(
                "{status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write mock headers");
            socket.write_all(&body).await.expect("write mock body");
            socket.shutdown().await.expect("shutdown mock");
        });
        (format!("http://{addr}/v1/chat/completions"), request_rx)
    }

    async fn serve_hanging_sse(
        first_event: &'static [u8],
    ) -> (String, tokio::sync::oneshot::Sender<()>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hanging compatibility mock");
        let addr = listener.local_addr().expect("hanging mock local addr");
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept hanging mock");
            let _ = read_mock_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .await
                .expect("write hanging mock headers");
            socket
                .write_all(format!("{:X}\r\n", first_event.len()).as_bytes())
                .await
                .expect("write hanging chunk length");
            socket
                .write_all(first_event)
                .await
                .expect("write hanging first event");
            socket
                .write_all(b"\r\n")
                .await
                .expect("finish hanging first chunk");
            socket.flush().await.expect("flush hanging first chunk");
            let _ = release_rx.await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}/v1/chat/completions"), release_tx)
    }

    async fn response_body(response: Response) -> Bytes {
        use futures::StreamExt;

        let mut stream = response.into_body().into_data_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.expect("client response chunk"));
        }
        Bytes::from(body)
    }

    async fn test_gateway() -> (
        crate::Gateway,
        tokio::sync::mpsc::Receiver<crate::logging::LogEntry>,
    ) {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-compat-handler-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        crate::Gateway::new(config).await.expect("gateway init")
    }

    fn call_context<'a>(
        gw: &'a crate::Gateway,
        provider: &'a Provider,
        ingress: ProtocolId,
        egress: ProtocolId,
        stream: bool,
    ) -> (CallCtx<'a>, RequestContext, AiRequest, RequestExtras) {
        let ingress_str: &'static str = Box::leak(ingress.to_string().into_boxed_str());
        let egress_str: &'static str = Box::leak(egress.to_string().into_boxed_str());
        let call_ctx = CallCtx {
            gw: gw.clone(),
            provider,
            model_id: "model-test",
            model_name: "Compatibility test",
            egress,
            ingress,
            ingress_str,
            egress_str,
            request_model: "virtual-model",
            actual_model: "upstream-model",
            api_key_id: None,
            api_key_name: None,
            is_stream: stream,
            enable_payload: Some(true),
            reasoning_effort: None,
            start: std::time::Instant::now(),
            req_ext: crate::proxy::context::ContextBag::new(),
        };
        let req_ctx = RequestContext::new(ingress, std::time::Duration::from_secs(30));
        let req_ir = AiRequest::new("virtual-model", Vec::new());
        let req_extras = RequestExtras {
            method: "POST".into(),
            path: "/v1/messages".into(),
            headers: None,
            body: None,
        };
        (call_ctx, req_ctx, req_ir, req_extras)
    }

    #[tokio::test]
    async fn production_handler_round_trips_anthropic_chat_buffered() {
        let (gw, mut log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let raw = br#"{ "unknown_before": 1, "model": "virtual-model", "max_tokens": 128, "messages": [{"role":"user","content":"hello"}], "unknown_after": 2 }"#;
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(false).with_model("upstream-model"),
                Bytes::copy_from_slice(raw),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let chat_response = br#"{"id":"chatcmpl_1","model":"upstream-model","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#;
        let (url, request_rx) = serve_once(
            "HTTP/1.1 200 OK",
            "application/json",
            chat_response.to_vec(),
        )
        .await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);
        let mut headers = ReqwestHeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            headers,
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::OK);
        assert_eq!(attempt.retry, RetryDisposition::DefaultStatusPolicy);
        let body = response_body(attempt.response).await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["content"][0]["text"], "hello");
        assert_eq!(value["usage"]["input_tokens"], 10);

        let request = request_rx.await.unwrap();
        let request_body = &request[request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4..];
        let upstream: Value = serde_json::from_slice(request_body).unwrap();
        assert_eq!(upstream["model"], "upstream-model");
        assert_eq!(upstream["messages"][0]["content"], "hello");
        assert!(request.starts_with(b"POST /v1/chat/completions HTTP/1.1\r\n"));
        // cc-switch's anthropic_to_openai rebuilds the body from a whitelist,
        // so unknown client fields must not leak to the upstream request.
        assert!(
            !request_body
                .windows(15)
                .any(|window| window == b"unknown_before")
        );
        assert!(
            !request_body
                .windows(14)
                .any(|window| window == b"unknown_after")
        );
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.client_status_code, 200);
        assert_eq!(entry.upstream_status_code, Some(200));
        assert_eq!(entry.input_tokens(), 10);
        assert_eq!(entry.output_tokens(), 2);
    }

    #[tokio::test]
    async fn production_handler_marks_responses_2xx_failure_retryable() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let raw = br#"{"model":"virtual-model","max_tokens":128,"messages":[{"role":"user","content":"hello"}]}"#;
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_responses(false, UpstreamFlavor::StandardResponses)
                    .with_model("upstream-model"),
                Bytes::copy_from_slice(raw),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let failed = br#"{"id":"resp_1","status":"failed","error":{"type":"server_error","message":"busy"},"output":[]}"#;
        let (url, _request_rx) =
            serve_once("HTTP/1.1 200 OK", "application/json", failed.to_vec()).await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_RESPONSES_V1,
            false,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(attempt.retry, RetryDisposition::ForceRetry);
        assert_eq!(attempt.health, HealthOutcome::Failure);
        let body = response_body(attempt.response).await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("server_error: busy")
        );
    }

    #[tokio::test]
    async fn production_handler_streams_complete_chat_sse_with_one_terminal() {
        let (gw, mut log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let raw = br#"{"model":"virtual-model","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#;
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(true).with_model("upstream-model"),
                Bytes::copy_from_slice(raw),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let sse = concat!(
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"upstream-model\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hel\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes()
        .to_vec();
        let (url, request_rx) = serve_once("HTTP/1.1 200 OK", "text/event-stream", sse).await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::OK);
        assert_eq!(attempt.health, HealthOutcome::Deferred);
        assert_eq!(
            attempt
                .response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap(),
            "text/event-stream"
        );
        let body = response_body(attempt.response).await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("event: message_start"));
        // cc-switch streams one text_delta per upstream chunk without merging.
        assert!(text.contains("\"text\":\"hel\""));
        assert!(text.contains("\"text\":\"lo\""));
        assert!(!text.contains("\"text\":\"hello\""));
        assert_eq!(text.matches("event: message_stop").count(), 1);
        let request = request_rx.await.unwrap();
        let request_body = &request[request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4..];
        let upstream: Value = serde_json::from_slice(request_body).unwrap();
        assert_eq!(upstream["stream"], true);
        assert_eq!(upstream["stream_options"]["include_usage"], true);

        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.client_status_code, 200);
        assert_eq!(entry.input_tokens(), 10);
        assert_eq!(entry.output_tokens(), 2);
        assert_eq!(req_ctx.get_outcome(), Some(&RequestOutcome::Success));
    }

    #[tokio::test]
    async fn production_handler_does_not_pretend_truncated_stream_completed() {
        let (gw, mut log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let raw = br#"{"model":"virtual-model","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#;
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(true).with_model("upstream-model"),
                Bytes::copy_from_slice(raw),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let sse = b"data: {\"id\":\"chatcmpl_trunc\",\"model\":\"upstream-model\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".to_vec();
        let (url, _request_rx) = serve_once("HTTP/1.1 200 OK", "text/event-stream", sse).await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::OK);
        let body = response_body(attempt.response).await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("partial"));
        assert!(!text.contains("event: message_stop"));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv()).await;
        assert!(matches!(
            req_ctx.get_outcome(),
            Some(RequestOutcome::PartialSuccess { .. }) | Some(RequestOutcome::Failed { .. })
        ));
    }

    #[tokio::test]
    async fn hanging_stream_deadline_releases_and_fails_health_permit() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(true).with_model("upstream-model"),
                Bytes::from_static(
                    br#"{"model":"virtual-model","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let (url, release_server) = serve_hanging_sse(
            b"data: {\"id\":\"chatcmpl_hanging\",\"model\":\"upstream-model\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        )
        .await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
        );
        req_ctx.deadline =
            crate::proxy::context::Deadline::from_now(std::time::Duration::from_millis(200));
        let host = crate::plugin::phase::HostContext::new(&gw);
        let health_key = "compat-hanging-deadline";
        gw.health_registry
            .try_acquire(health_key)
            .unwrap()
            .failure();
        gw.health_registry
            .try_acquire(health_key)
            .unwrap()
            .failure();

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire(health_key).unwrap(),
        )
        .await;
        assert_eq!(attempt.health, HealthOutcome::Deferred);
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            response_body(attempt.response),
        )
        .await
        .expect("response body should close at the request deadline");
        assert!(String::from_utf8_lossy(&body).contains("partial"));
        assert!(!gw.health_registry.is_healthy(health_key));
        assert!(gw.health_registry.try_acquire(health_key).is_none());
        let _ = release_server.send(());
    }

    #[tokio::test]
    async fn dropped_hanging_stream_releases_health_permit_neutrally() {
        let (gw, mut log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(true).with_model("upstream-model"),
                Bytes::from_static(
                    br#"{"model":"virtual-model","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let (url, release_server) = serve_hanging_sse(
            b"data: {\"id\":\"chatcmpl_cancel\",\"model\":\"upstream-model\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        )
        .await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);
        let health_key = "compat-hanging-cancelled";
        gw.health_registry
            .try_acquire(health_key)
            .unwrap()
            .failure();
        gw.health_registry
            .try_acquire(health_key)
            .unwrap()
            .failure();

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire(health_key).unwrap(),
        )
        .await;
        assert_eq!(attempt.health, HealthOutcome::Deferred);
        drop(attempt.response);
        tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("client cancellation should stop the stream task")
            .expect("stream task should emit a log entry");
        assert!(gw.health_registry.try_acquire(health_key).is_some());
        let _ = release_server.send(());
    }

    #[test]
    fn codex_anthropic_profile_uses_stripped_model() {
        let request = request("virtual", OPENAI_RESPONSES_V1);
        let selected = select_via_resolver(
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            &provider("anthropic", "default"),
            "https://api.anthropic.com",
            "claude-sonnet-4 [1M]",
            false,
            &HeaderMap::new(),
            br#"{"model":"virtual","input":"hello"}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();

        assert_eq!(selected.profile.model.as_deref(), Some("claude-sonnet-4"));
        assert!(selected.context_1m);
    }

    #[test]
    fn format_headers_keeps_only_allowlisted_diagnostic_values() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer super-secret".parse().unwrap());
        headers.insert("set-cookie", "session=cookie-secret".parse().unwrap());
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "2".parse().unwrap());
        headers.insert("cf-ray", "abc123-SJC".parse().unwrap());

        let formatted =
            crate::proxy::observability::headers_to_json(&headers).expect("header JSON");
        assert!(formatted.contains("authorization"), "{formatted}");
        assert!(formatted.contains("set-cookie"), "{formatted}");
        assert!(formatted.contains("retry-after\":\"30"), "{formatted}");
        assert!(
            formatted.contains("x-ratelimit-remaining\":\"2"),
            "{formatted}"
        );
        assert!(formatted.contains("cf-ray\":\"abc123-SJC"), "{formatted}");
        assert!(!formatted.contains("super-secret"), "{formatted}");
        assert!(!formatted.contains("cookie-secret"), "{formatted}");
    }

    fn anthropic_provider(base_url: &str) -> Provider {
        Provider {
            protocol: "anthropic-messages".into(),
            base_url: base_url.into(),
            ..provider("custom", "default")
        }
    }

    #[test]
    fn needs_transform_matrix_matches_egress_protocol() {
        let plain = anthropic_provider("https://api.anthropic.com");
        assert!(!crate::conversion::supports_raw_wire_compat(
            ANTHROPIC_MESSAGES_2023_06_01,
            ANTHROPIC_MESSAGES_2023_06_01,
            &plain,
            "https://api.anthropic.com",
            "claude-sonnet-5",
        ));
        for egress in [
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ] {
            assert!(
                crate::conversion::supports_raw_wire_compat(
                    ANTHROPIC_MESSAGES_2023_06_01,
                    egress,
                    &plain,
                    "https://api.example.com",
                    "claude-sonnet-5",
                ),
                "anthropic ingress with egress {egress} must convert"
            );
        }
        // DeepSeek-flavoured Anthropic upstream: even Anthropic→Anthropic
        // needs the ported normalizations.
        let deepseek = anthropic_provider("https://api.deepseek.com/anthropic");
        assert!(crate::conversion::supports_raw_wire_compat(
            ANTHROPIC_MESSAGES_2023_06_01,
            ANTHROPIC_MESSAGES_2023_06_01,
            &deepseek,
            "https://api.deepseek.com/anthropic",
            "deepseek-v4-pro",
        ));
        // Kimi deliberately stays generic (2026-08 feedback: injecting
        // thinking placeholders corrupts its chain of thought).
        let kimi = anthropic_provider("https://api.kimi.com/coding");
        assert!(!crate::conversion::supports_raw_wire_compat(
            ANTHROPIC_MESSAGES_2023_06_01,
            ANTHROPIC_MESSAGES_2023_06_01,
            &kimi,
            "https://api.kimi.com/coding",
            "kimi-for-coding",
        ));
    }

    #[test]
    fn compat_support_matrix_covers_positive_and_negative_boundaries() {
        let generic = provider("custom", "default");
        let xai = provider("xai", "default");
        let openai = provider("openai", "default");
        let codex = provider("custom", "codex");
        let sub2api = provider("custom", "sub2api");
        let deepseek = anthropic_provider("https://api.deepseek.com/anthropic");
        let kimi = anthropic_provider("https://api.kimi.com/coding");

        let cases = [
            (
                "anthropic_to_chat",
                ANTHROPIC_MESSAGES_2023_06_01,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                &generic,
                "https://example.com",
                "model",
                true,
            ),
            (
                "anthropic_to_responses",
                ANTHROPIC_MESSAGES_2023_06_01,
                OPENAI_RESPONSES_V1,
                &generic,
                "https://example.com",
                "model",
                true,
            ),
            (
                "anthropic_to_gemini",
                ANTHROPIC_MESSAGES_2023_06_01,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                &generic,
                "https://example.com",
                "model",
                true,
            ),
            (
                "responses_to_chat",
                OPENAI_RESPONSES_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                &generic,
                "https://example.com",
                "model",
                true,
            ),
            (
                "responses_to_anthropic",
                OPENAI_RESPONSES_V1,
                ANTHROPIC_MESSAGES_2023_06_01,
                &generic,
                "https://example.com",
                "model",
                true,
            ),
            (
                "responses_native_xai",
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &xai,
                "https://attacker.example",
                "grok-4.5",
                true,
            ),
            (
                "responses_native_third_party_candidate",
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &generic,
                "https://example.com",
                "model",
                true,
            ),
            (
                "anthropic_native_deepseek",
                ANTHROPIC_MESSAGES_2023_06_01,
                ANTHROPIC_MESSAGES_2023_06_01,
                &deepseek,
                "https://api.deepseek.com/anthropic",
                "deepseek-v4-pro",
                true,
            ),
            (
                "anthropic_native_generic",
                ANTHROPIC_MESSAGES_2023_06_01,
                ANTHROPIC_MESSAGES_2023_06_01,
                &generic,
                "https://api.anthropic.com",
                "claude-sonnet-5",
                false,
            ),
            (
                "anthropic_native_kimi",
                ANTHROPIC_MESSAGES_2023_06_01,
                ANTHROPIC_MESSAGES_2023_06_01,
                &kimi,
                "https://api.kimi.com/coding",
                "kimi-for-coding",
                false,
            ),
            (
                "responses_native_openai",
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &openai,
                "https://api.openai.com/v1",
                "gpt-5",
                false,
            ),
            (
                "responses_native_codex_channel",
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &codex,
                "https://chatgpt.com/backend-api/codex",
                "gpt-5",
                false,
            ),
            (
                "responses_native_sub2api_channel",
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &sub2api,
                "https://example.com",
                "gpt-5",
                false,
            ),
            (
                "chat_to_anthropic_uses_ir",
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                ANTHROPIC_MESSAGES_2023_06_01,
                &generic,
                "https://example.com",
                "model",
                false,
            ),
            (
                "gemini_to_chat_uses_ir",
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                &generic,
                "https://example.com",
                "model",
                false,
            ),
            (
                "embeddings_stay_native",
                crate::protocol::ids::OPENAI_COMPATIBLE_EMBEDDINGS_V1,
                crate::protocol::ids::OPENAI_COMPATIBLE_EMBEDDINGS_V1,
                &generic,
                "https://example.com",
                "embedding-model",
                false,
            ),
        ];

        for (name, ingress, egress, provider, base_url, model, expected) in cases {
            assert_eq!(
                crate::conversion::supports_raw_wire_compat(
                    ingress, egress, provider, base_url, model
                ),
                expected,
                "compat support mismatch for {name}"
            );
        }
    }

    #[test]
    fn anthropic_passthrough_profile_carries_normalization_hints() {
        let request = request("deepseek-v4-pro", ANTHROPIC_MESSAGES_2023_06_01);
        let selected = select_via_resolver(
            ANTHROPIC_MESSAGES_2023_06_01,
            ANTHROPIC_MESSAGES_2023_06_01,
            &anthropic_provider("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/anthropic",
            "deepseek-v4-pro",
            false,
            &HeaderMap::new(),
            br#"{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"hello"}]}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.profile.direction,
            nyro_ccswitch_compat::Direction::AnthropicToAnthropic
        );
        assert!(
            selected
                .profile
                .anthropic_normalization
                .as_ref()
                .is_some_and(|hints| hints.base_url == "https://api.deepseek.com/anthropic")
        );
    }

    #[test]
    fn codex_wire_api_is_decided_by_the_negotiated_egress() {
        let request = request("gpt-5", OPENAI_RESPONSES_V1);
        let raw = br#"{"model":"gpt-5","input":"hello"}"#;

        let chat = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &provider("custom", "default"),
            "https://example.com/v1",
            "gpt-5",
            false,
            &HeaderMap::new(),
            raw,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            chat.profile.direction,
            nyro_ccswitch_compat::Direction::CodexResponsesToChat
        );

        let anthropic = select_via_resolver(
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            &anthropic_provider("https://claude-gateway.example.com"),
            "https://claude-gateway.example.com",
            "claude-sonnet-5",
            false,
            &HeaderMap::new(),
            raw,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            anthropic.profile.direction,
            nyro_ccswitch_compat::Direction::CodexResponsesToAnthropic
        );

        // Anthropic and chat are mutually exclusive: chat egress never selects
        // the anthropic direction and vice versa.
        assert!(!matches!(
            chat.profile.direction,
            nyro_ccswitch_compat::Direction::CodexResponsesToAnthropic
        ));
        assert!(!matches!(
            anthropic.profile.direction,
            nyro_ccswitch_compat::Direction::CodexResponsesToChat
        ));

        // Plain responses passthrough without xai is not a compat conversion.
        assert!(
            select_via_resolver(
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &provider("custom", "default"),
                "https://api.openai.com/v1",
                "gpt-5",
                false,
                &HeaderMap::new(),
                raw,
                &request,
                &request,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn codex_chat_prompt_cache_routing_auto_enables_known_upstreams_only() {
        let request = request("gpt-5", OPENAI_RESPONSES_V1);
        let raw = br#"{"model":"gpt-5","input":"hello"}"#;
        for (base_url, expected) in [
            ("https://api.kimi.com/coding/v1", true),
            ("https://api.openai.com/v1", true),
            ("https://strict.example.com/v1", false),
        ] {
            let selected = select_via_resolver(
                OPENAI_RESPONSES_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                &provider("custom", "default"),
                base_url,
                "gpt-5",
                false,
                &HeaderMap::new(),
                raw,
                &request,
                &request,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                selected.profile.prompt_cache_key_supported, expected,
                "base_url {base_url}"
            );
        }
    }

    #[test]
    fn xai_oauth_invariants_ignore_base_url() {
        // nyro's vendor id is the authoritative signal: a stray base_url cannot
        // downgrade xAI away from the strict native-Responses conversion.
        let request = request("grok-4.5", OPENAI_RESPONSES_V1);
        let mut attacker = provider("xai", "default");
        attacker.base_url = "https://attacker.example/anthropic".into();
        let selected = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &attacker,
            "https://attacker.example/anthropic",
            "grok-4.5",
            false,
            &HeaderMap::new(),
            br#"{"model":"grok-4.5","input":"hello"}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.profile.direction,
            nyro_ccswitch_compat::Direction::XaiResponsesNative
        );
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::XaiStrictResponses
        );

        // Third-party responses natives are compat candidates now, but a
        // plain body (no Responses-Lite artifacts) selects None and keeps
        // the byte-level native passthrough.
        let plain = provider("custom", "default");
        assert!(crate::conversion::supports_raw_wire_compat(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &plain,
            "https://api.x.ai/v1",
            "grok-4.5",
        ));
        assert!(
            select_via_resolver(
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &plain,
                "https://api.example.com/v1",
                "grok-4.5",
                false,
                &HeaderMap::new(),
                br#"{"model":"grok-4.5","input":"hello"}"#,
                &request,
                &request,
            )
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn xai_oauth_invariants_ignore_editable_format_and_base_url() {
        // Nyro has no editable api_format override: the negotiated egress ID and
        // vendor identity are authoritative even when base_url is attacker-controlled.
        let raw = br#"{"model":"grok-4.5","max_tokens":2048,"thinking":{"type":"enabled","budget_tokens":20000},"messages":[{"role":"user","content":"hello"}]}"#;
        let request = request("grok-4.5", ANTHROPIC_MESSAGES_2023_06_01);
        let mut attacker = provider("xai", "default");
        attacker.base_url = "https://attacker.example/anthropic".into();
        let selected = select_via_resolver(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_RESPONSES_V1,
            &attacker,
            "https://attacker.example/anthropic",
            "grok-4.5",
            false,
            &HeaderMap::new(),
            raw,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.profile.direction, Direction::AnthropicToResponses);
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::XaiStrictResponses
        );
        assert_eq!(selected.profile.model.as_deref(), Some("grok-4.5"));

        let prepared = CompatEngine::default()
            .prepare_request(
                selected.profile,
                Bytes::copy_from_slice(raw),
                selected.identity,
            )
            .await
            .unwrap();
        let transformed: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(transformed["reasoning"]["effort"], "high");
        assert_eq!(
            transformed["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert!(transformed.get("store").is_none());
    }

    #[tokio::test]
    async fn namespace_flatten_gate_only_fires_for_xai_oauth() {
        let body = br#"{"model":"grok-4.5","tools":[{"type":"function","name":"plain_tool","parameters":{}},{"type":"namespace","name":"mcp__files__","tools":[{"type":"function","name":"read","parameters":{}}]}],"input":[{"role":"user","content":"hello"}]}"#;
        let request = request("grok-4.5", OPENAI_RESPONSES_V1);
        let selected = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &provider("xai", "default"),
            "https://attacker.example/anthropic",
            "grok-4.5",
            false,
            &HeaderMap::new(),
            body,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::XaiStrictResponses
        );
        let prepared = CompatEngine::default()
            .prepare_request(
                selected.profile,
                Bytes::copy_from_slice(body),
                selected.identity,
            )
            .await
            .unwrap();
        let flattened: Value = serde_json::from_slice(&prepared.body).unwrap();
        let tools = flattened["tools"].as_array().unwrap();
        assert!(tools.iter().all(|tool| tool["type"] == "function"));
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "mcp__files____read")
        );

        // A base URL that merely resembles xAI cannot select the xAI profile.
        let plain = provider("custom", "default");
        assert!(
            select_via_resolver(
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &plain,
                "https://api.x.ai/v1",
                "grok-4.5",
                false,
                &HeaderMap::new(),
                br#"{"model":"grok-4.5","input":"hello"}"#,
                &request,
                &request,
            )
            .unwrap()
            .is_none()
        );

        // Responses-Lite artifacts may still select the separate third-party
        // normalization profile, but never masquerade as xAI OAuth.
        let third_party = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &plain,
            "https://api.x.ai/v1",
            "grok-4.5",
            false,
            &HeaderMap::new(),
            body,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            third_party.profile.upstream_flavor,
            UpstreamFlavor::ThirdPartyStrictResponses
        );
    }

    #[test]
    fn xai_oauth_pins_native_responses_catalog_profile() {
        let request = request("grok-4.5", OPENAI_RESPONSES_V1);
        let mut xai = provider("xai", "default");
        xai.protocol = "anthropic-messages".into();
        xai.base_url = "https://attacker.example/anthropic".into();
        let selected = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &xai,
            "https://attacker.example/anthropic",
            "grok-4.5",
            false,
            &HeaderMap::new(),
            br#"{"model":"grok-4.5","input":"hello"}"#,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.profile.direction, Direction::XaiResponsesNative);
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::XaiStrictResponses
        );
        assert_eq!(
            selected.profile.upstream_protocol,
            nyro_ccswitch_compat::WireProtocol::OpenAiResponses
        );
    }

    #[test]
    fn selects_third_party_responses_for_carrier_bodies() {
        let request = request("glm-5.3", OPENAI_RESPONSES_V1);
        let body = br#"{"model":"glm-5.3","input":[{"type":"additional_tools","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait","parameters":{"type":"object"}}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let selected = select_via_resolver(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            &provider("glm", "default"),
            "https://open.bigmodel.cn/api/paas/v4",
            "glm-5.3",
            true,
            &HeaderMap::new(),
            body,
            &request,
            &request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.profile.direction,
            nyro_ccswitch_compat::Direction::XaiResponsesNative
        );
        assert_eq!(
            selected.profile.upstream_flavor,
            UpstreamFlavor::ThirdPartyStrictResponses
        );
    }

    #[test]
    fn openai_native_channels_stay_out_of_third_party_compat() {
        for (vendor, channel) in [
            ("openai", "default"),
            ("custom", "codex"),
            ("custom", "sub2api"),
        ] {
            let p = provider(vendor, channel);
            assert!(
                !crate::conversion::supports_raw_wire_compat(
                    OPENAI_RESPONSES_V1,
                    OPENAI_RESPONSES_V1,
                    &p,
                    "https://example.com",
                    "gpt-5.6",
                ),
                "{vendor}/{channel} must stay native"
            );
        }
    }

    fn streaming_decision_profile(
        direction: nyro_ccswitch_compat::Direction,
        client_stream: bool,
    ) -> nyro_ccswitch_compat::ConversionSession {
        use nyro_ccswitch_compat::{ClientSemantics, WireProtocol};
        let profile = nyro_ccswitch_compat::ConversionProfile {
            direction,
            client_protocol: WireProtocol::OpenAiResponses,
            upstream_protocol: WireProtocol::OpenAiResponses,
            client_semantics: ClientSemantics::CodexResponses,
            upstream_flavor: UpstreamFlavor::StandardResponses,
            client_stream,
            ..nyro_ccswitch_compat::ConversionProfile::xai_responses_native(false)
        };
        let engine = nyro_ccswitch_compat::CompatEngine::default();
        futures::executor::block_on(engine.prepare_request(
            profile,
            Bytes::from_static(br#"{"model":"gpt-5","input":"hello"}"#),
            nyro_ccswitch_compat::SessionIdentity::generated("test"),
        ))
        .unwrap()
        .session
    }

    #[test]
    fn upstream_sse_response_always_uses_streaming_path() {
        let session = streaming_decision_profile(
            nyro_ccswitch_compat::Direction::CodexResponsesToChat,
            false,
        );
        let mut metadata = nyro_ccswitch_compat::ResponseMetadata::new(200);
        metadata.content_type = Some("text/event-stream".to_string());
        assert!(should_stream_response(&session, &metadata));
    }

    #[test]
    fn non_streaming_response_stays_non_streaming_for_regular_openai_responses() {
        let session = streaming_decision_profile(
            nyro_ccswitch_compat::Direction::CodexResponsesToChat,
            false,
        );
        let mut metadata = nyro_ccswitch_compat::ResponseMetadata::new(200);
        metadata.content_type = Some("application/json".to_string());
        assert!(!should_stream_response(&session, &metadata));
    }

    async fn serve_broken_chunked_sse() -> String {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind broken mock");
        let addr = listener.local_addr().expect("broken mock addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept broken mock");
            let _ = read_mock_request(&mut socket).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\ngarbage not a chunk",
                )
                .await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/v1/chat/completions")
    }

    #[tokio::test]
    async fn streaming_first_chunk_error_is_retryable_before_success_record() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(true).with_model("upstream-model"),
                Bytes::from_static(
                    br#"{"model":"virtual-model","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let url = serve_broken_chunked_sse().await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.retry, RetryDisposition::ForceRetry);
        assert_ne!(attempt.health, HealthOutcome::Deferred);
        assert_ne!(attempt.response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn streaming_success_primes_first_chunk_and_replays_it() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("custom", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(true).with_model("upstream-model"),
                Bytes::from_static(
                    br#"{"model":"virtual-model","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        // The first chunk alone is not a complete SSE event; priming buffers
        // it and the replayed stream still yields the full event to the client.
        let sse = concat!(
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"upstream-model\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"he\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes()
        .to_vec();
        let (url, _request_rx) = serve_once("HTTP/1.1 200 OK", "text/event-stream", sse).await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::OK);
        let body = response_body(attempt.response).await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("\"text\":\"he\""));
        assert!(text.contains("\"text\":\"llo\""));
        assert_eq!(text.matches("event: message_stop").count(), 1);
    }

    #[tokio::test]
    async fn codex_proxy_413_points_to_upstream_not_local_proxy() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("HCAI", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::codex_responses_to_anthropic(false),
                Bytes::from_static(
                    br#"{"model":"gpt-5.5","input":[{"role":"user","content":"hello"}],"max_output_tokens":128}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let nginx_html = b"<html>\r\n<head><title>413 Request Entity Too Large</title></head>\r\n<body>\r\n<center><h1>413 Request Entity Too Large</h1></center>\r\n<hr><center>nginx/1.29.6</center>\r\n</body>\r\n</html>".to_vec();
        let (url, _request_rx) =
            serve_once("HTTP/1.1 413 Payload Too Large", "text/html", nginx_html).await;
        let (mut call_ctx, mut req_ctx, mut req_ir, mut req_extras) = call_context(
            &gw,
            &provider,
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            false,
        );
        call_ctx.actual_model = "gpt-5.5";
        req_extras.path = "/responses".into();
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(attempt.health, HealthOutcome::Neutral);
        let body = response_body(attempt.response).await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(!message.contains("local proxy failed"));
        assert!(message.contains("413"));
        assert!(message.to_lowercase().contains("upstream"));
        assert!(message.contains("/compact"));
        assert!(!message.contains("<html>"));
        assert!(!message.contains("nginx/1.29.6"));
        assert_eq!(value["error"]["upstream_status"], 413);
        assert_eq!(value["error"]["provider"], "HCAI");
        assert_eq!(value["error"]["model"], "gpt-5.5");
        assert_eq!(value["error"]["endpoint"], "/responses");
    }

    #[tokio::test]
    async fn codex_proxy_forward_error_includes_context_and_cause() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("DeepSeek", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::codex_responses_to_chat(false),
                Bytes::from_static(
                    br#"{"model":"deepseek-chat","input":[{"role":"user","content":"hello"}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        // Connect to a closed port: the transport itself fails.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/v1/chat/completions");
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            OPENAI_RESPONSES_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(attempt.health, HealthOutcome::Failure);
        let body = response_body(attempt.response).await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("local proxy failed"), "{message}");
        assert!(message.contains("DeepSeek"));
        assert!(message.contains("upstream-model"));
        assert!(message.contains("cause"));
        assert_eq!(value["error"]["provider"], "DeepSeek");

        // Lock the exact source-level envelope assertions with Nyro's documented
        // product-name/code substitution; the transport failure above remains
        // the production-path integration coverage.
        let direct = nyro_ccswitch_compat::codex_client_error_json(
            "DeepSeek",
            "deepseek-chat",
            "/responses",
            None,
            None,
            Some("连接失败: dns lookup failed"),
        );
        let direct: Value = serde_json::from_slice(&direct).unwrap();
        let direct_message = direct["error"]["message"].as_str().unwrap();
        assert!(direct_message.contains("Nyro local proxy failed"));
        assert!(direct_message.contains("DeepSeek"));
        assert!(direct_message.contains("deepseek-chat"));
        assert!(direct_message.contains("/responses"));
        assert!(direct_message.contains("dns lookup failed"));
        assert_eq!(direct["error"]["code"], "nyro_forward_failed");
        assert_eq!(direct["error"]["provider"], "DeepSeek");
        assert_eq!(direct["error"]["model"], "deepseek-chat");
        assert_eq!(direct["error"]["endpoint"], "/responses");
    }

    #[tokio::test]
    async fn codex_proxy_upstream_error_normalizes_nonstandard_body() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = provider("MiniMax", "default");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::codex_responses_to_chat(false),
                Bytes::from_static(
                    br#"{"model":"abab6.5s","input":[{"role":"user","content":"hello"}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let upstream =
            br#"{"base_resp":{"status_code":2013,"status_msg":"upstream gateway failed"}}"#
                .to_vec();
        let (url, _request_rx) =
            serve_once("HTTP/1.1 502 Bad Gateway", "application/json", upstream).await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            OPENAI_RESPONSES_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(attempt.health, HealthOutcome::Failure);
        let body = response_body(attempt.response).await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("upstream_status: HTTP 502"), "{message}");
        assert!(message.contains("upstream gateway failed"));
        assert_eq!(value["error"]["code"], 2013);
        assert_eq!(value["error"]["upstream_status"], 502);
    }

    #[tokio::test]
    async fn anthropic_passthrough_deepseek_round_trip_normalizes_history() {
        let (gw, _log_rx) = test_gateway().await;
        let provider = anthropic_provider("https://api.deepseek.com/anthropic");
        let prepared = gw
            .compat_engine
            .prepare_request(
                ConversionProfile::anthropic_passthrough_normalized(false)
                    .with_anthropic_normalization(
                        "deepseek-v4-pro",
                        "https://api.deepseek.com/anthropic",
                        "deepseek",
                    ),
                Bytes::from_static(
                    br#"{"model":"deepseek-v4-pro","max_tokens":128,"messages":[{"role":"assistant","content":[{"type":"text","text":"inspecting"},{"type":"tool_use","id":"call_1","name":"read_file","input":{"path":"README.md"}}]}]}"#,
                ),
                nyro_ccswitch_compat::SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let upstream = br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"model":"deepseek-v4-pro","usage":{"input_tokens":7,"output_tokens":3}}"#.to_vec();
        let (url, request_rx) =
            serve_once("HTTP/1.1 200 OK", "application/json", upstream.clone()).await;
        let (call_ctx, mut req_ctx, mut req_ir, req_extras) = call_context(
            &gw,
            &provider,
            ANTHROPIC_MESSAGES_2023_06_01,
            ANTHROPIC_MESSAGES_2023_06_01,
            false,
        );
        let host = crate::plugin::phase::HostContext::new(&gw);

        let attempt = handle_compat(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            prepared,
            &call_ctx,
            &req_extras,
            &mut req_ctx,
            &mut req_ir,
            &host,
            gw.health_registry.try_acquire("compat-test").unwrap(),
        )
        .await;
        assert_eq!(attempt.response.status(), StatusCode::OK);
        let body = response_body(attempt.response).await;
        assert_eq!(body.as_ref(), upstream.as_slice());

        let request = request_rx.await.unwrap();
        let request_body = &request[request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4..];
        let value: Value = serde_json::from_slice(request_body).unwrap();
        let content = value["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "tool call");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
    }
}
