//! Per-vendor `reasoning_effort` egress policy.
//!
//! Different upstreams disagree on which effort values they accept (measured
//! against the live production matrix on 2026-08-22):
//!
//! | Upstream            | `none` | `disable`/`disabled` | Policy              |
//! |---------------------|:------:|:----------------------:|---------------------|
//! | GLM (zhipuai)       |   ✅   |          ❌ 400         | normalize → `none`  |
//! | DeepSeek v4         |   ✅   |          ❌ 400         | normalize → `none`  |
//! | grok (x.ai build)   | ❌ 400 |          ✅ ignored    | drop the field      |
//! | MiniMax             |   ✅   |     ✅ honored off     | normalize → `none`  |
//! | Kimi coding         |   ✅   |       ✅ ignored       | normalize → `none`  |
//! | OpenAI / sub2api    |   ✅   |       ✅ ignored       | normalize → `none`  |
//! | OpenCode zen (go)   | ❌ 400 |       n/a (→none)      | clamp off → `low`   |
//!
//! OpenCode zen 的思考型模型（如 glm-5.3）拒绝 `none`：
//! `[1210] This model always engages in thinking and cannot be disabled;
//! please use low, high, or max`（实测 2026-08-25，Console Go 上游）。off
//! 意图只能降级到最小合法档 `low`。
//!
//! Ecosystem convention (also seen in the ported cc-switch transforms) treats
//! `disable` / `disabled` / `off` as misspellings of "turn reasoning off".
//! The IR decoder cannot fix this alone: mapping them to
//! `ReasoningEffort::None` would emit `"none"`, which grok rejects. The
//! normalization therefore happens per-vendor at the wire boundary, keeping
//! each vendor's dialect explicit.

use serde_json::Value;

/// Normalize a raw `reasoning_effort` wire value for an upstream that accepts
/// the seven-value OpenAI-style enum (`none..max`).
///
/// Misspellings of "off" (`disable` / `disabled` / `off`, any case) are
/// rewritten to `none`; any other value is forwarded verbatim so genuinely new
/// upstream-side values still pass through.
fn normalize_to_openai_enum(effort: &str) -> &str {
    if matches!(
        effort.trim().to_ascii_lowercase().as_str(),
        "disable" | "disabled" | "off"
    ) {
        "none"
    } else {
        effort
    }
}

/// GLM / DeepSeek / MiniMax / Kimi: strict or lenient seven-value enum.
/// Off-misspellings become `none`; unknown-but-plausible values pass through.
pub(crate) fn normalize_enum_effort(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(Value::String(effort)) = object.get("reasoning_effort") {
        let normalized = normalize_to_openai_enum(effort).to_string();
        if normalized != *effort {
            object.insert("reasoning_effort".to_string(), Value::String(normalized));
        }
    }
}

/// grok-4.6-build rejects `reasoning_effort: "none"` outright and silently
/// ignores every other value. The only safe "off" representation is to remove
/// the directive entirely and let the upstream default apply.
///
/// Handles both wire shapes the IR egress can produce:
/// - chat-completions: top-level `reasoning_effort`
/// - responses: nested `reasoning.effort` (empty `reasoning` removed)
pub(crate) fn drop_grok_effort(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let is_off_misspelling = object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(is_off_effort);
    if is_off_misspelling {
        object.remove("reasoning_effort");
    }

    // Responses wire shape: nested reasoning.effort. Preserve sibling keys
    // (summary, etc.); remove the reasoning object entirely once empty.
    let nested_off = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .is_some_and(is_off_effort);
    if nested_off
        && let Some(reasoning) = object.get_mut("reasoning")
        && let Some(reasoning_obj) = reasoning.as_object_mut()
    {
        reasoning_obj.remove("effort");
        if reasoning_obj.is_empty() {
            object.remove("reasoning");
        }
    }
}

/// OpenCode zen (opencode-go)：思考型模型不接受任何 off 形态（`none` 也
/// 400）。off 意图钳制为最小合法档 `low`，其余值原样透传。
pub(crate) fn clamp_opencode_effort(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(Value::String(effort)) = object.get("reasoning_effort")
        && is_off_effort(effort)
    {
        object.insert(
            "reasoning_effort".to_string(),
            Value::String("low".to_string()),
        );
    }
}

