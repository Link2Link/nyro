//! End-to-end nyro→nyro chain over real HTTP:
//!
//! ```text
//! client → local Nyro (vendor `nyro-gateway`) → remote Nyro (auth_key set) → mock LLM
//! ```
//!
//! Proves the Nyro preset reaches the matching remote ingress for every
//! protocol (no conversion), normalizes a pasted `/v1` suffix, and passes the
//! remote's global `NYRO_PROXY_AUTH_KEY` middleware with Bearer — including on
//! Anthropic egress, where the default pipeline would send `x-api-key`.
//! Two Nyro-preset hops also preserve complete Responses JSON and SSE bytes,
//! client-supplied tool-result continuations, and safe protocol headers without
//! provider compatibility rewrites or gateway-side tool execution.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{OriginalUri, Request, State},
    http::HeaderMap,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use futures::StreamExt;
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider, CreateProviderProtocolEndpoint},
    proxy::server::create_router,
};
use serde_json::{Value, json};

const REMOTE_KEY: &str = "chain-secret";

#[derive(Clone, Default)]
struct Mock {
    hits: Arc<Mutex<Vec<String>>>,
}

async fn mock_handler(
    State(mock): State<Mock>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> Response {
    let path = uri.path().to_string();
    mock.hits.lock().unwrap().push(path.clone());
    let model = body["model"].as_str().unwrap_or("gemini").to_string();
    let reply = if path == "/v1/chat/completions" {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 0, "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong-chat"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    } else if path == "/v1/messages" {
        json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": model,
            "content": [{"type": "text", "text": "pong-claude"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
    } else {
        json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "pong-gemini"}]}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
        })
    };
    Json(reply).into_response()
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

async fn gateway(dir: &tempfile::TempDir, auth_key: Option<&str>) -> Gateway {
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        auth_key: auth_key.map(str::to_string),
        ..Default::default()
    };
    Gateway::new(config).await.expect("gateway init").0
}

fn endpoint(
    protocol: &str,
    base_url: &str,
    api_key: &str,
    auth_scheme: &str,
    priority: i32,
) -> CreateProviderProtocolEndpoint {
    CreateProviderProtocolEndpoint {
        protocol: protocol.into(),
        base_url: base_url.into(),
        api_key: api_key.into(),
        auth_scheme: auth_scheme.into(),
        is_enabled: true,
        priority,
    }
}

async fn adaptive_provider(
    gw: &Gateway,
    name: &str,
    vendor: Option<&str>,
    endpoints: Vec<CreateProviderProtocolEndpoint>,
) -> String {
    let first = endpoints[0].clone();
    gw.admin()
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: name.into(),
            vendor: vendor.map(str::to_string),
            protocol: first.protocol.clone(),
            base_url: first.base_url.clone(),
            protocol_mode: "adaptive".into(),
            protocol_endpoints: endpoints,
            preset_key: vendor.map(str::to_string),
            channel: vendor.map(|_| "default".to_string()),
            models_source: None,
            static_models: None,
            api_key: first.api_key.clone(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await
        .expect("create provider")
        .id
}

async fn route(gw: &Gateway, name: &str, provider: &str, target: &str) {
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: provider.into(),
            target_model: target.into(),
            targets: vec![],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await
        .expect("create route");
}

/// Remote Nyro (global auth key) in front of the mock LLM; returns its proxy URL.
async fn remote(dir: &tempfile::TempDir, mock_url: &str) -> String {
    let gw = gateway(dir, Some(REMOTE_KEY)).await;
    let provider = adaptive_provider(
        &gw,
        "mock-llm",
        None,
        vec![
            endpoint(
                "openai-compatible/chat-completions/v1",
                &format!("{mock_url}/v1"),
                "sk-mock",
                "auto",
                0,
            ),
            endpoint(
                "anthropic-messages/messages/2023-06-01",
                mock_url,
                "sk-mock",
                "auto",
                1,
            ),
            endpoint(
                "google-gemini/generate-content/v1beta",
                mock_url,
                "sk-mock",
                "bearer",
                2,
            ),
        ],
    )
    .await;
    for name in ["remote-chat", "remote-claude", "remote-gemini"] {
        route(&gw, name, &provider, "upstream-model").await;
    }
    serve(create_router(gw)).await
}

/// Local Nyro whose only provider is the Nyro preset pointing at `remote_url`.
async fn local(dir: &tempfile::TempDir, remote_url: &str, key: &str) -> String {
    let (gw, _) = local_gateway(dir, remote_url, key).await;
    serve(create_router(gw)).await
}

/// The local gateway and its Nyro provider id, without serving it.
async fn local_gateway(dir: &tempfile::TempDir, remote_url: &str, key: &str) -> (Gateway, String) {
    let gw = gateway(dir, None).await;
    // Users paste the address with `/v1`; it must not double-version any route.
    let address = format!("{remote_url}/v1");
    let endpoints = [
        "openai-compatible/chat-completions/v1",
        "openai-responses/responses/v1",
        "anthropic-messages/messages/2023-06-01",
        "google-gemini/generate-content/v1beta",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, protocol)| endpoint(protocol, &address, key, "bearer", index as i32))
    .collect();
    let provider = adaptive_provider(&gw, "nyro-remote", Some("nyro-gateway"), endpoints).await;
    route(&gw, "chat", &provider, "remote-chat").await;
    route(&gw, "claude", &provider, "remote-claude").await;
    route(&gw, "gemini", &provider, "remote-gemini").await;
    (gw, provider)
}

