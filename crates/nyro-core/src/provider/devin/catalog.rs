//! Small, conservative catalog derived from CLIProxyAPI's Devin model resolver.
//! Exact upstream UIDs remain usable; suggestions do not assert account access.
use crate::protocol::ir::request::{ReasoningConfig, ReasoningEffort};

pub(super) const MODELS: &[&str] = &[
    "swe-2",
    "swe-1-7",
    "swe-1-7-lightning",
    "swe-1-6",
    "swe-1-6-slow",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-fable-5-1",
    "claude-5-fable",
    "claude-haiku-5-5",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
    "gpt-6-astra",
    "gpt-6-1-sol",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5-6-sol",
    "gpt-5-6-terra",
    "gpt-5-6-luna",
    "gpt-5-5",
    "gpt-5-4",
    "gpt-5-4-mini",
    "gpt-5-3-codex",
    "gpt-5-2",
    "gemini-4-argon",
    "gemini-3-8-flash",
    "gemini-3-7-flash",
    "gemini-3-6-flash",
    "gemini-3-5-flash",
    "gemini-3-1-pro",
    "glm-5-3",
    "glm-5-3-flash",
    "glm-5-2",
    "kimi-k3",
    "kimi-k2-7",
    "kimi-k2-6",
    "grok-4-7",
    "grok-4-6",
    "grok-4-5",
    "deepseek-v4-pro",
    "deepseek-v4-1-flash",
    "deepseek-v4-flash",
    "nemotron-3-ultra",
    "inkling",
];

/// A model family whose upstream UIDs are `<family>-<effort>`.
///
/// A bare family name (what a client or route asks for) resolves to the variant
/// matching the request's reasoning effort, clamped to the tiers the family
/// really offers, so one route can serve every tier. An exact UID such as
/// `claude-opus-5-5-high` never reaches this table: `resolve` sends it as-is.
struct TieredFamily {
    /// Lower-case, dash-separated family name (`.` is normalised to `-`).
    family: &'static str,
    /// Offered tiers, lowest to highest. No family has a thinking-off variant.
    tiers: &'static [&'static str],
    /// Tier used when the request carries no reasoning directive at all. It
    /// follows CLIProxyAPI's default selection: Gemini lines prefer `high`,
    /// other thinking families `medium`.
    default: &'static str,
}

const LOW_TO_MAX: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const NONE_TO_MAX: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
const NONE_TO_XHIGH: &[&str] = &["none", "low", "medium", "high", "xhigh"];
const LOW_TO_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
const LOW_TO_HIGH: &[&str] = &["low", "medium", "high"];
const MINIMAL_TO_HIGH: &[&str] = &["minimal", "low", "medium", "high"];
const LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const HIGH_MAX: &[&str] = &["high", "max"];
const NONE_MEDIUM_HIGH: &[&str] = &["none", "medium", "high"];
const LOW_HIGH: &[&str] = &["low", "high"];

const fn family(
    family: &'static str,
    tiers: &'static [&'static str],
    default: &'static str,
) -> TieredFamily {
    TieredFamily {
        family,
        tiers,
        default,
    }
}

