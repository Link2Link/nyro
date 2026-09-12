//! Gemini MALFORMED_FUNCTION_CALL zero-payload terminal: pre-commit probe
//! and per-target failover through the real dispatcher and local HTTP.
//!
//! Production incident replay (de-enveloped pure Gemini SSE; the parsed
//! path is identical to the v1internal envelope form): the upstream answers
//! HTTP 200 with a single SSE event whose only candidate carries
//! `finishReason: "MALFORMED_FUNCTION_CALL"` and no text/tool-call output.
//! The gateway must fail that attempt exactly once — no same-target replay —
//! and fail over to the next target of the same model's ordered candidates
//! (selector-appended fallback rows included), surfacing an explicit 502
//! that preserves the upstream 200, finishReason/finishMessage and usage
//! when every candidate is exhausted, without regressing healthy streams or
//! other finish reasons.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{
        CreateModel, CreateModelBackend, CreateProvider, LogQuery, UpsertOAuthCredential,
    },
    logging::{logging_status, run_collector},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
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
        }
    }
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

type UpstreamState = (Arc<Vec<Reply>>, Arc<std::sync::atomic::AtomicUsize>);

async fn scripted_upstream(State((script, calls)): State<UpstreamState>) -> Response {
    let index = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let reply = script
        .get(index.min(script.len().saturating_sub(1)))
        .copied()
        .unwrap_or(Reply::Healthy);
    ([("content-type", "text/event-stream")], reply.sse()).into_response()
}

struct Upstream {
    url: String,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    _job: tokio::task::JoinHandle<()>,
}

impl Upstream {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

async fn start_upstream(script: Vec<Reply>) -> anyhow::Result<Upstream> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = (Arc::new(script), calls.clone());
    let job = tokio::spawn(async move {
        let app = Router::new()
            .route("/*path", post(scripted_upstream))
            .with_state(state);
        let _ = axum::serve(listener, app).await;
    });
    Ok(Upstream {
        url,
        calls,
        _job: job,
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
    let request = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let ctx = RequestContext::new(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, TIMEOUT);
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
    let body = chat_stream_request(model);
    let request = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let ctx = RequestContext::new(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, TIMEOUT);
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

// ── 1. Single backend: one call, explicit 502 ────────────────────────────────

#[tokio::test]
async fn malformed_single_backend_fails_once_with_502() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    // The healthy second reply exists only to prove it is never fetched.
    let upstream = start_upstream(vec![Reply::Malformed, Reply::Healthy]).await?;
    let provider = gemini_provider(&gw, &upstream.url).await?;
    route(&gw, "malformed-single", provider, vec![]).await?;

    let (_id, status, wire) = dispatch_stream(&gw, "malformed-single").await?;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "single backend must surface the explicit 502: {wire}"
    );
    let value: Value = serde_json::from_str(&wire)?;
    let message = value["error"]["message"].as_str().expect("error message");
    assert!(message.contains("finish_reason=malformed_function_call"));
    assert!(message.contains("Function call is empty"));
    assert_eq!(
        upstream.calls(),
        1,
        "no same-target replay; the healthy second reply stays unfetched"
    );

    let rows = recv_logs(&mut logs, 1).await;
    let row = &rows[0];
    assert_eq!(row.client_status_code, 502);
    assert_eq!(row.upstream_status_code, Some(200), "upstream was a 200");
    assert_eq!(row.diagnostic.attempt_index, Some(1));
    assert_eq!(row.diagnostic.attempt_outcome, "failed");
    assert_eq!(
        row.diagnostic.failure_kind.as_deref(),
        Some("upstream_error"),
        "explicit record_failure must outrank ambiguous_terminal: {row:?}"
    );
    let causes = row.diagnostic.error_causes.join(" | ");
    assert!(
        causes.contains("finish_reason=malformed_function_call")
            && causes.contains("Function call is empty"),
        "diagnostic must carry the upstream finish reason verbatim: {causes}"
    );
    assert_eq!(row.usage.prompt_tokens, 65036, "usage preserved: {row:?}");
    assert_eq!(row.usage.total_tokens, 65322);
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

    let (_id, status, wire) = dispatch_stream(&gw, "malformed-exhaust").await?;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "client must see 502: {wire}"
    );
    let value: Value = serde_json::from_str(&wire)?;
    let message = value["error"]["message"].as_str().expect("error message");
    assert!(message.contains("finish_reason=malformed_function_call"));
    assert!(message.contains("Function call is empty"));
    assert_eq!(broken_a.calls(), 1, "backend A called exactly once");
    assert_eq!(broken_b.calls(), 1, "backend B called exactly once");

