//! End-to-end coverage for the Anthropic Claude Code OAuth channel through the
//! real dispatcher: what reaches the upstream (CLI identity, cloaked body,
//! signed `cch`, MCP tool aliases) and what reaches the client (original tool
//! names). The upstream is a loopback fixture; credentials are synthetic.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider, UpsertOAuthCredential},
    protocol::{ids::*, ir::RawEnvelope},
    provider::anthropic::claude_code::cch,
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MODEL: &str = "cc-fixture";
/// A model with a mid-conversation `role: system` slot (not in the legacy list).
const UPSTREAM_MODEL: &str = "claude-opus-5";
const ACCOUNT_UUID: &str = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
/// The fixture answers this model with Anthropic's `credits_required` 429.
const CREDITS_MODEL: &str = "claude-fable-5-1";

#[derive(Debug, Clone)]
struct Captured {
    line: String,
    headers: Vec<(String, String)>,
    raw_body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    fn body(&self) -> Value {
        serde_json::from_slice(&self.raw_body).unwrap()
    }
}

/// Anthropic fixture: answers with a `tool_use` that calls the first declared
/// tool by whatever (aliased) name the request used.
async fn start_upstream() -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let sink = sink.clone();
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let body: Value = serde_json::from_slice(&request.raw_body).unwrap_or(Value::Null);
                // Claude subscription usage (`/api/oauth/usage`, as Claude Code reads it).
                if request.line.starts_with("GET /api/oauth/usage ") {
                    let usage = json!({
                        "five_hour": {"utilization": 42.0, "resets_at": "2099-01-01T05:00:00Z"},
                        "seven_day": {"utilization": 18.0, "resets_at": "2099-01-07T00:00:00Z"},
                        "seven_day_sonnet": {"utilization": 6.0, "resets_at": "2099-01-07T00:00:00Z"},
                        "seven_day_opus": null,
                        "limits": [{"kind": "weekly_scoped", "group": "weekly", "percent": 33,
                            "resets_at": "2099-01-07T00:00:00Z", "severity": "normal", "is_active": true,
                            "scope": {"model": {"display_name": "Fable"}}}]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{usage}",
                        usage.len()
                    );
                    sink.lock().unwrap().push(request);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                    return;
                }
                // OAuth profile: the plan behind the usage card badge.
                if request.line.starts_with("GET /api/oauth/profile ") {
                    let profile = json!({
                        "account": {"uuid": ACCOUNT_UUID, "email": "user@example.com"},
                        "organization": {"uuid": "org-1", "organization_type": "claude_max",
                            "rate_limit_tier": "default_claude_max_20x"}
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{profile}",
                        profile.len()
                    );
                    sink.lock().unwrap().push(request);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                    return;
                }
                // A model this plan only runs on usage credits (Anthropic's reply).
                if body["model"] == CREDITS_MODEL {
                    let refusal = json!({"type": "error", "error": {
                        "type": "rate_limit_error",
                        "message": "Usage credits are required for this model.",
                        "details": {"error_code": "credits_required", "can_user_purchase_credits": true,
                            "has_chargeable_saved_payment_method": false,
                            "disabled_reason": "org_level_disabled", "exhausted_included_allowance": false}
                    }})
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{refusal}",
                        refusal.len()
                    );
                    sink.lock().unwrap().push(request);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                    return;
                }
                sink.lock().unwrap().push(request);
                let tool = body["tools"][0]["name"]
                    .as_str()
                    .unwrap_or("none")
                    .to_string();
                let stream = body["stream"].as_bool().unwrap_or(false);
                let response = if stream {
                    let events = [
                        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":UPSTREAM_MODEL,"content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":tool,"input":{}}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
                        json!({"type":"message_stop"}),
                    ];
                    let sse: String = events
                        .iter()
                        .map(|event| {
                            format!(
                                "event: {}\ndata: {event}\n\n",
                                event["type"].as_str().unwrap()
                            )
                        })
                        .collect();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                } else {
                    let message = json!({
                        "id":"msg_1","type":"message","role":"assistant","model":UPSTREAM_MODEL,
                        "content":[{"type":"tool_use","id":"toolu_1","name":tool,"input":{"cmd":"ls"}}],
                        "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":3}
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
                        message.len()
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, captured)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> anyhow::Result<Captured> {
    let mut input = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        if let Some(offset) = input.windows(4).position(|w| w == b"\r\n\r\n") {
            break offset + 4;
        }
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "request ended before headers");
        input.extend_from_slice(&buffer[..count]);
    };
    let head = std::str::from_utf8(&input[..header_end])?.to_string();
    let mut lines = head.lines();
    let line = lines.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while input.len() < header_end + length {
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "request ended before body");
        input.extend_from_slice(&buffer[..count]);
    }
    Ok(Captured {
        line,
        headers,
        raw_body: input[header_end..header_end + length].to_vec(),
    })
}

