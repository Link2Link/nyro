//! Anthropic-side request normalizations ported from cc-switch's
//! `proxy/providers/claude.rs`.
//!
//! Two narrow behaviors, both keyed off provider identity rather than the
//! conversion direction:
//!
//! 1. **Tool-call thinking history** — DeepSeek's Anthropic-compatible endpoint
//!    requires every assistant turn containing `tool_use` to replay a plain
//!    `thinking` block. Anthropic SDK clients keep tool history but may drop or
//!    redact the thinking block, which the upstream rejects. Only providers
//!    known to require this (`REASONING_VENDOR_HINTS`) are touched; Kimi
//!    deliberately stays out of the list (2026-08 feedback: injecting
//!    placeholders there corrupts the chain of thought).
//! 2. **DeepSeek official effort stripping** — DeepSeek's official endpoint
//!    treats `thinking: {type: "disabled"}` and effort parameters as mutually
//!    exclusive (HTTP 400). Claude Code intentionally disables thinking for
//!    sub-agents, so the conflicting effort fields are removed instead.

use serde_json::Value;

/// Vendors whose Anthropic-compatible endpoints require thinking replay on
/// tool-call turns and accept `reasoning_content` on Chat conversions.
pub(crate) const REASONING_VENDOR_HINTS: &[&str] = &["deepseek", "mimo", "xiaomimimo"];

/// DeepSeek official Anthropic-compatible endpoint URL.
pub(crate) const DEEPSEEK_OFFICIAL_ANTHROPIC_URL: &str = "https://api.deepseek.com/anthropic";

/// Placeholder thinking text injected when history lacks a thinking block.
pub(crate) const ANTHROPIC_THINKING_PLACEHOLDER: &str = "tool call";

/// Placeholder text replacing `redacted_thinking` blocks on replay.
pub(crate) const ANTHROPIC_REDACTED_THINKING_PLACEHOLDER: &str = "[redacted thinking]";

/// Claude Code client identity (used for Codex→Anthropic emulation to pass a
/// gateway's "Claude Code only" check).
pub(crate) const CLAUDE_CODE_SYSTEM_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

pub(crate) fn is_reasoning_vendor_identifier(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    REASONING_VENDOR_HINTS
        .iter()
        .any(|hint| value.contains(hint))
}

/// Whether an Anthropic→Anthropic upstream needs cc-switch's request
/// normalizations, judged from the vendor id, upstream base URL, or model
/// name. DeepSeek's official URL is covered by the `deepseek` hint.
pub fn anthropic_normalization_needed(vendor: &str, base_url: &str, model: &str) -> bool {
    is_reasoning_vendor_identifier(vendor)
        || is_reasoning_vendor_identifier(base_url)
        || is_reasoning_vendor_identifier(model)
}

/// Whether the upstream is DeepSeek's official Anthropic-compatible endpoint.
/// `base_url` comes from the provider's normalized upstream URL.
pub(crate) fn is_deepseek_official_anthropic_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == DEEPSEEK_OFFICIAL_ANTHROPIC_URL
}

/// Gate for thinking-history normalization: applies only when the model, the
/// upstream URL, or the declared vendor belongs to a reasoning vendor.
pub(crate) fn should_normalize_anthropic_tool_thinking_history(
    model: &str,
    base_url: &str,
    vendor: &str,
) -> bool {
    is_reasoning_vendor_identifier(model)
        || is_reasoning_vendor_identifier(base_url)
        || is_reasoning_vendor_identifier(vendor)
}

/// DeepSeek's official endpoint treats `thinking: {type: "disabled"}` and
/// effort parameters (`output_config.effort` / `reasoning_effort`) as mutually
/// exclusive, returning HTTP 400. Claude Code's intentional `thinking:
/// disabled` for sub-agents is respected; the conflicting effort parameters
/// are removed instead. See <https://github.com/deepseek-ai/DeepSeek-V3/issues/1397>.
pub(crate) fn normalize_deepseek_thinking_disabled_strip_effort(
    body: &mut Value,
    base_url: &str,
) -> bool {
    if !is_deepseek_official_anthropic_endpoint(base_url) {
        return false;
    }

    let thinking_type = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str());

    if thinking_type != Some("disabled") {
        return false;
    }

    let mut changed = false;

    // Remove output_config.effort (Anthropic format)
    if let Some(oc) = body
        .get_mut("output_config")
        .and_then(|v| v.as_object_mut())
    {
        changed |= oc.remove("effort").is_some();
        // Clean up empty output_config
        if oc.is_empty() {
            body.as_object_mut()
                .map(|root| root.remove("output_config"));
        }
    }

    // Remove reasoning_effort (OpenAI format, may be present in passthrough)
    if body.get("reasoning_effort").is_some() {
        body.as_object_mut()
            .map(|root| root.remove("reasoning_effort"));
        changed = true;
    }

    changed
}

