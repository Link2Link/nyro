//! Model-aware quota admission through the public HTTP server and real log collector.
//!
//! All provider/model/bucket state is local to each fixture. Construction is
//! single-polled on an undriven runtime, cancelling models.dev/usage/OAuth jobs
//! before they can run. The only HTTP peers are two ephemeral loopback servers;
//! no discovery, real credentials, or external quota API is involved.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    admin::ProviderUsageTier,
    config::GatewayConfig,
    db::models::{LogQuery, Model, ModelBackend, Provider, RequestLog},
    error::GatewayError,
    logging::{ENABLE_PAYLOAD_KEY, LogEntry, run_collector},
    protocol::{
        ids::{OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId},
        ir::{AiRequest, AiResponse},
    },
    provider::{
        common::pipeline,
        custom::CustomVendor,
        inbound::InboundResponse,
        outbound::OutboundRequest,
        registry::{VendorRegistration, VendorScope},
        vendor::{ProviderCtx, Vendor},
        vendor_ext::VendorCtx,
    },
    proxy::server::create_router,
    quota_buckets::{QuotaAttribution, QuotaBucket, QuotaBucketBinding, opaque_quota_id},
    router::quota::{ProviderScheduling, ProviderSchedulingStatus},
    storage::{MemoryStorage, SqliteStorage, Storage},
};
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::sync::mpsc;

const PROVIDER: &str = "local-quota-provider";
const TOKEN: &str = "inert-local-quota-api-key";
const FLASH: &str = "gemini-3.8-flash";
const ARGON: &str = "gemini-4-argon";
const FLASH_ALIAS: &str = "local-flash-backend-alias";
const OWNER_SWITCH_ALIAS: &str = "local-owner-switch-backend-alias";
const FLASH_HIGH: &str = "gemini-3.8-flash-high";
const REWRITE_VENDOR: &str = "quota-route-wire-rewrite-fixture";
const REVISION: u64 = 7;
const TIMEOUT: Duration = Duration::from_secs(5);

/// A fixture-only custom OpenAI vendor exercising the real post-encode hook.
/// It simulates a channel's alias -> wire-tier rewrite without Google OAuth,
/// discovery, or replacing any production vendor registration.
struct WireRewriteVendor;

#[async_trait]
impl Vendor for WireRewriteVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: REWRITE_VENDOR,
        }
    }

    fn vendor_id(&self) -> &'static str {
        REWRITE_VENDOR
    }

    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
    }

    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> reqwest::header::HeaderMap {
        CustomVendor.auth_headers(ctx)
    }

    fn build_url(&self, ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        CustomVendor.build_url(ctx, base_url, path)
    }

    async fn pre_request(
        &self,
        ctx: &VendorCtx<'_>,
        _request: &mut AiRequest,
        gateway: &Gateway,
    ) -> anyhow::Result<()> {
        if ctx.actual_model == OWNER_SWITCH_ALIAS {
            // Deterministic owner replacement after credential/admission
            // preparation started, but before final request admission. Both
            // owners are healthy: percentages alone cannot detect this race.
            let mut replacement = bucket("Replacement Flash", &[FLASH_HIGH], 5.0, 10.0);
            replacement.scope_id = replacement_scope_id();
            replacement.id = opaque_quota_id(
                "offline_replacement_bucket_v1",
                &[&replacement.scope_id, "flash"],
            );
            gateway
                .quota_registry
                .observe_buckets(&ctx.provider.id, &[replacement], Some(true));
        }
        Ok(())
    }

    async fn post_encode(
        &self,
        _ctx: &VendorCtx<'_>,
        body: &mut Value,
        _headers: &mut reqwest::header::HeaderMap,
    ) -> anyhow::Result<()> {
        if body["model"] == FLASH_ALIAS || body["model"] == OWNER_SWITCH_ALIAS {
            body["model"] = json!(FLASH_HIGH);
        }
        Ok(())
    }

    async fn build_request(
        &self,
        request: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        pipeline::build_request(self, request, ctx).await
    }

    async fn parse_response(
        &self,
        response: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, response, ctx).await
    }

    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        CustomVendor.map_error(status, body)
    }
}

