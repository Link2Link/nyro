//! Pre-commit hold on the byte-level Responses passthrough path, exercised
//! through the real dispatcher and loopback HTTP.
//!
//! Incident replay (request dc053439): the upstream answers HTTP 200 and
//! streams only framing (`response.created`, `response.in_progress`,
//! keep-alives, `response.metadata`), then emits `event: error` /
//! `server_error`. Before the hold, the relay committed the 200 on the first
//! byte, so the dispatcher could not fail over. Now a retryable error that
//! arrives before any payload fails the attempt and the ordered route moves on
//! to the next target. Errors after payload, and deterministic request errors,
//! still reach the client verbatim.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, extract::State, http::HeaderMap, http::StatusCode, routing::post};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateModelBackend, CreateProvider},
    logging::LogEntry,
    protocol::{ids::OPENAI_RESPONSES_V1, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};

const WATCHDOG: Duration = Duration::from_secs(15);
const UPSTREAM_MODEL: &str = "responses-fixture";

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

/// The incident prefix: lifecycle events and keep-alives, no model output.
fn framing() -> String {
    [
        event(json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp_flaky","status":"in_progress","output":[]}})),
        event(json!({"type":"response.in_progress","sequence_number":1})),
        ": keep-alive\n\n".repeat(3),
        event(json!({"type":"keepalive","sequence_number":2})),
        event(json!({"type":"response.metadata","sequence_number":3,
            "response_id":"resp_flaky","metadata":{}})),
    ]
    .concat()
}

fn server_error() -> String {
    event(json!({"type":"error","sequence_number":4,"error":{
        "message":"An error occurred while processing your request. Please include the request ID ea2be9d4 in your message.",
        "type":"server_error","param":null,"code":"server_error"}}))
}

fn text_delta() -> String {
    event(json!({"type":"response.output_text.delta","output_index":0,
        "content_index":0,"item_id":"msg_ok","delta":"hello"}))
}

fn healthy() -> String {
    [
        event(json!({"type":"response.created","response":{"id":"resp_ok","status":"in_progress","output":[]}})),
        event(json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"msg_ok","type":"message","role":"assistant","content":[],"status":"in_progress"}})),
        text_delta(),
        event(json!({"type":"response.completed","response":{"id":"resp_ok","object":"response",
            "model":UPSTREAM_MODEL,"status":"completed","output":[{
                "id":"msg_ok","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"hello","annotations":[]}]}],
            "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}}})),
    ]
    .concat()
}

#[derive(Clone)]
struct UpstreamState {
    wire: Arc<String>,
    calls: Arc<AtomicUsize>,
}

struct Upstream {
    url: String,
    calls: Arc<AtomicUsize>,
    job: tokio::task::JoinHandle<()>,
}

impl Upstream {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.job.abort();
    }
}

async fn scripted(State(state): State<UpstreamState>) -> impl axum::response::IntoResponse {
    state.calls.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        state.wire.as_ref().clone(),
    )
}

async fn start_upstream(wire: String) -> anyhow::Result<Upstream> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let calls = Arc::new(AtomicUsize::new(0));
    let state = UpstreamState {
        wire: Arc::new(wire),
        calls: calls.clone(),
    };
    let job = tokio::spawn(async move {
        let app = Router::new()
            .route("/*path", post(scripted))
            .with_state(state);
        let _ = axum::serve(listener, app).await;
    });
    Ok(Upstream { url, calls, job })
}

async fn gateway() -> anyhow::Result<(
    tempfile::TempDir,
    Gateway,
    tokio::sync::mpsc::Receiver<LogEntry>,
)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, logs) = Gateway::from_storage(config, storage).await?;
    Ok((dir, gw, logs))
}

