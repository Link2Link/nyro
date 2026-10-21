//! Kiro transport: a JSON request, an AWS event-stream response, and the
//! existing gateway cancellation, deadline, usage observation and native-IR
//! downstream conversion.
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio_stream::wrappers::ReceiverStream;

use super::credential::KiroCredential;
use super::stream::{Session, SessionOptions};
use super::usage::{self, KiroUsage};
use super::{catalog, request, unavailable};
use crate::auth::types::StoredCredential;
use crate::protocol::ids::KIRO_CHAT_V1;
use crate::protocol::ir::{AiRequest, AiStreamDelta};
use crate::provider::native::{AttemptControl, IrUpstream, SessionGuard};
use crate::provider::vendor::ProviderCtx;
use crate::proxy::client::{
    ObservedResponse, ProxyClient, UpstreamReadError, UpstreamTimeoutPhase,
};
use crate::proxy::context::{CancellationToken, Deadline};
use crate::{
    Gateway,
    db::models::Provider,
    error::{AccountFailure, AccountFailureReason as Reason, GatewayError, TimeoutPhase},
};

const STREAM_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
const IDE_VERSION: &str = "0.11.63";
/// Error bodies are read only to classify the failure; never forwarded.
const MAX_ERROR_BYTES: usize = 16 * 1024;
const MAX_USAGE_BYTES: usize = 1024 * 1024;
const MAX_DETAIL_CHARS: usize = 300;

fn sensitive(value: &str) -> Result<HeaderValue, GatewayError> {
    let mut value =
        HeaderValue::from_str(value).map_err(|_| unavailable("invalid Kiro credential header"))?;
    value.set_sensitive(true);
    Ok(value)
}

fn token_headers(token: &str) -> Result<HeaderMap, GatewayError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(unavailable("Kiro OAuth credential is missing"));
    }
    let mut headers = HeaderMap::new();
    headers.insert("authorization", sensitive(&format!("Bearer {token}"))?);
    headers.insert(
        "amz-sdk-invocation-id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).unwrap(),
    );
    if super::credential::is_api_key(token) {
        headers.insert("tokentype", HeaderValue::from_static("API_KEY"));
    }
    Ok(headers)
}

fn stream_headers(token: &str, credential: &KiroCredential) -> Result<HeaderMap, GatewayError> {
    // Generation and usage share the Kiro IDE identity, as in the reference:
    // https://github.com/justlovemaki/AIClient-2-API/blob/master/src/providers/claude/claude-kiro.js
    // Keep operation-specific headers (including retry metadata) separate.
    let mut headers = ide_headers(token, credential)?;
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=3"),
    );
    headers.insert("x-amz-target", HeaderValue::from_static(STREAM_TARGET));
    Ok(headers)
}

/// Stable per-account identity the IDE embeds in its user agent.
fn machine_id(credential: &KiroCredential) -> String {
    let seed = credential
        .profile_arn
        .as_deref()
        .or(credential.client_id.as_deref())
        .unwrap_or("KIRO_DEFAULT_MACHINE");
    hex::encode(Sha256::digest(seed.as_bytes()))
}

fn ide_headers(token: &str, credential: &KiroCredential) -> Result<HeaderMap, GatewayError> {
    let mut headers = token_headers(token)?;
    let machine = machine_id(credential);
    let os = std::env::consts::OS;
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=1"),
    );
    for (name, value) in [
        (
            "x-amz-user-agent",
            format!("aws-sdk-js/1.0.34 KiroIDE-{IDE_VERSION}-{machine}"),
        ),
        (
            "user-agent",
            format!(
                "aws-sdk-js/1.0.34 ua/2.1 os/{os} lang/js md/nodejs#22.12.0 api/codewhispererstreaming#1.0.34 m/E KiroIDE-{IDE_VERSION}-{machine}"
            ),
        ),
    ] {
        headers.insert(
            name,
            HeaderValue::from_str(&value).map_err(|_| unavailable("invalid Kiro header"))?,
        );
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
    // Never expose upstream bodies or error chains: they can echo credentials.
    GatewayError::UpstreamStatus {
        provider: "kiro".into(),
        status: 502,
        body: Some("Kiro transport failed".into()),
    }
}

