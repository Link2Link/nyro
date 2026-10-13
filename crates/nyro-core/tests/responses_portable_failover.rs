//! Portable Responses history exercised through real loopback sockets and the
//! dispatcher, not a direct call to the compatibility transformer. Every tool,
//! message, credential and opaque reasoning value below is synthetic.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::http::{HeaderMap, StatusCode};
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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};

const WATCHDOG: Duration = Duration::from_secs(10);
const UPSTREAM_MODEL: &str = "responses-fixture";
const CUSTOM_CALL: &str = "call_fixture_custom";
const NEXT_CALL: &str = "call_fixture_next";

struct AbortOnDrop<T>(JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy)]
enum Reply {
    /// Consume the request and close without response headers. This exercises a
    /// real transport failure while retaining the primary attempt's wire body.
    CloseBeforeHeaders,
    StrictThirdParty,
    NativeCompleted,
    NativePartialThenClose,
}

struct Upstream {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    errors: Arc<Mutex<Vec<String>>>,
    _task: AbortOnDrop<()>,
}

impl Upstream {
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn assert_healthy_fixture(&self) {
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "mock server errors: {errors:?}");
    }
}

async fn start_upstream(reply: Reply) -> anyhow::Result<Upstream> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let failures = errors.clone();
    let task = AbortOnDrop(tokio::spawn(async move {
        let mut connections = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            let captured = captured.clone();
            let failures = failures.clone();
            connections.push(AbortOnDrop(tokio::spawn(async move {
                if let Err(error) = serve(socket, reply, captured).await {
                    failures.lock().unwrap().push(error.to_string());
                }
            })));
        }
    }));
    Ok(Upstream {
        url,
        requests,
        errors,
        _task: task,
    })
}

async fn read_request(socket: &mut TcpStream) -> anyhow::Result<Value> {
    let mut input = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        if let Some(offset) = input.windows(4).position(|w| w == b"\r\n\r\n") {
            break offset + 4;
        }
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "request closed before headers");
        input.extend_from_slice(&buffer[..count]);
        anyhow::ensure!(input.len() < 1024 * 1024, "fixture request too large");
    };
    let headers = std::str::from_utf8(&input[..header_end])?;
    let request_line = headers.lines().next().unwrap_or_default();
    anyhow::ensure!(
        request_line.starts_with("POST ") && request_line.contains("/responses "),
        "unexpected route: {request_line}"
    );
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("fixture expects content-length"))?;
    anyhow::ensure!(length < 1024 * 1024, "fixture request body too large");
    while input.len() < header_end + length {
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "request closed before body");
        input.extend_from_slice(&buffer[..count]);
    }
    Ok(serde_json::from_slice(
        &input[header_end..header_end + length],
    )?)
}

async fn serve(
    mut socket: TcpStream,
    reply: Reply,
    requests: Arc<Mutex<Vec<Value>>>,
) -> anyhow::Result<()> {
    let body = read_request(&mut socket).await?;
    requests.lock().unwrap().push(body.clone());
    if matches!(reply, Reply::CloseBeforeHeaders) {
        socket.shutdown().await?;
        return Ok(());
    }
    if matches!(reply, Reply::StrictThirdParty)
        && let Err(error) = validate_portable_request(&body)
    {
        // A bad conversion is an actual HTTP 400, not an assertion hidden in a
        // detached server task. The dispatcher test must observe the failure.
        let response =
            json!({"error":{"type":"invalid_request_error","message":error.to_string()}})
                .to_string();
        write_response(
            &mut socket,
            "400 Bad Request",
            "application/json",
            &response,
            false,
        )
        .await?;
        return Ok(());
    }
    let wire = match reply {
        Reply::StrictThirdParty if !has_output(&body, NEXT_CALL) => tool_sse(),
        Reply::NativePartialThenClose => partial_text_sse(),
        _ => completed_text_sse(),
    };
    write_response(
        &mut socket,
        "200 OK",
        "text/event-stream",
        &wire,
        matches!(reply, Reply::NativePartialThenClose),
    )
    .await
}

