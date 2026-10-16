//! Small, conservative catalog derived from CLIProxyAPI's Devin model resolver.
//! Exact upstream UIDs remain usable; suggestions do not assert account access.
use crate::protocol::ir::request::{ReasoningConfig, ReasoningEffort};

pub(super) const MODELS: &[&str] = &[
    "swe-2",
    "swe-1-7",
    "swe-1-6",
    "swe-1-6-slow",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
    "gpt-5-2",
    "gemini-3-8-flash",
    "glm-5-2",
];

fn effort(reasoning: &ReasoningConfig) -> Option<&'static str> {
    use ReasoningEffort::*;
    let budget = |tokens| match tokens {
        0 => "none",
        1..=4096 => "low",
        4097..=16384 => "medium",
        16385..=32768 => "high",
        _ => "max",
    };
    match reasoning.effort.as_ref() {
        Some(None) => Some("none"),
        Some(Minimal) => Some("minimal"),
        Some(Low) => Some("low"),
        Some(Medium) => Some("medium"),
        Some(High) => Some("high"),
        Some(Xhigh) => Some("xhigh"),
        Some(Max) => Some("max"),
        Some(Budget(tokens)) => Some(budget(*tokens)),
        std::option::Option::None => reasoning
            .budget_tokens
            .map(budget)
            .or_else(|| reasoning.enabled.then_some("high")),
    }
}

fn clamp(requested: Option<&str>, allowed: &[&str], default: &str) -> String {
    let Some(requested) = requested else {
        return default.into();
    };
    if allowed.contains(&requested) {
        return requested.into();
    }
    let order = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let rank = |value: &str| order.iter().position(|entry| *entry == value).unwrap_or(0);
    allowed
        .iter()
        .min_by_key(|value| rank(value).abs_diff(rank(requested)))
        .unwrap_or(&default)
        .to_string()
}

pub(super) fn resolve(raw: &str, reasoning: &ReasoningConfig) -> String {
    let raw = raw.trim();
    let model = if raw
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("devin/"))
    {
        &raw[6..]
    } else {
        raw
    };
    if model.is_empty() {
        return "swe-2-high".into();
    }
    // An exact UID (including an explicit effort/priority) overrides body effort.
    let lower = model.to_ascii_lowercase();
    if [
        "-none",
        "-minimal",
        "-low",
        "-medium",
        "-high",
        "-xhigh",
        "-max",
        "-fast",
        "-slow",
        "-priority",
        "-thinking",
        "-thinking-1m",
        "-max-1m",
        "-none-1m",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
        || model.starts_with("MODEL_")
    {
        return model.into();
    }
    let canonical = lower.replace('.', "-");
    let requested = effort(reasoning);
    match canonical.as_str() {
        "claude-haiku-4-5" => "MODEL_PRIVATE_11".into(),
        "gpt-4-1" => "MODEL_CHAT_GPT_4_1_2025_04_14".into(),
        "claude-sonnet-4-5" => if requested.is_some_and(|v| v != "none") {
            "MODEL_PRIVATE_3"
        } else {
            "MODEL_PRIVATE_2"
        }
        .into(),
        "swe-1-6" => canonical,
        "swe-1-7" => {
            if requested == Some("medium") {
                "swe-1-7-medium".into()
            } else {
                canonical
            }
        }
        "glm-5-2" | "glm-5-2-1m" => match requested {
            Some("none" | "max") => format!(
                "glm-5-2-{}{}",
                requested.unwrap(),
                if canonical.ends_with("-1m") {
                    "-1m"
                } else {
                    ""
                }
            ),
            _ => canonical,
        },
        "claude-opus-4-6" | "claude-sonnet-4-6" | "claude-opus-4-6-1m" | "claude-sonnet-4-6-1m" => {
            if requested.is_some_and(|v| v != "none") {
                format!(
                    "{}-thinking{}",
                    canonical.trim_end_matches("-1m"),
                    if canonical.ends_with("-1m") {
                        "-1m"
                    } else {
                        ""
                    }
                )
            } else {
                canonical
            }
        }
        "swe-2" => format!(
            "swe-2-{}",
            clamp(requested, &["medium", "high", "max"], "high")
        ),
        "gpt-5-2" => format!(
            "gpt-5-2-{}",
            clamp(
                requested,
                &["none", "low", "medium", "high", "xhigh"],
                "low"
            )
        ),
        "gemini-3-flash" | "gemini-3-8-flash" => format!(
            "gemini-3-8-flash-{}",
            clamp(requested, &["low", "medium", "high"], "high")
        ),
        _ => model.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn devin_model_defaults_private_aliases_and_exact_uids() {
        let config = ReasoningConfig::default();
        assert_eq!(resolve("devin/swe-2", &config), "swe-2-high");
        assert_eq!(resolve("gpt-5.2", &config), "gpt-5-2-low");
        assert_eq!(resolve("claude-haiku-4-5", &config), "MODEL_PRIVATE_11");
        assert_eq!(resolve("FutureUID", &config), "FutureUID");
        assert_eq!(resolve("MODEL_PRIVATE_11", &config), "MODEL_PRIVATE_11");
    }
    #[test]
    fn devin_effort_is_bounded_and_explicit_variants_win() {
        let config = ReasoningConfig {
            effort: Some(ReasoningEffort::Max),
            ..Default::default()
        };
        assert_eq!(resolve("swe-2", &config), "swe-2-max");
        assert_eq!(resolve("gpt-5-2", &config), "gpt-5-2-xhigh");
        assert_eq!(resolve("swe-2-low", &config), "swe-2-low");
        assert_eq!(
            resolve("claude-opus-4-6-1m", &config),
            "claude-opus-4-6-thinking-1m"
        );
    }
}