/// Normalize only the narrow tool-call history shape for providers known to
/// require plain `thinking` blocks.
pub(crate) fn normalize_anthropic_tool_thinking_history_for_provider(
    body: &mut Value,
    model: &str,
    base_url: &str,
    vendor: &str,
) -> bool {
    if !should_normalize_anthropic_tool_thinking_history(model, base_url, vendor) {
        return false;
    }

    normalize_anthropic_tool_thinking_history(body)
}

/// Entry point for Anthropic-format upstreams: applies both normalizations.
pub(crate) fn normalize_anthropic_messages_for_provider(
    body: &mut Value,
    model: &str,
    base_url: &str,
    vendor: &str,
) -> bool {
    let mut changed =
        normalize_anthropic_tool_thinking_history_for_provider(body, model, base_url, vendor);
    changed |= normalize_deepseek_thinking_disabled_strip_effort(body, base_url);
    changed
}

fn normalize_anthropic_tool_thinking_history(body: &mut Value) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };

    let mut changed = false;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }

        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if !content
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        {
            continue;
        }

        let mut has_thinking = false;
        for block in content.iter_mut() {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    let has_non_empty_thinking = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty());
                    if let Some(obj) = block.as_object_mut() {
                        if obj.remove("signature").is_some() {
                            changed = true;
                        }
                        if !has_non_empty_thinking {
                            obj.insert(
                                "thinking".to_string(),
                                Value::String(ANTHROPIC_THINKING_PLACEHOLDER.to_string()),
                            );
                            changed = true;
                        }
                    }
                    has_thinking = true;
                }
                Some("redacted_thinking") => {
                    *block = serde_json::json!({
                        "type": "thinking",
                        "thinking": ANTHROPIC_REDACTED_THINKING_PLACEHOLDER
                    });
                    has_thinking = true;
                    changed = true;
                }
                _ => {}
            }
        }

        if !has_thinking {
            content.insert(
                0,
                serde_json::json!({
                    "type": "thinking",
                    "thinking": ANTHROPIC_THINKING_PLACEHOLDER
                }),
            );
            changed = true;
        }
    }

    changed
}