fn is_off_effort(effort: &str) -> bool {
    matches!(
        effort.trim().to_ascii_lowercase().as_str(),
        "none" | "disable" | "disabled" | "off"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_upstream_rewrites_off_misspellings_to_none() {
        for raw in ["disable", "disabled", "off", "DISABLE", "Disabled"] {
            let mut body = json!({"reasoning_effort": raw});
            normalize_enum_effort(&mut body);
            assert_eq!(body["reasoning_effort"], "none", "raw={raw}");
        }
    }

    #[test]
    fn enum_upstream_preserves_valid_and_unknown_values() {
        for raw in ["none", "low", "xhigh", "max", "future-value"] {
            let mut body = json!({"reasoning_effort": raw});
            normalize_enum_effort(&mut body);
            assert_eq!(body["reasoning_effort"], raw);
        }
    }

    #[test]
    fn enum_upstream_ignores_non_string_or_missing_effort() {
        let mut body = json!({"reasoning_effort": 3, "model": "m"});
        normalize_enum_effort(&mut body);
        assert_eq!(body["reasoning_effort"], 3);

        let mut bare = json!({"model": "m"});
        normalize_enum_effort(&mut bare);
        assert!(bare.get("reasoning_effort").is_none());
    }

    #[test]
    fn opencode_clamps_off_values_to_low() {
        // zen 上游 400 [1210]：思考型模型连 none 都不接受，off 降级为 low。
        for raw in ["none", "disable", "disabled", "off", "Disable", "OFF"] {
            let mut body = json!({"reasoning_effort": raw, "model": "glm-5.3"});
            clamp_opencode_effort(&mut body);
            assert_eq!(body["reasoning_effort"], "low", "raw={raw} clamps to low");
            assert_eq!(body["model"], "glm-5.3");
        }
    }

    #[test]
    fn opencode_keeps_real_effort_levels() {
        for raw in ["low", "medium", "high", "max", "future"] {
            let mut body = json!({"reasoning_effort": raw});
            clamp_opencode_effort(&mut body);
            assert_eq!(body["reasoning_effort"], raw, "raw={raw} passes through");
        }

        let mut bare = json!({"model": "m"});
        clamp_opencode_effort(&mut bare);
        assert!(bare.get("reasoning_effort").is_none());
    }

    #[test]
    fn grok_drops_every_off_spelling() {
        for raw in ["none", "disable", "disabled", "off", "NONE"] {
            let mut body = json!({"reasoning_effort": raw, "model": "grok-4.6"});
            drop_grok_effort(&mut body);
            assert!(
                body.get("reasoning_effort").is_none(),
                "raw={raw} must be dropped"
            );
            assert_eq!(body["model"], "grok-4.6");
        }
    }

    #[test]
    fn grok_keeps_real_effort_levels() {
        for raw in ["low", "high", "max"] {
            let mut body = json!({"reasoning_effort": raw});
            drop_grok_effort(&mut body);
            assert_eq!(body["reasoning_effort"], raw);
        }
    }

    #[test]
    fn grok_drops_nested_responses_effort_and_empty_reasoning() {
        for raw in ["none", "disable", "disabled"] {
            let mut body = json!({"reasoning": {"effort": raw}});
            drop_grok_effort(&mut body);
            assert!(body.get("reasoning").is_none(), "raw={raw}");
        }
    }

    #[test]
    fn grok_keeps_reasoning_siblings_when_dropping_nested_effort() {
        let mut body = json!({"reasoning": {"effort": "none", "summary": "auto"}});
        drop_grok_effort(&mut body);
        assert!(body.pointer("/reasoning/effort").is_none());
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn grok_keeps_real_nested_effort() {
        let mut body = json!({"reasoning": {"effort": "high"}});
        drop_grok_effort(&mut body);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn grok_ignores_missing_effort() {
        let mut body = json!({"model": "grok-4.6"});
        drop_grok_effort(&mut body);
        assert_eq!(body["model"], "grok-4.6");
    }
}
