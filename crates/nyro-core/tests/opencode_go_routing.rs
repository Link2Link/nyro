//! Strict OpenCode Go routing through the real dispatcher and admin probe.
//! The mock deliberately does NOT import the production routing table: it only
//! succeeds on a model's measured endpoint, with the correct auth and session.

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
    db::models::{CreateModel, CreateProvider, CreateProviderProtocolEndpoint, UpdateProvider},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};

type Endpoint = &'static str;
const INGRESS: [ProtocolId; 3] = [
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    OPENAI_RESPONSES_V1,
    ANTHROPIC_MESSAGES_2023_06_01,
];
const REPRESENTATIVES: [(&str, Endpoint); 3] = [
    ("glm-5.3", "chat"),
    ("grok-4.7", "responses"),
    ("claude-haiku-5-5", "messages"),
];

// Independent wire oracle: 31 current models plus four historical mappings.
// Never derive these expectations from provider::opencode_go::routing.
const MODEL_ENDPOINTS: &[(&str, Endpoint)] = &[
    ("gpt-5.6-luna", "responses"),
    ("gpt-6-luna", "responses"),
    ("grok-4.6", "responses"),
    ("grok-4.7", "responses"),
    ("muse-spark-1.2-contributor", "responses"),
    ("muse-spark-1.3-contributor", "responses"),
    ("claude-haiku-5-5", "messages"),
    ("minimax-m2.5", "messages"),
    ("minimax-m2.7", "messages"),
    ("minimax-m3", "messages"),
    ("qwen3.6-plus", "messages"),
    ("qwen3.7-max", "messages"),
    ("qwen3.7-plus", "messages"),
    ("qwen3.8-flash", "messages"),
    ("qwen3.8-max", "messages"),
    ("deepseek-v4-flash", "chat"),
    ("deepseek-v4-flash-vision-exp", "chat"),
    ("deepseek-v4-pro", "chat"),
    ("deepseek-v4.1-flash", "chat"),
    ("glm-5.1", "chat"),
    ("glm-5.2", "chat"),
    ("glm-5.3", "chat"),
    ("glm-5.3-flash", "chat"),
    ("hy3", "chat"),
    ("hy4-preview", "chat"),
    ("kimi-k2.6", "chat"),
    ("kimi-k2.7-code", "chat"),
    ("kimi-k3", "chat"),
    ("longcat-2.0", "chat"),
    ("longcat-2.5-preview-free", "chat"),
    ("mimo-v2.5", "chat"),
    ("mimo-v2.5-pro", "chat"),
    ("mimo-v2.6-flash", "chat"),
    ("mimo-v2.6-pro", "chat"),
    ("space-bunny", "chat"),
];

fn expected_endpoint(model: &str) -> Option<Endpoint> {
    MODEL_ENDPOINTS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(model.trim()))
        .map(|(_, endpoint)| *endpoint)
}

fn protocol(endpoint: Endpoint) -> ProtocolId {
    match endpoint {
        "chat" => OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "responses" => OPENAI_RESPONSES_V1,
        "messages" => ANTHROPIC_MESSAGES_2023_06_01,
        _ => panic!("unknown endpoint {endpoint}"),
    }
}

#[derive(Debug, Clone)]
struct Seen {
    endpoint: Endpoint,
    model: String,
    session: Option<String>,
    authorization: Option<String>,
    api_key: Option<String>,
    body: Value,
}

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<Seen>>>,
    // Simulates an upstream that unexpectedly stopped serving a pinned model.
    // The error is the same opaque 500 as a wrong OpenCode Go endpoint.
    reject_model: Arc<Mutex<Option<String>>>,
}

