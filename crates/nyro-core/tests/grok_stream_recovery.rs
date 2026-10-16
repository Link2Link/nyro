//! Grok native Responses recovery through real loopback HTTP and dispatch_pipeline.
//! All IDs, credentials, payloads and URLs are synthetic. A missing HTTP chunk
//! terminator deliberately produces a reqwest read error, not a clean SSE EOF.
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::http::{HeaderMap, StatusCode};
use nyro_core::{
    Gateway,
    config::{GatewayConfig, UpstreamTimeouts},
    db::models::{CreateModel, CreateModelBackend, CreateProvider},
    logging::LogEntry,
    protocol::{ids::OPENAI_RESPONSES_V1, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const WATCHDOG: Duration = Duration::from_secs(10);
const MODEL: &str = "grok-fixture";

struct AbortOnDrop<T>(JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy, Debug)]
enum Reply {
    HalfCreated,
    CreatedThenBroken,
    CreatedSplit,
    Healthy,
    Http502,
    SemanticError,
    InvalidJson,
    CleanEof,
    Oversized,
    Timeout,
    Heartbeats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Captured {
    line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Upstream {
    url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    errors: Arc<Mutex<Vec<String>>>,
    release: Option<oneshot::Sender<()>>,
    _task: AbortOnDrop<()>,
}
impl Upstream {
    fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
    fn assert_clean(&self) {
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "fixture server errors: {errors:?}");
    }
}

async fn upstream(script: Vec<Reply>) -> anyhow::Result<Upstream> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let (release, receiver) = oneshot::channel();
    let mut receiver = Some(receiver);
    let captured = requests.clone();
    let failures = errors.clone();
    let task = AbortOnDrop(tokio::spawn(async move {
        let mut connections = Vec::new();
        let mut index = 0;
        while let Ok((socket, _)) = listener.accept().await {
            let reply = script[index.min(script.len() - 1)];
            index += 1;
            let hold = if matches!(reply, Reply::CreatedThenBroken) {
                receiver.take()
            } else {
                None
            };
            let captured = captured.clone();
            let failures = failures.clone();
            connections.push(AbortOnDrop(tokio::spawn(async move {
                if let Err(error) = serve(socket, reply, hold, captured).await {
                    failures.lock().unwrap().push(error.to_string());
                }
            })));
        }
    }));
    Ok(Upstream {
        url,
        requests,
        errors,
        release: Some(release),
        _task: task,
    })
}

async fn read_request(socket: &mut TcpStream) -> anyhow::Result<Captured> {
    let mut input = Vec::new();
    let mut buf = [0; 4096];
    let header_end = loop {
        if let Some(start) = input.windows(4).position(|window| window == b"\r\n\r\n") {
            break start + 4;
        }
        let n = socket.read(&mut buf).await?;
        anyhow::ensure!(
            n > 0 && input.len() + n < 1024 * 1024,
            "invalid request headers"
        );
        input.extend_from_slice(&buf[..n]);
    };
    let headers = std::str::from_utf8(&input[..header_end])?;
    let line = headers.lines().next().unwrap_or_default().to_string();
    anyhow::ensure!(
        line.starts_with("POST ") && line.contains("/responses "),
        "unexpected request: {line}"
    );
    let headers: Vec<(String, String)> = headers
        .lines()
        .skip(1)
        .filter_map(|line| {
            line.split_once(':')
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .ok_or_else(|| anyhow::anyhow!("fixture requires content-length"))?
        .1
        .parse()?;
    anyhow::ensure!(length < 1024 * 1024, "fixture request too large");
    while input.len() < header_end + length {
        let n = socket.read(&mut buf).await?;
        anyhow::ensure!(n > 0, "request body truncated");
        input.extend_from_slice(&buf[..n]);
    }
    Ok(Captured {
        line,
        headers,
        body: input[header_end..header_end + length].to_vec(),
    })
}

async fn chunk(socket: &mut TcpStream, data: &[u8]) -> anyhow::Result<()> {
    socket
        .write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    socket.write_all(data).await?;
    socket.write_all(b"\r\n").await?;
    Ok(())
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn created() -> String {
    event(
        json!({"type":"response.created","response":{"id":"resp_synthetic","object":"response","status":"in_progress","output":[]}}),
    )
}
fn healthy() -> String {
    format!(
        "{}{}",
        created(),
        event(json!({"type":"response.completed","response":{
        "id":"resp_synthetic","object":"response","status":"completed","model":MODEL,
        "output":[{"id":"msg_synthetic","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"synthetic recovered answer","annotations":[]}]}],
        "usage":{"input_tokens":5,"output_tokens":4,"total_tokens":9}}}))
    )
}
fn half_created() -> Vec<u8> {
    // Incident-sized first transfer chunk: syntactically unfinished response.created.
    let mut bytes = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_synthetic\",\"padding\":\"".to_vec();
    bytes.resize(8186, b'x');
    assert_eq!(bytes.len(), 8186);
    bytes
}

async fn serve(
    mut socket: TcpStream,
    reply: Reply,
    hold: Option<oneshot::Receiver<()>>,
    requests: Arc<Mutex<Vec<Captured>>>,
) -> anyhow::Result<()> {
    socket.set_nodelay(true)?;
    let request = read_request(&mut socket).await?;
    requests.lock().unwrap().push(request);
    let status = if matches!(reply, Reply::Http502) {
        "502 Bad Gateway"
    } else {
        "200 OK"
    };
    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    match reply {
        Reply::HalfCreated => chunk(&mut socket, &half_created()).await?,
        Reply::CreatedThenBroken => {
            chunk(&mut socket, created().as_bytes()).await?;
            hold.ok_or_else(|| anyhow::anyhow!("missing release handshake"))?
                .await?;
        }
        Reply::CreatedSplit => {
            let wire = healthy();
            let split = created().len() / 2;
            chunk(&mut socket, &wire.as_bytes()[..split]).await?;
            chunk(&mut socket, &wire.as_bytes()[split..]).await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::Healthy => {
            chunk(&mut socket, healthy().as_bytes()).await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::Http502 => {
            chunk(
                &mut socket,
                b"{\"error\":{\"message\":\"synthetic bad gateway\"}}",
            )
            .await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::SemanticError => {
            chunk(&mut socket, event(json!({"type":"error","error":{"type":"invalid_request_error","message":"synthetic semantic error"}})).as_bytes()).await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::InvalidJson => {
            chunk(
                &mut socket,
                b"event: response.created\ndata: {not-json}\n\n",
            )
            .await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::CleanEof => {
            chunk(
                &mut socket,
                b"event: response.created\ndata: {\"type\":\"response.created\"",
            )
            .await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::Oversized => {
            chunk(&mut socket, &vec![b' '; 256 * 1024]).await?;
            socket.write_all(b"0\r\n\r\n").await?;
        }
        Reply::Heartbeats => {
            // Active keepalives defeat an idle-byte timeout but must not defeat
            // the bounded first-complete-event probe.
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            loop {
                interval.tick().await;
                if chunk(&mut socket, b": heartbeat\n\n").await.is_err() {
                    return Ok(());
                }
            }
        }
        Reply::Timeout => {
            // Hold the socket open without sleeping; the client's first-byte
            // watchdog closes it. The outer task aborts this connection on drop.
            let mut byte = [0];
            let _ = socket.read(&mut byte).await;
            return Ok(());
        }
    }
    socket.shutdown().await?;
    Ok(())
}

async fn setup() -> anyhow::Result<(tempfile::TempDir, Gateway, mpsc::Receiver<LogEntry>)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        upstream_timeouts: UpstreamTimeouts {
            connect: WATCHDOG,
            response_headers: WATCHDOG,
            first_byte: Duration::from_millis(180),
            stream_idle: WATCHDOG,
        },
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, logs) = Gateway::from_storage(config, storage).await?;
    Ok((dir, gw, logs))
}
async fn provider(gw: &Gateway, url: &str) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            keys: vec![],
            name: uuid::Uuid::new_v4().to_string(),
            vendor: Some("xai".into()),
            protocol: OPENAI_RESPONSES_V1.to_string(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: Some("grok".into()),
            models_source: None,
            static_models: None,
            api_key: "synthetic-local-only-token".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}
async fn route(gw: &Gateway, primary: &str, fallback: Option<&str>) -> anyhow::Result<()> {
    let targets = fallback
        .map(|secondary| {
            vec![
                CreateModelBackend {
                    provider_id: primary.into(),
                    model: MODEL.into(),
                    weight: Some(100),
                    priority: Some(1),
                    is_fallback: Some(false),
                },
                CreateModelBackend {
                    provider_id: secondary.into(),
                    model: MODEL.into(),
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
            target_model: MODEL.into(),
            targets,
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}
fn request() -> Value {
    json!({"model":MODEL,"stream":true,"store":false,"input":"synthetic question",
        "tools":[{"type":"function","name":"fixture_lookup","parameters":{"type":"object","properties":{}}},
            {"type":"custom","name":"fixture_exec","format":{"type":"text"}}]})
}
struct Output {
    id: String,
    status: StatusCode,
    wire: String,
}
async fn dispatch(
    gw: &Gateway,
    body: Value,
    release: Option<oneshot::Sender<()>>,
) -> anyhow::Result<Output> {
    let ingress = OPENAI_RESPONSES_V1;
    let decoded = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let ctx = RequestContext::new(ingress, WATCHDOG);
    let id = ctx.request_id.clone();
    let response = tokio::time::timeout(
        WATCHDOG,
        dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            RawEnvelope::new(Some(body), Default::default(), "POST", "/v1/responses"),
            decoded,
            ingress,
            ctx,
        ),
    )
    .await?;
    let status = response.status();
    if let Some(release) = release {
        assert_eq!(
            status,
            StatusCode::OK,
            "complete response.created must commit before socket closes"
        );
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("fixture release receiver closed"))?;
    }
    let bytes = tokio::time::timeout(
        WATCHDOG,
        axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024),
    )
    .await??;
    Ok(Output {
        id,
        status,
        wire: String::from_utf8(bytes.to_vec())?,
    })
}
async fn rows(rx: &mut mpsc::Receiver<LogEntry>, count: usize, id: &str) -> Vec<LogEntry> {
    let mut rows = Vec::new();
    for _ in 0..count {
        let row = tokio::time::timeout(WATCHDOG, rx.recv())
            .await
            .expect("log watchdog")
            .expect("log channel");
        assert_eq!(row.diagnostic.client_request_id.as_deref(), Some(id));
        rows.push(row);
    }
    rows.sort_by_key(|row| row.diagnostic.attempt_index);
    let mut ids = HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.diagnostic.attempt_index, Some(index as i32 + 1));
        assert!(
            ids.insert(&row.diagnostic.log_id),
            "attempts have independent log identities"
        );
        if index + 1 < rows.len() {
            assert!(row.diagnostic.final_result.is_none());
        }
    }
    let final_row = rows.last().unwrap();
    let final_result = final_row
        .diagnostic
        .final_result
        .as_ref()
        .expect("final request result");
    assert_eq!(final_result.attempt_count, count as i32);
    assert_eq!(
        final_result.final_attempt_id.as_deref(),
        Some(final_row.diagnostic.log_id.as_str())
    );
    assert!(rx.try_recv().is_err(), "unexpected extra attempt log");
    rows
}
fn assert_failed_read(row: &LogEntry) {
    assert_eq!(row.upstream_status_code, Some(200));
    assert_eq!(row.client_status_code, 502);
    assert_eq!(row.diagnostic.attempt_outcome, "failed");
    assert_eq!(row.performance.completion, "failed");
    assert_eq!(
        row.diagnostic.failure_kind.as_deref(),
        Some("upstream_read_error")
    );
}
fn assert_same_prepared_request(requests: &[Captured]) -> anyhow::Result<()> {
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "retry must reuse identical prepared body and headers"
    );
    let body: Value = serde_json::from_slice(&requests[0].body)?;
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body.get("previous_response_id").is_none());
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer synthetic-local-only-token")
    );
    Ok(())
}

#[tokio::test]
async fn broken_8186_byte_created_retries_same_prepared_request_once() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let server = upstream(vec![Reply::HalfCreated, Reply::Healthy]).await?;
    route(&gw, &provider(&gw, &server.url).await?, None).await?;
    let output = dispatch(&gw, request(), None).await?;
    assert_eq!(output.status, StatusCode::OK, "{}", output.wire);
    assert!(output.wire.contains("synthetic recovered answer"));
    assert!(
        !output.wire.contains(&"x".repeat(32)),
        "partial event leaked"
    );
    assert_same_prepared_request(&server.requests())?;
    let rows = rows(&mut logs, 2, &output.id).await;
    assert_failed_read(&rows[0]);
    assert_eq!(
        rows[0].upstream_response_body.as_deref().map(str::len),
        Some(8186)
    );
    assert_eq!(
        rows[0].diagnostic.error_message.as_deref(),
        Some("Failed to read the upstream response")
    );
    assert_eq!(rows[1].diagnostic.attempt_outcome, "completed");
    assert_eq!(rows[1].client_status_code, 200);
    server.assert_clean();
    Ok(())
}

#[tokio::test]
async fn two_broken_8186_byte_created_events_exhaust_budget_and_fail_over() -> anyhow::Result<()> {
    for fallback in [false, true] {
        let (_dir, gw, mut logs) = setup().await?;
        let broken = upstream(vec![Reply::HalfCreated, Reply::HalfCreated, Reply::Healthy]).await?;
        let next = upstream(vec![Reply::Healthy]).await?;
        let primary = provider(&gw, &broken.url).await?;
        let secondary = if fallback {
            Some(provider(&gw, &next.url).await?)
        } else {
            None
        };
        route(&gw, &primary, secondary.as_deref()).await?;
        let output = dispatch(&gw, request(), None).await?;
        assert_eq!(
            broken.requests().len(),
            2,
            "healthy third response must not be fetched"
        );
        assert_same_prepared_request(&broken.requests())?;
        let mut expected = 2;
        if fallback {
            expected = 3;
            assert_eq!(output.status, StatusCode::OK, "{}", output.wire);
            assert_eq!(next.requests().len(), 1);
            assert!(output.wire.contains("synthetic recovered answer"));
        } else {
            assert_eq!(output.status, StatusCode::BAD_GATEWAY, "{}", output.wire);
            assert!(next.requests().is_empty());
        }
        let rows = rows(&mut logs, expected, &output.id).await;
        assert_failed_read(&rows[0]);
        assert_failed_read(&rows[1]);
        if fallback {
            assert_eq!(rows[2].diagnostic.attempt_outcome, "completed");
        }
        broken.assert_clean();
        next.assert_clean();
    }
    Ok(())
}

#[tokio::test]
async fn complete_created_commits_then_broken_chunk_is_an_error_not_a_replay() -> anyhow::Result<()>
{
    let (_dir, gw, mut logs) = setup().await?;
    let mut server = upstream(vec![Reply::CreatedThenBroken, Reply::Healthy]).await?;
    route(&gw, &provider(&gw, &server.url).await?, None).await?;
    let output = dispatch(&gw, request(), server.release.take()).await?;
    assert_eq!(output.status, StatusCode::OK, "{}", output.wire);
    assert!(
        output.wire.contains("event: response.created"),
        "{}",
        output.wire
    );
    assert!(
        output.wire.contains("upstream_read_error"),
        "{}",
        output.wire
    );
    assert!(
        !output.wire.contains("response.completed"),
        "{}",
        output.wire
    );
    assert_eq!(server.requests().len(), 1, "committed stream cannot replay");
    let rows = rows(&mut logs, 1, &output.id).await;
    assert_eq!(rows[0].client_status_code, 200);
    assert_eq!(rows[0].upstream_status_code, Some(200));
    assert_eq!(
        rows[0].diagnostic.failure_kind.as_deref(),
        Some("upstream_read_error")
    );
    server.assert_clean();
    Ok(())
}

#[tokio::test]
async fn split_complete_created_preserves_event_once_without_replay() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = setup().await?;
    let server = upstream(vec![Reply::CreatedSplit, Reply::Healthy]).await?;
    route(&gw, &provider(&gw, &server.url).await?, None).await?;
    let output = dispatch(&gw, request(), None).await?;
    assert_eq!(output.status, StatusCode::OK, "{}", output.wire);
    assert_eq!(
        output.wire.matches("event: response.created").count(),
        1,
        "{}",
        output.wire
    );
    assert_eq!(
        output.wire.matches("event: response.completed").count(),
        1,
        "{}",
        output.wire
    );
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        rows(&mut logs, 1, &output.id).await[0]
            .diagnostic
            .attempt_outcome,
        "completed"
    );
    server.assert_clean();
    Ok(())
}

#[tokio::test]
async fn non_transport_first_event_failures_never_retry() -> anyhow::Result<()> {
    for first in [
        Reply::Http502,
        Reply::SemanticError,
        Reply::InvalidJson,
        Reply::CleanEof,
        Reply::Oversized,
        Reply::Timeout,
        Reply::Heartbeats,
    ] {
        let (_dir, gw, mut logs) = setup().await?;
        let server = upstream(vec![first, Reply::Healthy]).await?;
        route(&gw, &provider(&gw, &server.url).await?, None).await?;
        let output = dispatch(&gw, request(), None).await?;
        if matches!(first, Reply::SemanticError) {
            assert!(
                output.wire.contains("synthetic semantic error"),
                "{}",
                output.wire
            );
        } else {
            assert_ne!(
                output.status,
                StatusCode::OK,
                "{first:?} unexpectedly succeeded: {}",
                output.wire
            );
        }
        assert_eq!(
            server.requests().len(),
            1,
            "{first:?} retried despite no transport read error"
        );
        let rows = rows(&mut logs, 1, &output.id).await;
        if matches!(first, Reply::Timeout | Reply::Heartbeats) {
            assert_eq!(rows[0].diagnostic.failure_kind.as_deref(), Some("timeout"));
            assert_eq!(output.status, StatusCode::GATEWAY_TIMEOUT);
        }
        assert_ne!(
            rows[0].diagnostic.failure_kind.as_deref(),
            Some("upstream_read_error"),
            "{first:?}"
        );
        server.assert_clean();
    }
    Ok(())
}

#[tokio::test]
async fn stateful_or_server_tool_requests_never_replay_broken_created() -> anyhow::Result<()> {
    let mut cases = Vec::new();
    let mut stored = request();
    stored["store"] = json!(true);
    cases.push(("store_true", stored));
    let mut omitted = request();
    omitted.as_object_mut().unwrap().remove("store");
    cases.push(("store_omitted", omitted));
    let mut previous = request();
    previous["previous_response_id"] = json!("resp_synthetic_prior");
    cases.push(("previous_response_id", previous));
    let mut server_tool = request();
    server_tool["tools"] = json!([{"type":"web_search_preview"}]);
    cases.push(("server_tool", server_tool));
    let mut background = request();
    background["background"] = json!(true);
    cases.push(("background", background));
    let mut conversation = request();
    conversation["conversation"] = json!("conv_synthetic");
    cases.push(("conversation", conversation));
    for (name, body) in cases {
        let (_dir, gw, mut logs) = setup().await?;
        let server = upstream(vec![Reply::HalfCreated, Reply::Healthy]).await?;
        let fallback = upstream(vec![Reply::Healthy]).await?;
        let primary = provider(&gw, &server.url).await?;
        let secondary = provider(&gw, &fallback.url).await?;
        route(&gw, &primary, Some(&secondary)).await?;
        let output = dispatch(&gw, body, None).await?;
        assert!(
            fallback.requests().is_empty(),
            "{name}: unsafe fallback replay"
        );
        assert_eq!(
            output.status,
            StatusCode::BAD_GATEWAY,
            "{name}: {}",
            output.wire
        );
        assert_eq!(
            server.requests().len(),
            1,
            "{name}: unsafe request replayed"
        );
        assert_failed_read(&rows(&mut logs, 1, &output.id).await[0]);
        server.assert_clean();
    }
    Ok(())
}
