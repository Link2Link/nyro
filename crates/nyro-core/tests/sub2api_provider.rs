//! Sub2API preset end to end over real HTTP:
//!
//! ```text
//! client → Nyro (vendor `sub2api`, one platform channel) → mock Sub2API
//! ```
//!
//! Proves the per-platform routing contract: the gateway root survives a
//! pasted `/v1` or `/antigravity` suffix, Antigravity's Messages and Gemini
//! traffic lands under `/antigravity`, every protocol authenticates with
//! Bearer (never `x-api-key` rewriting or a `?key=` query), unchecked
//! protocols are converted to the platform's default protocol, the OpenAI
//! platform keeps the Codex consumer contract, and admin connectivity checks
//! and model discovery use the same routes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider, CreateProviderProtocolEndpoint, OpenAiSpeedMode},
    proxy::server::create_router,
};
use serde_json::{Value, json};

const KEY: &str = "sk-sub2api-test";
const CHAT: &str = "openai-compatible/chat-completions/v1";
const RESPONSES: &str = "openai-responses/responses/v1";
const MESSAGES: &str = "anthropic-messages/messages/2023-06-01";
const GEMINI: &str = "google-gemini/generate-content/v1beta";

#[derive(Debug, Clone)]
struct Hit {
    method: Method,
    path: String,
    query: String,
    authorization: Option<String>,
    x_api_key: Option<String>,
    body: Value,
}

#[derive(Clone, Default)]
struct Mock {
    hits: Arc<Mutex<Vec<Hit>>>,
    /// Paths answered with 400 like a group whose platform rejects the route.
    reject: Arc<Mutex<Vec<String>>>,
    /// `GET /v1/sub2api/billing` group name.
    group_name: Arc<Mutex<Option<String>>>,
    /// `GET /v1/models` override.
    listing: Arc<Mutex<Option<Value>>>,
    /// Every POST answers with this status/body (a Sub2API rejection).
    fail_posts: Arc<Mutex<Option<(u16, Value)>>>,
}

impl Mock {
    fn posts(&self) -> Vec<Hit> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| hit.method == Method::POST)
            .cloned()
            .collect()
    }
    fn gets(&self) -> Vec<String> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| hit.method == Method::GET)
            .map(|hit| hit.path.clone())
            .collect()
    }
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| match event["type"].as_str() {
            Some(kind) => format!("event: {kind}\ndata: {event}\n\n"),
            None => format!("data: {event}\n\n"),
        })
        .collect()
}

fn responses_stream(model: &str) -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{
            "id":"msg_1","type":"message","role":"assistant","content":[],"status":"in_progress"}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,
            "item_id":"msg_1","delta":"pong-responses"}),
        json!({"type":"response.completed","response":{"id":"resp_1","object":"response",
            "model":model,"status":"completed","output":[{
                "id":"msg_1","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"pong-responses","annotations":[]}]}],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ])
}

async fn mock_handler(
    State(mock): State<Mock>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let header_value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    mock.hits.lock().unwrap().push(Hit {
        method: method.clone(),
        path: path.clone(),
        query: query.clone(),
        authorization: header_value("authorization"),
        x_api_key: header_value("x-api-key"),
        body: body.clone(),
    });
    if mock.reject.lock().unwrap().contains(&path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"code":400,"message":"API key group platform is not gemini","status":"INVALID_ARGUMENT"}})),
        )
            .into_response();
    }
    if header_value("authorization").as_deref() != Some(&format!("Bearer {KEY}")) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"Invalid API key"}})),
        )
            .into_response();
    }
    let model = body["model"].as_str().unwrap_or("gemini").to_string();
    if method == Method::GET && path == "/v1/sub2api/billing" {
        let group = mock.group_name.lock().unwrap().clone();
        return Json(json!({"object":"sub2api.key_billing","schema_version":1,"group_name":group}))
            .into_response();
    }
    if method == Method::GET
        && path == "/v1/models"
        && let Some(listing) = mock.listing.lock().unwrap().clone()
    {
        return Json(listing).into_response();
    }
    if method == Method::POST
        && let Some((status, body)) = mock.fail_posts.lock().unwrap().clone()
    {
        return (StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
    }
    if method == Method::GET {
        let listing = if path.ends_with("/v1beta/models") {
            json!({"models":[{"name":"models/gemini-2.5-pro"},{"name":"models/gemini-3.8-flash"}]})
        } else {
            json!({"object":"list","data":[{"id":"claude-sonnet-4-6","type":"model"},{"id":"gemini-3.8-flash","type":"model"}]})
        };
        return Json(listing).into_response();
    }
    if path.ends_with("/v1/messages") {
        return Json(json!({
            "id":"msg_1","type":"message","role":"assistant","model":model,
            "content":[{"type":"text","text":"pong-messages"}],
            "stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":1,"output_tokens":1}
        }))
        .into_response();
    }
    if path.ends_with("/v1/chat/completions") {
        return Json(json!({
            "id":"chatcmpl-1","object":"chat.completion","created":0,"model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":"pong-chat"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }))
        .into_response();
    }
    if path.ends_with("/v1/responses") {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            responses_stream(&model),
        )
            .into_response();
    }
    let gemini = json!({
        "candidates":[{"content":{"role":"model","parts":[{"text":"pong-gemini"}]},"finishReason":"STOP","index":0}],
        "usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}
    });
    if query.contains("alt=sse") {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            sse(&[gemini]),
        )
            .into_response();
    }
    Json(gemini).into_response()
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

