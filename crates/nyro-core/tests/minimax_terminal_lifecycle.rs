//! Original-wire terminal evidence through the real dispatcher and local upstream.
//! MiniMax Chat may end at clean EOF; this must not normalize the delivered wire,
//! weaken other vendors/protocols, or discard ambiguous evidence with payloads off.

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider},
    logging::{ENABLE_PAYLOAD_KEY, LogEntry, diagnostics::OUTCOME_VERSION, run_collector},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::{SqliteStorage, Storage},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};

// Sanitized replay shape: reasoning delta uses an empty (not null) finish_reason;
// the final chunk reports length and 64 completion tokens, then clean EOF, no DONE.
const REASONING: &str = concat!(
    "data: {\"id\":\"sanitized-replay\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"MiniMax-M2\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning_content\":\"Let me reason about this test.\"},\"finish_reason\":\"\"}]}\n\n"
);
const LENGTH: &str = concat!(
    "data: {\"id\":\"sanitized-replay\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"MiniMax-M2\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"length\"}],",
    "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":64,\"total_tokens\":75}}\n\n"
);
const STOP: &str = "data: {\"id\":\"sanitized-replay\",\"model\":\"MiniMax-M2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n";
const DONE: &str = "data: [DONE]\n\n";
const ANTHROPIC_MISSING_STOP: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"sanitized\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"test\",\"content\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n"
);

fn wire(scenario: &str) -> String {
    match scenario {
        "stop" => format!("{REASONING}{STOP}"),
        "stop-done" => format!("{REASONING}{STOP}{DONE}"),
        "length" => format!("{REASONING}{LENGTH}"),
        "empty-only" => REASONING.into(),
        "partial" => "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n".into(),
        "mixed" => format!("{STOP}data: {{\"choices\":[{{\"index\":1,\"delta\":{{\"reasoning_content\":\"still thinking\"}},\"finish_reason\":\"\"}}]}}\n\n"),
        "mixed-done" => format!("{}{DONE}", wire("mixed")),
        "all-choices" => format!("{REASONING}{STOP}data: {{\"choices\":[{{\"index\":1,\"delta\":{{\"content\":\"second\"}},\"finish_reason\":\"stop\"}}]}}\n\n"),
        "tool-calls" => "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-test\",\"type\":\"function\",\"function\":{\"name\":\"test_tool\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n".into(),
        "unknown-reason" => "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"vendor_future_reason\"}]}\n\n".into(),
        "opaque" => "data: {\"vendor_event\":\"opaque\"}\n\n".into(),
        "malformed-tail" => format!("{STOP}data: {{\"choices\":"),
        other => panic!("unknown replay scenario: {other}"),
    }
}

async fn chat(Json(body): Json<Value>) -> Response {
    assert_eq!(body["stream"], true, "test must exercise upstream SSE");
    let scenario = body["model"].as_str().expect("upstream model");
    ([("content-type", "text/event-stream")], wire(scenario)).into_response()
}

async fn anthropic(Json(body): Json<Value>) -> Response {
    assert_eq!(body["stream"], true);
    (
        [("content-type", "text/event-stream")],
        ANTHROPIC_MISSING_STOP,
    )
        .into_response()
}

