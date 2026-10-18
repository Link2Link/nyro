//! Server-streaming Connect transport with existing gateway cancellation,
//! deadline, usage observation and native-IR downstream conversion.
use futures::StreamExt;
use prost::Message;
use reqwest::header::{HeaderMap, HeaderValue};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

use super::{BASE_URL, CHAT_PATH, STATUS_PATH, catalog, request, stream, unavailable, usage};
use crate::protocol::{
    ids::DEVIN_CHAT_V1,
    ir::{AiRequest, AiStreamDelta},
};
use crate::provider::{
    native::{AttemptControl, IrUpstream, SessionGuard},
    vendor::ProviderCtx,
};
use crate::proxy::{
    client::{ProxyClient, UpstreamReadError, UpstreamTimeoutPhase},
    context::{CancellationToken, Deadline},
};
use crate::{
    Gateway,
    auth::types::StoredCredential,
    db::models::Provider,
    error::{GatewayError, TimeoutPhase},
};

fn headers(token: &str, streaming: bool) -> Result<HeaderMap, GatewayError> {
    if token.trim().is_empty() {
        return Err(unavailable("Devin OAuth credential is missing"));
    }
    let mut headers = HeaderMap::new();
    // This is the upstream's literal scheme, NOT RFC7617 base64 encoding.
    let mut auth = HeaderValue::from_str(&format!("Basic {token}-{token}"))
        .map_err(|_| unavailable("invalid Devin credential header"))?;
    auth.set_sensitive(true);
    headers.insert("authorization", auth);
    headers.insert(
        "content-type",
        HeaderValue::from_static(if streaming {
            "application/connect+proto"
        } else {
            "application/proto"
        }),
    );
    headers.insert("connect-protocol-version", HeaderValue::from_static("1"));
    headers.insert("accept", HeaderValue::from_static("*/*"));
    if streaming {
        headers.insert("connect-accept-encoding", HeaderValue::from_static("gzip"));
        let trace = format!(
            "{}-{}-1",
            uuid::Uuid::new_v4().simple(),
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        );
        headers.insert("sentry-trace", HeaderValue::from_str(&trace).unwrap());
    }
    Ok(headers)
}

fn network(error: anyhow::Error) -> GatewayError {
    if let Some(UpstreamReadError::Timeout { phase }) = error.downcast_ref::<UpstreamReadError>() {
        return GatewayError::UpstreamTimeout {
            phase: match phase {
                UpstreamTimeoutPhase::ResponseHeaders => TimeoutPhase::ResponseHeaders,
                UpstreamTimeoutPhase::FirstByte => TimeoutPhase::FirstByte,
                UpstreamTimeoutPhase::StreamIdle => TimeoutPhase::Streaming,
                UpstreamTimeoutPhase::Total => TimeoutPhase::Total,
            },
        };
    }
    if error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_timeout())
        || matches!(error.downcast_ref::<UpstreamReadError>(), Some(UpstreamReadError::Transport(e)) if e.is_timeout())
    {
        return GatewayError::UpstreamTimeout {
            phase: TimeoutPhase::Connect,
        };
    }
    // Never expose upstream bodies (which may echo the embedded token).
    GatewayError::UpstreamStatus {
        provider: "devin".into(),
        status: 502,
        body: Some("Devin transport failed".into()),
    }
}

pub(super) async fn start(
    request: AiRequest,
    ctx: &ProviderCtx<'_>,
    transport: ProxyClient,
    control: AttemptControl,
) -> Result<IrUpstream, GatewayError> {
    super::validate(ctx.provider)?;
    if ctx.egress_base_url.trim_end_matches('/') != BASE_URL {
        return Err(unavailable("Devin requires its official HTTPS origin"));
    }
    execute(
        request,
        ctx.actual_model,
        ctx.api_key,
        format!("{BASE_URL}{CHAT_PATH}"),
        transport,
        control,
    )
    .await
}

