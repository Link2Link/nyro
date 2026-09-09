//! End-to-end payload retention through the real collector flush path.
//! Versioned `unknown` outcomes must keep their bounded evidence even when
//! payload recording is disabled; the version-0 default and confirmed
//! completions stay cleared.

use std::sync::Arc;

use nyro_core::logging::diagnostics::{LogDiagnostic, OUTCOME_VERSION};
use nyro_core::logging::{ENABLE_PAYLOAD_KEY, LogEntry, enqueue_log, run_collector};
use nyro_core::protocol::ir::Usage;
use nyro_core::storage::{SqliteStorage, Storage};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::sync::mpsc;

fn entry(diagnostic: LogDiagnostic) -> LogEntry {
    let payload = Some("{\"evidence\":\"kept-or-cleared\"}".to_string());
    LogEntry {
        diagnostic,
        api_key_id: None,
        api_key_name: None,
        created_at: chrono::Utc::now().timestamp_millis(),
        client_protocol: "openai-compatible/chat-completions/v1".into(),
        upstream_protocol: "openai-compatible/chat-completions/v1".into(),
        provider_id: "p".into(),
        provider_name: "provider".into(),
        model_id: None,
        model_name: None,
        upstream_url: None,
        client_model: "m".into(),
        upstream_model: "m".into(),
        reasoning_effort: None,
        route_decision: None,
        method: Some("POST".into()),
        path: Some("/v1/chat/completions".into()),
        client_request_headers: payload.clone(),
        client_request_body: payload.clone(),
        client_response_headers: payload.clone(),
        client_response_body: payload.clone(),
        upstream_request_headers: payload.clone(),
        upstream_request_body: payload.clone(),
        upstream_response_headers: payload.clone(),
        upstream_response_body: payload,
        upstream_status_code: Some(200),
        client_status_code: 200,
        performance: Default::default(),
        latency_total_ms: 100,
        latency_upstream_ms: Some(90),
        usage: Usage::default(),
        is_stream: true,
        stream_chunks_count: 3,
        stream_first_chunk_ms: Some(50),
        // Per-model switch off: only the forced outcomes may retain evidence.
        enable_payload: Some(false),
    }
}

fn diagnostic(version: i32, outcome: &str) -> LogDiagnostic {
    LogDiagnostic {
        outcome_version: version,
        attempt_outcome: outcome.to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn versioned_unknown_keeps_payload_through_collector_flush() -> anyhow::Result<()> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    // Global payload recording off: retention must come from forced outcomes.
    storage.settings().set(ENABLE_PAYLOAD_KEY, "false").await?;

    let ambiguous = entry(diagnostic(OUTCOME_VERSION, "unknown"));
    let legacy = entry(diagnostic(0, "unknown"));
    let completed = entry(diagnostic(OUTCOME_VERSION, "completed"));
    let ids = [
        ambiguous.diagnostic.log_id.clone(),
        legacy.diagnostic.log_id.clone(),
        completed.diagnostic.log_id.clone(),
    ];

    let (tx, rx) = mpsc::channel(8);
    let collector = tokio::spawn(run_collector(
        rx,
        Arc::new(storage.clone()) as nyro_core::storage::DynStorage,
    ));
    enqueue_log(&tx, ambiguous);
    enqueue_log(&tx, legacy);
    enqueue_log(&tx, completed);
    drop(tx);
    collector.await?;

    let storage = SqliteStorage::from_pool(storage.pool().clone());
    let retained = |log: &nyro_core::db::models::RequestLog| {
        log.upstream_response_body.is_some() && log.client_request_body.is_some()
    };
    let ambiguous = storage
        .logs()
        .find_by_id(&ids[0])
        .await?
        .expect("ambiguous row");
    assert!(
        retained(&ambiguous),
        "versioned unknown must retain payload evidence: {:?}",
        ambiguous.attempt_outcome
    );
    assert_eq!(ambiguous.attempt_outcome, "unknown");
    assert_eq!(ambiguous.outcome_version, OUTCOME_VERSION);

    let legacy = storage
        .logs()
        .find_by_id(&ids[1])
        .await?
        .expect("legacy row");
    assert!(
        !retained(&legacy),
        "version-0 default unknown stays cleared"
    );
    let completed = storage
        .logs()
        .find_by_id(&ids[2])
        .await?
        .expect("completed row");
    assert!(!retained(&completed), "confirmed completion stays cleared");
    Ok(())
}
