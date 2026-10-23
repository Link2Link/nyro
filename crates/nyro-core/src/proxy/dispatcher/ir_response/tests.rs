use super::*;
use std::sync::Arc;
use std::time::Duration;

use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::Provider;
use crate::integrations::{HookContext, ResponseHook, ResponseHookRegistration};
use crate::plugin::phase::{
    Phase, PhaseCtx, PhaseHook, PhaseHookRegistration, PhaseOutcome, ResponseView,
};
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1, ProtocolId,
};
use crate::protocol::ir::AiResponse;
use crate::provider::native::SessionGuard;
use crate::proxy::context::CancellationToken;
use async_trait::async_trait;

const BUFFERED_HOOK_MODEL: &str = "__native_ir_buffered_hooks__";
const STREAM_HOOK_MODEL: &str = "__native_ir_stream_hooks__";

struct LegacyHook;
#[async_trait]
impl ResponseHook for LegacyHook {
    fn name(&self) -> &'static str {
        "native-ir-legacy-test"
    }
    async fn on_response(&self, _: &HookContext, response: &mut AiResponse, _: u64) {
        if response.model == BUFFERED_HOOK_MODEL {
            response.content.push_str("[legacy]");
        }
    }
}
inventory::submit! { ResponseHookRegistration { make: || Box::new(LegacyHook) } }

struct ResponsePhaseHook;
#[async_trait]
impl PhaseHook for ResponsePhaseHook {
    fn name(&self) -> &'static str {
        "native-ir-phase-test"
    }
    fn phase(&self) -> Phase {
        Phase::OnResponse
    }
    async fn run(&self, ctx: &mut PhaseCtx<'_>) -> PhaseOutcome {
        match &mut ctx.response {
            ResponseView::Full(response) if ctx.request.model == BUFFERED_HOOK_MODEL => {
                response.content.push_str("[phase]");
            }
            ResponseView::Stream(AiStreamDelta::TextDelta(text))
                if ctx.request.model == STREAM_HOOK_MODEL =>
            {
                text.push_str("[phase]");
            }
            _ => {}
        }
        PhaseOutcome::Continue
    }
}
inventory::submit! { PhaseHookRegistration { make: || Arc::new(ResponsePhaseHook) } }

struct Fixture {
    _dir: tempfile::TempDir,
    gw: Gateway,
    logs: tokio::sync::mpsc::Receiver<crate::logging::LogEntry>,
    provider: Provider,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (gw, logs) = Gateway::new(GatewayConfig {
            data_dir: dir.path().to_path_buf(),
            config_poll_interval: Duration::ZERO,
            ..Default::default()
        })
        .await
        .unwrap();
        let provider = serde_json::from_value(serde_json::json!({
            "id":"native-fixture", "name":"native-fixture", "vendor":"devin",
            "protocol":"devin-chat", "base_url":"https://server.codeium.com",
            "api_key":"fixture", "is_enabled":true, "created_at":"", "updated_at":""
        }))
        .unwrap();
        Self {
            _dir: dir,
            gw,
            logs,
            provider,
        }
    }

    async fn invoke(
        &self,
        upstream: IrUpstream,
        ingress: ProtocolId,
        stream: bool,
        model: &str,
    ) -> ConversionAttempt {
        let mut ctx = RequestContext::new(ingress, Duration::from_secs(10));
        let call = self.call_ctx(&ctx, ingress, stream, model);
        let mut request = AiRequest::new(model, Vec::new());
        request.stream.enabled = stream;
        handle_ir_response(
            upstream,
            &call,
            &request_extras(),
            ToolRoutePlan::default(),
            &mut ctx,
            &mut request,
            &HostContext::new(&self.gw),
        )
        .await
    }

    fn call_ctx<'a>(
        &'a self,
        ctx: &RequestContext,
        ingress: ProtocolId,
        stream: bool,
        model: &'a str,
    ) -> CallCtx<'a> {
        CallCtx {
            performance: None,
            gw: self.gw.clone(),
            provider: &self.provider,
            model_id: "native-fixture",
            model_name: "native-fixture",
            egress: ingress,
            ingress,
            ingress_str: "fixture",
            egress_str: "devin-chat",
            request_model: model,
            actual_model: model,
            backend_model: model,
            api_key_id: None,
            api_key_name: None,
            provider_key_name: None,
            is_stream: stream,
            enable_payload: Some(false),
            reasoning_effort: None,
            route_decision: None,
            start: Instant::now(),
            req_ext: ctx.extensions.clone(),
        }
    }
}