async fn post_json(
    client: &reqwest::Client,
    url: String,
    body: Value,
    anthropic: bool,
) -> (u16, String) {
    let mut request = client
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(15));
    if anthropic {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let response = request.send().await.expect("local response");
    (
        response.status().as_u16(),
        response.text().await.unwrap_or_default(),
    )
}

#[tokio::test]
async fn nyro_preset_chains_every_protocol_through_an_authenticated_remote() {
    let (remote_dir, local_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mock = Mock::default();
    let mock_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(mock_handler))
            .route("/v1/messages", post(mock_handler))
            .route("/v1beta/models/:model_action", post(mock_handler))
            .with_state(mock.clone()),
    )
    .await;
    let remote_url = remote(&remote_dir, &mock_url).await;
    let local_url = local(&local_dir, &remote_url, REMOTE_KEY).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        format!("{local_url}/v1/chat/completions"),
        json!({"model": "chat", "messages": [{"role": "user", "content": "ping"}]}),
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-chat"), "{body}");

    // Anthropic egress authenticates with Bearer, not x-api-key.
    let (status, body) = post_json(
        &client,
        format!("{local_url}/v1/messages"),
        json!({"model": "claude", "max_tokens": 16, "messages": [{"role": "user", "content": "ping"}]}),
        true,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-claude"), "{body}");

    let (status, body) = post_json(
        &client,
        format!("{local_url}/v1beta/models/gemini:generateContent"),
        json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}]}),
        false,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-gemini"), "{body}");

    // Each request reached the mock on its native route: no protocol conversion
    // at either hop.
    assert_eq!(
        *mock.hits.lock().unwrap(),
        [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1beta/models/upstream-model:generateContent",
        ]
    );
}

