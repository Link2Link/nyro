//! Native Cursor I/O orchestration around the pure wire session state machine.
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

use super::{
    BASE_URL, CLIENT_VERSION, RUN_PATH, catalog, gateway_error, gateway_response_error, history,
    session,
};
use crate::Gateway;
use crate::auth::types::StoredCredential;
use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ir::{AiRequest, AiStreamDelta};
use crate::provider::native::{AttemptControl, IrUpstream, SessionGuard};
use crate::provider::vendor::ProviderCtx;
use crate::proxy::client::{ProxyClient, duplex::UpstreamFrame};
use crate::proxy::context::{CancellationToken, Deadline};

fn unavailable(message: impl Into<String>) -> GatewayError {
    GatewayError::ProviderUnavailable {
        provider: "cursor".into(),
        reason: message.into(),
    }
}
fn network(error: anyhow::Error) -> GatewayError {
    use crate::error::TimeoutPhase;
    use crate::proxy::client::{UpstreamTimeoutPhase, duplex::DuplexError};
    if let Some(kind) = error.downcast_ref::<DuplexError>() {
        return match kind {
            DuplexError::Cancelled => GatewayError::ClientCancelled,
            DuplexError::Timeout(phase) => GatewayError::UpstreamTimeout {
                phase: match phase {
                    UpstreamTimeoutPhase::ResponseHeaders => TimeoutPhase::ResponseHeaders,
                    UpstreamTimeoutPhase::FirstByte => TimeoutPhase::FirstByte,
                    UpstreamTimeoutPhase::StreamIdle => TimeoutPhase::Streaming,
                    UpstreamTimeoutPhase::Total => TimeoutPhase::Total,
                },
            },
        };
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        if error.is_timeout() {
            return GatewayError::UpstreamTimeout {
                phase: TimeoutPhase::Connect,
            };
        }
    }
    GatewayError::UpstreamStatus {
        provider: "cursor".into(),
        status: 502,
        body: Some(format!("Cursor transport failed: {error}")),
    }
}
fn record_failure(performance: &crate::performance::Attempt, error: &GatewayError) {
    let (outcome, kind) = match error {
        GatewayError::ClientCancelled => ("cancelled", "client_cancelled"),
        GatewayError::UpstreamTimeout { .. } => ("timed_out", "timeout"),
        _ => ("failed", "cursor_protocol_error"),
    };
    performance.record_failure(outcome, kind, "upstream_read", error);
}
async fn send_upload(
    sender: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    frame: Vec<u8>,
    cancellation: &CancellationToken,
) -> Result<(), GatewayError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(GatewayError::ClientCancelled),
        result = tokio::time::timeout(Duration::from_secs(30), sender.send(Ok(Bytes::from(frame)))) => {
            result.map_err(|_| GatewayError::UpstreamTimeout { phase: crate::error::TimeoutPhase::Streaming })?
                .map_err(|_| unavailable("Cursor upload stream closed"))
        }
    }
}
fn headers(token: &str, stream: bool) -> Result<HeaderMap, GatewayError> {
    let mut result = HeaderMap::new();
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| unavailable("invalid Cursor credential header"))?;
    authorization.set_sensitive(true);
    result.insert("authorization", authorization);
    result.insert(
        "content-type",
        HeaderValue::from_static(if stream {
            "application/connect+proto"
        } else {
            "application/proto"
        }),
    );
    result.insert("connect-protocol-version", HeaderValue::from_static("1"));
    result.insert("x-cursor-client-type", HeaderValue::from_static("cli"));
    result.insert(
        "x-cursor-client-version",
        HeaderValue::from_static(CLIENT_VERSION),
    );
    result.insert("x-ghost-mode", HeaderValue::from_static("true"));
    result.insert(
        "x-request-id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).unwrap(),
    );
    if stream {
        result.insert("te", HeaderValue::from_static("trailers"));
        result.insert("connect-accept-encoding", HeaderValue::from_static("gzip"));
        result.insert(
            "x-cursor-agent-allowed-tools",
            HeaderValue::from_static("mcp_tool_call"),
        );
    }
    Ok(result)
}

