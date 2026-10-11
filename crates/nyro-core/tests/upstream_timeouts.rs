//! Real sockets + dispatcher regression coverage for inference phase timeouts.
//! All credentials/payloads are synthetic; Google subscription setup follows
//! gemini_malformed_retry.rs / stream_chunk_boundaries.rs. Do not pause Tokio:
//! SQLite/background timers would advance the I/O watchdog without real I/O.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use nyro_core::{
    Gateway,
    config::{GatewayConfig, UpstreamTimeouts},
    db::models::{CreateModel, CreateModelBackend, CreateProvider, UpsertOAuthCredential},
    logging::LogEntry,
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{
        client::{ProxyClient, UpstreamReadError},
        context::RequestContext,
        dispatcher::dispatch_pipeline,
    },
    storage::SqliteStorage,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    task::JoinHandle,
};

const PHASE: Duration = Duration::from_millis(200);
const TICK: Duration = Duration::from_millis(20);
const WATCHDOG: Duration = Duration::from_secs(5);
const TICKS: usize = 24;
const MODEL: &str = "phase-fixture";

fn short_timeouts() -> UpstreamTimeouts {
    UpstreamTimeouts {
        connect: PHASE,
        response_headers: PHASE,
        first_byte: PHASE,
        stream_idle: PHASE,
    }
}

/// Covers early returns, failed assertions and an outer watchdog expiring.
struct AbortOnDrop<T>(JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    ChatPassthrough,
    GeminiIr,
    AnthropicCompat,
    ForceStreamNonstream,
}
const STREAM_PATHS: [Path; 3] = [Path::ChatPassthrough, Path::GeminiIr, Path::AnthropicCompat];
const ALL_PATHS: [Path; 4] = [
    Path::ChatPassthrough,
    Path::GeminiIr,
    Path::AnthropicCompat,
    Path::ForceStreamNonstream,
];

impl Path {
    fn ingress(self) -> ProtocolId {
        if self == Self::AnthropicCompat {
            ANTHROPIC_MESSAGES_2023_06_01
        } else {
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        }
    }

    fn google(self) -> bool {
        matches!(self, Self::GeminiIr | Self::ForceStreamNonstream)
    }

    fn strategy(self) -> &'static str {
        match self {
            Self::ChatPassthrough => "pass_through",
            Self::AnthropicCompat => "raw_wire_compat",
            Self::GeminiIr | Self::ForceStreamNonstream => "native_ir",
        }
    }

    fn text_event(self, text: &str) -> String {
        let event = if self.google() {
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":text}]}}],
                "modelVersion":"gemini-fixture","responseId":"fixture-response"})
        } else {
            json!({"id":"fixture-response","object":"chat.completion.chunk","created":0,
                "model":"chat-fixture","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]})
        };
        self.event(event)
    }

    fn event(self, event: Value) -> String {
        let event = if self == Self::ForceStreamNonstream {
            json!({"response":event,"traceId":"fixture-trace"})
        } else {
            event
        };
        format!("data: {event}\n\n")
    }

    fn terminal(self) -> String {
        if self.google() {
            self.event(
                json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],
                "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},
                "modelVersion":"gemini-fixture","responseId":"fixture-response"}),
            )
        } else {
            format!("{}data: [DONE]\n\n", self.event(json!({
                "id":"fixture-response","object":"chat.completion.chunk","created":0,"model":"chat-fixture",
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}
            })))
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Reply {
    Active,
    HeartbeatsBeforeOutput,
    NoHeaders,
    NoFirstByte,
    PartialThenIdle,
    HealthyFallback,
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    line: String,
    body: Value,
}

