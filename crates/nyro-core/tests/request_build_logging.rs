//! Request-build failures through the real dispatcher, collector and public Admin API.
//! All HTTP is loopback-only. Gateway background discovery/OAuth jobs are never polled.

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
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{LogQuery, Model, ModelBackend, Provider, ProviderProtocolEndpoint, RequestLog},
    logging::{ENABLE_PAYLOAD_KEY, LogEntry, diagnostics::OUTCOME_VERSION, run_collector},
    protocol::{
        ids::{GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1},
        ir::RawEnvelope,
    },
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::{MemoryStorage, SqliteStorage, Storage},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const CLIENT_MODEL: &str = "request-build-log-route";
const TIMEOUT: Duration = Duration::from_secs(5);
const PROVIDER_TOKEN: &str = "opaque-provider-credential-fixture";
const CLIENT_TOKEN: &str = "opaque-client-credential-fixture";

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Enter without driving a current-thread runtime, poll construction once, then
/// drop it. The MemoryStorage constructor must be Ready; a future yield fails
/// closed. Queued models.dev/usage/OAuth tasks never get a poll or network I/O.
fn offline_gateway(
    config: GatewayConfig,
    providers: Vec<Provider>,
    model: Model,
) -> anyhow::Result<(Gateway, mpsc::Receiver<LogEntry>)> {
    let storage = Arc::new(MemoryStorage::new(providers, vec![model], vec![]));
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
    result
}

fn provider(id: &str, vendor: &str, base_url: &str) -> Provider {
    Provider {
        keys: vec![],
        id: id.into(),
        name: format!("local-{id}"),
        vendor: Some(vendor.into()),
        protocol: if vendor == "google" {
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string()
        } else {
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string()
        },
        base_url: base_url.into(),
        protocol_mode: "fixed".into(),
        protocol_endpoints: vec![],
        preset_key: None,
        channel: Some("default".into()),
        models_source: None,
        static_models: None,
        api_key: PROVIDER_TOKEN.into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
        fast_mode: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn target(provider: &Provider, index: usize, fallback: bool) -> ModelBackend {
    ModelBackend {
        id: format!("backend-{index}"),
        model_id: "build-log-model".into(),
        provider_id: provider.id.clone(),
        model: if provider.vendor.as_deref() == Some("google") {
            "gemini-2.0-flash".into()
        } else {
            "local-chat-model".into()
        },
        weight: 100,
        priority: index as i32,
        is_fallback: fallback,
        created_at: String::new(),
    }
}

#[derive(Clone, Debug)]
struct Seen {
    uri: Uri,
    body: Value,
}

async fn upstream(
    State(seen): State<Arc<Mutex<Vec<Seen>>>>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Json<Value> {
    seen.lock().unwrap().push(Seen { uri, body });
    Json(json!({
        "id": "local-success", "object": "chat.completion", "model": "local-chat-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
    }))
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Fixture {
    gateway: Gateway,
    logs: Option<mpsc::Receiver<LogEntry>>,
    listener: Option<std::net::TcpListener>,
    seen: Arc<Mutex<Vec<Seen>>>,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(
        build_providers: impl FnOnce(&str) -> Vec<Provider>,
        fallback: bool,
    ) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let providers = build_providers(&base_url);
        let targets: Vec<_> = providers
            .iter()
            .enumerate()
            .map(|(i, provider)| target(provider, i, fallback && i + 1 == providers.len()))
            .collect();
        let first = targets.first().expect("at least one target");
        let model = Model {
            id: "build-log-model".into(),
            name: CLIENT_MODEL.into(),
            balance: "priority".into(),
            target_provider: first.provider_id.clone(),
            target_model: first.model.clone(),
            enable_auth: false,
            enable_payload: Some(false),
            force_max_reasoning: false,
            vision_shim: None,
            is_enabled: true,
            created_at: String::new(),
            targets,
        };
        let dir = tempfile::tempdir()?;
        let (gateway, logs) = offline_gateway(
            GatewayConfig {
                data_dir: dir.path().into(),
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            providers,
            model,
        )?;
        Ok(Self {
            gateway,
            logs: Some(logs),
            listener: Some(listener),
            seen: Arc::new(Mutex::new(Vec::new())),
            dir,
        })
    }

    fn start(&mut self) -> anyhow::Result<AbortOnDrop> {
        let listener = tokio::net::TcpListener::from_std(self.listener.take().unwrap())?;
        let app = Router::new()
            .route("/*path", post(upstream))
            .with_state(self.seen.clone());
        Ok(AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })))
    }

    async fn dispatch(&self, body: Value) -> anyhow::Result<(StatusCode, String, String)> {
        let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        let request = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        let headers: HeaderMap = HeaderMap::from_iter([
            (
                axum::http::header::AUTHORIZATION,
                format!("Bearer {CLIENT_TOKEN}").parse()?,
            ),
            (
                axum::http::header::HeaderName::from_static("x-api-key"),
                CLIENT_TOKEN.parse()?,
            ),
        ]);
        let envelope_headers = headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap().to_string(),
                )
            })
            .collect::<HashMap<_, _>>();
        let ctx = RequestContext::new(ingress, TIMEOUT);
        let expected_id = ctx.request_id.clone();
        let response = tokio::time::timeout(
            TIMEOUT,
            dispatch_pipeline(
                self.gateway.clone(),
                headers,
                RawEnvelope::new(Some(body), envelope_headers, "POST", "/v1/chat/completions"),
                request,
                ingress,
                ctx,
            ),
        )
        .await?;
        let status = response.status();
        let request_id = response.headers()["x-nyro-request-id"]
            .to_str()?
            .to_string();
        assert_eq!(request_id, expected_id);
        let bytes = tokio::time::timeout(
            TIMEOUT,
            axum::body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await??;
        Ok((status, request_id, String::from_utf8(bytes.to_vec())?))
    }

    /// Keep Gateway construction offline/in-memory, but persist its real log
    /// receiver through the normal SQLite sink. Closing the receiver drains and
    /// flushes deterministically even while the Gateway still owns a sender.
    /// The returned Gateway uses this sink for public AdminService queries.
    async fn persisted_gateway(&mut self) -> anyhow::Result<Gateway> {
        let storage = Arc::new(
            SqliteStorage::from_config(&GatewayConfig {
                data_dir: self.dir.path().join("logs"),
                ..Default::default()
            })
            .await?,
        );
        storage.settings().set(ENABLE_PAYLOAD_KEY, "false").await?;
        let mut logs = self.logs.take().expect("flush once");
        logs.close();
        run_collector(logs, storage.clone()).await;
        let mut gateway = self.gateway.clone();
        gateway.storage = storage;
        Ok(gateway)
    }
}