struct CatalogEntry {
    at: Instant,
    models: Vec<catalog::CursorModel>,
    _default_model: Option<String>,
}
type CatalogCache = HashMap<String, Arc<Mutex<Option<CatalogEntry>>>>;
static CATALOGS: OnceLock<Mutex<CatalogCache>> = OnceLock::new();

pub(crate) async fn discover_models(
    gw: &Gateway,
    provider: &Provider,
    credential: &StoredCredential,
    force: bool,
) -> anyhow::Result<Vec<catalog::CursorModel>> {
    let token = credential
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Cursor credential is missing"))?;
    let key = format!(
        "{}:{}:{CLIENT_VERSION}",
        provider.id,
        hex::encode(Sha256::digest(token.as_bytes()))
    );
    let cache = CATALOGS.get_or_init(|| Mutex::new(HashMap::new()));
    let slot = {
        let mut map = cache.lock().await;
        // Bound stale token-version entries without retaining credentials.
        if map.len() >= 256 {
            map.retain(|existing, value| existing == &key || Arc::strong_count(value) > 1);
        }
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut entry = slot.lock().await;
    if !force {
        if let Some(cached) = entry
            .as_ref()
            .filter(|cached| cached.at.elapsed() < Duration::from_secs(300))
        {
            return Ok(cached.models.clone());
        }
    }
    let client = ProxyClient::new(gw.required_http2_client(provider.use_proxy).await?)
        .with_timeouts(gw.config.upstream_timeouts)
        .with_deadline(Deadline::from_now(Duration::from_secs(10)));
    let result = client
        .call_http2_unary(
            &format!("{BASE_URL}{}", super::wire::GET_USABLE_MODELS_PATH),
            headers(token, false).map_err(|error| anyhow::anyhow!(error))?,
            Bytes::from(catalog::encode_get_usable_models_request()),
            CancellationToken::new(),
            super::framing::MAX_INFLATED_FRAME,
        )
        .await?;
    anyhow::ensure!(
        (200..300).contains(&result.0),
        "Cursor model discovery returned HTTP {}",
        result.0
    );
    let mut models = catalog::decode_get_usable_models_response(&result.2)?;
    let optional_client = client.with_deadline(Deadline::from_now(Duration::from_secs(2)));
    let enrichment_url = format!("{BASE_URL}{}", super::discovery::AVAILABLE_MODELS_PATH);
    let default_url = format!("{BASE_URL}{}", super::discovery::GET_DEFAULT_MODEL_PATH);
    let enrichment = optional_client.call_http2_unary(
        &enrichment_url,
        headers(token, false).map_err(|error| anyhow::anyhow!(error))?,
        Bytes::from(catalog::encode_available_models_request()),
        CancellationToken::new(),
        super::framing::MAX_INFLATED_FRAME,
    );
    let default_model = optional_client.call_http2_unary(
        &default_url,
        headers(token, false).map_err(|error| anyhow::anyhow!(error))?,
        Bytes::from(catalog::encode_get_default_model_for_cli_request()),
        CancellationToken::new(),
        super::framing::MAX_INFLATED_FRAME,
    );
    let (enrichment, default_model) = tokio::join!(enrichment, default_model);
    if let Ok((200, _, body)) = enrichment {
        if catalog::decode_enrich_models(&body, &mut models).is_err() {
            tracing::warn!(provider_id = %provider.id, "Cursor optional model metadata rejected; keeping authoritative IDs");
        }
    }
    let default_model = default_model
        .ok()
        .filter(|response| response.0 == 200)
        .and_then(|response| {
            catalog::decode_get_default_model_for_cli_response(&response.2)
                .ok()
                .flatten()
        })
        .filter(|id| models.iter().any(|model| model.id == *id));
    *entry = Some(CatalogEntry {
        at: Instant::now(),
        models: models.clone(),
        _default_model: default_model,
    });
    Ok(models)
}

static GLOBAL_RUNS: OnceLock<Arc<Semaphore>> = OnceLock::new();
static PROVIDER_RUNS: OnceLock<std::sync::Mutex<HashMap<String, std::sync::Weak<Semaphore>>>> =
    OnceLock::new();
fn permits(provider: &str) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), GatewayError> {
    let exhausted = || GatewayError::UpstreamStatus {
        provider: "cursor".into(),
        status: 429,
        body: Some("Cursor concurrent run limit reached".into()),
    };
    let global = GLOBAL_RUNS
        .get_or_init(|| Arc::new(Semaphore::new(64)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| exhausted())?;
    let pool = {
        let mut pools = PROVIDER_RUNS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        pools.retain(|_, weak| weak.strong_count() > 0);
        let pool = pools
            .get(provider)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| Arc::new(Semaphore::new(16)));
        pools.insert(provider.into(), Arc::downgrade(&pool));
        pool
    };
    let local = pool.try_acquire_owned().map_err(|_| exhausted())?;
    Ok((global, local))
}

