//! MiniMax vendor (OpenAI-compatible).

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
    ProtocolId,
};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::openai::{
    openai_bearer_auth_headers, openai_build_url, openai_map_error,
};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const METADATA: VendorMetadata = VendorMetadata {
    id: "minimax",
    label: Label {
        zh: "MiniMax",
        en: "MiniMax",
    },
    icon: "minimax",
    default_protocol: "openai-compatible",
    channels: &[
        ChannelDef {
            id: "default",
            label: Label {
                zh: "默认",
                en: "Default",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://api.minimax.io/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://api.minimax.io/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("ai://models.dev/minimax"),
            capabilities_source: CapabilitiesSource::ModelsDev("minimax"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: false,
            auth_schemes: None,
        },
        ChannelDef {
            id: "china",
            label: Label {
                zh: "中国站",
                en: "China",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://api.minimaxi.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://api.minimaxi.com/anthropic",
                },
                ProtocolBaseUrl {
                    protocol: "openai-responses",
                    base_url: "https://api.minimaxi.com/v1",
                },
            ],
            api_key: None,
            models_source: Some("https://api.minimaxi.com/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("minimax"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: None,
        },
    ],
};

pub struct MinimaxVendor;

/// Operator-requested floor for explicitly supplied MiniMax Chat output budgets.
/// This is an allowance, not a target length or a claim about model capacity.
const CHAT_OUTPUT_TOKEN_FLOOR: u64 = 256 * 1024;

pub(crate) fn apply_chat_output_token_floor(
    body: &mut Value,
    vendor: Option<&str>,
    protocol: ProtocolId,
) {
    if vendor != Some("minimax") || protocol != OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        return;
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };
    // Match the Chat decoder's precedence. Do not repair invalid values or
    // insert a budget when the client left it unspecified.
    let budget = object
        .get("max_completion_tokens")
        .filter(|value| !value.is_null())
        .or_else(|| object.get("max_tokens"))
        .and_then(Value::as_u64);
    if let Some(budget) = budget {
        object.remove("max_completion_tokens");
        object.insert(
            "max_tokens".into(),
            Value::from(budget.max(CHAT_OUTPUT_TOKEN_FLOOR)),
        );
    }
}

/// MiniMax enables thinking by default when the request omits a thinking
/// directive. Keep reasoning output structurally separated on Chat Completions,
/// while preserving every client-supplied effort, budget, or thinking declaration.
fn apply_reasoning_defaults(protocol: ProtocolId, body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };

    if protocol == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        // MiniMax documents `reasoning_split` as an output-format switch, not a
        // thinking on/off switch: `true` returns reasoning separately via
        // `reasoning_content` / `reasoning_details`; `false` embeds it in
        // `content` as `<think>...</think>`. Default to the split shape so an
        // OpenAI-compatible client never receives reasoning in normal content.
        object
            .entry("reasoning_split".to_string())
            .or_insert(Value::Bool(true));

        // MiniMax-M3 uses `thinking`, rather than `reasoning_split`, to
        // control whether thinking is emitted. Preserve an explicit thinking
        // block and explicit non-none effort; otherwise honor Nyro's
        // default-off policy. M2.x accepts this field but cannot disable
        // thinking, so `reasoning_split: true` still keeps it out of content.
        let explicit_non_none_effort = object
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| !effort.eq_ignore_ascii_case("none"));
        if !object.contains_key("thinking") && !explicit_non_none_effort {
            object.insert(
                "thinking".to_string(),
                serde_json::json!({"type": "disabled"}),
            );
        }
    } else if protocol == OPENAI_RESPONSES_V1 {
        match object.entry("reasoning".to_string()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(serde_json::json!({"effort": "none"}));
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                if let Some(reasoning) = entry.get_mut().as_object_mut() {
                    reasoning
                        .entry("effort".to_string())
                        .or_insert_with(|| Value::String("none".to_string()));
                }
            }
        }
    } else if protocol == ANTHROPIC_MESSAGES_2023_06_01
        && !object.contains_key("thinking")
        && object
            .get("output_config")
            .and_then(Value::as_object)
            .is_none_or(|config| !config.contains_key("effort"))
    {
        object.insert(
            "thinking".to_string(),
            serde_json::json!({"type": "disabled"}),
        );
    }
}