fn chat_request(stream: bool, schema: Option<Value>) -> Value {
    let mut body = json!({
        "model": CLIENT_MODEL, "stream": stream,
        "messages": [{"role": "user", "content": "private request body fixture"}]
    });
    if let Some(schema) = schema {
        body["tools"] = json!([{"type": "function", "function": {
            "name": "lookup", "parameters": schema
        }}]);
    }
    body
}

fn overlapping_schema(same_type: bool) -> Value {
    // Neither case is a lossless union: integer overlaps number, and the two
    // string branches overlap. They must stay rejected after disjoint-oneOf
    // lowering is expanded; do not use a newly supported disjoint union here.
    let alternatives = if same_type {
        json!([{"type": "string", "minLength": 1}, {"type": "string", "maxLength": 5}])
    } else {
        json!([{"type": "number"}, {"type": "integer"}])
    };
    // Adversarial field name: the typed schema path contains the provider's
    // opaque fake credential. It may appear in controlled raw payload evidence,
    // never in error_message/error_causes or scalar diagnostic metadata.
    let mut properties = serde_json::Map::new();
    properties.insert(PROVIDER_TOKEN.into(), json!({"oneOf": alternatives}));
    json!({"type": "object", "properties": properties})
}

fn metadata(row: &RequestLog) -> Value {
    serde_json::from_str(row.payload_metadata.as_deref().unwrap()).unwrap()
}

