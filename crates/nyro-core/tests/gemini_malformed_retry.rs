//! Gemini MALFORMED_FUNCTION_CALL zero-payload terminal: pre-commit probe
//! and per-target failover through the real dispatcher and local HTTP.
//!
//! Production incident replay (plain Gemini and v1internal envelope SSE,
//! including CRLF framing and unterminated events at EOF): the upstream answers
//! HTTP 200 with a single SSE event whose only candidate carries
//! `finishReason: "MALFORMED_FUNCTION_CALL"` and no text/tool-call output.
//! The gateway may retry that zero-output terminal once on the same prepared
//! target (at most two attempts per provider/model), then resume ordinary
//! failover through the model's ordered candidates. Each attempt has its own
//! performance/log identity. Exhaustion surfaces an explicit 502 preserving
//! upstream 200, finishReason/finishMessage and usage, without retrying ordinary
//! HTTP errors, committed output, cancellation or an exceeded deadline.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{
        CreateModel, CreateModelBackend, CreateProvider, LogQuery, UpsertOAuthCredential,
    },
    logging::{LogEntry, logging_status, run_collector},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{
        context::{CancellationToken, RequestContext},
        dispatcher::dispatch_pipeline,
    },
    storage::SqliteStorage,
};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The production incident payload (de-enveloped pure Gemini SSE); the
/// thought signature is a placeholder, the shape is the incident's.
const MALFORMED_SSE: &str = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"test-signature","text":""}]},"finishReason":"MALFORMED_FUNCTION_CALL","finishMessage":"Malformed function call: Failed to parse function call: Function call is empty - no input to parse."}],"usageMetadata":{"promptTokenCount":65036,"totalTokenCount":65322,"cachedContentTokenCount":60835,"thoughtsTokenCount":286},"modelVersion":"gemini-3.8-flash","responseId":"malformed-1"}

"#;

const HEALTHY_SSE: &str = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"recovered answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},"modelVersion":"gemini-3.8-flash","responseId":"ok-recovered"}

"#;

const SAFETY_SSE: &str = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"SAFETY","finishMessage":"Blocked by safety"}],"usageMetadata":{"promptTokenCount":9,"totalTokenCount":9},"modelVersion":"gemini-3.8-flash","responseId":"safety-1"}

"#;

const TEXT_THEN_MALFORMED_SSE: &str = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"partial output"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}],"usageMetadata":{"promptTokenCount":5,"totalTokenCount":7},"modelVersion":"gemini-3.8-flash","responseId":"partial-1"}

"#;

/// Wrap one plain Gemini SSE fixture into the Code Assist v1internal
/// `{"response": …}` envelope the google/antigravity channel actually serves
/// (outer trace metadata included; the vendor raw-chunk hook unwraps every
/// data line before the Gemini stream decoder parses it). Built
/// programmatically from the plain fixture so the long hand-written JSON
/// cannot drift into an unbalanced shape.
fn enveloped(plain_sse: &str) -> String {
    let data = plain_sse
        .trim_start()
        .strip_prefix("data: ")
        .expect("plain fixture must be a single data line")
        .trim();
    let inner: Value = serde_json::from_str(data).expect("plain fixture must be valid JSON");
    format!(
        "data: {}\n\n",
        json!({"response": inner, "traceId": "fixture-trace"})
    )
}

/// Scripted reply: served in order, extra calls repeat the last entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reply {
    Malformed,
    Healthy,
    Safety,
    TextThenMalformed,
    EnvelopedMalformed,
    EnvelopedHealthy,
    ThinkingThenMalformed,
    ToolThenMalformed,
    EnvelopedTextThenMalformed,
    EnvelopedThinkingThenMalformed,
    EnvelopedToolThenMalformed,
    Http502,
}

impl Reply {
    fn sse(self) -> String {
        match self {
            Reply::Malformed => MALFORMED_SSE.to_string(),
            Reply::Healthy => HEALTHY_SSE.to_string(),
            Reply::Safety => SAFETY_SSE.to_string(),
            Reply::TextThenMalformed => TEXT_THEN_MALFORMED_SSE.to_string(),
            Reply::EnvelopedMalformed => enveloped(MALFORMED_SSE),
            Reply::EnvelopedHealthy => enveloped(HEALTHY_SSE),
            Reply::ThinkingThenMalformed => payload_then_malformed(json!({
                "thought": true, "text": "partial thinking"
            })),
            Reply::ToolThenMalformed => payload_then_malformed(json!({
                "functionCall": {"name": "lookup_weather", "args": {"city": "Paris"}}
            })),
            Reply::EnvelopedTextThenMalformed => enveloped(TEXT_THEN_MALFORMED_SSE),
            Reply::EnvelopedThinkingThenMalformed => enveloped(&Reply::ThinkingThenMalformed.sse()),
            Reply::EnvelopedToolThenMalformed => enveloped(&Reply::ToolThenMalformed.sse()),
            Reply::Http502 => unreachable!("HTTP errors are not SSE fixtures"),
        }
    }
}

fn payload_then_malformed(part: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": [part]},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 7},
            "modelVersion": "gemini-3.8-flash",
            "responseId": "partial-1"
        })
    )
}

/// Every fixture's data payload must be a single valid JSON document — a
/// hand-written long string that accidentally closes its outer object early
/// (trailing top-level fields) turns the case under test into a
/// conversion_parse_error instead.
#[test]
fn sse_fixtures_are_valid_single_json_documents() {
    for plain in [
        MALFORMED_SSE,
        HEALTHY_SSE,
        SAFETY_SSE,
        TEXT_THEN_MALFORMED_SSE,
    ] {
        let data = plain
            .trim_start()
            .strip_prefix("data: ")
            .expect("data line")
            .trim();
        serde_json::from_str::<Value>(data).expect("plain fixture must parse");
    }
    for wrapped in [enveloped(MALFORMED_SSE), enveloped(HEALTHY_SSE)] {
        let data = wrapped
            .trim_start()
            .strip_prefix("data: ")
            .expect("data line")
            .trim();
        let value: Value = serde_json::from_str(data).expect("enveloped fixture must parse");
        assert!(
            value.get("response").is_some_and(Value::is_object),
            "envelope must carry the response object: {value}"
        );
    }
}

