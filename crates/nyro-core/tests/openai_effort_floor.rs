//! OpenAI effort-floor regression through the real dispatcher and loopback HTTP.
//!
//! Gateway discovery/usage/OAuth tasks are queued on an undriven, single-thread
//! bootstrap runtime, then cancelled before the actual test runtime is driven.
//! All provider URLs are literal loopback sockets, credentials are synthetic API
//! keys, and the mock never redirects. In particular, `codex` below is an API-key
//! channel fixture, NOT the real OAuth driver (which can pin a production URL).
//! The public admin client disables proxies/redirects; the private inference
//! client is not replaced. No external host, discovery or OAuth flow is used.

use std::{
    collections::HashMap,
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
    logging::LogEntry,
    protocol::{
        ids::{
            ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1, ProtocolId,
        },
        ir::RawEnvelope,
    },
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::MemoryStorage,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const CLIENT_MODEL: &str = "effort-floor-route";
const SOL: &str = "gpt-6.1-sol";
const TOKEN: &str = "synthetic-loopback-api-key";
const TEXT: &str = "Synthetic effort-floor prompt.";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
enum Route {
    Responses,
    Chat,
    ChatToResponses,
    ResponsesToChat,
    AnthropicToResponses,
}
impl Route {
    fn ingress(self) -> ProtocolId {
        match self {
            Self::Responses | Self::ResponsesToChat => OPENAI_RESPONSES_V1,
            Self::Chat | Self::ChatToResponses => OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Self::AnthropicToResponses => ANTHROPIC_MESSAGES_2023_06_01,
        }
    }
    fn egress(self) -> ProtocolId {
        match self {
            Self::Chat | Self::ResponsesToChat => OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            _ => OPENAI_RESPONSES_V1,
        }
    }
    fn path(self) -> &'static str {
        match self.ingress() {
            OPENAI_RESPONSES_V1 => "/v1/responses",
            ANTHROPIC_MESSAGES_2023_06_01 => "/v1/messages",
            _ => "/v1/chat/completions",
        }
    }
    fn strategy(self, channel: &str) -> (&'static str, &'static str) {
        match self {
            Self::Responses | Self::Chat => ("pass_through", "native-no-mutations"),
            Self::ChatToResponses => ("native_ir", "cross-protocol-ir"),
            Self::ResponsesToChat => ("raw_wire_compat", "responses-to-chat"),
            Self::AnthropicToResponses if channel == "default" => {
                ("raw_wire_compat", "anthropic-to-responses-standard")
            }
            Self::AnthropicToResponses => ("raw_wire_compat", "anthropic-to-responses-codex-oauth"),
        }
    }
}
const ROUTES: [Route; 5] = [
    Route::Responses,
    Route::Chat,
    Route::ChatToResponses,
    Route::ResponsesToChat,
    Route::AnthropicToResponses,
];

#[derive(Debug, Clone)]
struct Seen {
    uri: Uri,
    headers: HeaderMap,
    body: Value,
}
type Captured = Arc<Mutex<Vec<Seen>>>;