inventory::submit! {
    VendorRegistration { make: || Box::new(WireRewriteVendor) }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Do not block_on Gateway construction: it queues unconditional background
/// discovery/monitor tasks. Fresh MemoryStorage locks must let the constructor
/// finish in ONE poll. Entering a current-thread runtime does not drive tasks;
/// dropping it cancels all queued jobs without a single network-capable poll.
/// A future constructor yield fails closed instead of silently going online.
fn offline_gateway(
    config: GatewayConfig,
    provider: Provider,
    models: Vec<Model>,
) -> anyhow::Result<(Gateway, mpsc::Receiver<LogEntry>)> {
    let storage = Arc::new(MemoryStorage::new(
        vec![provider],
        models,
        vec![(ENABLE_PAYLOAD_KEY.into(), "false".into())],
    ));
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
    let (mut gateway, logs) = result?;
    gateway.http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()?;
    Ok((gateway, logs))
}

fn provider(vendor: &str, base_url: String) -> Provider {
    Provider {
        id: PROVIDER.into(),
        name: "Local Flash and Argon".into(),
        vendor: Some(vendor.into()),
        protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
        base_url,
        protocol_mode: "fixed".into(),
        protocol_endpoints: vec![],
        keys: vec![],
        preset_key: None,
        channel: Some("default".into()),
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
    }
}

fn route(name: &str, balance: &str, upstream_models: &[&str]) -> Model {
    let id = format!("route-{name}");
    let targets: Vec<_> = upstream_models
        .iter()
        .enumerate()
        .map(|(index, model)| ModelBackend {
            id: format!("{id}-backend-{index}"),
            model_id: id.clone(),
            provider_id: PROVIDER.into(),
            model: (*model).into(),
            weight: 100,
            priority: index as i32,
            is_fallback: false,
            created_at: String::new(),
        })
        .collect();
    Model {
        id,
        name: name.into(),
        balance: balance.into(),
        target_provider: PROVIDER.into(),
        target_model: upstream_models[0].into(),
        enable_auth: false,
        enable_payload: Some(false),
        force_max_reasoning: false,
        vision_shim: None,
        is_enabled: true,
        created_at: String::new(),
        targets,
    }
}

fn scope_id() -> String {
    opaque_quota_id("offline_quota_scope_v1", &[PROVIDER, "local-entitlement"])
}

fn replacement_scope_id() -> String {
    opaque_quota_id(
        "offline_quota_scope_v1",
        &[PROVIDER, "replacement-entitlement"],
    )
}

fn bucket_id(label: &str) -> String {
    opaque_quota_id("offline_quota_bucket_v1", &[&scope_id(), label])
}

fn bucket(label: &str, models: &[&str], five_hour: f64, weekly: f64) -> QuotaBucket {
    let now = chrono::Utc::now();
    QuotaBucket {
        id: bucket_id(label),
        scope_id: scope_id(),
        label: label.into(),
        models: models.iter().map(|model| (*model).into()).collect(),
        binding: QuotaBucketBinding {
            status: "resolved".into(),
            source: "configured".into(),
            revision: REVISION,
        },
        tiers: vec![
            ProviderUsageTier {
                name: "five_hour".into(),
                used_percent: five_hour,
                resets_at: Some((now + chrono::Duration::hours(4)).to_rfc3339()),
            },
            ProviderUsageTier {
                name: "weekly_limit".into(),
                used_percent: weekly,
                resets_at: Some((now + chrono::Duration::days(6)).to_rfc3339()),
            },
        ],
        constraint_ids: vec![],
        source: "enforced_summary".into(),
        queried_at: now.to_rfc3339(),
        stale: false,
        scheduling: ProviderScheduling::default(),
        api_price: None,
    }
}

#[derive(Clone, Debug)]
struct Seen {
    uri: Uri,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<Seen>>>,
    reject_flash: bool,
}

async fn upstream_handler(
    State(upstream): State<Upstream>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    upstream
        .seen
        .lock()
        .unwrap()
        .push(Seen { uri, headers, body });
    if upstream.reject_flash && model == FLASH {
        // No resource ID, bucket, or structured allowance denial: this is an
        // unknown 429, not evidence that the whole entitlement is exhausted.
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "30")],
            Json(json!({"error": {
                "type": "rate_limit_error", "code": "temporary_throttle",
                "message": "Temporarily busy; retry later"
            }})),
        )
            .into_response();
    }
    Json(json!({
        "id": "local-quota-reply", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "local reply"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
    }))
    .into_response()
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Running {
    url: String,
    _upstream: AbortOnDrop,
    _proxy: AbortOnDrop,
}