/// An EOF fixture has no blank event separator. Keep a single line ending
/// to exercise LF/CRLF/CR independently; BareEof also omits that line ending.
#[derive(Clone, Copy, Debug)]
enum Framing {
    Lf,
    Crlf,
    Cr,
    EofLf,
    EofCrlf,
    EofCr,
    BareEof,
}

impl Framing {
    fn apply(self, sse: String) -> String {
        let ending = match self {
            Self::Lf => "\n\n",
            Self::Crlf => "\r\n\r\n",
            Self::Cr => "\r\r",
            Self::EofLf => "\n",
            Self::EofCrlf => "\r\n",
            Self::EofCr => "\r",
            Self::BareEof => "",
        };
        format!("{}{ending}", sse.trim_end_matches(['\r', '\n']))
    }
}

const REGRESSION_FRAMINGS: [Framing; 6] = [
    Framing::Crlf,
    Framing::Cr,
    Framing::EofLf,
    Framing::EofCrlf,
    Framing::EofCr,
    Framing::BareEof,
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedRequest {
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone)]
struct UpstreamState {
    script: Arc<Vec<Reply>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    framing: Framing,
    cancel_on_reply: Option<CancellationToken>,
}

async fn scripted_upstream(
    State(state): State<UpstreamState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state
        .requests
        .lock()
        .unwrap()
        .push(CapturedRequest { uri, headers, body });
    let index = state
        .calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let reply = state
        .script
        .get(index.min(state.script.len().saturating_sub(1)))
        .copied()
        .unwrap_or(Reply::Healthy);
    if let Some(token) = &state.cancel_on_reply {
        token.cancel();
    }
    if reply == Reply::Http502 {
        // Even a body naming the finish reason cannot forge the local marker.
        return (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": {"message": "MALFORMED_FUNCTION_CALL: ordinary HTTP 502"}})),
        )
            .into_response();
    }
    (
        [("content-type", "text/event-stream")],
        state.framing.apply(reply.sse()),
    )
        .into_response()
}

struct Upstream {
    url: String,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    job: tokio::task::JoinHandle<()>,
}

impl Upstream {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.job.abort();
    }
}

async fn start_upstream(script: Vec<Reply>) -> anyhow::Result<Upstream> {
    start_upstream_with_framing(script, Framing::Lf).await
}

async fn start_upstream_with_framing(
    script: Vec<Reply>,
    framing: Framing,
) -> anyhow::Result<Upstream> {
    start_upstream_controlled(script, framing, None).await
}

async fn start_upstream_controlled(
    script: Vec<Reply>,
    framing: Framing,
    cancel_on_reply: Option<CancellationToken>,
) -> anyhow::Result<Upstream> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = UpstreamState {
        script: Arc::new(script),
        calls: calls.clone(),
        requests: requests.clone(),
        framing,
        cancel_on_reply,
    };
    let job = tokio::spawn(async move {
        let app = Router::new()
            .route("/*path", post(scripted_upstream))
            .with_state(state);
        let _ = axum::serve(listener, app).await;
    });
    Ok(Upstream {
        url,
        calls,
        requests,
        job,
    })
}

async fn setup() -> anyhow::Result<(
    tempfile::TempDir,
    Gateway,
    tokio::sync::mpsc::Receiver<nyro_core::logging::LogEntry>,
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

async fn gemini_provider(gw: &Gateway, url: &str) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: uuid::Uuid::new_v4().to_string(),
            vendor: Some("google".into()),
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "local-fake-token".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}

/// google/antigravity OAuth channel backed by a fixture credential.
///
/// The credential's driver key is deliberately unknown to the auth registry:
/// `build_driver` then yields no driver, so `resolve_provider_runtime` keeps
/// the default runtime binding and the provider's local scripted base URL
/// stays the egress target (the real google driver's `bind_runtime` would
/// override it with Google's daily Cloud Code host). `meta.project_id` still
/// satisfies the v1internal request-envelope wrap in `post_encode`.
async fn antigravity_provider(gw: &Gateway, url: &str) -> anyhow::Result<String> {
    let id = gw
        .admin()
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: uuid::Uuid::new_v4().to_string(),
            vendor: Some("google".into()),
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: Some("antigravity".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id;
    gw.storage
        .oauth_credentials()
        .upsert(
            &id,
            UpsertOAuthCredential {
                driver_key: "google-fixture".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "fixture-access-token".into(),
                refresh_token: Some("fixture-refresh-token".into()),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                resource_url: None,
                subject_id: Some("fixture-user".into()),
                scopes: Some("[]".into()),
                meta: Some(r#"{"project_id":"fixture-project"}"#.into()),
            },
        )
        .await?;
    Ok(id)
}

async fn route(
    gw: &Gateway,
    name: &str,
    provider: String,
    targets: Vec<CreateModelBackend>,
) -> anyhow::Result<()> {
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: provider,
            target_model: "gemini-3.8-flash".into(),
            targets,
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}

/// Simple tools keep the chat→Gemini conversion on the native IR path
/// (mirrors tests/gemini_route_regressions.rs).
fn chat_stream_request(model: &str) -> Value {
    json!({
        "model": model,
        "stream": true,
        "messages": [{"role": "user", "content": "Check the weather in Paris."}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "lookup_weather",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        }]
    })
}

/// Non-stream chat request: on the antigravity channel this forces the
/// upstream call into streaming mode and aggregates (non_stream_via_upstream_stream).
fn chat_non_stream_request(model: &str) -> Value {
    let mut request = chat_stream_request(model);
    request["stream"] = json!(false);
    request
}

async fn dispatch_request(
    gw: &Gateway,
    body: Value,
) -> anyhow::Result<(String, StatusCode, String)> {
    let ctx = RequestContext::new(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, TIMEOUT);
    dispatch_with_context(gw, body, ctx).await
}

async fn dispatch_with_context(
    gw: &Gateway,
    body: Value,
    ctx: RequestContext,
) -> anyhow::Result<(String, StatusCode, String)> {
    let request = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let request_id = ctx.request_id.clone();
    let response = tokio::time::timeout(
        TIMEOUT,
        dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            RawEnvelope::new(
                Some(body),
                Default::default(),
                "POST",
                "/v1/chat/completions",
            ),
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ctx,
        ),
    )
    .await?;
    let status = response.status();
    let bytes = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024),
    )
    .await??;
    Ok((request_id, status, String::from_utf8(bytes.to_vec())?))
}

