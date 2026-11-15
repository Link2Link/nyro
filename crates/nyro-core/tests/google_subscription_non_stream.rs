//! Non-stream Anthropic requests must retain Google subscription upstream SSE.
//! Real dispatcher, synthetic credentials and loopback-only HTTP; no discovery,
//! OAuth refresh, official manifest, or Gateway background task is allowed out.
//! Admin probes serve their catalog request on an explicit loopback-only route.
//! The same fixture also covers admin probe diagnostics for these channels.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::StreamExt;
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{Model, Provider, UpsertOAuthCredential},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::MemoryStorage,
};
use serde_json::{Value, json};

const CLIENT_MODEL: &str = "subscription-non-stream";
// An explicit wire tier keeps streaming regressions independent of dynamic
// catalog evidence; source-less credential snapshots are intentionally ignored.
const UPSTREAM_MODEL: &str = "gemini-3.8-flash-tiered";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct Seen {
    uri: Uri,
    body: Value,
}

type Requests = Arc<Mutex<Vec<Seen>>>;

async fn upstream(State(seen): State<Requests>, uri: Uri, Json(body): Json<Value>) -> Response {
    let streaming = uri.path() == "/v1internal:streamGenerateContent";
    seen.lock().unwrap().push(Seen { uri, body });
    if !streaming {
        // The Code Assist non-stream action can succeed without any output.
        // Returning an empty object makes the old canonicalizer fail rather
        // than accidentally passing by accepting SSE on the wrong action.
        return Json(json!({})).into_response();
    }
    let first = json!({"response": {
        "responseId": "subscription-local-reply",
        "modelVersion": UPSTREAM_MODEL,
        "candidates": [{"index": 0, "content": {
            "role": "model", "parts": [{"text": "Hello "}]
        }}]
    }});
    let last = json!({"response": {
        "candidates": [{"index": 0, "content": {
            "role": "model", "parts": [{"text": "from subscription"}]
        }, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10
        }
    }});
    (
        [("content-type", "text/event-stream")],
        format!("data: {first}\n\ndata: {last}\n\n"),
    )
        .into_response()
}

/// A bare Google NOT_FOUND does not identify the underlying cause.
async fn not_found_upstream(
    State(seen): State<Requests>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Response {
    seen.lock().unwrap().push(Seen { uri, body });
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {
            "code": 404,
            "message": "Requested entity was not found.",
            "status": "NOT_FOUND"
        }})),
    )
        .into_response()
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

fn offline_gateway(channel: &str, base_url: &str) -> anyhow::Result<Gateway> {
    let provider = Provider {
        keys: vec![],
        id: "local-subscription".into(),
        name: "Local Google subscription".into(),
        vendor: Some("google".into()),
        protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string(),
        base_url: base_url.into(),
        protocol_mode: "fixed".into(),
        protocol_endpoints: vec![],
        preset_key: Some("google".into()),
        channel: Some(channel.into()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        auth_mode: "oauth".into(),
        use_proxy: true,
        fast_mode: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    };
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
        targets: vec![],
    };
    offline_gateway_with(provider, vec![model], base_url)
}

fn offline_gateway_with(
    provider: Provider,
    models: Vec<Model>,
    base_url: &str,
) -> anyhow::Result<Gateway> {
    // The fixture is also a non-forwarding HTTP proxy: even an accidental
    // HTTPS endpoint override can only reach this loopback listener (CONNECT
    // is rejected), never an official Google endpoint or a system proxy.
    let storage = Arc::new(MemoryStorage::new(
        vec![provider],
        models,
        vec![
            ("proxy_enabled".into(), "true".into()),
            ("proxy_url".into(), base_url.into()),
        ],
    ));
    let bootstrap = runtime()?;
    let result = {
        let _entered = bootstrap.enter();
        let mut constructor = Box::pin(Gateway::from_storage(
            GatewayConfig {
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            storage,
        ));
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        match constructor.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => anyhow::bail!(
                "offline Gateway construction yielded; refusing to poll background network tasks"
            ),
        }
    };
    // No worker ever polls the queued models.dev/usage/OAuth monitors.
    drop(bootstrap);
    let (mut gateway, _logs) = result?;
    gateway.http_client = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(base_url)?)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()?;
    Ok(gateway)
}