/// Native OpenAI Responses provider: ingress == egress and no response
/// mutations, so the dispatcher takes the byte-level passthrough relay.
async fn provider(gw: &Gateway, name: &str, url: &str) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            name: name.into(),
            vendor: Some("openai".into()),
            protocol: "openai-responses".into(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: Some("sub2api".into()),
            models_source: None,
            static_models: None,
            api_key: "fixture-local-secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}

async fn route(
    gw: &Gateway,
    name: &str,
    primary: &str,
    fallback: Option<&str>,
) -> anyhow::Result<()> {
    let mut targets = vec![CreateModelBackend {
        provider_id: primary.into(),
        model: UPSTREAM_MODEL.into(),
        weight: Some(100),
        priority: Some(1),
        is_fallback: Some(false),
    }];
    if let Some(fallback) = fallback {
        targets.push(CreateModelBackend {
            provider_id: fallback.into(),
            model: UPSTREAM_MODEL.into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(false),
        });
    }
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: primary.into(),
            target_model: UPSTREAM_MODEL.into(),
            targets,
            enable_auth: Some(false),
            enable_payload: Some(true),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}

async fn dispatch(gw: &Gateway, model: &str) -> anyhow::Result<(String, StatusCode, String)> {
    let body = json!({
        "model": model,
        "stream": true,
        "input": [{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"hi"}]}]
    });
    let request = OPENAI_RESPONSES_V1
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let ctx = RequestContext::new(OPENAI_RESPONSES_V1, WATCHDOG);
    let id = ctx.request_id.clone();
    let (status, wire) = tokio::time::timeout(WATCHDOG, async {
        let response = dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            RawEnvelope::new(Some(body), Default::default(), "POST", "/v1/responses"),
            request,
            OPENAI_RESPONSES_V1,
            ctx,
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
        Ok::<_, anyhow::Error>((status, String::from_utf8(bytes.to_vec())?))
    })
    .await??;
    Ok((id, status, wire))
}

async fn recv_logs(
    logs: &mut tokio::sync::mpsc::Receiver<LogEntry>,
    count: usize,
) -> anyhow::Result<Vec<LogEntry>> {
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.push(
            tokio::time::timeout(Duration::from_secs(5), logs.recv())
                .await?
                .ok_or_else(|| anyhow::anyhow!("log channel closed"))?,
        );
    }
    rows.sort_by_key(|row| row.diagnostic.attempt_index);
    Ok(rows)
}

// ── 1. The incident: framing + server_error fails over ───────────────────────