// ── Error classification ─────────────────────────────────────────────────────

fn collect_text(value: &Value, out: &mut String, depth: usize) {
    if depth > 3 {
        return;
    }
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_text(item, out, depth + 1)),
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(
                    key.to_ascii_lowercase().as_str(),
                    "message"
                        | "reason"
                        | "error"
                        | "error_description"
                        | "description"
                        | "detail"
                        | "details"
                        | "msg"
                        | "__type"
                        | "code"
                ) {
                    collect_text(child, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// Lower-cased message text of an upstream error body, `_` treated as a space
/// so `TEMPORARILY_SUSPENDED` and `temporarily suspended` match alike.
fn error_text(body: &[u8]) -> String {
    let mut text = String::new();
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => collect_text(&value, &mut text, 0),
        Err(_) => text.push_str(&String::from_utf8_lossy(body)),
    }
    text.to_ascii_lowercase().replace('_', " ")
}

fn detail(body: &[u8]) -> String {
    let mut text = String::new();
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        collect_text(&value, &mut text, 0);
    }
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DETAIL_CHARS)
        .collect()
}

/// Map a non-2xx upstream response to the status and (sanitized) message the
/// client sees. Quota exhaustion maps to 429 so the dispatcher fails over to
/// the next target; credential problems keep 401/403.
pub(super) fn classify(status: u16, body: &[u8]) -> GatewayError {
    let text = error_text(body);
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
    let token_rejected = has(&["token", "authorization", "authenticate", "credential"])
        && has(&["expired", "invalid", "unauthorized"]);
    let (mapped, message) = match status {
        401 => (
            401,
            "Kiro rejected the access token; re-authorize this provider".to_string(),
        ),
        402 => (429, "Kiro monthly request quota is exhausted".to_string()),
        403 if has(&[
            "suspended",
            "disabled",
            "violation of terms",
            "terms of service",
            "appeal",
        ]) =>
        {
            (403, "Kiro account is suspended or disabled".to_string())
        }
        403 if has(&["quota", "limit exceeded", "payment required"]) => {
            (429, "Kiro quota is exhausted".to_string())
        }
        403 if token_rejected => (
            401,
            "Kiro rejected the access token; re-authorize this provider".to_string(),
        ),
        403 => (
            403,
            "Kiro denied the request; check the account's profile and entitlement".to_string(),
        ),
        429 => (429, "Kiro rate limit reached".to_string()),
        400 | 422 => {
            let detail = detail(body);
            (
                400,
                if detail.is_empty() {
                    "Kiro rejected the request".to_string()
                } else {
                    format!("Kiro rejected the request: {detail}")
                },
            )
        }
        500..=599 => (status, "Kiro upstream error".to_string()),
        400..=499 => (status, format!("Kiro returned HTTP {status}")),
        _ => (502, format!("Kiro returned HTTP {status}")),
    };
    let reason = match mapped {
        401 => Some(Reason::AuthInvalid),
        403 if message.contains("suspended") => Some(Reason::AuthInvalid),
        403 => Some(Reason::ModelForbidden),
        429 if status == 402 || message.contains("quota is exhausted") => {
            Some(Reason::MonthlyQuota)
        }
        429 => Some(Reason::RateLimited),
        503 => Some(Reason::Capacity),
        _ => None,
    };
    if let Some(reason) = reason {
        // A bare proxy 403/429/503 is not proof that generation was refused.
        // Accept only a recognized AWS exception code or an explicit auth/billing
        // refusal; messages alone affect health, not replay permission.
        let code = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                ["__type", "code", "error"]
                    .iter()
                    .find_map(|key| value.get(key)?.as_str().map(str::to_owned))
            })
            .unwrap_or_default();
        let code = code.rsplit(['#', ':']).next().unwrap_or("");
        let explicit = matches!(status, 401 | 402)
            || matches!(
                code,
                "ThrottlingException"
                    | "TooManyRequestsException"
                    | "AccessDeniedException"
                    | "UnauthorizedException"
                    | "UnrecognizedClientException"
                    | "ExpiredTokenException"
                    | "ServiceQuotaExceededException"
                    | "LimitExceededException"
                    | "ServiceUnavailableException"
            );
        GatewayError::AccountUpstream {
            provider: "Kiro",
            status: mapped,
            message: Some(message),
            failure: AccountFailure {
                reason,
                rejected_before_execution: explicit,
                retry_after_seconds: None,
            },
        }
    } else {
        GatewayError::UpstreamStatus {
            provider: "kiro".into(),
            status: mapped,
            body: Some(message),
        }
    }
}

