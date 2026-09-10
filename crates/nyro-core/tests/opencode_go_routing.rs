//! OpenCode Go per-model endpoint routing, exercised through the real
//! dispatcher and the real admin probe against a mock upstream that serves all
//! three protocol shapes.
//!
//! The Go plan does not serve every model on every endpoint, and an endpoint
//! that does not serve a model answers with an opaque HTTP 500 — so the mapping
//! is hardcoded in `provider::opencode_go::routing`. These tests pin the
//! resulting wire behaviour: native when the model serves the client's
//! protocol, transcoded onto the model's own endpoint otherwise, with the
//! Anthropic endpoint authenticated by `x-api-key` and every request carrying
//! the conversation's `x-opencode-session`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider, CreateProviderProtocolEndpoint},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};

/// Endpoint tag recorded for each upstream request.
type Endpoint = &'static str;

#[derive(Debug, Clone)]
struct Seen {
    endpoint: Endpoint,
    model: String,
    session: Option<String>,
    authorization: Option<String>,
    api_key: Option<String>,
}

#[derive(Clone)]
struct Upstream {
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    fn record(&self, endpoint: Endpoint, headers: &HeaderMap, body: &Value) {
        self.seen.lock().unwrap().push(Seen {
            endpoint,
            model: body["model"].as_str().unwrap_or_default().to_string(),
            session: header(headers, "x-opencode-session"),
            authorization: header(headers, "authorization"),
            api_key: header(headers, "x-api-key"),
        });
    }

    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("no upstream call")
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn chat_reply() -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "glm-5.3",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
}

fn responses_reply() -> Value {
    json!({
        "id": "resp_test",
        "object": "response",
        "status": "completed",
        "model": "grok-4.6",
        "output": [{
            "type": "message",
            "id": "msg_test",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "hello", "annotations": []}],
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
    })
}

fn messages_reply() -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "minimax-m2.7",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
}

async fn chat_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.record("chat", &headers, &body);
    (StatusCode::OK, Json(chat_reply())).into_response()
}

async fn responses_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.record("responses", &headers, &body);
    (StatusCode::OK, Json(responses_reply())).into_response()
}

async fn messages_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.record("messages", &headers, &body);
    (StatusCode::OK, Json(messages_reply())).into_response()
}

/// Model catalog the admin list/probe surfaces read, including two models the
/// Go plan advertises but does not serve.
async fn models_handler() -> Response {
    let ids = [
        "glm-5.3",
        "grok-4.6",
        "minimax-m2.7",
        "glm-5",
        "qwen3.5-plus",
    ];
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "object": "model"}))
        .collect();
    (
        StatusCode::OK,
        Json(json!({"object": "list", "data": data})),
    )
        .into_response()
}

async fn setup() -> anyhow::Result<(tempfile::TempDir, Gateway, Upstream, String)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, _logs) = Gateway::from_storage(config, storage).await?;

    let upstream = Upstream {
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let router = Router::new()
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/messages", post(messages_handler))
        .route("/v1/models", get(models_handler))
        .with_state(upstream.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((dir, gw, upstream, format!("http://{addr}/v1")))
}

/// Adaptive OpenCode Go provider declaring all three endpoints, mirroring what
/// the preset seeds after this change.
async fn adaptive_opencode_provider(gw: &Gateway, base_url: &str) -> anyhow::Result<String> {
    let endpoint =
        |protocol: &str, auth_scheme: &str, priority: i32| CreateProviderProtocolEndpoint {
            protocol: protocol.into(),
            base_url: base_url.into(),
            api_key: "sk-test".into(),
            auth_scheme: auth_scheme.into(),
            is_enabled: true,
            priority,
        };
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            name: "opencode-go-adaptive".into(),
            vendor: Some("opencode-go".into()),
            protocol: "openai-compatible".into(),
            base_url: base_url.into(),
            protocol_mode: "adaptive".into(),
            protocol_endpoints: vec![
                endpoint("openai-compatible/chat-completions/v1", "auto", 0),
                endpoint("openai-responses/responses/v1", "auto", 1),
                endpoint("anthropic-messages/messages/2023-06-01", "x-api-key", 2),
            ],
            preset_key: Some("opencode-go".into()),
            channel: Some("default".into()),
            models_source: Some(format!("{base_url}/models")),
            static_models: None,
            api_key: "sk-test".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}

async fn route(gw: &Gateway, name: &str, provider: String, target: &str) -> anyhow::Result<()> {
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: provider,
            target_model: target.into(),
            targets: vec![],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}

/// Dispatch one non-streaming request with `prompt` as the opening turn.
async fn dispatch(gw: &Gateway, name: &str, ingress: ProtocolId, prompt: &str) -> StatusCode {
    let body = match ingress {
        ANTHROPIC_MESSAGES_2023_06_01 => json!({
            "model": name,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 64,
        }),
        OPENAI_RESPONSES_V1 => json!({
            "model": name,
            "input": prompt,
            "max_output_tokens": 64,
        }),
        _ => json!({
            "model": name,
            "messages": [{"role": "user", "content": prompt}],
        }),
    };
    let request = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())
        .unwrap();
    let path = match ingress {
        ANTHROPIC_MESSAGES_2023_06_01 => "/v1/messages",
        OPENAI_RESPONSES_V1 => "/v1/responses",
        _ => "/v1/chat/completions",
    };
    let ctx = RequestContext::new(ingress, Duration::from_secs(5));
    let response = dispatch_pipeline(
        gw.clone(),
        HeaderMap::new(),
        RawEnvelope::new(Some(body), HashMap::new(), "POST", path),
        request,
        ingress,
        ctx,
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    assert!(
        status == StatusCode::OK,
        "dispatch failed ({status}): {}",
        String::from_utf8_lossy(&bytes)
    );
    status
}

#[tokio::test]
async fn routes_each_model_to_the_endpoint_that_serves_it() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = adaptive_opencode_provider(&gw, &base_url).await?;
    route(&gw, "oc-glm", provider.clone(), "glm-5.3").await?;
    route(&gw, "oc-grok", provider.clone(), "grok-4.6").await?;
    route(&gw, "oc-minimax", provider, "minimax-m2.7").await?;

    // Chat-only model from a chat client: native passthrough.
    dispatch(
        &gw,
        "oc-glm",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "hello",
    )
    .await;
    assert_eq!(upstream.last().endpoint, "chat");
    assert_eq!(upstream.last().model, "glm-5.3");

    // Responses-only model from a chat client: transcoded onto /v1/responses.
    dispatch(
        &gw,
        "oc-grok",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "hello",
    )
    .await;
    assert_eq!(
        upstream.last().endpoint,
        "responses",
        "grok-4.6 is not served on /v1/chat/completions",
    );

    // Responses-only model from Claude Code: still /v1/responses.
    dispatch(&gw, "oc-grok", ANTHROPIC_MESSAGES_2023_06_01, "hello").await;
    assert_eq!(upstream.last().endpoint, "responses");

    // Chat-only model from Claude Code: downgraded to /v1/chat/completions,
    // otherwise the adaptive provider would pick the messages endpoint and 500.
    dispatch(&gw, "oc-glm", ANTHROPIC_MESSAGES_2023_06_01, "hello").await;
    assert_eq!(upstream.last().endpoint, "chat");

    // Chat-only model from a Codex client: downgraded to chat as well.
    dispatch(&gw, "oc-glm", OPENAI_RESPONSES_V1, "hello").await;
    assert_eq!(upstream.last().endpoint, "chat");

    // Messages-only model: the Anthropic endpoint, authenticated by x-api-key.
    dispatch(
        &gw,
        "oc-minimax",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "hello",
    )
    .await;
    let seen = upstream.last();
    assert_eq!(
        seen.endpoint, "messages",
        "minimax-m2.7 is served on /v1/messages only"
    );
    assert_eq!(seen.api_key.as_deref(), Some("sk-test"));
    assert_eq!(
        seen.authorization, None,
        "the Go messages endpoint takes x-api-key, not Bearer",
    );

    Ok(())
}