impl Upstream {
    fn calls(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    fn reply(&self, endpoint: Endpoint, headers: &HeaderMap, body: Value) -> Response {
        let seen = Seen {
            endpoint,
            model: body["model"].as_str().unwrap_or_default().to_string(),
            session: header(headers, "x-opencode-session"),
            authorization: header(headers, "authorization"),
            api_key: header(headers, "x-api-key"),
            body,
        };
        self.seen.lock().unwrap().push(seen.clone());
        if expected_endpoint(&seen.model) != Some(endpoint)
            || self.reject_model.lock().unwrap().as_deref() == Some(seen.model.as_str())
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"type":"error","error":{"type":"error","message":"Internal server error"}})),
            )
                .into_response();
        }
        if !valid_auth(&seen) {
            return (StatusCode::UNAUTHORIZED, "wrong endpoint authentication").into_response();
        }
        if !seen
            .session
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            return (StatusCode::BAD_REQUEST, "missing x-opencode-session").into_response();
        }
        let valid_body = match endpoint {
            "responses" => seen.body.get("input").is_some(),
            "messages" => seen.body["messages"].is_array() && seen.body["max_tokens"].is_number(),
            _ => seen.body["messages"].is_array(),
        };
        if !valid_body {
            return (StatusCode::BAD_REQUEST, "wrong request shape").into_response();
        }
        if seen.body["stream"].as_bool() == Some(true) {
            return (
                [("content-type", "text/event-stream")],
                stream_reply(endpoint, &seen.model),
            )
                .into_response();
        }
        Json(reply(endpoint, &seen.model)).into_response()
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn valid_auth(seen: &Seen) -> bool {
    if seen.endpoint == "messages" {
        seen.api_key.as_deref() == Some("sk-test") && seen.authorization.is_none()
    } else {
        seen.authorization.as_deref() == Some("Bearer sk-test") && seen.api_key.is_none()
    }
}

fn assert_dispatch_wire(
    seen: &Seen,
    model: &str,
    endpoint: Endpoint,
    ingress: ProtocolId,
    client_stream: bool,
) {
    // Native/IR Responses forces SSE for non-stream clients; Anthropic
    // raw-wire compat preserves the original flag. Probes use their own body.
    let upstream_stream =
        client_stream || (endpoint == "responses" && ingress != ANTHROPIC_MESSAGES_2023_06_01);
    assert_wire(seen, model, endpoint, upstream_stream);
}

fn assert_wire(seen: &Seen, model: &str, endpoint: Endpoint, upstream_stream: bool) {
    assert_eq!(seen.model, model, "wrong upstream model: {seen:?}");
    assert_eq!(seen.endpoint, endpoint, "wrong endpoint: {seen:?}");
    assert!(valid_auth(seen), "wrong endpoint authentication: {seen:?}");
    assert!(
        seen.session
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        "missing session: {seen:?}"
    );
    assert_eq!(
        seen.body["stream"].as_bool().unwrap_or(false),
        upstream_stream,
        "wrong upstream stream flag: {seen:?}"
    );
}