fn assert_safe_diagnostics(row: &RequestLog) {
    let safe = json!({
        "error_message": row.error_message,
        "error_causes": row.error_causes,
        "payload_metadata": row.payload_metadata,
        "client_request_headers": row.client_request_headers,
        "upstream_request_headers": row.upstream_request_headers,
    })
    .to_string();
    assert!(
        !safe.contains(PROVIDER_TOKEN),
        "provider credential in scalar diagnostics"
    );
    assert!(
        !safe.contains(CLIENT_TOKEN),
        "client credential in scalar diagnostics"
    );
}

fn assert_local_failure(
    row: &RequestLog,
    request_id: &str,
    status: i32,
    kind: &str,
    builder: &str,
) {
    assert_eq!(row.client_request_id.as_deref(), Some(request_id));
    assert_eq!(row.outcome_version, OUTCOME_VERSION);
    assert_eq!(row.attempt_outcome, "failed");
    assert_eq!(row.client_status_code, Some(status));
    assert_eq!(row.failure_stage.as_deref(), Some("request_build"));
    assert_eq!(row.failure_kind.as_deref(), Some(kind));
    assert!(row.is_error);
    assert!(row.upstream_status_code.is_none());
    assert!(row.upstream_url.is_none());
    assert!(row.upstream_request_body.is_none());
    assert!(row.upstream_request_headers.is_none());
    assert!(row.upstream_response_body.is_none());
    assert!(row.upstream_response_headers.is_none());
    assert!(row.latency_upstream_ms.is_none());
    assert_eq!(row.input_tokens, 0);
    assert_eq!(row.output_tokens, 0);
    assert_eq!(row.stream_chunks_count, 0);
    assert!(row.stream_first_chunk_ms.is_none());
    let meta = metadata(row);
    assert_eq!(meta["request_build"]["builder"], builder);
    assert_eq!(meta["request_build"]["upstream_contacted"], false);
    assert_eq!(meta["upstream_response_body"]["capture_state"], "absent");
    // Both switches are off: abnormal outcomes still retain bounded, redacted
    // client evidence, without inventing an outbound body or upstream status.
    assert!(row.client_request_body.is_some());
    assert_eq!(meta["client_request_body"]["capture_state"], "captured");
    let headers: Value =
        serde_json::from_str(row.client_request_headers.as_deref().unwrap()).unwrap();
    assert_eq!(headers["authorization"], "***");
    assert_eq!(headers["x-api-key"], "***");
    assert_safe_diagnostics(row);
}