async fn write_response(
    socket: &mut TcpStream,
    status: &str,
    content_type: &str,
    wire: &str,
    incomplete: bool,
) -> anyhow::Result<()> {
    // A longer declared body produces a deterministic read error after the
    // payload, rather than a clean EOF or a timeout that depends on scheduling.
    let length = wire.len() + if incomplete { 100 } else { 0 };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    );
    socket.write_all(headers.as_bytes()).await?;
    socket.write_all(wire.as_bytes()).await?;
    socket.shutdown().await?;
    Ok(())
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn tool_sse() -> String {
    let arguments = json!({"input":"fixture_next()"}).to_string();
    let item = json!({"id":"fc_fixture_next","type":"function_call","call_id":NEXT_CALL,
        "name":"exec","arguments":arguments,"status":"completed"});
    [
        event(json!({"type":"response.created","response":{"id":"resp_fixture_tool","status":"in_progress","output":[]}})),
        event(json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"fc_fixture_next","type":"function_call","call_id":NEXT_CALL,"name":"exec","arguments":"","status":"in_progress"}})),
        event(json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture_next","delta":arguments})),
        event(json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_fixture_next","arguments":arguments})),
        event(json!({"type":"response.output_item.done","output_index":0,"item":item})),
        event(json!({"type":"response.completed","response":{"id":"resp_fixture_tool","object":"response",
            "model":UPSTREAM_MODEL,"status":"completed","output":[item],
            "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}}})),
    ].concat()
}

fn partial_text_sse() -> String {
    [
        event(json!({"type":"response.created","response":{"id":"resp_fixture_text","status":"in_progress","output":[]}})),
        event(json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"msg_fixture","type":"message","role":"assistant","content":[],"status":"in_progress"}})),
        event(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,
            "item_id":"msg_fixture","delta":"fixture answer"})),
    ].concat()
}

fn completed_text_sse() -> String {
    let mut wire = partial_text_sse();
    wire.push_str(&event(
        json!({"type":"response.completed","response":{"id":"resp_fixture_text",
        "object":"response","model":UPSTREAM_MODEL,"status":"completed","output":[{
            "id":"msg_fixture","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"fixture answer","annotations":[]}]}],
        "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}}}),
    ));
    wire
}

fn custom_output() -> Value {
    json!([{"type":"input_text","text":"synthetic result; literal ctco_do_not_rewrite in tool output"}])
}

fn fixture(model: &str) -> Value {
    // Native tool normalization already defaults function tools to strict=false;
    // spell it out so exact input equality below isolates portable mutations.
    json!({
        "model":model,"store":false,"stream":true,
        "input":[
            {"type":"additional_tools","tools":[
                {"type":"custom","name":"exec","description":"Synthetic fixture tool"},
                {"type":"function","name":"lookup","strict":false,"parameters":{"type":"object","properties":{}}}
            ]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Continue the synthetic tool session."}]},
            {"type":"reasoning","id":format!("rs_{}", "a".repeat(64)),"summary":[],
                "encrypted_content":"fixture-opaque-not-a-real-ciphertext"},
            {"type":"custom_tool_call","id":"ctc_fixture_old","call_id":CUSTOM_CALL,"name":"exec","input":"fixture_old()"},
            {"type":"custom_tool_call_output","id":"ctco_fixture_old","call_id":CUSTOM_CALL,"output":custom_output()},
            {"type":"function_call","id":"fc_fixture_plain","call_id":"call_fixture_plain","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","id":"fco_fixture_plain","call_id":"call_fixture_plain","output":"plain fixture result"}
        ]
    })
}

fn has_output(body: &Value, call_id: &str) -> bool {
    body["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            ) && item["call_id"] == call_id
        })
    })
}

fn validate_portable_request(body: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(body["store"] == false && body["stream"] == true);
    let items = body["input"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing input"))?;
    anyhow::ensure!(
        !items.iter().any(|item| matches!(
            item["type"].as_str(),
            Some("additional_tools" | "custom_tool_call" | "custom_tool_call_output")
        )),
        "private item leaked"
    );
    anyhow::ensure!(
        !items
            .iter()
            .any(|item| item.get("encrypted_content").is_some()),
        "opaque reasoning leaked"
    );
    anyhow::ensure!(
        !items.iter().any(|item| item["type"] == "reasoning"),
        "opaque reasoning shell retained"
    );
    let tools = body["tools"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("carrier not lifted"))?;
    anyhow::ensure!(tools.len() == 2 && tools.iter().all(|tool| tool["type"] == "function"));
    for id in [CUSTOM_CALL, NEXT_CALL] {
        for item in items.iter().filter(|item| item["call_id"] == id) {
            anyhow::ensure!(
                item.get("id").is_none(),
                "converted item retains its optional id"
            );
        }
    }
    for item in items
        .iter()
        .filter(|item| item["type"] == "function_call_output")
    {
        anyhow::ensure!(
            items
                .iter()
                .any(|call| call["type"] == "function_call" && call["call_id"] == item["call_id"]),
            "orphan output"
        );
    }
    let old_call = items
        .iter()
        .find(|item| item["type"] == "function_call" && item["call_id"] == CUSTOM_CALL)
        .ok_or_else(|| anyhow::anyhow!("custom call lost"))?;
    let arguments: Value =
        serde_json::from_str(old_call["arguments"].as_str().unwrap_or_default())?;
    anyhow::ensure!(arguments == json!({"input":"fixture_old()"}));
    let old_output = items
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == CUSTOM_CALL)
        .ok_or_else(|| anyhow::anyhow!("custom output lost"))?;
    anyhow::ensure!(
        old_output["output"] == custom_output(),
        "tool output changed"
    );
    anyhow::ensure!(
        items
            .iter()
            .any(|item| item["id"] == "fc_fixture_plain" && item["type"] == "function_call"),
        "ordinary call id changed"
    );
    anyhow::ensure!(
        items
            .iter()
            .any(|item| item["id"] == "fco_fixture_plain"
                && item["output"] == "plain fixture result"),
        "ordinary output changed"
    );
    if has_output(body, NEXT_CALL) {
        let next = items
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == NEXT_CALL)
            .unwrap();
        anyhow::ensure!(next["output"] == "next synthetic result");
        let call = items
            .iter()
            .find(|item| item["type"] == "function_call" && item["call_id"] == NEXT_CALL)
            .ok_or_else(|| anyhow::anyhow!("follow-up call lost"))?;
        let args: Value = serde_json::from_str(call["arguments"].as_str().unwrap_or_default())?;
        anyhow::ensure!(
            args == json!({"input":"fixture_next()"}),
            "arguments wrapped twice"
        );
    }
    Ok(())
}

async fn gateway() -> anyhow::Result<(tempfile::TempDir, Gateway, mpsc::Receiver<LogEntry>)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gateway, logs) = Gateway::from_storage(config, storage).await?;
    Ok((dir, gateway, logs))
}