    let rows = recv_logs(&mut logs, 2).await;
    assert!(rows.iter().all(|row| row.client_status_code == 502));
    assert!(rows.iter().all(|row| row.upstream_status_code == Some(200)));
    let mut indexes = rows
        .iter()
        .map(|row| row.diagnostic.attempt_index)
        .collect::<Vec<_>>();
    indexes.sort();
    assert_eq!(indexes, vec![Some(1), Some(2)], "rows: {rows:?}");
    assert!(
        rows.iter()
            .all(|row| row.diagnostic.failure_kind.as_deref() == Some("upstream_error"))
    );
    assert!(logs.try_recv().is_err(), "no extra rows");
    Ok(())
}

// ── 3. Fail over to the next target (fallback row, different model) ─────────

#[tokio::test]
async fn malformed_backend_fails_over_to_next_target() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let broken = start_upstream(vec![Reply::Malformed, Reply::Malformed]).await?;
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

    let (_id, status, wire) = dispatch_stream(&gw, "malformed-failover").await?;
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
        1,
        "backend A failed once; no same-target replay"
    );
    assert_eq!(healthy.calls(), 1, "backend B called once");

    let rows = recv_logs(&mut logs, 2).await;
    let failed: Vec<_> = rows
        .iter()
        .filter(|row| row.diagnostic.attempt_outcome == "failed")
        .collect();
    let completed: Vec<_> = rows
        .iter()
        .filter(|row| row.diagnostic.attempt_outcome == "completed")
        .collect();
    assert_eq!((failed.len(), completed.len()), (1, 1), "rows: {rows:?}");
    let failed = failed[0];
    assert_eq!(failed.client_status_code, 502);
    assert_eq!(failed.upstream_status_code, Some(200));
    assert_eq!(failed.provider_id, provider_a, "failed row is target A");
    assert_eq!(failed.diagnostic.attempt_index, Some(1));
    assert_eq!(
        failed.diagnostic.failure_kind.as_deref(),
        Some("upstream_error")
    );
    let causes = failed.diagnostic.error_causes.join(" | ");
    assert!(
        causes.contains("finish_reason=malformed_function_call")
            && causes.contains("Function call is empty"),
        "diagnostic must carry the upstream finish reason: {causes}"
    );
    let completed = completed[0];
    assert_eq!(completed.client_status_code, 200);
    assert_eq!(completed.provider_id, provider_b, "answered by target B");
    assert_eq!(completed.diagnostic.attempt_index, Some(2));
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
// the malformed zero-payload terminal on target A must fail that attempt
// once and fail over to the different-model fallback target B.
#[tokio::test]
async fn enveloped_malformed_nonstream_fails_over_via_upstream_stream() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let broken = start_upstream(vec![Reply::EnvelopedMalformed, Reply::EnvelopedMalformed]).await?;
    let healthy = start_upstream(vec![Reply::EnvelopedHealthy]).await?;
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

    let (_id, status, wire) =
        dispatch_request(&gw, chat_non_stream_request("enveloped-failover")).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "aggregated non-stream response must come from target B: {wire}"
    );
    let value: Value = serde_json::from_str(&wire)?;
    assert_eq!(
        value["choices"][0]["message"]["content"], "recovered answer",
        "target B content: {wire}"
    );
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    assert!(
        !wire.contains("MALFORMED_FUNCTION_CALL") && !wire.contains("test-signature"),
        "enveloped failure must not leak into the client response: {wire}"
    );
    assert_eq!(
        broken.calls(),
        1,
        "target A failed once; no same-target replay"
    );
    assert_eq!(healthy.calls(), 1, "target B called once");

    let rows = recv_logs(&mut logs, 2).await;
    let failed: Vec<_> = rows
        .iter()
        .filter(|row| row.diagnostic.attempt_outcome == "failed")
        .collect();
    let completed: Vec<_> = rows
        .iter()
        .filter(|row| row.diagnostic.attempt_outcome == "completed")
        .collect();
    assert_eq!((failed.len(), completed.len()), (1, 1), "rows: {rows:?}");
    let failed = failed[0];
    assert_eq!(failed.client_status_code, 502);
    assert_eq!(failed.upstream_status_code, Some(200));
    assert_eq!(failed.provider_id, provider_a);
    assert_eq!(failed.diagnostic.attempt_index, Some(1));
    assert_eq!(
        failed.diagnostic.failure_kind.as_deref(),
        Some("upstream_error")
    );
    let causes = failed.diagnostic.error_causes.join(" | ");
    assert!(
        causes.contains("finish_reason=malformed_function_call")
            && causes.contains("Function call is empty"),
        "diagnostic must carry the enveloped finish reason: {causes}"
    );
    assert_eq!(
        failed.usage.prompt_tokens, 65036,
        "enveloped usage metadata preserved: {failed:?}"
    );
    let completed = completed[0];
    assert_eq!(completed.client_status_code, 200);
    assert_eq!(completed.provider_id, provider_b);
    assert_eq!(completed.diagnostic.attempt_index, Some(2));
    assert_eq!(
        completed.upstream_model, "gemini-3.5-pro",
        "fallback target serves a different backend model"
    );
    assert!(logs.try_recv().is_err(), "no extra rows");
    Ok(())
}