struct Upstream {
    url: String,
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    _job: AbortOnDrop<()>,
}
impl Upstream {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Raw HTTP makes the three phases unambiguous: headers are written separately
/// from the first chunk, and a stall never accidentally turns into an empty EOF.
async fn start_upstream(path: Path, reply: Reply) -> anyhow::Result<Upstream> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let call_count = calls.clone();
    let captured = requests.clone();
    let job = AbortOnDrop(tokio::spawn(async move {
        // Dropping the listener task aborts every accepted connection too.
        let mut connections = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            let calls = call_count.clone();
            let requests = captured.clone();
            connections.push(AbortOnDrop(tokio::spawn(async move {
                let _ = serve(socket, path, reply, calls, requests).await;
            })));
        }
    }));
    Ok(Upstream {
        url,
        calls,
        requests,
        _job: job,
    })
}

async fn read_request(socket: &mut TcpStream) -> anyhow::Result<CapturedRequest> {
    let mut input = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        if let Some(offset) = input.windows(4).position(|window| window == b"\r\n\r\n") {
            break offset + 4;
        }
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "request ended before headers");
        input.extend_from_slice(&buffer[..count]);
        anyhow::ensure!(input.len() < 1024 * 1024, "fixture request too large");
    };
    let headers = std::str::from_utf8(&input[..header_end])?;
    let line = headers.lines().next().unwrap().to_string();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    anyhow::ensure!(length < 1024 * 1024, "fixture request body too large");
    while input.len() < header_end + length {
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "request ended before body");
        input.extend_from_slice(&buffer[..count]);
    }
    Ok(CapturedRequest {
        line,
        body: serde_json::from_slice(&input[header_end..header_end + length])?,
    })
}

async fn chunk(socket: &mut TcpStream, wire: &str) -> anyhow::Result<()> {
    socket
        .write_all(format!("{:x}\r\n", wire.len()).as_bytes())
        .await?;
    socket.write_all(wire.as_bytes()).await?;
    socket.write_all(b"\r\n").await?;
    Ok(())
}

async fn serve(
    mut socket: TcpStream,
    path: Path,
    reply: Reply,
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
) -> anyhow::Result<()> {
    socket.set_nodelay(true)?;
    let request = read_request(&mut socket).await?;
    calls.fetch_add(1, Ordering::SeqCst);
    requests.lock().unwrap().push(request);
    if !matches!(reply, Reply::NoHeaders) {
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await?;
    }
    match reply {
        Reply::NoHeaders | Reply::NoFirstByte => {}
        Reply::PartialThenIdle => chunk(&mut socket, &path.text_event("partial output")).await?,
        Reply::Active | Reply::HeartbeatsBeforeOutput => {
            for index in 0..TICKS {
                tokio::time::sleep(TICK).await;
                let event = if matches!(reply, Reply::HeartbeatsBeforeOutput) {
                    ": heartbeat\n\n".to_string()
                } else {
                    path.text_event(&format!("tick-{index:02} "))
                };
                chunk(&mut socket, &event).await?;
            }
            if matches!(reply, Reply::HeartbeatsBeforeOutput) {
                chunk(&mut socket, &path.text_event("heartbeat survived")).await?;
            }
            chunk(&mut socket, &path.terminal()).await?;
            socket.write_all(b"0\r\n\r\n").await?;
            return Ok(());
        }
        Reply::HealthyFallback => {
            chunk(&mut socket, &path.text_event("unexpected fallback answer")).await?;
            chunk(&mut socket, &path.terminal()).await?;
            socket.write_all(b"0\r\n\r\n").await?;
            return Ok(());
        }
    }
    // Remain open until the client abandons the timed-out request. The owner
    // also aborts this connection if a failed test drops before that happens.
    let mut end = [0_u8; 1];
    let _ = socket.read(&mut end).await;
    Ok(())
}

async fn setup(
    timeouts: UpstreamTimeouts,
) -> anyhow::Result<(tempfile::TempDir, Gateway, mpsc::Receiver<LogEntry>)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        upstream_timeouts: timeouts,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gateway, logs) = Gateway::from_storage(config, storage).await?;
    Ok((dir, gateway, logs))
}