#[test]
fn native_gemini_encoding_422_is_queryable_with_correlated_retained_error() -> anyhow::Result<()> {
    for same_type in [false, true] {
        for stream in [false, true] {
            let mut fixture = Fixture::new(|base| vec![provider("gemini", "google", base)], false)?;
            runtime()?.block_on(async {
                let _server = fixture.start()?;
                let body = chat_request(stream, Some(overlapping_schema(same_type)));
                let (status, request_id, rendered) = fixture.dispatch(body.clone()).await?;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rendered}");
                let response: Value = serde_json::from_str(&rendered)?;
                assert_eq!(response["error"]["type"], "NYRO_PROTOCOL_LOSSY_REJECTED");
                assert_eq!(response["error"]["request_id"], request_id);
                assert!(rendered.contains("oneOf"));
                assert!(
                    fixture.seen.lock().unwrap().is_empty(),
                    "encoder failure contacted upstream"
                );
                let persisted = fixture.persisted_gateway().await?;
                let page = persisted
                    .admin()
                    .query_logs(LogQuery {
                        client_request_id: Some(request_id.clone()),
                        ..Default::default()
                    })
                    .await?;
                assert_eq!(page.total, 1);
                let row = persisted.admin().get_log(&page.items[0].id).await?.unwrap();
                assert_local_failure(
                    &row,
                    &request_id,
                    422,
                    "protocol_lossy_rejected",
                    "build_request",
                );
                assert_eq!(row.attempt_index, Some(1));
                assert_eq!(
                    row.client_protocol.as_deref(),
                    Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string().as_str())
                );
                assert_eq!(
                    row.upstream_protocol.as_deref(),
                    Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string().as_str())
                );
                assert_eq!(row.provider_id.as_deref(), Some("gemini"));
                assert_eq!(row.model_id.as_deref(), Some("build-log-model"));
                assert_eq!(row.client_model.as_deref(), Some(CLIENT_MODEL));
                assert_eq!(row.upstream_model.as_deref(), Some("gemini-2.0-flash"));
                assert_eq!(row.is_stream, stream);
                assert!(
                    row.route_decision
                        .as_deref()
                        .unwrap()
                        .contains("local-gemini")
                );
                let meta = metadata(&row);
                assert_eq!(
                    meta["request_build"]["gateway_error_code"],
                    "NYRO_PROTOCOL_LOSSY_REJECTED"
                );
                assert_eq!(meta["request_build"]["lost_field_count"], 1);
                assert_eq!(meta["request_build"]["conversion_strategy"], "native_ir");
                assert_eq!(
                    meta["request_build"]["conversion_rule"],
                    "cross-protocol-ir"
                );
                assert_eq!(row.client_response_body.as_deref(), Some(rendered.as_str()));
                assert_eq!(
                    serde_json::from_str::<Value>(row.client_request_body.as_deref().unwrap())?,
                    body
                );
                assert_eq!(meta["client_response_body"]["complete"], true);
                let result = row.request_result.unwrap();
                assert_eq!(result.final_outcome, "failed");
                assert_eq!(result.attempt_count, 1);
                assert_eq!(result.final_attempt_id.as_deref(), Some(row.id.as_str()));
                // A local encoder rejection is not provider-health evidence.
                assert!(
                    fixture
                        .gateway
                        .health_registry
                        .open_provider_secs("gemini")
                        .is_none()
                );
                Ok::<_, anyhow::Error>(())
            })?;
        }
    }
    Ok(())
}

#[test]
fn passthrough_auth_header_build_error_is_local_and_sanitized() -> anyhow::Result<()> {
    let mut fixture = Fixture::new(
        |base| {
            let mut provider = provider("passthrough", "openai", base);
            provider.protocol_mode = "adaptive".into();
            provider.protocol_endpoints = vec![ProviderProtocolEndpoint {
                id: "local-endpoint".into(),
                provider_id: provider.id.clone(),
                protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
                base_url: base.into(),
                api_key: format!("{PROVIDER_TOKEN}\n{PROVIDER_TOKEN}"),
                auth_scheme: "bearer".into(),
                is_enabled: true,
                priority: 0,
                test_status: "untested".into(),
                test_error: None,
                tested_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            }];
            vec![provider]
        },
        false,
    )?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let (status, request_id, rendered) = fixture.dispatch(chat_request(false, None)).await?;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{rendered}");
        let response: Value = serde_json::from_str(&rendered)?;
        assert_eq!(response["error"]["type"], "NYRO_INTERNAL_ERROR");
        assert_eq!(response["error"]["request_id"], request_id);
        assert!(!rendered.contains(PROVIDER_TOKEN));
        assert!(fixture.seen.lock().unwrap().is_empty());
        let persisted = fixture.persisted_gateway().await?;
        let attempts = persisted
            .admin()
            .get_request_log_attempts(&request_id)
            .await?;
        assert_eq!(attempts.attempts.len(), 1);
        let row = persisted
            .admin()
            .get_log(&attempts.attempts[0].id)
            .await?
            .unwrap();
        assert_local_failure(&row, &request_id, 500, "request_build", "passthrough");
        assert_eq!(row.client_response_body.as_deref(), Some(rendered.as_str()));
        assert_eq!(
            metadata(&row)["request_build"]["gateway_error_code"],
            "NYRO_INTERNAL_ERROR"
        );
        assert_eq!(attempts.result.unwrap().final_outcome, "failed");
        Ok(())
    })
}

