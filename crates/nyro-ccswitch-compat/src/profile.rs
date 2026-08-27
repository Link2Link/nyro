use crate::CodexChatReasoningConfig;

/// Infer cc-switch's Chat reasoning configuration from provider/model hints.
/// An explicit declaration always wins; platform identity wins over model name.
pub fn resolve_chat_reasoning_config(
    explicit: Option<CodexChatReasoningConfig>,
    provider_name: &str,
    base_url: &str,
    model: &str,
) -> Option<CodexChatReasoningConfig> {
    if let Some(mut config) = explicit {
        if config.supports_effort.unwrap_or(false) && config.supports_thinking.is_none() {
            config.supports_thinking = Some(true);
        }
        return Some(config);
    }
    let platform = format!("{} {}", provider_name, base_url).to_ascii_lowercase();
    if platform.contains("openrouter") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(false),
            supports_effort: Some(true),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning.effort".to_string()),
            effort_value_mode: Some("openrouter".to_string()),
            output_format: Some("auto".to_string()),
        });
    }
    if platform.contains("siliconflow") {
        return Some(thinking_config(
            "enable_thinking",
            false,
            "reasoning_content",
        ));
    }
    let haystack = format!("{platform} {}", model.to_ascii_lowercase());
    if haystack.contains("deepseek") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("deepseek".to_string()),
            output_format: Some("reasoning_content".to_string()),
        });
    }
    if haystack.contains("stepfun") || haystack.contains("step-3.5-flash-2603") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(model.to_ascii_lowercase().contains("2603")),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("low_high".to_string()),
            output_format: Some("reasoning".to_string()),
        });
    }
    if haystack.contains("qwen") || haystack.contains("dashscope") || haystack.contains("bailian") {
        return Some(thinking_config(
            "enable_thinking",
            false,
            "reasoning_content",
        ));
    }
    if haystack.contains("minimax") {
        return Some(thinking_config(
            "reasoning_split",
            false,
            "reasoning_details",
        ));
    }
    // 智谱 GLM 系（glm / zhipu / z.ai 同源同 API）：官方 chat 端点接受
    // `thinking: {type: "enabled"}` 与 `reasoning_effort` 组合，档位枚举
    // low/high/max（glm-5.3 / glm-5.3-flash 迁移文档，docs.bigmodel.cn，
    // 2026-08 抓取；旧模型 glm-4.x 亦接受 reasoning_effort 七值枚举——见
    // nyro-core effort_policy 方言表的 GLM 行）。档位窄化走 "glm" 模式。
    if haystack.contains("glm") || haystack.contains("zhipu") || haystack.contains("z.ai") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("glm".to_string()),
            output_format: Some("reasoning_content".to_string()),
        });
    }
    if haystack.contains("kimi") || haystack.contains("moonshot") || haystack.contains("mimo") {
        return Some(thinking_config("thinking", false, "reasoning_content"));
    }
    None
}

fn thinking_config(
    thinking_param: &str,
    supports_effort: bool,
    output_format: &str,
) -> CodexChatReasoningConfig {
    CodexChatReasoningConfig {
        supports_thinking: Some(true),
        supports_effort: Some(supports_effort),
        thinking_param: Some(thinking_param.to_string()),
        effort_param: Some(if supports_effort {
            "reasoning_effort".to_string()
        } else {
            "none".to_string()
        }),
        effort_value_mode: None,
        output_format: Some(output_format.to_string()),
    }
}

/// JSON/SSE wire protocol on either side of a conversion session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireProtocol {
    AnthropicMessages,
    OpenAiChat,
    OpenAiResponses,
    GeminiNative,
}

/// Client-side semantic contract. Both variants can use Responses JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientSemantics {
    Anthropic,
    CodexResponses,
}