async fn dispatch_stream(
    gw: &Gateway,
    model: &str,
) -> anyhow::Result<(String, StatusCode, String)> {
    dispatch_request(gw, chat_stream_request(model)).await
}

async fn recv_logs(
    logs: &mut tokio::sync::mpsc::Receiver<nyro_core::logging::LogEntry>,
    count: usize,
) -> Vec<nyro_core::logging::LogEntry> {
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.push(
            tokio::time::timeout(Duration::from_secs(5), logs.recv())
                .await
                .expect("log row must arrive")
                .expect("log channel must stay open"),
        );
    }
    rows
}

/// Distinct performance attempts share only the client request identity;
/// only the last row owns the final result and its total attempt count.
fn assert_attempt_sequence(rows: &mut [LogEntry], request_id: &str) {
    rows.sort_by_key(|row| row.diagnostic.attempt_index);
    let mut ids = std::collections::HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.diagnostic.attempt_index, Some(index as i32 + 1));
        assert_eq!(
            row.diagnostic.client_request_id.as_deref(),
            Some(request_id)
        );
        assert!(
            ids.insert(&row.diagnostic.log_id),
            "attempt log IDs must differ"
        );
        if index + 1 != rows.len() {
            assert!(
                row.diagnostic.final_result.is_none(),
                "intermediate row: {row:?}"
            );
        }
    }
    let last = rows.last().expect("at least one attempt");
    let result = last.diagnostic.final_result.as_ref().expect("final result");
    assert_eq!(result.attempt_count, rows.len() as i32);
    assert_eq!(result.client_request_id, request_id);
    assert_eq!(
        result.final_attempt_id.as_deref(),
        Some(last.diagnostic.log_id.as_str())
    );
    assert_eq!(result.final_outcome, last.diagnostic.attempt_outcome);
}

fn assert_malformed_attempt(row: &LogEntry) {
    assert_eq!(row.client_status_code, 502);
    assert_eq!(row.upstream_status_code, Some(200), "upstream was a 200");
    assert_eq!(row.diagnostic.attempt_outcome, "failed");
    assert_eq!(row.performance.completion, "failed");
    assert_eq!(
        row.diagnostic.failure_kind.as_deref(),
        Some("upstream_error")
    );
    let causes = row.diagnostic.error_causes.join(" | ");
    assert!(
        causes.contains("finish_reason=malformed_function_call")
            && causes.contains("Function call is empty"),
        "upstream failure evidence: {causes}"
    );
    assert_eq!(row.usage.prompt_tokens, 65036, "usage preserved: {row:?}");
    assert_eq!(row.usage.total_tokens, 65322);
}

fn assert_no_failed_response_leak(wire: &str) {
    for marker in [
        "MALFORMED_FUNCTION_CALL",
        "malformed_function_call",
        "test-signature",
        "malformed-1",
        "Function call is empty",
        "65036",
    ] {
        assert!(
            !wire.contains(marker),
            "failed response leaked ({marker}): {wire}"
        );
    }
}

fn fallback_targets(primary: &str, fallback: &str) -> Vec<CreateModelBackend> {
    vec![
        CreateModelBackend {
            provider_id: primary.into(),
            model: "gemini-3.8-flash".into(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: Some(false),
        },
        CreateModelBackend {
            provider_id: fallback.into(),
            model: "gemini-3.5-pro".into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(true),
        },
    ]
}

// ── 1. Single backend: at most two attempts, explicit 502 ────────────────────

#[tokio::test]
async fn malformed_single_backend_stops_after_two_attempts_with_502() -> anyhow::Result<()> {
    assert_malformed_single_backend(Reply::Malformed, Framing::Lf).await
}

#[tokio::test]
async fn malformed_crlf_and_eof_single_backend_returns_502_not_empty_200() -> anyhow::Result<()> {
    for reply in [Reply::Malformed, Reply::EnvelopedMalformed] {
        for framing in REGRESSION_FRAMINGS {
            assert_malformed_single_backend(reply, framing).await?;
        }
    }
    Ok(())
}

async fn assert_malformed_single_backend(reply: Reply, framing: Framing) -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    // The healthy third reply proves the same-target budget is exactly two.
    let healthy = if reply == Reply::EnvelopedMalformed {
        Reply::EnvelopedHealthy
    } else {
        Reply::Healthy
    };
    let upstream = start_upstream_with_framing(vec![reply, reply, healthy], framing).await?;
    let provider = if reply == Reply::EnvelopedMalformed {
        antigravity_provider(&gw, &upstream.url).await?
    } else {
        gemini_provider(&gw, &upstream.url).await?
    };
    route(&gw, "malformed-single", provider, vec![]).await?;

    let (id, status, wire) = dispatch_stream(&gw, "malformed-single").await?;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "{reply:?}/{framing:?}: single backend must surface the explicit 502: {wire}"
    );
    let value: Value = serde_json::from_str(&wire)?;
    let message = value["error"]["message"].as_str().expect("error message");
    assert!(message.contains("finish_reason=malformed_function_call"));
    assert!(message.contains("Function call is empty"));
    assert_eq!(
        upstream.calls(),
        2,
        "one same-target retry only; the healthy third reply stays unfetched"
    );
    assert!(
        !wire.contains("recovered answer"),
        "third reply was fetched: {wire}"
    );

    let mut rows = recv_logs(&mut logs, 2).await;
    assert_attempt_sequence(&mut rows, &id);
    for row in &rows {
        assert_malformed_attempt(row);
    }
    assert!(logs.try_recv().is_err(), "no extra rows");
    Ok(())
}