struct Outcome {
    status: StatusCode,
    request_id: String,
    body: Value,
}

impl Outcome {
    fn assert_ok(&self) {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        assert_eq!(self.body["choices"][0]["message"]["content"], "local reply");
    }
}

struct Fixture {
    gateway: Gateway,
    logs: mpsc::Receiver<LogEntry>,
    upstream_listener: Option<std::net::TcpListener>,
    proxy_listener: Option<std::net::TcpListener>,
    upstream: Upstream,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(vendor: &str, models: Vec<Model>, buckets: Vec<QuotaBucket>) -> anyhow::Result<Self> {
        let upstream_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        upstream_listener.set_nonblocking(true)?;
        let base_url = format!("http://{}", upstream_listener.local_addr()?);
        let proxy_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        proxy_listener.set_nonblocking(true)?;
        let dir = tempfile::tempdir()?;
        let (gateway, logs) = offline_gateway(
            GatewayConfig {
                data_dir: dir.path().into(),
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            provider(vendor, base_url),
            models,
        )?;
        gateway
            .quota_registry
            .observe_buckets(PROVIDER, &buckets, Some(true));
        Ok(Self {
            gateway,
            logs,
            upstream_listener: Some(upstream_listener),
            proxy_listener: Some(proxy_listener),
            upstream: Upstream::default(),
            _dir: dir,
        })
    }

    fn start(&mut self) -> anyhow::Result<Running> {
        let upstream_listener =
            tokio::net::TcpListener::from_std(self.upstream_listener.take().unwrap())?;
        let app = Router::new()
            .route("/*path", post(upstream_handler))
            .with_state(self.upstream.clone());
        let upstream = AbortOnDrop(tokio::spawn(async move {
            axum::serve(upstream_listener, app).await.unwrap();
        }));
        let proxy_listener =
            tokio::net::TcpListener::from_std(self.proxy_listener.take().unwrap())?;
        let url = format!(
            "http://{}/v1/chat/completions",
            proxy_listener.local_addr()?
        );
        let app = create_router(self.gateway.clone());
        let proxy = AbortOnDrop(tokio::spawn(async move {
            axum::serve(proxy_listener, app).await.unwrap();
        }));
        Ok(Running {
            url,
            _upstream: upstream,
            _proxy: proxy,
        })
    }

    async fn request(&self, running: &Running, model: &str) -> anyhow::Result<Outcome> {
        self.request_protocol(running, model, false).await
    }

    async fn request_protocol(
        &self,
        running: &Running,
        model: &str,
        anthropic: bool,
    ) -> anyhow::Result<Outcome> {
        let url = if anthropic {
            running.url.replace("/v1/chat/completions", "/v1/messages")
        } else {
            running.url.clone()
        };
        let mut body = json!({
            "model": model,
            "stream": false,
            "messages": [{"role": "user", "content": "private offline fixture prompt"}],
            // These untrusted fields must NEVER create server attribution.
            "quota_attribution": {"bucket_id": "client-forged", "scope_id": "client-forged"},
            "route_decision": {"quota_attribution": {
                "version": 1, "bucket_id": "client-forged", "scope_id": "client-forged",
                "backend_model": "client-forged", "wire_model": "client-forged"
            }}
        });
        if anthropic {
            body["max_tokens"] = json!(64);
        }
        let response = self
            .gateway
            .http_client
            .post(url)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let request_id = response.headers()["x-nyro-request-id"].to_str()?.to_owned();
        let wire = response.text().await?;
        let body: Value = serde_json::from_str(&wire)?;
        Ok(Outcome {
            status,
            request_id,
            body,
        })
    }

    fn seen(&self) -> Vec<Seen> {
        self.upstream.seen.lock().unwrap().clone()
    }

    fn assert_wire_models(&self, expected: &[&str]) {
        let seen = self.seen();
        let models: Vec<_> = seen
            .iter()
            .map(|request| request.body["model"].as_str().unwrap())
            .collect();
        assert_eq!(
            models, expected,
            "wrong upstream attempt count/model: {seen:?}"
        );
        for request in &seen {
            assert_eq!(request.uri.path(), "/v1/chat/completions");
            assert_eq!(
                request.headers["authorization"].to_str().unwrap(),
                format!("Bearer {TOKEN}")
            );
        }
    }

    /// Receive exactly the actual finalized logs (no synthetic test entries),
    /// then use the normal collector and an isolated in-memory SQLite sink.
    /// The extra hop tests the payload-off flush policy and persisted JSON, not
    /// just an in-process decision struct. Never touches an application DB.
    async fn recorded_logs(&mut self, count: usize) -> anyhow::Result<Vec<RequestLog>> {
        // Requests are complete and their response bodies consumed before this
        // call. Closing seals the actual log stream before the exact-count
        // assertion, rather than leaving an open receiver between test phases.
        self.logs.close();
        let mut entries = Vec::new();
        for _ in 0..count {
            entries.push(
                tokio::time::timeout(TIMEOUT, self.logs.recv())
                    .await?
                    .expect("expected one finalized log per attempt/local rejection"),
            );
        }
        assert!(
            self.logs.try_recv().is_err(),
            "unexpected duplicate attempt log"
        );
        let ids: Vec<_> = entries
            .iter()
            .map(|entry| entry.diagnostic.log_id.clone())
            .collect();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .in_memory(true)
                    .foreign_keys(true),
            )
            .await?;
        let storage = Arc::new(SqliteStorage::from_pool(pool));
        storage.bootstrap().migrate().await?;
        storage.settings().set(ENABLE_PAYLOAD_KEY, "false").await?;
        let (tx, rx) = mpsc::channel(count + 1);
        for entry in entries {
            tx.try_send(entry)
                .expect("fixture log channel has capacity");
        }
        drop(tx);
        tokio::time::timeout(TIMEOUT, run_collector(rx, storage.clone())).await?;
        let page = storage.logs().query(LogQuery::default()).await?;
        assert_eq!(
            page.total, count as i64,
            "collector must retain exactly one row per event"
        );
        let mut rows = Vec::new();
        for id in ids {
            rows.push(
                storage
                    .logs()
                    .find_by_id(&id)
                    .await?
                    .expect("persisted attempt log"),
            );
        }
        Ok(rows)
    }
}

