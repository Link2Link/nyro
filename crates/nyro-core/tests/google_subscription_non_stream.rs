//! Non-stream Anthropic requests must retain Google subscription upstream SSE.
//! Real dispatcher, synthetic credentials and loopback-only HTTP; no discovery,
//! OAuth refresh, official manifest, or Gateway background task is allowed out.
//! Admin probes serve their catalog request on an explicit loopback-only route.
//! The same fixture also covers admin probe diagnostics for these channels,
//! Antigravity system-prompt relocation and detail-free 429 scoping.

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
        fast_mode: Default::default(),
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

async fn fixture_chat(gateway: &Gateway, stream: bool) -> anyhow::Result<Response> {
    let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    let body = json!({"model":CLIENT_MODEL,"stream":stream,"messages":[{"role":"user","content":"hello"}]});
    let ir = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    Ok(tokio::time::timeout(
        TIMEOUT,
        dispatch_pipeline(
            gateway.clone(),
            HeaderMap::new(),
            RawEnvelope::new(
                Some(body),
                Default::default(),
                "POST",
                "/v1/chat/completions",
            ),
            ir,
            ingress,
            RequestContext::new(ingress, TIMEOUT),
        ),
    )
    .await?)
}

#[test]
fn antigravity_retry_after_reaches_client_and_scheduler_on_both_request_paths() -> anyhow::Result<()>
{
    for stream in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base = format!("http://{}", listener.local_addr()?);
        let gw = offline_gateway("antigravity", &base)?;
        runtime()?.block_on(async move {
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = hits.clone();
            let app = Router::new().fallback(move |uri: Uri| {
                let count = count.clone();
                async move {
                    if uri.path() != "/v1internal:streamGenerateContent" { return StatusCode::NOT_FOUND.into_response(); }
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut error = json!({"code":429,"status":"RESOURCE_EXHAUSTED"});
                    if n == 0 {
                        // The body's explicit zero wins over the conflicting HTTP wait.
                        error["details"] = json!([{ "@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"0s" }]);
                    }
                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after","900"),("x-secret-unrelated","fixture")],Json(json!({"error":error}))).into_response()
                }
            });
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let _server = AbortOnDrop(tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); }));
            bind_fixture_credential(&gw).await?;
            for wait in ["0", "900"] {
                let response = fixture_chat(&gw, stream).await?;
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                assert_eq!(response.headers()["retry-after"], wait);
                assert!(!response.headers().contains_key("x-secret-unrelated"));
            }
            let blocked = fixture_chat(&gw, stream).await?;
            assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
            let remaining = blocked.headers()["retry-after"].to_str()?.parse::<u64>()?;
            assert!((890..=900).contains(&remaining), "{remaining}");
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert!(gw.quota_registry.is_model_schedulable("local-subscription", "sibling"));
            anyhow::Ok(())
        })?;
    }
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

