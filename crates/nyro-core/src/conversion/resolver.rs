use axum::http::HeaderMap;
use bytes::Bytes;
use nyro_ccswitch_compat::{
    ConversionProfile, Direction, SessionClient, SessionIdentity, UpstreamFlavor,
    extract_session_identity, resolve_chat_reasoning_config,
};

use crate::db::models::Provider;
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1, ProtocolId,
};
use crate::protocol::ir::AiRequest;

use super::wire_patch::request_patch;
use super::{ConversionPlan, ConversionPlanError, RequestConversionMode, ResponseConversionMode};

#[derive(Debug, Clone)]
pub(crate) struct RawWireCompatSelection {
    pub(crate) ingress: ProtocolId,
    pub(crate) egress: ProtocolId,
    pub(crate) profile: ConversionProfile,
    pub(crate) identity: SessionIdentity,
    pub(crate) patch: Option<Bytes>,
    pub(crate) context_1m: bool,
}

impl RawWireCompatSelection {
    pub(crate) fn rule_id(&self) -> &'static str {
        raw_wire_rule_id(&self.profile)
    }

    pub(crate) fn plan(&self) -> ConversionPlan {
        ConversionPlan::raw_wire_compat(self.ingress, self.egress, self.rule_id())
    }
}

pub(crate) struct ResolveRawWireCompatInput<'a> {
    pub(crate) ingress: ProtocolId,
    pub(crate) egress: ProtocolId,
    pub(crate) provider: &'a Provider,
    pub(crate) egress_base_url: &'a str,
    pub(crate) actual_model: &'a str,
    pub(crate) client_stream: bool,
    pub(crate) headers: &'a HeaderMap,
    pub(crate) raw_body: &'a [u8],
    pub(crate) baseline_request: &'a AiRequest,
    pub(crate) current_request: &'a AiRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedConversion {
    plan: ConversionPlan,
    raw_wire: Option<RawWireCompatSelection>,
}

impl ResolvedConversion {
    pub(crate) fn plan(&self) -> &ConversionPlan {
        &self.plan
    }

    pub(crate) fn raw_wire(&self) -> Option<&RawWireCompatSelection> {
        self.raw_wire.as_ref()
    }

    pub(crate) fn into_parts(self) -> (ConversionPlan, Option<RawWireCompatSelection>) {
        (self.plan, self.raw_wire)
    }
}

pub(crate) struct ResolveConversionInput {
    pub(crate) ingress: ProtocolId,
    pub(crate) egress: ProtocolId,
    pub(crate) raw_wire: Option<RawWireCompatSelection>,
    pub(crate) protocol_is_native: bool,
    pub(crate) request_passthrough: bool,
    pub(crate) response_passthrough: bool,
}

pub(crate) fn resolve_conversion(
    input: ResolveConversionInput,
) -> Result<ResolvedConversion, ConversionPlanError> {
    if let Some(selection) = input.raw_wire {
        debug_assert_eq!(selection.ingress, input.ingress);
        debug_assert_eq!(selection.egress, input.egress);
        let plan = selection.plan();
        return Ok(ResolvedConversion {
            plan,
            raw_wire: Some(selection),
        });
    }

    let plan = if input.request_passthrough && input.response_passthrough {
        ConversionPlan::pass_through(input.ingress, input.egress, "native-no-mutations")?
    } else {
        let request_mode = if input.request_passthrough {
            RequestConversionMode::PassThroughJson
        } else {
            RequestConversionMode::IrEncode
        };
        let response_mode = if input.response_passthrough {
            ResponseConversionMode::PassThroughBytes
        } else {
            ResponseConversionMode::IrDecodeEncode
        };
        let rule_id = if input.protocol_is_native {
            "native-with-mutations"
        } else {
            "cross-protocol-ir"
        };
        ConversionPlan::native_ir(
            input.ingress,
            input.egress,
            request_mode,
            response_mode,
            rule_id,
        )?
    };

    Ok(ResolvedConversion {
        plan,
        raw_wire: None,
    })
}

