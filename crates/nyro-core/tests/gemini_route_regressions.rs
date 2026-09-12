//! Gemini schema/name regressions through the real dispatcher and local HTTP.
//!
//! These are deliberately route tests, not direct encoder calls: compat preview
//! selection, native encoding, passthrough, vendor URL/auth, and stream conversion
//! must cooperate. No fixture sets internal request metadata to choose a path.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{Model, Provider},
    protocol::{
        codec::google::gemini::decoder::GoogleDecoder,
        ids::{
            ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1, ProtocolId,
        },
        ir::RawEnvelope,
    },
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::MemoryStorage,
};
use serde_json::{Value, json};

const CLIENT_MODEL: &str = "gemini-route-test";
const UPSTREAM_MODEL: &str = "gemini-2.0-flash";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum Reply {
    Text,
    ParallelCalls,
    InvalidCall,
    ConflictingCall,
}

#[derive(Debug, Clone)]
struct Seen {
    uri: Uri,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone)]
struct Upstream {
    seen: Arc<Mutex<Vec<Seen>>>,
    reply: Reply,
}

fn text_reply() -> Value {
    json!({
        "responseId": "gemini-local-reply",
        "modelVersion": UPSTREAM_MODEL,
        "candidates": [{
            "index": 0,
            "content": {"role": "model", "parts": [{"text": "local reply"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1, "totalTokenCount": 3}
    })
}

async fn upstream_handler(
    State(upstream): State<Upstream>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream
        .seen
        .lock()
        .unwrap()
        .push(Seen { uri, headers, body });
    match upstream.reply {
        Reply::Text => Json(text_reply()).into_response(),
        Reply::ParallelCalls => {
            // Neither call has a provider ID. Both occupy candidate 0 but need
            // different downstream tool indices and distinct synthesized IDs.
            let calls = json!({
                "responseId": "gemini-local-stream",
                "modelVersion": UPSTREAM_MODEL,
                "candidates": [{
                    "index": 0,
                    "content": {"role": "model", "parts": [
                        {"functionCall": {"name": "lookup_weather", "args": {"city": "Paris"}}},
                        {"functionCall": {"name": "lookup_time", "args": {"city": "Tokyo"}}}
                    ]}
                }]
            });
            let terminal = json!({
                "candidates": [{"index": 0, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 3, "totalTokenCount": 5}
            });
            let wire = format!("data: {calls}\n\ndata: {terminal}\n\n");
            ([("content-type", "text/event-stream")], wire).into_response()
        }
        Reply::InvalidCall | Reply::ConflictingCall => {
            let valid = json!({
                "candidates": [{"index": 0, "content": {"role": "model", "parts": [
                    {"functionCall": {"id": "upstream-call-1", "name": "lookup_weather", "args": {"city": "Paris"}}}
                ]}}]
            });
            let invalid_call = match upstream.reply {
                // Syntactically valid JSON, but args is a fragment string rather
                // than a complete argument object; the native parser rejects it.
                Reply::InvalidCall => json!({"name": "lookup_weather", "args": "{\"city\":"}),
                // A later complete payload cannot silently replace or append to
                // the earlier call that has the same explicit upstream ID.
                _ => {
                    json!({"id": "upstream-call-1", "name": "lookup_weather", "args": {"city": "Tokyo"}})
                }
            };
            let invalid = json!({
                "candidates": [{"index": 0, "content": {"role": "model", "parts": [
                    {"functionCall": invalid_call}
                ]}}]
            });
            let terminal = json!({"candidates": [{"index": 0, "finishReason": "STOP"}]});
            let prefix = if matches!(upstream.reply, Reply::ConflictingCall) {
                format!("data: {valid}\n\n")
            } else {
                String::new()
            };
            let wire = format!("{prefix}data: {invalid}\n\ndata: {terminal}\n\n");
            ([("content-type", "text/event-stream")], wire).into_response()
        }
    }
}

/// Drop also runs on assertion failure; no mock server leaks into another test.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Gateway construction normally spawns an unconditional models.dev refresh
/// and usage/OAuth monitors. They must NEVER run in this offline test harness.
///
/// Enter (but do not drive) a current-thread bootstrap runtime and poll the
/// constructor exactly once. Fresh MemoryStorage locks are uncontended, so the
/// constructor completes synchronously. `tokio::spawn` only queues background
/// tasks; it never synchronously polls them, and this runtime has no workers.
/// Dropping it cancels every queued task before any can perform network I/O.
/// If construction ever starts yielding, fail closed instead of driving it.
fn offline_gateway(config: GatewayConfig, provider: Provider) -> anyhow::Result<Gateway> {
    let model = Model {
        id: "local-model".into(),
        name: CLIENT_MODEL.into(),
        balance: "priority".into(),
        target_provider: provider.id.clone(),
        target_model: UPSTREAM_MODEL.into(),
        enable_auth: false,
        enable_payload: Some(false),
        force_max_reasoning: false,
        vision_shim: None,
        is_enabled: true,
        created_at: String::new(),
        targets: Vec::new(),
    };
    let storage = Arc::new(MemoryStorage::new(vec![provider], vec![model], vec![]));
    let bootstrap = runtime()?;
    let result = {
        let _entered = bootstrap.enter();
        let mut constructor = Box::pin(Gateway::from_storage(config, storage));
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        match constructor.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => anyhow::bail!(
                "offline Gateway construction yielded; refusing to run background network tasks"
            ),
        }
    };
    drop(bootstrap);
    let (mut gateway, _logs) = result?;
    gateway.http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()?;
    Ok(gateway)
}

struct Fixture {
    gateway: Gateway,
    listener: Option<std::net::TcpListener>,
    upstream: Upstream,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// All credentials are inert strings; no discovery/probe/OAuth API is used.
    fn new(vendor: &str, reply: Reply) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base = format!("http://{}", listener.local_addr()?);
        let base_url = if vendor == "vertexai" {
            format!("{base}/v1/projects/local-test/locations/global")
        } else {
            base
        };
        let provider = Provider {
            id: "local-provider".into(),
            name: format!("local-{vendor}"),
            vendor: Some(vendor.into()),
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string(),
            base_url,
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "local-fake-token".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let dir = tempfile::tempdir()?;
        let gateway = offline_gateway(
            GatewayConfig {
                data_dir: dir.path().into(),
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            provider,
        )?;
        Ok(Self {
            gateway,
            listener: Some(listener),
            upstream: Upstream {
                seen: Arc::new(Mutex::new(Vec::new())),
                reply,
            },
            _dir: dir,
        })
    }

    fn start(&mut self) -> anyhow::Result<AbortOnDrop> {
        let listener = tokio::net::TcpListener::from_std(self.listener.take().unwrap())?;
        let app = Router::new()
            .route("/*path", post(upstream_handler))
            .with_state(self.upstream.clone());
        Ok(AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })))
    }

    fn only_request(&self) -> Seen {
        let seen = self.upstream.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "expected exactly one local upstream attempt: {seen:?}"
        );
        seen[0].clone()
    }
}

async fn dispatch(gateway: &Gateway, ingress: ProtocolId, body: Value) -> anyhow::Result<String> {
    let (status, wire) = dispatch_outcome(gateway, ingress, body).await?;
    assert_eq!(status, StatusCode::OK, "dispatch failed: {wire}");
    Ok(wire)
}

async fn dispatch_outcome(
    gateway: &Gateway,
    ingress: ProtocolId,
    body: Value,
) -> anyhow::Result<(StatusCode, String)> {
    let path = match ingress {
        ANTHROPIC_MESSAGES_2023_06_01 => "/v1/messages".to_string(),
        OPENAI_RESPONSES_V1 => "/v1/responses".to_string(),
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA => {
            format!("/v1beta/models/{CLIENT_MODEL}:generateContent")
        }
        _ => "/v1/chat/completions".to_string(),
    };
    let request = if ingress == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
        // Same URL-derived model/stream inputs as the real Gemini ingress shell.
        GoogleDecoder.decode_with_model(body.clone(), CLIENT_MODEL, false)?
    } else {
        ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?
    };
    let response = tokio::time::timeout(
        TIMEOUT,
        dispatch_pipeline(
            gateway.clone(),
            HeaderMap::new(),
            RawEnvelope::new(Some(body), HashMap::new(), "POST", &path),
            request,
            ingress,
            RequestContext::new(ingress, TIMEOUT),
        ),
    )
    .await?;
    let status = response.status();
    let bytes = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 1024 * 1024),
    )
    .await??;
    let wire = String::from_utf8(bytes.to_vec())?;
    Ok((status, wire))
}

