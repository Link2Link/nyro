//! Per-vendor `reasoning_effort` egress policy.
//!
//! Different upstreams disagree on which effort values they accept (measured
//! against the live production matrix on 2026-08-22):
//!
//! | Upstream            | `none` | `disable`/`disabled` | Policy              |
//! |---------------------|:------:|:----------------------:|---------------------|
//! | GLM (zhipuai)       |   ✅   |          ❌ 400         | normalize → `none`  |
//! | DeepSeek v4         |   ✅   |          ❌ 400         | normalize → `none`  |
//! | grok (x.ai build)   | ❌ 400 |          ✅ ignored    | drop off; `max`→`xhigh` |
//! | MiniMax             |   ✅   |     ✅ honored off     | normalize → `none`  |
//! | Kimi coding         |   ✅   |       ✅ ignored       | normalize → `none`  |
//! | OpenAI / sub2api    |   ✅   |       ✅ ignored       | normalize → `none`  |
//! | GPT-6 Astra         | ❌ 400 |       n/a (→none)      | clamp off → `low`   |
//! | OpenCode zen (go)   | ❌ 400 |       n/a (→none)      | clamp off → `low`   |
//! | Volcengine Ark glm  | ❌ 400 |       n/a (→none)      | clamp off → `low`   |
//!
//! OpenCode zen 的思考型模型（如 glm-5.3）拒绝 `none`：
//! `[1210] This model always engages in thinking and cannot be disabled;
//! please use low, high, or max`（实测 2026-08-25，Console Go 上游）。off
//! 意图只能降级到最小合法档 `low`。
//!
//! 火山引擎 Ark coding 端点的 glm-5.3 同样拒收 `none`：`InvalidParameter:
//! reasoning_effort `none` is not supported by this model`（实测
//! 2026-08-26，请求 58e799fa，ark.cn-beijing.volces.com）。该钳制分支必须
//! 排在 normalize 之前：normalize 会把 misspelling `disable` 归一成
//! `none`，对这个 (vendor, model) 组合恰恰是致死值。
//!
//! 智谱官方模型文档（2026-08 抓取）进一步把 off 拒收下沉到模型本体：
//! glm-5.3 与 glm-5.3-flash 均「始终启用思考功能」，thinking.type 仅支持
//! enabled；迁移提示对旧用法 disabled 明言「否则，请求将失败」并指引改设
//! reasoning_effort: low（reasoning_effort 枚举同时收窄为 low/high/max）。
//! 这类模型按 THINKING_MANDATORY_MODELS 登记表在模型级钳制、跨上游生效，
//! 其余 GLM 模型维持上表的 normalize 方言。
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

/// grok rejects `reasoning_effort: "none"` outright (400) and rejects
/// `"max"` the same way (400 `Invalid reasoning effort.`, live request
/// 65fffc9a, cli-chat-proxy `/v1/responses`, 2026-08-26 — 4/4 repro). The
/// only safe "off" representation is to remove the directive entirely; the
/// only safe "max" representation is a downgrade to `xhigh`, grok's top
/// accepted tier.
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
    } else if object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(is_max_effort)
    {
        object.insert(
            "reasoning_effort".to_string(),
            Value::String("xhigh".to_string()),
        );
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
    } else if let Some(reasoning) = object.get_mut("reasoning")
        && reasoning
            .get("effort")
            .and_then(Value::as_str)
            .is_some_and(is_max_effort)
        && let Some(reasoning_obj) = reasoning.as_object_mut()
    {
        reasoning_obj.insert("effort".to_string(), Value::String("xhigh".to_string()));
    }
}

/// grok 顶格档位是 `xhigh`；`max` 不在其枚举内（400 invalid-argument）。
fn is_max_effort(effort: &str) -> bool {
    effort.trim().eq_ignore_ascii_case("max")
}

/// Off 意图（`none` 及其 misspelling）钳制为最小合法档 `low` 的共享内核。
/// 供「思考型模型拒收一切 off 形态」的上游方言复用。
/// Handles Chat and Responses without narrowing other tiers: GPT-6 Astra
/// accepts medium/xhigh as well as low/high/max (request bcd2a3ed).
pub(crate) fn clamp_off_effort_to_low(body: &mut Value) {
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
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut)
        && reasoning
            .get("effort")
            .and_then(Value::as_str)
            .is_some_and(is_off_effort)
    {
        reasoning.insert("effort".to_string(), Value::String("low".to_string()));
    }
}

/// OpenCode zen (opencode-go)：思考型模型不接受任何 off 形态（`none` 也
/// 400）。off 意图钳制为最小合法档 `low`，其余值原样透传。
pub(crate) fn clamp_opencode_effort(body: &mut Value) {
    clamp_off_effort_to_low(body);
}