fn response_json(responses: bool, model: &str) -> Value {
    if responses {
        json!({"id":"resp_local","object":"response","created_at":1,"model":model,"status":"completed",
            "output":[{"id":"msg_local","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"fixture reply","annotations":[]}]}],
            "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}})
    } else {
        json!({"id":"chat_local","object":"chat.completion","created":1,"model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":"fixture reply"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
    }
}
fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn response_sse(responses: bool, model: &str) -> String {
    if responses {
        let completed = response_json(true, model);
        let item = completed["output"][0].clone();
        [
            event(json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_local","object":"response","model":model,"status":"in_progress","output":[]}})),
            event(json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"msg_local","type":"message","role":"assistant","status":"in_progress","content":[]}})),
            event(json!({"type":"response.content_part.added","sequence_number":2,"output_index":0,"content_index":0,"item_id":"msg_local","part":{"type":"output_text","text":"","annotations":[]}})),
            event(json!({"type":"response.output_text.delta","sequence_number":3,"output_index":0,"content_index":0,"item_id":"msg_local","delta":"fixture reply"})),
            event(json!({"type":"response.output_text.done","sequence_number":4,"output_index":0,"content_index":0,"item_id":"msg_local","text":"fixture reply"})),
            event(json!({"type":"response.content_part.done","sequence_number":5,"output_index":0,"content_index":0,"item_id":"msg_local","part":item["content"][0]})),
            event(json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":item})),
            event(json!({"type":"response.completed","sequence_number":7,"response":completed})),
        ].concat()
    } else {
        let delta = json!({"id":"chat_local","object":"chat.completion.chunk","created":1,"model":model,
            "choices":[{"index":0,"delta":{"role":"assistant","content":"fixture reply"},"finish_reason":null}]});
        let terminal = json!({"id":"chat_local","object":"chat.completion.chunk","created":1,"model":model,
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}});
        format!("data: {delta}\n\ndata: {terminal}\n\ndata: [DONE]\n\n")
    }
}
async fn upstream(
    State(seen): State<Captured>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let responses = uri.path().ends_with("/responses");
    let model = body["model"].as_str().unwrap().to_string();
    let stream = body["stream"] == true;
    seen.lock().unwrap().push(Seen { uri, headers, body });
    if stream {
        (
            [("content-type", "text/event-stream")],
            response_sse(responses, &model),
        )
            .into_response()
    } else {
        Json(response_json(responses, &model)).into_response()
    }
}
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

struct Case {
    route: Route,
    vendor: Option<&'static str>,
    preset: Option<&'static str>,
    channel: &'static str,
    model: &'static str,
    force_max: bool,
}
impl Case {
    fn openai(route: Route) -> Self {
        Self {
            route,
            vendor: Some("openai"),
            preset: None,
            channel: "default",
            model: SOL,
            force_max: false,
        }
    }
}

/// A single poll is intentional: Pending must fail closed, never block_on the
/// constructor and accidentally drive models.dev, account or OAuth discovery.
fn offline_gateway(
    config: GatewayConfig,
    provider: Provider,
    model: Model,
) -> anyhow::Result<(Gateway, mpsc::Receiver<LogEntry>)> {
    let storage = Arc::new(MemoryStorage::new(vec![provider], vec![model], vec![]));
    let bootstrap = runtime()?;
    let result = {
        let _entered = bootstrap.enter();
        let mut constructor = Box::pin(Gateway::from_storage(config, storage));
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        match constructor.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => anyhow::bail!(
                "offline Gateway construction yielded; refusing background network tasks"
            ),
        }
    };
    drop(bootstrap);
    let (mut gateway, logs) = result?;
    gateway.http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()?;
    Ok((gateway, logs))
}

fn run(case: Case, body: Value, expected: Value) -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let provider = Provider {
        keys: vec![],
        id: "local-provider".into(),
        name: "local-effort-provider".into(),
        vendor: case.vendor.map(str::to_string),
        protocol: case.route.egress().to_string(),
        base_url,
        protocol_mode: "fixed".into(),
        protocol_endpoints: vec![],
        preset_key: case.preset.map(str::to_string),
        channel: Some(case.channel.into()),
        models_source: None,
        static_models: None,
        api_key: TOKEN.into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
        fast_mode: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    };
    assert_eq!(
        provider.effective_auth_mode(),
        "apikey",
        "must not invoke the real Codex OAuth driver"
    );
    let model = Model {
        id: "local-model".into(),
        name: CLIENT_MODEL.into(),
        balance: "priority".into(),
        target_provider: provider.id.clone(),
        target_model: case.model.into(),
        enable_auth: false,
        enable_payload: Some(false),
        force_max_reasoning: case.force_max,
        vision_shim: None,
        is_enabled: true,
        created_at: String::new(),
        targets: vec![],
    };
    let dir = tempfile::tempdir()?;
    let (gateway, mut logs) = offline_gateway(
        GatewayConfig {
            data_dir: dir.path().into(),
            config_poll_interval: Duration::ZERO,
            ..Default::default()
        },
        provider,
        model,
    )?;
    let seen: Captured = Arc::new(Mutex::new(vec![]));
    runtime()?.block_on(async {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let app = Router::new()
            .route("/*path", post(upstream))
            .with_state(seen.clone());
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let ingress = case.route.ingress();
        let request = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        let context = RequestContext::new(ingress, TIMEOUT);
        let trace = context.trace.clone();
        let request_id = context.request_id.clone();
        let response = tokio::time::timeout(
            TIMEOUT,
            dispatch_pipeline(
                gateway,
                HeaderMap::new(),
                RawEnvelope::new(
                    Some(body.clone()),
                    HashMap::new(),
                    "POST",
                    case.route.path(),
                ),
                request,
                ingress,
                context,
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
        assert_eq!(
            status,
            StatusCode::OK,
            "{:?}/{}: {wire}",
            case.route,
            case.channel
        );
        assert!(
            wire.contains("fixture reply"),
            "missing usable reply: {wire}"
        );
        let captured = seen.lock().unwrap().clone();
        assert_eq!(
            captured.len(),
            1,
            "must make one loopback attempt: {captured:?}"
        );
        let sent = &captured[0];
        let expected_path = if case.route.egress() == OPENAI_RESPONSES_V1 {
            if case.channel == "codex" {
                "/responses"
            } else {
                "/v1/responses"
            }
        } else {
            "/v1/chat/completions"
        };
        assert_eq!(sent.uri.path(), expected_path);
        assert_eq!(sent.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(
            sent.body, expected,
            "exact final wire body: {:?}/{}",
            case.route, case.channel
        );
        let (strategy, rule) = case.route.strategy(case.channel);
        assert!(
            trace.snapshot().iter().any(|e| e.tag == "conversion"
                && e.detail
                    .contains(&format!("strategy={strategy}; rule={rule};"))),
            "wrong dispatcher path: {:?}",
            trace.snapshot()
        );
        let row = tokio::time::timeout(TIMEOUT, logs.recv())
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing real dispatcher log"))?;
        assert_eq!(
            row.diagnostic.client_request_id.as_deref(),
            Some(request_id.as_str())
        );
        assert_eq!(row.upstream_model, case.model);
        assert_eq!(row.client_status_code, 200);
        assert_eq!(row.upstream_status_code, Some(200));
        // Payload capture is disabled: this scalar is the value persisted as
        // request_logs.upstream_effort_raw, taken from the actual outgoing body.
        let effort = expected
            .pointer("/reasoning/effort")
            .or_else(|| expected.get("reasoning_effort"))
            .and_then(Value::as_str);
        assert_eq!(row.performance.effort_raw.as_deref(), effort);
        assert!(logs.try_recv().is_err(), "unexpected retry/log");
        Ok(())
    })
}

fn request(route: Route, stream: bool, effort: Option<&str>) -> Value {
    let mut body = match route.ingress() {
        OPENAI_RESPONSES_V1 => {
            json!({"model":CLIENT_MODEL,"input":TEXT,"store":false,"stream":stream,"max_output_tokens":64})
        }
        ANTHROPIC_MESSAGES_2023_06_01 => {
            json!({"model":CLIENT_MODEL,"system":"Local instructions.","messages":[{"role":"user","content":TEXT}],"stream":stream,"max_tokens":64})
        }
        _ => {
            json!({"model":CLIENT_MODEL,"messages":[{"role":"user","content":TEXT}],"stream":stream,"max_tokens":64})
        }
    };
    if let Some(effort) = effort {
        match route.ingress() {
            OPENAI_RESPONSES_V1 => body["reasoning"] = json!({"effort":effort,"summary":"auto"}),
            ANTHROPIC_MESSAGES_2023_06_01 => body["output_config"] = json!({"effort":effort}),
            _ => body["reasoning_effort"] = json!(effort),
        }
    }
    body
}
fn expected(case: &Case, original: &Value, effort: Option<&str>) -> Value {
    let stream = original["stream"] == true;
    let relay = case.vendor == Some("nyro-gateway");
    let mut body = match case.route {
        Route::Responses | Route::Chat => {
            let mut body = original.clone();
            if matches!(case.route, Route::Responses) && !relay {
                body["stream"] = json!(true);
            }
            if matches!(case.route, Route::Chat) && stream && !relay {
                body["stream_options"] = json!({"include_usage":true});
            }
            body
        }
        Route::ChatToResponses => {
            json!({"instructions":"You are a helpful assistant.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":TEXT}]}],"store":false,"stream":true})
        }
        Route::ResponsesToChat => {
            // Existing native vendor token-budget policy also rides the compat
            // patch when non-off reasoning is enabled; this is not the floor.
            let reasoning_enabled = case.force_max
                || original
                    .pointer("/reasoning/effort")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value != "none");
            let max_tokens = if reasoning_enabled { 128000 } else { 64 };
            let mut body = json!({"messages":[{"role":"user","content":TEXT}],"stream":stream,"max_tokens":max_tokens});
            if stream {
                body["stream_options"] = json!({"include_usage":true});
            }
            body
        }
        Route::AnthropicToResponses => {
            let mut body = json!({"instructions":"Local instructions.","input":[{"role":"user","content":[{"type":"input_text","text":TEXT}]}],"stream":stream,"max_output_tokens":64});
            if case.channel != "default" {
                body.as_object_mut().unwrap().remove("max_output_tokens");
                body["store"] = json!(false);
                body["stream"] = json!(true);
                body["include"] = json!(["reasoning.encrypted_content"]);
                body["tools"] = json!([]);
                body["parallel_tool_calls"] = json!(false);
            }
            body
        }
    };
    body["model"] = json!(case.model);
    if case.route.egress() == OPENAI_RESPONSES_V1 {
        if let Some(effort) = effort {
            if !body.get("reasoning").is_some_and(Value::is_object) {
                body["reasoning"] = json!({});
            }
            body["reasoning"]["effort"] = json!(effort);
        }
        if case.channel != "default" {
            body.as_object_mut().unwrap().remove("max_output_tokens");
        }
    } else if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}