fn stream_subscription_error_after_finish(channel: &str, in_band_429: bool) -> anyhow::Result<()> {
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
                            if uri.path() != "/v1internal:streamGenerateContent" {
                                return StatusCode::NOT_FOUND.into_response();
                            }
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
                                if in_band_429 {
                                    let frame = json!({"error":{"code":429,"message":"fixture private error","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"13s"}]}});
                                    return Ok(bytes::Bytes::from(format!("data: {frame}\n\ndata: [DONE]\n\n")));
                                }
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
                gateway.clone(),
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
        let expected_type = if in_band_429 { "rate_limit_error" } else { "upstream_read_error" };
        assert!(wire.contains(expected_type), "{channel}: {wire}");
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
        assert_eq!(errors[0]["error"]["type"], expected_type);
        if in_band_429 {
            assert_eq!(errors[0]["error"]["code"], 429);
            assert!(!wire.contains("fixture private error"));
            assert!(!gateway.quota_registry.is_model_schedulable("local-subscription", UPSTREAM_MODEL));
        }
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
            Some(if in_band_429 { "upstream_error" } else { "upstream_read_error" })
        );
        assert_eq!(
            row.diagnostic.failure_stage.as_deref(),
            Some(if in_band_429 { "upstream_response" } else { "upstream_read" })
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
    stream_subscription_error_after_finish("antigravity", false)
}

#[test]
fn openai_stream_gemini_cli_read_error_after_finish_is_not_success() -> anyhow::Result<()> {
    stream_subscription_error_after_finish("gemini-cli", false)
}

#[test]
fn antigravity_429_after_delivered_content_cools_only_that_model_and_never_replays()
-> anyhow::Result<()> {
    stream_subscription_error_after_finish("antigravity", true)
}

#[test]
fn antigravity_in_band_forbidden_before_output_returns_403_on_both_paths() -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base = format!("http://{}", listener.local_addr()?);
    let gateway = offline_gateway("antigravity", &base)?;
    runtime()?.block_on(async move {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let app = Router::new().fallback(move |uri: Uri| {
            let count = count.clone();
            async move {
                assert_eq!(uri.path(), "/v1internal:streamGenerateContent");
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let err = json!({"error":{"code":403,"message":"private details"}});
                // EOF tail without a blank line exercises the final decoder flush.
                (
                    [("content-type", "text/event-stream")],
                    format!("data: {err}"),
                )
            }
        });
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
        bind_fixture_credential(&gateway).await?;
        for stream in [false, true] {
            let response = fixture_chat(&gateway, stream).await?;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            let error = serde_json::from_slice::<Value>(&bytes)?;
            assert_eq!(error["error"]["code"], 403);
            assert!(!error.to_string().contains("private details"));
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        Ok(())
    })
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
            fast_mode: Default::default(),
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

/// Scripted Antigravity upstream: records every inference body and answers with
/// the next scripted reply (`opaque` / `retry` 429s, otherwise SSE success).
async fn scripted_upstream(
    State((seen, script)): State<(Requests, Arc<Mutex<Vec<&'static str>>>)>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Response {
    // The 429 path triggers a best-effort quota refresh; its catalog call
    // lands here too. Keep it out of the inference script and fail it.
    if uri.path() != "/v1internal:streamGenerateContent" {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    seen.lock().unwrap().push(Seen {
        uri: uri.clone(),
        body: body.clone(),
    });
    let next = {
        let mut script = script.lock().unwrap();
        if script.is_empty() {
            "ok"
        } else {
            script.remove(0)
        }
    };
    match next {
        // The exact detail-free refusal Antigravity returned on 2026-11-15
        // for a third-party agent identity in systemInstruction.
        "opaque" => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": {
                "code": 429,
                "message": "Resource has been exhausted (e.g. check quota).",
                "status": "RESOURCE_EXHAUSTED"
            }})),
        )
            .into_response(),
        "retry" => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": {
                "code": 429,
                "message": "Resource has been exhausted (e.g. check quota).",
                "status": "RESOURCE_EXHAUSTED",
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"},
                    {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "30s"}
                ]
            }})),
        )
            .into_response(),
        _ => upstream(State(Requests::default()), uri, Json(body)).await,
    }
}