/// Upstream wire behavior that is not expressible by protocol alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamFlavor {
    StandardChat,
    StandardResponses,
    CodexOAuthResponses,
    XaiStrictResponses,
    /// Strict third-party Responses upstream (GLM, DeepSeek, …): same
    /// protocol both sides, but Codex Responses-Lite artifacts (the
    /// `additional_tools` carrier, `namespace`/`custom` tool shapes) must be
    /// normalized to public Responses shapes on egress and restored on
    /// ingress.
    ThirdPartyStrictResponses,
    Anthropic,
    Gemini,
}

/// Every production conversion direction defined by the compatibility layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    AnthropicToChat,
    AnthropicToResponses,
    AnthropicToGemini,
    AnthropicToAnthropic,
    CodexResponsesToChat,
    CodexResponsesToAnthropic,
    XaiResponsesNative,
}

/// Upstream identity used to gate cc-switch's Anthropic-side request
/// normalizations (thinking-history replay for DeepSeek/MiMo-compatible
/// upstreams and DeepSeek-official effort stripping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicNormalizationHints {
    pub model: String,
    pub base_url: String,
    pub vendor: String,
}

/// Deterministic request/response conversion configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionProfile {
    pub direction: Direction,
    pub client_protocol: WireProtocol,
    pub upstream_protocol: WireProtocol,
    pub client_semantics: ClientSemantics,
    pub upstream_flavor: UpstreamFlavor,
    pub client_stream: bool,
    pub model: Option<String>,
    pub provider_id: Option<String>,
    pub cache_key: Option<String>,
    pub codex_fast_mode: bool,
    pub preserve_chat_reasoning_content: bool,
    pub codex_anthropic_default_max_tokens: u64,
    pub chat_reasoning: Option<CodexChatReasoningConfig>,
    pub prompt_cache_key_supported: bool,
    /// Anthropic→Anthropic passthrough with cc-switch's provider-keyed request
    /// normalizations applied (DeepSeek/MiMo thinking history, DeepSeek
    /// official effort stripping).
    pub anthropic_normalization: Option<AnthropicNormalizationHints>,
    /// Codex→Anthropic only: prepend the Claude Code identity as the first
    /// system block so "Claude Code only" gateways accept the request.
    /// Off by default, mirroring cc-switch's opt-in toggle.
    pub impersonate_claude_code: bool,
}

impl ConversionProfile {
    pub fn anthropic_to_chat(client_stream: bool) -> Self {
        Self::new(
            Direction::AnthropicToChat,
            WireProtocol::AnthropicMessages,
            WireProtocol::OpenAiChat,
            ClientSemantics::Anthropic,
            UpstreamFlavor::StandardChat,
            client_stream,
        )
    }

    pub fn anthropic_to_responses(client_stream: bool, flavor: UpstreamFlavor) -> Self {
        debug_assert!(matches!(
            flavor,
            UpstreamFlavor::StandardResponses
                | UpstreamFlavor::CodexOAuthResponses
                | UpstreamFlavor::XaiStrictResponses
        ));
        Self::new(
            Direction::AnthropicToResponses,
            WireProtocol::AnthropicMessages,
            WireProtocol::OpenAiResponses,
            ClientSemantics::Anthropic,
            flavor,
            client_stream,
        )
    }

    pub fn anthropic_to_gemini(client_stream: bool) -> Self {
        Self::new(
            Direction::AnthropicToGemini,
            WireProtocol::AnthropicMessages,
            WireProtocol::GeminiNative,
            ClientSemantics::Anthropic,
            UpstreamFlavor::Gemini,
            client_stream,
        )
    }

    /// Anthropic→Anthropic passthrough with provider-keyed request
    /// normalizations. The wire format never changes; only the narrow
    /// DeepSeek/MiMo body fixups from cc-switch are applied.
    pub fn anthropic_passthrough_normalized(client_stream: bool) -> Self {
        Self::new(
            Direction::AnthropicToAnthropic,
            WireProtocol::AnthropicMessages,
            WireProtocol::AnthropicMessages,
            ClientSemantics::Anthropic,
            UpstreamFlavor::Anthropic,
            client_stream,
        )
    }

