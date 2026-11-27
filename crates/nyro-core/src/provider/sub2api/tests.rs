use super::url::{endpoint_url, models_url, route_base, sub2api_root};
use super::*;
use crate::protocol::ids::Protocol;

fn provider(vendor: Option<&str>, preset: Option<&str>, channel: Option<&str>) -> Provider {
    Provider {
        keys: Vec::new(),
        id: "sub2api-test".into(),
        name: "Sub2API".into(),
        vendor: vendor.map(Into::into),
        protocol: "openai-compatible".into(),
        base_url: "https://s.example.com".into(),
        protocol_mode: "adaptive".into(),
        protocol_endpoints: Vec::new(),
        preset_key: preset.map(Into::into),
        channel: channel.map(Into::into),
        models_source: None,
        static_models: None,
        api_key: "sk-test".into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
        fast_mode: Default::default(),
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn sub2api(channel: &str) -> Provider {
    provider(Some(VENDOR_ID), Some(VENDOR_ID), Some(channel))
}

#[test]
fn root_strips_one_known_suffix_and_keeps_proxy_prefixes() {
    for (input, expected) in [
        ("https://s.example.com", "https://s.example.com"),
        ("https://s.example.com/", "https://s.example.com"),
        ("  https://s.example.com/v1  ", "https://s.example.com"),
        ("https://s.example.com/V1/", "https://s.example.com"),
        ("https://s.example.com/v1beta", "https://s.example.com"),
        ("https://s.example.com/antigravity", "https://s.example.com"),
        (
            "https://s.example.com/antigravity/v1",
            "https://s.example.com",
        ),
        (
            "https://s.example.com/antigravity/v1beta/",
            "https://s.example.com",
        ),
        (
            "https://s.example.com/backend-api/codex",
            "https://s.example.com",
        ),
        (
            "https://edge.example.com/sub2api/v1",
            "https://edge.example.com/sub2api",
        ),
        (
            "https://edge.example.com/sub2api",
            "https://edge.example.com/sub2api",
        ),
        // Only Sub2API's own suffixes are stripped.
        ("https://edge.example.com/v2", "https://edge.example.com/v2"),
        // A host named like a suffix is not a path segment.
        ("http://v1", "http://v1"),
        ("http://antigravity", "http://antigravity"),
        ("", ""),
    ] {
        assert_eq!(sub2api_root(input), expected, "input: {input:?}");
    }
}

#[test]
fn urls_follow_platform_route_prefixes() {
    let base = "https://s.example.com/v1";
    for platform in Sub2ApiPlatform::ALL {
        for (protocol, path) in [
            (Protocol::OpenAICompatible, "/v1/chat/completions"),
            (Protocol::OpenAIResponses, "/v1/responses"),
            (Protocol::AnthropicMessages, "/v1/messages"),
            (
                Protocol::GoogleGemini,
                "/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse",
            ),
        ] {
            let prefix = if platform == Sub2ApiPlatform::Antigravity
                && matches!(
                    protocol,
                    Protocol::AnthropicMessages | Protocol::GoogleGemini
                ) {
                "/antigravity"
            } else {
                ""
            };
            assert_eq!(
                endpoint_url(Some(platform), protocol, base, path),
                format!("https://s.example.com{prefix}{path}"),
                "{platform:?} {protocol:?}"
            );
        }
    }
    // A pasted Antigravity address never doubles the prefix.
    assert_eq!(
        endpoint_url(
            Some(Sub2ApiPlatform::Antigravity),
            Protocol::AnthropicMessages,
            "https://s.example.com/antigravity",
            "/v1/messages",
        ),
        "https://s.example.com/antigravity/v1/messages"
    );
    assert_eq!(
        route_base(
            Some(Sub2ApiPlatform::Antigravity),
            Protocol::GoogleGemini,
            "https://s.example.com/antigravity/v1beta"
        ),
        "https://s.example.com/antigravity"
    );
    assert_eq!(
        models_url(Some(Sub2ApiPlatform::Gemini), "https://s.example.com/v1"),
        Some("https://s.example.com/v1beta/models".into())
    );
    assert_eq!(
        models_url(
            Some(Sub2ApiPlatform::Antigravity),
            "https://s.example.com/antigravity"
        ),
        Some("https://s.example.com/v1/models".into())
    );
    assert_eq!(models_url(None, "  "), None);
}

#[test]
fn vendor_build_url_reads_the_platform_from_the_channel() {
    use crate::protocol::ids::{ANTHROPIC_MESSAGES_2023_06_01, OPENAI_RESPONSES_V1};
    let antigravity = sub2api("antigravity");
    let ctx = VendorCtx {
        provider: &antigravity,
        protocol_id: ANTHROPIC_MESSAGES_2023_06_01,
        api_key: "sk-test",
        actual_model: "claude-sonnet-4-6",
        credential: None,
    };
    assert_eq!(
        Sub2ApiVendor.build_url(&ctx, "https://s.example.com/v1", "/v1/messages"),
        "https://s.example.com/antigravity/v1/messages"
    );
    let openai = sub2api("openai");
    let ctx = VendorCtx {
        provider: &openai,
        protocol_id: OPENAI_RESPONSES_V1,
        api_key: "sk-test",
        actual_model: "gpt-5.6",
        credential: None,
    };
    assert_eq!(
        Sub2ApiVendor.build_url(&ctx, "https://s.example.com/v1", "/v1/responses"),
        "https://s.example.com/v1/responses"
    );
    assert_eq!(
        Sub2ApiVendor.auth_headers(&ctx)["authorization"],
        "Bearer sk-test"
    );
}

#[test]
fn metadata_declares_one_shared_key_bearer_channel_per_platform() {
    let ids: Vec<&str> = METADATA.channels.iter().map(|channel| channel.id).collect();
    let expected: Vec<&str> = Sub2ApiPlatform::ALL.iter().map(|p| p.id()).collect();
    assert_eq!(ids, expected);
    for channel in METADATA.channels {
        let platform = Sub2ApiPlatform::from_id(channel.id).expect("platform channel");
        let protocols: Vec<&str> = channel.base_urls.iter().map(|e| e.protocol).collect();
        assert!(channel.shared_key_protocols, "{}", channel.id);
        assert!(!channel.deprecated, "{}", channel.id);
        assert!(channel.notes.is_some(), "{}", channel.id);
        assert!(
            channel.base_urls.iter().all(|e| e.base_url.is_empty()),
            "{}: the address is user-supplied",
            channel.id
        );
        let schemes = channel.auth_schemes.expect("auth schemes");
        for protocol in &protocols {
            assert!(
                schemes
                    .iter()
                    .any(|s| s.protocol == *protocol && s.auth_scheme == "bearer"),
                "{}: {protocol} must use bearer",
                channel.id
            );
        }
        for optional in channel.optional_protocols {
            assert!(protocols.contains(optional), "{}: {optional}", channel.id);
        }
        let default = channel.default_protocol.expect("default protocol");
        assert!(protocols.contains(&default), "{}", channel.id);
        assert!(
            !channel.optional_protocols.contains(&default),
            "{}: the default protocol must be checked",
            channel.id
        );
        let relay = channel.relay.expect("relay addressing");
        assert_eq!(relay.models_path, platform.models_path(), "{}", channel.id);
        assert_eq!(relay.root_suffixes, url::ROOT_SUFFIXES);
        for (protocol, suite) in [
            (CHAT, Protocol::OpenAICompatible),
            (RESPONSES, Protocol::OpenAIResponses),
            (MESSAGES, Protocol::AnthropicMessages),
            (GEMINI, Protocol::GoogleGemini),
        ] {
            assert_eq!(
                relay.route_prefix(protocol),
                platform.route_prefix(suite),
                "{}: {protocol} route prefix",
                channel.id
            );
        }
    }
}

#[test]
fn metadata_serializes_relay_fields_for_the_webui() {
    let value = serde_json::to_value(METADATA).unwrap();
    let antigravity = value["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|channel| channel["id"] == "antigravity")
        .unwrap();
    assert_eq!(antigravity["defaultProtocol"], "anthropic-messages");
    assert_eq!(antigravity["relay"]["modelsPath"], "/v1/models");
    assert_eq!(
        antigravity["relay"]["routePrefixes"]["anthropic-messages"],
        "/antigravity"
    );
    assert_eq!(
        antigravity["relay"]["rootSuffixes"][0],
        "/antigravity/v1beta"
    );
    assert_eq!(
        antigravity["optionalProtocols"],
        serde_json::json!(["openai-compatible", "openai-responses"])
    );
    assert!(antigravity.get("deprecated").is_none());
    assert!(antigravity["notes"]["en"].is_string());
}

#[test]
fn platform_classification_covers_preset_and_legacy_channels() {
    assert_eq!(
        Sub2ApiPlatform::of(&sub2api("antigravity")),
        Some(Sub2ApiPlatform::Antigravity)
    );
    assert_eq!(
        Sub2ApiPlatform::of(&sub2api(" OpenCode_Go ")),
        Some(Sub2ApiPlatform::OpenCodeGo)
    );
    // Vendor or preset identifies the preset.
    assert_eq!(
        Sub2ApiPlatform::of_preset(&provider(None, Some("sub2api"), Some("grok"))),
        Some(Sub2ApiPlatform::Grok)
    );
    // TypeSafe and unknown ids are not supported platforms.
    assert_eq!(Sub2ApiPlatform::of(&sub2api("typesafe")), None);
    assert_eq!(Sub2ApiPlatform::of(&sub2api("default")), None);
    // The legacy OpenAI-preset channel (also seen under custom) relays the
    // OpenAI platform, but only of() includes it.
    for vendor in ["openai", "custom"] {
        let legacy = provider(Some(vendor), Some(vendor), Some("sub2api"));
        assert_eq!(Sub2ApiPlatform::of(&legacy), Some(Sub2ApiPlatform::OpenAI));
        assert_eq!(Sub2ApiPlatform::of_preset(&legacy), None);
        assert!(!is_sub2api(&legacy));
    }
    // Same channel names on other vendors are not Sub2API.
    for (vendor, channel) in [
        ("google", "antigravity"),
        ("openai", "openai"),
        ("xai", "grok"),
        ("nyro-gateway", "sub2api"),
    ] {
        assert_eq!(
            Sub2ApiPlatform::of(&provider(Some(vendor), Some(vendor), Some(channel))),
            None,
            "{vendor}/{channel}"
        );
    }
}

#[test]
fn dialects_map_platforms_to_wire_policies() {
    use Sub2ApiPlatform::*;
    for (platform, dialect) in [
        (Anthropic, Some("anthropic")),
        (OpenAI, Some("openai")),
        (Gemini, Some("google")),
        (Antigravity, None),
        (Grok, Some("xai")),
        (Kimi, Some("moonshotai")),
        (Zhipu, Some("zhipuai")),
        (DeepSeek, Some("deepseek")),
        (MiniMax, Some("minimax")),
        (OpenCodeGo, Some("opencode-go")),
        (CommandCode, None),
        (Cline, None),
        (Composite, None),
    ] {
        assert_eq!(platform.dialect_vendor(), dialect, "{platform:?}");
        assert_eq!(platform.codex_consumer(), platform == OpenAI);
        assert_eq!(platform.transparent(), platform == Composite);
    }
}

#[test]
fn sub2api_channels_never_match_vendor_channel_predicates() {
    // Platform ids reuse channel names of other vendors (`antigravity`,
    // `grok`, `openai`); identity predicates must stay vendor-qualified so a
    // relay never picks up OAuth envelopes, cloaking or native transports.
    use crate::provider::{anthropic, devin, google, kiro, nyro_gateway, opencode_go, vertexai};
    for platform in Sub2ApiPlatform::ALL {
        let p = sub2api(platform.id());
        let id = platform.id();
        assert!(!google::antigravity::is_google_antigravity(&p), "{id}");
        assert!(!google::gemini_cli::is_google_gemini_cli(&p), "{id}");
        assert!(!google::is_subscription_channel(&p), "{id}");
        assert!(!google::antigravity::forces_upstream_stream(&p), "{id}");
        assert!(
            !anthropic::claude_code::egress::is_claude_code_channel(&p),
            "{id}"
        );
        assert!(!opencode_go::session::is_opencode_go(&p), "{id}");
        assert!(!nyro_gateway::is_nyro_gateway(&p), "{id}");
        assert!(!devin::is_devin(&p), "{id}");
        assert!(!kiro::is_kiro(&p), "{id}");
        assert!(!vertexai::is_vertex_vendor(&p), "{id}");
    }
}