// ── 4. Healthy stream unchanged ──────────────────────────────────────────────

#[tokio::test]
async fn healthy_first_chunk_stream_is_unchanged() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let upstream = start_upstream(vec![Reply::Healthy]).await?;
    let provider = gemini_provider(&gw, &upstream.url).await?;
    route(&gw, "healthy-stream", provider, vec![]).await?;

    let (_id, status, wire) = dispatch_stream(&gw, "healthy-stream").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(wire.contains("recovered answer"), "{wire}");
    assert!(wire.contains("\"finish_reason\":\"stop\""), "{wire}");
    assert_eq!(upstream.calls(), 1, "no retries on healthy streams");

    let rows = recv_logs(&mut logs, 1).await;
    assert_eq!(rows[0].diagnostic.attempt_outcome, "completed");
    assert_eq!(rows[0].client_status_code, 200);
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
async fn text_then_malformed_is_not_retried() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let upstream = start_upstream(vec![Reply::TextThenMalformed]).await?;
    let provider = gemini_provider(&gw, &upstream.url).await?;
    route(&gw, "partial-stream", provider, vec![]).await?;

    let (_id, status, wire) = dispatch_stream(&gw, "partial-stream").await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "committed stream stays committed: {wire}"
    );
    assert!(
        wire.contains("partial output"),
        "payload must reach the client: {wire}"
    );
    assert_eq!(upstream.calls(), 1, "payload already delivered; no retry");
    assert!(
        wire.contains("\"finish_reason\":\"MALFORMED_FUNCTION_CALL\""),
        "{wire}"
    );

    let rows = recv_logs(&mut logs, 1).await;
    assert_eq!(rows[0].client_status_code, 200);
    assert!(logs.try_recv().is_err(), "exactly one row");
    Ok(())
}

// ── 7. Failover rows persist with distinct primary keys ──────────────────────

// The failed attempt's row and the answering target's row ride different
// performance Attempts (one per dispatcher target iteration), each with its
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
    assert_eq!(broken.calls(), 1, "backend A called once");
    assert_eq!(healthy.calls(), 1, "backend B called once");

    // The collector flushes on its 2s tick; poll the table until both the
    // failed 502 row and the final 200 row are visible.
    let mut rows = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while rows.len() < 2 && tokio::time::Instant::now() < deadline {
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
    assert_eq!(rows.len(), 2, "both failover rows must persist: {rows:?}");
    assert_ne!(
        rows[0].id, rows[1].id,
        "rows must not share the primary key"
    );
    let statuses: Vec<i32> = rows
        .iter()
        .filter_map(|row| row.client_status_code)
        .collect();
    assert!(statuses.contains(&502), "failed attempt row: {statuses:?}");
    assert!(statuses.contains(&200), "final success row: {statuses:?}");
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