    pub fn codex_responses_to_chat(client_stream: bool) -> Self {
        Self::new(
            Direction::CodexResponsesToChat,
            WireProtocol::OpenAiResponses,
            WireProtocol::OpenAiChat,
            ClientSemantics::CodexResponses,
            UpstreamFlavor::StandardChat,
            client_stream,
        )
    }

    pub fn codex_responses_to_anthropic(client_stream: bool) -> Self {
        Self::new(
            Direction::CodexResponsesToAnthropic,
            WireProtocol::OpenAiResponses,
            WireProtocol::AnthropicMessages,
            ClientSemantics::CodexResponses,
            UpstreamFlavor::Anthropic,
            client_stream,
        )
    }

    pub fn xai_responses_native(client_stream: bool) -> Self {
        Self::new(
            Direction::XaiResponsesNative,
            WireProtocol::OpenAiResponses,
            WireProtocol::OpenAiResponses,
            ClientSemantics::CodexResponses,
            UpstreamFlavor::XaiStrictResponses,
            client_stream,
        )
    }

    /// Codex Responses → strict third-party Responses upstream (GLM,
    /// DeepSeek, …). Same wire protocol, but Codex Responses-Lite artifacts
    /// are normalized on egress and restored on ingress.
    pub fn third_party_responses_native(client_stream: bool) -> Self {
        Self::new(
            Direction::XaiResponsesNative,
            WireProtocol::OpenAiResponses,
            WireProtocol::OpenAiResponses,
            ClientSemantics::CodexResponses,
            UpstreamFlavor::ThirdPartyStrictResponses,
            client_stream,
        )
    }