pub(crate) async fn start(
    mut request: AiRequest,
    ctx: &ProviderCtx<'_>,
    transport: ProxyClient,
    control: AttemptControl,
) -> Result<IrUpstream, GatewayError> {
    if ctx.egress_base_url.trim_end_matches('/') != BASE_URL {
        return Err(unavailable(
            "Cursor endpoint must use its fixed official HTTPS host",
        ));
    }
    request.model = ctx.actual_model.to_string();
    // First validate non-reasoning capabilities without requiring a network call.
    let reasoning = request.reasoning.clone();
    request.reasoning = Default::default();
    history::preflight(&request).map_err(gateway_error)?;
    let credential = ctx
        .credential
        .ok_or_else(|| unavailable("Cursor OAuth credential is missing"))?;
    let models = discover_models(ctx.gw, ctx.provider, credential, false)
        .await
        .map_err(network)?;
    if !models.iter().any(|model| model.id == ctx.actual_model) {
        return Err(GatewayError::ModelNotFound {
            model: ctx.actual_model.into(),
        });
    }
    let route = catalog::resolve_route(
        &models,
        ctx.actual_model,
        &reasoning,
        ctx.force_max_reasoning,
    )
    .map_err(gateway_error)?;
    request.model = route.legacy_model_id.clone();
    let actual_model = route.legacy_model_id.clone();
    let model_metadata = serde_json::json!({
        "legacy_model_id": route.legacy_model_id, "requested_model_id": route.requested_model_id,
        "parameters": route.parameters.iter().map(|parameter| serde_json::json!({"id": parameter.id, "value": parameter.value})).collect::<Vec<_>>(),
        "max_mode": route.max_mode, "protocol_source_revision": super::wire::SOURCE_REVISION,
    });
    let permits = match permits(&ctx.provider.id) {
        Ok(permits) => permits,
        Err(error) => {
            control
                .local_failure
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(error);
        }
    };
    let (mut session, initial) =
        session::CursorSession::start(&request, route).map_err(gateway_error)?;
    let identity = headers(ctx.api_key, true)?;
    let url = format!("{BASE_URL}{RUN_PATH}");
    let (upload_tx, upload_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    let session_cancel = CancellationToken::new();
    let (stop_link, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let parent_cancel = control.cancellation.clone();
    let child_cancel = session_cancel.clone();
    let deadline = control.deadline.clone();
    let link_task = tokio::spawn(async move {
        tokio::select! {
            _ = &mut stop_rx => {},
            _ = parent_cancel.cancelled() => child_cancel.cancel(),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.at())) => child_cancel.cancel(),
        }
    });
    let mut initial_bytes = Vec::new();
    let mut initial_events = Vec::new();
    for action in initial {
        match action {
            session::Action::SendFrame(bytes) => {
                initial_bytes.extend_from_slice(&bytes);
                upload_tx
                    .try_send(Ok(Bytes::from(bytes)))
                    .map_err(|_| unavailable("Cursor initial upload exceeds queue capacity"))?;
            }
            session::Action::Event(event) => initial_events.push(event),
            _ => return Err(unavailable("unexpected Cursor initial action")),
        }
    }
    if !initial_events.is_empty() {
        event_tx
            .try_send(Ok(initial_events))
            .map_err(|_| unavailable("Cursor initial event queue is closed"))?;
    }
    control.performance.configure_native_stream();
    control.performance.note_native_request(&identity, &initial_bytes, &serde_json::json!({
        "message_count": request.messages.len(), "tool_count": request.tools.as_ref().map_or(0, Vec::len),
        "has_system": request.system.is_some(), "stream": request.stream.enabled,
    }));
    control.mark_submitted();
    let response = match transport
        .call_duplex(
            &url,
            identity,
            Box::pin(ReceiverStream::new(upload_rx)),
            session_cancel.clone(),
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            link_task.abort();
            session_cancel.cancel();
            return Err(network(error));
        }
    };
    if !(200..300).contains(&response.status) {
        if matches!(response.status, 401 | 403 | 404 | 429 | 503) {
            control
                .rejected
                .store(true, std::sync::atomic::Ordering::Release);
        }
        link_task.abort();
        session_cancel.cancel();
        return Err(GatewayError::UpstreamStatus {
            provider: "cursor".into(),
            status: response.status,
            body: Some(format!(
                "Cursor AgentService returned HTTP {}",
                response.status
            )),
        });
    }
    let status = response.status;
    let response_headers = response.headers.clone();
    let performance = control.performance.clone();
    let worker_cancel = session_cancel.clone();
    let worker = tokio::spawn(async move {
        let _permits = permits;
        let _stop_link = stop_link;
        let mut frames = response.frames;
        let mut upload_tx = Some(upload_tx);
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        loop {
            let next = tokio::select! {
                biased;
                _ = worker_cancel.cancelled() => break,
                _ = event_tx.closed() => break,
                _ = heartbeat.tick(), if upload_tx.is_some() => {
                    match session.heartbeat_frame() {
                        Ok(frame) => if let Err(error) = send_upload(upload_tx.as_ref().unwrap(), frame, &worker_cancel).await {
                            record_failure(&performance, &error);
                            let _ = event_tx.send(Err(error)).await;
                            break;
                        },
                        Err(_) => break,
                    }
                    continue;
                }
                frame = frames.next() => frame,
            };
            let eof = next.is_none();
            let actions = match next {
                Some(Ok(UpstreamFrame::Data(bytes))) => session.accept_data(&bytes),
                Some(Ok(UpstreamFrame::Trailers(headers))) => {
                    session.accept_trailers(headers.iter().filter_map(|(key, value)| {
                        value.to_str().ok().map(|value| (key.as_str(), value))
                    }))
                }
                Some(Err(error)) => {
                    let error = network(error);
                    record_failure(&performance, &error);
                    let _ = event_tx.send(Err(error)).await;
                    break;
                }
                None => session.finish(),
            };
            let actions = match actions {
                Ok(actions) => actions,
                Err(error) => {
                    let error = gateway_response_error(error);
                    performance.record_failure(
                        "failed",
                        "cursor_protocol_error",
                        "upstream_response",
                        &error,
                    );
                    let _ = event_tx.send(Err(error)).await;
                    break;
                }
            };
            let mut events = Vec::new();
            let mut terminal = None;
            let mut failure = None;
            for action in actions {
                match action {
                    session::Action::SendFrame(bytes) => {
                        let Some(sender) = upload_tx.as_ref() else {
                            failure =
                                Some(unavailable("Cursor attempted upload after transport end"));
                            break;
                        };
                        if let Err(error) = send_upload(sender, bytes, &worker_cancel).await {
                            failure = Some(error);
                            break;
                        }
                    }
                    session::Action::Event(event) => events.push(event),
                    session::Action::Terminal(reason) => terminal = Some(reason),
                    session::Action::Failure(error) => {
                        failure = Some(gateway_response_error(error));
                        break;
                    }
                }
            }
            if let Some(error) = failure {
                record_failure(&performance, &error);
                let _ = event_tx.send(Err(error)).await;
                break;
            }
            if session.upload_finished() {
                upload_tx.take();
            }
            let usage = session.usage();
            let usage_provenance = if usage.input_tokens.is_some() && usage.output_tokens.is_some()
            {
                crate::performance::NativeUsageProvenance::Reported
            } else if usage.input_tokens.is_some()
                || usage.output_tokens.is_some()
                || usage.cache_read_tokens.is_some()
                || usage.cache_write_tokens.is_some()
            {
                crate::performance::NativeUsageProvenance::Partial
            } else {
                crate::performance::NativeUsageProvenance::Unknown
            };
            performance.note_native_usage_provenance(usage_provenance);
            if let Some(reason) = terminal {
                performance.observe_native_terminal(match reason {
                    session::TerminalReason::Completed => {
                        crate::performance::NativeTerminalEvidence::Completed
                    }
                    session::TerminalReason::ToolHandoff => {
                        crate::performance::NativeTerminalEvidence::ToolHandoff
                    }
                });
            }
            if !events.is_empty() {
                let sent = tokio::select! {
                    _ = worker_cancel.cancelled() => false,
                    result = event_tx.send(Ok(events)) => result.is_ok(),
                };
                if !sent {
                    break;
                }
            }
            if session.terminal_reason().is_some() || eof {
                break;
            }
        }
        drop(upload_tx);
        drop(frames);
    });
    Ok(IrUpstream {
        status,
        headers: response_headers,
        url,
        actual_model,
        model_metadata,
        events: Box::pin(ReceiverStream::new(event_rx)),
        guard: SessionGuard::new(
            session_cancel,
            vec![worker.abort_handle(), link_task.abort_handle()],
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::client::{UpstreamTimeoutPhase, duplex::DuplexError};

    #[test]
    fn typed_cancel_timeout_and_limits_keep_their_origin() {
        assert!(matches!(
            network(DuplexError::Cancelled.into()),
            GatewayError::ClientCancelled
        ));
        assert!(matches!(
            network(DuplexError::Timeout(UpstreamTimeoutPhase::FirstByte).into()),
            GatewayError::UpstreamTimeout {
                phase: crate::error::TimeoutPhase::FirstByte
            }
        ));
        let limit = super::super::errors::CursorError::LimitExceeded {
            resource: "frame",
            limit: 4,
        };
        assert_eq!(gateway_error(limit.clone()).http_status().as_u16(), 413);
        assert_eq!(gateway_response_error(limit).http_status().as_u16(), 502);
        assert!(headers("test-bearer", true).unwrap()["authorization"].is_sensitive());
    }

    #[tokio::test]
    async fn upload_burst_waits_for_capacity_without_losing_frames() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let cancel = CancellationToken::new();
        let sender = tokio::spawn(async move {
            for index in 0..96u8 {
                send_upload(&tx, vec![index], &cancel).await.unwrap();
            }
        });
        for index in 0..96u8 {
            tokio::task::yield_now().await;
            assert_eq!(rx.recv().await.unwrap().unwrap(), Bytes::from(vec![index]));
        }
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn upload_backpressure_is_cancellable() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"full"))).await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            send_upload(&tx, vec![0], &cancel).await,
            Err(GatewayError::ClientCancelled)
        ));
    }
}