fn request_extras() -> RequestExtras {
    let headers = std::collections::HashMap::from([
        ("authorization".into(), "Bearer client-secret".into()),
        ("x-api-key".into(), "api-key-secret".into()),
        ("cookie".into(), "session=cookie-secret".into()),
        ("content-type".into(), "application/json".into()),
    ]);
    RequestExtras {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        headers: Some(Arc::new(crate::logging::payload::capture_header_map(
            &headers,
        ))),
        body: Some(Arc::new(crate::logging::payload::capture_json(
            &serde_json::json!({
                "model": "client-model",
                "messages": [{"role": "user", "content": "native logging request"}],
            }),
            true,
        ))),
    }
}

fn assert_request_extras(row: &crate::logging::LogEntry) {
    let expected = request_extras();
    assert_eq!(row.method.as_deref(), Some(expected.method.as_str()));
    assert_eq!(row.path.as_deref(), Some(expected.path.as_str()));
    let headers = row
        .client_request_headers
        .as_deref()
        .expect("client headers");
    let parsed: serde_json::Value = serde_json::from_str(headers).unwrap();
    for name in ["authorization", "x-api-key", "cookie"] {
        assert_eq!(parsed[name], "***", "{name} must remain redacted");
    }
    assert_eq!(parsed["content-type"], "application/json");
    for secret in ["client-secret", "api-key-secret", "cookie-secret"] {
        assert!(!headers.contains(secret));
    }
    let capture = expected.body.unwrap();
    assert_eq!(row.client_request_body, capture.body);
    assert_eq!(
        row.diagnostic.payload_metadata["client_request_body"],
        capture.metadata
    );
    assert_eq!(
        row.diagnostic.payload_metadata["client_request_headers"],
        expected.headers.unwrap().metadata
    );
    assert_eq!(
        row.diagnostic.payload_metadata["client_request_headers"]["redacted_headers"],
        3
    );
    // The sink still owns retention: attaching captures must not change this gate.
    assert_eq!(row.enable_payload, Some(false));
}

async fn next_log(fixture: &mut Fixture) -> crate::logging::LogEntry {
    tokio::time::timeout(Duration::from_secs(2), fixture.logs.recv())
        .await
        .expect("native log must be emitted")
        .expect("log channel remains open")
}

