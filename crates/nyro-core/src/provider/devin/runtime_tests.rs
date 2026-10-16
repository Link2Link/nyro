//! In-process HTTP tests for the native Devin transport, never live credentials.
use super::*;
use crate::protocol::ir::{Message as ChatMessage, MessageContent, Role};
use crate::provider::devin::wire;
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
    ProxyClient::new(reqwest::Client::builder().no_proxy().build().unwrap())
        .with_deadline(control.deadline.clone())
        .with_performance(control.performance.clone())
}
fn input(streaming: bool) -> AiRequest {
    let mut request = AiRequest::new(
        "swe-2",
        vec![ChatMessage {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = streaming;
    request
}
fn frame(value: wire::ChatResponse) -> Vec<u8> {
    wire::envelope(&value.encode_to_vec()).unwrap()
}
fn end(value: &[u8]) -> Vec<u8> {
    let mut bytes = wire::envelope(value).unwrap();
    bytes[0] = 2;
    bytes
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
        CHAT_PATH,
        post(move |request: Request<Body>| {
            let capture = capture.clone();
            let response = body.clone();
            async move {
                let headers = request.headers().clone();
                let request_body = axum::body::to_bytes(request.into_body(), wire::MAX_FRAME + 5)
                    .await
                    .unwrap();
                if let Some(tx) = capture.lock().unwrap().take() {
                    let _ = tx.send((headers, request_body.to_vec()));
                }
                // Split headers and protobuf fields across arbitrary HTTP chunks.
                let chunks: Vec<Result<Bytes, std::io::Error>> = response
                    .chunks(3)
                    .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                    .collect();
                Response::builder()
                    .status(status)
                    .header("content-type", "application/connect+proto")
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
    (format!("http://{address}{CHAT_PATH}"), rx, server)
}

#[tokio::test]
async fn mocked_transport_headers_payload_and_text_tool_usage_in_both_modes() {
    for streaming in [false, true] {
        let mut bytes = frame(wire::ChatResponse {
            text: b"hello back".into(),
            ..Default::default()
        });
        bytes.extend(frame(wire::ChatResponse {
            tools: vec![wire::CallDelta {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: br#"{"q":"ok"}"#.to_vec(),
                ..Default::default()
            }],
            ..Default::default()
        }));
        bytes.extend(frame(wire::ChatResponse {
            usage: Some(wire::UsageSnapshot {
                input: Some(2),
                output: Some(3),
                cache_read: Some(5),
                cache_write: Some(7),
                ..Default::default()
            }),
            stop_reason: 10,
            ..Default::default()
        }));
        bytes.extend(end(b"{}"));
        let (url, captured, server) = mock(200, bytes).await;
        let control = control();
        let mut upstream = execute(
            input(streaming),
            "swe-2",
            "mock-token",
            url,
            client(&control),
            control.clone(),
        )
        .await
        .unwrap();
        let mut events = Vec::new();
        while let Some(batch) = upstream.events.next().await {
            events.extend(batch.unwrap());
        }
        let (headers, body) = captured.await.unwrap();
        assert_eq!(headers["authorization"], "Basic mock-token-mock-token");
        assert_eq!(headers["content-type"], "application/connect+proto");
        assert_eq!(headers["connect-protocol-version"], "1");
        assert!(headers.contains_key("sentry-trace"));
        assert!(!headers.contains_key("user-agent"));
        assert_eq!(body[0], 0);
        let decoded = wire::ChatRequest::decode(&body[5..]).unwrap();
        assert_eq!(decoded.model, "swe-2-high");
        assert_eq!(decoded.metadata.unwrap().token, "mock-token");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AiStreamDelta::TextDelta(t) if t == "hello back"))
        );
        assert!(events.iter().any(|e| matches!(e, AiStreamDelta::ToolCallComplete { tool_call, .. } if tool_call.id == "call_1" && tool_call.arguments == r#"{"q":"ok"}"#)));
        assert!(events.iter().any(|e| matches!(e, AiStreamDelta::Usage(u) if u.prompt_tokens == 14 && u.completion_tokens == 3 && u.total_tokens == 17)));
        assert!(
            matches!(events.last(), Some(AiStreamDelta::Done { stop_reason }) if stop_reason == "tool_calls")
        );
        assert!(control.is_submitted());
        assert!(!control.rejected.load(Ordering::Acquire));
        assert_ne!(
            control.performance.diagnostic().attempt_outcome,
            "cancelled",
            "verified EOS must still drain original HTTP EOF"
        );
        assert_eq!(
            control.performance.diagnostic().payload_metadata["usage_provenance"]["source"],
            "devin_native"
        );
        drop(upstream);
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn bare_family_name_sends_the_variant_matching_the_requested_effort() {
    use crate::protocol::ir::ReasoningEffort;
    // One route name ("claude-opus-5-5") serves every tier: the request's own
    // reasoning effort picks the upstream UID that goes on the wire.
    for (effort, wire_uid) in [
        (None, "claude-opus-5-5-medium"),
        (Some(ReasoningEffort::Low), "claude-opus-5-5-low"),
        (Some(ReasoningEffort::High), "claude-opus-5-5-high"),
        (Some(ReasoningEffort::Xhigh), "claude-opus-5-5-xhigh"),
        (Some(ReasoningEffort::Max), "claude-opus-5-5-max"),
        // Thinking off has no variant of its own: it lands on the lowest tier.
        (Some(ReasoningEffort::None), "claude-opus-5-5-low"),
    ] {
        let mut bytes = frame(wire::ChatResponse {
            text: b"ok".into(),
            ..Default::default()
        });
        bytes.extend(frame(wire::ChatResponse {
            stop_reason: 1,
            ..Default::default()
        }));
        bytes.extend(end(b"{}"));
        let (url, captured, server) = mock(200, bytes).await;
        let control = control();
        let mut request = input(true);
        request.model = "claude-opus-5-5".into();
        request.reasoning.effort = effort.clone();
        let mut upstream = execute(
            request,
            "claude-opus-5-5",
            "mock-token",
            url,
            client(&control),
            control.clone(),
        )
        .await
        .unwrap();
        while upstream.events.next().await.is_some() {}
        let (_, body) = captured.await.unwrap();
        let decoded = wire::ChatRequest::decode(&body[5..]).unwrap();
        assert_eq!(decoded.model, wire_uid, "effort {effort:?}");
        // The log keeps both sides: what the client asked for and what was sent.
        assert_eq!(upstream.actual_model, wire_uid);
        assert_eq!(
            upstream.model_metadata,
            serde_json::json!({"requested_model": "claude-opus-5-5", "upstream_model": wire_uid})
        );
        drop(upstream);
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn bare_haiku_name_resolves_into_a_published_variant() {
    use crate::protocol::ir::ReasoningEffort;
    // Regression for request 4d01f363: Devin's catalog publishes only
    // `-low/-medium/-high` variants for the haiku 5.5 line, so the bare
    // family name must never reach the wire (upstream answers 403
    // permission_denied). No directive lands on the family default.
    for (effort, wire_uid) in [
        (None, "claude-haiku-5-5-medium"),
        (Some(ReasoningEffort::Low), "claude-haiku-5-5-low"),
        (Some(ReasoningEffort::High), "claude-haiku-5-5-high"),
        // The line has no xhigh/max variant: requests clamp down to high.
        (Some(ReasoningEffort::Xhigh), "claude-haiku-5-5-high"),
        // Thinking off has no variant of its own: it lands on the lowest tier.
        (Some(ReasoningEffort::None), "claude-haiku-5-5-low"),
    ] {
        let mut bytes = frame(wire::ChatResponse {
            text: b"ok".into(),
            ..Default::default()
        });
        bytes.extend(frame(wire::ChatResponse {
            stop_reason: 1,
            ..Default::default()
        }));
        bytes.extend(end(b"{}"));
        let (url, captured, server) = mock(200, bytes).await;
        let control = control();
        let mut request = input(true);
        request.model = "claude-haiku-5-5".into();
        request.reasoning.effort = effort.clone();
        let mut upstream = execute(
            request,
            "claude-haiku-5-5",
            "mock-token",
            url,
            client(&control),
            control.clone(),
        )
        .await
        .unwrap();
        while upstream.events.next().await.is_some() {}
        let (_, body) = captured.await.unwrap();
        let decoded = wire::ChatRequest::decode(&body[5..]).unwrap();
        assert_ne!(decoded.model, "claude-haiku-5-5", "effort {effort:?}");
        assert_eq!(decoded.model, wire_uid, "effort {effort:?}");
        assert_eq!(upstream.actual_model, wire_uid);
        drop(upstream);
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mocked_transport_rejects_truncation_and_error_trailers_without_done() {
    for bytes in [
        frame(wire::ChatResponse {
            text: b"partial".into(),
            ..Default::default()
        }),
        end(br#"{"error":{"code":"resource_exhausted","message":"secret-token quota"}}"#),
    ] {
        let (url, _captured, server) = mock(200, bytes).await;
        let control = control();
        let mut upstream = execute(
            input(true),
            "swe-2",
            "mock-token",
            url,
            client(&control),
            control,
        )
        .await
        .unwrap();
        let mut failed = false;
        let mut done = false;
        while let Some(batch) = upstream.events.next().await {
            match batch {
                Ok(events) => {
                    done |= events
                        .iter()
                        .any(|e| matches!(e, AiStreamDelta::Done { .. }))
                }
                Err(error) => {
                    failed = true;
                    assert!(!error.to_string().contains("secret-token"));
                }
            }
        }
        assert!(failed);
        assert!(!done);
        drop(upstream);
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mocked_http_401_is_an_explicit_rejection() {
    let (url, _captured, server) = mock(401, b"secret-token".to_vec()).await;
    let control = control();
    let result = execute(
        input(false),
        "swe-2",
        "mock-token",
        url,
        client(&control),
        control.clone(),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("HTTP 401 accepted"),
    };
    assert_eq!(error.http_status().as_u16(), 401);
    assert!(!error.to_string().contains("secret-token"));
    assert!(control.rejected.load(Ordering::Acquire));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn precancelled_transport_does_not_connect() {
    let (url, mut captured, server) = mock(200, end(b"{}")).await;
    let control = control();
    control.cancellation.cancel();
    let result = execute(
        input(false),
        "swe-2",
        "mock-token",
        url,
        client(&control),
        control,
    )
    .await;
    assert!(matches!(result, Err(GatewayError::ClientCancelled)));
    // The biased cancellation branch wins without polling the HTTP request future.
    assert!(matches!(
        captured.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    server.abort();
    let _ = server.await;
}

// ---- GetUserStatus (unary): shared by the connection test and usage ----

async fn status_mock(
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
        STATUS_PATH,
        post(move |request: Request<Body>| {
            let capture = capture.clone();
            let response = body.clone();
            async move {
                let headers = request.headers().clone();
                let request_body = axum::body::to_bytes(request.into_body(), wire::MAX_FRAME)
                    .await
                    .unwrap();
                if let Some(tx) = capture.lock().unwrap().take() {
                    let _ = tx.send((headers, request_body.to_vec()));
                }
                Response::builder()
                    .status(status)
                    .header("content-type", "application/proto")
                    .body(Body::from(response))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}{STATUS_PATH}"), rx, server)
}

fn status_client() -> ProxyClient {
    ProxyClient::new(reqwest::Client::builder().no_proxy().build().unwrap())
        .with_deadline(Deadline::from_now(Duration::from_secs(5)))
}

/// The shape observed from a live quota-billed Pro account.
fn live_status_body() -> Vec<u8> {
    use usage::{PlanInfo, PlanStatus, StatusResponse, UserStatus};
    StatusResponse {
        user: Some(UserStatus {
            plan_status: Some(PlanStatus {
                daily_remaining_percent: Some(83),
                weekly_remaining_percent: Some(91),
                overage_balance_micros: 10_000_000,
                daily_reset_unix: 1_792_137_600,
                weekly_reset_unix: 1_792_310_400,
                ..Default::default()
            }),
        }),
        plan: Some(PlanInfo {
            name: "Pro".into(),
            billing_strategy: 2,
            ..Default::default()
        }),
    }
    .encode_to_vec()
}

#[tokio::test]
async fn mocked_status_request_sends_the_unary_identity_and_decodes_usage() {
    let (url, captured, server) = status_mock(200, live_status_body()).await;

    let status = request_status_with(&status_client(), "mock-token", &url)
        .await
        .unwrap();
    let usage = usage::snapshot(status).unwrap();

    let (headers, body) = captured.await.unwrap();
    // Unary protobuf, not a Connect envelope, with the literal Basic scheme.
    assert_eq!(headers["authorization"], "Basic mock-token-mock-token");
    assert_eq!(headers["content-type"], "application/proto");
    assert_eq!(headers["connect-protocol-version"], "1");
    assert!(!headers.contains_key("sentry-trace"));
    assert!(!headers.contains_key("user-agent"));
    assert_eq!(
        wire::ChatRequest::decode(body.as_slice())
            .unwrap()
            .metadata
            .unwrap()
            .token,
        "mock-token"
    );
    assert_eq!(usage.plan_name.as_deref(), Some("Pro"));
    assert_eq!(usage.daily.as_ref().unwrap().remaining_percent, 83);
    assert_eq!(usage.weekly.as_ref().unwrap().remaining_percent, 91);
    assert_eq!(usage.overage_usd, Some(10.0));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mocked_status_auth_failures_tell_the_user_to_reauthorize_without_leaking() {
    for code in [401, 403] {
        let (url, _captured, server) = status_mock(code, b"secret-token".to_vec()).await;

        let error = request_status_with(&status_client(), "mock-token", &url)
            .await
            .unwrap_err()
            .to_string();

        // The admin route classifies these phrases as a PROVIDER credential
        // failure (502), never as an expired admin session (401).
        assert!(error.contains("authentication failed"), "{error}");
        assert!(error.contains("re-authorize"), "{error}");
        assert!(!error.contains("secret-token"));
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mocked_status_other_http_errors_and_bad_bodies_fail_closed() {
    let (url, _c, server) = status_mock(500, b"secret-token".to_vec()).await;
    let error = request_status_with(&status_client(), "mock-token", &url)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("HTTP 500"), "{error}");
    assert!(!error.contains("secret-token"));
    server.abort();
    let _ = server.await;

    let (url, _c, server) = status_mock(200, b"\xff\xff\xffsecret-token".to_vec()).await;
    let error = request_status_with(&status_client(), "mock-token", &url)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Invalid Devin status response");
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mocked_status_response_is_size_bounded() {
    let (url, _c, server) = status_mock(200, vec![0; MAX_STATUS_BYTES + 1]).await;

    let error = request_status_with(&status_client(), "mock-token", &url)
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(error, "Devin status response is too large");
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mocked_status_reply_without_a_user_is_not_an_unlimited_account() {
    let body = usage::StatusResponse {
        user: None,
        plan: None,
    }
    .encode_to_vec();
    let (url, _c, server) = status_mock(200, body).await;

    let status = request_status_with(&status_client(), "mock-token", &url)
        .await
        .unwrap();

    assert!(usage::snapshot(status).is_err());
    server.abort();
    let _ = server.await;
}