#[tokio::test]
async fn server_error_before_any_output_fails_over_to_the_next_target() -> anyhow::Result<()> {
    let flaky = start_upstream(framing() + &server_error()).await?;
    let good = start_upstream(healthy()).await?;
    let (_dir, gw, mut logs) = gateway().await?;
    let a = provider(&gw, "flaky", &flaky.url).await?;
    let b = provider(&gw, "good", &good.url).await?;
    route(&gw, "incident", &a, Some(&b)).await?;

    let (id, status, wire) = dispatch(&gw, "incident").await?;

    assert_eq!(status, StatusCode::OK, "{wire}");
    assert_eq!(flaky.calls(), 1, "the flaky target is tried exactly once");
    assert_eq!(good.calls(), 1, "the healthy target takes over");
    assert_eq!(wire, healthy(), "the client sees only the healthy stream");
    assert!(!wire.contains("server_error"), "{wire}");
    assert!(!wire.contains("resp_flaky"), "{wire}");

    let rows = recv_logs(&mut logs, 2).await?;
    let (failed, served) = (&rows[0], &rows[1]);
    assert_eq!(failed.diagnostic.attempt_index, Some(1));
    assert_eq!(
        failed.diagnostic.client_request_id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(failed.diagnostic.attempt_outcome, "failed");
    assert_eq!(
        failed.diagnostic.failure_kind.as_deref(),
        Some("upstream_error")
    );
    assert_eq!(
        failed.diagnostic.failure_stage.as_deref(),
        Some("upstream_response")
    );
    assert_eq!(failed.upstream_status_code, Some(200));
    assert_eq!(failed.client_status_code, 502);
    assert!(
        failed
            .upstream_response_body
            .as_deref()
            .is_some_and(|body| body.contains("server_error") && body.contains("resp_flaky")),
        "the failed row keeps the upstream evidence"
    );
    assert_eq!(served.diagnostic.attempt_index, Some(2));
    assert_eq!(served.client_status_code, 200);
    let result = served
        .diagnostic
        .final_result
        .as_ref()
        .expect("final result");
    assert_eq!(result.attempt_count, 2);
    Ok(())
}

// ── 2. Nowhere left to go: an explicit 502, not a broken 200 ─────────────────

#[tokio::test]
async fn single_target_failure_surfaces_a_502_instead_of_a_committed_200() -> anyhow::Result<()> {
    let flaky = start_upstream(framing() + &server_error()).await?;
    let (_dir, gw, _logs) = gateway().await?;
    let a = provider(&gw, "flaky", &flaky.url).await?;
    route(&gw, "alone", &a, None).await?;

    let (_, status, wire) = dispatch(&gw, "alone").await?;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{wire}");
    assert_eq!(flaky.calls(), 1);
    assert!(
        !wire.contains("response.created"),
        "no stream was committed: {wire}"
    );
    Ok(())
}

// ── 3. Deterministic request errors are not replayed elsewhere ───────────────

#[tokio::test]
async fn non_retryable_error_reaches_the_client_without_failover() -> anyhow::Result<()> {
    let wire = framing()
        + &event(json!({"type":"error","error":{
            "code":"context_length_exceeded","message":"input is too long"}}));
    let strict = start_upstream(wire.clone()).await?;
    let spare = start_upstream(healthy()).await?;
    let (_dir, gw, _logs) = gateway().await?;
    let a = provider(&gw, "strict", &strict.url).await?;
    let b = provider(&gw, "spare", &spare.url).await?;
    route(&gw, "too-long", &a, Some(&b)).await?;

    let (_, status, got) = dispatch(&gw, "too-long").await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, wire, "bytes are forwarded verbatim");
    assert_eq!(
        spare.calls(),
        0,
        "replaying a request-shape error is pointless"
    );
    Ok(())
}

// ── 4. Once output was sent the stream is committed ──────────────────────────

#[tokio::test]
async fn error_after_output_is_forwarded_and_never_replayed() -> anyhow::Result<()> {
    let wire = framing() + &text_delta() + &server_error();
    let partial = start_upstream(wire.clone()).await?;
    let spare = start_upstream(healthy()).await?;
    let (_dir, gw, _logs) = gateway().await?;
    let a = provider(&gw, "partial", &partial.url).await?;
    let b = provider(&gw, "spare", &spare.url).await?;
    route(&gw, "mid-stream", &a, Some(&b)).await?;

    let (_, status, got) = dispatch(&gw, "mid-stream").await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        got, wire,
        "committed bytes, including the error, are verbatim"
    );
    assert_eq!(
        spare.calls(),
        0,
        "splicing a second answer mid-stream is not safe"
    );
    Ok(())
}

// ── 5. The hold is invisible on a healthy stream ─────────────────────────────

#[tokio::test]
async fn framing_then_payload_is_forwarded_byte_for_byte() -> anyhow::Result<()> {
    let wire = framing() + &healthy();
    let upstream = start_upstream(wire.clone()).await?;
    let (_dir, gw, mut logs) = gateway().await?;
    let a = provider(&gw, "steady", &upstream.url).await?;
    route(&gw, "steady", &a, None).await?;

    let (_, status, got) = dispatch(&gw, "steady").await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        got, wire,
        "held-then-replayed bytes match the upstream exactly"
    );
    assert_eq!(upstream.calls(), 1);
    let rows = recv_logs(&mut logs, 1).await?;
    assert_eq!(rows[0].client_status_code, 200);
    assert!(
        rows[0].stream_chunks_count >= 1,
        "chunk counters cover replayed bytes: {}",
        rows[0].stream_chunks_count
    );
    Ok(())
}