fn source(batches: Vec<Result<Vec<AiStreamDelta>, GatewayError>>) -> IrUpstream {
    IrUpstream {
        status: 200,
        headers: Default::default(),
        url: "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetChatMessage".into(),
        actual_model: "actual-native-model".into(),
        model_metadata: serde_json::json!({"legacy_model_id":"actual-native-model"}),
        events: Box::pin(futures::stream::iter(batches)),
        guard: SessionGuard::new(CancellationToken::new(), Vec::new()),
    }
}
fn start(model: &str) -> AiStreamDelta {
    AiStreamDelta::MessageStart {
        id: "native-response".into(),
        model: model.into(),
    }
}
fn done() -> AiStreamDelta {
    AiStreamDelta::Done {
        stop_reason: "stop".into(),
    }
}
async fn text(response: Response) -> String {
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("body must terminate")
    .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn native_success_logs_preserve_client_request_extras() {
    let mut fixture = Fixture::new().await;
    for stream in [false, true] {
        let result = fixture
            .invoke(
                source(vec![Ok(vec![
                    start("fixture"),
                    AiStreamDelta::TextDelta("answer".into()),
                    done(),
                ])]),
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                stream,
                "fixture",
            )
            .await;
        assert_eq!(result.response.status(), StatusCode::OK);
        assert!(text(result.response).await.contains("answer"));
        let row = next_log(&mut fixture).await;
        assert_eq!(row.client_status_code, 200);
        assert_eq!(row.is_stream, stream);
        assert_request_extras(&row);
    }
}

#[tokio::test]
async fn native_handler_failure_logs_preserve_client_request_extras() {
    let mut fixture = Fixture::new().await;
    for stream in [false, true] {
        for after_payload in [false, true] {
            let mut batches = vec![Ok(vec![start("fixture")])];
            if after_payload {
                batches.push(Ok(vec![AiStreamDelta::TextDelta("partial".into())]));
            }
            batches.push(Err(invalid_ir("native fixture failure")));
            let result = fixture
                .invoke(
                    source(batches),
                    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                    stream,
                    "fixture",
                )
                .await;
            let expected_status = if stream && after_payload { 200 } else { 502 };
            assert_eq!(result.response.status().as_u16(), expected_status);
            let _ = text(result.response).await;
            let row = next_log(&mut fixture).await;
            assert_eq!(row.client_status_code, i32::from(expected_status));
            assert_eq!(row.diagnostic.attempt_outcome, "failed");
            assert_request_extras(&row);
        }
    }
}

#[tokio::test]
async fn native_start_error_log_preserves_client_request_extras() {
    let mut fixture = Fixture::new().await;
    let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    let ctx = RequestContext::new(ingress, Duration::from_secs(10));
    let performance = crate::performance::Attempt::new(
        fixture.gw.log_tx.clone(),
        ctx.cancellation.clone(),
        ctx.deadline.clone(),
    );
    performance.configure_native_stream();
    performance.correlate(&ctx.request_id, 1);
    let mut call = fixture.call_ctx(&ctx, ingress, true, "claude-sonnet-5-5");
    call.performance = Some(performance.clone());
    let mut request = AiRequest::new("claude-sonnet-5-5", Vec::new());
    request.reasoning.effort = Some(crate::protocol::ir::ReasoningEffort::High);
    let error = invalid_ir("native start failed");
    performance.record_failure(
        "failed",
        "native_upstream_error",
        "upstream_request",
        &error,
    );
    // Exercise the exact log emitter used by the dispatcher's start_ir Err arm,
    // without real OAuth credentials or any native upstream network calls.
    super::super::log_native_start_error(&call, &request_extras(), &request, &error);
    let row = next_log(&mut fixture).await;
    assert_eq!(row.client_status_code, 502);
    assert_eq!(
        row.diagnostic.failure_stage.as_deref(),
        Some("upstream_request")
    );
    assert_eq!(row.upstream_model, "claude-sonnet-5-5-high");
    assert_eq!(
        row.diagnostic.payload_metadata["devin_model_route"]["upstream_model"],
        "claude-sonnet-5-5-high"
    );
    assert_request_extras(&row);
}

#[tokio::test]
async fn kiro_start_error_log_uses_kiro_wire_model_and_namespace() {
    let mut fixture = Fixture::new().await;
    fixture.provider.vendor = Some("kiro".into());
    let ingress = ANTHROPIC_MESSAGES_2023_06_01;
    for requested_model in ["claude-opus-5-5", "claude-opus-5.5"] {
        let ctx = RequestContext::new(ingress, Duration::from_secs(10));
        let performance = crate::performance::Attempt::new(
            fixture.gw.log_tx.clone(),
            ctx.cancellation.clone(),
            ctx.deadline.clone(),
        );
        performance.configure_kiro_stream();
        performance.correlate(&ctx.request_id, 1);
        performance.response(400, &reqwest::header::HeaderMap::new());
        let mut call = fixture.call_ctx(&ctx, ingress, true, requested_model);
        call.performance = Some(performance.clone());
        let mut request = AiRequest::new(requested_model, Vec::new());
        request.reasoning.effort = Some(crate::protocol::ir::ReasoningEffort::Max);
        let error = GatewayError::upstream_status("kiro", 400, None);
        super::super::log_native_start_error(&call, &request_extras(), &request, &error);
        let row = next_log(&mut fixture).await;
        assert_eq!(row.client_status_code, 400);
        assert_eq!(row.upstream_status_code, Some(400));
        assert_eq!(row.upstream_model, "claude-opus-5.5");
        assert_eq!(
            row.diagnostic.payload_metadata["kiro_model_route"],
            serde_json::json!({
                "requested_model": requested_model,
                "upstream_model": "claude-opus-5.5",
            })
        );
        assert!(
            row.diagnostic
                .payload_metadata
                .get("devin_model_route")
                .is_none()
        );
        assert_request_extras(&row);
    }
}

#[tokio::test]
async fn unknown_native_start_error_does_not_apply_devin_model_dialect() {
    let mut fixture = Fixture::new().await;
    fixture.provider.vendor = Some("future-native".into());
    let ctx = RequestContext::new(OPENAI_RESPONSES_V1, Duration::from_secs(10));
    let call = fixture.call_ctx(&ctx, OPENAI_RESPONSES_V1, false, "claude-opus-5-5");
    let mut request = AiRequest::new("claude-opus-5-5", Vec::new());
    request.reasoning.effort = Some(crate::protocol::ir::ReasoningEffort::High);
    super::super::log_native_start_error(
        &call,
        &request_extras(),
        &request,
        &invalid_ir("native start failed"),
    );
    let row = next_log(&mut fixture).await;
    assert_eq!(row.upstream_model, "claude-opus-5-5");
    assert_eq!(
        row.diagnostic.payload_metadata["native_model_route"]["upstream_model"],
        "claude-opus-5-5"
    );
    assert!(
        row.diagnostic
            .payload_metadata
            .get("devin_model_route")
            .is_none()
    );
    assert!(
        row.diagnostic
            .payload_metadata
            .get("kiro_model_route")
            .is_none()
    );
}

#[tokio::test]
async fn kiro_started_response_uses_same_model_namespace_as_start_errors() {
    let mut fixture = Fixture::new().await;
    fixture.provider.vendor = Some("kiro".into());
    for stream in [false, true] {
        let mut upstream = source(vec![Ok(vec![
            start("claude-opus-5.5"),
            AiStreamDelta::TextDelta("answer".into()),
            done(),
        ])]);
        upstream.actual_model = "claude-opus-5.5".into();
        upstream.model_metadata = serde_json::json!({
            "requested_model": "claude-opus-5-5",
            "upstream_model": "claude-opus-5.5",
        });
        let result = fixture
            .invoke(
                upstream,
                ANTHROPIC_MESSAGES_2023_06_01,
                stream,
                "claude-opus-5-5",
            )
            .await;
        assert_eq!(result.response.status(), StatusCode::OK);
        assert!(text(result.response).await.contains("answer"));
        let row = next_log(&mut fixture).await;
        assert_eq!(row.upstream_model, "claude-opus-5.5");
        assert_eq!(
            row.diagnostic.payload_metadata["kiro_model_route"]["upstream_model"],
            "claude-opus-5.5"
        );
        assert!(
            row.diagnostic
                .payload_metadata
                .get("devin_model_route")
                .is_none()
        );
    }
}

#[tokio::test]
async fn initial_failures_are_json_never_retry_and_preserve_upstream_status() {
    let mut fixture = Fixture::new().await;
    for batches in [
        vec![Ok(vec![start("fixture")]), Err(invalid_ir("wire failed"))],
        vec![Ok(vec![start("fixture")])],
        vec![Ok(vec![
            start("fixture"),
            AiStreamDelta::TextDelta("discard".into()),
            AiStreamDelta::UnexpectedEof,
        ])],
    ] {
        let result = fixture
            .invoke(
                source(batches),
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                true,
                "fixture",
            )
            .await;
        assert_eq!(result.retry, RetryDisposition::Never);
        assert_eq!(result.health, HealthDisposition::Failure);
        assert_eq!(result.response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            result.response.headers()["x-nyro-provider-limitations"],
            LIMITATIONS
        );
        assert!(
            result.response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let body: serde_json::Value = serde_json::from_str(&text(result.response).await).unwrap();
        assert_eq!(body["error"]["type"], "NYRO_UPSTREAM_ERROR");
        assert!(body["error"]["request_id"].is_string());
        let row = fixture.logs.recv().await.unwrap();
        assert_eq!(row.upstream_status_code, Some(200));
        assert_eq!(row.client_status_code, 502);
    }
}

#[tokio::test]
async fn native_ir_formats_stream_and_buffered_for_all_ingress_protocols() {
    let fixture = Fixture::new().await;
    for ingress in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPENAI_RESPONSES_V1,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        for stream in [false, true] {
            let result = fixture
                .invoke(
                    source(vec![Ok(vec![
                        start("fixture"),
                        AiStreamDelta::TextDelta("native-hello".into()),
                        done(),
                    ])]),
                    ingress,
                    stream,
                    "fixture",
                )
                .await;
            assert_eq!(result.response.status(), StatusCode::OK);
            assert_eq!(result.retry, RetryDisposition::Never);
            assert_eq!(
                result.health,
                if stream {
                    HealthDisposition::Deferred
                } else {
                    HealthDisposition::Success
                }
            );
            let body = text(result.response).await;
            assert!(body.contains("native-hello"), "{ingress}: {body}");
            if stream {
                let mut parser = ingress.handler().make_stream_response_decoder();
                let mut deltas = parser.parse_chunk(&body).unwrap();
                deltas.extend(parser.finish().unwrap());
                assert!(
                    deltas
                        .iter()
                        .any(|d| matches!(d, AiStreamDelta::Done { .. })),
                    "{ingress}: {body}"
                );
                assert!(
                    !deltas.iter().any(|d| matches!(
                        d,
                        AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
                    )),
                    "{ingress}: {body}"
                );
            } else {
                let response = ingress
                    .handler()
                    .make_response_decoder()
                    .parse_response(serde_json::from_str(&body).unwrap())
                    .unwrap();
                assert_eq!(response.content, "native-hello");
            }
        }
    }
}

#[tokio::test]
async fn native_response_advertises_devin_limitations() {
    let fixture = Fixture::new().await;
    for stream in [false, true] {
        let result = fixture
            .invoke(
                source(vec![Ok(vec![
                    start("fixture"),
                    AiStreamDelta::TextDelta("hello".into()),
                    done(),
                ])]),
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                stream,
                "fixture",
            )
            .await;
        assert_eq!(
            result.response.headers()["x-nyro-provider-limitations"],
            "system_best_effort,inline_images_only,tools_buffered"
        );
        assert!(text(result.response).await.contains("hello"));
    }
}

#[tokio::test]
async fn midstream_failure_and_missing_done_cannot_emit_success_terminal() {
    let mut fixture = Fixture::new().await;
    for trailing in [
        vec![],
        vec![Err(invalid_ir("wire error"))],
        vec![Ok(vec![done()]), Err(invalid_ir("error after Done"))],
    ] {
        let mut batches = vec![Ok(vec![
            start("fixture"),
            AiStreamDelta::TextDelta("partial".into()),
        ])];
        batches.extend(trailing);
        let result = fixture
            .invoke(
                source(batches),
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                true,
                "fixture",
            )
            .await;
        assert_eq!(result.response.status(), StatusCode::OK);
        let body = text(result.response).await;
        assert!(body.contains("partial"));
        assert!(body.contains("\"error\""), "{body}");
        assert!(!body.contains("[DONE]"), "must not append success: {body}");
        assert!(
            !body.contains("\"finish_reason\":\"stop\""),
            "must hold Done until source EOF: {body}"
        );
        let row = fixture.logs.recv().await.unwrap();
        assert_eq!(row.diagnostic.attempt_outcome, "failed");
        assert_eq!(row.upstream_status_code, Some(200));
    }
}

#[tokio::test]
async fn buffered_hooks_run_once_and_precommit_replay_does_not_duplicate_stream_hook() {
    let fixture = Fixture::new().await;
    let result = fixture
        .invoke(
            source(vec![Ok(vec![
                start(BUFFERED_HOOK_MODEL),
                AiStreamDelta::TextDelta("answer".into()),
                done(),
            ])]),
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false,
            BUFFERED_HOOK_MODEL,
        )
        .await;
    let body: serde_json::Value = serde_json::from_str(&text(result.response).await).unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "answer[legacy][phase]"
    );
    let result = fixture
        .invoke(
            source(vec![
                Ok(vec![start(STREAM_HOOK_MODEL)]),
                Ok(vec![AiStreamDelta::TextDelta("answer".into())]),
                Ok(vec![done()]),
            ]),
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
            STREAM_HOOK_MODEL,
        )
        .await;
    let body = text(result.response).await;
    assert_eq!(body.matches("[phase]").count(), 1, "{body}");
    assert!(
        !body.contains("[legacy]"),
        "legacy buffered hook must not run per delta"
    );
}