// ── 2. All candidates exhausted: explicit 502 with details ───────────────────

#[tokio::test]
async fn malformed_all_backends_exhausted_returns_502_with_details() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let broken_a = start_upstream(vec![Reply::Malformed]).await?;
    let broken_b = start_upstream(vec![Reply::Malformed]).await?;
    let provider_a = gemini_provider(&gw, &broken_a.url).await?;
    let provider_b = gemini_provider(&gw, &broken_b.url).await?;
    let targets = vec![
        CreateModelBackend {
            provider_id: provider_a.clone(),
            model: "gemini-3.8-flash".into(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: Some(false),
        },
        CreateModelBackend {
            provider_id: provider_b,
            model: "gemini-3.8-flash".into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(false),
        },
    ];
    route(&gw, "malformed-exhaust", provider_a, targets).await?;

    let (id, status, wire) = dispatch_stream(&gw, "malformed-exhaust").await?;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "client must see 502: {wire}"
    );
    let value: Value = serde_json::from_str(&wire)?;
    let message = value["error"]["message"].as_str().expect("error message");
    assert!(message.contains("finish_reason=malformed_function_call"));
    assert!(message.contains("Function call is empty"));
    assert_eq!(broken_a.calls(), 2, "backend A gets one retry");
    assert_eq!(broken_b.calls(), 2, "backend B gets its own retry");

    let mut rows = recv_logs(&mut logs, 4).await;
    assert_attempt_sequence(&mut rows, &id);
    for row in &rows {
        assert_malformed_attempt(row);
    }
    assert!(logs.try_recv().is_err(), "no extra rows");
    Ok(())
}

// ── 3. Fail over to the next target (fallback row, different model) ─────────

#[tokio::test]
async fn malformed_backend_fails_over_to_next_target() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let broken = start_upstream(vec![Reply::Malformed, Reply::Malformed, Reply::Healthy]).await?;
    let healthy = start_upstream(vec![Reply::Healthy]).await?;
    let provider_a = gemini_provider(&gw, &broken.url).await?;
    let provider_b = gemini_provider(&gw, &healthy.url).await?;
    let targets = vec![
        CreateModelBackend {
            provider_id: provider_a.clone(),
            model: "gemini-3.8-flash".into(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: Some(false),
        },
        CreateModelBackend {
            provider_id: provider_b.clone(),
            model: "gemini-3.5-pro".into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(true),
        },
    ];
    route(&gw, "malformed-failover", provider_a.clone(), targets).await?;

    let (id, status, wire) = dispatch_stream(&gw, "malformed-failover").await?;
    assert_eq!(status, StatusCode::OK, "target B must answer: {wire}");
    assert!(
        wire.contains("recovered answer"),
        "target B content: {wire}"
    );
    assert!(
        wire.contains("\"finish_reason\":\"stop\""),
        "terminal missing: {wire}"
    );
    // The failed first stream must not leak into the client response.
    assert!(
        !wire.contains("MALFORMED_FUNCTION_CALL") && !wire.contains("test-signature"),
        "first failure stream leaked: {wire}"
    );
    assert_eq!(
        broken.calls(),
        2,
        "target A's healthy third reply stays unfetched"
    );
    assert_eq!(healthy.calls(), 1, "backend B called once");
    assert_no_failed_response_leak(&wire);

    let mut rows = recv_logs(&mut logs, 3).await;
    assert_attempt_sequence(&mut rows, &id);
    for failed in &rows[..2] {
        assert_malformed_attempt(failed);
        assert_eq!(failed.provider_id, provider_a, "failed row is target A");
    }
    let completed = &rows[2];
    assert_eq!(completed.diagnostic.attempt_outcome, "completed");
    assert_eq!(completed.client_status_code, 200);
    assert_eq!(completed.provider_id, provider_b, "answered by target B");
    assert_eq!(completed.diagnostic.attempt_index, Some(3));
    assert_eq!(
        completed.upstream_model, "gemini-3.5-pro",
        "target B serves a different backend model"
    );
    assert!(logs.try_recv().is_err(), "no extra rows");
    Ok(())
}

// ── 3b. Real v1internal envelope, non-stream client, force-stream fallback ───

// google/antigravity OAuth channel: the upstream serves the enveloped
// {"response": …} SSE shape from the production incident, the client asks
// for a non-stream response (so the dispatcher routes through
// handle_non_stream_via_upstream_stream with the vendor raw-chunk hook), and
// the malformed zero-payload terminal on target A gets one same-target retry
// before failing over to the different-model fallback target B.
#[tokio::test]
async fn enveloped_malformed_nonstream_fails_over_via_upstream_stream() -> anyhow::Result<()> {
    assert_enveloped_malformed_failover(Framing::Lf, false).await
}

