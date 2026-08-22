//! Nyro-specific reasoning policy layered around the mechanically ported transforms.
//!
//! The files under `ported/` retain the pinned cc-switch behavior. Nyro deliberately
//! accepts a broader set of third-party reasoning models and preserves provider-specific
//! effort values verbatim; capture those overrides before the ported request transform
//! consumes the Anthropic body, then reapply them to the converted wire shape.

use serde_json::{Value, json};

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ReasoningOverrides {
    openai_effort: Option<String>,
    gemini_thinking_config: Option<Value>,
    /// grok 上游拒收 reasoning.effort=none 且忽略其他值;对 grok 模型
    /// 最安全的 off 表示是不带 effort(由模型变体名控制推理)。
    is_grok_model: bool,
}

impl ReasoningOverrides {
    pub(crate) fn capture(body: &Value) -> Self {
        let openai_effort = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| supports_reasoning_effort(model))
            .and_then(|_| resolve_reasoning_effort(body))
            .map(str::to_string);

        let is_grok_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .starts_with("grok-");

        Self {
            openai_effort,
            gemini_thinking_config: gemini_thinking_config(body),
            is_grok_model,
        }
    }

    pub(crate) fn apply_chat(&self, converted: &mut Value) {
        if self.is_grok_model {
            drop_grok_effort_chat(converted);
            return;
        }
        if let Some(effort) = self.openai_effort.as_ref() {
            converted["reasoning_effort"] = Value::String(effort.clone());
        }
    }

    pub(crate) fn apply_responses(&self, converted: &mut Value) {
        if self.is_grok_model {
            drop_grok_effort_responses(converted);
            return;
        }
        let Some(effort) = self.openai_effort.as_ref() else {
            return;
        };
        let Some(object) = converted.as_object_mut() else {
            return;
        };
        let reasoning = object
            .entry("reasoning".to_string())
            .or_insert_with(|| json!({}));
        if !reasoning.is_object() {
            *reasoning = json!({});
        }
        reasoning["effort"] = Value::String(effort.clone());
    }

    pub(crate) fn apply_gemini(&self, converted: &mut Value) {
        let Some(config) = self.gemini_thinking_config.as_ref() else {
            return;
        };
        let Some(object) = converted.as_object_mut() else {
            return;
        };
        let generation = object
            .entry("generationConfig".to_string())
            .or_insert_with(|| json!({}));
        if !generation.is_object() {
            *generation = json!({});
        }
        generation["thinkingConfig"] = config.clone();
    }
}

fn supports_reasoning_effort(model: &str) -> bool {
    let normalized = model.to_lowercase();
    !(normalized.starts_with("gpt-3.5")
        || normalized.starts_with("gpt-4o")
        || normalized.starts_with("gpt-4-turbo")
        || normalized.starts_with("gpt-4.")
        || normalized == "gpt-4"
        || normalized.starts_with("chatgpt-")
        || normalized.starts_with("claude-")
        || normalized.starts_with("gemini-"))
}

fn resolve_reasoning_effort(body: &Value) -> Option<&str> {
    if let Some(effort) = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(effort);
    }

    let thinking = body.get("thinking")?;
    match thinking.get("type").and_then(Value::as_str) {
        Some("adaptive") => Some("max"),
        Some("enabled") => match thinking.get("budget_tokens").and_then(Value::as_u64) {
            Some(budget) if budget < 4_000 => Some("low"),
            Some(budget) if budget < 16_000 => Some("medium"),
            Some(_) | None => Some("high"),
        },
        _ => None,
    }
}

fn gemini_thinking_config(body: &Value) -> Option<Value> {
    let disabled = body
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "disabled");
    if disabled {
        return Some(json!({"thinkingBudget": 0}));
    }

    let budget = body
        .pointer("/thinking/budget_tokens")
        .and_then(Value::as_u64);
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !is_gemini_3_series(model) {
        return budget.map(|tokens| json!({"thinkingBudget": tokens}));
    }

    let level = resolve_reasoning_effort(body).map(|effort| match effort {
        "low" => "low",
        "medium" => "medium",
        _ => "high",
    });

    match (level, budget) {
        (Some(level), Some(budget)) => Some(json!({
            "thinkingBudget": budget,
            "thinkingLevel": level
        })),
        (Some(level), None) => Some(json!({"thinkingLevel": level})),
        (None, Some(budget)) => Some(json!({"thinkingBudget": budget})),
        (None, None) => None,
    }
}

fn is_gemini_3_series(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    normalized.starts_with("gemini-3")
        || normalized
            .rsplit('/')
            .next()
            .is_some_and(|tail| tail.starts_with("gemini-3"))
}

fn is_off_effort(effort: &str) -> bool {
    matches!(
        effort.trim().to_ascii_lowercase().as_str(),
        "none" | "disable" | "disabled" | "off"
    )
}