async fn mock_sub2api() -> (Mock, String) {
    let mock = Mock::default();
    let url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;
    (mock, url)
}

async fn gateway(dir: &tempfile::TempDir) -> Gateway {
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    Gateway::new(config).await.expect("gateway init").0
}

/// A gateway whose request logs are persisted (the collector flushes every 2s).
async fn logged_gateway(dir: &tempfile::TempDir) -> Gateway {
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let (gw, logs) = Gateway::new(config).await.expect("gateway init");
    tokio::spawn(nyro_core::logging::run_collector(logs, gw.storage.clone()));
    gw
}

/// Create a Sub2API provider the way the WebUI does (shared key, one
/// endpoint per checked protocol), but with the raw pasted address and
/// `auto` schemes to prove the admin layer normalizes both.
async fn sub2api_provider(
    gw: &Gateway,
    channel: &str,
    address: &str,
    protocols: &[&str],
    fast_mode: OpenAiSpeedMode,
) -> anyhow::Result<String> {
    let endpoints: Vec<CreateProviderProtocolEndpoint> = protocols
        .iter()
        .enumerate()
        .map(|(index, protocol)| CreateProviderProtocolEndpoint {
            protocol: (*protocol).into(),
            base_url: address.into(),
            api_key: KEY.into(),
            auth_scheme: "auto".into(),
            is_enabled: true,
            priority: index as i32,
        })
        .collect();
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: format!("sub2api-{channel}"),
            vendor: Some("sub2api".into()),
            protocol: protocols[0].into(),
            base_url: address.into(),
            protocol_mode: "adaptive".into(),
            protocol_endpoints: endpoints,
            preset_key: Some("sub2api".into()),
            channel: Some(channel.into()),
            models_source: None,
            static_models: None,
            api_key: KEY.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode,
        })
        .await?
        .id)
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

async fn post(url: String, body: Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(url)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("nyro response");
    (
        response.status().as_u16(),
        response.text().await.unwrap_or_default(),
    )
}

fn assert_bearer_only(hit: &Hit) {
    assert_eq!(
        hit.authorization.as_deref(),
        Some(format!("Bearer {KEY}").as_str()),
        "{hit:?}"
    );
    assert!(hit.x_api_key.is_none(), "no x-api-key rewrite: {hit:?}");
    assert!(!hit.query.contains("key="), "no query key: {hit:?}");
}

