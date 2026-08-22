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
}

impl ReasoningOverrides {
    pub(crate) fn capture(body: &Value) -> Self {
        let openai_effort = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| supports_reasoning_effort(model))
            .and_then(|_| resolve_reasoning_effort(body))
            .map(str::to_string);

        Self {
            openai_effort,
            gemini_thinking_config: gemini_thinking_config(body),
        }
    }

    pub(crate) fn apply_chat(&self, converted: &mut Value) {
        if let Some(effort) = self.openai_effort.as_ref() {
            converted["reasoning_effort"] = Value::String(effort.clone());
        }
    }

    pub(crate) fn apply_responses(&self, converted: &mut Value) {
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