#[tokio::test]
async fn nyro_preset_with_a_wrong_remote_key_never_reaches_the_upstream() {
    let (remote_dir, local_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mock = Mock::default();
    let mock_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(mock_handler))
            .with_state(mock.clone()),
    )
    .await;
    let remote_url = remote(&remote_dir, &mock_url).await;
    let local_url = local(&local_dir, &remote_url, "not-the-key").await;

    let (status, body) = post_json(
        &reqwest::Client::new(),
        format!("{local_url}/v1/chat/completions"),
        json!({"model": "chat", "messages": [{"role": "user", "content": "ping"}]}),
        false,
    )
    .await;
    assert_ne!(status, 200, "{body}");
    assert!(mock.hits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nyro_preset_usage_forwards_the_remote_report_through_the_admin_api() {
    let (remote_dir, local_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mock_url = serve(Router::new()).await;
    let remote_url = remote(&remote_dir, &mock_url).await;

    // The remote never serves upstream usage anonymously, even on open routes.
    let anonymous = reqwest::Client::new()
        .get(format!("{remote_url}/v1/nyro/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let (gw, provider) = local_gateway(&local_dir, &remote_url, REMOTE_KEY).await;
    let usage = gw
        .admin()
        .get_provider_usage(&provider)
        .await
        .expect("usage");
    assert_eq!(usage.kind, "nyro_gateway");
    let wire = serde_json::to_value(&usage).unwrap();
    let report = &wire["upstream"];
    assert_eq!(report["object"], "nyro.usage");
    // The gateway key is a master key: no money fields are withheld.
    assert!(report.get("redacted").is_none());
    let providers = report["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["name"], "mock-llm");
    assert_eq!(
        providers[0]["routes"],
        json!(["remote-chat", "remote-claude", "remote-gemini"])
    );
    // A plain relay has no usage backend on the remote.
    assert_eq!(providers[0]["status"], "unsupported");
    // One unsupported upstream keeps the local provider schedulable.
    assert_eq!(
        usage.scheduling.status,
        nyro_core::admin::ProviderSchedulingStatus::Eligible
    );
}

// These tests deliberately use the Nyro preset at BOTH hops. The older remote()
// helper uses a custom provider, whose third-party compatibility rewrites are
// appropriate at a real provider boundary but not between Nyro gateways.
#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    headers: HeaderMap,
    body: Value,
}

type Captures = Arc<Mutex<Vec<CapturedRequest>>>;

#[derive(Clone)]
struct WireMock {
    requests: Captures,
    json: Value,
    sse: Bytes,
    sse_status: u16,
}

async fn capture_remote(
    State(requests): State<Captures>,
    request: Request,
    next: Next,
) -> Response {
    // Provider usage/model discovery may issue unrelated, body-less requests.
    if request.method() != axum::http::Method::POST || request.uri().path() != "/v1/responses" {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap();
    requests.lock().unwrap().push(CapturedRequest {
        path: parts.uri.path().into(),
        headers: parts.headers.clone(),
        body,
    });
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

async fn wire_mock(
    State(mock): State<WireMock>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let stream = body["stream"] == true;
    mock.requests.lock().unwrap().push(CapturedRequest {
        path: uri.path().into(),
        headers,
        body,
    });
    // Honor the actual upstream request, so forcing stream:true is observable
    // both in the captured body and in the returned content type.
    let (status, content_type, body) = if stream {
        // One-byte chunks split UTF-8 code points, SSE field names and JSON.
        let chunks = (0..mock.sse.len())
            .map(|i| Ok::<_, std::convert::Infallible>(mock.sse.slice(i..i + 1)))
            .collect::<Vec<_>>();
        let stream = futures::stream::iter(chunks).then(|chunk| async move {
            tokio::task::yield_now().await;
            chunk
        });
        (
            mock.sse_status,
            "text/event-stream",
            Body::from_stream(stream),
        )
    } else {
        (201, "application/json", Body::from(mock.json.to_string()))
    };
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("x-request-id", "upstream-wire-id")
        .header("retry-after", "7")
        .header("x-ratelimit-remaining-requests", "42")
        .body(body)
        .unwrap()
}

async fn nyro_wire_hop(
    dir: &tempfile::TempDir,
    upstream: &str,
    key: &str,
    auth_key: Option<&str>,
    routes: &[(&str, &str)],
    captures: Option<Captures>,
) -> String {
    let gw = gateway(dir, auth_key).await;
    let endpoints = [
        "openai-compatible/chat-completions/v1",
        "openai-responses/responses/v1",
        "anthropic-messages/messages/2023-06-01",
        "google-gemini/generate-content/v1beta",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, protocol)| endpoint(protocol, &format!("{upstream}/v1"), key, "bearer", i as i32))
    .collect();
    let provider = adaptive_provider(&gw, "nyro-wire", Some("nyro-gateway"), endpoints).await;
    for (alias, target) in routes {
        route(&gw, alias, &provider, target).await;
    }
    let mut router = create_router(gw);
    if let Some(captures) = captures {
        router = router.layer(middleware::from_fn_with_state(captures, capture_remote));
    }
    serve(router).await
}

async fn responses_chain(
    local_dir: &tempfile::TempDir,
    remote_dir: &tempfile::TempDir,
    mock: &WireMock,
) -> (String, Captures) {
    let mock_url = serve(
        Router::new()
            .route("/v1/responses", post(wire_mock))
            .with_state(mock.clone()),
    )
    .await;
    let remote_requests = Captures::default();
    let remote_url = nyro_wire_hop(
        remote_dir,
        &mock_url,
        "sk-mock",
        Some(REMOTE_KEY),
        &[("grok-remote-alias", "grok-upstream-alias")],
        Some(remote_requests.clone()),
    )
    .await;
    let local_url = nyro_wire_hop(
        local_dir,
        &remote_url,
        REMOTE_KEY,
        None,
        &[("responses", "grok-remote-alias")],
        None,
    )
    .await;
    (local_url, remote_requests)
}

const CLIENT_PROTOCOL_HEADERS: &[(&str, &str)] = &[
    ("openai-beta", "responses=v1"),
    ("openai-organization", "org-wire"),
    ("openai-project", "proj-wire"),
    ("originator", "codex_cli_rs"),
    ("session_id", "session-wire"),
    ("x-codex-turn-state", "opaque-turn-state"),
    ("x-openai-internal-codex-responses-lite", "true"),
    ("x-client-request-id", "client-wire-id"),
];

async fn wire_post(url: &str, path: &str, body: &Value) -> (u16, HeaderMap, Bytes) {
    let mut request = reqwest::Client::new()
        .post(format!("{url}{path}"))
        .json(body)
        .timeout(Duration::from_secs(15))
        .header("authorization", "Bearer client-must-not-leak")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14");
    for (name, value) in CLIENT_PROTOCOL_HEADERS {
        request = request.header(*name, *value);
    }
    let response = request.send().await.expect("wire response");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.bytes().await.expect("complete response bytes");
    (status, headers, bytes)
}

fn assert_wire_requests(
    requests: &Captures,
    originals: &[Value],
    path: &str,
    model: &str,
    key: &str,
) {
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        originals.len(),
        "only client turns may call upstream; no gateway tool loop"
    );
    for (actual, original) in requests.iter().zip(originals) {
        let mut expected = original.clone();
        expected["model"] = json!(model);
        assert_eq!(actual.path, path);
        assert_eq!(
            actual.body, expected,
            "Nyro may change only the routing model alias"
        );
        for (name, value) in CLIENT_PROTOCOL_HEADERS {
            assert_eq!(
                actual.headers.get(*name).and_then(|v| v.to_str().ok()),
                Some(*value),
                "{name}"
            );
        }
        assert_eq!(actual.headers["authorization"], format!("Bearer {key}"));
        assert!(!actual.headers.contains_key("x-api-key"));
    }
}

fn assert_wire_response_headers(headers: &HeaderMap, content_type: &str) {
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with(content_type)
    );
    assert_eq!(headers["x-request-id"], "upstream-wire-id");
    assert_eq!(headers["retry-after"], "7");
    assert_eq!(headers["x-ratelimit-remaining-requests"], "42");
}

fn responses_request() -> Value {
    json!({
        "model": "responses", "stream": true, "store": false,
        "instructions": "Keep the client's opaque protocol fields.",
        "input": [
            {"type":"additional_tools", "x-discovery":"opaque", "tools":[
                {"type":"function", "name":"late_tool", "parameters":{"type":"object"}},
                {"type":"custom", "name":"late_custom", "format":{"type":"text"}}
            ]},
            {"type":"message", "role":"developer", "content":[{"type":"input_text", "text":"No local tool execution."}]},
            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"你好 🦀"}]}
        ],
        "tools": [
            {"type":"function", "name":"public_lookup", "parameters":{"type":"object", "properties":{"q":{"type":"string"}}}, "x-public":true},
            {"type":"namespace", "name":"workspace", "description":"Client-owned tools", "tools":[
                {"type":"function", "name":"lookup", "parameters":{"type":"object"}},
                {"type":"custom", "name":"patch", "format":{"type":"grammar", "syntax":"lark", "definition":"start: /.+/"}}
            ]},
            {"type":"custom", "name":"public_custom", "format":{"type":"text"}},
            {"type":"tool_search", "execution":"client", "x-search":{"limit":3}},
            {"type":"web_search_preview", "search_context_size":"low"}
        ],
        "tool_choice":{"type":"allowed_tools", "mode":"auto", "tools":[
            {"type":"function", "name":"lookup", "namespace":"workspace"},
            {"type":"custom", "name":"patch", "namespace":"workspace"},
            {"type":"function", "name":"public_lookup"}
        ]},
        "include":["reasoning.encrypted_content"],
        "reasoning":{"effort":"max", "summary":"detailed", "x-reasoning":[1,null]},
        "max_output_tokens":64, "parallel_tool_calls":false,
        "text":{"format":{"type":"text"}, "verbosity":"low"},
        "metadata":{"client":"wire-fixture"}, "prompt_cache_key":"cache-wire",
        "x-client-extension":{"nested":[null,false,{"opaque":"unchanged"}]}
    })
}