#[tokio::test]
async fn antigravity_routes_messages_and_gemini_under_the_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    // Users paste the address shown by Sub2API's "Use Key" dialog.
    let provider = sub2api_provider(
        &gw,
        "antigravity",
        &format!("{mock_url}/antigravity/v1"),
        &[MESSAGES, GEMINI],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    let stored = gw.admin().get_provider(&provider).await.unwrap();
    assert_eq!(stored.base_url, mock_url);
    assert_eq!(
        stored.models_source.as_deref(),
        Some(format!("{mock_url}/v1/models").as_str())
    );
    for endpoint in &stored.protocol_endpoints {
        assert_eq!(endpoint.base_url, mock_url, "{endpoint:?}");
        assert_eq!(endpoint.auth_scheme, "bearer", "{endpoint:?}");
        assert_eq!(endpoint.test_status, "success", "{endpoint:?}");
    }
    // The connectivity check used the prefixed listing routes (the
    // background quota refresh reads `/v1/usage` from the root).
    assert_eq!(
        {
            let mut gets = mock.gets();
            gets.retain(|path| path != "/v1/usage");
            gets.sort();
            gets
        },
        ["/antigravity/v1/models", "/antigravity/v1beta/models"]
    );

    route(&gw, "claude", &provider, "claude-sonnet-4-6").await;
    route(&gw, "flash", &provider, "gemini-3.8-flash").await;
    let nyro = serve(create_router(gw)).await;

    let (status, body) = post(
        format!("{nyro}/v1/messages"),
        json!({"model":"claude","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-messages"), "{body}");

    let (status, body) = post(
        format!("{nyro}/v1beta/models/flash:generateContent"),
        json!({"contents":[{"role":"user","parts":[{"text":"ping"}]}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-gemini"), "{body}");

    // Chat is not checked: Nyro converts it to the default Messages route.
    let (status, body) = post(
        format!("{nyro}/v1/chat/completions"),
        json!({"model":"claude","messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-messages"), "{body}");

    let posts = mock.posts();
    let paths: Vec<&str> = posts.iter().map(|hit| hit.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/antigravity/v1/messages",
            "/antigravity/v1beta/models/gemini-3.8-flash:generateContent",
            "/antigravity/v1/messages",
        ]
    );
    for hit in &posts {
        assert_bearer_only(hit);
    }
    assert_eq!(posts[0].body["model"], "claude-sonnet-4-6");
    assert_eq!(posts[2].body["model"], "claude-sonnet-4-6");
}

#[tokio::test]
async fn gemini_platform_converts_other_clients_to_v1beta_with_bearer() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let provider = sub2api_provider(
        &gw,
        "gemini",
        &format!("{mock_url}/v1beta"),
        &[GEMINI],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    let stored = gw.admin().get_provider(&provider).await.unwrap();
    assert_eq!(
        stored.models_source.as_deref(),
        Some(format!("{mock_url}/v1beta/models").as_str())
    );
    // Discovery reads the Gemini listing with Bearer and strips `models/`.
    let mut models = gw.admin().test_provider_models(&provider).await.unwrap();
    models.sort();
    assert_eq!(models, ["gemini-2.5-pro", "gemini-3.8-flash"]);

    route(&gw, "pro", &provider, "gemini-2.5-pro").await;
    let nyro = serve(create_router(gw)).await;
    let (status, body) = post(
        format!("{nyro}/v1/chat/completions"),
        json!({"model":"pro","messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-gemini"), "{body}");

    let posts = mock.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(
        posts[0].path,
        "/v1beta/models/gemini-2.5-pro:generateContent"
    );
    assert_bearer_only(&posts[0]);
    assert!(
        mock.hits
            .lock()
            .unwrap()
            .iter()
            .all(|hit| !hit.query.contains("key=")),
        "discovery and dispatch never use a query key"
    );
}

#[tokio::test]
async fn openai_platform_keeps_the_codex_consumer_contract() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let provider = sub2api_provider(
        &gw,
        "openai",
        &format!("{mock_url}/v1"),
        &[RESPONSES],
        OpenAiSpeedMode::Fast,
    )
    .await
    .unwrap();
    route(&gw, "gpt", &provider, "gpt-5.6").await;
    let nyro = serve(create_router(gw)).await;

    // A Chat client is converted to Responses; the Codex contract strips the
    // sampling parameters and Fast mode forces the priority tier.
    let (status, body) = post(
        format!("{nyro}/v1/chat/completions"),
        json!({"model":"gpt","temperature":0.3,"top_p":0.9,"messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-responses"), "{body}");

    let posts = mock.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let hit = &posts[0];
    assert_eq!(hit.path, "/v1/responses");
    assert_bearer_only(hit);
    assert_eq!(hit.body["model"], "gpt-5.6");
    assert_eq!(hit.body["service_tier"], "priority");
    for stripped in ["temperature", "top_p", "max_output_tokens"] {
        assert!(hit.body.get(stripped).is_none(), "{stripped}: {}", hit.body);
    }
}

#[tokio::test]
async fn multi_protocol_platform_passes_each_protocol_through() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let provider = sub2api_provider(
        &gw,
        "kimi",
        &mock_url,
        &[CHAT, MESSAGES, RESPONSES],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    route(&gw, "kimi", &provider, "kimi-k3").await;
    let nyro = serve(create_router(gw)).await;

    let (status, body) = post(
        format!("{nyro}/v1/chat/completions"),
        json!({"model":"kimi","messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        format!("{nyro}/v1/messages"),
        json!({"model":"kimi","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-messages"), "{body}");

    let posts = mock.posts();
    let paths: Vec<&str> = posts.iter().map(|hit| hit.path.as_str()).collect();
    assert_eq!(paths, ["/v1/chat/completions", "/v1/messages"]);
    for hit in &posts {
        assert_bearer_only(hit);
    }
}

#[tokio::test]
async fn platform_mismatch_fails_the_connectivity_check_with_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    // The key belongs to a non-gemini group: `/v1beta` answers 400.
    mock.reject.lock().unwrap().push("/v1beta/models".into());
    let gw = gateway(&dir).await;
    let provider = sub2api_provider(&gw, "gemini", &mock_url, &[GEMINI], OpenAiSpeedMode::Normal)
        .await
        .unwrap();
    let result = gw.admin().test_provider(&provider).await.unwrap();
    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("platform"), "{error}");
}

#[tokio::test]
async fn unsupported_platforms_are_rejected_on_save() {
    let dir = tempfile::tempdir().unwrap();
    let gw = gateway(&dir).await;
    for channel in ["typesafe", "default", ""] {
        let error = sub2api_provider(
            &gw,
            channel,
            "https://s.example.invalid",
            &[CHAT],
            OpenAiSpeedMode::Normal,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Sub2API platform"), "{channel}: {error}");
    }
}

#[tokio::test]
async fn model_probes_follow_the_platform_routes_with_bearer() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let antigravity = sub2api_provider(
        &gw,
        "antigravity",
        &mock_url,
        &[MESSAGES, GEMINI],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    let gemini = sub2api_provider(&gw, "gemini", &mock_url, &[GEMINI], OpenAiSpeedMode::Normal)
        .await
        .unwrap();

    let outcome = gw
        .admin()
        .probe_provider_models(&antigravity, Some(vec!["claude-sonnet-4-6".into()]))
        .await
        .unwrap();
    assert!(outcome.results[0].success, "{:?}", outcome.results[0]);
    let outcome = gw
        .admin()
        .probe_provider_models(&gemini, Some(vec!["gemini-2.5-pro".into()]))
        .await
        .unwrap();
    assert!(outcome.results[0].success, "{:?}", outcome.results[0]);

    let posts = mock.posts();
    let routes: Vec<(&str, &str)> = posts
        .iter()
        .map(|hit| (hit.path.as_str(), hit.query.as_str()))
        .collect();
    assert_eq!(
        routes,
        [
            ("/antigravity/v1/messages", ""),
            (
                "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
                "alt=sse"
            ),
        ]
    );
    for hit in &posts {
        assert_bearer_only(hit);
    }
    // A Sub2API Antigravity probe is never wrapped in a v1internal envelope.
    assert!(posts[1].body.get("request").is_none(), "{}", posts[1].body);
}

#[tokio::test]
async fn compat_paths_keep_platform_routes_and_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let antigravity = sub2api_provider(
        &gw,
        "antigravity",
        &mock_url,
        &[MESSAGES, GEMINI],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    let openai = sub2api_provider(
        &gw,
        "openai",
        &mock_url,
        &[RESPONSES],
        OpenAiSpeedMode::Fast,
    )
    .await
    .unwrap();
    route(&gw, "agy", &antigravity, "claude-sonnet-4-6").await;
    route(&gw, "gpt", &openai, "gpt-5.6").await;
    let nyro = serve(create_router(gw)).await;

    // Codex (Responses) against Antigravity: converted to Messages under the prefix.
    let (status, body) = post(
        format!("{nyro}/v1/responses"),
        json!({"model":"agy","input":"ping","stream":false}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-messages"), "{body}");

    // Claude Code (Messages) against the OpenAI platform: Codex consumer flavor.
    let (status, body) = post(
        format!("{nyro}/v1/messages"),
        json!({"model":"gpt","max_tokens":64,"temperature":0.2,"messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-responses"), "{body}");

    let posts = mock.posts();
    let paths: Vec<&str> = posts.iter().map(|hit| hit.path.as_str()).collect();
    assert_eq!(paths, ["/antigravity/v1/messages", "/v1/responses"]);
    for hit in &posts {
        assert_bearer_only(hit);
    }
    assert_eq!(posts[0].body["model"], "claude-sonnet-4-6");
    let codex = &posts[1].body;
    assert_eq!(codex["model"], "gpt-5.6");
    assert_eq!(codex["store"], false, "{codex}");
    assert_eq!(codex["stream"], true, "{codex}");
    assert_eq!(codex["service_tier"], "priority", "{codex}");
    for stripped in ["temperature", "top_p", "max_output_tokens"] {
        assert!(codex.get(stripped).is_none(), "{stripped}: {codex}");
    }
}

// ── P2: upstream usage, rejection failover and platform detection ─────────────

async fn custom_chat_provider(gw: &Gateway, name: &str, base: &str) -> String {
    gw.admin()
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: name.into(),
            vendor: Some("custom".into()),
            protocol: "openai-compatible".into(),
            base_url: format!("{base}/v1"),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("custom".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: KEY.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: Default::default(),
        })
        .await
        .expect("create backup provider")
        .id
}

async fn failover_route(gw: &Gateway, name: &str, primary: &str, backup: &str, model: &str) {
    use nyro_core::db::models::CreateModelBackend;
    let target = |provider: &str, priority| CreateModelBackend {
        provider_id: provider.into(),
        model: model.into(),
        weight: Some(100),
        priority: Some(priority),
        is_fallback: Some(false),
    };
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: primary.into(),
            target_model: model.into(),
            targets: vec![target(primary, 1), target(backup, 2)],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await
        .expect("create failover route");
}

/// Wait for the attempt log of `provider` and return its recorded causes.
async fn attempt_causes(gw: &Gateway, provider: &str) -> String {
    use nyro_core::db::models::LogQuery;
    for _ in 0..200 {
        let page = gw.admin().query_logs(LogQuery::default()).await.unwrap();
        for item in page.items {
            if item.provider_id.as_deref() == Some(provider) {
                let row = gw.admin().get_log(&item.id).await.unwrap().unwrap();
                return row.error_causes.unwrap_or_default();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no attempt log for provider {provider}");
}

#[tokio::test]
async fn usage_uses_local_accounting_without_polling_upstream() {
    use nyro_core::admin::ProviderSchedulingStatus;
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let provider = sub2api_provider(
        &gw,
        "openai",
        &mock_url,
        &[RESPONSES],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    let usage = gw.admin().get_provider_usage(&provider).await.unwrap();
    assert_eq!(usage.kind, "sub2api");
    assert_eq!(usage.site, "local");
    assert!(usage.tiers.is_empty());
    assert!(usage.balances.is_empty());
    assert!(usage.spends.is_empty());
    assert_eq!(usage.is_available, None);
    assert!(usage.api_price.is_some());
    assert_eq!(usage.scheduling.status, ProviderSchedulingStatus::Eligible);
    assert!(gw.quota_registry.is_schedulable(&provider));
    assert!(
        !mock.gets().iter().any(|path| path == "/v1/usage"),
        "Sub2API usage must not poll upstream /v1/usage: {:?}",
        mock.gets()
    );
}

#[tokio::test]
async fn billing_rejection_fails_over_records_a_hint_without_pausing_via_upstream_usage() {
    use nyro_core::admin::ProviderSchedulingStatus;
    let dir = tempfile::tempdir().unwrap();
    let (relay, relay_url) = mock_sub2api().await;
    let (backup, backup_url) = mock_sub2api().await;
    let gw = logged_gateway(&dir).await;
    let primary = sub2api_provider(&gw, "kimi", &relay_url, &[CHAT], OpenAiSpeedMode::Normal)
        .await
        .unwrap();
    let secondary = custom_chat_provider(&gw, "backup", &backup_url).await;
    failover_route(&gw, "kimi", &primary, &secondary, "kimi-k3").await;

    *relay.fail_posts.lock().unwrap() = Some((
        403,
        json!({"type":"error","error":{"type":"billing_error","message":"insufficient balance"}}),
    ));
    let nyro = serve(create_router(gw.clone())).await;
    let (status, body) = post(
        format!("{nyro}/v1/chat/completions"),
        json!({"model":"kimi","messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("pong-chat"), "{body}");
    assert_eq!(relay.posts().len(), 1);
    assert_eq!(backup.posts().len(), 1);

    let causes = attempt_causes(&gw, &primary).await;
    assert!(
        causes.contains("Sub2API rejected the key for billing"),
        "{causes}"
    );
    assert_eq!(
        gw.quota_registry.snapshot(&primary).status,
        ProviderSchedulingStatus::Eligible
    );
    assert!(
        !relay.gets().iter().any(|path| path == "/v1/usage"),
        "billing rejection must not trigger upstream /v1/usage polling: {:?}",
        relay.gets()
    );
}

#[tokio::test]
async fn configuration_rejections_fail_over_even_on_404() {
    let dir = tempfile::tempdir().unwrap();
    let (relay, relay_url) = mock_sub2api().await;
    let (backup, backup_url) = mock_sub2api().await;
    *relay.fail_posts.lock().unwrap() = Some((
        404,
        json!({"error":{"type":"not_found_error","message":"Model \"kimi-k3\" is not available for this group"}}),
    ));
    let gw = logged_gateway(&dir).await;
    let primary = sub2api_provider(&gw, "kimi", &relay_url, &[CHAT], OpenAiSpeedMode::Normal)
        .await
        .unwrap();
    let secondary = custom_chat_provider(&gw, "backup", &backup_url).await;
    failover_route(&gw, "kimi", &primary, &secondary, "kimi-k3").await;
    let nyro = serve(create_router(gw.clone())).await;
    let (status, body) = post(
        format!("{nyro}/v1/chat/completions"),
        json!({"model":"kimi","messages":[{"role":"user","content":"ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((relay.posts().len(), backup.posts().len()), (1, 1));
    let causes = attempt_causes(&gw, &primary).await;
    assert!(
        causes.contains("not enabled for the Sub2API key's group"),
        "{causes}"
    );
}

#[tokio::test]
async fn model_probe_explains_sub2api_rejections() {
    let dir = tempfile::tempdir().unwrap();
    let (relay, relay_url) = mock_sub2api().await;
    *relay.fail_posts.lock().unwrap() = Some((
        403,
        json!({"type":"error","error":{"type":"permission_error","message":"This group does not allow /v1/messages dispatch"}}),
    ));
    let gw = gateway(&dir).await;
    let provider = sub2api_provider(
        &gw,
        "openai",
        &relay_url,
        &[MESSAGES],
        OpenAiSpeedMode::Normal,
    )
    .await
    .unwrap();
    let outcome = gw
        .admin()
        .probe_provider_models(&provider, Some(vec!["gpt-5.6".into()]))
        .await
        .unwrap();
    let error = outcome.results[0].error.clone().unwrap_or_default();
    assert!(error.contains("Uncheck Anthropic Messages"), "{error}");
}

#[tokio::test]
async fn platform_detection_reads_listing_shape_routes_and_group_name() {
    use nyro_core::admin::{Sub2ApiDetectConfidence, Sub2ApiDetectInput};
    let dir = tempfile::tempdir().unwrap();
    let (mock, mock_url) = mock_sub2api().await;
    let gw = gateway(&dir).await;
    let detect = |base: String, key: &str| {
        let gw = gw.clone();
        let key = key.to_string();
        async move {
            gw.admin()
                .detect_sub2api_platform(Sub2ApiDetectInput {
                    base_url: base,
                    api_key: key,
                    use_proxy: false,
                })
                .await
        }
    };

    // Antigravity: Claude + Gemini models, /v1beta rejected for the group.
    mock.reject.lock().unwrap().push("/v1beta/models".into());
    *mock.group_name.lock().unwrap() = Some("Antigravity Pro".into());
    let found = detect(format!("{mock_url}/antigravity/v1"), KEY)
        .await
        .unwrap();
    assert_eq!(found.platform.as_deref(), Some("antigravity"));
    assert_eq!(found.confidence, Sub2ApiDetectConfidence::High);
    assert_eq!(found.gemini_route, Some(false));
    assert_eq!(found.group_name.as_deref(), Some("Antigravity Pro"));
    assert_eq!(found.model_count, 2);

    // OpenAI: Sub2API's OpenAI listing format.
    *mock.listing.lock().unwrap() = Some(json!({"object":"list","data":[
        {"id":"gpt-5.6","object":"model","created":1,"owned_by":"openai","type":"model","display_name":"gpt-5.6"}
    ]}));
    let found = detect(mock_url.clone(), KEY).await.unwrap();
    assert_eq!(found.platform.as_deref(), Some("openai"));

    // Gemini: /v1beta accepted with Gemini models only.
    mock.reject.lock().unwrap().clear();
    *mock.listing.lock().unwrap() = Some(json!({"object":"list","data":[
        {"id":"gemini-2.5-pro","type":"model","display_name":"x","created_at":"2024-01-01T00:00:00Z"}
    ]}));
    let found = detect(mock_url.clone(), KEY).await.unwrap();
    assert_eq!(found.platform.as_deref(), Some("gemini"));
    assert_eq!(found.gemini_route, Some(true));

    // A wrong key is an error, not a guess; detection never posts.
    let error = detect(mock_url.clone(), "sk-wrong")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid or disabled"), "{error}");
    assert!(mock.posts().is_empty());
}