#[tokio::test]
async fn enveloped_crlf_and_eof_malformed_fails_over_before_commit() -> anyhow::Result<()> {
    for framing in REGRESSION_FRAMINGS {
        for stream in [true, false] {
            assert_enveloped_malformed_failover(framing, stream).await?;
        }
    }
    Ok(())
}

async fn assert_enveloped_malformed_failover(framing: Framing, stream: bool) -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let broken = start_upstream_with_framing(
        vec![
            Reply::EnvelopedMalformed,
            Reply::EnvelopedMalformed,
            Reply::EnvelopedHealthy,
        ],
        framing,
    )
    .await?;
    let healthy = start_upstream_with_framing(vec![Reply::EnvelopedHealthy], framing).await?;
    let provider_a = antigravity_provider(&gw, &broken.url).await?;
    let provider_b = antigravity_provider(&gw, &healthy.url).await?;
    let targets = vec![
        CreateModelBackend {
            provider_id: provider_a.clone(),
            model: "gemini-3.8-flash".into(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: Some(false),
        },
        CreateModelBackend {
            provider_id: provider_b.clone(),
            model: "gemini-3.5-pro".into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(true),
        },
    ];
    route(&gw, "enveloped-failover", provider_a.clone(), targets).await?;

    let request = if stream {
        chat_stream_request("enveloped-failover")
    } else {
        chat_non_stream_request("enveloped-failover")
    };
    let (id, status, wire) = dispatch_request(&gw, request).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "{framing:?}/stream={stream}: response must come from target B: {wire}"
    );
    if stream {
        assert_healthy_stream_once(&wire);
    } else {
        let value: Value = serde_json::from_str(&wire)?;
        assert_eq!(
            value["choices"][0]["message"]["content"], "recovered answer",
            "target B content: {wire}"
        );
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["prompt_tokens"], 5);
        assert_eq!(value["usage"]["total_tokens"], 7);
    }
    assert!(
        !wire.contains("MALFORMED_FUNCTION_CALL") && !wire.contains("test-signature"),
        "enveloped failure must not leak into the client response: {wire}"
    );
    assert_eq!(
        broken.calls(),
        2,
        "target A's healthy third reply stays unfetched"
    );
    assert_eq!(healthy.calls(), 1, "target B called once");
    assert_no_failed_response_leak(&wire);

    let mut rows = recv_logs(&mut logs, 3).await;
    assert_attempt_sequence(&mut rows, &id);
    for failed in &rows[..2] {
        assert_malformed_attempt(failed);
        assert_eq!(failed.provider_id, provider_a);
    }
    let completed = &rows[2];
    assert_eq!(completed.diagnostic.attempt_outcome, "completed");
    assert_eq!(completed.client_status_code, 200);
    assert_eq!(completed.provider_id, provider_b);
    assert_eq!(completed.diagnostic.attempt_index, Some(3));
    assert_eq!(
        completed.upstream_model, "gemini-3.5-pro",
        "fallback target serves a different backend model"
    );
    assert!(logs.try_recv().is_err(), "no extra rows");
    Ok(())
}

// ── Same-target recovery: plain stream + subscription stream/aggregation ───

#[tokio::test]
async fn malformed_recovers_in_place_without_touching_fallback() -> anyhow::Result<()> {
    for (subscription, stream) in [(false, true), (true, true), (true, false)] {
        let (_dir, gw, mut logs) = setup().await?;
        let (malformed, healthy) = if subscription {
            (Reply::EnvelopedMalformed, Reply::EnvelopedHealthy)
        } else {
            (Reply::Malformed, Reply::Healthy)
        };
        let upstream = start_upstream(vec![malformed, healthy]).await?;
        let fallback = start_upstream(vec![healthy]).await?;
        let provider = if subscription {
            antigravity_provider(&gw, &upstream.url).await?
        } else {
            gemini_provider(&gw, &upstream.url).await?
        };
        let fallback_provider = if subscription {
            antigravity_provider(&gw, &fallback.url).await?
        } else {
            gemini_provider(&gw, &fallback.url).await?
        };
        route(
            &gw,
            "in-place-recovery",
            provider.clone(),
            fallback_targets(&provider, &fallback_provider),
        )
        .await?;
        let body = if stream {
            chat_stream_request("in-place-recovery")
        } else {
            chat_non_stream_request("in-place-recovery")
        };
        let ctx = RequestContext::new(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, TIMEOUT);
        let cancellation = ctx.cancellation.clone();
        let (id, status, wire) = dispatch_with_context(&gw, body, ctx).await?;
        assert!(
            !cancellation.is_cancelled(),
            "dropping the failed attempt must not cancel recovery"
        );
        assert_eq!(
            status,
            StatusCode::OK,
            "subscription={subscription}/stream={stream}: {wire}"
        );
        if stream {
            assert_healthy_stream_once(&wire);
        } else {
            let value: Value = serde_json::from_str(&wire)?;
            assert_eq!(
                value["choices"][0]["message"]["content"],
                "recovered answer"
            );
            assert_eq!(value["choices"][0]["finish_reason"], "stop");
            assert_eq!(value["usage"]["prompt_tokens"], 5);
            assert_eq!(value["usage"]["total_tokens"], 7);
        }
        assert_no_failed_response_leak(&wire);
        assert_eq!(upstream.calls(), 2, "one retry on the selected target");
        assert_eq!(fallback.calls(), 0, "recovery must not reroute");

        let requests = upstream.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0], requests[1],
            "retry must reuse URI, body and all headers"
        );
        let request_body: Value = serde_json::from_slice(&requests[0].body)?;
        let request_url = reqwest::Url::parse(&format!("{}{}", upstream.url, requests[0].uri))?;
        let query_keys = request_url
            .query_pairs()
            .filter(|(name, _)| name == "key")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        if subscription {
            // The public base name resolves to the adaptive tier even before
            // account discovery. Retrying must reuse that exact prepared id.
            assert_eq!(request_body["model"], "gemini-3.8-flash-tiered");
            // This local fixture intentionally bypasses the Google auth driver
            // to retain its loopback URL. It has no runtime Bearer binding and
            // does not test Bearer generation; the entire prepared header/URI
            // set must still be identical on retry (asserted above).
            assert!(!requests[0].headers.contains_key("authorization"));
            assert!(query_keys.is_empty());
        } else {
            assert!(requests[0].uri.path().contains("gemini-3.8-flash"));
            assert_eq!(
                query_keys,
                ["local-fake-token"],
                "same configured API credential"
            );
        }

        let mut rows = recv_logs(&mut logs, 2).await;
        assert_attempt_sequence(&mut rows, &id);
        assert_malformed_attempt(&rows[0]);
        assert_eq!(rows[1].diagnostic.attempt_outcome, "completed");
        assert_eq!(rows[1].performance.completion, "completed");
        assert_eq!(rows[1].client_status_code, 200);
        assert_eq!(rows[1].upstream_status_code, Some(200));
        assert_eq!(
            rows[1].usage.total_tokens, 7,
            "failed usage must not accumulate"
        );
        for row in &rows {
            assert_eq!(row.provider_id, provider);
            assert_eq!(row.upstream_model, "gemini-3.8-flash");
        }
        assert_eq!(rows[0].provider_key_name, rows[1].provider_key_name);
        assert!(logs.try_recv().is_err(), "exactly two attempt rows");
    }
    Ok(())
}