async fn provider(
    gw: &Gateway,
    name: &str,
    url: &str,
    native: Option<&str>,
) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            name: name.into(),
            vendor: Some(if native.is_some() { "openai" } else { "custom" }.into()),
            protocol: "openai-responses".into(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: native.map(str::to_owned),
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
    let targets = fallback
        .map(|fallback| {
            vec![
                CreateModelBackend {
                    provider_id: primary.into(),
                    model: UPSTREAM_MODEL.into(),
                    weight: Some(100),
                    priority: Some(1),
                    is_fallback: Some(false),
                },
                CreateModelBackend {
                    provider_id: fallback.into(),
                    model: UPSTREAM_MODEL.into(),
                    weight: Some(100),
                    priority: Some(2),
                    is_fallback: Some(true),
                },
            ]
        })
        .unwrap_or_default();
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

struct Output {
    id: String,
    status: StatusCode,
    events: Vec<Value>,
}

async fn dispatch(gw: &Gateway, original: &Value) -> anyhow::Result<Output> {
    let snapshot = original.clone();
    let request = OPENAI_RESPONSES_V1
        .handler()
        .make_request_decoder()
        .decode_request(original.clone())?;
    let ctx = RequestContext::new(OPENAI_RESPONSES_V1, WATCHDOG);
    let id = ctx.request_id.clone();
    let (status, wire) = tokio::time::timeout(WATCHDOG, async {
        let response = dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            RawEnvelope::new(
                Some(original.clone()),
                HashMap::new(),
                "POST",
                "/v1/responses",
            ),
            request,
            OPENAI_RESPONSES_V1,
            ctx,
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        Ok::<_, anyhow::Error>((status, String::from_utf8(body.to_vec())?))
    })
    .await??;
    assert_eq!(original, &snapshot, "caller-owned original request changed");
    anyhow::ensure!(
        status == StatusCode::OK,
        "unexpected response {status}: {wire}"
    );
    let events = wire
        .split("\n\n")
        .filter_map(|block| {
            let data = block
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");
            (!data.is_empty() && data != "[DONE]").then_some(data)
        })
        .map(|data| serde_json::from_str(&data))
        .collect::<Result<Vec<Value>, _>>()?;
    Ok(Output { id, status, events })
}

async fn logs_for(
    logs: &mut mpsc::Receiver<LogEntry>,
    output: &Output,
    original: &Value,
    count: usize,
) -> anyhow::Result<Vec<LogEntry>> {
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.push(
            tokio::time::timeout(WATCHDOG, logs.recv())
                .await?
                .ok_or_else(|| anyhow::anyhow!("log channel closed"))?,
        );
    }
    rows.sort_by_key(|row| row.diagnostic.attempt_index);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            row.diagnostic.client_request_id.as_deref(),
            Some(output.id.as_str())
        );
        assert_eq!(row.diagnostic.attempt_index, Some(index as i32 + 1));
        let captured: Value = serde_json::from_str(
            row.client_request_body
                .as_deref()
                .expect("payload capture enabled"),
        )?;
        assert_eq!(
            &captured, original,
            "attempt overwrote the original ingress body"
        );
    }
    assert_eq!(
        rows.iter()
            .filter(|row| row.diagnostic.final_result.is_some())
            .count(),
        1
    );
    assert!(logs.try_recv().is_err(), "unexpected extra attempt/log");
    Ok(rows)
}