fn reply(endpoint: Endpoint, model: &str) -> Value {
    match endpoint {
        "chat" => json!({
            "id":"chatcmpl-test", "object":"chat.completion", "model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }),
        "responses" => json!({
            "id":"resp_test", "object":"response", "status":"completed", "model":model,
            "output":[{"type":"message","id":"msg_test","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"hello","annotations":[]}]}],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }),
        "messages" => json!({
            "id":"msg_test", "type":"message", "role":"assistant", "model":model,
            "content":[{"type":"text","text":"hello"}], "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
        _ => unreachable!(),
    }
}

// Minimal valid lifecycle fixtures, following conv_streaming.rs. These are
// explicit upstream wire events, not generated by production formatters.
fn stream_reply(endpoint: Endpoint, model: &str) -> String {
    let event = |name: &str, data: Value| format!("event: {name}\ndata: {data}\n\n");
    match endpoint {
        "chat" => {
            let chunk = |delta: Value, finish: Value| format!("data: {}\n\n", json!({
                "id":"chatcmpl-test","object":"chat.completion.chunk","model":model,
                "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
            }));
            [
                chunk(json!({"role":"assistant"}), Value::Null),
                chunk(json!({"content":"hello"}), Value::Null),
                chunk(json!({}), json!("stop")),
                "data: [DONE]\n\n".into(),
            ].concat()
        }
        "responses" => [
            event("response.created", json!({"type":"response.created","response":{"id":"resp_test","model":model,"status":"in_progress","output":[]}})),
            event("response.output_item.added", json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_test","type":"message","role":"assistant","content":[]}})),
            event("response.content_part.added", json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"msg_test","part":{"type":"output_text","text":""}})),
            event("response.output_text.delta", json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_test","delta":"hello"})),
            event("response.output_text.done", json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg_test","text":"hello"})),
            event("response.completed", json!({"type":"response.completed","response":reply(endpoint,model)})),
        ].concat(),
        "messages" => [
            event("message_start", json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}})),
            event("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            event("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}})),
            event("content_block_stop", json!({"type":"content_block_stop","index":0})),
            event("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})),
            event("message_stop", json!({"type":"message_stop"})),
        ].concat(),
        _ => unreachable!(),
    }
}

async fn chat_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.reply("chat", &headers, body)
}
async fn responses_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.reply("responses", &headers, body)
}
async fn messages_handler(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    upstream.reply("messages", &headers, body)
}

async fn models_handler() -> Response {
    let data: Vec<Value> = [
        "glm-5.3",
        "grok-4.6",
        "minimax-m2.7",
        "glm-5",
        "qwen3.5-plus",
    ]
    .iter()
    .map(|id| json!({"id":id,"object":"model"}))
    .collect();
    Json(json!({"object":"list","data":data})).into_response()
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
    let upstream = Upstream::default();
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

fn endpoints(base_url: &str) -> Vec<CreateProviderProtocolEndpoint> {
    ["chat", "responses", "messages"]
        .into_iter()
        .enumerate()
        .map(|(priority, endpoint)| CreateProviderProtocolEndpoint {
            protocol: protocol(endpoint).to_string(),
            base_url: base_url.into(),
            api_key: "sk-test".into(),
            auth_scheme: if endpoint == "messages" {
                "x-api-key"
            } else {
                "auto"
            }
            .into(),
            is_enabled: true,
            priority: priority as i32,
        })
        .collect()
}

async fn provider(
    gw: &Gateway,
    base_url: &str,
    mode: &str,
    default: Endpoint,
) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: format!("opencode-go-{mode}-{default}"),
            vendor: Some("opencode-go".into()),
            protocol: protocol(default).to_string(),
            base_url: base_url.into(),
            protocol_mode: mode.into(),
            protocol_endpoints: if mode == "adaptive" {
                endpoints(base_url)
            } else {
                vec![]
            },
            // Intentionally vendor-only and localhost: neither a preset nor an
            // opencode.ai URL may be necessary for routing/session injection.
            preset_key: None,
            channel: None,
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

async fn route(gw: &Gateway, name: &str, provider: &str, target: &str) -> anyhow::Result<()> {
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
        .await?;
    Ok(())
}

struct Dispatched {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn dispatch(
    gw: &Gateway,
    name: &str,
    ingress: ProtocolId,
    prompt: &str,
    stream: bool,
) -> Dispatched {
    let mut body = match ingress {
        ANTHROPIC_MESSAGES_2023_06_01 => {
            json!({"model":name,"messages":[{"role":"user","content":prompt}],"max_tokens":64})
        }
        OPENAI_RESPONSES_V1 => json!({"model":name,"input":prompt,"max_output_tokens":64}),
        _ => json!({"model":name,"messages":[{"role":"user","content":prompt}]}),
    };
    body["stream"] = json!(stream);
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
    let response = dispatch_pipeline(
        gw.clone(),
        HeaderMap::new(),
        RawEnvelope::new(Some(body), HashMap::new(), "POST", path),
        request,
        ingress,
        RequestContext::new(ingress, Duration::from_secs(5)),
    )
    .await;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .expect("response body timed out")
    .expect("response body failed");
    Dispatched {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

fn assert_success(response: &Dispatched, ingress: ProtocolId, stream: bool) {
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    if stream {
        assert!(
            header(&response.headers, "content-type")
                .is_some_and(|v| v.starts_with("text/event-stream")),
            "not SSE: {:?}",
            response.headers
        );
        let events: Vec<Value> = response
            .body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect();
        let text: String = events
            .iter()
            .filter_map(|event| match ingress {
                ANTHROPIC_MESSAGES_2023_06_01 => event["delta"]["text"].as_str(),
                OPENAI_RESPONSES_V1 if event["type"] == "response.output_text.delta" => {
                    event["delta"].as_str()
                }
                OPENAI_RESPONSES_V1 => None,
                _ => event["choices"][0]["delta"]["content"].as_str(),
            })
            .collect();
        assert_eq!(text, "hello", "{}", response.body);
        let terminal = match ingress {
            ANTHROPIC_MESSAGES_2023_06_01 => "message_stop",
            OPENAI_RESPONSES_V1 => "response.completed",
            _ => "[DONE]",
        };
        assert!(
            response.body.contains(terminal),
            "missing terminal {terminal}: {}",
            response.body
        );
    } else {
        let body: Value = serde_json::from_str(&response.body).expect("non-JSON response");
        let text = match ingress {
            ANTHROPIC_MESSAGES_2023_06_01 => &body["content"][0]["text"],
            OPENAI_RESPONSES_V1 => &body["output"][0]["content"][0]["text"],
            _ => &body["choices"][0]["message"]["content"],
        };
        assert_eq!(text, "hello", "wrong ingress response shape: {body}");
    }
}

fn assert_unavailable(response: &Dispatched) {
    assert_eq!(
        response.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        response.body
    );
    let body: Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"]["type"], "NYRO_SERVICE_UNAVAILABLE", "{body}");
    assert_eq!(
        body["error"]["message"], "provider is unavailable",
        "{body}"
    );
}

#[tokio::test]
async fn all_three_ingresses_by_three_egresses_non_streaming_and_streaming() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    for (index, (model, endpoint)) in REPRESENTATIVES.iter().enumerate() {
        let alias = format!("matrix-{index}");
        route(&gw, &alias, &provider, model).await?;
        for ingress in INGRESS {
            for stream in [false, true] {
                upstream.clear();
                let response = dispatch(&gw, &alias, ingress, "hello", stream).await;
                assert_success(&response, ingress, stream);
                let seen = upstream.calls();
                assert_eq!(
                    seen.len(),
                    1,
                    "{model} from {ingress}, stream={stream}: {seen:?}"
                );
                assert_dispatch_wire(&seen[0], model, endpoint, ingress, stream);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn every_catalog_and_historical_model_is_pinned_for_every_ingress() -> anyhow::Result<()> {
    assert_eq!(
        MODEL_ENDPOINTS.len(),
        35,
        "31 current plus four historical models"
    );
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    for (index, (model, endpoint)) in MODEL_ENDPOINTS.iter().enumerate() {
        let alias = format!("catalog-{index}");
        route(&gw, &alias, &provider, model).await?;
        for ingress in INGRESS {
            upstream.clear();
            assert_success(
                &dispatch(&gw, &alias, ingress, "hello", false).await,
                ingress,
                false,
            );
            let seen = upstream.calls();
            assert_eq!(seen.len(), 1, "{model} from {ingress}: {seen:?}");
            assert_dispatch_wire(&seen[0], model, endpoint, ingress, false);
        }
    }
    Ok(())
}

#[tokio::test]
async fn unknown_and_prefix_lookalike_models_are_rejected_before_network_io() -> anyhow::Result<()>
{
    let (_dir, gw, upstream, base_url) = setup().await?;
    for mode in ["adaptive", "fixed"] {
        let provider = provider(&gw, &base_url, mode, "chat").await?;
        for (index, model) in [
            "ghost-model",
            "grok-4.7-preview",
            "claude-haiku-5-5-extra",
            "space-bunny-next",
        ]
        .iter()
        .enumerate()
        {
            let alias = format!("unknown-{mode}-{index}");
            route(&gw, &alias, &provider, model).await?;
            upstream.clear();
            for ingress in INGRESS {
                assert_unavailable(&dispatch(&gw, &alias, ingress, "hello", false).await);
            }
            let outcome = gw
                .admin()
                .probe_provider_models(&provider, Some(vec![model.to_string()]))
                .await?;
            assert_eq!(outcome.results.len(), 1);
            let result = &outcome.results[0];
            assert_eq!(result.model, *model);
            assert!(
                !result.success,
                "unknown model must fail locally: {result:?}"
            );
            assert!(result.error.as_deref().is_some_and(|e| !e.is_empty()));
            assert!(
                upstream.calls().is_empty(),
                "unknown model must not be guessed onto chat"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn missing_or_disabled_required_endpoint_rejects_dispatch_and_probe() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    for (index, (model, required)) in REPRESENTATIVES.iter().enumerate() {
        let alias = format!("unavailable-{index}");
        route(&gw, &alias, &provider, model).await?;
        for disabled in [false, true] {
            let mut configured = endpoints(&base_url);
            if disabled {
                configured
                    .iter_mut()
                    .find(|e| e.protocol == protocol(required).to_string())
                    .unwrap()
                    .is_enabled = false;
            } else {
                configured.retain(|e| e.protocol != protocol(required).to_string());
            }
            let default = configured
                .iter()
                .find(|e| e.is_enabled)
                .unwrap()
                .protocol
                .clone();
            gw.admin()
                .update_provider(
                    &provider,
                    UpdateProvider {
                        protocol: Some(default),
                        protocol_endpoints: Some(configured),
                        ..Default::default()
                    },
                )
                .await?;
            upstream.clear();
            for ingress in INGRESS {
                assert_unavailable(&dispatch(&gw, &alias, ingress, "hello", false).await);
            }
            let outcome = gw
                .admin()
                .probe_provider_models(&provider, Some(vec![model.to_string()]))
                .await?;
            assert_eq!(
                outcome.results.len(),
                1,
                "one failed result, not a batch error"
            );
            let result = &outcome.results[0];
            assert_eq!(result.model, *model);
            assert!(
                !result.success,
                "required={required}, disabled={disabled}: {result:?}"
            );
            assert!(result.error.as_deref().is_some_and(|e| !e.is_empty()));
            assert!(
                upstream.calls().is_empty(),
                "must not fall back to any enabled endpoint: {:?}",
                upstream.calls()
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn no_enabled_endpoints_returns_individual_probe_failures_without_fallback()
-> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    for (index, (model, _)) in REPRESENTATIVES.iter().enumerate() {
        route(&gw, &format!("no-endpoints-{index}"), &provider, model).await?;
    }
    for disabled in [false, true] {
        // Imported/legacy storage may contain configurations the admin form
        // prevents creating. Exercise the real reader without weakening it.
        let configured = if disabled {
            endpoints(&base_url)
                .into_iter()
                .map(|mut endpoint| {
                    endpoint.is_enabled = false;
                    endpoint
                })
                .collect()
        } else {
            Vec::new()
        };
        gw.storage
            .providers()
            .update(
                &provider,
                UpdateProvider {
                    protocol_endpoints: Some(configured),
                    ..Default::default()
                },
            )
            .await?;
        gw.model_cache
            .write()
            .await
            .reload(gw.storage.snapshots())
            .await?;
        upstream.clear();
        for (index, _) in REPRESENTATIVES.iter().enumerate() {
            for ingress in INGRESS {
                assert_unavailable(
                    &dispatch(
                        &gw,
                        &format!("no-endpoints-{index}"),
                        ingress,
                        "hello",
                        false,
                    )
                    .await,
                );
            }
        }
        let outcome = gw
            .admin()
            .probe_provider_models(
                &provider,
                Some(
                    REPRESENTATIVES
                        .iter()
                        .map(|(model, _)| model.to_string())
                        .collect(),
                ),
            )
            .await?;
        assert_eq!(outcome.results.len(), REPRESENTATIVES.len());
        for (model, _) in REPRESENTATIVES {
            let result = outcome
                .results
                .iter()
                .find(|result| result.model == model)
                .unwrap();
            assert!(!result.success, "{result:?}");
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|error| !error.is_empty())
            );
        }
        assert!(
            upstream.calls().is_empty(),
            "empty/disabled must not synthesize a default endpoint"
        );
    }
    Ok(())
}

#[tokio::test]
async fn fixed_providers_accept_only_the_models_matching_their_protocol() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    for configured in ["chat", "responses", "messages"] {
        let provider = provider(&gw, &base_url, "fixed", configured).await?;
        for (index, (model, required)) in REPRESENTATIVES.iter().enumerate() {
            let alias = format!("fixed-{configured}-{index}");
            route(&gw, &alias, &provider, model).await?;
            for ingress in INGRESS {
                upstream.clear();
                let response = dispatch(&gw, &alias, ingress, "hello", false).await;
                if configured == *required {
                    assert_success(&response, ingress, false);
                    let seen = upstream.calls();
                    assert_eq!(seen.len(), 1);
                    assert_dispatch_wire(&seen[0], model, required, ingress, false);
                } else {
                    assert_unavailable(&response);
                    assert!(upstream.calls().is_empty(), "fixed must not bypass the pin");
                }
            }
            upstream.clear();
            let outcome = gw
                .admin()
                .probe_provider_models(&provider, Some(vec![model.to_string()]))
                .await?;
            assert_eq!(outcome.results.len(), 1);
            let result = &outcome.results[0];
            assert_eq!(result.success, configured == *required, "{result:?}");
            if result.success {
                assert_eq!(result.protocol, protocol(required).to_string());
                assert_eq!(result.reply.as_deref(), Some("hello"));
                let seen = upstream.calls();
                assert_eq!(seen.len(), 1);
                assert_wire(&seen[0], model, required, false);
            } else {
                assert!(result.error.as_deref().is_some_and(|e| !e.is_empty()));
                assert!(upstream.calls().is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn conversation_identity_is_stable_across_turns_and_distinct_for_other_prompts()
-> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    route(&gw, "session", &provider, "grok-4.7").await?;
    upstream.clear();
    for prompt in ["hello", "hello", "a different conversation"] {
        assert_success(
            &dispatch(&gw, "session", ANTHROPIC_MESSAGES_2023_06_01, prompt, false).await,
            ANTHROPIC_MESSAGES_2023_06_01,
            false,
        );
    }
    let seen = upstream.calls();
    assert_eq!(seen.len(), 3);
    for call in &seen {
        assert_dispatch_wire(
            call,
            "grok-4.7",
            "responses",
            ANTHROPIC_MESSAGES_2023_06_01,
            false,
        );
        assert!(call.session.as_deref().unwrap().starts_with("nyro-"));
    }
    assert_eq!(seen[0].session, seen[1].session);
    assert_ne!(seen[0].session, seen[2].session);
    Ok(())
}

#[tokio::test]
async fn vendor_only_custom_base_url_probe_captures_all_model_endpoints_auth_and_sessions()
-> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    let stored = gw.admin().get_provider(&provider).await?;
    assert_eq!(stored.vendor.as_deref(), Some("opencode-go"));
    assert!(stored.preset_key.is_none());
    assert!(stored.base_url.starts_with("http://127.0.0.1:"));
    upstream.clear();
    let outcome = gw
        .admin()
        .probe_provider_models(
            &provider,
            Some(
                MODEL_ENDPOINTS
                    .iter()
                    .map(|(model, _)| model.to_string())
                    .collect(),
            ),
        )
        .await?;
    assert_eq!(outcome.results.len(), MODEL_ENDPOINTS.len());
    let seen = upstream.calls();
    assert_eq!(seen.len(), MODEL_ENDPOINTS.len());
    for (model, endpoint) in MODEL_ENDPOINTS {
        let result = outcome
            .results
            .iter()
            .find(|result| result.model == *model)
            .unwrap();
        assert!(result.success, "{result:?}");
        assert_eq!(result.protocol, protocol(endpoint).to_string());
        assert_eq!(result.reply.as_deref(), Some("hello"));
        let calls: Vec<_> = seen.iter().filter(|call| call.model == *model).collect();
        assert_eq!(
            calls.len(),
            1,
            "one probe per model, no alternate endpoint trials"
        );
        assert_wire(calls[0], model, endpoint, false);
    }
    Ok(())
}

#[tokio::test]
async fn probe_subset_deduplicates_and_reports_unknown_model_failure_without_wire()
-> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    upstream.clear();
    let outcome = gw
        .admin()
        .probe_provider_models(
            &provider,
            Some(vec![
                "glm-5.3".into(),
                "glm-5.3".into(),
                "ghost-model".into(),
            ]),
        )
        .await?;
    assert_eq!(outcome.results.len(), 2);
    let known = outcome
        .results
        .iter()
        .find(|r| r.model == "glm-5.3")
        .unwrap();
    let unknown = outcome
        .results
        .iter()
        .find(|r| r.model == "ghost-model")
        .unwrap();
    assert!(known.success, "{known:?}");
    assert!(!unknown.success, "{unknown:?}");
    assert!(unknown.error.is_some());
    let seen = upstream.calls();
    assert_eq!(seen.len(), 1);
    assert_wire(&seen[0], "glm-5.3", "chat", false);
    assert!(
        gw.admin()
            .probe_provider_models(&provider, Some(Vec::new()))
            .await
            .is_err()
    );
    assert_eq!(
        upstream.calls().len(),
        1,
        "empty selection must not probe everything"
    );
    Ok(())
}

#[tokio::test]
async fn unavailable_models_are_hidden_from_model_list_and_catalog_probe() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    assert_eq!(
        gw.admin().get_provider_models(&provider).await?,
        vec!["glm-5.3", "grok-4.6", "minimax-m2.7"]
    );
    upstream.clear();
    let outcome = gw.admin().probe_provider_models(&provider, None).await?;
    assert_eq!(outcome.results.len(), 3);
    let seen = upstream.calls();
    assert_eq!(seen.len(), 3);
    for result in outcome.results {
        assert!(result.success, "{result:?}");
        let endpoint = expected_endpoint(&result.model).unwrap();
        assert_eq!(result.protocol, protocol(endpoint).to_string());
        assert_wire(
            seen.iter().find(|s| s.model == result.model).unwrap(),
            &result.model,
            endpoint,
            false,
        );
    }
    Ok(())
}

#[tokio::test]
async fn opaque_wrong_endpoint_500_does_not_trigger_protocol_fallback() -> anyhow::Result<()> {
    let (_dir, gw, upstream, base_url) = setup().await?;
    let provider = provider(&gw, &base_url, "adaptive", "chat").await?;
    for (index, (model, endpoint)) in REPRESENTATIVES.iter().enumerate() {
        let alias = format!("opaque-error-{index}");
        route(&gw, &alias, &provider, model).await?;
        *upstream.reject_model.lock().unwrap() = Some(model.to_string());
        for ingress in INGRESS {
            upstream.clear();
            let response = dispatch(&gw, &alias, ingress, "hello", false).await;
            assert_eq!(
                response.status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{}",
                response.body
            );
            let calls = upstream.calls();
            assert!(!calls.is_empty());
            // Same-endpoint transport retry is permitted; protocol guessing is not.
            for call in &calls {
                assert_dispatch_wire(call, model, endpoint, ingress, false);
            }
        }
        upstream.clear();
        let outcome = gw
            .admin()
            .probe_provider_models(&provider, Some(vec![model.to_string()]))
            .await?;
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.results[0].success);
        assert_eq!(outcome.results[0].protocol, protocol(endpoint).to_string());
        let calls = upstream.calls();
        assert!(!calls.is_empty());
        for call in &calls {
            assert_wire(call, model, endpoint, false);
        }
    }
    Ok(())
}

#[tokio::test]
async fn mock_itself_rejects_wrong_endpoint_authentication_and_missing_session()
-> anyhow::Result<()> {
    let (_dir, _gw, upstream, base_url) = setup().await?;
    let client = reqwest::Client::new();
    for (model, endpoint) in REPRESENTATIVES {
        for attempted in ["chat", "responses", "messages"] {
            let path = if attempted == "chat" {
                "chat/completions"
            } else {
                attempted
            };
            let body = match attempted {
                "responses" => json!({"model":model,"input":"hello"}),
                _ => {
                    json!({"model":model,"messages":[{"role":"user","content":"hello"}],"max_tokens":64})
                }
            };
            let auth = |request: reqwest::RequestBuilder| {
                if attempted == "messages" {
                    request.header("x-api-key", "sk-test")
                } else {
                    request.bearer_auth("sk-test")
                }
            };
            let url = format!("{base_url}/{path}");
            let response = auth(client.post(&url))
                .header("x-opencode-session", "test-session")
                .json(&body)
                .send()
                .await?;
            assert_eq!(
                response.status(),
                if attempted == endpoint {
                    StatusCode::OK
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            );
            if attempted == endpoint {
                let no_session = auth(client.post(&url)).json(&body).send().await?;
                assert_eq!(no_session.status(), StatusCode::BAD_REQUEST);
                let wrong_auth = client
                    .post(&url)
                    .header("x-opencode-session", "test-session")
                    .json(&body)
                    .send()
                    .await?;
                assert_eq!(wrong_auth.status(), StatusCode::UNAUTHORIZED);
            }
        }
    }
    assert!(!upstream.calls().is_empty());
    Ok(())
}
