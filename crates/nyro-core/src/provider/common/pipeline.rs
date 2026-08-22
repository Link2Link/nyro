//! Standard 7-step request/response pipeline shared by every
//! OpenAI-compatible vendor.
//!
//! # Usage
//!
//! Delegate `build_request` and `parse_response` to the free functions here:
//!
//! ```rust,ignore
//! use crate::provider::common::pipeline;
//!
//! async fn build_request(&self, req, ctx) -> Result<OutboundRequest> {
//!     pipeline::build_request(self, req, ctx).await
//! }
//! async fn parse_response(&self, resp, ctx) -> Result<AiResponse> {
//!     pipeline::parse_response(self, resp, ctx).await
//! }
//! ```

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::{OPENAI_RESPONSES_V1, ProtocolId};
use crate::provider::vendor::Vendor;

/// OpenAI Responses 渠道开关「Fast 模式」：
///
/// 开启后，转发到上游的 OpenAI Responses 请求如果缺少 `service_tier` 字段，
/// 就补上 `"service_tier": "priority"`（对应 OpenAI 官方 Fast mode：
/// 优先处理、响应更快；默认值是 `auto`）。客户端显式携带的 `service_tier`
/// 永远优先，本函数不做覆盖。当前对 OpenAI 预设的 `sub2api` 与 `codex`
/// 渠道生效。
pub(crate) fn maybe_inject_openai_fast_mode(
    body: &mut Value,
    provider: &Provider,
    protocol: ProtocolId,
) {
    let is_openai_fast = provider.fast_mode
        && provider.channel.as_deref().is_some_and(|channel| {
            channel.eq_ignore_ascii_case("sub2api") || channel.eq_ignore_ascii_case("codex")
        });
    if !is_openai_fast || protocol != OPENAI_RESPONSES_V1 {
        return;
    }
    if let Some(object) = body.as_object_mut()
        && !object.contains_key("service_tier")
    {
        object.insert(
            "service_tier".to_string(),
            Value::String("priority".to_string()),
        );
    }
}

/// Codex 消费级上游（`chatgpt.com/backend-api/codex`）不接受 OpenAI Responses
/// 的 `max_output_tokens`/`temperature`/`top_p` 参数（codex-rs 协议契约里
/// 没有这些字段，OpenAI 自己的客户端不发它们）。原生直通与 IR 转码两条路径
/// 转发前都需要剥离，否则上游返回 400 "Unsupported parameter: max_output_tokens"。
pub(crate) fn maybe_sanitize_codex_consumer_request(body: &mut Value, provider: &Provider) {
    let is_codex = provider
        .channel
        .as_deref()
        .is_some_and(|channel| channel.eq_ignore_ascii_case("codex"));
    if !is_codex {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
        object.remove("temperature");
        object.remove("top_p");
    }
}