/// grok chat 线格式: off 写法 → 删除顶层 reasoning_effort。
fn drop_grok_effort_chat(converted: &mut Value) {
    let is_off = converted
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(is_off_effort);
    if is_off && let Some(object) = converted.as_object_mut() {
        object.remove("reasoning_effort");
    }
}

/// grok Responses 线格式: off 写法 → 删除嵌套 reasoning.effort
/// (保留 reasoning 对象的其他键,如 encrypted_content include)。
fn drop_grok_effort_responses(converted: &mut Value) {
    let is_off = converted
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .is_some_and(is_off_effort);
    if is_off
        && let Some(reasoning) = converted.get_mut("reasoning")
        && let Some(object) = reasoning.as_object_mut()
    {
        object.remove("effort");
        if object.is_empty() {
            if let Some(top) = converted.as_object_mut() {
                top.remove("reasoning");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_explicit_and_third_party_effort_values() {
        let max = ReasoningOverrides::capture(&json!({
            "model": "gpt-5.4",
            "output_config": {"effort": "max"}
        }));
        let mut chat = json!({});
        max.apply_chat(&mut chat);
        assert_eq!(chat["reasoning_effort"], "max");

        let custom = ReasoningOverrides::capture(&json!({
            "model": "glm-5.3",
            "output_config": {"effort": "extreme"}
        }));
        let mut responses = json!({});
        custom.apply_responses(&mut responses);
        assert_eq!(responses["reasoning"]["effort"], "extreme");
    }

    #[test]
    fn derives_reasoning_for_third_party_models_but_rejects_known_non_reasoning_models() {
        let third_party = ReasoningOverrides::capture(&json!({
            "model": "deepseek-v4-flash",
            "thinking": {"type": "enabled", "budget_tokens": 8000}
        }));
        let mut converted = json!({});
        third_party.apply_chat(&mut converted);
        assert_eq!(converted["reasoning_effort"], "medium");

        let rejected = ReasoningOverrides::capture(&json!({
            "model": "gpt-4o-mini",
            "thinking": {"type": "adaptive"}
        }));
        let mut converted = json!({});
        rejected.apply_chat(&mut converted);
        assert!(converted.get("reasoning_effort").is_none());
    }

    #[test]
    fn grok_responses_path_drops_off_effort_but_keeps_other_reasoning_keys() {
        let overrides = ReasoningOverrides::capture(&json!({
            "model": "grok-4.6-build",
            "output_config": {"effort": "none"}
        }));
        let mut converted = json!({
            "model": "grok-4.6-build",
            "reasoning": {"effort": "none", "summary": "auto"}
        });
        overrides.apply_responses(&mut converted);
        assert!(
            converted.pointer("/reasoning/effort").is_none(),
            "grok off effort must be dropped on the Responses wire"
        );
        assert_eq!(
            converted["reasoning"]["summary"], "auto",
            "other reasoning keys must survive"
        );
    }

    #[test]
    fn grok_responses_path_removes_empty_reasoning_object() {
        let overrides = ReasoningOverrides::capture(&json!({
            "model": "grok-4.5",
            "output_config": {"effort": "disable"}
        }));
        let mut converted = json!({
            "model": "grok-4.5",
            "reasoning": {"effort": "disable"}
        });
        overrides.apply_responses(&mut converted);
        assert!(
            converted.get("reasoning").is_none(),
            "empty reasoning object must be removed entirely"
        );
    }

    #[test]
    fn grok_chat_path_drops_top_level_off_effort() {
        let overrides = ReasoningOverrides::capture(&json!({
            "model": "grok-4.6",
            "reasoning": {"type": "disabled"}
        }));
        let mut converted = json!({
            "model": "grok-4.6",
            "reasoning_effort": "none"
        });
        overrides.apply_chat(&mut converted);
        assert!(converted.get("reasoning_effort").is_none());
    }

    #[test]
    fn grok_keeps_real_effort_levels_on_both_wire_shapes() {
        let overrides = ReasoningOverrides::capture(&json!({
            "model": "grok-4.6",
            "output_config": {"effort": "high"}
        }));
        let mut chat = json!({"reasoning_effort": "high"});
        overrides.apply_chat(&mut chat);
        assert_eq!(chat["reasoning_effort"], "high");

        let mut responses = json!({"reasoning": {"effort": "high"}});
        overrides.apply_responses(&mut responses);
        assert_eq!(responses["reasoning"]["effort"], "high");
    }

    #[test]
    fn preserves_gemini_three_custom_effort_as_top_tier() {
        let overrides = ReasoningOverrides::capture(&json!({
            "model": "gemini-3-pro",
            "output_config": {"effort": "turbo"}
        }));
        let mut converted = json!({"generationConfig": {}});
        overrides.apply_gemini(&mut converted);
        assert_eq!(
            converted["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }
}