async fn bind_fixture_credential(gateway: &Gateway) -> anyhow::Result<()> {
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            "local-subscription",
            UpsertOAuthCredential {
                // Deliberately bypass Google auth preparation to avoid its
                // manifest refresh. This fixture tests dispatch streaming and
                // diagnostics, not the real driver identity or catalog cache.
                driver_key: "google-fixture".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "fixture-access-token".into(),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                meta: Some(
                    json!({
                        "project_id": "fixture-project",
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
        )
        .await?;
    Ok(())
}

fn non_stream_anthropic_subscription(channel: &str) -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let gateway = offline_gateway(channel, &base_url)?;
    runtime()?.block_on(async move {
        let seen = Requests::default();
        let app = Router::new()
            .route("/*path", post(upstream))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        bind_fixture_credential(&gateway).await?;
        // Matches admin's Anthropic model probe: stream absent, max_tokens 1024.
        let body = json!({
            "model": CLIENT_MODEL,
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let ingress = ANTHROPIC_MESSAGES_2023_06_01;
        let request = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        assert!(!request.stream.enabled);
        let response = tokio::time::timeout(
            TIMEOUT,
            dispatch_pipeline(
                gateway,
                HeaderMap::new(),
                RawEnvelope::new(Some(body), Default::default(), "POST", "/v1/messages"),
                request,
                ingress,
                RequestContext::new(ingress, TIMEOUT),
            ),
        )
        .await?;
        let status = response.status();
        let content_type = response.headers().get("content-type").cloned();
        let bytes = tokio::time::timeout(
            TIMEOUT,
            axum::body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await??;
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1, "exactly one upstream call: {requests:?}");
        let request = &requests[0];
        assert_eq!(
            request.uri.path_and_query().unwrap().as_str(),
            "/v1internal:streamGenerateContent?alt=sse",
            "non-stream {channel} ingress must preserve forced upstream streaming"
        );
        assert_eq!(request.body["model"], UPSTREAM_MODEL);
        assert_eq!(request.body["project"], "fixture-project");
        assert_eq!(
            request.body["request"]["contents"][0]["parts"][0]["text"],
            "hi"
        );
        assert!(request.body.get("stream").is_none());
        assert!(request.body["request"].get("stream").is_none());
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(content_type.unwrap(), "application/json");
        let output: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(output["type"], "message");
        assert_eq!(output["role"], "assistant");
        assert_eq!(output["content"][0]["type"], "text");
        assert_eq!(output["content"][0]["text"], "Hello from subscription");
        assert_eq!(output["stop_reason"], "end_turn");
        assert_eq!(output["usage"]["input_tokens"], 7);
        assert_eq!(output["usage"]["output_tokens"], 3);
        Ok(())
    })
}

#[test]
fn anthropic_non_stream_antigravity_uses_sse_and_aggregates_reply_and_usage() -> anyhow::Result<()>
{
    non_stream_anthropic_subscription("antigravity")
}

#[test]
fn anthropic_non_stream_gemini_cli_uses_sse_and_aggregates_reply_and_usage() -> anyhow::Result<()> {
    non_stream_anthropic_subscription("gemini-cli")
}

fn stream_subscription_read_error_after_finish(channel: &str) -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let mut gateway = offline_gateway(channel, &base_url)?;
    let (log_tx, mut logs) = tokio::sync::mpsc::channel(8);
    gateway.log_tx = log_tx;
    runtime()?.block_on(async move {
        let seen = Requests::default();
        let release_error = Arc::new(tokio::sync::Notify::new());
        let fixture_release = release_error.clone();
        let app = Router::new()
            .route(
                "/*path",
                post(
                    move |State(seen): State<Requests>, uri: Uri, Json(body): Json<Value>| {
                        let release_error = fixture_release.clone();
                        async move {
                            seen.lock().unwrap().push(Seen { uri, body });
                            let first = json!({"response": {
                                "responseId": "subscription-read-error",
                                "modelVersion": UPSTREAM_MODEL,
                                "candidates": [{"index": 0, "content": {
                                    "role": "model", "parts": [{"text": "before read error"}]
                                }, "finishReason": "STOP"}]
                            }});
                            let stream = futures::stream::once(async move {
                                Ok::<_, std::io::Error>(bytes::Bytes::from(format!(
                                    "data: {first}\n\n"
                                )))
                            })
                            .chain(futures::stream::once(async move {
                                // Do not let Hyper coalesce the body error with the
                                // first write. The client must observe the payload
                                // before the upstream transport is deliberately cut.
                                release_error.notified().await;
                                Err::<bytes::Bytes, _>(std::io::Error::other(
                                    "fixture read failure",
                                ))
                            }));
                            (
                                [("content-type", "text/event-stream")],
                                Body::from_stream(stream),
                            )
                                .into_response()
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        bind_fixture_credential(&gateway).await?;
        let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        let body = json!({
            "model": CLIENT_MODEL,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let request = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        let context = RequestContext::new(ingress, TIMEOUT);
        let request_id = context.request_id.clone();
        let response = tokio::time::timeout(
            TIMEOUT,
            dispatch_pipeline(
                gateway,
                HeaderMap::new(),
                RawEnvelope::new(
                    Some(body),
                    Default::default(),
                    "POST",
                    "/v1/chat/completions",
                ),
                request,
                ingress,
                context,
            ),
        )
        .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut client = response.into_body().into_data_stream();
        let mut wire = Vec::new();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let bytes = client
                    .next()
                    .await
                    .expect("client must receive the initial payload before failure")?;
                wire.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&wire).contains("before read error") {
                    break;
                }
            }
            Ok::<_, axum::Error>(())
        })
        .await??;
        release_error.notify_one();
        tokio::time::timeout(TIMEOUT, async {
            while let Some(bytes) = client.next().await {
                wire.extend_from_slice(&bytes?);
            }
            Ok::<_, axum::Error>(())
        })
        .await??;
        let wire = String::from_utf8(wire)?;
        assert!(wire.contains("upstream_read_error"), "{channel}: {wire}");
        assert!(!wire.contains("[DONE]"), "{channel}: {wire}");
        let events: Vec<Value> = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| serde_json::from_str(data.trim()).expect("JSON SSE event"))
            .collect();
        assert!(
            events
                .iter()
                .all(|event| !event["choices"][0]["finish_reason"].is_string()),
            "pending finishReason must not become client success: {events:?}"
        );
        let errors: Vec<_> = events
            .iter()
            .filter(|event| event.get("error").is_some())
            .collect();
        assert_eq!(errors.len(), 1, "{events:?}");
        assert_eq!(errors[0]["error"]["type"], "upstream_read_error");
        assert!(
            !wire.contains("fixture read failure"),
            "raw upstream details must stay private"
        );

        let row = tokio::time::timeout(TIMEOUT, logs.recv())
            .await?
            .expect("dispatcher must log the failed stream");
        assert_eq!(
            row.diagnostic.client_request_id.as_deref(),
            Some(request_id.as_str())
        );
        assert_eq!(row.client_status_code, 200);
        assert_eq!(row.upstream_status_code, Some(200));
        assert_eq!(row.diagnostic.attempt_outcome, "failed", "{row:?}");
        assert_eq!(row.performance.completion, "failed", "{row:?}");
        assert!(row.performance.completed_at.is_none(), "{row:?}");
        assert_eq!(
            row.diagnostic.failure_kind.as_deref(),
            Some("upstream_read_error")
        );
        assert_eq!(
            row.diagnostic.failure_stage.as_deref(),
            Some("upstream_read")
        );
        assert_eq!(
            row.diagnostic
                .final_result
                .as_ref()
                .expect("final outcome")
                .final_outcome,
            "failed"
        );
        assert_eq!(row.usage.total_tokens, 0, "no upstream usage was received");
        let requests = seen.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "read failures must not replay: {requests:?}"
        );
        assert_eq!(
            requests[0].uri.path_and_query().unwrap().as_str(),
            "/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(requests[0].body["model"], UPSTREAM_MODEL);
        assert_eq!(requests[0].body["project"], "fixture-project");
        Ok(())
    })
}

#[test]
fn openai_stream_antigravity_read_error_after_finish_is_not_success() -> anyhow::Result<()> {
    stream_subscription_read_error_after_finish("antigravity")
}

#[test]
fn openai_stream_gemini_cli_read_error_after_finish_is_not_success() -> anyhow::Result<()> {
    stream_subscription_read_error_after_finish("gemini-cli")
}

async fn capture_relay_probe(
    State(seen): State<Requests>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
    seen.lock().unwrap().push(Seen {
        uri: parts.uri.clone(),
        body: serde_json::from_slice(&bytes).unwrap(),
    });
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[test]
fn admin_anthropic_probe_through_remote_nyro_aggregates_google_subscription_sse()
-> anyhow::Result<()> {
    let google_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    google_listener.set_nonblocking(true)?;
    let google_url = format!("http://{}", google_listener.local_addr()?);
    let remote_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    remote_listener.set_nonblocking(true)?;
    let remote_url = format!("http://{}", remote_listener.local_addr()?);
    // Construct both gateways before driving the test runtime. Dropping each
    // bootstrap runtime cancels startup networking on both hops, not just one.
    let remote = offline_gateway("antigravity", &google_url)?;
    let downstream = offline_gateway_with(
        Provider {
            keys: vec![],
            id: "local-remote-nyro".into(),
            name: "Local remote Nyro".into(),
            vendor: Some("nyro-gateway".into()),
            protocol: ANTHROPIC_MESSAGES_2023_06_01.to_string(),
            base_url: remote_url.clone(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: Some("nyro-gateway".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "local-remote-token".into(),
            auth_mode: "apikey".into(),
            use_proxy: true,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        },
        vec![],
        &remote_url,
    )?;
    runtime()?.block_on(async move {
        let google_seen = Requests::default();
        let google_app = Router::new()
            .route("/*path", post(upstream))
            .with_state(google_seen.clone());
        let google_listener = tokio::net::TcpListener::from_std(google_listener)?;
        let _google_server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(google_listener, google_app).await.unwrap();
        }));
        bind_fixture_credential(&remote).await?;
        let relay_seen = Requests::default();
        // Use Nyro's real HTTP ingress shell as well as its dispatcher. The
        // middleware observes the admin probe without replacing its payload.
        let remote_app = nyro_core::proxy::server::create_router(remote).layer(
            middleware::from_fn_with_state(relay_seen.clone(), capture_relay_probe),
        );
        let remote_listener = tokio::net::TcpListener::from_std(remote_listener)?;
        let _remote_server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(remote_listener, remote_app).await.unwrap();
        }));
        let outcome = tokio::time::timeout(
            TIMEOUT,
            downstream
                .admin()
                .probe_provider_models("local-remote-nyro", Some(vec![CLIENT_MODEL.into()])),
        )
        .await??;
        assert_eq!(outcome.meta.base_url, remote_url);
        assert_eq!(
            outcome.meta.protocol,
            ANTHROPIC_MESSAGES_2023_06_01.to_string()
        );
        assert_eq!(outcome.results.len(), 1);
        let result = &outcome.results[0];
        assert!(result.success, "two-hop admin probe failed: {result:?}");
        assert!(result.error.is_none());
        assert_eq!(result.model, CLIENT_MODEL);
        assert_eq!(result.reply.as_deref(), Some("Hello from subscription"));
        let relay_requests = relay_seen.lock().unwrap();
        assert_eq!(relay_requests.len(), 1, "{relay_requests:?}");
        let relay_request = &relay_requests[0];
        assert_eq!(relay_request.uri.path(), "/v1/messages");
        assert_eq!(relay_request.body["model"], CLIENT_MODEL);
        assert_eq!(relay_request.body["messages"][0]["content"], "hi");
        assert_ne!(
            relay_request.body["stream"], true,
            "probe must be non-streaming"
        );
        let google_requests = google_seen.lock().unwrap();
        assert_eq!(google_requests.len(), 1, "{google_requests:?}");
        let google_request = &google_requests[0];
        assert_eq!(
            google_request.uri.path_and_query().unwrap().as_str(),
            "/v1internal:streamGenerateContent?alt=sse",
            "the remote must preserve Google subscription streaming for a non-stream admin probe"
        );
        assert_eq!(google_request.body["model"], UPSTREAM_MODEL);
        assert_eq!(google_request.body["project"], "fixture-project");
        assert!(google_request.body.get("stream").is_none());
        assert!(google_request.body["request"].get("stream").is_none());
        Ok(())
    })
}

/// A bare 404 NOT_FOUND must preserve Google's message and explain possible
/// model/endpoint/project/client/account causes, not assert a plan exclusion.
/// A valid empty local catalog must not prevent probing a manual model.
fn admin_probe_explains_google_subscription_not_found(channel: &str) -> anyhow::Result<()> {
    // Not a unified tier family, so the manual id reaches upstream verbatim
    // (Gemini 4 Carbon is listed only as an Ultra placeholder on AI Pro).
    const REQUESTED_MODEL: &str = "gemini-4-carbon";
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let gateway = offline_gateway(channel, &base_url)?;
    runtime()?.block_on(async move {
        let seen = Requests::default();
        let catalog_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured_catalog_calls = catalog_calls.clone();
        // Axum 0.7 treats ':' in a route segment as a parameter marker;
        // inspect the actual URI instead of registering a pseudo-exact action.
        let app = Router::new()
            .route(
                "/*path",
                post(
                    move |state: State<Requests>,
                          uri: Uri,
                          headers: HeaderMap,
                          Json(body): Json<Value>| {
                        let catalog_calls = captured_catalog_calls.clone();
                        async move {
                            if uri.path() == "/v1internal:fetchAvailableModels" {
                                assert_eq!(headers["authorization"], "Bearer fixture-access-token");
                                assert_eq!(body, json!({"project":"fixture-project"}));
                                catalog_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                Json(json!({"models":{}})).into_response()
                            } else {
                                not_found_upstream(state, uri, Json(body)).await
                            }
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        bind_fixture_credential(&gateway).await?;
        let outcome = tokio::time::timeout(
            TIMEOUT,
            gateway
                .admin()
                .probe_provider_models("local-subscription", Some(vec![REQUESTED_MODEL.into()])),
        )
        .await??;
        assert_eq!(outcome.results.len(), 1);
        let result = &outcome.results[0];
        assert_eq!(result.model, REQUESTED_MODEL);
        assert!(!result.success, "{result:?}");
        let error = result
            .error
            .as_deref()
            .expect("failed probe carries an error");
        assert!(error.starts_with("HTTP 404 Not Found: "), "{error}");
        assert!(error.contains("Requested entity was not found."), "{error}");
        assert!(error.contains("several possible causes"), "{error}");
        assert!(error.contains("endpoint/project mismatch"), "{error}");
        assert!(
            error.contains("alone does not establish subscription entitlement"),
            "{error}"
        );
        assert!(!error.contains("outside this account's plan"), "{error}");
        assert!(!error.contains("promotional"), "{error}");
        assert!(
            error.contains(
                "model=gemini-4-carbon; wire_model=gemini-4-carbon; endpoint_source=provider_config"
            ),
            "{error}"
        );
        assert_eq!(outcome.meta.base_url, base_url);
        assert_eq!(
            catalog_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must use the explicit local catalog endpoint exactly once"
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1, "exactly one upstream call: {requests:?}");
        assert_eq!(
            requests[0].uri.path_and_query().unwrap().as_str(),
            "/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(requests[0].body["model"], REQUESTED_MODEL);
        assert_eq!(requests[0].body["project"], "fixture-project");
        Ok(())
    })
}

#[test]
fn admin_probe_explains_antigravity_not_found() -> anyhow::Result<()> {
    admin_probe_explains_google_subscription_not_found("antigravity")
}

#[test]
fn admin_probe_explains_gemini_cli_not_found() -> anyhow::Result<()> {
    admin_probe_explains_google_subscription_not_found("gemini-cli")
}