fn log_for<'a>(rows: &'a [RequestLog], outcome: &Outcome) -> &'a RequestLog {
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| row.client_request_id.as_deref() == Some(outcome.request_id.as_str()))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected one log for {}",
        outcome.request_id
    );
    matches[0]
}

fn decision(row: &RequestLog) -> Value {
    // Cross the public RequestLog JSON representation, in which route_decision
    // is itself JSON text, before checking the server-owned typed metadata.
    let wire = serde_json::to_string(row).unwrap();
    let row_json: Value = serde_json::from_str(&wire).unwrap();
    serde_json::from_str(
        row_json["route_decision"]
            .as_str()
            .expect("route decision JSON"),
    )
    .unwrap()
}

fn assert_attribution(row: &RequestLog, label: &str, backend: &str, wire_model: &str) {
    let decision = decision(row);
    let attribution: QuotaAttribution =
        serde_json::from_value(decision["quota_attribution"].clone())
            .expect("typed quota attribution");
    assert_eq!(attribution.version, 1);
    assert_eq!(attribution.bucket_id, bucket_id(label));
    assert_eq!(attribution.scope_id, scope_id());
    assert_eq!(attribution.backend_model, backend);
    assert_eq!(attribution.wire_model, wire_model);
    assert_eq!(attribution.source, "configured");
    assert_eq!(attribution.revision, REVISION);
    assert!(attribution.dispatched_at > 0);
    assert!(attribution.dispatched_at <= chrono::Utc::now().timestamp_millis());
    assert_eq!(row.provider_id.as_deref(), Some(PROVIDER));
    assert_eq!(row.upstream_model.as_deref(), Some(wire_model));
    let metadata = decision.to_string();
    assert!(
        !metadata.contains("client-forged"),
        "client supplied attribution was trusted"
    );
    assert!(
        !metadata.contains(TOKEN),
        "quota metadata exposed a credential"
    );
}

fn assert_payload_disabled(row: &RequestLog) {
    assert_eq!(row.client_status_code, Some(200));
    assert_eq!(row.upstream_status_code, Some(200));
    for payload in [
        &row.client_request_headers,
        &row.client_request_body,
        &row.client_response_headers,
        &row.client_response_body,
        &row.upstream_request_headers,
        &row.upstream_request_body,
        &row.upstream_response_headers,
        &row.upstream_response_body,
    ] {
        assert!(
            payload.is_none(),
            "ordinary payload was retained despite opt-out"
        );
    }
}