pub(crate) fn supports_raw_wire_compat(
    ingress: ProtocolId,
    egress: ProtocolId,
    provider: &Provider,
    egress_base_url: &str,
    actual_model: &str,
) -> bool {
    let vendor_id = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    let channel = provider
        .channel
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    let openai_native = vendor_id.eq_ignore_ascii_case("openai")
        || channel.eq_ignore_ascii_case("codex")
        || channel.eq_ignore_ascii_case("sub2api");

    matches!(
        (ingress, egress),
        (
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        ) | (ANTHROPIC_MESSAGES_2023_06_01, OPENAI_RESPONSES_V1)
            | (
                ANTHROPIC_MESSAGES_2023_06_01,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
            )
            | (OPENAI_RESPONSES_V1, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
            | (OPENAI_RESPONSES_V1, ANTHROPIC_MESSAGES_2023_06_01)
    ) || (ingress == OPENAI_RESPONSES_V1
        && egress == OPENAI_RESPONSES_V1
        && vendor_id.eq_ignore_ascii_case("xai"))
        || (ingress == OPENAI_RESPONSES_V1
            && egress == OPENAI_RESPONSES_V1
            && !openai_native
            && !vendor_id.eq_ignore_ascii_case("xai"))
        || (ingress == ANTHROPIC_MESSAGES_2023_06_01
            && egress == ANTHROPIC_MESSAGES_2023_06_01
            && nyro_ccswitch_compat::anthropic_normalization_needed(
                vendor_id,
                egress_base_url,
                actual_model,
            ))
}

pub(crate) fn resolve_raw_wire_compat(
    input: ResolveRawWireCompatInput<'_>,
) -> Result<Option<RawWireCompatSelection>, String> {
    let ResolveRawWireCompatInput {
        ingress,
        egress,
        provider,
        egress_base_url,
        actual_model,
        client_stream,
        headers,
        raw_body,
        baseline_request,
        current_request,
    } = input;

    let vendor_id = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("custom");
    let channel = provider.channel.as_deref().unwrap_or_default().trim();

    let mut context_1m = false;
    let mut upstream_model = actual_model;
    let mut profile = match (ingress, egress) {
        (ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1) => {
            let haystack =
                format!("{vendor_id} {egress_base_url} {actual_model}").to_ascii_lowercase();
            let mut profile = ConversionProfile::anthropic_to_chat(client_stream);
            profile.preserve_chat_reasoning_content = ["deepseek", "mimo", "xiaomimimo"]
                .iter()
                .any(|hint| haystack.contains(hint));
            profile
        }
        (ANTHROPIC_MESSAGES_2023_06_01, OPENAI_RESPONSES_V1) => {
            let flavor = if vendor_id.eq_ignore_ascii_case("openai")
                && channel.eq_ignore_ascii_case("codex")
            {
                UpstreamFlavor::CodexOAuthResponses
            } else if vendor_id.eq_ignore_ascii_case("xai") {
                UpstreamFlavor::XaiStrictResponses
            } else {
                UpstreamFlavor::StandardResponses
            };
            let mut profile = ConversionProfile::anthropic_to_responses(client_stream, flavor);
            if provider.fast_mode
                && (channel.eq_ignore_ascii_case("sub2api")
                    || channel.eq_ignore_ascii_case("codex"))
            {
                profile.codex_fast_mode = true;
            }
            profile
        }
        (ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA) => {
            ConversionProfile::anthropic_to_gemini(client_stream)
        }
        (OPENAI_RESPONSES_V1, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1) => {
            let mut profile = ConversionProfile::codex_responses_to_chat(client_stream);
            if let Some(reasoning) =
                resolve_chat_reasoning_config(None, &provider.name, egress_base_url, actual_model)
            {
                profile = profile.with_chat_reasoning(reasoning);
            }
            profile.with_prompt_cache_key_support(chat_prompt_cache_key_supported(egress_base_url))
        }
        (OPENAI_RESPONSES_V1, ANTHROPIC_MESSAGES_2023_06_01) => {
            upstream_model = strip_one_m_suffix(actual_model);
            context_1m = upstream_model != actual_model;
            ConversionProfile::codex_responses_to_anthropic(client_stream)
        }
        (OPENAI_RESPONSES_V1, OPENAI_RESPONSES_V1) if vendor_id.eq_ignore_ascii_case("xai") => {
            ConversionProfile::xai_responses_native(client_stream)
        }
        (OPENAI_RESPONSES_V1, OPENAI_RESPONSES_V1) => {
            if !nyro_ccswitch_compat::request_needs_rewrite(raw_body) {
                return Ok(None);
            }
            ConversionProfile::third_party_responses_native(client_stream)
        }
        (ANTHROPIC_MESSAGES_2023_06_01, ANTHROPIC_MESSAGES_2023_06_01) => {
            ConversionProfile::anthropic_passthrough_normalized(client_stream)
                .with_anthropic_normalization(actual_model, egress_base_url, vendor_id)
        }
        _ => return Ok(None),
    };
    profile = profile
        .with_model(upstream_model)
        .with_provider_id(provider.id.clone());

    let session_client = if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
        SessionClient::Anthropic
    } else {
        SessionClient::CodexResponses
    };
    let compat_headers = headers
        .iter()
        .map(|(name, value)| {
            nyro_ccswitch_compat::Header::new(
                name.as_str(),
                Bytes::copy_from_slice(value.as_bytes()),
            )
        })
        .collect::<Vec<_>>();
    let identity = extract_session_identity(session_client, &compat_headers, raw_body)
        .map_err(|error| error.to_string())?;
    if matches!(profile.direction, Direction::AnthropicToResponses)
        && let Some(cache_key) = identity.prompt_cache_key()
    {
        profile = profile.with_cache_key(cache_key);
    }
    let patch = request_patch(baseline_request, current_request)?;

    Ok(Some(RawWireCompatSelection {
        ingress,
        egress,
        profile,
        identity,
        patch,
        context_1m,
    }))
}

