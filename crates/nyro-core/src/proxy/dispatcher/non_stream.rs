//! Non-streaming response handlers.
//!
//! `handle_non_stream`: standard non-streaming upstream call.
//! `handle_non_stream_via_upstream_stream`: upstream forces SSE but client
//!   requested non-stream — accumulate into a single response.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::Value;

use crate::integrations::{HookContext, HookRegistry};
use crate::plugin::phase::{HostContext, Phase, PhaseOutcome, ResponseView};
use crate::protocol::codec::tool_bridge::ToolRoutePlan;
use crate::protocol::ir::AiRequest;
use crate::provider::inbound::InboundResponse;
use crate::provider::vendor::ProviderCtx;
use crate::proxy::client::{ProxyClient, UpstreamResponseDecodeError};
use crate::proxy::context::RequestContext;
use crate::proxy::observability::headers_to_json;

use super::{
    CallCtx, LogBuilder, RequestExtras, StreamResponseAccumulator, error_response, run_phase_hooks,
};

// ── Non-streaming response handler ───────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_non_stream(
    client: ProxyClient,
    url: &str,
    headers: ReqwestHeaderMap,
    body: Value,
    call_ctx: &CallCtx<'_>,
    req_extras: &RequestExtras,
    adapter: &dyn crate::provider::vendor::Vendor,
    // `ctx` is the vendor-level provider context used for codec operations.
    ctx: &ProviderCtx<'_>,
    // When true: Native protocol + no response mutations → skip IR round-trip.
    passthrough_resp: bool,
    tool_route_plan: &ToolRoutePlan,
    // Request-scoped context + IR + host boundary, threaded for the OnResponse phase.
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &HostContext<'_>,
) -> Response {
    let egress = call_ctx.egress;
    let ingress = call_ctx.ingress;
    let egress_str = call_ctx.egress_str; // used in tracing::debug!
    let actual_model = call_ctx.actual_model;
    // Shared log builder pre-filled with identity + request-side extras.
    let log = LogBuilder::from_ctx(call_ctx).with_req_extras(req_extras);
    let upstream_req_hdrs_str = crate::proxy::observability::reqwest_headers_to_json(&headers);
    let upstream_req_body_str = serde_json::to_string(&body).ok();

    let upstream_start = std::time::Instant::now();
    let call_result = match client
        .call_non_stream(url, headers.clone(), body.clone())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let upstream_latency_ms = upstream_start.elapsed().as_millis() as i64;
            let log = log
                .upstream_url(url)
                .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str);
            if let Some(decode) = e.downcast_ref::<UpstreamResponseDecodeError>() {
                let upstream_hdrs_str = headers_to_json(&decode.headers);
                let upstream_body_str = Some(decode.body_text());
                log.status(502)
                    .with_upstream_response(
                        decode.status as i32,
                        upstream_hdrs_str,
                        upstream_body_str,
                        Some(upstream_latency_ms),
                    )
                    .resp_body(Some(
                        serde_json::json!({ "error": { "message": format!("upstream error: {e:#}") } })
                            .to_string(),
                    ))
                    .emit();
            } else {
                log.status(502)
                    .resp_body(Some(
                        serde_json::json!({ "error": { "message": format!("upstream error: {e:#}") } })
                            .to_string(),
                    ))
                    .emit();
            }
            return error_response(502, &format!("upstream error: {e:#}"));
        }
    };
    let upstream_latency_ms = upstream_start.elapsed().as_millis() as i64;

    let (resp, status, upstream_headers) = call_result;
    let upstream_hdrs_str = headers_to_json(&upstream_headers);

    if status >= 400 {
        let body_str = serde_json::to_string(&resp).ok();
        log.status(status)
            .upstream_url(url)
            .upstream_status(status as i32)
            .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
            .with_upstream_response(
                status as i32,
                upstream_hdrs_str.clone(),
                body_str.clone(),
                Some(upstream_latency_ms),
            )
            .resp_body(body_str)
            .emit();
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(resp),
        )
            .into_response();
    }

    // Embeddings: passthrough response (parse_response is not implemented for codec).
    if egress.handler().capabilities().embeddings {
        let usage = crate::protocol::codec::openai::compatible::embeddings::parse_usage(&resp);
        let resp_str = serde_json::to_string(&resp).ok();
        log.status(status)
            .upstream_url(url)
            .usage(usage)
            .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
            .with_upstream_response(
                status as i32,
                upstream_hdrs_str.clone(),
                resp_str.clone(),
                Some(upstream_latency_ms),
            )
            .with_client_response(None, resp_str)
            .emit();
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            Json(resp),
        )
            .into_response();
    }

    // PassThrough: Native protocol + no response mutations → forward upstream JSON verbatim,
    // skipping the IR round-trip (parse_response → InternalResponse → format_response).
    if passthrough_resp {
        tracing::debug!(
            mode = "passthrough",
            egress = egress_str,
            "bypassing IR round-trip"
        );
        // Preserve the wire response while still decoding usage for logging.
        // A best-effort side parse must never turn a valid passthrough response
        // into an error returned to the client.
        let usage = match adapter
            .parse_response(
                InboundResponse {
                    status,
                    body: resp.clone(),
                },
                ctx,
            )
            .await
        {
            Ok(ai_resp) => ai_resp.usage,
            Err(error) => {
                tracing::warn!(%error, egress = egress_str, "failed to parse passthrough usage");
                Default::default()
            }
        };
        let resp_str = serde_json::to_string(&resp).ok();
        log.status(status)
            .upstream_url(url)
            .usage(usage)
            .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
            .with_upstream_response(
                status as i32,
                upstream_hdrs_str.clone(),
                resp_str.clone(),
                Some(upstream_latency_ms),
            )
            .with_client_response(None, resp_str)
            .emit();
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            Json(resp),
        )
            .into_response();
    }

    // Parse response via ProviderAdapter.
    let upstream_resp_str = serde_json::to_string(&resp).ok();
    let inbound = InboundResponse { status, body: resp };
    let mut ai_resp = match adapter.parse_response(inbound, ctx).await {
        Ok(r) => r,
        Err(e) => {
            log.status(500)
                .upstream_url(url)
                .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
                .with_upstream_response(
                    status as i32,
                    upstream_hdrs_str.clone(),
                    upstream_resp_str,
                    Some(upstream_latency_ms),
                )
                .resp_body(Some(
                    serde_json::json!({ "error": { "message": format!("parse error: {e}") } })
                        .to_string(),
                ))
                .emit();
            return error_response(500, &format!("parse error: {e}"));
        }
    };
    tool_route_plan.restore_response(&mut ai_resp);

    // Ensure actual_model is set in the response.
    if ai_resp.model.is_empty() {
        ai_resp.model = actual_model.to_string();
    }

    // ── Response hooks ──────────────────────────────────────────────────────
    let hook_registry = HookRegistry::global();
    if hook_registry.has_response_hooks() {
        let latency_ms = call_ctx.start.elapsed().as_millis() as u64;
        let hook_ctx = HookContext {
            model_id: call_ctx.model_id.to_string(),
            provider_name: call_ctx.provider.name.clone(),
            model: ai_resp.model.clone(),
            api_key_id: call_ctx.api_key_id.map(str::to_string),
        };
        for hook in hook_registry.response_hooks() {
            hook.on_response(&hook_ctx, &mut ai_resp, latency_ms).await;
        }
    }

    // ── OnResponse phase (full body) ─────────────────────────────────────────
    // Hooks see the buffered `AiResponse` and may reshape it before it is
    // encoded for the client. ShortCircuit/Reject replace the response (the
    // native success log below is then skipped; OnLog still fires at the
    // pipeline boundary). No-op when no OnResponse hooks are registered.
    match run_phase_hooks(
        Phase::OnResponse,
        req_ctx,
        req_ir,
        ResponseView::Full(&mut ai_resp),
        host,
    )
    .await
    {
        PhaseOutcome::Continue => {}
        PhaseOutcome::ShortCircuit(resp) => return resp,
        PhaseOutcome::Reject(e) => return e.render(None),
    }

    let usage = ai_resp.usage.clone();
    let formatter = ingress.handler().make_response_encoder();
    let output = formatter.format_response(&ai_resp);

    let response_body_full = serde_json::to_string(&output).ok();
    log.status(status)
        .upstream_url(url)
        .usage(usage)
        .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
        .with_upstream_response(
            status as i32,
            upstream_hdrs_str,
            upstream_resp_str,
            Some(upstream_latency_ms),
        )
        .with_client_response(None, response_body_full)
        .emit();

    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        Json(output),
    )
        .into_response()
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use reqwest::header::HeaderValue;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::Gateway;
    use crate::config::GatewayConfig;
    use crate::db::models::Provider;
    use crate::error::GatewayError;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };
    use crate::protocol::ir::{AiRequest, AiResponse};
    use crate::provider::anthropic::AnthropicVendor;
    use crate::provider::outbound::OutboundRequest;
    use crate::provider::registry::VendorScope;
    use crate::provider::vendor::Vendor;
    use crate::provider::vendor_ext::VendorCtx;

    struct NoopVendor;

    #[async_trait]
    impl Vendor for NoopVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor { vendor_id: "noop" }
        }

        fn vendor_id(&self) -> &'static str {
            "noop"
        }

        fn supported_protocols(&self) -> &'static [crate::protocol::ids::ProtocolId] {
            &[GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA]
        }

        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!("test calls handle_non_stream after outbound is built")
        }

        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!("decode error happens before provider response parsing")
        }

        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("noop", status, None)
        }

        fn auth_headers(&self, _ctx: &VendorCtx<'_>) -> ReqwestHeaderMap {
            ReqwestHeaderMap::new()
        }
    }

    fn fake_provider(base_url: String) -> Provider {
        Provider {
            id: "provider-google".into(),
            name: "Google".into(),
            vendor: Some("google".into()),
            protocol: "google-gemini".into(),
            base_url,
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
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

    async fn serve_invalid_json_once() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await.expect("read request");
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-request-id: upstream-123\r\ncontent-length: 16\r\n\r\nnot valid json!!",
                )
                .await
                .expect("write response");
        });
        format!("http://{addr}/v1beta/models/gemini:generateContent?key=secret")
    }

    async fn serve_anthropic_response_once() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let body = serde_json::json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "deepseek-v4-pro",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "cache_read_input_tokens": 26624,
                "input_tokens": 282,
                "output_tokens": 595
            }
        })
        .to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await.expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{addr}/v1/messages")
    }

    #[tokio::test]
    async fn logs_usage_for_non_stream_native_passthrough_response() {
        let url = serve_anthropic_response_once().await;
        let provider = Provider {
            id: "provider-anthropic".into(),
            name: "Anthropic-compatible".into(),
            vendor: Some("anthropic".into()),
            protocol: "anthropic-messages".into(),
            base_url: url.clone(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let config = GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "nyro-passthrough-usage-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        };
        let (gw, mut log_rx) = Gateway::new(config).await.expect("gateway init");
        let req_ext = crate::proxy::context::ContextBag::new();
        let call_ctx = CallCtx {
            gw: gw.clone(),
            provider: &provider,
            model_id: "route-anthropic",
            model_name: "Anthropic route",
            egress: ANTHROPIC_MESSAGES_2023_06_01,
            ingress: ANTHROPIC_MESSAGES_2023_06_01,
            ingress_str: "anthropic-messages/messages/2023-06-01",
            egress_str: "anthropic-messages/messages/2023-06-01",
            request_model: "deepseek-v4-pro",
            actual_model: "deepseek-v4-pro",
            api_key_id: None,
            api_key_name: None,
            is_stream: false,
            enable_payload: None,
            reasoning_effort: None,
            start: std::time::Instant::now(),
            req_ext,
        };
        let req_extras = RequestExtras {
            method: "POST".into(),
            path: "/v1/messages".into(),
            headers: None,
            body: None,
        };
        let provider_ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: &provider.base_url,
            api_key: "secret",
            auth_scheme: "auto",
            actual_model: "deepseek-v4-pro",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let mut req_ctx = crate::proxy::context::RequestContext::new(
            ANTHROPIC_MESSAGES_2023_06_01,
            std::time::Duration::from_secs(30),
        );
        let mut req_ir = AiRequest::new("deepseek-v4-pro", Vec::new());
        let host = HostContext::new(&gw);
        let tool_route_plan = ToolRoutePlan::default();

        let response = handle_non_stream(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            ReqwestHeaderMap::new(),
            serde_json::json!({"model": "deepseek-v4-pro"}),
            &call_ctx,
            &req_extras,
            &AnthropicVendor,
            &provider_ctx,
            true,
            &tool_route_plan,
            &mut req_ctx,
            &mut req_ir,
            &host,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("log entry should be emitted")
            .expect("log channel should remain open");
        // After the IR-usage gross/net unification, prompt_tokens is the
        // GROSS input (net input 282 + cache_read 26624 = 26906). The wire
        // shape sent back to Anthropic clients is unchanged (still
        // input_tokens=282); only the IR / DB representation is gross now.
        assert_eq!(entry.input_tokens(), 26906);
        assert_eq!(entry.output_tokens(), 595);
        assert_eq!(entry.cache_read_tokens(), 26624);
    }

    #[tokio::test]
    async fn logs_upstream_wire_data_when_non_stream_response_json_decode_fails() {
        let url = serve_invalid_json_once().await;
        let base_url = url.split("/v1beta").next().unwrap().to_string();
        let provider = fake_provider(base_url);
        let config = GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-decode-log-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, mut log_rx) = Gateway::new(config).await.expect("gateway init");

        let req_ext = crate::proxy::context::ContextBag::new();
        let call_ctx = CallCtx {
            gw: gw.clone(),
            provider: &provider,
            model_id: "route-google",
            model_name: "Google route",
            egress: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ingress_str: "openai/chat/v1",
            egress_str: "google/gemini/generateContent/v1beta",
            request_model: "virtual-gemini",
            actual_model: "gemini-2.5-flash",
            api_key_id: None,
            api_key_name: None,
            is_stream: false,
            enable_payload: None,
            reasoning_effort: None,
            start: std::time::Instant::now(),
            req_ext: req_ext.clone(),
        };
        let req_extras = RequestExtras {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            headers: None,
            body: Some(r#"{"model":"virtual-gemini"}"#.into()),
        };
        let provider_ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: &provider.base_url,
            api_key: "secret",
            auth_scheme: "auto",
            actual_model: "gemini-2.5-flash",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let mut headers = ReqwestHeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let mut onresp_ctx = crate::proxy::context::RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        );
        let mut onresp_req = AiRequest::new("virtual-gemini", Vec::new());
        let onresp_host = HostContext::new(&gw);
        let tool_route_plan = ToolRoutePlan::default();

        let response = handle_non_stream(
            ProxyClient::new(reqwest::Client::new()),
            &url,
            headers,
            serde_json::json!({"model": "gemini-2.5-flash"}),
            &call_ctx,
            &req_extras,
            &NoopVendor,
            &provider_ctx,
            false,
            &tool_route_plan,
            &mut onresp_ctx,
            &mut onresp_req,
            &onresp_host,
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("log entry should be emitted")
            .expect("log channel should remain open");

        assert_eq!(entry.upstream_status_code, Some(200));
        assert_eq!(
            entry.upstream_response_body.as_deref(),
            Some("not valid json!!")
        );
        assert!(
            entry
                .upstream_response_headers
                .as_deref()
                .is_some_and(|h| h.contains("upstream-123"))
        );
        assert!(
            entry
                .upstream_request_body
                .as_deref()
                .is_some_and(|b| b.contains("gemini-2.5-flash"))
        );
        assert!(
            entry
                .upstream_url
                .as_deref()
                .is_some_and(|u| u.contains("generateContent") && u.contains("key=***"))
        );
        assert!(
            entry
                .client_response_body
                .as_deref()
                .is_some_and(|b| b.contains("error decoding response body"))
        );

        // OnResponse → ctx: a canonical ResponseStats snapshot is injected on emit.
        let stats = req_ext
            .get::<crate::plugin::phase::ResponseStats>()
            .expect("ResponseStats snapshot injected into request context");
        assert_eq!(stats.client_status, 502);
        assert_eq!(stats.upstream_status, Some(200));
        assert_eq!(stats.stream_chunks, 0);
    }

    /// Regression (live MiniMax finding): a passthrough body carries no
    /// `stream` flag, but the force-upstream-stream handler must request SSE
    /// explicitly; and when the upstream ignores the flag and answers with a
    /// plain JSON document, the handler must fall back to the full-response
    /// decoder instead of returning an empty success.
    #[tokio::test]
    async fn forced_upstream_stream_sets_stream_flag_and_parses_json_fallback() {
        use crate::protocol::ids::OPENAI_RESPONSES_V1;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test addr");
        let (request_tx, request_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
        let body = serde_json::json!({
            "id": "resp_plainjson",
            "object": "response",
            "created_at": 1786730493,
            "model": "MiniMax-M2.7-highspeed",
            "status": "completed",
            "output": [
                {"id": "rs", "type": "reasoning", "status": "completed", "summary": [],
                 "content": [{"type": "reasoning_text", "text": "thinking"}]},
                {"id": "msg", "type": "message", "status": "completed", "role": "assistant",
                 "content": [{"type": "output_text", "text": "PLAIN-JSON-OK", "annotations": []}]}
            ],
            "usage": {"input_tokens": 47, "output_tokens": 115, "total_tokens": 162}
        })
        .to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.expect("read request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let content_length = std::str::from_utf8(&buf[..pos])
                        .ok()
                        .and_then(|headers| {
                            headers
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or_default();
                    if buf.len() >= pos + 4 + content_length {
                        break;
                    }
                }
            }
            let _ = request_tx.send(buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let url = format!("http://{addr}/v1/responses");

        let provider = Provider {
            id: "provider-resp".into(),
            name: "Responses-compatible".into(),
            vendor: Some("custom".into()),
            protocol: "openai-responses".into(),
            base_url: url.clone(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let config = GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-forced-stream-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        let call_ctx = CallCtx {
            gw: gw.clone(),
            provider: &provider,
            model_id: "route-resp",
            model_name: "Responses route",
            egress: OPENAI_RESPONSES_V1,
            ingress: OPENAI_RESPONSES_V1,
            ingress_str: "openai-responses/responses/v1",
            egress_str: "openai-responses/responses/v1",
            request_model: "mm-resp",
            actual_model: "MiniMax-M2.7-highspeed",
            api_key_id: None,
            api_key_name: None,
            is_stream: false,
            enable_payload: None,
            reasoning_effort: None,
            start: std::time::Instant::now(),
            req_ext: crate::proxy::context::ContextBag::new(),
        };
        let mut req_ctx =
            RequestContext::new(OPENAI_RESPONSES_V1, std::time::Duration::from_secs(30));
        let mut req_ir = AiRequest::new("MiniMax-M2.7-highspeed", Vec::new());
        let host = HostContext::new(&gw);
        let tool_route_plan = ToolRoutePlan::default();
        let client = ProxyClient::new(reqwest::Client::new());

        let response = handle_non_stream_via_upstream_stream(
            client,
            &url,
            ReqwestHeaderMap::new(),
            serde_json::json!({"model": "mm-resp", "input": "hi"}),
            &call_ctx,
            tool_route_plan,
            &mut req_ctx,
            &mut req_ir,
            &host,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(value["id"], "resp_plainjson");
        let text: String = value["output"]
            .as_array()
            .expect("output items")
            .iter()
            .filter(|item| item["type"] == "message")
            .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
            .filter(|part| part["type"] == "output_text")
            .map(|part| part["text"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(text, "PLAIN-JSON-OK");
        assert_eq!(value["usage"]["input_tokens"], 47);

        let request = request_rx.await.expect("captured upstream request");
        let request_body = String::from_utf8_lossy(&request);
        let sent: Value = serde_json::from_str(
            &request_body[request_body
                .find("\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or_default()..],
        )
        .expect("upstream request json");
        assert_eq!(sent["stream"], true, "force-stream must set stream:true");
    }
}

// ── Force-stream non-stream handler ──────────────────────────────────────────

/// Consume a streaming upstream response and return a non-streaming client
/// response. Used when the egress protocol forces `stream: true` upstream
/// (e.g. Responses API) but the ingress client requested non-stream.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_non_stream_via_upstream_stream(
    client: ProxyClient,
    url: &str,
    headers: ReqwestHeaderMap,
    body: Value,
    call_ctx: &CallCtx<'_>,
    mut tool_route_plan: ToolRoutePlan,
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &HostContext<'_>,
) -> Response {
    let egress = call_ctx.egress;
    let ingress = call_ctx.ingress;
    let actual_model = call_ctx.actual_model;
    let log = LogBuilder::from_ctx(call_ctx).upstream_url(url);

    let mut body = body;
    // `force_upstream_stream` semantics: the upstream call is always made in
    // streaming mode regardless of the client's `stream` flag. A passthrough
    // body carries the client's (absent) flag, so set it explicitly — an
    // upstream that never receives `stream: true` answers with plain JSON,
    // which the SSE parser below cannot read.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), Value::Bool(true));
    }

    let upstream_start = std::time::Instant::now();
    let call_result = match client.call_stream(url, headers.clone(), body.clone()).await {
        Ok(r) => r,
        Err(e) => {
            log.status(502)
                .resp_body(Some(
                    serde_json::json!({ "error": { "message": format!("upstream error: {e:#}") } })
                        .to_string(),
                ))
                .emit();
            return error_response(502, &format!("upstream error: {e:#}"));
        }
    };
    let upstream_latency_ms = upstream_start.elapsed().as_millis() as i64;

    let (resp, status) = call_result;
    let upstream_hdrs_str = headers_to_json(resp.headers());
    let upstream_req_hdrs_str = crate::proxy::observability::reqwest_headers_to_json(&headers);
    let upstream_req_body_str = serde_json::to_string(&body).ok();

    if status >= 400 {
        let err_body: Value = resp
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({"error": {"message": "upstream error"}}));
        let err_body_str = serde_json::to_string(&err_body).ok();
        log.status(status)
            .upstream_url(url)
            .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
            .with_upstream_response(
                status as i32,
                upstream_hdrs_str,
                err_body_str.clone(),
                Some(upstream_latency_ms),
            )
            .resp_body(err_body_str)
            .emit();
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(err_body),
        )
            .into_response();
    }

    let mut stream_parser = egress.handler().make_stream_response_decoder();
    let mut byte_stream = resp.bytes_stream();
    let mut accumulator = StreamResponseAccumulator::default();
    let mut raw_text = String::new();

    while let Some(chunk) = byte_stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                log.status(502)
                    .error(format!("stream read error: {e}"))
                    .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
                    .upstream_resp_headers(upstream_hdrs_str)
                    .resp_body(Some(
                        serde_json::json!({ "error": { "message": format!("upstream stream error: {e}") } })
                            .to_string(),
                    ))
                    .emit();
                return error_response(502, &format!("upstream stream error: {e}"));
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        raw_text.push_str(&text);
        if let Ok(ai_deltas) = stream_parser.parse_chunk(&text) {
            let ai_deltas = tool_route_plan.restore_stream_deltas(ai_deltas);
            accumulator.apply_all(&ai_deltas);
        }
    }

    if let Ok(ai_deltas) = stream_parser.finish() {
        let ai_deltas = tool_route_plan.restore_stream_deltas(ai_deltas);
        accumulator.apply_all(&ai_deltas);
    }
    accumulator.apply_all(&tool_route_plan.finish_stream());

    let mut ai_resp = accumulator.into_ai_response();
    // Some Responses-compatible gateways ignore `stream: true` and answer with
    // a complete JSON document. The SSE parser yields nothing for that shape;
    // fall back to the full-response decoder instead of returning an empty
    // success (mirrors cc-switch's #2234 unlabelled-JSON fallback).
    let parsed_nothing =
        ai_resp.id.is_empty() && ai_resp.content.is_empty() && ai_resp.items.is_none();
    if parsed_nothing
        && raw_text.trim_start().starts_with('{')
        && let Ok(full) = serde_json::from_str::<Value>(&raw_text)
        && let Ok(parsed) = egress
            .handler()
            .make_response_decoder()
            .parse_response(full)
    {
        ai_resp = parsed;
    }
    if ai_resp.id.is_empty() {
        ai_resp.id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    }
    if ai_resp.model.is_empty() {
        ai_resp.model = actual_model.to_string();
    }
    if ai_resp.stop_reason.is_none() {
        ai_resp.stop_reason = Some("stop".to_string());
    }

    // ── OnResponse phase (full body) ─────────────────────────────────────────
    // Force-stream collapses an upstream SSE into one buffered `AiResponse`, so
    // hooks see it as a non-streaming full body (same contract as
    // `handle_non_stream`).
    match run_phase_hooks(
        Phase::OnResponse,
        req_ctx,
        req_ir,
        ResponseView::Full(&mut ai_resp),
        host,
    )
    .await
    {
        PhaseOutcome::Continue => {}
        PhaseOutcome::ShortCircuit(resp) => return resp,
        PhaseOutcome::Reject(e) => return e.render(None),
    }

    let usage = ai_resp.usage.clone();
    let formatter = ingress.handler().make_response_encoder();
    let output = formatter.format_response(&ai_resp);

    let client_resp_body_str = serde_json::to_string(&output).ok();
    log.status(status)
        .upstream_url(url)
        .usage(usage)
        .with_upstream_request(upstream_req_hdrs_str, upstream_req_body_str)
        .with_upstream_response(
            status as i32,
            upstream_hdrs_str,
            None,
            Some(upstream_latency_ms),
        )
        .with_client_response(None, client_resp_body_str)
        .emit();

    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        Json(output),
    )
        .into_response()
}