struct Fixture {
    _dir: tempfile::TempDir,
    gw: Gateway,
    storage: Arc<SqliteStorage>,
    logs: mpsc::Receiver<LogEntry>,
    url: String,
    server: JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let config = GatewayConfig {
            data_dir: dir.path().into(),
            config_poll_interval: Duration::ZERO,
            ..Default::default()
        };
        let storage = Arc::new(SqliteStorage::from_config(&config).await?);
        let (gw, logs) = Gateway::from_storage(config, storage.clone()).await?;
        storage.settings().set(ENABLE_PAYLOAD_KEY, "false").await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .route("/v1/messages", post(anthropic)),
            )
            .await
            .unwrap();
        });
        Ok(Self {
            _dir: dir,
            gw,
            storage,
            logs,
            url,
            server,
        })
    }

    async fn route(
        &self,
        name: &str,
        vendor: &str,
        protocol: &str,
        scenario: &str,
    ) -> anyhow::Result<()> {
        let provider = self
            .gw
            .admin()
            .create_provider(CreateProvider {
                name: name.into(),
                vendor: Some(vendor.into()),
                protocol: protocol.into(),
                base_url: self.url.clone(),
                protocol_mode: "fixed".into(),
                protocol_endpoints: vec![],
                preset_key: None,
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "local-test-only".into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
                fast_mode: false,
            })
            .await?;
        self.gw
            .admin()
            .create_model(CreateModel {
                name: name.into(),
                balance: None,
                target_provider: provider.id,
                target_model: scenario.into(),
                targets: vec![],
                enable_auth: Some(false),
                enable_payload: Some(false),
                force_max_reasoning: None,
                vision_shim: None,
            })
            .await?;
        Ok(())
    }

    async fn dispatch(
        &mut self,
        name: &str,
        ingress: ProtocolId,
    ) -> anyhow::Result<(String, LogEntry)> {
        let body = json!({"model":name,"max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true});
        let request = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        let ctx = RequestContext::new(ingress, Duration::from_secs(5));
        let request_id = ctx.request_id.clone();
        let response = dispatch_pipeline(
            self.gw.clone(),
            HeaderMap::new(),
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
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert!(
            self.logs.try_recv().is_err(),
            "{name}: enqueue must await body delivery"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        let entry = tokio::time::timeout(Duration::from_secs(3), self.logs.recv())
            .await?
            .expect("dispatcher log");
        assert_eq!(entry.client_status_code, 200, "{name}");
        assert_eq!(entry.upstream_status_code, Some(200), "{name}");
        assert_eq!(
            entry.diagnostic.client_request_id.as_deref(),
            Some(request_id.as_str()),
            "{name}"
        );
        assert_eq!(entry.enable_payload, Some(false));
        assert!(self.logs.try_recv().is_err(), "{name}: exactly one attempt");
        Ok((String::from_utf8(bytes.to_vec())?, entry))
    }
}

fn outcome(entry: &LogEntry, expected: &str) {
    assert_eq!(
        entry.diagnostic.outcome_version, OUTCOME_VERSION,
        "{:?}",
        entry.diagnostic
    );
    assert_eq!(
        entry.diagnostic.attempt_outcome, expected,
        "{:?}",
        entry.diagnostic
    );
    let final_result = entry
        .diagnostic
        .final_result
        .as_ref()
        .expect("final request result");
    assert_eq!(final_result.final_outcome, expected);
    assert_eq!(final_result.attempt_count, 1);
    assert_eq!(
        final_result.final_attempt_id.as_deref(),
        Some(entry.diagnostic.log_id.as_str())
    );
    assert_eq!(entry.performance.completion, expected);
    assert_eq!(
        entry.performance.completed_at.is_some(),
        expected == "completed"
    );
    if expected == "completed" {
        assert!(entry.diagnostic.failure_kind.is_none());
        assert!(entry.diagnostic.error_causes.is_empty());
    }
}

#[tokio::test]
async fn clean_chat_eof_is_scoped_to_minimax_and_preserves_wire() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    for vendor in ["minimax", "custom"] {
        for scenario in ["stop", "all-choices", "tool-calls", "stop-done"] {
            let name = format!("{vendor}-{scenario}");
            fixture
                .route(&name, vendor, "openai-compatible", scenario)
                .await?;
            let (delivered, entry) = fixture
                .dispatch(&name, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
                .await?;
            assert_eq!(
                delivered,
                wire(scenario),
                "{name}: observer must not rewrite SSE or append DONE"
            );
            let upstream: Value =
                serde_json::from_str(entry.upstream_request_body.as_deref().unwrap())?;
            let client: Value =
                serde_json::from_str(entry.client_request_body.as_deref().unwrap())?;
            assert_eq!(
                client["max_tokens"], 64,
                "original client evidence stays intact"
            );
            assert_eq!(
                upstream["max_tokens"],
                if vendor == "minimax" { 262_144 } else { 64 }
            );
            outcome(
                &entry,
                if vendor == "minimax" || scenario == "stop-done" {
                    "completed"
                } else {
                    "unknown"
                },
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn partial_empty_unknown_and_malformed_eof_are_versioned_unknown() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    for vendor in ["minimax", "custom"] {
        for scenario in [
            "empty-only",
            "partial",
            "mixed",
            "mixed-done",
            "unknown-reason",
            "opaque",
            "malformed-tail",
        ] {
            let name = format!("{vendor}-{scenario}");
            fixture
                .route(&name, vendor, "openai-compatible", scenario)
                .await?;
            let (delivered, entry) = fixture
                .dispatch(&name, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
                .await?;
            assert_eq!(
                delivered,
                wire(scenario),
                "{name}: partial output must remain untouched"
            );
            outcome(&entry, "unknown");
        }
    }
    Ok(())
}

#[tokio::test]
async fn sanitized_length_replay_remains_output_limited_without_done() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    for vendor in ["minimax", "custom"] {
        let name = format!("{vendor}-limit");
        fixture
            .route(&name, vendor, "openai-compatible", "length")
            .await?;
        let (delivered, entry) = fixture
            .dispatch(&name, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
            .await?;
        assert_eq!(delivered, wire("length"));
        assert!(!delivered.contains("[DONE]"));
        assert!(delivered.contains("reasoning_content"));
        outcome(&entry, "output_limited");
        assert_eq!(entry.usage.completion_tokens, 64);
        assert_eq!(entry.usage.prompt_tokens, 11);
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("output_limit")
        );
    }
    Ok(())
}

#[tokio::test]
async fn converted_chat_stream_uses_original_eof_not_synthetic_terminal() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    for (vendor, scenario, expected) in [
        ("minimax", "stop", "completed"),
        ("custom", "stop", "unknown"),
        ("minimax", "length", "output_limited"),
        ("minimax", "empty-only", "unknown"),
    ] {
        let name = format!("converted-{vendor}-{scenario}");
        fixture
            .route(&name, vendor, "openai-compatible", scenario)
            .await?;
        let (delivered, entry) = fixture
            .dispatch(&name, ANTHROPIC_MESSAGES_2023_06_01)
            .await?;
        assert!(
            delivered.contains("event: message_start"),
            "{name}: {delivered}"
        );
        assert_eq!(
            entry.client_protocol,
            ANTHROPIC_MESSAGES_2023_06_01.to_string()
        );
        assert_eq!(
            entry.upstream_protocol,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string()
        );
        outcome(&entry, expected);
        if scenario == "stop" {
            assert!(delivered.contains("hello"));
            assert!(delivered.contains("event: message_stop"));
        } else if scenario == "length" {
            // The compatibility converter currently caches the first non-null
            // finish_reason (even empty). Do not require a normal-output rewrite:
            // the diagnostic must still use the original final length evidence.
            assert_eq!(entry.usage.completion_tokens, 64);
        }
    }
    Ok(())
}

#[tokio::test]
async fn minimax_non_chat_egress_still_requires_its_terminal() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture
        .route("minimax-anthropic", "minimax", "anthropic", "missing-stop")
        .await?;
    let (delivered, entry) = fixture
        .dispatch("minimax-anthropic", ANTHROPIC_MESSAGES_2023_06_01)
        .await?;
    assert_eq!(delivered, ANTHROPIC_MISSING_STOP);
    outcome(&entry, "failed");
    assert_eq!(
        entry.diagnostic.failure_kind.as_deref(),
        Some("missing_terminal")
    );
    Ok(())
}

#[tokio::test]
async fn real_dispatcher_unknown_and_limit_evidence_survive_collector_payload_off()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    let (tx, rx) = mpsc::channel(8);
    let mut expected_rows = Vec::new();
    for (vendor, scenario, expected) in [
        ("minimax", "empty-only", "unknown"),
        ("minimax", "opaque", "unknown"),
        ("custom", "stop", "unknown"),
        ("minimax", "length", "output_limited"),
        ("minimax", "stop", "completed"),
    ] {
        let name = format!("retention-{vendor}-{scenario}");
        fixture
            .route(&name, vendor, "openai-compatible", scenario)
            .await?;
        let (delivered, entry) = fixture
            .dispatch(&name, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
            .await?;
        assert_eq!(delivered, wire(scenario));
        outcome(&entry, expected);
        expected_rows.push((entry.diagnostic.log_id.clone(), scenario, expected));
        // Forward the actual dispatcher record unchanged, not a synthetic LogEntry.
        tx.send(entry).await?;
    }
    drop(tx);
    // Closing the finite input forces the production collector's final batch flush.
    run_collector(rx, fixture.storage.clone()).await;
    for (id, scenario, expected) in expected_rows {
        let row = fixture
            .storage
            .logs()
            .find_by_id(&id)
            .await?
            .expect("persisted dispatcher record");
        assert_eq!(row.outcome_version, OUTCOME_VERSION);
        assert_eq!(row.attempt_outcome, expected);
        if expected == "completed" {
            assert!(row.client_request_body.is_none());
            assert!(row.upstream_request_body.is_none());
            assert!(row.upstream_response_body.is_none());
            assert!(row.client_response_body.is_none());
        } else {
            assert!(
                row.client_request_body
                    .as_deref()
                    .is_some_and(|s| s.contains("hi"))
            );
            assert!(row.upstream_request_body.is_some());
            assert_eq!(
                row.upstream_response_body.as_deref(),
                Some(wire(scenario).as_str())
            );
            assert_eq!(
                row.client_response_body.as_deref(),
                Some(wire(scenario).as_str())
            );
        }
    }
    Ok(())
}
