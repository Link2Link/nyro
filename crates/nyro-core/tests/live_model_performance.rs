//! Real upstream -> dispatcher -> finalized log -> SQL -> performance Admin API.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::to_bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use nyro_core::admin::SetModelRating;
use nyro_core::db::models::{CreateModel, CreateProvider};
use nyro_core::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use nyro_core::protocol::ir::{AiRequest, RawEnvelope, ReasoningEffort};
use nyro_core::proxy::context::RequestContext;
use nyro_core::proxy::dispatcher::dispatch_pipeline;
use nyro_core::storage::SqliteStorage;
use nyro_core::{Gateway, config::GatewayConfig};
use serde_json::{Value, json};

async fn upstream(Json(request): Json<Value>) -> axum::response::Response {
    // Nonzero timing ensures an actual measurable rate without synthetic metrics.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let model = request["model"].as_str().unwrap_or("model");
    let limited = model == "limited";
    if request["stream"].as_bool().unwrap_or(false) {
        let reason = if limited { "length" } else { "stop" };
        let text = format!(
            "data: {{\"id\":\"r\",\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hello\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"id\":\"r\",\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{reason}\"}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":10,\"total_tokens\":15}}}}\n\ndata: [DONE]\n\n"
        );
        ([("content-type", "text/event-stream")], text).into_response()
    } else {
        Json(json!({"id":"r","model":model,"choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":if limited {"length"} else {"stop"}}],"usage":{"prompt_tokens":5,"completion_tokens":10,"total_tokens":15}})).into_response()
    }
}

#[tokio::test]
async fn live_completion_metadata_is_diagnostic_and_performance_matches_usage() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let pool = storage.pool().clone();
    let (gw, mut logs) = Gateway::from_storage(config, Arc::new(storage)).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", post(upstream)),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await
    });
    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            name: "live-performance".to_string(),
            vendor: None,
            protocol: "openai-compatible".to_string(),
            base_url: format!("http://{address}/v1"),
            protocol_mode: "fixed".to_string(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "test".to_string(),
            auth_mode: "apikey".to_string(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    // One specific entry matches the exact model; a non-boundary prefix must not.
    gw.admin()
        .set_model_rating("", SetModelRating { score: 80 })
        .await
        .unwrap_err();
    gw.admin()
        .set_model_rating("buffered", SetModelRating { score: 95 })
        .await?;
    gw.admin()
        .set_model_rating("limited", SetModelRating { score: 60 })
        .await?;
    gw.admin()
        .set_model_rating("stream", SetModelRating { score: 40 })
        .await?;
    for model in ["buffered", "streamed", "limited", "abandoned"] {
        gw.admin()
            .create_model(CreateModel {
                name: model.to_string(),
                balance: None,
                target_provider: provider.id.clone(),
                target_model: model.to_string(),
                targets: vec![],
                enable_auth: Some(false),
                enable_payload: None,
                force_max_reasoning: None,
                vision_shim: None,
            })
            .await?;
        let stream = model != "buffered";
        let value = json!({"model":model,"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high","stream":stream});
        let envelope =
            RawEnvelope::new(Some(value), HashMap::new(), "POST", "/v1/chat/completions");
        let mut request = AiRequest::new(model, vec![]);
        request.stream.enabled = stream;
        request.reasoning.enabled = true;
        request.reasoning.effort = Some(ReasoningEffort::High);
        let response = dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                Duration::from_secs(5),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        if model == "abandoned" {
            drop(response);
        } else {
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            assert!(!bytes.is_empty());
        }
        let entry = tokio::time::timeout(Duration::from_secs(3), logs.recv())
            .await?
            .expect("finalized log");
        assert_eq!(entry.upstream_model, model);
        assert_eq!(entry.performance.effort_tier.as_deref(), Some("high"));
        if matches!(model, "buffered" | "streamed") {
            assert_eq!(
                entry.performance.completion, "completed",
                "{model}: {:?}",
                entry.performance
            );
            assert!(entry.performance.upstream_duration_ms.unwrap_or(0) >= 15);
        } else {
            assert_ne!(entry.performance.completion, "completed", "{model}");
        }
        gw.storage.logs().append_batch(vec![entry]).await?;
        assert!(logs.try_recv().is_err(), "one finalized row per attempt");
    }
    let stats = gw.admin().get_model_performance(Some(&provider.id)).await?;
    // "stream" does not match "streamed" at a segment boundary, so streamed and
    // abandoned never appear.
    assert_eq!(stats.models.len(), 2);
    let by_prefix: HashMap<_, _> = stats
        .models
        .iter()
        .map(|item| (item.model_prefix.as_str(), item))
        .collect();
    let buffered = by_prefix["buffered"];
    assert_eq!(buffered.score, 95);
    assert_eq!(buffered.variants.len(), 1);
    assert_eq!(buffered.variants[0].upstream_model, "buffered");
    let limited = by_prefix["limited"];
    assert_eq!(limited.score, 60);
    assert_eq!(limited.variants.len(), 1);
    assert_eq!(limited.variants[0].upstream_model, "limited");
    for item in &stats.models {
        let usage = gw
            .storage
            .logs()
            .model_usage_stats(&provider.id, &item.variants[0].upstream_model)
            .await?;
        assert_eq!(
            item.mixed.average_tps, usage.average_tps,
            "{}",
            item.variants[0].upstream_model
        );
        // Each model has one raw log: limited counts whenever its legacy
        // tokens and timings are usable, independently of lifecycle completion.
        assert_eq!(
            item.mixed.valid_tps_count,
            i64::from(usage.average_tps.is_some())
        );
        assert_eq!(item.mixed.selected_request_count, usage.recent_sample_count);
        assert_eq!(item.mixed.selected_request_count, 1);
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_logs WHERE request_completion = 'completed'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 2);
    let _ = stop_tx.send(());
    server.await??;
    Ok(())
}