fn local_ref_schema() -> Value {
    json!({
        "type": "object",
        "$defs": {"City": {"type": "string", "description": "City to query", "minLength": 1}},
        "properties": {"city": {"$ref": "#/$defs/City"}},
        "required": ["city"],
        "additionalProperties": false
    })
}

fn chat_request(stream: bool) -> Value {
    json!({
        "model": CLIENT_MODEL,
        "stream": stream,
        "messages": [
            {"role": "user", "content": "Check weather and time."},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_weather_17", "type": "function", "function": {
                    "name": "lookup_weather", "arguments": "{\"city\":\"Paris\"}"
                }},
                {"id": "call_time_42", "type": "function", "function": {
                    "name": "lookup_time", "arguments": "{\"city\":\"Tokyo\"}"
                }}
            ]},
            // Deliberately reverse the results. Their IDs are not function names.
            {"role": "tool", "tool_call_id": "call_time_42", "content": "09:00"},
            {"role": "tool", "tool_call_id": "call_weather_17", "content": "sunny"}
        ],
        "tools": [
            {"type": "function", "function": {"name": "lookup_weather", "parameters": local_ref_schema()}},
            {"type": "function", "function": {"name": "lookup_time", "parameters": local_ref_schema()}}
        ]
    })
}

fn function_parts<'a>(body: &'a Value, kind: &str) -> Vec<&'a Value> {
    body["contents"]
        .as_array()
        .expect("Gemini contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().expect("Gemini parts"))
        .filter_map(|part| part.get(kind))
        .collect()
}