/// 火山引擎 Ark coding（ark-coding / volces.com）：glm-5.3 拒收 `none`
/// （400 `InvalidParameter: reasoning_effort "none" is not supported by
/// this model`，实测 2026-08-26，请求 58e799fa）。与 OpenCode zen 同病同
/// 方：off 意图（含 misspelling `disable`/`disabled`/`off`）钳制为 `low`。
pub(crate) fn clamp_volcengine_ark_effort(body: &mut Value) {
    clamp_off_effort_to_low(body);
}

/// 官方三档（low/high/max）之外的档位窄化：minimal 向下取最近合法档
/// `low`；medium/xhigh 按「向保推理质量一侧就近、持平取高档」收拢——
/// medium 归入 `high` 而非 `low`，xhigh 与 high/max 等距取 `high`。
/// 返回 None 表示无需改写。
fn narrow_to_doc_tiers(effort: &str) -> Option<&'static str> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some("low"),
        "medium" | "xhigh" => Some("high"),
        _ => None,
    }
}

/// 思考强制开启的模型（登记表见 pipeline 模块 THINKING_MANDATORY_MODELS，
/// 成员 glm-5.3 / glm-5.3-flash）：官方文档明确 thinking.type 仅支持
/// enabled、不支持关闭思考，off 意图「请求将失败」，且 reasoning_effort
/// 枚举收窄为 low/high/max（docs.bigmodel.cn 模型页，2026-08 抓取）。该
/// 约束属于模型本体而非某一上游方言，因此按模型名匹配、跨上游生效：
/// - off 意图（none 及 misspelling）钳制为最小合法档 low；
/// - 三档之外的已知档位（minimal/medium/xhigh）按 [`narrow_to_doc_tiers`]
///   单调向下窄化；未知值不认识就不动，交由上游裁决。
///
/// 处理两种 wire 形态：chat-completions 顶层 reasoning_effort，以及
/// OpenAI Responses 直通的嵌套 reasoning.effort（bigmodel 官方提供
/// Responses 接入端点；保留 summary 等兄弟键，仅改写档位本身）。
pub(crate) fn clamp_thinking_mandatory_effort(body: &mut Value) {
    /// off 意图 → 最小合法档；三档外已知档位窄化；其余返回 None 不动。
    fn target_for(raw: &str) -> Option<&'static str> {
        if is_off_effort(raw) {
            Some("low")
        } else {
            narrow_to_doc_tiers(raw)
        }
    }

    let Some(object) = body.as_object_mut() else {
        return;
    };

    // chat-completions 形态：顶层 reasoning_effort。
    let top_raw = object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(raw) = top_raw
        && let Some(target) = target_for(&raw)
    {
        object.insert(
            "reasoning_effort".to_string(),
            Value::String(target.to_string()),
        );
    }

    // OpenAI Responses 直通形态：嵌套 reasoning.effort（summary 等兄弟键
    // 原样保留，仅改写档位本身）。
    let nested_raw = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(raw) = nested_raw
        && let Some(target) = target_for(&raw)
        && let Some(reasoning) = object.get_mut("reasoning")
        && let Some(reasoning_obj) = reasoning.as_object_mut()
    {
        reasoning_obj.insert("effort".to_string(), Value::String(target.to_string()));
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
    fn off_clamp_handles_both_wire_shapes_without_narrowing_valid_tiers() {
        for raw in ["none", "disable", "disabled", "off", " NONE ", "OFF"] {
            let mut body = json!({
                "reasoning_effort": raw,
                "reasoning": {"effort": raw, "summary": "auto"}
            });
            clamp_off_effort_to_low(&mut body);
            assert_eq!(body["reasoning_effort"], "low", "raw={raw}");
            assert_eq!(body["reasoning"]["effort"], "low", "raw={raw}");
            assert_eq!(body["reasoning"]["summary"], "auto");
            let once = body.clone();
            clamp_off_effort_to_low(&mut body);
            assert_eq!(body, once, "clamp must be idempotent");
        }
        for raw in ["low", "medium", "high", "xhigh", "max", "future-value"] {
            let mut body = json!({"reasoning_effort": raw, "reasoning": {"effort": raw}});
            let original = body.clone();
            clamp_off_effort_to_low(&mut body);
            assert_eq!(body, original, "raw={raw} must pass through");
        }
        for mut body in [
            json!(null),
            json!([]),
            json!({}),
            json!({"reasoning": {"summary": "auto"}}),
            json!({"reasoning_effort": null, "reasoning": {"effort": 3}}),
            json!({"reasoning": "none"}),
        ] {
            let original = body.clone();
            clamp_off_effort_to_low(&mut body);
            assert_eq!(
                body, original,
                "missing/malformed directives stay untouched"
            );
        }
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
    fn ark_clamps_off_values_to_low() {
        // ark 上游 400 InvalidParameter：glm-5.3 拒收 none；线上事故里客户端
        // 发的是 misspelling "disable"（请求 58e799fa），因此 off 全形态都
        // 必须钳住。
        for raw in ["none", "disable", "disabled", "off", "Disable", "OFF"] {
            let mut body = json!({"reasoning_effort": raw, "model": "glm-5.3"});
            clamp_volcengine_ark_effort(&mut body);
            assert_eq!(body["reasoning_effort"], "low", "raw={raw} clamps to low");
            assert_eq!(body["model"], "glm-5.3");
        }
    }

    #[test]
    fn ark_keeps_real_effort_levels() {
        for raw in ["low", "medium", "high", "max", "future"] {
            let mut body = json!({"reasoning_effort": raw});
            clamp_volcengine_ark_effort(&mut body);
            assert_eq!(body["reasoning_effort"], raw, "raw={raw} passes through");
        }

        let mut bare = json!({"model": "glm-5.3"});
        clamp_volcengine_ark_effort(&mut bare);
        assert!(bare.get("reasoning_effort").is_none());
    }

    #[test]
    fn thinking_mandatory_clamps_both_wire_shapes() {
        // chat-completions 形态：顶层 reasoning_effort，off 全形态钳 low。
        for raw in ["none", "disable", "disabled", "off", "OFF"] {
            let mut body = json!({"reasoning_effort": raw, "model": "glm-5.3"});
            clamp_thinking_mandatory_effort(&mut body);
            assert_eq!(body["reasoning_effort"], "low", "raw={raw} clamps to low");
        }

        // OpenAI Responses 直通形态：嵌套 reasoning.effort；summary 等
        // 兄弟键保留，仅钳制档位本身。
        for raw in ["none", "disable"] {
            let mut body = json!({"reasoning": {"effort": raw, "summary": "auto"}});
            clamp_thinking_mandatory_effort(&mut body);
            assert_eq!(
                body["reasoning"]["effort"], "low",
                "nested raw={raw} clamps"
            );
            assert_eq!(body["reasoning"]["summary"], "auto", "siblings preserved");
        }

        // 合法三档原样透传（嵌套形态同样不动）。
        for raw in ["low", "high", "max"] {
            let mut body = json!({"reasoning_effort": raw});
            clamp_thinking_mandatory_effort(&mut body);
            assert_eq!(body["reasoning_effort"], raw, "tier raw={raw} passes");
            let mut nested = json!({"reasoning": {"effort": raw}});
            clamp_thinking_mandatory_effort(&mut nested);
            assert_eq!(nested["reasoning"]["effort"], raw);
        }

        // 官方三档之外的已知档位窄化：minimal→low；medium/xhigh 向保推理
        // 质量一侧收拢——medium 归 high，xhigh 与 high/max 等距取 high。
        assert_eq!(narrow_to_doc_tiers("minimal"), Some("low"));
        assert_eq!(narrow_to_doc_tiers("medium"), Some("high"));
        assert_eq!(narrow_to_doc_tiers("xhigh"), Some("high"));
        let mut body = json!({"reasoning_effort": "XHIGH", "model": "glm-5.3"});
        clamp_thinking_mandatory_effort(&mut body);
        assert_eq!(
            body["reasoning_effort"], "high",
            "case-insensitive narrowing"
        );

        // 未知值不认识就不动，交由上游裁决。
        let mut unknown = json!({"reasoning_effort": "future-value"});
        clamp_thinking_mandatory_effort(&mut unknown);
        assert_eq!(unknown["reasoning_effort"], "future-value");

        // 无 effort 指令时不凭空造字段。
        let mut bare = json!({"model": "m"});
        clamp_thinking_mandatory_effort(&mut bare);
        assert!(bare.get("reasoning").is_none());
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
        for raw in ["low", "medium", "high", "xhigh", "future-value"] {
            let mut body = json!({"reasoning_effort": raw});
            drop_grok_effort(&mut body);
            assert_eq!(body["reasoning_effort"], raw);
        }
    }

    #[test]
    fn grok_downgrades_max_to_xhigh() {
        // 线上事故 65fffc9a：cli-chat-proxy /v1/responses 对 max 直接 400
        // `Invalid reasoning effort.`（4/4 复现）。max 降级为 grok 顶格档
        // xhigh，两种 wire shape 都要覆盖。
        for raw in ["max", "MAX", "Max"] {
            let mut body = json!({"reasoning_effort": raw, "model": "grok-4.6"});
            drop_grok_effort(&mut body);
            assert_eq!(body["reasoning_effort"], "xhigh", "raw={raw}");
            assert_eq!(body["model"], "grok-4.6");
        }
    }

    #[test]
    fn grok_downgrades_nested_max_to_xhigh() {
        let mut body = json!({"reasoning": {"effort": "max", "summary": "auto"}});
        drop_grok_effort(&mut body);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto", "siblings preserved");
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
        let mut body = json!({"reasoning": {"effort": "xhigh"}});
        drop_grok_effort(&mut body);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn grok_ignores_missing_effort() {
        let mut body = json!({"model": "grok-4.6"});
        drop_grok_effort(&mut body);
        assert_eq!(body["model"], "grok-4.6");
    }
}