fn single_event<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == kind)
        .collect();
    assert_eq!(matching.len(), 1, "expected exactly one {kind}: {events:?}");
    matching[0]
}

#[tokio::test]
async fn native_connect_failure_portable_fallback_tool_followup_and_native_return()
-> anyhow::Result<()> {
    let (_dir, gw, mut logs) = gateway().await?;
    let primary = start_upstream(Reply::CloseBeforeHeaders).await?;
    let secondary = start_upstream(Reply::StrictThirdParty).await?;
    // sub2api uses the same native Codex request contract without an OAuth
    // driver that could override the loopback URL with a production endpoint.
    let p = provider(&gw, "native primary", &primary.url, Some("sub2api")).await?;
    let s = provider(&gw, "strict fallback", &secondary.url, None).await?;
    route(&gw, "portable-failover", &p, Some(&s)).await?;
    route(&gw, "portable-followup", &s, None).await?;

    let original = fixture("portable-failover");
    let first = dispatch(&gw, &original).await?;
    assert_eq!(first.status, StatusCode::OK);
    let rows = logs_for(&mut logs, &first, &original, 2).await?;
    assert_eq!(rows[0].provider_id, p);
    assert_eq!(rows[0].diagnostic.attempt_outcome, "failed");
    assert_eq!(rows[0].client_status_code, 502);
    assert_eq!(rows[0].upstream_status_code, None);
    assert!(rows[0].diagnostic.final_result.is_none());
    assert_eq!(rows[1].provider_id, s);
    assert_eq!(rows[1].diagnostic.attempt_outcome, "completed");
    assert_ne!(rows[0].diagnostic.log_id, rows[1].diagnostic.log_id);
    let result = rows[1].diagnostic.final_result.as_ref().unwrap();
    assert_eq!(result.attempt_count, 2);
    assert_eq!(result.final_outcome, "completed");
    assert_eq!(
        result.final_attempt_id.as_deref(),
        Some(rows[1].diagnostic.log_id.as_str())
    );
    assert_eq!(primary.requests().len(), 1);
    assert_eq!(secondary.requests().len(), 1);
    assert_eq!(
        primary.requests()[0]["input"],
        original["input"],
        "native primary was polluted by the portable rewrite"
    );
    validate_portable_request(&secondary.requests()[0])?;

    let added = &single_event(&first.events, "response.output_item.added")["item"];
    let done = &single_event(&first.events, "response.output_item.done")["item"];
    let delta = single_event(&first.events, "response.custom_tool_call_input.delta");
    let input_done = single_event(&first.events, "response.custom_tool_call_input.done");
    let completed = single_event(&first.events, "response.completed");
    assert_eq!(added["type"], "custom_tool_call");
    assert_eq!(done["type"], "custom_tool_call");
    assert_eq!(done["name"], "exec");
    assert_eq!(done["call_id"], NEXT_CALL);
    assert_eq!(done["input"], "fixture_next()");
    assert_eq!(added["id"], done["id"]);
    assert_eq!(delta["item_id"], done["id"]);
    assert_eq!(input_done["item_id"], done["id"]);
    assert_eq!(delta["delta"], "fixture_next()");
    assert_eq!(input_done["input"], "fixture_next()");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["output"], json!([done]));
    assert!(
        !first
            .events
            .iter()
            .any(|event| event["type"] == "response.function_call_arguments.delta")
    );

    // The client replays the restored custom call plus its newly executed tool
    // result. This is a fresh request directly to the former fallback provider.
    let mut followup = original.clone();
    followup["model"] = json!("portable-followup");
    let input = followup["input"].as_array_mut().unwrap();
    input.push(done.clone());
    input.push(json!({"type":"custom_tool_call_output","id":"ctco_fixture_next","call_id":NEXT_CALL,"output":"next synthetic result"}));
    let second = dispatch(&gw, &followup).await?;
    let rows = logs_for(&mut logs, &second, &followup, 1).await?;
    assert_eq!(rows[0].diagnostic.attempt_outcome, "completed");
    single_event(&second.events, "response.completed");
    assert_eq!(
        primary.requests().len(),
        1,
        "direct fallback turn contacted primary"
    );
    assert_eq!(secondary.requests().len(), 2);
    validate_portable_request(&secondary.requests()[1])?;

    // Re-submit that original client history to both native consumer channels.
    // Explicit API-key fixture auth keeps these calls local even for codex.
    for channel in ["codex", "sub2api"] {
        let native = start_upstream(Reply::NativeCompleted).await?;
        let name = format!("native-return-{channel}");
        let n = provider(&gw, &name, &native.url, Some(channel)).await?;
        route(&gw, &name, &n, None).await?;
        let mut returned = followup.clone();
        returned["model"] = json!(name);
        let output = dispatch(&gw, &returned).await?;
        let rows = logs_for(&mut logs, &output, &returned, 1).await?;
        assert_eq!(rows[0].diagnostic.attempt_outcome, "completed");
        let requests = native.requests();
        assert_eq!(requests.len(), 1);
        let items = requests[0]["input"].as_array().unwrap();
        assert!(items.iter().any(|item| item["type"] == "additional_tools"));
        let call = items
            .iter()
            .find(|item| item["type"] == "custom_tool_call" && item["call_id"] == NEXT_CALL)
            .unwrap();
        assert!(
            call["id"].as_str().unwrap().starts_with("ctc"),
            "native prefix defense did not run"
        );
        assert_eq!(call["input"], "fixture_next()");
        let result = items
            .iter()
            .find(|item| item["type"] == "custom_tool_call_output" && item["call_id"] == NEXT_CALL)
            .unwrap();
        assert_eq!(result["id"], "ctco_fixture_next");
        assert_eq!(result["output"], "next synthetic result");
        assert!(
            items
                .iter()
                .any(|item| item["encrypted_content"] == "fixture-opaque-not-a-real-ciphertext"),
            "portable sanitizer escaped into native path"
        );
        native.assert_healthy_fixture();
    }
    primary.assert_healthy_fixture();
    secondary.assert_healthy_fixture();
    Ok(())
}