async fn provider(gw: &Gateway, path: Path, url: &str) -> anyhow::Result<String> {
    let subscription = path == Path::ForceStreamNonstream;
    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            keys: vec![],
            name: uuid::Uuid::new_v4().to_string(),
            vendor: path.google().then(|| "google".into()),
            protocol: if path.google() {
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string()
            } else {
                "openai-compatible".into()
            },
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: path.google().then(|| {
                if subscription {
                    "antigravity"
                } else {
                    "default"
                }
                .into()
            }),
            models_source: None,
            static_models: None,
            api_key: "local-fake-token".into(),
            auth_mode: if subscription { "oauth" } else { "apikey" }.into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    if subscription {
        // Unknown auth driver intentionally keeps the loopback base URL rather
        // than binding a real Google host; the envelope only needs project_id.
        gw.storage
            .oauth_credentials()
            .upsert(
                &provider.id,
                UpsertOAuthCredential {
                    driver_key: "google-fixture".into(),
                    scheme: "oauth_auth_code_pkce".into(),
                    access_token: "fixture-access-token".into(),
                    refresh_token: None,
                    expires_at: Some("2099-01-01T00:00:00Z".into()),
                    resource_url: None,
                    subject_id: None,
                    scopes: None,
                    meta: Some(r#"{"project_id":"fixture-project"}"#.into()),
                },
            )
            .await?;
    }
    Ok(provider.id)
}

async fn route(
    gw: &Gateway,
    path: Path,
    primary: &str,
    fallback: Option<&str>,
) -> anyhow::Result<()> {
    let target_model = if path.google() {
        "gemini-fixture"
    } else {
        "chat-fixture"
    };
    let targets = fallback
        .map(|fallback| {
            vec![
                CreateModelBackend {
                    provider_id: primary.into(),
                    model: target_model.into(),
                    weight: Some(100),
                    priority: Some(1),
                    is_fallback: Some(false),
                },
                CreateModelBackend {
                    provider_id: fallback.into(),
                    model: target_model.into(),
                    weight: Some(100),
                    priority: Some(2),
                    is_fallback: Some(true),
                },
            ]
        })
        .unwrap_or_default();
    gw.admin()
        .create_model(CreateModel {
            name: MODEL.into(),
            balance: Some("priority".into()),
            target_provider: primary.into(),
            target_model: target_model.into(),
            targets,
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}

struct Output {
    id: String,
    status: StatusCode,
    wire: String,
    elapsed: Duration,
}

async fn dispatch(gw: &Gateway, path: Path) -> anyhow::Result<Output> {
    let ingress = path.ingress();
    let mut body = json!({"model":MODEL,"stream":path != Path::ForceStreamNonstream,
        "messages":[{"role":"user","content":"Give a short answer."}]});
    if path == Path::AnthropicCompat {
        body["max_tokens"] = json!(64);
    }
    if path.google() {
        // Native, simple tool schema mirrors the existing Gemini route fixtures.
        body["tools"] = json!([{"type":"function","function":{"name":"lookup_weather",
            "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]);
    }
    let request = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let ctx = RequestContext::without_total_timeout(ingress);
    // This guards the default factory without waiting 600 seconds.
    assert!(ctx.deadline.remaining() > Duration::from_secs(600));
    let id = ctx.request_id.clone();
    let trace = ctx.trace.clone();
    let started = Instant::now();
    let (status, wire) = tokio::time::timeout(WATCHDOG, async {
        let response = dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            RawEnvelope::new(
                Some(body),
                Default::default(),
                "POST",
                if path == Path::AnthropicCompat {
                    "/v1/messages"
                } else {
                    "/v1/chat/completions"
                },
            ),
            request,
            ingress,
            ctx,
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        Ok::<_, anyhow::Error>((status, String::from_utf8(bytes.to_vec())?))
    })
    .await??;
    let events = trace.snapshot();
    assert!(
        events.iter().any(|event| event.tag == "conversion"
            && event
                .detail
                .contains(&format!("strategy={};", path.strategy()))),
        "{path:?} must exercise its intended dispatcher strategy: {events:?}"
    );
    Ok(Output {
        id,
        status,
        wire,
        elapsed: started.elapsed(),
    })
}

async fn one_log(
    logs: &mut mpsc::Receiver<LogEntry>,
    output: &Output,
    outcome: &str,
) -> anyhow::Result<LogEntry> {
    let row = tokio::time::timeout(WATCHDOG, logs.recv())
        .await?
        .expect("attempt log");
    assert_eq!(
        row.diagnostic.client_request_id.as_deref(),
        Some(output.id.as_str()),
        "{row:?}"
    );
    assert_eq!(row.diagnostic.attempt_index, Some(1), "{row:?}");
    assert_eq!(row.diagnostic.attempt_outcome, outcome, "{row:?}");
    assert_eq!(row.performance.completion, outcome, "{row:?}");
    let result = row
        .diagnostic
        .final_result
        .as_ref()
        .expect("final request result");
    assert_eq!(result.client_request_id, output.id);
    assert_eq!(result.attempt_count, 1, "no same-target retry: {row:?}");
    assert_eq!(result.final_outcome, outcome, "{row:?}");
    assert_eq!(
        result.final_attempt_id.as_deref(),
        Some(row.diagnostic.log_id.as_str())
    );
    assert!(
        logs.try_recv().is_err(),
        "one request produces exactly one attempt row"
    );
    Ok(row)
}

fn assert_timeout_event(path: Path, output: &Output) {
    let errors: Vec<Value> = output
        .wire
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter(|event| event.get("error").is_some())
        .collect();
    assert_eq!(errors.len(), 1, "one explicit error event: {}", output.wire);
    assert_eq!(
        errors[0]["error"]["failure_kind"], "timeout",
        "{}",
        output.wire
    );
    assert_eq!(errors[0]["error"]["type"], "timeout", "{}", output.wire);
    assert_eq!(
        errors[0]["error"]["request_id"], output.id,
        "{}",
        output.wire
    );
    assert!(
        !output.wire.contains("conversion_stream_error"),
        "{}",
        output.wire
    );
    if path == Path::AnthropicCompat {
        assert!(output.wire.contains("event: error"), "{}", output.wire);
        assert!(
            !output.wire.contains("event: message_stop"),
            "timeout must not synthesize success: {}",
            output.wire
        );
    } else {
        assert!(
            !output.wire.contains("event: error"),
            "chat error must use a chat envelope"
        );
        assert!(
            !output.wire.contains("data: [DONE]"),
            "timeout must not synthesize success: {}",
            output.wire
        );
    }
}

fn assert_timeout_response(path: Path, output: &Output) {
    if output.status.is_success() {
        assert_ne!(
            path,
            Path::ForceStreamNonstream,
            "aggregation must not return empty/partial success: {}",
            output.wire
        );
        assert_timeout_event(path, output);
    } else {
        // Header send and compat pre-commit failures keep their existing HTTP
        // mapping; this change must expose a timeout, not mandate HTTP 504.
        assert!(
            output.status.is_server_error(),
            "{}: {}",
            output.status,
            output.wire
        );
        let value: Value = serde_json::from_str(&output.wire).expect("JSON error response");
        assert!(value["error"].is_object(), "{}", output.wire);
        let lower = output.wire.to_ascii_lowercase();
        assert!(
            lower.contains("timeout") || lower.contains("timed out"),
            "explicit timeout message: {}",
            output.wire
        );
        assert!(
            value.get("choices").is_none(),
            "error must not be an empty completion"
        );
    }
}

async fn assert_completed(path: Path, reply: Reply) -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup(short_timeouts()).await?;
    let upstream = start_upstream(path, reply).await?;
    let primary = provider(&gw, path, &upstream.url).await?;
    route(&gw, path, &primary, None).await?;
    let output = dispatch(&gw, path).await?;
    assert_eq!(output.status, StatusCode::OK, "{path:?}: {}", output.wire);
    assert!(
        output.elapsed > PHASE * 2,
        "stream must outlive both short first-byte/idle budgets"
    );
    assert!(
        !output.wire.contains("\"error\""),
        "{path:?}: {}",
        output.wire
    );
    assert!(
        !output.wire.to_ascii_lowercase().contains("timeout"),
        "{}",
        output.wire
    );
    let expected = if matches!(reply, Reply::HeartbeatsBeforeOutput) {
        "heartbeat survived".to_string()
    } else {
        (0..TICKS)
            .map(|index| format!("tick-{index:02} "))
            .collect::<String>()
    };
    if path == Path::ForceStreamNonstream {
        let value: Value = serde_json::from_str(&output.wire)?;
        assert_eq!(value["choices"][0]["message"]["content"], expected);
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    } else {
        if matches!(reply, Reply::HeartbeatsBeforeOutput) {
            assert_eq!(output.wire.matches(&expected).count(), 1, "{}", output.wire);
        } else {
            for word in expected.split_whitespace() {
                assert_eq!(
                    output.wire.matches(word).count(),
                    1,
                    "content lost/duplicated: {}",
                    output.wire
                );
            }
        }
        let terminal = if path == Path::AnthropicCompat {
            "event: message_stop"
        } else {
            "data: [DONE]"
        };
        assert_eq!(output.wire.matches(terminal).count(), 1, "{}", output.wire);
    }
    assert_eq!(upstream.calls(), 1);
    if path == Path::ForceStreamNonstream {
        let requests = upstream.requests.lock().unwrap();
        // The antigravity vendor owns the final URL action and may retain
        // :generateContent even when the dispatcher forces SSE aggregation.
        // A non-stream ingress receiving this fixture's enveloped SSE as a
        // complete JSON response (asserted above), plus native_ir trace, is
        // the handler-level proof; do not infer that handler from the action.
        assert!(
            requests[0].line.starts_with("POST /v1internal:"),
            "subscription URL: {:?}",
            requests[0]
        );
        assert!(
            requests[0].body["request"].is_object(),
            "v1internal envelope: {:?}",
            requests[0]
        );
    }
    let row = one_log(&mut logs, &output, "completed").await?;
    assert_eq!(row.upstream_status_code, Some(200));
    assert!(row.diagnostic.failure_kind.is_none(), "{row:?}");
    assert!(row.diagnostic.failure_stage.is_none(), "{row:?}");
    assert!(row.diagnostic.error_causes.is_empty(), "{row:?}");
    Ok(())
}

#[tokio::test]
async fn active_streams_outlive_first_byte_and_idle_budgets_on_all_paths() -> anyhow::Result<()> {
    for path in ALL_PATHS {
        assert_completed(path, Reply::Active).await?;
    }
    Ok(())
}

#[tokio::test]
async fn raw_heartbeats_keep_precommit_and_aggregation_alive_without_output() -> anyhow::Result<()>
{
    for path in ALL_PATHS {
        assert_completed(path, Reply::HeartbeatsBeforeOutput).await?;
    }
    Ok(())
}

async fn assert_stall(path: Path, reply: Reply, stage: &str, fallback: bool) -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup(short_timeouts()).await?;
    let upstream = start_upstream(path, reply).await?;
    let healthy = start_upstream(path, Reply::HealthyFallback).await?;
    let primary = provider(&gw, path, &upstream.url).await?;
    let fallback_id = if fallback {
        Some(provider(&gw, path, &healthy.url).await?)
    } else {
        None
    };
    route(&gw, path, &primary, fallback_id.as_deref()).await?;
    let output = dispatch(&gw, path).await?;
    assert_timeout_response(path, &output);
    assert!(
        output.elapsed >= PHASE,
        "phase timer must be exercised, not an immediate decode error"
    );
    if fallback {
        assert_eq!(
            output.status,
            StatusCode::OK,
            "already committed response: {}",
            output.wire
        );
        assert_eq!(
            output.wire.matches("partial output").count(),
            1,
            "partial output must survive: {}",
            output.wire
        );
    }
    assert_eq!(
        upstream.calls(),
        1,
        "{path:?}/{reply:?}: no same-target retry"
    );
    assert_eq!(
        healthy.calls(),
        0,
        "{path:?}/{reply:?}: committed output must never fail over"
    );
    assert!(!output.wire.contains("unexpected fallback answer"));
    let row = one_log(&mut logs, &output, "timed_out").await?;
    assert_eq!(row.provider_id, primary);
    assert_eq!(
        row.diagnostic.failure_kind.as_deref(),
        Some("timeout"),
        "{row:?}"
    );
    assert_eq!(
        row.diagnostic.failure_stage.as_deref(),
        Some(stage),
        "{path:?}: {row:?}"
    );
    assert!(row.diagnostic.error_message.is_some(), "{row:?}");
    assert_eq!(
        row.upstream_status_code,
        if matches!(reply, Reply::NoHeaders) {
            None
        } else {
            Some(200)
        }
    );
    Ok(())
}

#[tokio::test]
async fn no_response_headers_times_out_on_all_paths() -> anyhow::Result<()> {
    for path in ALL_PATHS {
        assert_stall(path, Reply::NoHeaders, "upstream_headers", false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn headers_without_first_body_byte_times_out_on_all_paths() -> anyhow::Result<()> {
    for path in ALL_PATHS {
        assert_stall(path, Reply::NoFirstByte, "upstream_first_byte", false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn partial_output_then_idle_emits_timeout_without_retry_or_fallback() -> anyhow::Result<()> {
    for path in STREAM_PATHS {
        assert_stall(path, Reply::PartialThenIdle, "upstream_idle", true).await?;
    }
    Ok(())
}

#[tokio::test]
async fn force_stream_aggregation_idle_fails_instead_of_partial_or_empty_success()
-> anyhow::Result<()> {
    assert_stall(
        Path::ForceStreamNonstream,
        Reply::PartialThenIdle,
        "upstream_idle",
        false,
    )
    .await
}

#[tokio::test]
async fn tls_connect_budget_is_separate_from_response_headers() -> anyhow::Result<()> {
    let timeouts = UpstreamTimeouts {
        connect: Duration::from_millis(100),
        response_headers: Duration::from_secs(1),
        ..short_timeouts()
    };
    let (_dir, gw, _logs) = setup(timeouts).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("https://{}", listener.local_addr()?);
    let (hello_tx, hello_rx) = tokio::sync::oneshot::channel();
    let _server = AbortOnDrop(tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut hello = [0_u8; 4096];
        let count = socket.read(&mut hello).await.unwrap();
        let _ = hello_tx.send(count);
        // Accept TCP and ClientHello, but never send a TLS server handshake.
        // No certificate or outside network is involved in this connect stall.
        let mut end = [0_u8; 1];
        while socket.read(&mut end).await.unwrap_or(0) != 0 {}
    }));
    let client =
        ProxyClient::new(gw.inference_client_for_provider(false).await?).with_timeouts(timeouts);
    let started = Instant::now();
    let error = match tokio::time::timeout(
        WATCHDOG,
        client.call_stream_raw(
            &url,
            reqwest::header::HeaderMap::new(),
            Bytes::from_static(b"{}"),
        ),
    )
    .await?
    {
        Ok(_) => panic!("a silent TLS peer must not connect successfully"),
        Err(error) => error,
    };
    assert!(
        tokio::time::timeout(WATCHDOG, hello_rx).await?? > 0,
        "client must reach loopback TLS peer"
    );
    let error = error
        .downcast_ref::<UpstreamReadError>()
        .expect("typed upstream error");
    assert!(error.is_timeout(), "{error:?}");
    assert_eq!(
        error.stage(),
        "upstream_connect",
        "header watchdog must not win: {error:?}"
    );
    assert!(matches!(error, UpstreamReadError::Transport(error) if error.is_connect()));
    assert!(
        started.elapsed() < timeouts.response_headers,
        "connect timeout must precede header budget"
    );
    Ok(())
}