fn check(
    case: Case,
    stream: bool,
    input: Option<&str>,
    output: Option<&str>,
) -> anyhow::Result<()> {
    let body = request(case.route, stream, input);
    let expected = expected(&case, &body, output);
    run(case, body, expected)
}

#[test]
fn none_is_low_on_all_five_real_dispatch_paths_and_both_stream_modes() -> anyhow::Result<()> {
    for route in ROUTES {
        for stream in [false, true] {
            for model in [
                SOL,
                "arbitrary-route-model",
                "gpt-4o",
                "grok-labelled-model",
            ] {
                let mut case = Case::openai(route);
                case.model = model;
                check(case, stream, Some("none"), Some("low"))?;
            }
        }
    }
    Ok(())
}
#[test]
fn same_protocol_off_aliases_and_non_off_values_keep_the_wire_contract() -> anyhow::Result<()> {
    for route in [Route::Responses, Route::Chat] {
        for stream in [false, true] {
            for alias in [
                "none",
                "off",
                "disable",
                "disabled",
                "  NoNe  ",
                " OFF ",
                "DiSaBlE",
                " DISABLED ",
            ] {
                check(Case::openai(route), stream, Some(alias), Some("low"))?;
            }
            for effort in [
                "low",
                "minimal",
                "medium",
                "high",
                "xhigh",
                "max",
                "future-effort",
            ] {
                check(Case::openai(route), stream, Some(effort), Some(effort))?;
            }
        }
    }
    Ok(())
}
#[test]
fn cross_protocol_legal_tiers_and_missing_effort_are_not_floor_overrides() -> anyhow::Result<()> {
    for route in ROUTES {
        for stream in [false, true] {
            check(Case::openai(route), stream, None, None)?;
            for effort in ["minimal", "low", "medium", "high", "xhigh", "max"] {
                check(Case::openai(route), stream, Some(effort), Some(effort))?;
            }
        }
    }
    Ok(())
}
#[test]
fn codex_and_sub2api_channels_use_loopback_api_keys_not_oauth() -> anyhow::Result<()> {
    for channel in ["codex", "sub2api"] {
        for route in [Route::Responses, Route::AnthropicToResponses] {
            for stream in [false, true] {
                let mut case = Case::openai(route);
                case.channel = channel;
                check(case, stream, Some("none"), Some("low"))?;
            }
        }
    }
    Ok(())
}
#[test]
fn floor_uses_actual_vendor_not_model_prefix_or_an_explicitly_stale_preset() -> anyhow::Result<()> {
    for route in [Route::Responses, Route::Chat] {
        for stream in [false, true] {
            for model in [
                "arbitrary-route-model",
                "gpt-4o",
                "gpt-6-astra",
                "grok-labelled-model",
            ] {
                let mut case = Case::openai(route);
                case.model = model;
                check(case, stream, Some("none"), Some("low"))?;
            }
            for vendor in [Some("custom"), Some("nyro-gateway"), None] {
                let mut case = Case::openai(route);
                case.vendor = vendor;
                check(case, stream, Some("none"), Some("none"))?;
            }
            let mut fallback = Case::openai(route);
            fallback.vendor = None;
            fallback.preset = Some("openai");
            check(fallback, stream, Some("none"), Some("low"))?;
            let mut explicit_custom = Case::openai(route);
            explicit_custom.vendor = Some("custom");
            explicit_custom.preset = Some("openai");
            check(explicit_custom, stream, Some("none"), Some("none"))?;
            let mut astra = Case::openai(route);
            astra.vendor = Some("custom");
            astra.model = "gpt-6-astra";
            check(astra, stream, Some("none"), Some("low"))?;
        }
    }
    Ok(())
}
#[test]
fn force_max_reasoning_remains_an_explicit_route_override() -> anyhow::Result<()> {
    for route in ROUTES {
        for stream in [false, true] {
            let mut case = Case::openai(route);
            case.force_max = true;
            check(case, stream, Some("none"), Some("max"))?;
        }
    }
    Ok(())
}
#[test]
fn small_sol_request_and_opaque_responses_replay_preserve_non_effort_fields() -> anyhow::Result<()>
{
    for stream in [false, true] {
        // Incident shape only: no real conversation or production cache key.
        let original = json!({"model":CLIENT_MODEL,"store":false,"stream":stream,"max_output_tokens":64,
            "prompt_cache_key":"synthetic-sol-cache-key","reasoning":{"effort":"none","summary":"auto"},
            "input":[{"role":"user","content":"Synthetic title input."},{"role":"assistant","content":"Synthetic prior answer."}]});
        let case = Case::openai(Route::Responses);
        let outgoing = expected(&case, &original, Some("low"));
        assert_eq!(outgoing["input"], original["input"]);
        assert_eq!(outgoing["reasoning"]["summary"], "auto");
        run(case, original, outgoing)?;
        for channel in ["default", "codex", "sub2api"] {
            let mut original = request(Route::Responses, stream, Some("none"));
            original["input"] = json!([
                {"type":"reasoning","id":format!("rs_{}", "a".repeat(64)),"encrypted_content":"synthetic-not-real-ciphertext","summary":[]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":TEXT}]}
            ]);
            original["tools"] = json!([
                {"type":"function","name":"strict_lookup","strict":true,"parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}},
                {"type":"function","name":"default_lookup","parameters":{"type":"object","properties":{}}}
            ]);
            let mut case = Case::openai(Route::Responses);
            case.channel = channel;
            let mut outgoing = expected(&case, &original, Some("low"));
            outgoing["tools"][1]["strict"] = json!(false);
            assert_eq!(outgoing["input"], original["input"]);
            assert_eq!(outgoing["tools"][0], original["tools"][0]);
            assert_eq!(outgoing["reasoning"]["summary"], "auto");
            run(case, original, outgoing)?;
        }
    }
    Ok(())
}
