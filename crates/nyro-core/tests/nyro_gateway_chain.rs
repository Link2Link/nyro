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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    response::{IntoResponse, Response},
    routing::post,
};
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