async fn execute(
    request: AiRequest,
    requested_model: &str,
    token: &str,
    url: String,
    transport: ProxyClient,
    control: AttemptControl,
) -> Result<IrUpstream, GatewayError> {
    let actual_model = catalog::resolve(requested_model, &request.reasoning);
    let identity = headers(token, true)?;
    let body = request::encode(&request, token, &actual_model)?;
    control.performance.configure_devin_stream();
    control.performance.note_native_request(&identity, &body, &serde_json::json!({
        "message_count": request.messages.len(), "tool_count": request.tools.as_ref().map_or(0, Vec::len),
        "has_system": request.system.is_some(), "stream": request.stream.enabled,
    }));
    control.mark_submitted();
    let response = tokio::select! {
        biased;
        _ = control.cancellation.cancelled() => return Err(GatewayError::ClientCancelled),
        result = transport.call_stream_raw(&url, identity, body.into()) => result.map_err(network)?,
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        // Bounded classification only; opaque response bytes are never logged.
        let mut bytes = Vec::new();
        let mut body = response.bytes_stream();
        while bytes.len() < 16 * 1024 {
            let chunk = tokio::select! {
                biased;
                _ = control.cancellation.cancelled() => return Err(GatewayError::ClientCancelled),
                _ = tokio::time::sleep(control.deadline.remaining()) => break,
                chunk = body.next() => chunk,
            };
            match chunk {
                Some(Ok(chunk)) => {
                    bytes.extend_from_slice(&chunk[..chunk.len().min(16 * 1024 - bytes.len())])
                }
                _ => break,
            }
        }
        let message = String::from_utf8_lossy(&bytes);
        let error = super::errors::failure(
            if status >= 400 { status } else { 502 },
            &message,
            super::errors::http_refusal(status, &bytes),
            retry_after,
        );
        if error
            .account_failure()
            .is_some_and(|f| f.rejected_before_execution)
        {
            control.rejected.store(true, Ordering::Release);
        }
        return Err(error);
    }
    let response_headers = response.headers().clone();
    let (sender, receiver) = tokio::sync::mpsc::channel(32);
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let model = actual_model.clone();
    let worker = tokio::spawn(async move {
        let mut session = stream::Session::new(&model);
        let mut bytes = Box::pin(response.bytes_stream());
        loop {
            let chunk = tokio::select! {
                biased;
                _ = child.cancelled() => break,
                _ = control.cancellation.cancelled() => break,
                _ = sender.closed() => break,
                chunk = bytes.next() => chunk,
            };
            let eof = chunk.is_none();
            let batch = match chunk {
                Some(Ok(chunk)) => session.push(&chunk),
                Some(Err(error)) => Err(network(error.into())),
                None => session.finish(),
            };
            let failed = batch.is_err();
            match &batch {
                Ok(events) => {
                    for event in events {
                        match event {
                            AiStreamDelta::Usage(_) => control
                                .performance
                                .note_native_usage_provenance(session.usage_provenance()),
                            AiStreamDelta::Done { stop_reason } => {
                                if matches!(stop_reason.as_str(), "length" | "content_filter") {
                                    control.performance.record_message(
                                        "output_limited",
                                        "output_limit",
                                        "upstream_response",
                                        "Devin output was limited by the upstream",
                                    );
                                } else {
                                    control.performance.observe_native_terminal(
                                        if stop_reason == "tool_calls" {
                                            crate::performance::NativeTerminalEvidence::ToolHandoff
                                        } else {
                                            crate::performance::NativeTerminalEvidence::Completed
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(error) => control.performance.record_failure(
                    if matches!(error, GatewayError::UpstreamTimeout { .. }) {
                        "timed_out"
                    } else {
                        "failed"
                    },
                    if matches!(error, GatewayError::UpstreamTimeout { .. }) {
                        "timeout"
                    } else {
                        "devin_protocol_error"
                    },
                    "upstream_read",
                    error,
                ),
            }
            if !matches!(&batch, Ok(events) if events.is_empty()) {
                let sent = tokio::select! {
                    _ = child.cancelled() => false,
                    _ = control.cancellation.cancelled() => false,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(control.deadline.at())) => false,
                    sent = sender.send(batch) => sent.is_ok(),
                };
                if !sent {
                    break;
                }
            }
            // Drain the HTTP body after Connect EOS: the observer requires
            // original EOF, and late bytes/errors must not become success.
            if failed || eof {
                break;
            }
        }
    });
    Ok(IrUpstream {
        status,
        headers: response_headers,
        url,
        actual_model: actual_model.clone(),
        model_metadata: serde_json::json!({"requested_model": requested_model, "upstream_model": actual_model}),
        events: Box::pin(ReceiverStream::new(receiver)),
        guard: SessionGuard::new(cancel, vec![worker.abort_handle()]),
    })
}

/// Largest accepted `GetUserStatus` body. The real response carries the whole
/// model catalogue (~150 KiB), so this leaves generous headroom while still
/// bounding a hostile or broken upstream.
const MAX_STATUS_BYTES: usize = 1024 * 1024;

/// One authenticated, non-generating `GetUserStatus` round trip against `url`,
/// decoded into the shared status shape. Does not spend a chat request.
async fn request_status(
    gw: &Gateway,
    provider: &Provider,
    token: &str,
    url: &str,
) -> anyhow::Result<usage::StatusResponse> {
    super::validate(provider)?;
    let client = ProxyClient::new(gw.devin_http_client(provider.use_proxy, false).await?)
        .with_timeouts(gw.config.upstream_timeouts)
        .with_deadline(Deadline::from_now(Duration::from_secs(15)));
    request_status_with(&client, token, url).await
}

async fn request_status_with(
    client: &ProxyClient,
    token: &str,
    url: &str,
) -> anyhow::Result<usage::StatusResponse> {
    let response = client
        .call_stream_raw(
            url,
            headers(token, false)?,
            request::user_status_request(token).into(),
        )
        .await
        .map_err(network)?;
    let status = response.status();
    // Wording matters: the admin usage route maps these phrases to a
    // provider-credential failure instead of an admin-session expiry.
    if matches!(status.as_u16(), 401 | 403) {
        anyhow::bail!(
            "Devin authentication failed (HTTP {}); re-authorize this provider",
            status.as_u16()
        );
    }
    anyhow::ensure!(
        status.is_success(),
        "Devin status check returned HTTP {}",
        status.as_u16()
    );
    let mut bytes = Vec::new();
    let mut stream = Box::pin(response.bytes_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| network(error.into()))?;
        anyhow::ensure!(
            bytes.len() + chunk.len() <= MAX_STATUS_BYTES,
            "Devin status response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    usage::StatusResponse::decode(bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("Invalid Devin status response"))
}

/// Authenticated, non-generating health check. Does not spend a chat request.
pub(crate) async fn check_connection(
    gw: &Gateway,
    provider: &Provider,
    token: &str,
) -> anyhow::Result<()> {
    let status = request_status(gw, provider, token, &format!("{BASE_URL}{STATUS_PATH}")).await?;
    anyhow::ensure!(
        status.user.is_some(),
        "Devin status response contains no user status"
    );
    Ok(())
}

/// Plan, daily/weekly quota windows and extra-usage balance of the account,
/// read from the same `GetUserStatus` call as the health check.
pub(crate) async fn fetch_usage(
    gw: &Gateway,
    provider: &Provider,
    token: &str,
) -> anyhow::Result<usage::DevinUsage> {
    usage::snapshot(request_status(gw, provider, token, &format!("{BASE_URL}{STATUS_PATH}")).await?)
}

/// Explicit per-model probes share the same transport and completion checks.
pub(crate) async fn probe(
    gw: &Gateway,
    provider: &Provider,
    credential: &StoredCredential,
    model: &str,
) -> anyhow::Result<String> {
    use crate::protocol::ir::request::{Message as ChatMessage, MessageContent, Role};
    let mut request = AiRequest::new(
        model,
        vec![ChatMessage {
            role: Role::User,
            content: MessageContent::Text("Reply with OK.".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.generation.max_tokens = Some(1024);
    let deadline = Deadline::from_now(Duration::from_secs(30));
    let cancellation = CancellationToken::new();
    let performance =
        crate::performance::Attempt::new(gw.log_tx.clone(), cancellation.clone(), deadline.clone());
    let client = ProxyClient::new(gw.devin_http_client(provider.use_proxy, true).await?)
        .with_timeouts(gw.config.upstream_timeouts)
        .with_deadline(deadline.clone())
        .with_performance(performance.clone());
    let ctx = ProviderCtx {
        provider,
        protocol: DEVIN_CHAT_V1,
        egress_base_url: BASE_URL,
        api_key: credential.access_token.as_deref().unwrap_or_default(),
        auth_scheme: "oauth",
        actual_model: model,
        force_max_reasoning: false,
        credential: Some(credential),
        gw,
        disable_default_auth: true,
    };
    let mut upstream = start(
        request,
        &ctx,
        client,
        AttemptControl {
            deadline,
            cancellation,
            performance,
            run_submitted: Arc::new(AtomicBool::new(false)),
            local_failure: Arc::new(AtomicBool::new(false)),
            rejected: Arc::new(AtomicBool::new(false)),
        },
    )
    .await?;
    let mut completed = false;
    let mut reply = String::new();
    while let Some(batch) = upstream.events.next().await {
        for event in batch? {
            match event {
                AiStreamDelta::TextDelta(text) => reply.push_str(&text),
                AiStreamDelta::Done { stop_reason } => {
                    completed = stop_reason == "stop" || stop_reason == "length"
                }
                _ => {}
            }
        }
    }
    anyhow::ensure!(completed, "Devin probe did not reach verified completion");
    Ok(if reply.is_empty() {
        "[completed]".into()
    } else {
        reply
    })
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod transport_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn devin_headers_preserve_literal_auth_without_exposing_it() {
        let h = headers("devin-session-token$test", true).unwrap();
        assert_eq!(
            h["authorization"],
            "Basic devin-session-token$test-devin-session-token$test"
        );
        assert!(h["authorization"].is_sensitive());
        assert_eq!(h["content-type"], "application/connect+proto");
        assert!(!h.contains_key("user-agent"));
        assert_eq!(
            headers("test", false).unwrap()["content-type"],
            "application/proto"
        );
        assert!(headers("", true).is_err());
        assert!(headers("bad\nvalue", true).is_err());
    }
    #[test]
    fn devin_network_errors_are_typed_and_sanitized() {
        assert!(matches!(
            network(
                UpstreamReadError::Timeout {
                    phase: UpstreamTimeoutPhase::FirstByte
                }
                .into()
            ),
            GatewayError::UpstreamTimeout {
                phase: TimeoutPhase::FirstByte
            }
        ));
        assert!(
            !network(anyhow::anyhow!("secret-token"))
                .to_string()
                .contains("secret-token")
        );
    }
}