fn assert_tool_names(body: &Value) {
    let calls = function_parts(body, "functionCall");
    assert_eq!(calls.len(), 2, "assistant calls lost: {body}");
    assert_eq!(calls[0]["name"], "lookup_weather");
    assert_eq!(calls[1]["name"], "lookup_time");
    let results = function_parts(body, "functionResponse");
    assert_eq!(results.len(), 2, "tool results lost: {body}");
    assert_eq!(results[0]["id"], "call_time_42");
    assert_eq!(results[0]["name"], "lookup_time");
    assert_eq!(results[1]["id"], "call_weather_17");
    assert_eq!(results[1]["name"], "lookup_weather");
    // Matching names but swapping payloads is not sufficient correlation.
    assert!(results[0]["response"].to_string().contains("09:00"));
    assert!(results[1]["response"].to_string().contains("sunny"));
}

fn assert_lowered_schema(body: &Value) {
    let declarations = body["tools"][0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(declarations.len(), 2);
    for declaration in declarations {
        assert_eq!(
            declaration["parameters"],
            json!({
                "type": "object",
                "properties": {"city": {"type": "string", "description": "City to query"}},
                "required": ["city"]
            }),
            "local reference must resolve before cleanup: {declaration}"
        );
        assert!(declaration.get("parametersJsonSchema").is_none());
    }
}

#[derive(Default)]
struct ChatCall {
    id: String,
    name: String,
    arguments: String,
    starts: usize,
}

fn assert_parallel_chat_calls(wire: &str) -> anyhow::Result<()> {
    let mut calls = BTreeMap::<u64, ChatCall>::new();
    let mut terminal = false;
    for data in wire.lines().filter_map(|line| line.strip_prefix("data: ")) {
        if data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(data)?;
        assert!(event.get("error").is_none(), "stream error: {wire}");
        for choice in event["choices"].as_array().into_iter().flatten() {
            terminal |= choice["finish_reason"] == "tool_calls";
            for delta in choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let index = delta["index"].as_u64().expect("Chat tool index");
                let call = calls.entry(index).or_default();
                if let Some(id) = delta["id"].as_str() {
                    call.id = id.to_string();
                    call.starts += 1;
                }
                if let Some(name) = delta["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(arguments) = delta["function"]["arguments"].as_str() {
                    call.arguments.push_str(arguments);
                }
            }
        }
    }
    assert_eq!(
        calls.len(),
        2,
        "parallel calls must not share candidate index 0: {wire}"
    );
    let weather = &calls[&0];
    let time = &calls[&1];
    assert_eq!(
        (weather.starts, time.starts),
        (1, 1),
        "duplicate starts: {wire}"
    );
    assert!(
        !weather.id.is_empty() && !time.id.is_empty(),
        "missing IDs: {wire}"
    );
    assert_ne!(
        weather.id, time.id,
        "synthetic IDs must be distinct: {wire}"
    );
    assert_eq!(weather.name, "lookup_weather");
    assert_eq!(time.name, "lookup_time");
    assert_eq!(
        serde_json::from_str::<Value>(&weather.arguments)?,
        json!({"city": "Paris"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&time.arguments)?,
        json!({"city": "Tokyo"})
    );
    assert!(
        terminal && wire.contains("data: [DONE]"),
        "missing Chat terminal: {wire}"
    );
    Ok(())
}

#[test]
fn chat_to_native_gemini_correlates_results_lowers_refs_and_streams_parallel_calls()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new("google", Reply::ParallelCalls)?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let wire = dispatch(
            &fixture.gateway,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            chat_request(true),
        )
        .await?;
        let seen = fixture.only_request();
        assert_eq!(
            seen.uri.path(),
            format!("/v1beta/models/{UPSTREAM_MODEL}:streamGenerateContent")
        );
        assert!(seen.uri.query().unwrap_or_default().contains("alt=sse"));
        assert!(
            seen.uri
                .query()
                .unwrap_or_default()
                .contains("key=local-fake-token")
        );
        assert_tool_names(&seen.body);
        assert_lowered_schema(&seen.body);
        assert_parallel_chat_calls(&wire)
    })
}