async fn read_limited(response: ObservedResponse, limit: usize) -> Vec<u8> {
    let mut stream = Box::pin(response.bytes_stream());
    let mut body = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = limit.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= limit {
            break;
        }
    }
    body
}

// ── Chat ─────────────────────────────────────────────────────────────────────

pub(super) async fn start(
    request: AiRequest,
    ctx: &ProviderCtx<'_>,
    transport: ProxyClient,
    control: AttemptControl,
) -> Result<IrUpstream, GatewayError> {
    super::validate(ctx.provider)?;
    let stored = ctx
        .credential
        .ok_or_else(|| unavailable("Kiro OAuth credential is missing"))?;
    let credential = KiroCredential::from_stored(stored);
    let url = credential.generate_url();
    execute(
        request,
        ctx.actual_model,
        ctx.api_key,
        &credential,
        url,
        transport,
        control,
    )
    .await
}

async fn execute(
    request: AiRequest,
    requested_model: &str,
    token: &str,
    credential: &KiroCredential,
    url: String,
    transport: ProxyClient,
    control: AttemptControl,
) -> Result<IrUpstream, GatewayError> {
    let identity = stream_headers(token, credential)?;
    let encoded = request::encode(&request, requested_model, credential.profile_arn.as_deref())?;
    let upstream_model = encoded.upstream_model.clone();
    let options = SessionOptions {
        thinking_tags: encoded.thinking_tags,
        tool_names: encoded.tool_names.clone(),
        input_estimate: encoded.input_estimate,
        context_tokens: catalog::context_tokens(&upstream_model),
    };
    control.performance.configure_kiro_stream();
    control
        .performance
        .note_native_request(&identity, &encoded.body, &encoded.summary);
    // Once submitted, even an empty timeout can represent billed generation.
    // Only a structured refusal can authorize another account attempt.
    control.mark_submitted();
    let response = tokio::select! {
        biased;
        _ = control.cancellation.cancelled() => return Err(GatewayError::ClientCancelled),
        result = transport.call_stream_raw(&url, identity, encoded.body.into()) => result.map_err(network)?,
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let body = read_limited(response, MAX_ERROR_BYTES).await;
        let mut error = classify(status, &body);
        if let GatewayError::AccountUpstream { failure, .. } = &mut error {
            failure.retry_after_seconds = retry_after;
            control.rejected.store(
                failure.rejected_before_execution,
                std::sync::atomic::Ordering::Release,
            );
        }
        return Err(error);
    }
    let response_headers = response.headers().clone();
    let (sender, receiver) = tokio::sync::mpsc::channel(32);
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let model = upstream_model.clone();
    let worker = tokio::spawn(async move {
        let mut session = Session::new(&model, options);
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
                Ok(events) => observe(&control, &session, events),
                Err(error) => control.performance.record_failure(
                    if matches!(error, GatewayError::UpstreamTimeout { .. }) {
                        "timed_out"
                    } else {
                        "failed"
                    },
                    if matches!(error, GatewayError::UpstreamTimeout { .. }) {
                        "timeout"
                    } else {
                        "kiro_protocol_error"
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
            if failed || eof {
                break;
            }
        }
    });
    Ok(IrUpstream {
        status,
        headers: response_headers,
        url,
        actual_model: upstream_model.clone(),
        model_metadata: serde_json::json!({
            "requested_model": requested_model,
            "upstream_model": upstream_model,
        }),
        events: Box::pin(ReceiverStream::new(receiver)),
        guard: SessionGuard::new(cancel, vec![worker.abort_handle()]),
    })
}

fn observe(control: &AttemptControl, session: &Session, events: &[AiStreamDelta]) {
    for event in events {
        match event {
            AiStreamDelta::Usage(_) => control
                .performance
                .note_native_usage_provenance(session.usage_provenance()),
            AiStreamDelta::Done { stop_reason } => {
                if stop_reason == "length" {
                    control.performance.record_message(
                        "output_limited",
                        "output_limit",
                        "upstream_response",
                        "Kiro output was limited by the upstream",
                    );
                } else {
                    control
                        .performance
                        .observe_native_terminal(if stop_reason == "tool_calls" {
                            crate::performance::NativeTerminalEvidence::ToolHandoff
                        } else {
                            crate::performance::NativeTerminalEvidence::Completed
                        });
                }
            }
            _ => {}
        }
    }
}

// ── Admin: connection check, usage, probe ────────────────────────────────────

async fn fetch_usage_with(
    client: &reqwest::Client,
    credential: &KiroCredential,
    token: &str,
    url: &str,
) -> anyhow::Result<KiroUsage> {
    let mut query = vec![
        ("isEmailRequired", "true"),
        ("origin", "AI_EDITOR"),
        ("resourceType", "AGENTIC_REQUEST"),
    ];
    if let Some(arn) = credential.profile_arn.as_deref() {
        query.push(("profileArn", arn));
    }
    let response = client
        .get(url)
        .timeout(Duration::from_secs(20))
        .query(&query)
        .headers(ide_headers(token, credential)?)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Kiro usage request could not connect"))?;
    let status = response.status();
    // Wording matters: callers map these phrases to a provider-credential
    // failure instead of an admin-session expiry.
    if matches!(status.as_u16(), 401 | 403) {
        anyhow::bail!(
            "Kiro authentication failed (HTTP {}); re-authorize this provider",
            status.as_u16()
        );
    }
    anyhow::ensure!(
        status.is_success(),
        "Kiro usage check returned HTTP {}",
        status.as_u16()
    );
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Kiro usage response could not be read"))?
    {
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_USAGE_BYTES,
            "Kiro usage response is too large"
        );
        body.extend_from_slice(&chunk);
    }
    let raw: Value = serde_json::from_slice(&body)
        .map_err(|_| anyhow::anyhow!("Invalid Kiro usage response"))?;
    Ok(usage::parse(&raw))
}