/// Admin probes use the same wire path, not a JSON request to the base URL.
pub(crate) async fn probe(
    gw: &Gateway,
    provider: &Provider,
    credential: &StoredCredential,
    model: &str,
) -> anyhow::Result<()> {
    use crate::protocol::ir::request::{Message, MessageContent, Role};
    let request = AiRequest::new(
        model,
        vec![Message {
            role: Role::User,
            content: MessageContent::Text("Reply with OK.".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let deadline = Deadline::from_now(Duration::from_secs(30));
    let cancellation = CancellationToken::new();
    let performance =
        crate::performance::Attempt::new(gw.log_tx.clone(), cancellation.clone(), deadline.clone());
    let client = ProxyClient::new(gw.required_http2_client(provider.use_proxy).await?)
        .with_timeouts(gw.config.upstream_timeouts)
        .with_deadline(deadline.clone())
        .with_performance(performance.clone());
    let context = ProviderCtx {
        provider,
        protocol: crate::protocol::ids::CURSOR_AGENT_RUN_V1,
        egress_base_url: BASE_URL,
        api_key: credential.access_token.as_deref().unwrap_or(""),
        auth_scheme: "bearer",
        actual_model: model,
        force_max_reasoning: false,
        credential: Some(credential),
        gw,
        disable_default_auth: true,
    };
    let mut upstream = start(
        request,
        &context,
        client,
        AttemptControl {
            deadline,
            cancellation,
            performance,
            run_submitted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            local_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rejected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .await?;
    let mut completed = false;
    while let Some(batch) = upstream.events.next().await {
        for event in batch? {
            if let AiStreamDelta::Done { stop_reason } = event {
                completed = stop_reason == "stop";
            }
        }
    }
    anyhow::ensure!(completed, "Cursor probe did not reach verified completion");
    Ok(())
}
