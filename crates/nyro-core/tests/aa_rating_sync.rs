use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use nyro_core::Gateway;
use nyro_core::admin::{SetAaModelAlias, SetModelRating, UpdateAaSyncConfig};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::*;
use nyro_core::db::{EffectiveRatingSource, normalize_model_key};
use nyro_core::storage::SqliteStorage;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct PageQuery {
    page: Option<usize>,
}

async fn new_gateway() -> anyhow::Result<(tempfile::TempDir, Gateway)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage)).await?;
    Ok((dir, gw))
}

#[tokio::test]
async fn aa_sync_fetches_paginated_models_consolidates_highest_effort_and_powers_performance()
-> anyhow::Result<()> {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let app = Router::new().route(
        "/api/v2/language/models/free",
        get(move |headers: HeaderMap, Query(q): Query<PageQuery>| {
            let hits = hits_clone.clone();
            async move {
                let key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("");
                if key != "aa_test_secret_1234" {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" })));
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let page = q.page.unwrap_or(1);
                if page == 1 {
                    (
                        StatusCode::OK,
                        Json(json!({
                            "tier": "free",
                            "intelligence_index_version": 5,
                            "pagination": { "page": 1, "page_size": 2, "total_pages": 2, "has_more": true },
                            "data": [
                                {
                                    "slug": "claude-opus-5-5-high",
                                    "name": "Claude Opus 5.5 (High, Default Fallback)",
                                    "release_date": "2026-10-01",
                                    "model_creator": { "name": "Anthropic" },
                                    "evaluations": { "artificial_analysis_intelligence_index": 51.8 }
                                },
                                {
                                    "slug": "claude-opus-5-5",
                                    "name": "Claude Opus 5.5 (Max, Default Fallback)",
                                    "release_date": "2026-10-01",
                                    "model_creator": { "name": "Anthropic" },
                                    "evaluations": { "artificial_analysis_intelligence_index": 59.1 }
                                }
                            ]
                        })),
                    )
                } else {
                    (
                        StatusCode::OK,
                        Json(json!({
                            "tier": "free",
                            "intelligence_index_version": 5,
                            "pagination": { "page": 2, "page_size": 2, "total_pages": 2, "has_more": false },
                            "data": [
                                {
                                    "slug": "claude-fable-5-1-xhigh",
                                    "name": "Claude Fable 5.1 (Xhigh)",
                                    "release_date": "2026-10-02",
                                    "model_creator": { "name": "Anthropic" },
                                    "evaluations": { "artificial_analysis_intelligence_index": 50.0 }
                                },
                                {
                                    "slug": "claude-fable-5-1",
                                    "name": "Claude Fable 5.1 (Max)",
                                    "release_date": "2026-10-02",
                                    "model_creator": { "name": "Anthropic" },
                                    "evaluations": { "artificial_analysis_intelligence_index": 49.2 }
                                },
                                {
                                    "slug": "gpt-6-2-astra",
                                    "name": "GPT-6.2 Astra (Max)",
                                    "release_date": "2026-11-10",
                                    "model_creator": { "name": "OpenAI" },
                                    "evaluations": { "artificial_analysis_intelligence_index": 62.4 }
                                }
                            ]
                        })),
                    )
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });

    let (_dir, gw) = new_gateway().await?;
    let admin = gw.admin();

    // Configure AA sync against local mock server.
    let cfg = admin
        .update_aa_rating_sync_config(UpdateAaSyncConfig {
            enabled: Some(true),
            api_key: Some("aa_test_secret_1234".to_string()),
            sync_interval_hours: Some(24),
            api_base_url: Some(format!("http://{addr}/api/v2/language/models/free")),
        })
        .await?;
    assert!(cfg.enabled);
    assert!(cfg.has_api_key);
    assert_eq!(cfg.api_key_preview.as_deref(), Some("aa_t...1234"));

    // Trigger sync and verify pagination + highest-effort consolidation.
    let overview = admin.trigger_aa_rating_sync().await?;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(overview.status.index_version.as_deref(), Some("5"));
    assert_eq!(overview.status.raw_model_count, 5);
    assert_eq!(overview.status.model_count, 3);
    assert!(overview.status.last_error.is_none());

    let opus = overview
        .models
        .iter()
        .find(|m| m.canonical_key == "claude-opus-5-5")
        .unwrap();
    assert_eq!(opus.score, 59);
    assert_eq!(opus.selected_effort, "max");

    // Fable 5.1 must use Max (49.2 -> 49) even though Xhigh scored 50.0.
    let fable = overview
        .models
        .iter()
        .find(|m| m.canonical_key == "claude-fable-5-1")
        .unwrap();
    assert_eq!(fable.score, 49);
    assert_eq!(fable.selected_effort, "max");

    // Explicit AA alias binding works and rejects unknown keys.
    assert!(
        admin
            .set_aa_alias(
                "custom-astra",
                SetAaModelAlias {
                    aa_canonical_key: "nonexistent".into()
                }
            )
            .await
            .is_err()
    );
    let alias = admin
        .set_aa_alias(
            "My-Custom-Astra",
            SetAaModelAlias {
                aa_canonical_key: "gpt-6-2-astra".into(),
            },
        )
        .await?;
    assert_eq!(alias.model_prefix, "my-custom-astra");

    // Manual override takes precedence over AA auto; AA auto covers other variants.
    let provider = admin
        .create_provider(CreateProvider {
            keys: Vec::new(),
            name: "test-prov".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "k".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: Default::default(),
        })
        .await?;

    admin
        .set_model_rating("claude-opus-5-5", SetModelRating { score: 88 })
        .await?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let make_log = |id: &str, model: &str| {
        let mut diag = nyro_core::logging::diagnostics::LogDiagnostic::default();
        diag.log_id = id.to_string();
        nyro_core::logging::LogEntry {
            diagnostic: diag,
            api_key_id: None,
            api_key_name: None,
            created_at: now_ms,
            client_protocol: "openai-compatible".into(),
            upstream_protocol: "openai-compatible".into(),
            provider_id: provider.id.clone(),
            provider_name: "test-prov".into(),
            provider_key_name: None,
            model_id: None,
            model_name: None,
            upstream_url: None,
            client_model: model.to_string(),
            upstream_model: model.to_string(),
            reasoning_effort: None,
            route_decision: None,
            method: Some("POST".into()),
            path: Some("/v1/chat/completions".into()),
            client_request_headers: None,
            client_request_body: None,
            client_response_headers: None,
            client_response_body: None,
            upstream_request_headers: None,
            upstream_request_body: None,
            upstream_response_headers: None,
            upstream_response_body: None,
            upstream_status_code: Some(200),
            client_status_code: 200,
            performance: nyro_core::performance::PerformanceMetadata::default(),
            latency_total_ms: 1000,
            latency_upstream_ms: Some(1000),
            usage: nyro_core::protocol::ir::Usage {
                prompt_tokens: 50,
                completion_tokens: 100,
                ..Default::default()
            },
            is_stream: false,
            stream_chunks_count: 0,
            stream_first_chunk_ms: None,
            enable_payload: None,
        }
    };

    gw.storage
        .logs()
        .append_batch(vec![
            make_log("l1", "Claude Opus 5.5"),
            make_log("l2", "gpt-6.2-astra"),
            make_log("l3", "my-custom-astra-v1"),
            make_log("l4", "codex-auto-review"),
        ])
        .await?;

    let perf = admin.get_model_performance(Some(&provider.id)).await?;
    assert_eq!(perf.models.len(), 3);

    let p_opus = perf
        .models
        .iter()
        .find(|m| m.model_prefix == "claude-opus-5-5")
        .unwrap();
    assert_eq!(p_opus.score, 88);
    assert_eq!(p_opus.score_source, Some(EffectiveRatingSource::Manual));

    let p_astra = perf
        .models
        .iter()
        .find(|m| m.model_prefix == "gpt-6-2-astra")
        .unwrap();
    assert_eq!(p_astra.score, 62);
    assert_eq!(p_astra.score_source, Some(EffectiveRatingSource::AaAuto));

    let p_alias = perf
        .models
        .iter()
        .find(|m| m.model_prefix == "my-custom-astra")
        .unwrap();
    assert_eq!(p_alias.score, 62);
    assert_eq!(p_alias.score_source, Some(EffectiveRatingSource::AaAlias));

    // Export config must NOT leak internal aa_rating_sync.* settings or API keys.
    let exported = admin.export_config().await?;
    assert!(
        !exported
            .settings
            .iter()
            .any(|(k, _)| k.starts_with("aa_rating_sync.")),
        "internal aa_rating_sync settings must be excluded from export"
    );

    // Cleanup alias works.
    admin.delete_aa_alias("my-custom-astra").await?;
    assert!(admin.list_aa_aliases().await?.is_empty());
    assert_eq!(normalize_model_key("Claude Opus 5.5"), "claude-opus-5-5");

    let _ = tx.send(());
    let _ = server.await;
    Ok(())
}
