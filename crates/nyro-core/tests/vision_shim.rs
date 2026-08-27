//! Vision-shim integration behaviour against a mocked helper provider.
//!
//! Exercises `vision_shim::apply` end-to-end: an in-memory gateway whose
//! model route enables the shim, a mock OpenAI-compatible helper server, and
//! real IR requests carrying image blocks.

use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use nyro_core::config::GatewayConfig;
use nyro_core::db::models::{Model, Provider};
use nyro_core::protocol::ir::{
    AiRequest, ContentBlock, MediaSource, Message, MessageContent, Role,
};
use nyro_core::storage::{DynStorage, MemoryStorage};
use nyro_core::vision_shim;

// ── Fixtures ──────────────────────────────────────────────────────────────

/// Spawn a mock helper `/chat/completions` server. Returns its base URL
/// (already version-suffixed like a real provider) and the captured request
/// bodies.
async fn spawn_helper(caption: Option<&'static str>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(body);
                match caption {
                    Some(caption) => Json(json!({
                        "choices": [{
                            "message": { "role": "assistant", "content": caption }
                        }],
                        "usage": { "total_tokens": 42 }
                    }))
                    .into_response(),
                    None => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": { "message": "helper exploded" } })),
                    )
                        .into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Base URL without a version suffix: `openai_build_url` keeps the
    // `/v1/chat/completions` path intact, matching the mounted route.
    (format!("http://{addr}"), calls)
}

fn provider_row(id: &str, base_url: &str) -> Provider {
    provider_row_with_protocol(id, base_url, "openai-compatible")
}