#[test]
fn all_failed_targets_keep_monotonic_indices_and_only_last_request_result() -> anyhow::Result<()> {
    let mut fixture = Fixture::new(
        |base| {
            (0..3)
                .map(|index| provider(&format!("gemini-{index}"), "google", base))
                .collect()
        },
        false,
    )?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let (status, request_id, rendered) = fixture
            .dispatch(chat_request(false, Some(overlapping_schema(false))))
            .await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(fixture.seen.lock().unwrap().is_empty());
        let persisted = fixture.persisted_gateway().await?;
        let attempts = persisted
            .admin()
            .get_request_log_attempts(&request_id)
            .await?;
        assert_eq!(attempts.attempts.len(), 3);
        for (index, summary) in attempts.attempts.iter().enumerate() {
            let row = persisted.admin().get_log(&summary.id).await?.unwrap();
            assert_local_failure(
                &row,
                &request_id,
                422,
                "protocol_lossy_rejected",
                "build_request",
            );
            assert_eq!(row.attempt_index, Some(index as i32 + 1));
            assert_eq!(row.provider_id, Some(format!("gemini-{index}")));
        }
        let last = persisted
            .admin()
            .get_log(&attempts.attempts[2].id)
            .await?
            .unwrap();
        assert_eq!(
            last.client_response_body.as_deref(),
            Some(rendered.as_str())
        );
        let result = attempts
            .result
            .expect("all-build-failed request must have a final result");
        assert_eq!(result.final_outcome, "failed");
        assert_eq!(result.attempt_count, 3);
        assert_eq!(result.final_attempt_id.as_deref(), Some(last.id.as_str()));
        Ok(())
    })
}

#[test]
fn build_failures_do_not_consume_upstream_budget_or_mark_successful_fallback_failed()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new(
        |base| {
            let mut providers: Vec<_> = (0..9)
                .map(|index| provider(&format!("gemini-{index}"), "google", base))
                .collect();
            providers.push(provider("fallback", "openai", base));
            providers
        },
        true,
    )?;
    runtime()?.block_on(async {
        let _server = fixture.start()?;
        let (status, request_id, rendered) = fixture
            .dispatch(chat_request(false, Some(overlapping_schema(false))))
            .await?;
        assert_eq!(status, StatusCode::OK, "{rendered}");
        {
            let seen = fixture.seen.lock().unwrap();
            assert_eq!(
                seen.len(),
                1,
                "only the successful fallback may contact upstream"
            );
            assert_eq!(seen[0].uri.path(), "/v1/chat/completions");
            assert_eq!(seen[0].body["model"], "local-chat-model");
        }
        let persisted = fixture.persisted_gateway().await?;
        let attempts = persisted
            .admin()
            .get_request_log_attempts(&request_id)
            .await?;
        assert_eq!(
            attempts.attempts.len(),
            10,
            "one log per failure and exactly one success"
        );
        for (index, summary) in attempts.attempts.iter().enumerate() {
            assert_eq!(summary.attempt_index, Some(index as i32 + 1));
            let row = persisted.admin().get_log(&summary.id).await?.unwrap();
            if index < 9 {
                assert_local_failure(
                    &row,
                    &request_id,
                    422,
                    "protocol_lossy_rejected",
                    "build_request",
                );
            } else {
                assert_eq!(row.provider_id.as_deref(), Some("fallback"));
                assert_eq!(row.client_status_code, Some(200));
                assert_eq!(row.upstream_status_code, Some(200));
                assert_eq!(row.attempt_outcome, "completed");
                assert!(!row.is_error);
                assert!(row.failure_stage.is_none());
                assert_eq!(row.input_tokens, 2);
                assert_eq!(row.output_tokens, 1);
                // Ordinary success obeys both payload-off switches; failures
                // above retain evidence via the same, unchanged collector.
                assert!(row.client_request_body.is_none());
                assert!(row.upstream_request_body.is_none());
            }
        }
        let result = attempts
            .result
            .expect("successful fallback must select its own result");
        assert_eq!(result.final_outcome, "completed");
        assert_eq!(result.attempt_count, 10);
        assert_eq!(
            result.final_attempt_id.as_deref(),
            Some(attempts.attempts[9].id.as_str())
        );
        assert_eq!(
            persisted
                .admin()
                .query_logs(LogQuery {
                    client_request_id: Some(request_id),
                    is_error: Some(true),
                    ..Default::default()
                })
                .await?
                .total,
            9
        );
        Ok(())
    })
}