/// Families resolved by table lookup. `swe-2`, `gpt-5-2` and `gemini-3-8-flash`
/// predate it and keep their own arms in `resolve`; a new `<family>-<effort>`
/// model line only needs an entry here (plus one in `MODELS` for suggestions).
///
/// Tiers and defaults mirror CLIProxyAPI's `devin_models.json`. Its default rule
/// is: GPT-5.x with a `low` tier use `low`; Gemini/Grok/GLM/Kimi/DeepSeek/Nemotron
/// prefer `high` when they offer it; everything else prefers `medium`.
const TIERED: &[TieredFamily] = &[
    // Claude
    family("claude-opus-5-5", LOW_TO_MAX, "medium"),
    family("claude-opus-5", LOW_TO_MAX, "medium"),
    family("claude-opus-4-8", LOW_TO_MAX, "medium"),
    family("claude-opus-4-7", LOW_TO_MAX, "medium"),
    family("claude-sonnet-5-5", LOW_TO_MAX, "medium"),
    family("claude-sonnet-5", LOW_TO_MAX, "medium"),
    family("claude-fable-5-1", LOW_TO_MAX, "medium"),
    family("claude-5-fable", LOW_TO_MAX, "medium"),
    family("claude-haiku-5-5", LOW_TO_HIGH, "medium"),
    // OpenAI
    family("gpt-6-astra", LOW_TO_MAX, "medium"),
    family("gpt-6-1-sol", LOW_TO_MAX, "medium"),
    family("gpt-6-sol", NONE_TO_MAX, "medium"),
    family("gpt-6-luna", NONE_TO_MAX, "medium"),
    family("gpt-5-6-sol", NONE_TO_MAX, "low"),
    family("gpt-5-6-terra", NONE_TO_MAX, "low"),
    family("gpt-5-6-luna", NONE_TO_MAX, "low"),
    family("gpt-5-5", NONE_TO_XHIGH, "low"),
    family("gpt-5-4", NONE_TO_XHIGH, "low"),
    family("gpt-5-4-mini", LOW_TO_XHIGH, "medium"),
    family("gpt-5-3-codex", LOW_TO_XHIGH, "medium"),
    // Google
    family("gemini-4-argon", LOW_TO_HIGH, "high"),
    family("gemini-3-7-flash", LOW_TO_HIGH, "high"),
    family("gemini-3-6-flash", MINIMAL_TO_HIGH, "high"),
    family("gemini-3-5-flash", MINIMAL_TO_HIGH, "high"),
    family("gemini-3-1-pro", LOW_HIGH, "high"),
    // Zhipu / Moonshot / xAI / DeepSeek / NVIDIA / Cognition
    family("glm-5-3", LOW_HIGH_MAX, "high"),
    family("glm-5-3-flash", LOW_HIGH_MAX, "high"),
    family("kimi-k3", LOW_HIGH_MAX, "high"),
    family("grok-4-7", LOW_TO_XHIGH, "high"),
    family("grok-4-6", LOW_TO_XHIGH, "high"),
    family("grok-4-5", LOW_TO_HIGH, "high"),
    family("deepseek-v4-pro", HIGH_MAX, "high"),
    family("deepseek-v4-1-flash", HIGH_MAX, "high"),
    family("deepseek-v4-flash", HIGH_MAX, "high"),
    family("nemotron-3-ultra", NONE_MEDIUM_HIGH, "high"),
    family("inkling", NONE_TO_MAX, "medium"),
    family("swe-1-7-lightning", &["medium"], "medium"),
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
        // On a tie prefer the higher tier (medium between low/high -> high).
        .min_by_key(|value| {
            (
                rank(value).abs_diff(rank(requested)),
                std::cmp::Reverse(rank(value)),
            )
        })
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
        _ => match TIERED.iter().find(|tiered| tiered.family == canonical) {
            Some(tiered) => format!(
                "{}-{}",
                tiered.family,
                clamp(requested, tiered.tiers, tiered.default)
            ),
            None => model.into(),
        },
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

    fn effort_config(effort: ReasoningEffort) -> ReasoningConfig {
        ReasoningConfig {
            effort: Some(effort),
            ..Default::default()
        }
    }

    /// Every effort a client can express, in ascending order.
    fn every_effort() -> Vec<ReasoningEffort> {
        vec![
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ]
    }

    #[test]
    fn tiered_family_maps_each_effort_to_its_own_variant() {
        // Five-tier families: legal tiers map 1:1, "off"/minimal clamp up to the
        // lowest tier because no family has a thinking-off variant.
        let expected = [
            (ReasoningEffort::None, "low"),
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Xhigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ];
        for family in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-fable-5-1",
            "gpt-6-astra",
            "gpt-6-1-sol",
        ] {
            for (effort, tier) in &expected {
                assert_eq!(
                    resolve(family, &effort_config(effort.clone())),
                    format!("{family}-{tier}"),
                    "{family} with {effort:?}"
                );
            }
        }
        // Three-tier families: xhigh/max clamp down to their top tier.
        let expected = [
            (ReasoningEffort::None, "low"),
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Xhigh, "high"),
            (ReasoningEffort::Max, "high"),
        ];
        for family in ["claude-haiku-5-5", "gemini-4-argon"] {
            for (effort, tier) in &expected {
                assert_eq!(
                    resolve(family, &effort_config(effort.clone())),
                    format!("{family}-{tier}"),
                    "{family} with {effort:?}"
                );
            }
        }
    }

    #[test]
    fn tiered_families_reach_every_published_variant_and_nothing_else() {
        // Independent of TIERED: the upstream UIDs each family really publishes.
        let published: [(&str, &[&str]); 7] = [
            (
                "claude-opus-5-5",
                &["low", "medium", "high", "xhigh", "max"],
            ),
            (
                "claude-sonnet-5-5",
                &["low", "medium", "high", "xhigh", "max"],
            ),
            (
                "claude-fable-5-1",
                &["low", "medium", "high", "xhigh", "max"],
            ),
            ("claude-haiku-5-5", &["low", "medium", "high"]),
            ("gpt-6-astra", &["low", "medium", "high", "xhigh", "max"]),
            ("gpt-6-1-sol", &["low", "medium", "high", "xhigh", "max"]),
            ("gemini-4-argon", &["low", "medium", "high"]),
        ];
        for (family, tiers) in published {
            let reached: std::collections::BTreeSet<String> = every_effort()
                .into_iter()
                .map(|effort| resolve(family, &effort_config(effort)))
                .collect();
            let wanted: std::collections::BTreeSet<String> = tiers
                .iter()
                .map(|tier| format!("{family}-{tier}"))
                .collect();
            // Every variant is reachable and no phantom UID (e.g. "-none") leaks out.
            assert_eq!(reached, wanted, "{family}");
        }
    }

    #[test]
    fn tiered_family_uses_budget_enabled_and_default_when_effort_is_absent() {
        let budget = |tokens| ReasoningConfig {
            budget_tokens: Some(tokens),
            ..Default::default()
        };
        // Same absolute thresholds the other Devin families already use.
        assert_eq!(
            resolve("claude-opus-5-5", &budget(2048)),
            "claude-opus-5-5-low"
        );
        assert_eq!(
            resolve("claude-opus-5-5", &budget(10_000)),
            "claude-opus-5-5-medium"
        );
        assert_eq!(
            resolve("claude-opus-5-5", &budget(31_999)),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            resolve("claude-opus-5-5", &budget(64_000)),
            "claude-opus-5-5-max"
        );
        // An explicit Budget effort behaves like budget_tokens.
        assert_eq!(
            resolve(
                "claude-opus-5-5",
                &effort_config(ReasoningEffort::Budget(10_000))
            ),
            "claude-opus-5-5-medium"
        );
        // Thinking on without a level means "high"; no directive uses the family default.
        let enabled = ReasoningConfig {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(
            resolve("claude-sonnet-5-5", &enabled),
            "claude-sonnet-5-5-high"
        );
        let none = ReasoningConfig::default();
        assert_eq!(
            resolve("claude-sonnet-5-5", &none),
            "claude-sonnet-5-5-medium"
        );
        assert_eq!(resolve("gpt-6-astra", &none), "gpt-6-astra-medium");
        assert_eq!(resolve("gpt-6-1-sol", &none), "gpt-6-1-sol-medium");
        assert_eq!(resolve("gpt-6.1-sol", &none), "gpt-6-1-sol-medium");
        assert_eq!(resolve("gemini-4-argon", &none), "gemini-4-argon-high");
        // Budget maps past a three-tier family's ceiling onto its top tier.
        assert_eq!(
            resolve("gemini-4-argon", &budget(64_000)),
            "gemini-4-argon-high"
        );
    }

    #[test]
    fn tiered_family_keeps_exact_uids_and_normalises_names() {
        let max = effort_config(ReasoningEffort::Max);
        // An explicit tier is a per-variant route and always wins over the body.
        for uid in [
            "claude-opus-5-5-low",
            "claude-opus-5-5-medium",
            "claude-opus-5-5-high",
            "claude-opus-5-5-xhigh",
            "claude-opus-5-5-max",
            "gemini-4-argon-medium",
            "gpt-6-astra-low",
        ] {
            assert_eq!(resolve(uid, &max), uid);
        }
        // Prefix, case and dotted spellings reach the same family.
        assert_eq!(
            resolve("devin/claude-opus-5-5", &max),
            "claude-opus-5-5-max"
        );
        assert_eq!(resolve("Claude-Opus-5.5", &max), "claude-opus-5-5-max");
        assert_eq!(resolve("  gpt-6-astra  ", &max), "gpt-6-astra-max");
        // Lookalikes are not families and pass through untouched.
        for other in ["claude-opus-5-55", "claude-opus-6", "gpt-6-astral", "gpt-5-4-mi"] {
            assert_eq!(resolve(other, &max), other);
        }
    }

    #[test]
    fn upstream_catalog_families_follow_their_published_tiers() {
        let resolve_with = |model: &str, effort: Option<ReasoningEffort>| {
            let config = ReasoningConfig {
                effort: effort.map(Some).unwrap_or(std::option::Option::None),
                ..Default::default()
            };
            resolve(model, &config)
        };
        use ReasoningEffort::*;
        // (model, effort, expected UID). `None` = no reasoning directive.
        let cases: &[(&str, Option<ReasoningEffort>, &str)] = &[
            // Families with a real thinking-off tier keep it.
            ("gpt-6-sol", Some(None), "gpt-6-sol-none"),
            ("gpt-6-luna", Some(Max), "gpt-6-luna-max"),
            ("gpt-5-5", Some(Xhigh), "gpt-5-5-xhigh"),
            ("inkling", Some(None), "inkling-none"),
            ("nemotron-3-ultra", Some(None), "nemotron-3-ultra-none"),
            // Defaults: GPT-5.x -> low, GPT-6 -> medium, Gemini/GLM/Kimi/Grok -> high.
            ("gpt-5-6-sol", Option::None, "gpt-5-6-sol-low"),
            ("gpt-5-4", Option::None, "gpt-5-4-low"),
            ("gpt-5-4-mini", Option::None, "gpt-5-4-mini-medium"),
            ("gpt-6-sol", Option::None, "gpt-6-sol-medium"),
            ("claude-opus-4-8", Option::None, "claude-opus-4-8-medium"),
            ("kimi-k3", Option::None, "kimi-k3-high"),
            ("glm-5-3", Option::None, "glm-5-3-high"),
            ("grok-4-6", Option::None, "grok-4-6-high"),
            ("deepseek-v4-pro", Option::None, "deepseek-v4-pro-high"),
            ("gemini-3-5-flash", Option::None, "gemini-3-5-flash-high"),
            // Gapped tier sets clamp to the nearest tier; a tie goes up.
            ("kimi-k3", Some(Medium), "kimi-k3-high"),
            ("kimi-k3", Some(Xhigh), "kimi-k3-max"),
            ("kimi-k3", Some(None), "kimi-k3-low"),
            ("deepseek-v4-flash", Some(Low), "deepseek-v4-flash-high"),
            ("deepseek-v4-flash", Some(Xhigh), "deepseek-v4-flash-max"),
            ("gemini-3-1-pro", Some(Medium), "gemini-3-1-pro-high"),
            ("nemotron-3-ultra", Some(Low), "nemotron-3-ultra-medium"),
            ("gemini-3-6-flash", Some(Minimal), "gemini-3-6-flash-minimal"),
            ("grok-4-6", Some(Max), "grok-4-6-xhigh"),
            ("gpt-5-3-codex", Some(None), "gpt-5-3-codex-low"),
            // Single-tier family.
            ("swe-1-7-lightning", Some(Max), "swe-1-7-lightning-medium"),
            ("swe-1-7-lightning", Option::None, "swe-1-7-lightning-medium"),
            // The longer name must not be swallowed by its shorter sibling.
            ("gpt-5-4-mini", Some(High), "gpt-5-4-mini-high"),
            ("glm-5-3-flash", Some(High), "glm-5-3-flash-high"),
            ("deepseek-v4-1-flash", Some(Max), "deepseek-v4-1-flash-max"),
            ("claude-opus-5", Some(Xhigh), "claude-opus-5-xhigh"),
            ("claude-sonnet-5", Some(Low), "claude-sonnet-5-low"),
            ("claude-5-fable", Some(High), "claude-5-fable-high"),
            ("claude-opus-5-5", Some(High), "claude-opus-5-5-high"),
            ("gpt-6.1-sol", Some(High), "gpt-6-1-sol-high"),
        ];
        for (model, effort, expected) in cases {
            assert_eq!(
                resolve_with(model, effort.clone()),
                *expected,
                "{model} with {effort:?}"
            );
        }
    }

    #[test]
    fn bare_models_without_thinking_tiers_pass_through() {
        let config = effort_config(ReasoningEffort::High);
        for model in ["kimi-k2-6", "kimi-k2-7", "swe-1-6", "swe-1-6-slow"] {
            assert_eq!(resolve(model, &config), model);
        }
    }

    #[test]
    fn tiered_table_is_consistent_with_suggestions() {
        for tiered in TIERED {
            assert!(
                MODELS.contains(&tiered.family),
                "{} is missing from MODELS",
                tiered.family
            );
            assert!(
                tiered.tiers.contains(&tiered.default),
                "{} default is not one of its tiers",
                tiered.family
            );
            assert!(
                tiered.tiers.windows(2).all(|pair| {
                    let order = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
                    let rank = |tier: &str| order.iter().position(|entry| *entry == tier);
                    rank(pair[0]) < rank(pair[1])
                }),
                "{} tiers must ascend",
                tiered.family
            );
            // The table must not shadow a family with a hand-written arm.
            let config = ReasoningConfig::default();
            assert!(
                resolve(tiered.family, &config).starts_with(tiered.family),
                "{} must resolve into its own family",
                tiered.family
            );
        }
        let mut names: Vec<_> = TIERED.iter().map(|tiered| tiered.family).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TIERED.len(), "duplicate family in TIERED");
    }
}