#[tokio::test]
async fn same_provider_different_models_have_independent_retry_budgets() -> anyhow::Result<()> {
    for stream in [true, false] {
        let (_dir, gw, mut logs) = setup().await?;
        let upstream = start_upstream(vec![
            Reply::EnvelopedMalformed,
            Reply::EnvelopedMalformed,
            Reply::EnvelopedMalformed,
            Reply::EnvelopedHealthy,
        ])
        .await?;
        let provider = antigravity_provider(&gw, &upstream.url).await?;
        route(
            &gw,
            "model-budgets",
            provider.clone(),
            fallback_targets(&provider, &provider),
        )
        .await?;
        let body = if stream {
            chat_stream_request("model-budgets")
        } else {
            chat_non_stream_request("model-budgets")
        };
        let (id, status, wire) = dispatch_request(&gw, body).await?;
        assert_eq!(status, StatusCode::OK, "stream={stream}: {wire}");
        assert!(wire.contains("recovered answer"), "{wire}");
        assert_no_failed_response_leak(&wire);
        assert_eq!(
            upstream.calls(),
            4,
            "each model target owns a two-attempt budget"
        );
        let requests = upstream.requests();
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[2], requests[3]);
        let models = requests
            .iter()
            .map(|request| {
                serde_json::from_slice::<Value>(&request.body).unwrap()["model"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            models,
            [
                "gemini-3.8-flash-tiered",
                "gemini-3.8-flash-tiered",
                "gemini-3.5-pro",
                "gemini-3.5-pro"
            ]
        );
        let mut rows = recv_logs(&mut logs, 4).await;
        assert_attempt_sequence(&mut rows, &id);
        for row in &rows[..3] {
            assert_malformed_attempt(row);
        }
        assert_eq!(rows[3].diagnostic.attempt_outcome, "completed");
        // HTTP attempt logs retain the configured backend names, whereas the
        // subscription envelope above carries the resolved wire tier. Each
        // backend must still own its independent two-attempt retry budget.
        for (row, model) in rows.iter().zip([
            "gemini-3.8-flash",
            "gemini-3.8-flash",
            "gemini-3.5-pro",
            "gemini-3.5-pro",
        ]) {
            assert_eq!(row.provider_id, provider);
            assert_eq!(row.upstream_model, model);
        }
        assert!(logs.try_recv().is_err());
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_http502_does_not_retry_the_same_target() -> anyhow::Result<()> {
    for (subscription, stream) in [(false, true), (true, true), (true, false)] {
        let (_dir, gw, mut logs) = setup().await?;
        let healthy_reply = if subscription {
            Reply::EnvelopedHealthy
        } else {
            Reply::Healthy
        };
        let upstream = start_upstream(vec![Reply::Http502, healthy_reply]).await?;
        let fallback = start_upstream(vec![healthy_reply]).await?;
        let provider = if subscription {
            antigravity_provider(&gw, &upstream.url).await?
        } else {
            gemini_provider(&gw, &upstream.url).await?
        };
        let fallback_provider = if subscription {
            antigravity_provider(&gw, &fallback.url).await?
        } else {
            gemini_provider(&gw, &fallback.url).await?
        };
        route(
            &gw,
            "http502",
            provider.clone(),
            fallback_targets(&provider, &fallback_provider),
        )
        .await?;
        let body = if stream {
            chat_stream_request("http502")
        } else {
            chat_non_stream_request("http502")
        };
        let (id, status, wire) = dispatch_request(&gw, body).await?;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert!(wire.contains("recovered answer"), "{wire}");
        assert_no_failed_response_leak(&wire);
        assert_eq!(
            upstream.calls(),
            1,
            "ordinary HTTP 502 must not use same-target retry"
        );
        assert_eq!(fallback.calls(), 1, "ordinary fallback policy is unchanged");
        let mut rows = recv_logs(&mut logs, 2).await;
        assert_attempt_sequence(&mut rows, &id);
        assert_eq!(rows[0].provider_id, provider);
        assert_eq!(rows[0].upstream_status_code, Some(502));
        assert_eq!(rows[0].diagnostic.attempt_outcome, "failed");
        assert_eq!(rows[1].provider_id, fallback_provider);
        assert_eq!(rows[1].diagnostic.attempt_outcome, "completed");
        assert!(logs.try_recv().is_err());
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_before_malformed_reply_prevents_same_target_retry() -> anyhow::Result<()> {
    for stream in [true, false] {
        let (_dir, gw, mut logs) = setup().await?;
        let ctx = RequestContext::new(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, TIMEOUT);
        let cancellation = ctx.cancellation.clone();
        let upstream = start_upstream_controlled(
            vec![Reply::EnvelopedMalformed, Reply::EnvelopedHealthy],
            Framing::Lf,
            Some(cancellation.clone()),
        )
        .await?;
        let provider = antigravity_provider(&gw, &upstream.url).await?;
        route(&gw, "cancelled-malformed", provider, vec![]).await?;
        let body = if stream {
            chat_stream_request("cancelled-malformed")
        } else {
            chat_non_stream_request("cancelled-malformed")
        };
        let (id, status, wire) = dispatch_with_context(&gw, body, ctx).await?;
        assert!(
            cancellation.is_cancelled(),
            "fixture cancels while the first call is in flight"
        );
        assert_ne!(
            status,
            StatusCode::OK,
            "cancelled attempt cannot recover: {wire}"
        );
        assert_eq!(
            upstream.calls(),
            1,
            "cancelled request must not be replayed"
        );
        assert!(!wire.contains("recovered answer"));
        let mut rows = recv_logs(&mut logs, 1).await;
        assert_attempt_sequence(&mut rows, &id);
        assert!(logs.try_recv().is_err());
    }
    Ok(())
}

// ── 4. Healthy stream unchanged ──────────────────────────────────────────────

fn assert_healthy_stream_once(wire: &str) {
    for marker in [
        "recovered answer",
        "\"finish_reason\":\"stop\"",
        "data: [DONE]",
    ] {
        assert_eq!(
            wire.matches(marker).count(),
            1,
            "healthy output must not be lost or duplicated ({marker}): {wire}"
        );
    }
    assert!(
        !wire.contains("\"error\""),
        "unexpected stream error: {wire}"
    );
}

#[tokio::test]
async fn healthy_first_chunk_stream_is_unchanged() -> anyhow::Result<()> {
    assert_healthy_stream(Reply::Healthy, Framing::Lf).await
}

#[tokio::test]
async fn healthy_crlf_and_eof_stream_is_not_lost_or_duplicated() -> anyhow::Result<()> {
    for reply in [Reply::Healthy, Reply::EnvelopedHealthy] {
        for framing in REGRESSION_FRAMINGS {
            assert_healthy_stream(reply, framing).await?;
        }
    }
    Ok(())
}

async fn assert_healthy_stream(reply: Reply, framing: Framing) -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let upstream = start_upstream_with_framing(vec![reply], framing).await?;
    let provider = if reply == Reply::EnvelopedHealthy {
        antigravity_provider(&gw, &upstream.url).await?
    } else {
        gemini_provider(&gw, &upstream.url).await?
    };
    route(&gw, "healthy-stream", provider, vec![]).await?;

    let (_id, status, wire) = dispatch_stream(&gw, "healthy-stream").await?;
    assert_eq!(status, StatusCode::OK, "{reply:?}/{framing:?}: {wire}");
    assert_healthy_stream_once(&wire);
    assert_eq!(upstream.calls(), 1, "no retries on healthy streams");

    let rows = recv_logs(&mut logs, 1).await;
    assert_eq!(rows[0].diagnostic.attempt_outcome, "completed");
    assert_eq!(rows[0].diagnostic.attempt_index, Some(1));
    assert_eq!(rows[0].client_status_code, 200);
    assert_eq!(rows[0].upstream_status_code, Some(200));
    assert_eq!(rows[0].usage.prompt_tokens, 5);
    assert_eq!(rows[0].usage.total_tokens, 7);
    assert!(logs.try_recv().is_err(), "exactly one row");
    Ok(())
}

// ── 5. SAFETY zero payload is not failed over ────────────────────────────────

#[tokio::test]
async fn safety_zero_payload_is_not_retried() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let upstream = start_upstream(vec![Reply::Safety]).await?;
    let provider = gemini_provider(&gw, &upstream.url).await?;
    route(&gw, "safety-stream", provider, vec![]).await?;

    let (_id, status, wire) = dispatch_stream(&gw, "safety-stream").await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "SAFETY keeps today's behavior: {wire}"
    );
    assert_eq!(upstream.calls(), 1, "SAFETY must not be retried");
    // The finish reason passes through verbatim (only STOP/MAX_TOKENS are
    // normalized by the Gemini parser).
    assert!(wire.contains("\"finish_reason\":\"SAFETY\""), "{wire}");
    assert!(wire.contains("data: [DONE]"), "{wire}");

    let rows = recv_logs(&mut logs, 1).await;
    assert_eq!(rows[0].client_status_code, 200);
    assert!(logs.try_recv().is_err(), "exactly one row");
    Ok(())
}

// ── 6. Payload before MALFORMED commits ──────────────────────────────────────

#[tokio::test]
async fn text_thinking_or_tool_then_malformed_is_not_retried() -> anyhow::Result<()> {
    for (subscription, stream) in [(false, true), (true, true), (true, false)] {
        for (plain, wrapped, payload) in [
            (
                Reply::TextThenMalformed,
                Reply::EnvelopedTextThenMalformed,
                "partial output",
            ),
            (
                Reply::ThinkingThenMalformed,
                Reply::EnvelopedThinkingThenMalformed,
                "partial thinking",
            ),
            (
                Reply::ToolThenMalformed,
                Reply::EnvelopedToolThenMalformed,
                "lookup_weather",
            ),
        ] {
            let (_dir, gw, mut logs) = setup().await?;
            let reply = if subscription { wrapped } else { plain };
            let healthy = if subscription {
                Reply::EnvelopedHealthy
            } else {
                Reply::Healthy
            };
            let upstream = start_upstream(vec![reply, healthy]).await?;
            let fallback = start_upstream(vec![healthy]).await?;
            let provider = if subscription {
                antigravity_provider(&gw, &upstream.url).await?
            } else {
                gemini_provider(&gw, &upstream.url).await?
            };
            let fallback_provider = if subscription {
                antigravity_provider(&gw, &fallback.url).await?
            } else {
                gemini_provider(&gw, &fallback.url).await?
            };
            route(
                &gw,
                "partial-output",
                provider.clone(),
                fallback_targets(&provider, &fallback_provider),
            )
            .await?;
            let body = if stream {
                chat_stream_request("partial-output")
            } else {
                chat_non_stream_request("partial-output")
            };
            let (id, status, wire) = dispatch_request(&gw, body).await?;
            assert_eq!(
                status,
                StatusCode::OK,
                "{reply:?}/stream={stream}: existing behavior stays committed: {wire}"
            );
            assert!(
                wire.contains(payload),
                "payload must reach the client: {wire}"
            );
            assert!(
                !wire.contains("recovered answer"),
                "no healthy replay/fallback: {wire}"
            );
            assert_eq!(upstream.calls(), 1, "output exists; no same-target retry");
            assert_eq!(fallback.calls(), 0, "output exists; no fallback");
            if !stream {
                let value: Value = serde_json::from_str(&wire)?;
                // OpenAI's formatter normalizes any nonempty tool_calls to
                // tool_calls; text/thinking preserve the upstream finish reason.
                let reason = if plain == Reply::ToolThenMalformed {
                    "tool_calls"
                } else {
                    "MALFORMED_FUNCTION_CALL"
                };
                assert_eq!(value["choices"][0]["finish_reason"], reason);
            } else if plain != Reply::ToolThenMalformed {
                assert!(
                    wire.contains("\"finish_reason\":\"MALFORMED_FUNCTION_CALL\""),
                    "{wire}"
                );
            }
            let mut rows = recv_logs(&mut logs, 1).await;
            assert_attempt_sequence(&mut rows, &id);
            assert_eq!(rows[0].client_status_code, 200);
            assert!(logs.try_recv().is_err(), "exactly one row");
        }
    }
    Ok(())
}

// ── 7. Failover rows persist with distinct primary keys ──────────────────────

// Both failed attempt rows and the answering target's row ride different
// performance Attempts (including the same-target replay), each with its
// own log id. This test runs the real logging collector against the sqlite
// database and reads the rows back from the table, guarding against any
// primary-key collision rejecting the append batch.
#[tokio::test]
async fn malformed_failover_rows_persist_with_distinct_ids() -> anyhow::Result<()> {
    let (_dir, gw, logs) = setup().await?;
    let storage = gw.storage.clone();
    let collector = tokio::spawn(run_collector(logs, storage.clone()));
    let dropped_before = logging_status().database_write_dropped;

    let broken = start_upstream(vec![Reply::Malformed]).await?;
    let healthy = start_upstream(vec![Reply::Healthy]).await?;
    let provider_a = gemini_provider(&gw, &broken.url).await?;
    let provider_b = gemini_provider(&gw, &healthy.url).await?;
    let targets = vec![
        CreateModelBackend {
            provider_id: provider_a,
            model: "gemini-3.8-flash".into(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: Some(false),
        },
        CreateModelBackend {
            provider_id: provider_b,
            model: "gemini-3.5-pro".into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(true),
        },
    ];
    route(
        &gw,
        "malformed-persist",
        gemini_provider(&gw, &broken.url).await?,
        targets,
    )
    .await?;

    let (request_id, status, wire) = dispatch_stream(&gw, "malformed-persist").await?;
    assert_eq!(status, StatusCode::OK, "{wire}");
    assert_eq!(broken.calls(), 2, "backend A gets one retry");
    assert_eq!(healthy.calls(), 1, "backend B called once");

    // The collector flushes on its 2s tick; poll the table until both failed
    // 502 attempts and the final 200 attempt are visible.
    let mut rows = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while rows.len() < 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        rows = storage
            .logs()
            .query(LogQuery {
                client_request_id: Some(request_id.clone()),
                limit: Some(10),
                ..Default::default()
            })
            .await?
            .items;
    }
    assert_eq!(
        rows.len(),
        3,
        "retry and failover rows must persist: {rows:?}"
    );
    let ids: std::collections::HashSet<_> = rows.iter().map(|row| &row.id).collect();
    assert_eq!(ids.len(), 3, "rows must not share the primary key");
    let statuses: Vec<i32> = rows
        .iter()
        .filter_map(|row| row.client_status_code)
        .collect();
    assert_eq!(
        statuses.iter().filter(|&&status| status == 502).count(),
        2,
        "failed attempts: {statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|&&status| status == 200).count(),
        1,
        "final success: {statuses:?}"
    );
    for row in &rows {
        assert_eq!(row.client_request_id.as_deref(), Some(request_id.as_str()));
    }
    // No append batch may have been rejected by the database writer.
    assert_eq!(
        logging_status().database_write_dropped,
        dropped_before,
        "append_batch must not have dropped any rows"
    );

    collector.abort();
    Ok(())
}