#[test]
fn anthropic_to_gemini_compat_rich_schema_is_not_rejected_or_cleaned_by_native_preview()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new("google", Reply::Text)?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        // A non-const oneOf is intentionally unsupported by native lowering.
        // The ordinary dispatcher-selected compat route supports it using the
        // richer parametersJsonSchema channel; the preview must not reject it.
        let schema = json!({
            "type": "object",
            "properties": {"city": {"oneOf": [{"type": "string"}, {"type": "integer"}]}},
            "required": ["city"],
            "additionalProperties": false
        });
        let body = json!({
            "model": CLIENT_MODEL,
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": "Check weather and time."},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_weather_17", "name": "lookup_weather", "input": {"city": "Paris"}},
                    {"type": "tool_use", "id": "call_time_42", "name": "lookup_time", "input": {"city": "Tokyo"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_time_42", "content": "09:00"},
                    {"type": "tool_result", "tool_use_id": "call_weather_17", "content": "sunny"}
                ]}
            ],
            "tools": [
                {"name": "lookup_weather", "description": "Weather", "input_schema": schema},
                {"name": "lookup_time", "description": "Time", "input_schema": schema}
            ]
        });
        let wire = dispatch(&fixture.gateway, ANTHROPIC_MESSAGES_2023_06_01, body).await?;
        let seen = fixture.only_request();
        assert_eq!(seen.uri.path(), format!("/v1beta/models/{UPSTREAM_MODEL}:generateContent"));
        assert_tool_names(&seen.body);
        let declarations = seen.body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(declarations.len(), 2);
        for declaration in declarations {
            assert!(declaration.get("parameters").is_none(), "compat rich schema became native: {declaration}");
            assert_eq!(declaration["parametersJsonSchema"], schema, "native preview changed compat schema");
        }
        let response: Value = serde_json::from_str(&wire)?;
        assert_eq!(response["type"], "message");
        assert_eq!(response["content"][0]["text"], "local reply");
        assert_eq!(response["stop_reason"], "end_turn");
        Ok(())
    })
}

#[test]
fn native_gemini_unsupported_schema_is_typed_422_without_contacting_upstream() -> anyhow::Result<()>
{
    let mut fixture = Fixture::new("google", Reply::Text)?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let mut body = chat_request(false);
        // The same composition is accepted by the ordinary Anthropic compat
        // test, but native lowering must reject rather than silently drop it.
        body["tools"][0]["function"]["parameters"] = json!({
            "type": "object",
            "properties": {"city": {"oneOf": [{"type": "string"}, {"type": "integer"}]}}
        });
        let (status, wire) = dispatch_outcome(
            &fixture.gateway,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            body,
        )
        .await?;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "native schema rejection lost its status: {wire}"
        );
        assert!(
            wire.contains("NYRO_PROTOCOL_LOSSY_REJECTED"),
            "native schema rejection lost its type: {wire}"
        );
        assert!(
            wire.contains("city") && wire.contains("oneOf"),
            "schema rejection lost its path/reason: {wire}"
        );
        assert!(
            fixture.upstream.seen.lock().unwrap().is_empty(),
            "native rejection must precede upstream HTTP"
        );
        Ok(())
    })
}

