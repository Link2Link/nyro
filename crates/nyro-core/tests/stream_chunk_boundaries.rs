//! Exercise byte-split upstream bodies through the real dispatcher. HTTP chunks
//! are deliberately smaller than UTF-8 code points and SSE data lines.
use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{Router, body::Body, http::HeaderMap, response::Response, routing::post};
use bytes::Bytes;
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider, UpsertOAuthCredential},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};

async fn run_fixture(channel: &str, stream: bool, wire: &str) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let chunks: Vec<_> = wire
        .as_bytes()
        .chunks(1)
        .map(Bytes::copy_from_slice)
        .collect();
    let app = Router::new().route(
        "/*path",
        post(move || {
            let chunks = chunks.clone();
            async move {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(futures::stream::iter(
                        chunks.into_iter().map(Ok::<_, Infallible>),
                    )))
                    .unwrap()
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let result = async {
        let dir = tempfile::tempdir()?;
        let config = GatewayConfig {
            data_dir: dir.path().into(),
            config_poll_interval: Duration::ZERO,
            ..Default::default()
        };
        let storage = Arc::new(SqliteStorage::from_config(&config).await?);
        let (gw, _logs) = Gateway::from_storage(config, storage).await?;
        let subscription = matches!(channel, "antigravity" | "gemini-cli");
        let provider = gw
            .admin()
            .create_provider(CreateProvider {
                keys: vec![],
                name: "split-fixture".into(),
                vendor: Some("google".into()),
                protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string(),
                base_url: url,
                protocol_mode: "fixed".into(),
                protocol_endpoints: vec![],
                preset_key: None,
                channel: Some(channel.into()),
                models_source: None,
                static_models: None,
                api_key: "fixture-key".into(),
                auth_mode: if subscription { "oauth" } else { "apikey" }.into(),
                use_proxy: false,
                fast_mode: false,
            })
            .await?;
        if subscription {
            // Unknown driver preserves the local fixture URL instead of binding
            // to real Google hosts; only synthetic credentials are ever used.
            gw.storage
                .oauth_credentials()
                .upsert(
                    &provider.id,
                    UpsertOAuthCredential {
                        driver_key: "google-fixture".into(),
                        scheme: "oauth_auth_code_pkce".into(),
                        access_token: "fixture".into(),
                        refresh_token: None,
                        expires_at: Some("2099-01-01T00:00:00Z".into()),
                        resource_url: None,
                        subject_id: None,
                        scopes: None,
                        meta: Some(r#"{"project_id":"fixture-project"}"#.into()),
                    },
                )
                .await?;
        }
        gw.admin()
            .create_model(CreateModel {
                name: "split-fixture".into(),
                balance: Some("priority".into()),
                target_provider: provider.id,
                target_model: "gemini-fixture".into(),
                targets: vec![],
                enable_auth: Some(false),
                enable_payload: Some(false),
                force_max_reasoning: None,
                vision_shim: None,
            })
            .await?;
        let body = json!({"model":"split-fixture","stream":stream,"messages":[{"role":"user","content":"read the file"}]});
        let ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        let ir = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            RawEnvelope::new(
                Some(body),
                Default::default(),
                "POST",
                "/v1/chat/completions",
            ),
            ir,
            ingress,
            RequestContext::new(ingress, Duration::from_secs(10)),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        let text = String::from_utf8(bytes.to_vec())?;
        anyhow::ensure!(status.is_success(), "{status}: {text}");
        Ok::<_, anyhow::Error>(text)
    };
    let result = tokio::time::timeout(Duration::from_secs(10), result).await;
    server.abort();
    result?
}

fn fixture(channel: &str, newline: &str, eof_tail: bool) -> String {
    let events = [
        json!({"candidates":[{"content":{"parts":[{"text":"你好🌍"}]}}],"modelVersion":"gemini-fixture"}),
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"file":"目录/🌍.txt"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}}),
    ];
    let mut wire = format!(": heartbeat{newline}{newline}");
    for event in events {
        let event = if channel == "default" {
            event
        } else {
            json!({"response":event,"traceId":"fixture"})
        };
        wire.push_str(&format!("data: {event}{newline}{newline}"));
    }
    if eof_tail {
        wire.truncate(wire.trim_end_matches(['\r', '\n']).len());
    }
    wire
}

fn assert_stream_output(text: &str) {
    assert!(!text.contains('�'), "{text}");
    let events: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let content: String = events
        .iter()
        .filter_map(|event| event["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "你好🌍", "{text}");
    let args: String = events
        .iter()
        .filter_map(|event| {
            event["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
        })
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&args).unwrap(),
        json!({"file":"目录/🌍.txt"})
    );
    assert!(
        events
            .iter()
            .any(|event| event["usage"]["prompt_tokens"] == 7),
        "{text}"
    );
    assert!(text.contains("data: [DONE]"), "{text}");
    assert!(!text.contains("NYRO_"), "{text}");
}

#[tokio::test]
async fn split_google_subscription_stream_survives_dispatch_probe_and_forwarding()
-> anyhow::Result<()> {
    for channel in ["antigravity", "gemini-cli"] {
        for (newline, eof_tail) in [("\n", false), ("\r\n", true), ("\r", false)] {
            let text = run_fixture(channel, true, &fixture(channel, newline, eof_tail)).await?;
            assert_stream_output(&text);
        }
    }
    Ok(())
}

#[tokio::test]
async fn split_utf8_survives_native_ir_without_vendor_hook() -> anyhow::Result<()> {
    let text = run_fixture("default", true, &fixture("default", "\n", false)).await?;
    assert_stream_output(&text);
    Ok(())
}

#[tokio::test]
async fn split_google_subscription_stream_aggregates_for_non_stream_client() -> anyhow::Result<()> {
    for (newline, eof_tail) in [("\n", false), ("\r\n", true), ("\r", false)] {
        let text = run_fixture(
            "antigravity",
            false,
            &fixture("antigravity", newline, eof_tail),
        )
        .await?;
        let body: Value = serde_json::from_str(&text)?;
        assert_eq!(body["choices"][0]["message"]["content"], "你好🌍");
        assert_eq!(
            serde_json::from_str::<Value>(
                body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )?,
            json!({"file":"目录/🌍.txt"})
        );
        assert_eq!(body["usage"]["prompt_tokens"], 7);
        assert_eq!(body["usage"]["completion_tokens"], 3);
    }
    Ok(())
}
