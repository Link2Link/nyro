//! Kiro model catalog: client-visible ids, upstream ids, context windows and
//! the reasoning-effort fields Kiro accepts per model family.
//!
//! Mappings follow AIClient2API's `claude-kiro.js` / `kiro-effort.js`. Exact
//! upstream ids stay usable even when they are not listed here.
use serde_json::{Value, json};

use crate::protocol::ir::request::{ReasoningConfig, ReasoningEffort};

/// Curated suggestions, not an account-specific entitlement catalog.
pub(super) const MODELS: &[&str] = &[
    "claude-sonnet-5-5",
    "claude-opus-5-5",
    "claude-sonnet-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-4-5",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

const DEFAULT_CONTEXT_TOKENS: u32 = 200_000;
const LARGE_CONTEXT_TOKENS: u32 = 1_000_000;

/// Map a client model id to Kiro's upstream id.
///
/// `claude-opus-4-6` and `claude-opus-4-6-20251101` become `claude-opus-4.6`;
/// ids that already use Kiro's dotted form, or anything unrecognised, pass
/// through unchanged so new upstream ids work without a release.
pub(super) fn resolve(model: &str) -> String {
    let model = model.trim();
    let lowered = model.to_ascii_lowercase();
    let Some(rest) = lowered.strip_prefix("claude-") else {
        return model.to_string();
    };
    let rest = strip_date_suffix(rest);
    let parts: Vec<&str> = rest.split('-').collect();
    // `<family>-<major>-<minor>` e.g. opus-4-6; `<family>-<major>` e.g. opus-5.
    match parts.as_slice() {
        [family, major, minor] if is_family(family) && is_number(major) && is_number(minor) => {
            format!("claude-{family}-{major}.{minor}")
        }
        [family, major] if is_family(family) && is_number(major) => {
            format!("claude-{family}-{major}")
        }
        _ => model.to_string(),
    }
}

fn is_family(value: &str) -> bool {
    matches!(value, "opus" | "sonnet" | "haiku" | "fable")
}

fn is_number(value: &str) -> bool {
    !value.is_empty() && value.len() <= 2 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Drop a trailing `-YYYYMMDD` snapshot date.
fn strip_date_suffix(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => model,
    }
}

/// Context window used to turn Kiro's `contextUsagePercentage` into tokens.
pub(super) fn context_tokens(upstream_model: &str) -> u32 {
    const LARGE: &[&str] = &[
        "claude-opus-5.5",
        "claude-sonnet-5.5",
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-opus-4.8",
        "claude-opus-4.7",
        "claude-opus-4.6",
        "claude-opus-4.5",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ];
    if LARGE.contains(&upstream_model) {
        LARGE_CONTEXT_TOKENS
    } else {
        DEFAULT_CONTEXT_TOKENS
    }
}

/// Reasoning-effort levels Kiro accepts for a model, or `None` when the model
/// must not receive `additionalModelRequestFields` at all (Kiro answers 400).
enum EffortFamily {
    /// `{"reasoning":{"effort":..}}`
    Gpt,
    /// `{"output_config":{"effort":..}}` limited to the listed levels.
    Claude(&'static [&'static str]),
}

const CLAUDE_FULL: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const CLAUDE_NO_XHIGH: &[&str] = &["low", "medium", "high", "max"];

fn effort_family(upstream_model: &str) -> Option<EffortFamily> {
    match upstream_model {
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => Some(EffortFamily::Gpt),
        "claude-opus-5.5" | "claude-sonnet-5.5" | "claude-opus-5" | "claude-opus-4.8"
        | "claude-opus-4.7" | "claude-sonnet-5" => Some(EffortFamily::Claude(CLAUDE_FULL)),
        "claude-opus-4.6" | "claude-sonnet-4.6" => Some(EffortFamily::Claude(CLAUDE_NO_XHIGH)),
        _ => None,
    }
}

/// Qualitative effort name carried by the IR, if any. Token budgets have no
/// qualitative name and are handled by the in-band thinking directive instead.
fn named_effort(reasoning: &ReasoningConfig) -> Option<&'static str> {
    Some(match reasoning.effort.as_ref()? {
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
        ReasoningEffort::Budget(_) => return None,
    })
}