/// Insert the Claude Code identity as the first block of the Anthropic request
/// `system` field. Anthropic subscription/OAuth gateways require the first
/// system block to be exactly this identity line. After conversion `system`
/// may be a string (from Codex instructions); normalize it into an array:
/// `[identity line, original system...]`. Idempotent.
pub(crate) fn prepend_claude_code_system_prompt(body: &mut Value) {
    let identity = serde_json::json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM_IDENTITY });
    let mut blocks: Vec<Value> = vec![identity];
    match body.get("system") {
        Some(Value::String(existing)) if !existing.is_empty() => {
            blocks.push(serde_json::json!({ "type": "text", "text": existing }));
        }
        Some(Value::Array(existing)) => {
            // Idempotent: skip re-injection if the first block is already the identity line.
            if existing
                .first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                == Some(CLAUDE_CODE_SYSTEM_IDENTITY)
            {
                return;
            }
            blocks.extend(existing.iter().cloned());
        }
        _ => {}
    }
    body["system"] = Value::Array(blocks);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DEEPSEEK_ANTHROPIC_URL: &str = "https://api.deepseek.com/anthropic";

    fn deepseek_official_base_url() -> &'static str {
        DEEPSEEK_ANTHROPIC_URL
    }

    #[test]
    fn test_anthropic_messages_no_longer_hoists_system_role_messages() {
        // role=system messages are left in `messages[]` (DeepSeek's endpoint
        // accepts them natively) and the top-level `system` field is untouched,
        // preserving the request prefix.
        let mut body = json!({
            "system": "Existing top-level system.",
            "model": "deepseek-v4-pro",
            "messages": [
                { "role": "system", "content": "Message system one." },
                { "role": "user", "content": "hello" },
                {
                    "role": "system",
                    "content": [{ "type": "text", "text": "Message system two." }]
                }
            ]
        });

        let changed = normalize_anthropic_messages_for_provider(
            &mut body,
            "deepseek-v4-pro",
            DEEPSEEK_ANTHROPIC_URL,
            "",
        );

        assert!(!changed);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "system");
        assert_eq!(body["system"], "Existing top-level system.");
    }

    #[test]
    fn test_anthropic_system_role_messages_skip_non_anthropic_format() {
        // Non-DeepSeek upstream: the pipeline must not touch anything.
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [
                { "role": "system", "content": "Keep in messages." },
                { "role": "user", "content": "hello" }
            ]
        });

        let changed = normalize_anthropic_messages_for_provider(
            &mut body,
            "deepseek-v4-pro",
            "https://api.deepseek.com/v1",
            "",
        );

        assert!(!changed);
        assert!(body.get("system").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn test_deepseek_anthropic_tool_history_injects_missing_thinking() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "I will inspect the repo."},
                    {"type": "tool_use", "id": "call_123", "name": "read_file", "input": {"path": "README.md"}}
                ]
            }]
        });

        let changed = normalize_anthropic_tool_thinking_history_for_provider(
            &mut body,
            "deepseek-v4-pro",
            DEEPSEEK_ANTHROPIC_URL,
            "",
        );

        assert!(changed);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], ANTHROPIC_THINKING_PLACEHOLDER);
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
    }

    #[test]
    fn test_deepseek_anthropic_tool_history_keeps_thinking_text_but_drops_signature() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Need to inspect the file.", "signature": "anthropic-signature"},
                    {"type": "tool_use", "id": "call_123", "name": "read_file", "input": {"path": "README.md"}}
                ]
            }]
        });

        let changed = normalize_anthropic_tool_thinking_history_for_provider(
            &mut body,
            "deepseek-v4-pro",
            DEEPSEEK_ANTHROPIC_URL,
            "",
        );

        assert!(changed);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "Need to inspect the file.");
        assert!(content[0].get("signature").is_none());
    }

    #[test]
    fn test_deepseek_anthropic_tool_history_rewrites_redacted_thinking() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "tool_use", "id": "call_123", "name": "read_file", "input": {"path": "README.md"}}
                ]
            }]
        });

        let changed = normalize_anthropic_tool_thinking_history_for_provider(
            &mut body,
            "deepseek-v4-pro",
            DEEPSEEK_ANTHROPIC_URL,
            "",
        );

        assert!(changed);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(
            content[0]["thinking"],
            ANTHROPIC_REDACTED_THINKING_PLACEHOLDER
        );
        assert!(content[0].get("data").is_none());
    }

    #[test]
    fn test_deepseek_official_detected_via_base_url_fallback() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "reasoning_effort": "high",
            "max_tokens": 100000
        });

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            "https://api.deepseek.com/anthropic",
        );

        assert!(changed);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_deepseek_official_no_effort_no_change() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "max_tokens": 100000
        });
        let original = body.clone();

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            deepseek_official_base_url(),
        );

        assert!(!changed);
        assert_eq!(body, original);
    }

    #[test]
    fn test_deepseek_official_non_disabled_not_modified() {
        let cases = vec![
            (
                "enabled",
                json!({ "type": "enabled", "budget_tokens": 16000 }),
            ),
            ("adaptive", json!({ "type": "adaptive" })),
        ];

        for (label, thinking_value) in cases {
            let mut body = json!({
                "model": "deepseek-v4-pro",
                "thinking": thinking_value,
                "output_config": { "effort": "max" },
                "max_tokens": 100000
            });
            let original = body.clone();

            let changed = normalize_deepseek_thinking_disabled_strip_effort(
                &mut body,
                deepseek_official_base_url(),
            );

            assert!(!changed, "should not modify thinking.type={label}");
            assert_eq!(body, original);
        }

        // missing thinking field entirely
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "output_config": { "effort": "max" },
            "max_tokens": 100000
        });
        let original = body.clone();
        assert!(!normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            deepseek_official_base_url()
        ));
        assert_eq!(body, original);
    }

    #[test]
    fn test_deepseek_official_preserves_output_config_other_fields() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "output_config": { "effort": "max", "temperature": 0.5 },
            "max_tokens": 100000
        });

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            deepseek_official_base_url(),
        );

        assert!(changed);
        assert_eq!(body["output_config"]["temperature"], 0.5);
        assert!(body["output_config"].get("effort").is_none());
    }

    #[test]
    fn test_deepseek_official_strips_both_effort_fields() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "output_config": { "effort": "max" },
            "reasoning_effort": "high",
            "max_tokens": 100000
        });

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            deepseek_official_base_url(),
        );

        assert!(changed);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_deepseek_official_strips_output_config_effort() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "output_config": { "effort": "max" },
            "max_tokens": 100000
        });

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            deepseek_official_base_url(),
        );

        assert!(changed);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn test_deepseek_official_strips_reasoning_effort() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "reasoning_effort": "high",
            "max_tokens": 100000
        });

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            deepseek_official_base_url(),
        );

        assert!(changed);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_deepseek_official_url_with_trailing_slash() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "output_config": { "effort": "max" },
            "max_tokens": 100000
        });

        let changed = normalize_deepseek_thinking_disabled_strip_effort(
            &mut body,
            "https://api.deepseek.com/anthropic/",
        );

        assert!(changed);
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn test_generic_anthropic_tool_history_is_not_modified() {
        let mut body = json!({
            "model": "claude-sonnet-4.6",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_123", "name": "read_file", "input": {"path": "README.md"}}
                ]
            }]
        });
        let original = body.clone();

        let changed = normalize_anthropic_tool_thinking_history_for_provider(
            &mut body,
            "claude-sonnet-4.6",
            "https://api.example.com/anthropic",
            "",
        );

        assert!(!changed);
        assert_eq!(body, original);
    }

    #[test]
    fn test_kimi_anthropic_tool_history_not_modified() {
        // Kimi 2026-08 feedback: its Anthropic-compatible endpoint no longer
        // requires thinking replay on tool_use turns; injecting placeholders
        // corrupts the chain of thought. Kimi gets generic passthrough.
        let mut body = json!({
            "model": "kimi-for-coding",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_123", "name": "read_file", "input": {"path": "README.md"}}
                ]
            }]
        });
        let original = body.clone();

        let changed = normalize_anthropic_tool_thinking_history_for_provider(
            &mut body,
            "kimi-for-coding",
            "https://api.kimi.com/coding",
            "",
        );

        assert!(!changed);
        assert_eq!(body, original);
    }

    #[test]
    fn test_non_deepseek_endpoint_not_modified() {
        let base_urls = [
            "https://other-api.com/anthropic",
            "https://api.anthropic.com",
        ];

        for base_url in base_urls {
            let mut body = json!({
                "model": "deepseek-v4-pro",
                "thinking": { "type": "disabled" },
                "output_config": { "effort": "max" },
                "max_tokens": 100000
            });
            let original = body.clone();

            let changed = normalize_deepseek_thinking_disabled_strip_effort(&mut body, base_url);

            assert!(!changed, "should not modify for {base_url}");
            assert_eq!(body, original);
        }
    }

    #[test]
    fn test_normalize_messages_pipeline_strips_effort_for_deepseek() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "thinking": { "type": "disabled" },
            "output_config": { "effort": "max" },
            "max_tokens": 100000,
            "messages": [{ "role": "user", "content": "hello" }]
        });

        let changed = normalize_anthropic_messages_for_provider(
            &mut body,
            "deepseek-v4-pro",
            deepseek_official_base_url(),
            "",
        );

        assert!(changed);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn test_model_name_alone_triggers_tool_history_normalization() {
        // cc-switch also matches on the model name when the base URL carries
        // no vendor hint (e.g. a relay fronting DeepSeek).
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_123", "name": "read_file", "input": {"path": "README.md"}}
                ]
            }]
        });

        let changed = normalize_anthropic_tool_thinking_history_for_provider(
            &mut body,
            "deepseek-v4-pro",
            "https://relay.example.com/anthropic",
            "",
        );

        assert!(changed);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
    }

    #[test]
    fn prepend_claude_code_system_prompt_from_string() {
        let mut body = json!({ "system": "You are a Codex agent." });
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
        assert_eq!(system[1]["text"], "You are a Codex agent.");
    }

    #[test]
    fn prepend_claude_code_system_prompt_is_idempotent() {
        let mut body = json!({ "system": "orig" });
        prepend_claude_code_system_prompt(&mut body);
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
        assert_eq!(system[1]["text"], "orig");
    }

    #[test]
    fn prepend_claude_code_system_prompt_when_absent() {
        let mut body = json!({});
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
    }
}