/// Production providers created from vendor presets store the canonical
/// endpoint id ("openai-compatible/chat-completions/v1") rather than the bare
/// suite name — the shim must accept both storage forms.
fn provider_row_with_protocol(id: &str, base_url: &str, protocol: &str) -> Provider {
    Provider {
        id: id.to_string(),
        name: "GLM test".to_string(),
        vendor: None,
        protocol: protocol.to_string(),
        base_url: base_url.to_string(),
        protocol_mode: "fixed".to_string(),
        protocol_endpoints: Vec::new(),
        preset_key: None,
        channel: None,
        models_source: None,
        static_models: None,
        api_key: "test-key".to_string(),
        auth_mode: "apikey".to_string(),
        use_proxy: false,
        fast_mode: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn shim_model(provider_id: &str) -> Model {
    Model {
        id: "model-glm".to_string(),
        name: "glm-5.3".to_string(),
        balance: "weighted".to_string(),
        target_provider: provider_id.to_string(),
        target_model: "glm-5.3".to_string(),
        enable_auth: false,
        enable_payload: None,
        vision_shim: Some(r#"{"helper_model":"glm-5.3-flash"}"#.to_string()),
        is_enabled: true,
        created_at: String::new(),
        targets: Vec::new(),
    }
}

fn plain_model(provider_id: &str, name: &str) -> Model {
    Model {
        id: format!("model-{name}"),
        name: name.to_string(),
        balance: "weighted".to_string(),
        target_provider: provider_id.to_string(),
        target_model: name.to_string(),
        enable_auth: false,
        enable_payload: None,
        vision_shim: None,
        is_enabled: true,
        created_at: String::new(),
        targets: Vec::new(),
    }
}

async fn gateway_with(providers: Vec<Provider>, models: Vec<Model>) -> nyro_core::Gateway {
    let storage: DynStorage = Arc::new(MemoryStorage::new(providers, models, Vec::new()));
    let (gw, _log_rx) = nyro_core::Gateway::from_storage(GatewayConfig::default(), storage)
        .await
        .unwrap();
    gw
}

fn image_request(model: &str, data: &str, question: &str) -> AiRequest {
    AiRequest::new(
        model,
        vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: question.to_string(),
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: MediaSource::Base64 {
                        media_type: "image/png".to_string(),
                        data: data.to_string(),
                    },
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

fn user_text(request: &AiRequest) -> String {
    request.messages[0].content.to_text()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn helper_captions_and_replaces_image_blocks() {
    let (base_url, calls) = spawn_helper(Some("a red square reading 'hello'")).await;
    let gw = gateway_with(vec![provider_row("p1", &base_url)], vec![shim_model("p1")]).await;

    let mut request = image_request("glm-5.3", "aGVsbG8=", "what is in the picture?");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();

    assert_eq!(stats.images, 1);
    assert_eq!(stats.captioned, 1);
    assert_eq!(stats.helper_tokens, 42);
    assert_eq!(stats.helper_model, "glm-5.3-flash");

    // The image block became a numbered caption text block.
    let text = user_text(&request);
    assert!(
        text.contains("what is in the picture?"),
        "question text kept: {text}"
    );
    assert!(
        text.contains("[Image 1: a red square reading 'hello']"),
        "caption inlined: {text}"
    );
    assert!(!matches!(
        request.messages[0].content,
        MessageContent::Blocks(ref blocks) if blocks.iter().any(|b| matches!(b, ContentBlock::Image { .. }))
    ));

    // The helper call carried the configured model, the data URL, and a
    // question-aware prompt.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call["model"], "glm-5.3-flash");
    let url = call["messages"][0]["content"][0]["image_url"]["url"]
        .as_str()
        .unwrap();
    assert_eq!(url, "data:image/png;base64,aGVsbG8=");
    let prompt = call["messages"][0]["content"][1]["text"].as_str().unwrap();
    assert!(prompt.contains("what is in the picture?"));
    assert!(prompt.contains("visual information extractor"));
}

#[tokio::test]
async fn repeated_request_hits_the_caption_cache() {
    let (base_url, calls) = spawn_helper(Some("cached caption")).await;
    let gw = gateway_with(vec![provider_row("p2", &base_url)], vec![shim_model("p2")]).await;

    for expected_captioned in [1, 0] {
        let mut request = image_request("glm-5.3", "Y2FjaGU=", "describe it");
        let stats = vision_shim::apply(&gw, &mut request).await.unwrap();
        assert_eq!(stats.captioned, expected_captioned);
        assert!(user_text(&request).contains("[Image 1: cached caption]"));
    }

    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "second request served from cache"
    );
}

#[tokio::test]
async fn helper_failure_degrades_to_placeholder() {
    let (base_url, _calls) = spawn_helper(None).await;
    let gw = gateway_with(vec![provider_row("p3", &base_url)], vec![shim_model("p3")]).await;

    let mut request = image_request("glm-5.3", "ZmFpbA==", "what is this?");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();

    assert_eq!(stats.images, 1);
    assert_eq!(stats.captioned, 0);
    assert_eq!(stats.placeholders, 1);
    let text = user_text(&request);
    assert!(
        text.contains("[Image 1: "),
        "placeholder still numbered: {text}"
    );
    assert!(
        text.contains("could not process this image"),
        "failure noted: {text}"
    );
}

#[tokio::test]
async fn models_without_shim_config_pass_through_untouched() {
    let (base_url, calls) = spawn_helper(Some("should not be called")).await;
    let gw = gateway_with(
        vec![provider_row("p4", &base_url)],
        vec![plain_model("p4", "glm-4.6")],
    )
    .await;

    let mut request = image_request("glm-4.6", "cGFzcw==", "hi");
    let before = format!("{request:?}");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();

    assert_eq!(stats.images, 0);
    assert_eq!(format!("{request:?}"), before, "request untouched");
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_model_leaves_request_untouched() {
    let (base_url, calls) = spawn_helper(Some("unused")).await;
    let gw = gateway_with(vec![provider_row("p5", &base_url)], vec![shim_model("p5")]).await;

    let mut request = image_request("no-such-model", "eHg=", "hi");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();
    assert_eq!(stats.images, 0);
    assert!(matches!(
        request.messages[0].content,
        MessageContent::Blocks(ref blocks) if matches!(blocks[1], ContentBlock::Image { .. })
    ));
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn canonical_endpoint_id_protocol_is_accepted() {
    let (base_url, calls) = spawn_helper(Some("blue square")).await;
    let gw = gateway_with(
        vec![provider_row_with_protocol(
            "p6",
            &base_url,
            "openai-compatible/chat-completions/v1",
        )],
        vec![shim_model("p6")],
    )
    .await;

    let mut request = image_request("glm-5.3", "aXNzdWU=", "what color?");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();

    assert_eq!(stats.images, 1);
    assert_eq!(
        stats.captioned, 1,
        "canonical endpoint-id protocol must resolve to the OpenAI-compatible suite"
    );
    assert!(user_text(&request).contains("[Image 1: blue square]"));
    assert_eq!(calls.lock().unwrap().len(), 1);
}

fn shim_model_with_backends(provider_primary: &str, backends: serde_json::Value) -> Model {
    let mut model = shim_model(provider_primary);
    model.vision_shim = Some(serde_json::json!({ "helper_backends": backends }).to_string());
    model
}

#[tokio::test]
async fn helper_backends_fail_over_across_providers() {
    // Two helper providers on different mock servers: the first fails with
    // 500, the second captions. The shim must try them in order and succeed
    // via the second — helper selection spans vendors like target selection.
    let (failing_base, failing_calls) = spawn_helper(None).await;
    let (ok_base, ok_calls) = spawn_helper(Some("green triangle")).await;
    let failing = provider_row("helper-a", &failing_base);
    let ok = provider_row("helper-b", &ok_base);
    // The target provider is a third, unused-for-caption provider.
    let target = provider_row("target-p", "http://127.0.0.1:9");

    let gw = gateway_with(
        vec![failing, ok, target],
        vec![shim_model_with_backends(
            "target-p",
            serde_json::json!([
                { "provider": "helper-a", "model": "vl-a" },
                { "provider": "helper-b", "model": "vl-b" }
            ]),
        )],
    )
    .await;

    let mut request = image_request("glm-5.3", "ZmFpbG92ZXI=", "what shape?");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();

    assert_eq!(stats.images, 1);
    assert_eq!(stats.captioned, 1);
    assert_eq!(stats.placeholders, 0);
    assert_eq!(
        stats.helper_model, "vl-b",
        "stats report the backend that actually captioned"
    );
    assert!(user_text(&request).contains("[Image 1: green triangle]"));

    assert_eq!(
        failing_calls.lock().unwrap().len(),
        1,
        "first backend attempted"
    );
    let ok_calls = ok_calls.lock().unwrap();
    assert_eq!(ok_calls.len(), 1);
    assert_eq!(ok_calls[0]["model"], "vl-b");
}

#[tokio::test]
async fn helper_backends_matching_route_targets_are_skipped() {
    let (base_url, calls) = spawn_helper(Some("unused caption")).await;
    // Route target and sole helper backend are the same pair on purpose:
    // captioning through the text-only target would always fail, so the
    // shim must disable itself instead of looping the image back.
    let provider = provider_row("p7", &base_url);
    let mut model = shim_model_with_backends(
        "p7",
        serde_json::json!([{ "provider": "p7", "model": "glm-5.3" }]),
    );
    model.target_provider = "p7".to_string();
    model.target_model = "glm-5.3".to_string();

    let gw = gateway_with(vec![provider], vec![model]).await;
    let mut request = image_request("glm-5.3", "c2VsZg==", "hi");
    let before = format!("{request:?}");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();

    assert_eq!(stats.images, 0, "self-referencing helper disables the shim");
    assert_eq!(format!("{request:?}"), before);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn glm_family_helper_gets_low_thinking() {
    // GLM-family provider (by vendor field): the caption request must carry
    // a bounded thinking level so hybrid reasoning cannot starve `content`,
    // while a light reasoning pass over the image is kept.
    let (base_url, calls) = spawn_helper(Some("a blue circle")).await;
    let mut provider = provider_row("glm-p", &base_url);
    provider.vendor = Some("zhipuai".to_string());
    let gw = gateway_with(vec![provider], vec![shim_model("glm-p")]).await;

    let mut request = image_request("glm-5.3", "aXNvbWU=", "what is it?");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();
    assert_eq!(stats.captioned, 1);

    let thinking_type = {
        let calls = calls.lock().unwrap();
        calls[0]["thinking"]["type"].clone()
    };
    assert_eq!(thinking_type, "low");

    // Non-GLM provider: no thinking field injected.
    let (base2, calls2) = spawn_helper(Some("plain caption")).await;
    let plain = provider_row("plain-p", &base2);
    let gw2 = gateway_with(vec![plain], vec![shim_model("plain-p")]).await;
    let mut request2 = image_request("glm-5.3", "cGxhaW4=", "what is it?");
    vision_shim::apply(&gw2, &mut request2).await.unwrap();
    assert!(calls2.lock().unwrap()[0].get("thinking").is_none());
}

#[tokio::test]
async fn empty_helper_content_degrades_to_placeholder_with_finish_reason() {
    // Helper answers 200 but with empty content — the reasoning-starvation
    // failure mode seen in production. The placeholder must surface the
    // finish_reason for diagnosis.
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "" },
                    "finish_reason": "length"
                }],
                "usage": { "total_tokens": 999 }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let gw = gateway_with(
        vec![provider_row("starved", &format!("http://{addr}"))],
        vec![shim_model("starved")],
    )
    .await;

    let mut request = image_request("glm-5.3", "c3RhcnZl", "describe");
    let stats = vision_shim::apply(&gw, &mut request).await.unwrap();
    assert_eq!(stats.captioned, 0);
    assert_eq!(stats.placeholders, 1);
    let text = user_text(&request);
    assert!(
        text.contains("finish_reason=length"),
        "diagnostic in placeholder: {text}"
    );
    assert!(
        text.contains("thinking consumed"),
        "root-cause hint in placeholder: {text}"
    );
}