#[tokio::test]
async fn every_routed_request_carries_the_conversation_identity() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = adaptive_opencode_provider(&gw, &base_url).await?;
    route(&gw, "oc-grok", provider, "grok-4.6").await?;

    dispatch(&gw, "oc-grok", ANTHROPIC_MESSAGES_2023_06_01, "hello").await;
    dispatch(&gw, "oc-grok", ANTHROPIC_MESSAGES_2023_06_01, "hello").await;

    let seen = upstream.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    for entry in &seen {
        assert!(
            entry
                .session
                .as_deref()
                .is_some_and(|value| value.starts_with("nyro-")),
            "missing session identity: {entry:?}",
        );
    }
    assert_eq!(
        seen[0].session, seen[1].session,
        "turns of one conversation share a routing identity",
    );
    Ok(())
}

#[tokio::test]
async fn probe_follows_the_same_per_model_endpoints() -> anyhow::Result<()> {
    let (_dir, gw, _upstream, base_url) = setup().await?;
    let provider = adaptive_opencode_provider(&gw, &base_url).await?;

    let outcome = gw.admin().probe_provider_models(&provider).await?;
    let by_model: HashMap<String, (bool, String)> = outcome
        .results
        .iter()
        .map(|result| {
            (
                result.model.clone(),
                (result.success, result.protocol.clone()),
            )
        })
        .collect();

    assert_eq!(
        by_model.len(),
        3,
        "blacklisted models never reach the probe"
    );
    for (model, endpoint) in [
        ("glm-5.3", "openai-compatible/chat-completions/v1"),
        ("grok-4.6", "openai-responses/responses/v1"),
        ("minimax-m2.7", "anthropic-messages/messages/2023-06-01"),
    ] {
        let (success, protocol) = by_model
            .get(model)
            .unwrap_or_else(|| panic!("missing probe result for {model}"));
        assert!(*success, "{model} should probe successfully");
        assert_eq!(
            protocol, endpoint,
            "{model} probed through the wrong endpoint"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unavailable_models_are_hidden_from_the_model_list() -> anyhow::Result<()> {
    let (_dir, gw, _upstream, base_url) = setup().await?;
    let provider = adaptive_opencode_provider(&gw, &base_url).await?;

    let models = gw.admin().get_provider_models(&provider).await?;

    assert_eq!(models, vec!["glm-5.3", "grok-4.6", "minimax-m2.7"]);
    assert!(!models.iter().any(|model| model == "glm-5"));
    assert!(!models.iter().any(|model| model == "qwen3.5-plus"));
    Ok(())
}

#[tokio::test]
async fn fixed_providers_keep_their_single_endpoint_behaviour() -> anyhow::Result<()> {
    // A provider that predates the adaptive preset (or one the user left on
    // "fixed") declares only chat completions, so the vendor preference is not
    // supported and negotiation falls back to the ingress-driven endpoint.
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            name: "opencode-go-fixed".into(),
            vendor: Some("opencode-go".into()),
            protocol: "openai-compatible".into(),
            base_url: base_url.clone(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "sk-test".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id;
    route(&gw, "oc-fixed", provider, "grok-4.6").await?;

    dispatch(
        &gw,
        "oc-fixed",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "hello",
    )
    .await;

    assert_eq!(
        upstream.last().endpoint,
        "chat",
        "a fixed chat-only provider degrades to today's behaviour",
    );
    Ok(())
}