#[test]
fn default_gemini_passthrough_preserves_raw_schema_without_native_cleanup() -> anyhow::Result<()> {
    let mut fixture = Fixture::new("google", Reply::Text)?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let body = json!({
            "contents": [{"role": "user", "parts": [{"text": "Keep this native request intact."}]}],
            "tools": [{"functionDeclarations": [{
                "name": "lookup_weather",
                "parameters": {
                    "type": "object",
                    "$defs": {"City": {"type": "string", "format": "custom-city"}},
                    "properties": {
                        "city": {"$ref": "#/$defs/City"},
                        "mode": {"oneOf": [{"type": "string"}, {"type": "integer"}]}
                    },
                    "additionalProperties": false,
                    "x-client-schema": {"keep": true}
                }
            }]}],
            "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
            "cachedContent": "cachedContents/local-only"
        });
        let wire = dispatch(
            &fixture.gateway,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            body.clone(),
        )
        .await?;
        let seen = fixture.only_request();
        assert_eq!(
            seen.uri.path(),
            format!("/v1beta/models/{UPSTREAM_MODEL}:generateContent")
        );
        assert_eq!(
            seen.body["tools"], body["tools"],
            "same-protocol route must bypass native schema cleanup"
        );
        assert_eq!(seen.body["contents"], body["contents"]);
        assert_eq!(seen.body["toolConfig"], body["toolConfig"]);
        assert_eq!(seen.body["cachedContent"], body["cachedContent"]);
        assert_eq!(serde_json::from_str::<Value>(&wire)?, text_reply());
        Ok(())
    })
}

#[test]
fn responses_to_gemini_invalid_or_conflicting_calls_emit_error_not_success() -> anyhow::Result<()> {
    for reply in [Reply::InvalidCall, Reply::ConflictingCall] {
        let mut fixture = Fixture::new("google", reply)?;
        runtime()?.block_on(async {
            let _server = fixture.start()?;
            let wire = dispatch(
                &fixture.gateway,
                OPENAI_RESPONSES_V1,
                json!({
                    "model": CLIENT_MODEL,
                    "stream": true,
                    "input": "Check the weather.",
                    "tools": [{"type": "function", "name": "lookup_weather", "parameters": {
                        "type": "object", "properties": {"city": {"type": "string"}}
                    }}]
                }),
            )
            .await?;
            let seen = fixture.only_request();
            assert_eq!(
                seen.uri.path(),
                format!("/v1beta/models/{UPSTREAM_MODEL}:streamGenerateContent")
            );
            assert!(
                wire.contains("event: error\n"),
                "decoder failure must reach the Responses client: {wire}"
            );
            assert!(
                !wire.contains("response.completed"),
                "failed stream was reported as completed: {wire}"
            );
            assert!(
                !wire.contains("response.output_item.done"),
                "invalid or conflicted call must not be committed: {wire}"
            );
            assert!(
                !wire.contains("data: [DONE]"),
                "error must not be followed by the success terminator: {wire}"
            );
            let events = wire
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(serde_json::from_str::<Value>)
                .collect::<Result<Vec<_>, _>>()?;
            let errors: Vec<_> = events
                .iter()
                .filter(|event| event["type"] == "error")
                .collect();
            assert_eq!(errors.len(), 1, "expected one explicit failure: {wire}");
            assert!(
                errors[0]["error"]["failure_kind"]
                    .as_str()
                    .is_some_and(|kind| !kind.is_empty())
            );
            assert!(
                errors[0]["request_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
            );
            assert_eq!(
                events.last().unwrap()["type"],
                "error",
                "failure must be terminal: {wire}"
            );
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn chat_to_vertex_native_uses_fake_bearer_and_shared_gemini_name_schema_encoding()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new("vertexai", Reply::Text)?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let wire = dispatch(&fixture.gateway, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, chat_request(false)).await?;
        let seen = fixture.only_request();
        assert_eq!(seen.uri.path(), format!(
            "/v1/projects/local-test/locations/global/publishers/google/models/{UPSTREAM_MODEL}:generateContent"
        ));
        assert_eq!(seen.headers["authorization"], "Bearer local-fake-token");
        assert!(seen.uri.query().is_none(), "Vertex must not use Gemini key query auth");
        assert_tool_names(&seen.body);
        assert_lowered_schema(&seen.body);
        let response: Value = serde_json::from_str(&wire)?;
        assert_eq!(response["choices"][0]["message"]["content"], "local reply");
        assert_eq!(response["choices"][0]["finish_reason"], "stop");
        Ok(())
    })
}