fn responses_reply() -> Value {
    json!({
        "id":"resp_wire", "object":"response", "model":"grok-upstream-alias",
        "status":"completed", "store":false, "created_at":123,
        "output":[
            {"type":"reasoning", "id":"rs_opaque", "summary":[{"type":"summary_text", "text":"工具 🦀"}],
             "encrypted_content":"opaque+/ciphertext==", "x-reasoning":{"version":9}},
            {"type":"function_call", "id":"item_function_original", "call_id":"call_function_original", "name":"lookup",
             "namespace":"workspace", "arguments":"{\"q\":\"你好\"}", "status":"completed", "x-call":1},
            {"type":"custom_tool_call", "id":"item_custom_original", "call_id":"call_custom_original", "name":"patch",
             "namespace":"workspace", "input":"*** client patch 🦀 ***", "status":"completed", "x-call":2},
            {"type":"tool_search_call", "id":"search_original", "status":"completed", "x-search":{"found":["workspace.lookup"]}},
            {"type":"message", "id":"msg_original", "role":"assistant", "status":"completed", "content":[
                {"type":"output_text", "text":"交给客户端", "annotations":[], "x-text":"opaque"}]}
        ],
        "usage":{"input_tokens":11, "output_tokens":7, "total_tokens":18, "output_tokens_details":{"reasoning_tokens":3}},
        "x-response-extension":{"opaque":[1,false,null]}
    })
}