#[test]
fn configured_flash_and_argon_routes_on_one_provider_have_independent_buckets() -> anyhow::Result<()>
{
    let mut fixture = Fixture::new(
        "custom",
        vec![
            route("flash-route", "priority", &[FLASH]),
            route("argon-route", "priority", &[ARGON]),
        ],
        vec![
            bucket("Flash", &[FLASH], 10.0, 20.0),
            bucket("Argon", &[ARGON], 30.0, 40.0),
        ],
    )?;
    runtime()?.block_on(async {
        let running = fixture.start()?;
        let flash = fixture.request(&running, "flash-route").await?;
        flash.assert_ok();
        let argon = fixture.request(&running, "argon-route").await?;
        argon.assert_ok();
        fixture.assert_wire_models(&[FLASH, ARGON]);
        let rows = fixture.recorded_logs(2).await?;
        assert_attribution(log_for(&rows, &flash), "Flash", FLASH, FLASH);
        assert_attribution(log_for(&rows, &argon), "Argon", ARGON, ARGON);
        for row in &rows {
            assert_payload_disabled(row);
        }
        Ok(())
    })
}

#[test]
fn all_balances_skip_flash_when_either_applicable_window_is_exhausted() -> anyhow::Result<()> {
    for balance in ["priority", "weighted", "latency", "usage"] {
        for (five_hour, weekly, blocker) in
            [(100.0, 20.0, "five_hour"), (10.0, 100.0, "weekly_limit")]
        {
            let mut fixture = Fixture::new(
                "custom",
                vec![route("configured-alternatives", balance, &[FLASH, ARGON])],
                vec![
                    bucket("Flash", &[FLASH], five_hour, weekly),
                    bucket("Argon", &[ARGON], 15.0, 25.0),
                ],
            )?;
            runtime()?.block_on(async {
                let running = fixture.start()?;
                let response = fixture.request(&running, "configured-alternatives").await?;
                response.assert_ok();
                fixture.assert_wire_models(&[ARGON]);
                let rows = fixture.recorded_logs(1).await?;
                let row = log_for(&rows, &response);
                assert_payload_disabled(row);
                assert_attribution(row, "Argon", ARGON, ARGON);
                let decision = decision(row);
                assert_eq!(decision["balance"], balance);
                let candidates = decision["candidates"].as_array().unwrap();
                assert_eq!(candidates.len(), 2);
                let flash = candidates
                    .iter()
                    .find(|candidate| candidate["target"] == FLASH)
                    .unwrap();
                let argon = candidates
                    .iter()
                    .find(|candidate| candidate["target"] == ARGON)
                    .unwrap();
                assert_eq!(flash["provider"], PROVIDER);
                assert_eq!(argon["provider"], PROVIDER);
                assert_eq!(
                    flash["skipped"]["reason"], "quota_exhausted",
                    "{balance}/{blocker}: {decision}"
                );
                assert_eq!(flash["skipped"]["window"], blocker);
                assert_eq!(flash["quota"]["bucket_id"], bucket_id("Flash"));
                assert!(
                    argon.get("skipped").is_none(),
                    "healthy same-provider sibling was marked skipped: {decision}"
                );
                assert_eq!(argon["quota"]["bucket_id"], bucket_id("Argon"));
                Ok::<_, anyhow::Error>(())
            })?;
        }
    }
    Ok(())
}

#[test]
fn flash_only_route_never_substitutes_healthy_unconfigured_argon() -> anyhow::Result<()> {
    let mut fixture = Fixture::new(
        "custom",
        vec![route("flash-only", "priority", &[FLASH])],
        vec![
            bucket("Flash", &[FLASH], 100.0, 20.0),
            bucket("Argon", &[ARGON], 10.0, 20.0),
        ],
    )?;
    runtime()?.block_on(async {
        let running = fixture.start()?;
        let response = fixture.request(&running, "flash-only").await?;
        assert_eq!(
            response.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            response.body
        );
        fixture.assert_wire_models(&[]);
        let rows = fixture.recorded_logs(1).await?;
        let row = log_for(&rows, &response);
        assert!(row.upstream_status_code.is_none());
        assert!(
            decision(row).get("quota_attribution").is_none(),
            "local skip must not imply quota consumption"
        );
        assert!(
            fixture
                .gateway
                .quota_registry
                .is_model_schedulable(PROVIDER, ARGON)
        );
        Ok(())
    })
}