pub(crate) fn raw_wire_rule_id(profile: &ConversionProfile) -> &'static str {
    match (profile.direction, profile.upstream_flavor) {
        (Direction::AnthropicToChat, _) => "anthropic-to-chat",
        (Direction::AnthropicToResponses, UpstreamFlavor::CodexOAuthResponses) => {
            "anthropic-to-responses-codex-oauth"
        }
        (Direction::AnthropicToResponses, UpstreamFlavor::XaiStrictResponses) => {
            "anthropic-to-responses-xai"
        }
        (Direction::AnthropicToResponses, _) => "anthropic-to-responses-standard",
        (Direction::AnthropicToGemini, _) => "anthropic-to-gemini",
        (Direction::AnthropicToAnthropic, _) => "anthropic-native-normalization",
        (Direction::CodexResponsesToChat, _) => "responses-to-chat",
        (Direction::CodexResponsesToAnthropic, _) => "responses-to-anthropic",
        (Direction::XaiResponsesNative, UpstreamFlavor::ThirdPartyStrictResponses) => {
            "responses-native-third-party-normalization"
        }
        (Direction::XaiResponsesNative, _) => "responses-native-xai-normalization",
    }
}

pub(crate) fn strip_one_m_suffix(model: &str) -> &str {
    const MARKER: &[u8] = b"[1m]";

    let trimmed = model.trim_end();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= MARKER.len()
        && bytes[bytes.len() - MARKER.len()..].eq_ignore_ascii_case(MARKER)
    {
        return trimmed[..trimmed.len() - MARKER.len()].trim_end();
    }
    model
}