async fn setup(upstream: &str) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await.unwrap());
    let (gw, _logs) = Gateway::from_storage(config, storage).await.unwrap();
    std::mem::forget(_logs);
    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            keys: vec![],
            name: "claude-code-fixture".into(),
            vendor: Some("anthropic".into()),
            protocol: "anthropic-messages".into(),
            base_url: upstream.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: Some("anthropic".into()),
            channel: Some("claude-code".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    // `resource_url` points the Claude Code runtime at the loopback fixture.
    gw.storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "claude-code".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "sk-ant-oat01-fixture".into(),
                refresh_token: None,
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                resource_url: Some(upstream.into()),
                subject_id: None,
                scopes: None,
                meta: Some(json!({"account": {"uuid": ACCOUNT_UUID}}).to_string()),
            },
        )
        .await
        .unwrap();
    gw.admin()
        .create_model(CreateModel {
            name: MODEL.into(),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: UPSTREAM_MODEL.into(),
            targets: vec![],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await
        .unwrap();
    (dir, gw)
}

async fn dispatch(
    gw: &Gateway,
    ingress: ProtocolId,
    path: &str,
    headers: HeaderMap,
    body: Value,
) -> (u16, String) {
    let request = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())
        .unwrap();
    let ctx = RequestContext::without_total_timeout(ingress);
    let envelope_headers = headers
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let response = dispatch_pipeline(
        gw.clone(),
        headers,
        RawEnvelope::new(Some(body), envelope_headers, "POST", path),
        request,
        ingress,
        ctx,
    )
    .await;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The single inference call. The gateway's background usage monitor may
/// also read `/api/oauth/*` on this fixture; those calls are not inference.
fn only_request(captured: &Arc<Mutex<Vec<Captured>>>) -> Captured {
    let requests = captured.lock().unwrap();
    let inference: Vec<&Captured> = requests
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/messages"))
        .collect();
    assert_eq!(inference.len(), 1, "exactly one upstream inference call");
    inference[0].clone()
}

#[tokio::test]
async fn third_party_anthropic_client_is_cloaked_signed_and_restored() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_static("opencode/1.0"));
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("fine-grained-tool-streaming-2025-05-14"),
    );
    let body = json!({
        "model": MODEL,
        "max_tokens": 2048,
        "system": "You are OpenCode, an interactive CLI.",
        "temperature": 0.3,
        "tools": [{"name": "bash", "description": "run", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "list the files here"}]
    });
    let (status, wire) = dispatch(
        &gw,
        ANTHROPIC_MESSAGES_2023_06_01,
        "/v1/messages",
        headers,
        body,
    )
    .await;
    assert_eq!(status, 200, "{wire}");

    let request = only_request(&captured);
    assert!(
        request.line.starts_with("POST /v1/messages?beta=true "),
        "{}",
        request.line
    );
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-ant-oat01-fixture")
    );
    assert_eq!(
        request.header("user-agent"),
        Some("claude-cli/2.1.280 (external, cli)")
    );
    assert_eq!(request.header("x-app"), Some("cli"));
    assert_eq!(request.header("x-stainless-lang"), Some("js"));
    assert!(request.header("origin").is_none() && request.header("x-api-key").is_none());
    let betas = request.header("anthropic-beta").unwrap();
    assert!(
        betas.starts_with("claude-code-20250219,oauth-2025-04-20,"),
        "{betas}"
    );
    assert!(
        betas.ends_with(",fine-grained-tool-streaming-2025-05-14"),
        "{betas}"
    );

    let sent = request.body();
    assert_eq!(sent["system"].as_array().unwrap().len(), 2, "{sent}");
    assert!(
        sent["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=2.1.280.")
    );
    assert_eq!(
        sent["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    // Caller prompt left the system slot (non-legacy model → role=system turn).
    assert_eq!(sent["messages"][1]["role"], "system", "{sent:#}");
    assert_eq!(
        sent["messages"][1]["content"][0]["text"],
        "You are OpenCode, an interactive CLI."
    );
    assert!(sent.get("temperature").is_none());
    let alias = sent["tools"][0]["name"].as_str().unwrap();
    assert!(
        alias.starts_with("mcp__") && alias.ends_with("_bash"),
        "{alias}"
    );
    let user_id: Value =
        serde_json::from_str(sent["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(user_id["account_uuid"], ACCOUNT_UUID);
    assert_eq!(
        user_id["session_id"],
        request.header("x-claude-code-session-id").unwrap()
    );
    // The attestation covers exactly the bytes on the wire.
    assert!(
        cch::verify_body(&request.raw_body),
        "cch must sign the final bytes"
    );

    // The client sees its own tool name, never the alias.
    let reply: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(reply["content"][0]["name"], "bash", "{wire}");
    assert!(!wire.contains("mcp__"), "{wire}");
}

async fn provider_id(gw: &Gateway) -> String {
    gw.admin()
        .list_providers()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.channel.as_deref() == Some("claude-code"))
        .unwrap()
        .id
}

#[tokio::test]
async fn subscription_usage_reads_the_claude_code_usage_endpoint() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let id = provider_id(&gw).await;

    let usage = gw.admin().get_provider_usage(&id).await.unwrap();
    assert_eq!(usage.kind, "claude_code_plan");
    let tiers: Vec<(&str, f64)> = usage
        .tiers
        .iter()
        .map(|tier| (tier.name.as_str(), tier.used_percent))
        .collect();
    assert_eq!(
        tiers,
        [
            ("five_hour", 42.0),
            ("weekly_limit", 18.0),
            ("feature:sonnet:weekly_limit", 6.0),
            ("feature:fable:weekly_limit", 33.0),
        ]
    );
    assert_eq!(
        usage.tiers[0].resets_at.as_deref(),
        Some("2099-01-01T05:00:00Z")
    );

    let request = captured.lock().unwrap()[0].clone();
    assert!(
        request.line.starts_with("GET /api/oauth/usage "),
        "{}",
        request.line
    );
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-ant-oat01-fixture")
    );
    assert_eq!(request.header("anthropic-beta"), Some("oauth-2025-04-20"));

    // Plan badge, read the way Claude Code reads it.
    assert_eq!(usage.level.as_deref(), Some("max 20x"));
    let profile = captured.lock().unwrap()[1].clone();
    assert!(
        profile.line.starts_with("GET /api/oauth/profile "),
        "{}",
        profile.line
    );
    // Cached: a second refresh re-reads usage but not the profile.
    gw.admin().get_provider_usage(&id).await.unwrap();
    let profile_calls = captured
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/profile "))
        .count();
    assert_eq!(profile_calls, 1);
}