/// Standard `build_request` pipeline:
/// `pre_request → normalize_tool_results → pre_encode → codec_encode →
///  post_encode → auth_headers → build_url`.
pub async fn build_request<V>(
    vendor: &V,
    req: &mut crate::protocol::ir::AiRequest,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<crate::provider::outbound::OutboundRequest, GatewayError>
where
    V: crate::provider::vendor::Vendor,
{
    req.model = ctx.actual_model.to_string();

    let vendor_ctx = ctx.to_vendor_ctx();

    // 1. pre_request hook
    vendor
        .pre_request(&vendor_ctx, req, ctx.gw)
        .await
        .map_err(GatewayError::internal)?;

    // 2. normalize tool results
    crate::protocol::codec::tool_correlation::normalize_request_tool_results(req);

    // 3. pre_encode hook
    vendor
        .pre_encode(&vendor_ctx, req)
        .await
        .map_err(GatewayError::internal)?;

    // 4. codec encode
    let egress_handler = ctx.protocol.handler();
    let encoder = egress_handler.make_request_encoder();
    let (mut body, mut extra_headers) = encoder
        .encode_request(req)
        .map_err(GatewayError::internal)?;

    // 5. post_encode hook
    vendor
        .post_encode(&vendor_ctx, &mut body, &mut extra_headers)
        .await
        .map_err(GatewayError::internal)?;

    // 5b. sub2api Fast 模式：缺 service_tier 时补 priority（IR 转码路径）
    maybe_inject_openai_fast_mode(&mut body, ctx.provider, ctx.protocol);
    // 5c. Codex 消费级上游：剥离其拒绝的 Responses 参数（IR 转码路径）
    maybe_sanitize_codex_consumer_request(&mut body, ctx.provider);

    // 6. auth headers
    //
    // OAuth drivers (codex, claude-code) stash their Bearer + provider-
    // specific headers in `RuntimeBinding.extra_headers` and ask the
    // dispatcher to skip the vendor's default `auth_headers` via
    // `ctx.disable_default_auth`. Skipping unconditionally would break
    // every API-key path; gating here keeps the OAuth invariant
    // ("no leaked empty x-api-key") in a single seam shared by every
    // openai-compatible adapter.
    let mut headers = if ctx.disable_default_auth {
        HeaderMap::new()
    } else {
        vendor.auth_headers(&vendor_ctx)
    };
    // Anthropic-protocol upstreams require `x-api-key` instead of
    // `Authorization: Bearer`. Most OpenAI-compatible vendors blindly emit
    // Bearer; rewrite here so any vendor with a declared anthropic endpoint
    // works out of the box.
    //
    // Skipped under `disable_default_auth`: when an OAuth driver owns auth
    // (claude-code uses `Bearer <oauth_token>` + `anthropic-beta=
    // oauth-2025-04-20`), `ctx.api_key` is the OAuth Bearer token, NOT a
    // real Anthropic API key. Rewriting it here would forward the Bearer
    // as a fake `x-api-key` and break the OAuth handshake.
    if !ctx.disable_default_auth
        && ctx.protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages
        && !headers.contains_key("x-api-key")
    {
        headers.remove(reqwest::header::AUTHORIZATION);
        if let Ok(v) = reqwest::header::HeaderValue::from_str(ctx.api_key) {
            headers.insert("x-api-key", v);
        }
    }
    headers.extend(extra_headers);

    // 7. build URL
    let egress_path = encoder.egress_path(ctx.actual_model, req.stream.enabled);
    let mut url = vendor.build_url(&vendor_ctx, ctx.egress_base_url, &egress_path);
    apply_explicit_auth_scheme(&mut headers, &mut url, ctx)?;

    Ok(crate::provider::outbound::OutboundRequest { url, headers, body })
}

/// Standard `parse_response` pipeline:
/// `pre_parse → codec_parse → reasoning_normalization → post_parse`.
pub async fn parse_response<V>(
    vendor: &V,
    resp: crate::provider::inbound::InboundResponse,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<crate::protocol::ir::AiResponse, GatewayError>
where
    V: crate::provider::vendor::Vendor,
{
    let vendor_ctx = ctx.to_vendor_ctx();
    let mut body = resp.body;

    // 1. pre_parse hook
    vendor
        .pre_parse(&vendor_ctx, &mut body)
        .await
        .map_err(GatewayError::internal)?;

    // 2. codec parse
    let egress_handler = ctx.protocol.handler();
    let parser = egress_handler.make_response_decoder();
    let mut ai_resp = parser
        .parse_response(body)
        .map_err(GatewayError::internal)?;

    // 3. reasoning normalization
    crate::protocol::codec::reasoning::normalize_response_reasoning(&mut ai_resp);

    // 4. post_parse hook
    vendor
        .post_parse(&vendor_ctx, &mut ai_resp)
        .await
        .map_err(GatewayError::internal)?;

    Ok(ai_resp)
}

/// PassThrough request builder: skips the IR codec entirely.
///
/// Used when [`crate::proxy::planner::ProtocolMode::Native`] is in effect
/// (ingress == egress) and the vendor declares no request mutations via
/// [`Vendor::declared_request_mutations`]. Authentication, URL/model
/// resolution, and narrowly scoped protocol defaults still apply; every other
/// client field is preserved. `is_stream` must come from the decoded ingress
/// request, not just a raw body field: native Gemini expresses streaming in the
/// URL action (`:streamGenerateContent`) rather than a JSON `stream` property.
fn normalize_openai_developer_roles(body: &mut serde_json::Value) {
    if let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    {
        for message in messages {
            if message.get("role").and_then(serde_json::Value::as_str) == Some("developer") {
                message["role"] = serde_json::Value::String("system".to_string());
            }
        }
    }
}

pub async fn passthrough_run(
    vendor: &dyn Vendor,
    mut raw_body: serde_json::Value,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    is_stream: bool,
) -> Result<crate::provider::outbound::OutboundRequest, GatewayError> {
    let vendor_ctx = ctx.to_vendor_ctx();
    let is_openai_chat = ctx.protocol.protocol == crate::protocol::ids::Protocol::OpenAICompatible
        && ctx.protocol.name == "chat-completions";

    // Replace the model field with the route-configured actual model so the
    // upstream receives the real model name, not the client's virtual alias.
    if let Some(obj) = raw_body.as_object_mut() {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(ctx.actual_model.to_string()),
        );

        // OpenAI chat-completions streaming only populates `usage` in the final
        // chunk when the client opts in via `stream_options.include_usage`.
        // PassThrough bypasses `OpenAIEncoder`, which injects this on the
        // transcode path
        // (encoder.rs "Always include_usage when streaming"). Mirror it here so
        // usage stays observable for logging/cost on the native path too. An
        // explicit client `stream_options` is preserved verbatim (same
        // precedence as the encoder). Embeddings (non-streaming, different
        // shape), Responses, and Anthropic/Gemini (usage reported by default)
        // are excluded.
        if is_stream && is_openai_chat && !obj.contains_key("stream_options") {
            obj.insert(
                "stream_options".to_string(),
                serde_json::json!({"include_usage": true}),
            );
        }
    }

    if is_openai_chat {
        normalize_openai_developer_roles(&mut raw_body);
    }
    if ctx.protocol == crate::protocol::ids::OPENAI_RESPONSES_V1 {
        crate::protocol::codec::openai::responses::normalize_function_tool_defaults(&mut raw_body);
        // sub2api Fast 模式：缺 service_tier 时补 priority（Responses 直通路径）
        maybe_inject_openai_fast_mode(&mut raw_body, ctx.provider, ctx.protocol);
        // Codex 消费级上游：剥离其拒绝的 Responses 参数（Responses 直通路径）
        maybe_sanitize_codex_consumer_request(&mut raw_body, ctx.provider);
    }

    let mut headers = if ctx.disable_default_auth {
        HeaderMap::new()
    } else {
        vendor.auth_headers(&vendor_ctx)
    };

    // Anthropic-family egress: rewrite Bearer → x-api-key (mirrors build_request).
    if !ctx.disable_default_auth
        && ctx.protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages
        && !headers.contains_key("x-api-key")
    {
        headers.remove(reqwest::header::AUTHORIZATION);
        if let Ok(v) = reqwest::header::HeaderValue::from_str(ctx.api_key) {
            headers.insert("x-api-key", v);
        }
    }

    let egress_path = ctx
        .protocol
        .handler()
        .make_request_encoder()
        .egress_path(ctx.actual_model, is_stream);
    let mut url = vendor.build_url(&vendor_ctx, ctx.egress_base_url, &egress_path);
    apply_explicit_auth_scheme(&mut headers, &mut url, ctx)?;

    Ok(crate::provider::outbound::OutboundRequest {
        url,
        headers,
        body: raw_body,
    })
}

fn apply_explicit_auth_scheme(
    headers: &mut HeaderMap,
    url: &mut String,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<(), GatewayError> {
    let scheme = ctx.auth_scheme.trim();
    if scheme.is_empty() || scheme == "auto" {
        return Ok(());
    }

    headers.remove(reqwest::header::AUTHORIZATION);
    headers.remove("x-api-key");
    remove_query_api_key(url)?;

    match scheme {
        "bearer" => {
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", ctx.api_key))
                .map_err(|error| GatewayError::internal(anyhow::Error::new(error)))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        "x-api-key" => {
            let value = reqwest::header::HeaderValue::from_str(ctx.api_key)
                .map_err(|error| GatewayError::internal(anyhow::Error::new(error)))?;
            headers.insert("x-api-key", value);
        }
        "query" => set_query_api_key(url, ctx.api_key)?,
        "none" => {}
        other => {
            return Err(GatewayError::internal(anyhow::anyhow!(
                "unsupported endpoint auth scheme: {other}"
            )));
        }
    }
    Ok(())
}

fn remove_query_api_key(raw_url: &mut String) -> Result<(), GatewayError> {
    rewrite_query_api_key(raw_url, None)
}

fn set_query_api_key(raw_url: &mut String, api_key: &str) -> Result<(), GatewayError> {
    rewrite_query_api_key(raw_url, Some(api_key))
}

fn rewrite_query_api_key(raw_url: &mut String, api_key: Option<&str>) -> Result<(), GatewayError> {
    let mut parsed = reqwest::Url::parse(raw_url)
        .map_err(|error| GatewayError::internal(anyhow::Error::new(error)))?;
    let existing = parsed
        .query_pairs()
        .filter(|(key, _)| key != "key")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    {
        let mut query = parsed.query_pairs_mut();
        query.clear();
        for (key, value) in existing {
            query.append_pair(&key, &value);
        }
        if let Some(api_key) = api_key {
            query.append_pair("key", api_key);
        }
    }
    *raw_url = parsed.to_string();
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests cover the `disable_default_auth` gate inside `build_request`.
    //! When `ProviderCtx.disable_default_auth` is set, the vendor's default
    //! `auth_headers` AND the Anthropic-egress `Authorization → x-api-key`
    //! rewrite MUST be suppressed. Both directions are pinned so a future
    //! refactor that flips a gate fails loudly.
    use super::*;
    use crate::Gateway;
    use crate::GatewayConfig;
    use crate::db::models::Provider;
    use crate::error::GatewayError;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId,
    };
    use crate::protocol::ir::{AiRequest, AiResponse};
    use crate::provider::inbound::InboundResponse;
    use crate::provider::outbound::OutboundRequest;
    use crate::provider::registry::VendorScope;
    use crate::provider::vendor::{ProviderCtx, Vendor};
    use crate::provider::vendor_ext::VendorCtx;
    use async_trait::async_trait;
    use reqwest::header::HeaderMap as ExtHeaderMap;
    use serde_json::Value;
    use uuid::Uuid;

    /// Stand-in vendor: injects `x-api-key: <ctx.api_key>`, mirroring
    /// how `AnthropicVendor::auth_headers` behaves.
    struct FakeApiKeyVendor;

    #[async_trait]
    impl Vendor for FakeApiKeyVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor {
                vendor_id: "fake-test",
            }
        }
        fn auth_headers(&self, ctx: &VendorCtx<'_>) -> ExtHeaderMap {
            let mut h = ExtHeaderMap::new();
            if !ctx.api_key.is_empty() {
                h.insert(
                    "x-api-key",
                    reqwest::header::HeaderValue::from_str(ctx.api_key).unwrap(),
                );
            }
            h
        }
        fn vendor_id(&self) -> &'static str {
            "fake-test"
        }
        fn supported_protocols(&self) -> &'static [ProtocolId] {
            &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
        }
        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!()
        }
        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!()
        }
        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("fake-test", status, None)
        }
    }

    /// Emits `Authorization: Bearer <ctx.api_key>`, mirroring OpenAI-compat
    /// vendors. PR #105's rewrite turns this into `x-api-key` on Anthropic egress.
    struct FakeBearerVendor;

    #[async_trait]
    impl Vendor for FakeBearerVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor {
                vendor_id: "fake-bearer",
            }
        }
        fn auth_headers(&self, ctx: &VendorCtx<'_>) -> ExtHeaderMap {
            let mut h = ExtHeaderMap::new();
            if !ctx.api_key.is_empty() {
                h.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", ctx.api_key))
                        .unwrap(),
                );
            }
            h
        }
        fn vendor_id(&self) -> &'static str {
            "fake-bearer"
        }
        fn supported_protocols(&self) -> &'static [ProtocolId] {
            &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
        }
        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!()
        }
        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!()
        }
        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("fake-bearer", status, None)
        }
    }

    fn provider_with_api_key(api_key: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some("fake-test".into()),
            protocol: "openai".into(),
            base_url: "https://upstream.local".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("fake-test".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: api_key.into(),
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

    fn minimal_chat_request() -> AiRequest {
        use crate::protocol::ir::{Message, MessageContent, Role};
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }];
        let mut req = AiRequest::new("ignored-by-actual-model", messages);
        req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        req
    }

    async fn build_test_gateway() -> Gateway {
        let config = GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("nyro-pipeline-test-{}", Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        gw
    }

    #[tokio::test]
    async fn build_request_suppresses_default_auth_when_oauth_owns_it() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("would-leak-if-bypassed");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gpt-test",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert!(
            out.headers.get("x-api-key").is_none(),
            "OAuth provider must not emit fallback x-api-key, got: {:?}",
            out.headers.get("x-api-key"),
        );
    }

    #[tokio::test]
    async fn build_request_keeps_default_auth_when_no_oauth() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gpt-test",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("apikey-abc"),
            "API-key path must still propagate x-api-key to upstream",
        );
    }

    #[tokio::test]
    async fn explicit_query_auth_uses_endpoint_credential_and_removes_header_auth() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("provider-level-key");
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://gemini.example",
            api_key: "endpoint-specific-key",
            auth_scheme: "query",
            actual_model: "gemini-test",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer stale"),
        );
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_static("stale"),
        );
        let mut url = "https://gemini.example/v1?existing=1&key=stale".to_string();

        apply_explicit_auth_scheme(&mut headers, &mut url, &ctx).unwrap();

        assert!(!headers.contains_key(reqwest::header::AUTHORIZATION));
        assert!(!headers.contains_key("x-api-key"));
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("existing").map(|value| value.as_ref()), Some("1"));
        assert_eq!(
            query.get("key").map(|value| value.as_ref()),
            Some("endpoint-specific-key")
        );
    }

    /// Pins the interaction: when an OAuth driver owns auth
    /// (`disable_default_auth=true`) AND the egress family is Anthropic, the
    /// `Authorization → x-api-key` rewrite must NOT fire.
    #[tokio::test]
    async fn build_request_does_not_rewrite_oauth_bearer_to_xapikey_on_anthropic_egress() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://api.anthropic.com",
            api_key: "oauth_bearer_token_should_not_become_xapikey",
            auth_scheme: "auto",
            actual_model: "claude-sonnet-4-6",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let out = build_request(&FakeBearerVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert!(
            out.headers.get("x-api-key").is_none(),
            "OAuth Bearer must not be rewritten as x-api-key, got: {:?}",
            out.headers.get("x-api-key"),
        );
        assert!(
            out.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "default Authorization must be suppressed under disable_default_auth too, got: {:?}",
            out.headers.get(reqwest::header::AUTHORIZATION),
        );
    }

    /// Mirror of #105's main use case: API-key-mode OpenAI-compat vendor
    /// hitting Anthropic egress — the rewrite block MUST fire and turn
    /// `Authorization: Bearer` into `x-api-key`.
    #[tokio::test]
    async fn build_request_rewrites_bearer_to_xapikey_on_anthropic_egress_for_apikey_path() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("real-anthropic-key");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://api.anthropic.com",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "claude-sonnet-4-6",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&FakeBearerVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("real-anthropic-key"),
            "API-key path on Anthropic egress must produce x-api-key",
        );
        assert!(
            out.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "Authorization must be removed once x-api-key is set",
        );
    }

    #[tokio::test]
    async fn passthrough_native_gemini_stream_uses_stream_generate_content_path() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("gemini-key");
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://gemini-proxy.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gemini-2.5-flash",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "contents": [{ "parts": [{ "text": "ping" }] }] }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.url,
            "https://gemini-proxy.local/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
            "Gemini stream passthrough selects streaming from the URL action, not a body stream flag",
        );
    }

    fn openai_chat_ctx<'a>(
        provider: &'a Provider,
        gw: &'a Gateway,
        actual_model: &'a str,
    ) -> ProviderCtx<'a> {
        ProviderCtx {
            provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model,
            credential: None,
            gw,
            disable_default_auth: false,
        }
    }

    /// The whole reason this injection exists: a native OpenAI chat-completions
    /// stream with no `stream_options` would otherwise be forwarded verbatim
    /// and the upstream would never report `usage`, so logging/cost sees 0/0.
    #[tokio::test]
    async fn passthrough_injects_include_usage_for_openai_streaming() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "gpt-test");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "messages": [{"role":"user","content":"ping"}], "stream": true }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["stream_options"]["include_usage"], true,
            "native openai stream without stream_options must get include_usage injected",
        );
    }

    #[tokio::test]
    async fn passthrough_converts_openai_developer_role_to_system() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "deepseek-v4-flash");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "messages": [
                    {"role": "developer", "content": "instructions"},
                    {"role": "user", "content": "hello"},
                    {"role": "assistant", "content": "hi"}
                ]
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(out.body["messages"][0]["role"], "system");
        assert_eq!(out.body["messages"][1]["role"], "user");
        assert_eq!(out.body["messages"][2]["role"], "assistant");
    }

    /// A client that explicitly sets `stream_options` (even to opt out of
    /// usage) owns that decision — the proxy must not override it.
    #[tokio::test]
    async fn passthrough_preserves_explicit_client_stream_options() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "gpt-test");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "messages": [{"role":"user","content":"ping"}],
                "stream": true,
                "stream_options": {"include_usage": false}
            }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["stream_options"]["include_usage"], false,
            "explicit client stream_options must be preserved verbatim",
        );
    }

    /// Non-streaming requests carry `usage` in the regular response body, so
    /// there is nothing to inject — and we must not pollute the body.
    fn provider_with_channel(api_key: &str, channel: Option<&str>, fast_mode: bool) -> Provider {
        let mut provider = provider_with_api_key(api_key);
        provider.channel = channel.map(str::to_string);
        provider.fast_mode = fast_mode;
        provider
    }

    fn responses_ctx<'a>(provider: &'a Provider, gw: &'a Gateway) -> ProviderCtx<'a> {
        ProviderCtx {
            provider,
            protocol: OPENAI_RESPONSES_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gpt-test",
            credential: None,
            gw,
            disable_default_auth: false,
        }
    }

    /// sub2api Fast 模式的核心行为：Responses 直通请求缺 service_tier 时
    /// 注入 "priority"（OpenAI 官方 Fast mode 语义）。
    #[tokio::test]
    async fn passthrough_injects_service_tier_priority_for_sub2api_fast_mode() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "o3", "input": "ping", "stream": true }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["service_tier"], "priority",
            "sub2api fast mode must inject service_tier=priority",
        );
    }

    /// Codex OAuth 渠道复用相同 Fast 模式语义。
    #[tokio::test]
    async fn passthrough_injects_service_tier_priority_for_codex_fast_mode() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("", Some("codex"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "gpt-5-codex", "input": "ping" }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["service_tier"], "priority",
            "codex fast mode must inject service_tier=priority",
        );
    }

    /// Codex 消费级上游：直通路径剥离其拒绝的 max_output_tokens/temperature/top_p。
    #[tokio::test]
    async fn passthrough_strips_rejected_params_for_codex_channel() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("", Some("codex"), false);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "gpt-5-codex",
                "input": "ping",
                "max_output_tokens": 4096,
                "temperature": 0.7,
                "top_p": 0.9,
                "stream": true,
            }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("max_output_tokens").is_none(),
            "codex consumer backend must not receive max_output_tokens",
        );
        assert!(
            out.body.get("temperature").is_none(),
            "codex consumer backend must not receive temperature",
        );
        assert!(
            out.body.get("top_p").is_none(),
            "codex consumer backend must not receive top_p",
        );
        assert_eq!(out.body["stream"], true, "stream must be preserved");
    }

    /// 非 codex 渠道必须保留这些参数，直通行为不受影响。
    #[tokio::test]
    async fn passthrough_keeps_rejected_params_for_other_channels() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), false);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "o3",
                "input": "ping",
                "max_output_tokens": 2048,
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["max_output_tokens"], 2048,
            "non-codex channel must keep max_output_tokens",
        );
    }

    /// IR 转码路径同样剥离：codex 渠道 encode 后清掉被拒绝的参数。
    #[tokio::test]
    async fn build_request_strips_rejected_params_for_codex_channel() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("", Some("codex"), false);
        let ctx = responses_ctx(&provider, &gw);
        let mut req = minimal_chat_request();

        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");

        assert!(
            out.body.get("max_output_tokens").is_none(),
            "IR transcode path must strip max_output_tokens for codex channel",
        );
        assert!(
            out.body.get("temperature").is_none(),
            "IR transcode path must strip temperature for codex channel",
        );
        assert!(
            out.body.get("top_p").is_none(),
            "IR transcode path must strip top_p for codex channel",
        );
    }

    /// 客户端显式携带的 service_tier 拥有最终决定权，Fast 模式不得覆盖。
    #[tokio::test]
    async fn passthrough_preserves_explicit_client_service_tier() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "o3",
                "input": "ping",
                "service_tier": "auto",
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["service_tier"], "auto",
            "explicit client service_tier must be preserved verbatim",
        );
    }

    /// Fast 模式开关关闭时不得注入任何字段。
    #[tokio::test]
    async fn passthrough_skips_service_tier_when_fast_mode_disabled() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), false);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "o3", "input": "ping" }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("service_tier").is_none(),
            "fast mode off must not inject service_tier",
        );
    }

    /// Fast 模式只属于 sub2api 渠道：其他渠道开启该标志也不会注入。
    #[tokio::test]
    async fn passthrough_skips_service_tier_for_other_channels() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-other", Some("default"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "o3", "input": "ping" }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("service_tier").is_none(),
            "non-sub2api channel must not receive service_tier injection",
        );
    }

    /// IR 转码路径（build_request）同样注入：post_encode 之后补 priority。
    #[tokio::test]
    async fn build_request_injects_service_tier_for_sub2api_fast_mode() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), true);
        let ctx = responses_ctx(&provider, &gw);
        let mut req = minimal_chat_request();

        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");

        assert_eq!(
            out.body["service_tier"], "priority",
            "IR transcode path must inject service_tier=priority for sub2api fast mode",
        );
    }

    #[tokio::test]
    async fn passthrough_skips_include_usage_when_not_streaming() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "gpt-test");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "messages": [{"role":"user","content":"ping"}] }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("stream_options").is_none(),
            "non-streaming passthrough must not inject stream_options",
        );
    }
}