#[tokio::test]
async fn native_responses_partial_read_error_never_calls_fallback() -> anyhow::Result<()> {
    let (_dir, gw, mut logs) = gateway().await?;
    let primary = start_upstream(Reply::NativePartialThenClose).await?;
    let secondary = start_upstream(Reply::StrictThirdParty).await?;
    let p = provider(&gw, "partial native", &primary.url, Some("sub2api")).await?;
    let s = provider(&gw, "unused fallback", &secondary.url, None).await?;
    route(&gw, "portable-partial", &p, Some(&s)).await?;
    let original = fixture("portable-partial");
    let output = dispatch(&gw, &original).await?;
    assert_eq!(
        single_event(&output.events, "response.output_text.delta")["delta"],
        "fixture answer"
    );
    let error = single_event(&output.events, "error");
    assert_eq!(error["error"]["type"], "upstream_read_error");
    assert!(
        !output
            .events
            .iter()
            .any(|event| event["type"] == "response.completed")
    );
    let rows = logs_for(&mut logs, &output, &original, 1).await?;
    assert_eq!(rows[0].client_status_code, 200);
    assert_eq!(rows[0].upstream_status_code, Some(200));
    assert_eq!(rows[0].diagnostic.attempt_outcome, "failed");
    assert_eq!(
        rows[0].diagnostic.failure_kind.as_deref(),
        Some("upstream_read_error")
    );
    let result = rows[0].diagnostic.final_result.as_ref().unwrap();
    assert_eq!(result.final_outcome, "failed");
    assert_eq!(result.attempt_count, 1);
    assert_eq!(primary.requests().len(), 1);
    assert_eq!(
        secondary.requests().len(),
        0,
        "committed stream was replayed to fallback"
    );
    primary.assert_healthy_fixture();
    secondary.assert_healthy_fixture();
    Ok(())
}