fn responses_wire_mock() -> WireMock {
    let reply = responses_reply();
    let events = vec![
        json!({"type":"response.created", "response":{"id":"resp_wire", "object":"response", "status":"in_progress", "output":[]}}),
        json!({"type":"response.output_item.added", "output_index":1, "item":{"type":"function_call", "id":"item_function_original", "call_id":"call_function_original", "name":"lookup", "namespace":"workspace", "arguments":""}}),
        json!({"type":"response.function_call_arguments.delta", "item_id":"item_function_original", "output_index":1, "delta":"{\"q\":\"你好\"}"}),
        json!({"type":"response.function_call_arguments.done", "item_id":"item_function_original", "output_index":1, "arguments":"{\"q\":\"你好\"}"}),
        json!({"type":"response.output_item.done", "output_index":1, "item":reply["output"][1]}),
        json!({"type":"response.output_item.added", "output_index":2, "item":{"type":"custom_tool_call", "id":"item_custom_original", "call_id":"call_custom_original", "name":"patch", "namespace":"workspace", "input":""}}),
        json!({"type":"response.custom_tool_call_input.delta", "item_id":"item_custom_original", "output_index":2, "delta":"*** client patch 🦀 ***"}),
        json!({"type":"response.custom_tool_call_input.done", "item_id":"item_custom_original", "output_index":2, "input":"*** client patch 🦀 ***"}),
        json!({"type":"response.output_item.done", "output_index":2, "item":reply["output"][2]}),
        json!({"type":"response.output_item.done", "output_index":0, "item":reply["output"][0]}),
        json!({"type":"response.completed", "response":reply}),
    ];
    let mut sse = ": upstream comment 🦀\r\nretry: 1234\r\n\r\nevent: vendor.unknown\nid: opaque-event\ndata: {\"opaque\":\"你好\",\"array\":[null,false]}\n\n".to_string();
    for (i, event) in events.iter().enumerate() {
        sse.push_str(&format!(
            "id: wire-{i}\nevent: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        ));
    }
    sse.push_str(": final comment\n\ndata: [DONE]\n\n");
    WireMock {
        requests: Captures::default(),
        json: reply,
        sse: Bytes::from(sse),
        sse_status: 201,
    }
}

#[tokio::test]
async fn nyro_responses_two_hops_preserve_sse_and_client_tool_round_trips() {
    let (local_dir, remote_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mock = responses_wire_mock();
    let (url, remote_requests) = responses_chain(&local_dir, &remote_dir, &mock).await;
    let first = responses_request();
    let tool_results = json!([
        {"type":"function_call_output", "id":"result_function_original", "call_id":"call_function_original", "output":"CLIENT supplied function result", "x-result":1},
        {"type":"custom_tool_call_output", "id":"result_custom_original", "call_id":"call_custom_original", "output":"CLIENT supplied custom result 🦀", "x-result":2}
    ]);
    let mut replay = first.clone();
    // store:false continuation replays the exact returned output, including
    // encrypted reasoning and the distinct item IDs / call IDs, then results
    // supplied by the client. No actual tool implementation exists in this test.
    replay["input"]
        .as_array_mut()
        .unwrap()
        .extend(mock.json["output"].as_array().unwrap().clone());
    replay["input"]
        .as_array_mut()
        .unwrap()
        .extend(tool_results.as_array().unwrap().clone());
    let mut previous = first.clone();
    previous["previous_response_id"] = json!("resp_wire");
    previous["input"] = tool_results;
    let turns = [first, replay, previous];
    for (i, body) in turns.iter().enumerate() {
        let (status, headers, bytes) = wire_post(&url, "/v1/responses", body).await;
        assert_eq!(
            status,
            mock.sse_status,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_wire_requests(
            &remote_requests,
            &turns[..=i],
            "/v1/responses",
            "grok-remote-alias",
            REMOTE_KEY,
        );
        assert_wire_requests(
            &mock.requests,
            &turns[..=i],
            "/v1/responses",
            "grok-upstream-alias",
            "sk-mock",
        );
        assert_eq!(
            bytes, mock.sse,
            "complete SSE bytes, not reconstructed events"
        );
        assert_wire_response_headers(&headers, "text/event-stream");
    }
}

#[tokio::test]
async fn nyro_responses_two_hops_preserve_nonstream_json_and_omitted_stream() {
    let (local_dir, remote_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mock = responses_wire_mock();
    let (url, remote_requests) = responses_chain(&local_dir, &remote_dir, &mock).await;
    let mut explicit = responses_request();
    explicit["stream"] = json!(false);
    let mut omitted = explicit.clone();
    omitted.as_object_mut().unwrap().remove("stream");
    // Plain function tools must stay native too: no custom/namespace feature
    // may be required to avoid forced SSE or injected `strict` defaults.
    let function_only = json!({
        "model":"responses", "input":"hello", "stream":false,
        "tools":[{"type":"function", "name":"public_lookup", "parameters":{"type":"object"}}],
        "tool_choice":{"type":"function", "name":"public_lookup"}
    });
    let mut function_omitted = function_only.clone();
    function_omitted.as_object_mut().unwrap().remove("stream");
    // No builtin/unknown items: this would be eligible for destructive
    // third-party PortableInline replay if Nyro were misclassified as a vendor.
    let encrypted_replay = json!({
        "model":"responses", "stream":false, "store":false,
        "include":["reasoning.encrypted_content"],
        "input":[
            {"role":"user", "content":"Continue the previous reasoning."},
            {"type":"reasoning", "id":"rs_private_original", "summary":[],
             "encrypted_content":"opaque+/ciphertext=="}
        ]
    });
    let turns = [
        explicit,
        omitted,
        function_only,
        function_omitted,
        encrypted_replay,
    ];
    for (i, body) in turns.iter().enumerate() {
        let (status, headers, bytes) = wire_post(&url, "/v1/responses", body).await;
        assert_wire_requests(
            &remote_requests,
            &turns[..=i],
            "/v1/responses",
            "grok-remote-alias",
            REMOTE_KEY,
        );
        assert_wire_requests(
            &mock.requests,
            &turns[..=i],
            "/v1/responses",
            "grok-upstream-alias",
            "sk-mock",
        );
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&bytes));
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), mock.json);
        assert_wire_response_headers(&headers, "application/json");
    }
}

#[tokio::test]
async fn nyro_responses_missing_native_endpoint_does_not_fallback_to_chat() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::default();
    let mock_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(mock_handler))
            .with_state(mock.clone()),
    )
    .await;
    let gw = gateway(&dir, None).await;
    let provider = adaptive_provider(
        &gw,
        "chat-only-nyro",
        Some("nyro-gateway"),
        vec![endpoint(
            "openai-compatible/chat-completions/v1",
            &format!("{mock_url}/v1"),
            "sk-mock",
            "bearer",
            0,
        )],
    )
    .await;
    route(&gw, "responses", &provider, "upstream-model").await;
    let url = serve(create_router(gw)).await;
    let (status, _, bytes) = wire_post(
        &url,
        "/v1/responses",
        &json!({"model":"responses", "input":"hello", "stream":false}),
    )
    .await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&bytes));
    assert!(
        mock.hits.lock().unwrap().is_empty(),
        "Nyro must not silently translate to Chat"
    );
}