#[async_trait]
impl Vendor for MinimaxVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "minimax",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        openai_build_url(base_url, path)
    }
    fn vendor_id(&self) -> &'static str {
        "minimax"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
        };
        &[
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
        ]
    }
    fn declared_request_mutations(&self) -> bool {
        true
    }

    async fn post_encode(
        &self,
        ctx: &VendorCtx<'_>,
        body: &mut Value,
        _headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        apply_reasoning_defaults(ctx.protocol_id, body);
        Ok(())
    }
    fn declared_response_mutations(&self) -> bool {
        false
    }
    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        pipeline::build_request(self, req, ctx).await
    }
    async fn parse_response(
        &self,
        resp: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, resp, ctx).await
    }
    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("minimax", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(MinimaxVendor) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_output_floor_normalizes_aliases_and_preserves_larger_budgets() {
        for field in ["max_completion_tokens", "max_tokens"] {
            for budget in [0, 64, 262_143, 262_144, 524_288] {
                let mut body = serde_json::json!({field: budget, "reasoning_effort": "high"});
                apply_chat_output_token_floor(
                    &mut body,
                    Some("minimax"),
                    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                );
                assert_eq!(body["max_tokens"], budget.max(CHAT_OUTPUT_TOKEN_FLOOR));
                assert!(body.get("max_completion_tokens").is_none());
                assert_eq!(body["reasoning_effort"], "high");
                let once = body.clone();
                apply_chat_output_token_floor(
                    &mut body,
                    Some("minimax"),
                    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                );
                assert_eq!(body, once);
            }
        }
        for (alias, legacy, expected) in [
            (serde_json::json!(64), 524_288, 262_144),
            (serde_json::json!(524_288), 64, 524_288),
            (Value::Null, 64, 262_144),
        ] {
            let mut body = serde_json::json!({"max_completion_tokens":alias,"max_tokens":legacy});
            apply_chat_output_token_floor(
                &mut body,
                Some("minimax"),
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            );
            assert_eq!(body, serde_json::json!({"max_tokens":expected}));
        }
    }

    #[test]
    fn chat_output_floor_leaves_missing_invalid_and_other_scopes_unchanged() {
        for mut body in [
            serde_json::json!({}),
            Value::Null,
            serde_json::json!([]),
            serde_json::json!({"max_tokens":null}),
            serde_json::json!({"max_completion_tokens":null}),
            serde_json::json!({"max_completion_tokens":-1,"max_tokens":64}),
            serde_json::json!({"max_tokens":1.5}),
            serde_json::json!({"max_tokens":"64"}),
        ] {
            let expected = body.clone();
            apply_chat_output_token_floor(
                &mut body,
                Some("minimax"),
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            );
            assert_eq!(body, expected);
        }
        for (vendor, protocol) in [
            (None, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            (Some("custom"), OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            (Some("minimax"), OPENAI_RESPONSES_V1),
            (Some("minimax"), ANTHROPIC_MESSAGES_2023_06_01),
        ] {
            let mut body = serde_json::json!({"model":"MiniMax-M3","max_completion_tokens":64,"max_tokens":64});
            let expected = body.clone();
            apply_chat_output_token_floor(&mut body, vendor, protocol);
            assert_eq!(body, expected);
        }
    }

    #[test]
    fn openai_chat_defaults_to_split_reasoning_output() {
        let mut body = serde_json::json!({"model": "MiniMax-M3", "messages": []});

        apply_reasoning_defaults(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_split"], true);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn openai_chat_preserves_explicit_effort_and_splits_output() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M3",
            "messages": [],
            "reasoning_effort": "high"
        });

        apply_reasoning_defaults(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["reasoning_split"], true);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn openai_chat_preserves_explicit_reasoning_split() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M3",
            "messages": [],
            "reasoning_split": false
        });

        apply_reasoning_defaults(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_split"], false);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn openai_chat_preserves_explicit_thinking_control() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M3",
            "messages": [],
            "thinking": {"type": "disabled"}
        });

        apply_reasoning_defaults(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_split"], true);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn openai_responses_defaults_missing_effort_to_none() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "input": [],
            "reasoning": {"summary": "auto"}
        });

        apply_reasoning_defaults(OPENAI_RESPONSES_V1, &mut body);

        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn openai_responses_preserves_explicit_effort() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "input": [],
            "reasoning": {"effort": "max"}
        });

        apply_reasoning_defaults(OPENAI_RESPONSES_V1, &mut body);

        assert_eq!(body["reasoning"]["effort"], "max");
    }

    #[test]
    fn anthropic_defaults_missing_thinking_to_disabled() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "messages": [],
            "max_tokens": 1024
        });

        apply_reasoning_defaults(ANTHROPIC_MESSAGES_2023_06_01, &mut body);

        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_preserves_explicit_thinking_and_effort() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "messages": [],
            "max_tokens": 1024,
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "output_config": {"effort": "high"}
        });
        let expected = body.clone();

        apply_reasoning_defaults(ANTHROPIC_MESSAGES_2023_06_01, &mut body);

        assert_eq!(body, expected);
    }
}
