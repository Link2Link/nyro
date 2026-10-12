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
            "id":"native-fixture", "name":"native-fixture", "vendor":"cursor",
            "protocol":"cursor-agent", "base_url":"https://api.cursor.sh",
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
        let call = CallCtx {
            performance: None,
            gw: self.gw.clone(),
            provider: &self.provider,
            model_id: "native-fixture",
            model_name: "native-fixture",
            egress: ingress,
            ingress,
            ingress_str: "fixture",
            egress_str: "cursor-agent",
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
        };
        let mut request = AiRequest::new(model, Vec::new());
        request.stream.enabled = stream;
        handle_ir_response(
            upstream,
            &call,
            ToolRoutePlan::default(),
            &mut ctx,
            &mut request,
            &HostContext::new(&self.gw),
        )
        .await
    }
}

fn source(batches: Vec<Result<Vec<AiStreamDelta>, GatewayError>>) -> IrUpstream {
    IrUpstream {
        status: 200,
        headers: Default::default(),
        url: "https://api.cursor.sh/agent.v1.AgentService/Run".into(),
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
        url: "https://api.cursor.sh/Run".into(),
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
