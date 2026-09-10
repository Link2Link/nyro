//! OpenCode Go routing identity, exercised through the real dispatcher.
//!
//! The Go surface rejects any request without a per-conversation
//! `x-opencode-session` (HTTP 400 MissingSessionID), so the gateway has to add
//! one on every egress path. These tests pin that wiring against a mock
//! upstream: the header must arrive, stay stable across the turns of one
//! conversation, differ between conversations, survive both the native
//! passthrough and the raw-wire compat build paths, and never overwrite a
//! client-supplied value.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};

/// One captured upstream request: the routed model plus the session header.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

#[derive(Clone)]
struct Upstream {
    seen: Seen,
}

async fn upstream_handler(
    State(state): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.seen.lock().unwrap().push((
        body["model"].as_str().unwrap_or_default().to_string(),
        headers
            .get("x-opencode-session")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    ));
    let reply = json!({
        "id": "r",
        "model": "ok",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    });
    (StatusCode::OK, Json(reply)).into_response()
}

async fn setup() -> anyhow::Result<(tempfile::TempDir, Gateway, String, Upstream)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, _logs) = Gateway::from_storage(config, storage).await?;

    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = Upstream { seen: seen.clone() };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let router = Router::new()
        .route("/v1/chat/completions", post(upstream_handler))
        .with_state(upstream.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((dir, gw, url, upstream))
}

async fn opencode_provider(gw: &Gateway, url: &str) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            name: "opencode-go-test".into(),
            vendor: Some("opencode-go".into()),
            protocol: "openai-compatible".into(),
            base_url: url.into(),
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

/// Dispatch one chat request for `name` with `prompt` as the opening turn.
async fn dispatch_chat(
    gw: &Gateway,
    name: &str,
    ingress: ProtocolId,
    prompt: &str,
    client_headers: HeaderMap,
) -> StatusCode {
    let body = if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
        json!({
            "model": name,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 20,
        })
    } else {
        json!({
            "model": name,
            "messages": [{"role": "user", "content": prompt}],
        })
    };
    let request = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())
        .unwrap();
    let ctx = RequestContext::new(ingress, Duration::from_secs(5));
    let response = dispatch_pipeline(
        gw.clone(),
        client_headers,
        RawEnvelope::new(
            Some(body),
            HashMap::new(),
            "POST",
            if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
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
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    status
}

fn session_of(seen: &Seen, index: usize) -> Option<String> {
    seen.lock().unwrap()[index].1.clone()
}

#[tokio::test]
async fn session_header_is_added_and_stays_stable_per_conversation() -> anyhow::Result<()> {
    let (_dir, gw, url, upstream) = setup().await?;
    let provider = opencode_provider(&gw, &url).await?;
    route(&gw, "oc-chat", provider.clone(), "glm-5.3").await?;
    route(&gw, "oc-anthropic", provider, "glm-5.3").await?;

    // Same conversation, first turn — native passthrough path.
    assert_eq!(
        dispatch_chat(
            &gw,
            "oc-chat",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "hello",
            HeaderMap::new(),
        )
        .await,
        StatusCode::OK,
    );
    // Same conversation, second turn — still native passthrough.
    assert_eq!(
        dispatch_chat(
            &gw,
            "oc-chat",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "hello",
            HeaderMap::new(),
        )
        .await,
        StatusCode::OK,
    );
    // Same conversation shape through the raw-wire compat path
    // (Anthropic ingress → OpenAI chat egress), which never touches the
    // vendor pipeline.
    assert_eq!(
        dispatch_chat(
            &gw,
            "oc-anthropic",
            ANTHROPIC_MESSAGES_2023_06_01,
            "hello",
            HeaderMap::new(),
        )
        .await,
        StatusCode::OK,
    );
    // A different conversation must not reuse the identity.
    assert_eq!(
        dispatch_chat(
            &gw,
            "oc-chat",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "an entirely different opening turn",
            HeaderMap::new(),
        )
        .await,
        StatusCode::OK,
    );

    let seen = &upstream.seen;
    assert_eq!(
        seen.lock().unwrap().len(),
        4,
        "all four requests reached upstream"
    );
    for index in 0..4 {
        let session = session_of(seen, index);
        assert!(
            session
                .as_deref()
                .is_some_and(|value| value.starts_with("nyro-")),
            "request {index} must carry a gateway session id, got {session:?}",
        );
    }
    assert_eq!(
        session_of(seen, 0),
        session_of(seen, 1),
        "turns of one conversation share a routing identity",
    );
    assert_eq!(
        session_of(seen, 0),
        session_of(seen, 2),
        "the same conversation keeps its identity across the compat path",
    );
    assert_ne!(
        session_of(seen, 0),
        session_of(seen, 3),
        "distinct conversations get distinct identities",
    );
    Ok(())
}

#[tokio::test]
async fn client_supplied_session_is_forwarded_untouched() -> anyhow::Result<()> {
    let (_dir, gw, url, upstream) = setup().await?;
    let provider = opencode_provider(&gw, &url).await?;
    route(&gw, "oc-client-session", provider, "glm-5.3").await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-opencode-session",
        "client-owned-session".parse().unwrap(),
    );

    assert_eq!(
        dispatch_chat(
            &gw,
            "oc-client-session",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "hello",
            headers,
        )
        .await,
        StatusCode::OK,
    );

    assert_eq!(
        session_of(&upstream.seen, 0).as_deref(),
        Some("client-owned-session"),
        "a client-provided routing identity must win over the derived one",
    );
    Ok(())
}

#[tokio::test]
async fn other_vendors_do_not_gain_the_opencode_header() -> anyhow::Result<()> {
    let (_dir, gw, url, upstream) = setup().await?;
    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            name: "zhipu-test".into(),
            vendor: Some("zhipuai".into()),
            protocol: "openai-compatible".into(),
            base_url: url,
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
    route(&gw, "glm", provider, "glm-5.3").await?;

    assert_eq!(
        dispatch_chat(
            &gw,
            "glm",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "hello",
            HeaderMap::new(),
        )
        .await,
        StatusCode::OK,
    );

    assert_eq!(
        session_of(&upstream.seen, 0),
        None,
        "session identity stays scoped to the OpenCode Go channel",
    );
    Ok(())
}

#[tokio::test]
async fn upstream_body_and_url_stay_intact() -> anyhow::Result<()> {
    let (_dir, gw, url, upstream) = setup().await?;
    let provider = opencode_provider(&gw, &url).await?;
    route(&gw, "oc-fidelity", provider, "glm-5.3").await?;

    assert_eq!(
        dispatch_chat(
            &gw,
            "oc-fidelity",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "hello",
            HeaderMap::new(),
        )
        .await,
        StatusCode::OK,
    );

    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    // The mock only answers the canonical chat path and echoes the routed
    // model, so reaching it at all proves the identity pass left the request
    // otherwise untouched.
    assert_eq!(seen[0].0, "glm-5.3");
    Ok(())
}