pub(crate) fn chat_prompt_cache_key_supported(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    match url.host_str() {
        Some("api.openai.com") => true,
        Some("api.kimi.com") => {
            let path = url.path().trim_end_matches('/');
            path == "/coding" || path.starts_with("/coding/")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::AiRequest;

    fn provider(vendor: &str, channel: &str) -> Provider {
        Provider {
            id: "provider-test".into(),
            name: vendor.into(),
            vendor: Some(vendor.into()),
            protocol: "openai-responses".into(),
            base_url: "https://example.com/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: Some(channel.into()),
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

    fn request(model: &str, source: ProtocolId) -> AiRequest {
        let mut request = AiRequest::new(model, Vec::new());
        request.meta.source_protocol = Some(source);
        request
    }

    #[test]
    fn openai_native_responses_channels_are_not_wire_compat_candidates() {
        for (vendor, channel) in [
            ("openai", "default"),
            ("custom", "codex"),
            ("custom", "sub2api"),
        ] {
            assert!(!supports_raw_wire_compat(
                OPENAI_RESPONSES_V1,
                OPENAI_RESPONSES_V1,
                &provider(vendor, channel),
                "https://example.com",
                "gpt-5",
            ));
        }
    }

    #[test]
    fn xai_selection_produces_owned_raw_wire_plan() {
        let request = request("grok-4.5", OPENAI_RESPONSES_V1);
        let selection = resolve_raw_wire_compat(ResolveRawWireCompatInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            provider: &provider("xai", "default"),
            egress_base_url: "https://attacker.example",
            actual_model: "grok-4.5",
            client_stream: false,
            headers: &HeaderMap::new(),
            raw_body: br#"{"model":"grok-4.5","input":"hello"}"#,
            baseline_request: &request,
            current_request: &request,
        })
        .unwrap()
        .unwrap();

        assert_eq!(selection.rule_id(), "responses-native-xai-normalization");
        let plan = selection.plan();
        assert_eq!(plan.kind(), super::super::ConversionKind::RawWireCompat);
        assert_eq!(plan.ingress(), OPENAI_RESPONSES_V1);
        assert_eq!(plan.egress(), OPENAI_RESPONSES_V1);

        let final_conversion = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            raw_wire: Some(selection.clone()),
            protocol_is_native: true,
            request_passthrough: true,
            response_passthrough: true,
        })
        .unwrap();
        let final_plan = final_conversion.plan();
        assert_eq!(
            final_plan.kind(),
            super::super::ConversionKind::RawWireCompat
        );
        assert_eq!(final_plan.rule_id(), "responses-native-xai-normalization");
        assert!(final_conversion.raw_wire().is_some());
    }

    #[test]
    fn final_plan_distinguishes_passthrough_and_mixed_native_ir() {
        let passthrough = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            raw_wire: None,
            protocol_is_native: true,
            request_passthrough: true,
            response_passthrough: true,
        })
        .unwrap();
        assert_eq!(
            passthrough.plan().kind(),
            super::super::ConversionKind::PassThrough
        );
        assert!(passthrough.raw_wire().is_none());

        let mixed = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            raw_wire: None,
            protocol_is_native: true,
            request_passthrough: false,
            response_passthrough: true,
        })
        .unwrap();
        let mixed_plan = mixed.plan();
        assert_eq!(mixed_plan.kind(), super::super::ConversionKind::NativeIr);
        assert_eq!(mixed_plan.request_mode(), RequestConversionMode::IrEncode);
        assert_eq!(
            mixed_plan.response_mode(),
            ResponseConversionMode::PassThroughBytes
        );
        assert_eq!(mixed_plan.rule_id(), "native-with-mutations");
    }

    #[test]
    fn final_plan_marks_cross_protocol_ir() {
        let resolved = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: ANTHROPIC_MESSAGES_2023_06_01,
            raw_wire: None,
            protocol_is_native: false,
            request_passthrough: false,
            response_passthrough: false,
        })
        .unwrap();
        assert_eq!(
            resolved.plan().kind(),
            super::super::ConversionKind::NativeIr
        );
        assert_eq!(resolved.plan().rule_id(), "cross-protocol-ir");
    }
}