async fn assert_native_hop(path: &str, request: Value, reply: Value) {
    let dir = tempfile::tempdir().unwrap();
    let mock = WireMock {
        requests: Captures::default(),
        json: reply,
        sse: Bytes::new(),
        sse_status: 200,
    };
    let mock_url = serve(
        Router::new()
            .route(path, post(wire_mock))
            .with_state(mock.clone()),
    )
    .await;
    let url = nyro_wire_hop(
        &dir,
        &mock_url,
        "sk-mock",
        None,
        &[("alias", "claude-sonnet-5-5")],
        None,
    )
    .await;
    let (status, headers, bytes) = wire_post(&url, path, &request).await;
    assert_wire_requests(
        &mock.requests,
        &[request],
        path,
        "claude-sonnet-5-5",
        "sk-mock",
    );
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0].headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        requests[0].headers["anthropic-beta"],
        "interleaved-thinking-2025-05-14"
    );
    assert_eq!(status, 201);
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), mock.json);
    assert_wire_response_headers(&headers, "application/json");
}

#[tokio::test]
async fn nyro_chat_hop_keeps_developer_effort_and_token_budget() {
    assert_native_hop("/v1/chat/completions", json!({
        "model":"alias", "stream":false, "max_tokens":32, "reasoning_effort":"max",
        "messages":[{"role":"developer", "content":"Keep this role"}, {"role":"user", "content":"hello"}],
        "tools":[{"type":"function", "function":{"name":"workspace.lookup", "parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function", "function":{"name":"workspace.lookup"}},
        "x-extension":{"opaque":true}
    }), json!({
        "id":"chat_wire", "object":"chat.completion", "created":0, "model":"claude-sonnet-5-5",
        "choices":[{"index":0, "message":{"role":"assistant", "content":"ok"}, "finish_reason":"stop"}],
        "usage":{"prompt_tokens":1, "completion_tokens":1, "total_tokens":2}, "x-extension":true
    })).await;
}

#[tokio::test]
async fn nyro_anthropic_hop_keeps_legacy_thinking_and_token_budget() {
    assert_native_hop("/v1/messages", json!({
        "model":"alias", "stream":false, "max_tokens":32,
        "thinking":{"type":"enabled", "budget_tokens":16, "display":"summarized"},
        "tools":[{"name":"workspace.lookup", "input_schema":{"type":"object"}}],
        "tool_choice":{"type":"tool", "name":"workspace.lookup"},
        "messages":[{"role":"user", "content":"hello"}], "x-extension":{"opaque":true}
    }), json!({
        "id":"msg_wire", "type":"message", "role":"assistant", "model":"claude-sonnet-5-5",
        "content":[{"type":"text", "text":"ok"}], "stop_reason":"end_turn", "stop_sequence":null,
        "usage":{"input_tokens":1, "output_tokens":1}, "x-extension":true
    })).await;
}

#[tokio::test]
async fn nyro_preset_usage_reports_actionable_errors() {
    let (remote_dir, local_dir, old_dir) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let mock_url = serve(Router::new()).await;
    let remote_url = remote(&remote_dir, &mock_url).await;

    let (gw, provider) = local_gateway(&local_dir, &remote_url, "not-the-key").await;
    let error = gw
        .admin()
        .get_provider_usage(&provider)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("authentication failed (HTTP 401"), "{error}");

    // A remote without the endpoint (older Nyro): 404 → upgrade hint.
    let (gw, provider) = local_gateway(&old_dir, &mock_url, REMOTE_KEY).await;
    let error = gw
        .admin()
        .get_provider_usage(&provider)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not support usage forwarding"),
        "{error}"
    );
}