    fn new(
        direction: Direction,
        client_protocol: WireProtocol,
        upstream_protocol: WireProtocol,
        client_semantics: ClientSemantics,
        upstream_flavor: UpstreamFlavor,
        client_stream: bool,
    ) -> Self {
        Self {
            direction,
            client_protocol,
            upstream_protocol,
            client_semantics,
            upstream_flavor,
            client_stream,
            model: None,
            provider_id: None,
            cache_key: None,
            codex_fast_mode: false,
            preserve_chat_reasoning_content: false,
            codex_anthropic_default_max_tokens: 8192,
            chat_reasoning: None,
            prompt_cache_key_supported: false,
            anthropic_normalization: None,
            impersonate_claude_code: false,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
    }

    pub fn with_cache_key(mut self, cache_key: impl Into<String>) -> Self {
        self.cache_key = Some(cache_key.into());
        self
    }

    pub fn with_chat_reasoning(mut self, config: CodexChatReasoningConfig) -> Self {
        self.chat_reasoning = Some(config);
        self
    }

    pub fn with_prompt_cache_key_support(mut self, supported: bool) -> Self {
        self.prompt_cache_key_supported = supported;
        self
    }

    pub fn with_anthropic_normalization(
        mut self,
        model: impl Into<String>,
        base_url: impl Into<String>,
        vendor: impl Into<String>,
    ) -> Self {
        self.anthropic_normalization = Some(AnthropicNormalizationHints {
            model: model.into(),
            base_url: base_url.into(),
            vendor: vendor.into(),
        });
        self
    }

    pub fn with_impersonate_claude_code(mut self) -> Self {
        self.impersonate_claude_code = true;
        self
    }

    pub fn force_upstream_stream(&self) -> bool {
        self.client_stream || matches!(self.upstream_flavor, UpstreamFlavor::CodexOAuthResponses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_oauth_always_forces_upstream_stream() {
        let profile =
            ConversionProfile::anthropic_to_responses(false, UpstreamFlavor::CodexOAuthResponses);
        assert!(profile.force_upstream_stream());
    }

    #[test]
    fn ordinary_buffered_profile_does_not_force_stream() {
        assert!(!ConversionProfile::anthropic_to_chat(false).force_upstream_stream());
    }

    #[test]
    fn test_resolve_codex_chat_reasoning_infers_deepseek_effort_support() {
        let config = resolve_chat_reasoning_config(
            None,
            "DeepSeek",
            "https://api.deepseek.com",
            "deepseek-v4-pro",
        )
        .unwrap();
        assert_eq!(config.supports_thinking, Some(true));
        assert_eq!(config.supports_effort, Some(true));
        assert_eq!(config.effort_value_mode.as_deref(), Some("deepseek"));
    }

    #[test]
    fn test_resolve_codex_chat_reasoning_explicit_meta_overrides_inference() {
        let explicit = CodexChatReasoningConfig {
            supports_thinking: Some(false),
            supports_effort: Some(false),
            thinking_param: Some("none".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("auto".to_string()),
        };
        let config = resolve_chat_reasoning_config(
            Some(explicit),
            "DeepSeek",
            "https://api.deepseek.com",
            "deepseek-v4-pro",
        )
        .unwrap();
        assert_eq!(config.supports_thinking, Some(false));
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.thinking_param.as_deref(), Some("none"));
    }

    #[test]
    fn test_resolve_codex_chat_reasoning_openrouter_platform_overrides_model() {
        let config = resolve_chat_reasoning_config(
            None,
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "deepseek/deepseek-chat-v3.1",
        )
        .unwrap();
        assert_eq!(config.thinking_param.as_deref(), Some("none"));
        assert_eq!(config.effort_param.as_deref(), Some("reasoning.effort"));
        assert_eq!(config.effort_value_mode.as_deref(), Some("openrouter"));
        assert_eq!(config.supports_effort, Some(true));
    }

    #[test]
    fn test_resolve_codex_chat_reasoning_siliconflow_platform_overrides_minimax() {
        let config = resolve_chat_reasoning_config(
            None,
            "SiliconFlow",
            "https://api.siliconflow.cn/v1",
            "MiniMaxAI/MiniMax-M2.7",
        )
        .unwrap();
        assert_eq!(config.thinking_param.as_deref(), Some("enable_thinking"));
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.output_format.as_deref(), Some("reasoning_content"));
    }

    /// GLM 系按官方文档推断为「thinking 开关 + reasoning_effort 档位」双支持
    /// （此前与 kimi/mimo 合并在 thinking-only 分支，档位被静默丢弃）。
    #[test]
    fn test_resolve_codex_chat_reasoning_infers_glm_effort_support() {
        for (provider, base_url, model) in [
            (
                "GLM",
                "https://open.bigmodel.cn/api/coding/paas/v4",
                "glm-5.3-flash",
            ),
            ("zhipuai", "https://open.bigmodel.cn/api/paas/v4", "glm-4.6"),
            ("Z.ai", "https://api.z.ai/api/paas/v4", "glm-5.3"),
        ] {
            let config = resolve_chat_reasoning_config(None, provider, base_url, model).unwrap();
            assert_eq!(config.supports_thinking, Some(true), "{provider}");
            assert_eq!(config.supports_effort, Some(true), "{provider}");
            assert_eq!(config.thinking_param.as_deref(), Some("thinking"));
            assert_eq!(config.effort_param.as_deref(), Some("reasoning_effort"));
            assert_eq!(config.effort_value_mode.as_deref(), Some("glm"));
            assert_eq!(config.output_format.as_deref(), Some("reasoning_content"));
        }
    }

    /// kimi / mimo 留在 thinking-only 分支（effort 支持未按文档核实）。
    #[test]
    fn test_resolve_codex_chat_reasoning_keeps_kimi_thinking_only() {
        let config = resolve_chat_reasoning_config(
            None,
            "Moonshot",
            "https://api.moonshot.cn/v1",
            "kimi-k2.6",
        )
        .unwrap();
        assert_eq!(config.thinking_param.as_deref(), Some("thinking"));
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.effort_param.as_deref(), Some("none"));
    }
}