/// Plan and quota windows of the account. Spends no generation request.
pub(crate) async fn fetch_usage(
    gw: &Gateway,
    provider: &Provider,
    credential: &StoredCredential,
    token: &str,
) -> anyhow::Result<KiroUsage> {
    super::validate(provider)?;
    let kiro = KiroCredential::from_stored(credential);
    let client = gw.devin_http_client(provider.use_proxy, false).await?;
    fetch_usage_with(&client, &kiro, token, &kiro.usage_limits_url()).await
}

/// Authenticated, non-generating health check.
pub(crate) async fn check_connection(
    gw: &Gateway,
    provider: &Provider,
    credential: &StoredCredential,
    token: &str,
) -> anyhow::Result<()> {
    fetch_usage(gw, provider, credential, token)
        .await
        .map(|_| ())
}

/// Explicit per-model probe: one tiny generation through the real transport.
pub(crate) async fn probe(
    gw: &Gateway,
    provider: &Provider,
    credential: &StoredCredential,
    model: &str,
) -> anyhow::Result<String> {
    use crate::protocol::ir::request::{Message as ChatMessage, MessageContent, Role};
    super::validate(provider)?;
    let token = credential.access_token.as_deref().unwrap_or_default();
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
    request.stream.enabled = true;
    let deadline = Deadline::from_now(Duration::from_secs(60));
    let cancellation = CancellationToken::new();
    let performance =
        crate::performance::Attempt::new(gw.log_tx.clone(), cancellation.clone(), deadline.clone());
    let client = ProxyClient::new(gw.devin_http_client(provider.use_proxy, true).await?)
        .with_timeouts(gw.config.upstream_timeouts)
        .with_deadline(deadline.clone())
        .with_performance(performance.clone());
    let ctx = ProviderCtx {
        provider,
        protocol: KIRO_CHAT_V1,
        egress_base_url: &provider.base_url,
        api_key: token,
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
    anyhow::ensure!(completed, "Kiro probe did not reach verified completion");
    Ok(if reply.is_empty() {
        "[completed]".into()
    } else {
        reply
    })
}

#[cfg(test)]
mod tests {
    use super::super::eventstream::{encode_event, encode_frame};
    use super::*;
    use crate::protocol::ir::{Message as ChatMessage, MessageContent, Role};
    use axum::{
        Router,
        body::{Body, Bytes},
        http::{Request, Response},
        routing::post,
    };
    use tokio::sync::{mpsc, oneshot};

    type Captured = (http::HeaderMap, Vec<u8>);

    fn control() -> AttemptControl {
        let deadline = Deadline::from_now(Duration::from_secs(5));
        let cancellation = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(2);
        AttemptControl {
            deadline: deadline.clone(),
            cancellation: cancellation.clone(),
            performance: crate::performance::Attempt::new(tx, cancellation, deadline),
            run_submitted: Arc::new(AtomicBool::new(false)),
            local_failure: Arc::new(AtomicBool::new(false)),
            rejected: Arc::new(AtomicBool::new(false)),
        }
    }

    fn client(control: &AttemptControl) -> ProxyClient {
        ProxyClient::new(
            reqwest::Client::builder()
                .no_proxy()
                .user_agent("Nyro Kiro Gateway")
                .build()
                .unwrap(),
        )
        .with_deadline(control.deadline.clone())
        .with_performance(control.performance.clone())
    }

    fn input() -> AiRequest {
        let mut request = AiRequest::new(
            "claude-sonnet-4-5",
            vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.stream.enabled = true;
        request
    }

    fn credential() -> KiroCredential {
        KiroCredential::from_stored(&StoredCredential::default())
    }

    async fn mock(
        status: u16,
        body: Vec<u8>,
    ) -> (
        String,
        oneshot::Receiver<Captured>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = oneshot::channel();
        let capture = Arc::new(std::sync::Mutex::new(Some(tx)));
        let app = Router::new().route(
            "/generateAssistantResponse",
            post(move |request: Request<Body>| {
                let capture = capture.clone();
                let response = body.clone();
                async move {
                    let headers = request.headers().clone();
                    let request_body = axum::body::to_bytes(request.into_body(), 4 * 1024 * 1024)
                        .await
                        .unwrap();
                    if let Some(tx) = capture.lock().unwrap().take() {
                        let _ = tx.send((headers, request_body.to_vec()));
                    }
                    // Split frames across arbitrary HTTP chunks.
                    let chunks: Vec<Result<Bytes, std::io::Error>> = response
                        .chunks(7)
                        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                        .collect();
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/vnd.amazon.eventstream")
                        .body(Body::from_stream(futures::stream::iter(chunks)))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            format!("http://{address}/generateAssistantResponse"),
            rx,
            server,
        )
    }

    async fn collect(mut upstream: IrUpstream) -> Result<Vec<AiStreamDelta>, GatewayError> {
        let mut out = Vec::new();
        while let Some(batch) = upstream.events.next().await {
            out.extend(batch?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn mocked_transport_sends_kiro_headers_and_decodes_text_tools_and_usage() {
        let mut wire = encode_event("assistantResponseEvent", r#"{"content":"hello back"}"#);
        wire.extend(encode_event(
            "toolUseEvent",
            r#"{"name":"lookup","toolUseId":"tu_1","input":"{\"q\":1}","stop":true}"#,
        ));
        wire.extend(encode_event(
            "contextUsageEvent",
            r#"{"contextUsagePercentage":2.0}"#,
        ));
        let (url, captured, server) = mock(200, wire).await;
        let control = control();
        let client = client(&control);
        let upstream = execute(
            input(),
            "claude-sonnet-4-5",
            "access-token",
            &credential(),
            url,
            client,
            control.clone(),
        )
        .await
        .unwrap();
        assert_eq!(upstream.status, 200);
        assert_eq!(upstream.actual_model, "claude-sonnet-4.5");
        let events = collect(upstream).await.unwrap();
        server.abort();

        let (headers, body) = captured.await.unwrap();
        assert_eq!(headers["authorization"], "Bearer access-token");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-amz-target"], STREAM_TARGET);
        assert!(
            headers["user-agent"]
                .to_str()
                .unwrap()
                .ends_with(&format!("KiroIDE-0.11.63-{}", machine_id(&credential())))
        );
        assert_eq!(
            headers["x-amz-user-agent"],
            format!(
                "aws-sdk-js/1.0.34 KiroIDE-0.11.63-{}",
                machine_id(&credential())
            )
        );
        assert_eq!(headers["amz-sdk-request"], "attempt=1; max=3");
        assert!(headers.get("tokentype").is_none());
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "hello"
        );
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4.5"
        );

        assert!(matches!(
            events.first(),
            Some(AiStreamDelta::MessageStart { .. })
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AiStreamDelta::TextDelta(t) if t == "hello back"))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            AiStreamDelta::ToolCallComplete { tool_call, .. }
                if tool_call.name == "lookup" && tool_call.arguments == r#"{"q":1}"#
        )));
        let usage = events.iter().find_map(|e| match e {
            AiStreamDelta::Usage(u) => Some(u.clone()),
            _ => None,
        });
        assert_eq!(usage.unwrap().total_tokens, 4_000, "2% of a 200k window");
        assert!(matches!(
            events.last(),
            Some(AiStreamDelta::Done { stop_reason }) if stop_reason == "tool_calls"
        ));
    }

    #[tokio::test]
    async fn api_keys_carry_the_token_type_header() {
        let wire = encode_event("assistantResponseEvent", r#"{"content":"ok"}"#);
        let (url, captured, server) = mock(200, wire).await;
        let control = control();
        let client = client(&control);
        let upstream = execute(
            input(),
            "claude-sonnet-4-5",
            "ksk_abc",
            &credential(),
            url,
            client,
            control.clone(),
        )
        .await
        .unwrap();
        collect(upstream).await.unwrap();
        server.abort();
        let (headers, _) = captured.await.unwrap();
        assert_eq!(headers["tokentype"], "API_KEY");
    }

    #[tokio::test]
    async fn upstream_errors_are_classified_and_never_echo_the_body() {
        for (status, body, expected_status, needle) in [
            (
                401,
                r#"{"message":"bad token SECRET-ABC"}"#,
                401,
                "re-authorize",
            ),
            (
                402,
                r#"{"message":"MONTHLY_REQUEST_COUNT exceeded SECRET-ABC"}"#,
                429,
                "quota",
            ),
            (
                403,
                r#"{"message":"Account TEMPORARILY_SUSPENDED SECRET-ABC"}"#,
                403,
                "suspended",
            ),
            (
                403,
                r#"{"message":"The bearer token included in the request is invalid. SECRET-ABC"}"#,
                401,
                "re-authorize",
            ),
            (
                429,
                r#"{"message":"slow down SECRET-ABC"}"#,
                429,
                "rate limit",
            ),
            (503, "overloaded SECRET-ABC", 503, "upstream error"),
        ] {
            let (url, _captured, server) = mock(status, body.as_bytes().to_vec()).await;
            let control = control();
            let client = client(&control);
            let error = match execute(input(), "m", "t", &credential(), url, client, control).await
            {
                Ok(_) => panic!("{status} must fail"),
                Err(error) => error,
            };
            server.abort();
            assert_eq!(error.http_status().as_u16(), expected_status);
            let message = error.message();
            assert!(message.to_lowercase().contains(needle), "{message}");
            assert!(!message.contains("SECRET"), "{message}");
            assert!(error.account_failure().is_some());
        }
    }

    #[test]
    fn account_failures_require_explicit_refusal_for_replay() {
        for status in [403, 429, 503] {
            let failure = classify(status, br#"{"message":"proxy rejected request"}"#)
                .account_failure()
                .unwrap();
            assert!(!failure.rejected_before_execution);
        }
        let limited = classify(429, br#"{"__type":"com.amazon#ThrottlingException"}"#)
            .account_failure()
            .unwrap();
        assert_eq!(limited.reason, Reason::RateLimited);
        assert!(limited.rejected_before_execution);
        let quota = classify(402, b"").account_failure().unwrap();
        assert_eq!(quota.reason, Reason::MonthlyQuota);
        assert!(quota.rejected_before_execution);
        let auth = classify(401, b"").account_failure().unwrap();
        assert_eq!(auth.reason, Reason::AuthInvalid);
        assert!(auth.rejected_before_execution);
    }

    #[tokio::test]
    async fn validation_errors_surface_the_upstream_reason_without_control_characters() {
        let error = classify(400, br#"{"message":"Improperly formed\u0007 request.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#);
        match error {
            GatewayError::UpstreamStatus {
                status: 400, body, ..
            } => {
                let body = body.unwrap();
                assert!(body.contains("Improperly formed request."), "{body}");
                assert!(!body.chars().any(char::is_control));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            classify(418, b""),
            GatewayError::UpstreamStatus { status: 418, .. }
        ));
        assert!(matches!(
            classify(302, b""),
            GatewayError::UpstreamStatus { status: 502, .. }
        ));
    }

    #[tokio::test]
    async fn a_mid_stream_exception_fails_the_stream_after_partial_output() {
        let mut wire = encode_event("assistantResponseEvent", r#"{"content":"partial"}"#);
        wire.extend(encode_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
            ],
            br#"{"message":"Too many requests"}"#,
        ));
        let (url, _captured, server) = mock(200, wire).await;
        let control = control();
        let client = client(&control);
        let mut upstream = execute(input(), "m", "t", &credential(), url, client, control)
            .await
            .unwrap();
        let mut texts = Vec::new();
        let mut failure = None;
        while let Some(batch) = upstream.events.next().await {
            match batch {
                Ok(events) => texts.extend(events),
                Err(error) => failure = Some(error),
            }
        }
        server.abort();
        assert!(
            texts
                .iter()
                .any(|e| matches!(e, AiStreamDelta::TextDelta(t) if t == "partial"))
        );
        assert!(matches!(
            failure,
            Some(GatewayError::AccountUpstream {
                status: 429,
                failure: AccountFailure {
                    rejected_before_execution: false,
                    ..
                },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_body_that_ends_without_content_is_an_error() {
        let wire = encode_event("meteringEvent", r#"{"usage":0.0}"#);
        let (url, _captured, server) = mock(200, wire).await;
        let control = control();
        let client = client(&control);
        let upstream = execute(input(), "m", "t", &credential(), url, client, control)
            .await
            .unwrap();
        let result = collect(upstream).await;
        server.abort();
        assert!(matches!(
            result,
            Err(GatewayError::UpstreamStatus { status: 502, .. })
        ));
    }

    #[test]
    fn headers_mark_the_token_sensitive_and_reject_blank_or_malformed_tokens() {
        let credential = credential();
        let headers = stream_headers("secret-token", &credential).unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(headers["amz-sdk-request"], "attempt=1; max=3");
        assert!(stream_headers("", &credential).is_err());
        assert!(stream_headers("   ", &credential).is_err());
        assert!(stream_headers("bad\nvalue", &credential).is_err());
        let ide = ide_headers("t", &credential).unwrap();
        assert!(ide["user-agent"].to_str().unwrap().contains("KiroIDE-"));
        assert!(ide.get("x-amz-target").is_none());
        assert_eq!(ide["amz-sdk-request"], "attempt=1; max=1");
        for name in ["user-agent", "x-amz-user-agent"] {
            assert_eq!(headers[name], ide[name]);
            let value = headers[name].to_str().unwrap().to_ascii_lowercase();
            assert!(!value.contains("nyro"));
            assert!(!value.contains("gateway"));
            assert!(!value.contains("amazonq-for-cli"));
        }
    }

    #[test]
    fn machine_id_is_stable_per_account_and_differs_between_accounts() {
        let mut a = credential();
        a.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/A".into());
        let mut b = credential();
        b.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/B".into());
        assert_eq!(machine_id(&a), machine_id(&a));
        assert_ne!(machine_id(&a), machine_id(&b));
        assert_eq!(machine_id(&a).len(), 64);
        for account in [&a, &b] {
            let stream = stream_headers("token", account).unwrap();
            let usage = ide_headers("token", account).unwrap();
            for name in ["user-agent", "x-amz-user-agent"] {
                assert_eq!(stream[name], usage[name]);
                assert!(
                    stream[name]
                        .to_str()
                        .unwrap()
                        .ends_with(&machine_id(account))
                );
            }
        }
        a.client_id = Some("client-a".into());
        let profile_id = machine_id(&a);
        a.client_id = Some("client-b".into());
        assert_eq!(machine_id(&a), profile_id, "profile takes precedence");
        a.profile_arn = None;
        assert_eq!(machine_id(&a), hex::encode(Sha256::digest(b"client-b")));
    }

    #[test]
    fn network_errors_are_typed_and_sanitized() {
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

    #[tokio::test]
    async fn usage_request_carries_profile_and_parses_the_plan() {
        let app = Router::new().route(
            "/getUsageLimits",
            axum::routing::get(|request: Request<Body>| async move {
                let query = request.uri().query().unwrap_or("").to_string();
                assert!(query.contains("resourceType=AGENTIC_REQUEST"), "{query}");
                assert!(query.contains("profileArn=arn"), "{query}");
                assert!(query.contains("origin=AI_EDITOR"), "{query}");
                let headers = request.headers();
                assert!(headers["user-agent"].to_str().unwrap().contains("KiroIDE-0.11.63-"));
                assert!(headers["x-amz-user-agent"].to_str().unwrap().starts_with("aws-sdk-js/1.0.34 KiroIDE-0.11.63-"));
                assert_eq!(headers["amz-sdk-request"], "attempt=1; max=1");
                assert!(!headers.contains_key("x-amz-target"));
                axum::Json(serde_json::json!({
                    "subscriptionInfo": {"subscriptionTitle": "KIRO PRO"},
                    "usageBreakdownList": [{"currentUsage": 5, "usageLimit": 50, "displayName": "Credits"}]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut credential = credential();
        credential.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/X".into());
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let usage = fetch_usage_with(
            &client,
            &credential,
            "t",
            &format!("http://{address}/getUsageLimits"),
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(usage.plan.as_deref(), Some("PRO"));
        assert_eq!(
            usage.items[0].remaining_percent().map(|p| p as i32),
            Some(90)
        );
    }

    #[tokio::test]
    async fn usage_auth_failures_use_the_credential_failure_wording() {
        let app = Router::new().route(
            "/getUsageLimits",
            axum::routing::get(|| async { (http::StatusCode::FORBIDDEN, "no") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = fetch_usage_with(
            &client,
            &credential(),
            "t",
            &format!("http://{address}/getUsageLimits"),
        )
        .await
        .unwrap_err();
        server.abort();
        assert!(
            error
                .to_string()
                .contains("authentication failed (HTTP 403)")
        );
    }
}