/// `additionalModelRequestFields` for this model and request, if any.
pub(super) fn additional_fields(
    upstream_model: &str,
    reasoning: &ReasoningConfig,
) -> Option<Value> {
    let family = effort_family(upstream_model)?;
    let explicit = named_effort(reasoning);
    match family {
        EffortFamily::Gpt => {
            let effort = match explicit {
                Some("none") => "none",
                Some(level @ ("low" | "medium" | "high" | "xhigh" | "max")) => level,
                Some(_) => "high",
                None if reasoning.enabled => "high",
                None => return None,
            };
            Some(json!({ "reasoning": { "effort": effort } }))
        }
        EffortFamily::Claude(allowed) => {
            let effort = match explicit {
                Some(level) if allowed.contains(&level) => level,
                Some(_) => "medium",
                None if reasoning.enabled => "medium",
                None => return None,
            };
            Some(json!({ "output_config": { "effort": effort } }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasoning(effort: Option<ReasoningEffort>, enabled: bool) -> ReasoningConfig {
        ReasoningConfig {
            enabled,
            effort,
            ..Default::default()
        }
    }

    #[test]
    fn dashed_claude_ids_become_dotted_upstream_ids() {
        for (input, expected) in [
            ("claude-opus-4-6", "claude-opus-4.6"),
            ("claude-sonnet-4-5", "claude-sonnet-4.5"),
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4.5"),
            ("claude-opus-5", "claude-opus-5"),
            ("Claude-Opus-5-5", "claude-opus-5.5"),
            // Already dotted, unknown, or non-Claude ids pass through.
            ("claude-opus-4.6", "claude-opus-4.6"),
            ("gpt-5.6-sol", "gpt-5.6-sol"),
            ("something-new", "something-new"),
            ("claude-3-7-sonnet-20250219", "claude-3-7-sonnet-20250219"),
        ] {
            assert_eq!(resolve(input), expected, "{input}");
        }
    }

    #[test]
    fn every_suggested_model_resolves_to_a_nonempty_upstream_id() {
        for model in MODELS {
            assert!(!resolve(model).is_empty(), "{model}");
        }
    }

    #[test]
    fn large_context_models_are_recognised_by_upstream_id() {
        assert_eq!(context_tokens("claude-opus-4.6"), 1_000_000);
        assert_eq!(context_tokens("claude-sonnet-4.6"), 200_000);
        assert_eq!(context_tokens("claude-haiku-4.5"), 200_000);
        assert_eq!(context_tokens("unknown"), 200_000);
    }

    #[test]
    fn unsupported_models_never_receive_effort_fields() {
        for model in [
            "claude-haiku-4.5",
            "claude-sonnet-4.5",
            "claude-opus-4.5",
            "mystery",
        ] {
            assert_eq!(
                additional_fields(model, &reasoning(Some(ReasoningEffort::High), true)),
                None,
                "{model}"
            );
        }
    }

    #[test]
    fn claude_effort_is_clamped_to_the_models_allowed_levels() {
        let high = reasoning(Some(ReasoningEffort::High), true);
        assert_eq!(
            additional_fields("claude-opus-4.8", &high),
            Some(json!({"output_config":{"effort":"high"}}))
        );
        let xhigh = reasoning(Some(ReasoningEffort::Xhigh), true);
        assert_eq!(
            additional_fields("claude-opus-4.8", &xhigh),
            Some(json!({"output_config":{"effort":"xhigh"}}))
        );
        // 4.6 has no xhigh tier: unsupported levels fall back to medium.
        assert_eq!(
            additional_fields("claude-opus-4.6", &xhigh),
            Some(json!({"output_config":{"effort":"medium"}}))
        );
        // Thinking enabled with no named effort defaults to medium.
        assert_eq!(
            additional_fields("claude-sonnet-5", &reasoning(None, true)),
            Some(json!({"output_config":{"effort":"medium"}}))
        );
        // A bare token budget carries no qualitative effort.
        assert_eq!(
            additional_fields(
                "claude-sonnet-5",
                &reasoning(Some(ReasoningEffort::Budget(8000)), true)
            ),
            Some(json!({"output_config":{"effort":"medium"}}))
        );
        assert_eq!(
            additional_fields("claude-sonnet-5", &reasoning(None, false)),
            None
        );
    }

    #[test]
    fn gpt_effort_uses_the_reasoning_field_and_honours_off() {
        assert_eq!(
            additional_fields(
                "gpt-5.6-sol",
                &reasoning(Some(ReasoningEffort::None), false)
            ),
            Some(json!({"reasoning":{"effort":"none"}}))
        );
        assert_eq!(
            additional_fields("gpt-5.6-luna", &reasoning(Some(ReasoningEffort::Max), true)),
            Some(json!({"reasoning":{"effort":"max"}}))
        );
        assert_eq!(
            additional_fields("gpt-5.6-terra", &reasoning(None, false)),
            None
        );
    }
}