#[test]
fn wildcard_targets_materialize_requested_model_before_quota_admission() -> anyhow::Result<()> {
    for wildcard in ["*", ""] {
        let mut fixture = Fixture::new(
            "custom",
            vec![
                route(FLASH, "priority", &[wildcard]),
                route(ARGON, "priority", &[wildcard]),
            ],
            vec![
                bucket("Flash", &[FLASH], 10.0, 100.0),
                bucket("Argon", &[ARGON], 15.0, 25.0),
            ],
        )?;
        runtime()?.block_on(async {
            let running = fixture.start()?;
            let flash = fixture.request(&running, FLASH).await?;
            assert_eq!(
                flash.status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                flash.body
            );
            let argon = fixture.request(&running, ARGON).await?;
            argon.assert_ok();
            fixture.assert_wire_models(&[ARGON]);
            let rows = fixture.recorded_logs(2).await?;
            let rejected = decision(log_for(&rows, &flash));
            assert!(rejected.get("quota_attribution").is_none());
            assert_eq!(
                rejected["candidates"][0]["target"], FLASH,
                "wildcard was not materialized: {rejected}"
            );
            let row = log_for(&rows, &argon);
            assert_attribution(row, "Argon", ARGON, ARGON);
            assert_payload_disabled(row);
            assert_eq!(decision(row)["candidates"][0]["target"], ARGON);
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn encoded_wire_model_controls_gate_and_payload_disabled_json_attribution() -> anyhow::Result<()> {
    wire_model_contract(false)
}

#[test]
fn compat_wire_model_controls_gate_and_payload_disabled_json_attribution() -> anyhow::Result<()> {
    wire_model_contract(true)
}

fn wire_model_contract(anthropic: bool) -> anyhow::Result<()> {
    for exhausted in [false, true] {
        let mut fixture = Fixture::new(
            REWRITE_VENDOR,
            vec![route(
                "alias-with-configured-fallback",
                "priority",
                &[FLASH_ALIAS, ARGON],
            )],
            vec![
                bucket(
                    "Flash",
                    &[FLASH_HIGH],
                    if exhausted { 100.0 } else { 10.0 },
                    20.0,
                ),
                bucket("Argon", &[ARGON], 15.0, 25.0),
            ],
        )?;
        // The IR/backend alias has no binding. Only the exact post-encode JSON
        // wire tier is known; a guessed prefix/family/alias gate cannot pass.
        assert!(
            fixture
                .gateway
                .quota_registry
                .model_snapshot(PROVIDER, FLASH_ALIAS)
                .is_none()
        );
        runtime()?.block_on(async {
            let running = fixture.start()?;
            let response = fixture
                .request_protocol(&running, "alias-with-configured-fallback", anthropic)
                .await?;
            assert_eq!(response.status, StatusCode::OK, "{}", response.body);
            if anthropic {
                assert_eq!(response.body["type"], "message");
                assert_eq!(response.body["content"][0]["text"], "local reply");
            } else {
                response.assert_ok();
            }
            let (label, backend, wire_model) = if exhausted {
                ("Argon", ARGON, ARGON)
            } else {
                ("Flash", FLASH_ALIAS, FLASH_HIGH)
            };
            fixture.assert_wire_models(&[wire_model]);
            let rows = fixture.recorded_logs(1).await?;
            let row = log_for(&rows, &response);
            assert_payload_disabled(row);
            assert_attribution(row, label, backend, wire_model);
            assert_eq!(row.input_tokens, 2);
            assert_eq!(row.output_tokens, 1);
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn owner_scope_replaced_during_preparation_never_dispatches_or_cross_attributes()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new(
        REWRITE_VENDOR,
        vec![route(
            "owner-switch-route",
            "priority",
            &[OWNER_SWITCH_ALIAS],
        )],
        vec![bucket("Flash", &[FLASH_HIGH], 10.0, 20.0)],
    )?;
    let prepared_owner = fixture
        .gateway
        .quota_registry
        .bucket_refresh_fence(PROVIDER);
    assert_eq!(
        fixture
            .gateway
            .quota_registry
            .model_snapshot(PROVIDER, FLASH_HIGH)
            .unwrap()
            .scope_id,
        scope_id(),
    );
    runtime()?.block_on(async {
        let running = fixture.start()?;
        let response = fixture.request(&running, "owner-switch-route").await?;
        assert_eq!(
            response.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            response.body
        );
        fixture.assert_wire_models(&[]);
        let replacement = fixture
            .gateway
            .quota_registry
            .model_snapshot(PROVIDER, FLASH_HIGH)
            .expect("pre-request hook installed the replacement owner");
        assert_eq!(replacement.scope_id, replacement_scope_id());
        assert_ne!(replacement.scope_id, scope_id());
        assert!(
            fixture
                .gateway
                .quota_registry
                .is_model_schedulable(PROVIDER, FLASH_HIGH),
            "a healthy replacement owner must not be mistaken for quota exhaustion"
        );
        assert!(
            !fixture
                .gateway
                .quota_registry
                .bucket_fence_is_current(PROVIDER, &prepared_owner),
            "the prepared owner generation must be invalidated"
        );
        let rows = fixture.recorded_logs(1).await?;
        let row = log_for(&rows, &response);
        assert_eq!(row.client_status_code, Some(503));
        assert!(row.upstream_status_code.is_none());
        assert_eq!(row.input_tokens, 0);
        assert_eq!(row.output_tokens, 0);
        assert!(
            decision(row).get("quota_attribution").is_none(),
            "an unsubmitted request must not be charged to either owner"
        );
        Ok(())
    })
}

#[test]
fn unknown_flash_429_is_model_scoped_and_argon_still_reaches_http() -> anyhow::Result<()> {
    let mut fixture = Fixture::new(
        "custom",
        vec![
            route("flash-route", "priority", &[FLASH]),
            route("argon-route", "priority", &[ARGON]),
        ],
        vec![
            bucket("Flash", &[FLASH], 10.0, 20.0),
            bucket("Argon", &[ARGON], 15.0, 25.0),
        ],
    )?;
    fixture.upstream.reject_flash = true;
    runtime()?.block_on(async {
        let running = fixture.start()?;
        let rejected = fixture.request(&running, "flash-route").await?;
        assert_eq!(
            rejected.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            rejected.body
        );
        let scheduling = fixture
            .gateway
            .quota_registry
            .model_scheduling(PROVIDER, FLASH);
        assert_eq!(scheduling.status, ProviderSchedulingStatus::QuotaExhausted);
        assert_eq!(scheduling.reason.as_deref(), Some("model_cooldown"));
        assert!(
            scheduling.blocking_tiers.is_empty(),
            "unknown 429 fabricated an exhausted allowance"
        );
        assert!(scheduling.next_check_at.is_some());
        assert!(fixture.gateway.quota_registry.is_schedulable(PROVIDER));
        assert!(
            fixture
                .gateway
                .quota_registry
                .is_model_schedulable(PROVIDER, ARGON)
        );
        assert_eq!(
            fixture
                .gateway
                .quota_registry
                .model_scheduling(PROVIDER, ARGON)
                .status,
            ProviderSchedulingStatus::Eligible
        );
        let cooled = fixture.request(&running, "flash-route").await?;
        assert_eq!(
            cooled.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            cooled.body
        );
        let argon = fixture.request(&running, "argon-route").await?;
        argon.assert_ok();
        fixture.assert_wire_models(&[FLASH, ARGON]);
        let rows = fixture.recorded_logs(3).await?;
        assert_attribution(log_for(&rows, &rejected), "Flash", FLASH, FLASH);
        assert!(
            decision(log_for(&rows, &cooled))
                .get("quota_attribution")
                .is_none()
        );
        let argon_row = log_for(&rows, &argon);
        assert_attribution(argon_row, "Argon", ARGON, ARGON);
        assert_payload_disabled(argon_row);
        let flash_bucket = fixture
            .gateway
            .quota_registry
            .model_snapshot(PROVIDER, FLASH)
            .unwrap();
        assert_eq!(flash_bucket.tiers[0].used_percent, 10.0);
        assert_eq!(flash_bucket.tiers[1].used_percent, 20.0);
        Ok(())
    })
}