#[tokio::test]
async fn dropping_stream_body_cancels_guard_and_aborts_parked_worker() {
    let fixture = Fixture::new().await;
    let child = CancellationToken::new();
    let (tx, rx) = mpsc::channel(2);
    tx.send(Ok(vec![
        start("fixture"),
        AiStreamDelta::TextDelta("first".into()),
    ]))
    .await
    .unwrap();
    let worker = tokio::spawn(async move {
        let _tx = tx;
        std::future::pending::<()>().await;
    });
    let upstream = IrUpstream {
        status: 200,
        headers: Default::default(),
        url: "https://server.codeium.com/Run".into(),
        actual_model: "actual-native-model".into(),
        model_metadata: serde_json::json!({}),
        events: Box::pin(ReceiverStream::new(rx)),
        guard: SessionGuard::new(child.clone(), vec![worker.abort_handle()]),
    };
    let result = fixture
        .invoke(
            upstream,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            true,
            "fixture",
        )
        .await;
    assert!(
        !child.is_cancelled(),
        "guard must survive returning HTTP headers"
    );
    drop(result.response);
    tokio::time::timeout(Duration::from_secs(2), child.cancelled())
        .await
        .expect("body drop cancels native worker");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
}

#[tokio::test]
async fn precommit_cap_commits_without_polling_the_parked_tail() {
    let mut upstream = source(vec![Ok(vec![
        AiStreamDelta::Usage(Default::default());
        MAX_PRECOMMIT_DELTAS
    ])]);
    upstream.events = Box::pin(upstream.events.chain(futures::stream::pending()));
    let mut progress = Progress::default();
    let buffered = tokio::time::timeout(
        Duration::from_secs(1),
        precommit(&mut upstream.events, &mut progress, None, Instant::now()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(buffered.len(), MAX_PRECOMMIT_DELTAS);
    assert!(!progress.terminal.has_done());
}