#[test]
fn antigravity_relocates_system_prompt_and_scopes_detail_free_429_to_one_request()
-> anyhow::Result<()> {
    use nyro_core::admin::ProviderUsageTier;
    use nyro_core::quota_buckets::{QuotaBucket, QuotaBucketBinding};
    use nyro_core::router::quota::ProviderScheduling;
    const SYSTEM: &str = "You are an AI agent powered by DeepSeek Harness.";
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let gateway = offline_gateway("antigravity", &base_url)?;
    runtime()?.block_on(async move {
        let seen = Requests::default();
        let script = Arc::new(Mutex::new(vec!["opaque", "ok", "retry"]));
        let app = Router::new()
            .route("/*path", post(scripted_upstream))
            .with_state((seen.clone(), script.clone()));
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        bind_fixture_credential(&gateway).await?;
        // Admission only trusts buckets of the credential's own owner scope.
        gateway
            .storage
            .settings()
            .set("quota_bucket_owner.local-subscription", "fixture-owner")
            .await?;
        let scope_id = nyro_core::quota_buckets::google_quota_scope_id(
            "local-subscription",
            "google_antigravity",
            "fixture-owner",
            "fixture-project",
        );
        // A resolved enforced bucket: the legacy path would cool the model down.
        let reset = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        gateway.quota_registry.observe_buckets(
            "local-subscription",
            &[QuotaBucket {
                id: "gemini-models".into(),
                scope_id,
                label: "Gemini Models".into(),
                models: vec![UPSTREAM_MODEL.into()],
                binding: QuotaBucketBinding {
                    status: "resolved".into(),
                    source: "official".into(),
                    revision: 1,
                },
                tiers: vec![ProviderUsageTier {
                    name: "five_hour".into(),
                    used_percent: 1.0,
                    resets_at: Some(reset),
                }],
                constraint_ids: vec![],
                source: "enforced_summary".into(),
                queried_at: chrono::Utc::now().to_rfc3339(),
                stale: false,
                scheduling: ProviderScheduling::default(),
                api_price: None,
            }],
            Some(true),
        );
        let send = |gateway: Gateway| async move {
            let body = json!({
                "model": CLIENT_MODEL,
                "messages": [
                    {"role": "system", "content": SYSTEM},
                    {"role": "user", "content": "hi"}
                ],
            });
            let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
            let request = ingress
                .handler()
                .make_request_decoder()
                .decode_request(body.clone())?;
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
                    RequestContext::new(ingress, TIMEOUT),
                ),
            )
            .await?;
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            anyhow::Ok((status, headers, serde_json::from_slice::<Value>(&bytes)?))
        };

        // 1. The system prompt rides a leading user turn, never systemInstruction.
        let (status, _, body) = send(gateway.clone()).await?;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{body} seen={:?}",
            seen.lock().unwrap().len()
        );
        {
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let inner = &requests[0].body["request"];
            assert!(inner.get("systemInstruction").is_none(), "{inner}");
            assert_eq!(inner["contents"][0]["role"], "user");
            assert_eq!(inner["contents"][0]["parts"][0]["text"], SYSTEM);
            assert_eq!(inner["contents"][1]["parts"][0]["text"], "hi");
        }
        // 2. A detail-free 429 is request-scoped: no local cooldown, so the
        //    very next request reaches Google again and succeeds.
        assert!(
            gateway
                .quota_registry
                .is_model_schedulable("local-subscription", UPSTREAM_MODEL)
        );
        let (status, _, body) = send(gateway.clone()).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(seen.lock().unwrap().len(), 2);

        // 3. A structured RetryInfo 429 still cools the wire model down, and the
        //    blocked request now explains why and when to retry.
        let (status, _, _) = send(gateway.clone()).await?;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            !gateway
                .quota_registry
                .is_model_schedulable("local-subscription", UPSTREAM_MODEL)
        );
        let (status, headers, body) = send(gateway.clone()).await?;
        assert_eq!(
            seen.lock().unwrap().len(),
            3,
            "blocked locally, not sent upstream"
        );
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["type"], "NYRO_QUOTA_UNAVAILABLE", "{body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("cooling down after a rate-limit response"),
            "{message}"
        );
        assert!(message.contains("retry after"), "{message}");
        assert!(!message.contains("RESOURCE_EXHAUSTED"), "{message}");
        let retry_after: u64 = headers["retry-after"].to_str()?.parse()?;
        assert!((1..=30).contains(&retry_after), "{retry_after}");
        Ok(())
    })
}

/// Replays one recorded-shape Antigravity tool turn: two parallel calls, the
/// first carrying a thoughtSignature, finishReason STOP and usage metadata.
async fn tool_turn_upstream(uri: Uri) -> Response {
    if uri.path() != "/v1internal:streamGenerateContent" {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let turn = json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "main.py"}}, "thoughtSignature": "c2lnbmF0dXJl"},
            {"functionCall": {"name": "read_file", "args": {"path": "utils.py"}}}
        ]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
        "modelVersion": "gemini-4-hydrogen"
    }});
    (
        [("content-type", "text/event-stream")],
        format!("data: {turn}\r\n\r\n"),
    )
        .into_response()
}

