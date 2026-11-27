//! Sub2API group platforms. A Sub2API API key belongs to one group and the
//! group's platform decides which gateway pipeline serves it; each platform
//! is one nyro channel whose id equals Sub2API's platform id.
//!
//! The platform drives only *client-side* decisions: which Sub2API route a
//! protocol uses and which wire dialect the final upstream speaks. Account
//! concerns (OAuth, envelopes, signatures, billing) stay inside Sub2API.

use crate::db::models::Provider;
use crate::protocol::ids::Protocol;

/// One Sub2API group platform (TypeSafe's System One protocol is not a chat
/// protocol and is intentionally absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sub2ApiPlatform {
    Anthropic,
    OpenAI,
    Gemini,
    Antigravity,
    Grok,
    Kimi,
    Zhipu,
    DeepSeek,
    MiniMax,
    OpenCodeGo,
    CommandCode,
    Cline,
    Composite,
}

impl Sub2ApiPlatform {
    /// Every supported platform, in WebUI display order.
    pub const ALL: [Self; 13] = [
        Self::Anthropic,
        Self::OpenAI,
        Self::Gemini,
        Self::Antigravity,
        Self::Grok,
        Self::Kimi,
        Self::Zhipu,
        Self::DeepSeek,
        Self::MiniMax,
        Self::OpenCodeGo,
        Self::CommandCode,
        Self::Cline,
        Self::Composite,
    ];

    /// Sub2API's platform id, also the nyro channel id.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::Grok => "grok",
            Self::Kimi => "kimi",
            Self::Zhipu => "zhipu",
            Self::DeepSeek => "deepseek",
            Self::MiniMax => "minimax",
            Self::OpenCodeGo => "opencode_go",
            Self::CommandCode => "command_code",
            Self::Cline => "cline",
            Self::Composite => "composite",
        }
    }

    /// Parse a channel / platform id (trimmed, case-insensitive).
    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::ALL
            .into_iter()
            .find(|platform| platform.id().eq_ignore_ascii_case(id))
    }

    /// Platform of a provider created from the Sub2API preset.
    pub fn of_preset(provider: &Provider) -> Option<Self> {
        if !super::is_sub2api(provider) {
            return None;
        }
        provider.channel.as_deref().and_then(Self::from_id)
    }

    /// [`Self::of_preset`] plus the legacy `sub2api` channel of the OpenAI
    /// preset (historical rows also carry it under `custom`), which relays
    /// Sub2API's OpenAI platform.
    pub fn of(provider: &Provider) -> Option<Self> {
        Self::of_preset(provider).or_else(|| is_legacy_channel(provider).then_some(Self::OpenAI))
    }

    /// Extra route prefix Sub2API expects for `protocol` on this platform.
    /// Antigravity's Messages and Gemini routes live under `/antigravity`
    /// (`ForcePlatform(antigravity)`: Antigravity accounts only); `/v1beta`
    /// without the prefix rejects non-gemini groups. Chat / Responses have no
    /// prefixed route and follow the group platform from the root.
    pub fn route_prefix(self, protocol: Protocol) -> &'static str {
        match (self, protocol) {
            (Self::Antigravity, Protocol::AnthropicMessages | Protocol::GoogleGemini) => {
                "/antigravity"
            }
            _ => "",
        }
    }

    /// nyro vendor id whose *wire dialect* the final upstream speaks, for
    /// pipeline-level wire policies only. `None` when Sub2API owns the
    /// translation (Antigravity's v1internal) or the upstream varies per
    /// model (aggregators, composite), leaving model-name rules in charge.
    pub const fn dialect_vendor(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("anthropic"),
            Self::OpenAI => Some("openai"),
            Self::Gemini => Some("google"),
            Self::Grok => Some("xai"),
            Self::Kimi => Some("moonshotai"),
            Self::Zhipu => Some("zhipuai"),
            Self::DeepSeek => Some("deepseek"),
            Self::MiniMax => Some("minimax"),
            Self::OpenCodeGo => Some("opencode-go"),
            Self::Antigravity | Self::CommandCode | Self::Cline | Self::Composite => None,
        }
    }

    /// Sub2API's OpenAI platform forwards to the same ChatGPT Codex consumer
    /// backend as the codex channel, so the Codex egress contract applies.
    pub const fn codex_consumer(self) -> bool {
        matches!(self, Self::OpenAI)
    }

    /// Composite groups resolve each model to a platform inside Sub2API, so
    /// nyro cannot know the final vendor and relays protocols untouched.
    pub const fn transparent(self) -> bool {
        matches!(self, Self::Composite)
    }

    /// Model discovery path. Gemini groups list the upstream catalog on
    /// `/v1beta/models`, which is more complete than the static
    /// `/v1/models` fallback when accounts declare no mapping.
    pub const fn models_path(self) -> &'static str {
        match self {
            Self::Gemini => "/v1beta/models",
            _ => "/v1/models",
        }
    }
}

/// The legacy `openai/sub2api` channel (any vendor except the Nyro relay,
/// whose channel ids never collide but whose identity must stay its own).
fn is_legacy_channel(provider: &Provider) -> bool {
    !super::is_sub2api(provider)
        && !crate::provider::nyro_gateway::is_nyro_gateway(provider)
        && provider
            .channel
            .as_deref()
            .is_some_and(|channel| channel.trim().eq_ignore_ascii_case("sub2api"))
}