#[tokio::test]
async fn model_probe_explains_credits_required_instead_of_quota() {
    let (url, _captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let id = provider_id(&gw).await;

    let outcome = gw
        .admin()
        .probe_provider_models_with_prompt(&id, Some(vec![CREDITS_MODEL.to_string()]), None)
        .await
        .unwrap();
    let result = &outcome.results[0];
    assert!(!result.success);
    let error = result.error.as_deref().unwrap();
    assert!(error.starts_with("HTTP 429"), "{error}");
    assert!(error.contains("not a quota window"), "{error}");
    assert!(
        error.contains("does not include it without usage credits"),
        "{error}"
    );
    assert!(error.contains("disabled for the organization"), "{error}");
    assert!(
        !error.contains("Google"),
        "no Google-specific wording on Anthropic: {error}"
    );
}

#[tokio::test]
async fn credits_required_refusal_does_not_trip_provider_health() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let id = provider_id(&gw).await;
    let models = gw.admin().list_models().await.unwrap();
    let model = models.iter().find(|m| m.name == MODEL).unwrap();
    gw.admin()
        .update_model(
            &model.id,
            nyro_core::db::models::UpdateModel {
                target_model: Some(CREDITS_MODEL.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Only refreshes the refusals could cause count; the background monitor
    // may have swept this fixture before the first request.
    let usage_reads_before = captured
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
        .count();

    // More refusals than the circuit's failure threshold (3).
    for attempt in 1..=4 {
        let body = json!({
            "model": MODEL,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (status, wire) = dispatch(
            &gw,
            ANTHROPIC_MESSAGES_2023_06_01,
            "/v1/messages",
            HeaderMap::new(),
            body,
        )
        .await;
        assert_eq!(status, 429, "attempt {attempt}: {wire}");
        assert!(
            wire.contains("credits_required"),
            "upstream reason reaches the client: {wire}"
        );
    }
    let health_key = nyro_core::provider::key_pool::health_key(
        &id,
        None,
        &ANTHROPIC_MESSAGES_2023_06_01.to_string(),
        CREDITS_MODEL,
    );
    assert!(
        gw.health_registry.is_healthy(&health_key),
        "an entitlement refusal must not open the provider circuit"
    );
    // Exactly one upstream call per request, and no usage re-read.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let requests = captured.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.line.starts_with("POST /v1/messages"))
            .count(),
        4
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count(),
        usage_reads_before,
        "credits_required must not trigger a usage refresh"
    );
}

#[tokio::test]
async fn model_discovery_returns_the_built_in_claude_code_catalog() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let id = provider_id(&gw).await;

    // Claude Code's `/model` picker comes from its compiled catalog; nyro
    // mirrors it and asks nothing upstream for discovery.
    for models in [
        gw.admin().get_provider_models(&id).await.unwrap(),
        gw.admin().test_provider_models(&id).await.unwrap(),
    ] {
        assert_eq!(
            &models[..4],
            &[
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "claude-fable-5-1",
                "claude-haiku-5-5"
            ],
            "{models:?}"
        );
        assert!(models.iter().any(|m| m == "claude-opus-4-8"));
        assert!(models.iter().any(|m| m == "claude-3-5-haiku-20241022"));
    }
    assert!(
        captured.lock().unwrap().is_empty(),
        "discovery must not call upstream"
    );
}

#[tokio::test]
async fn legacy_model_gets_caller_system_as_system_reminder() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    // Route the same virtual model to a legacy upstream model id.
    let body = json!({
        "model": MODEL,
        "max_tokens": 256,
        "system": [{"type": "text", "text": "House rules."}],
        "messages": [{"role": "user", "content": "go"}]
    });
    let models = gw.admin().list_models().await.unwrap();
    let model = models.iter().find(|m| m.name == MODEL).unwrap();
    gw.admin()
        .update_model(
            &model.id,
            nyro_core::db::models::UpdateModel {
                target_model: Some("claude-sonnet-4-5-20250929".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (status, wire) = dispatch(
        &gw,
        ANTHROPIC_MESSAGES_2023_06_01,
        "/v1/messages",
        HeaderMap::new(),
        body,
    )
    .await;
    assert_eq!(status, 200, "{wire}");
    let sent = only_request(&captured).body();
    let first = sent["messages"][0]["content"].as_array().unwrap();
    assert!(first[0]["text"].as_str().unwrap().contains("# currentDate"));
    assert_eq!(
        first[1]["text"],
        "<system-reminder>\nHouse rules.\n</system-reminder>"
    );
    assert!(
        sent["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system")
    );
    let betas = only_request(&captured)
        .header("anthropic-beta")
        .unwrap()
        .to_string();
    assert!(!betas.contains("mid-conversation-system"), "{betas}");
}

#[tokio::test]
async fn native_claude_code_keeps_identity_and_streams_through() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let session = "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d";
    let mut headers = HeaderMap::new();
    headers.insert(
        "user-agent",
        HeaderValue::from_static("claude-cli/2.1.285 (external, cli)"),
    );
    headers.insert("x-app", HeaderValue::from_static("cli"));
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static(
            "claude-code-20250219,interleaved-thinking-2025-05-14,effort-2025-11-24",
        ),
    );
    headers.insert(
        "x-claude-code-session-id",
        HeaderValue::from_static(session),
    );
    headers.insert("x-stainless-os", HeaderValue::from_static("Linux"));
    let user_id =
        json!({"device_id": "d".repeat(64), "account_uuid": "", "session_id": session}).to_string();
    let body = json!({
        "model": MODEL,
        "max_tokens": 2048,
        "stream": true,
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.285.abc; cc_entrypoint=cli;"},
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "Native long prompt"}
        ],
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
        "metadata": {"user_id": user_id},
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (status, wire) = dispatch(
        &gw,
        ANTHROPIC_MESSAGES_2023_06_01,
        "/v1/messages",
        headers,
        body,
    )
    .await;
    assert_eq!(status, 200, "{wire}");

    let request = only_request(&captured);
    assert_eq!(
        request.header("user-agent"),
        Some("claude-cli/2.1.285 (external, cli)")
    );
    assert_eq!(request.header("x-stainless-os"), Some("Linux"));
    assert_eq!(request.header("x-claude-code-session-id"), Some(session));
    assert_eq!(
        request.header("anthropic-beta"),
        Some(
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,effort-2025-11-24,extended-cache-ttl-2025-04-11"
        )
    );
    let sent = request.body();
    assert_eq!(sent["system"][2]["text"], "Native long prompt");
    assert_eq!(
        sent["tools"][0]["name"], "Bash",
        "native tool names are untouched"
    );
    let rebound: Value =
        serde_json::from_str(sent["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(rebound["account_uuid"], ACCOUNT_UUID);
    assert_eq!(rebound["session_id"], session);
    assert!(cch::verify_body(&request.raw_body));
    assert!(wire.contains(r#""name":"Bash""#), "{wire}");
}

#[tokio::test]
async fn openai_client_through_ir_gets_original_tool_names_back() {
    let (url, captured) = start_upstream().await;
    let (_dir, gw) = setup(&url).await;
    let body = json!({
        "model": MODEL,
        "stream": true,
        "messages": [
            {"role": "system", "content": "Be concise."},
            {"role": "user", "content": "check the weather"}
        ],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}]
    });
    let (status, wire) = dispatch(
        &gw,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        HeaderMap::new(),
        body,
    )
    .await;
    assert_eq!(status, 200, "{wire}");

    let request = only_request(&captured);
    let sent = request.body();
    let alias = sent["tools"][0]["name"].as_str().unwrap();
    assert!(
        alias.starts_with("mcp__") && alias.ends_with("_get_weather"),
        "{alias}"
    );
    assert_eq!(
        sent["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert!(cch::verify_body(&request.raw_body));
    assert!(wire.contains("\"get_weather\""), "{wire}");
    assert!(!wire.contains("mcp__"), "{wire}");
}