#[test]
fn antigravity_tool_turns_reach_anthropic_and_responses_clients_intact() -> anyhow::Result<()> {
    for (ingress, stream) in [
        (ANTHROPIC_MESSAGES_2023_06_01, true),
        (ANTHROPIC_MESSAGES_2023_06_01, false),
        (OPENAI_RESPONSES_V1, false),
        (OPENAI_RESPONSES_V1, true),
        (OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, false),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let gateway = offline_gateway("antigravity", &base_url)?;
        let wire = runtime()?.block_on(async move {
            let app = Router::new().route("/*path", post(tool_turn_upstream));
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let _server = AbortOnDrop(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            bind_fixture_credential(&gateway).await?;
            let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}});
            let (body, path) = if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
                (
                    json!({"model": CLIENT_MODEL, "max_tokens": 1024, "stream": stream,
                        "messages": [{"role": "user", "content": "read both files"}],
                        "tools": [{"name": "read_file", "input_schema": schema}]}),
                    "/v1/messages",
                )
            } else if ingress == OPENAI_RESPONSES_V1 {
                (
                    json!({"model": CLIENT_MODEL, "stream": stream, "input": "read both files",
                        "tools": [{"type": "function", "name": "read_file", "parameters": schema}]}),
                    "/v1/responses",
                )
            } else {
                (
                    json!({"model": CLIENT_MODEL, "stream": stream,
                        "messages": [{"role": "user", "content": "read both files"}],
                        "tools": [{"type": "function", "function": {"name": "read_file", "parameters": schema}}]}),
                    "/v1/chat/completions",
                )
            };
            let request = ingress
                .handler()
                .make_request_decoder()
                .decode_request(body.clone())?;
            let response = tokio::time::timeout(
                TIMEOUT,
                dispatch_pipeline(
                    gateway,
                    HeaderMap::new(),
                    RawEnvelope::new(Some(body), Default::default(), "POST", path),
                    request,
                    ingress,
                    RequestContext::new(ingress, TIMEOUT),
                ),
            )
            .await?;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            anyhow::Ok(String::from_utf8(bytes.to_vec())?)
        })?;
        let label = format!("{ingress} stream={stream}");
        let frames = || {
            wire.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                .collect::<Vec<_>>()
        };
        if ingress == ANTHROPIC_MESSAGES_2023_06_01 && stream {
            let frames = frames();
            let mut open: Option<u64> = None;
            let mut tools = Vec::new();
            for frame in &frames {
                let index = frame.get("index").and_then(Value::as_u64);
                match frame["type"].as_str() {
                    Some("content_block_start") => {
                        assert!(open.is_none(), "{label}: overlapping blocks: {wire}");
                        open = index;
                        tools.push(frame["content_block"]["name"].clone());
                    }
                    Some("content_block_delta") => assert_eq!(index, open, "{label}: {wire}"),
                    Some("content_block_stop") => {
                        assert_eq!(index, open, "{label}: {wire}");
                        open = None;
                    }
                    _ => {}
                }
            }
            assert_eq!(tools, vec!["read_file", "read_file"], "{label}: {wire}");
            let stop = frames
                .iter()
                .find(|f| f["type"] == "message_delta")
                .unwrap();
            assert_eq!(stop["delta"]["stop_reason"], "tool_use", "{label}");
        } else if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
            let body: Value = serde_json::from_str(&wire)?;
            assert_eq!(body["stop_reason"], "tool_use", "{label}: {body}");
            let inputs: Vec<&Value> = body["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .map(|block| &block["input"])
                .collect();
            assert_eq!(
                inputs,
                vec![&json!({"path": "main.py"}), &json!({"path": "utils.py"})]
            );
        } else if ingress == OPENAI_RESPONSES_V1 {
            let output = if stream {
                frames()
                    .into_iter()
                    .find(|frame| frame["type"] == "response.completed")
                    .map(|frame| frame["response"]["output"].clone())
                    .expect("response.completed")
            } else {
                serde_json::from_str::<Value>(&wire)?["output"].clone()
            };
            let calls: Vec<&str> = output
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call")
                .map(|item| item["arguments"].as_str().unwrap())
                .collect();
            assert_eq!(
                calls,
                vec![r#"{"path":"main.py"}"#, r#"{"path":"utils.py"}"#],
                "{label}: {wire}"
            );
        } else {
            let body: Value = serde_json::from_str(&wire)?;
            assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(
                body["choices"][0]["message"]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
    Ok(())
}
