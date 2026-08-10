use crate::protocol::ir::{AiResponse, ReasoningConfig, ReasoningEffort};

pub fn parse_reasoning_effort(value: &str) -> Option<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

pub fn reasoning_effort_name(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => Some("none"),
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::Xhigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
        ReasoningEffort::Budget(_) => None,
    }
}

pub fn anthropic_effort_name(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal => Some("low"),
        _ => reasoning_effort_name(effort),
    }
}

/// Gemini `thinkingConfig.thinkingLevel` values are lowercase per the API spec
/// (`low` / `medium` / `high`), unlike the uppercase wire conventions of the
/// OpenAI-family protocols.
pub fn google_thinking_level(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None | ReasoningEffort::Budget(_) => None,
        ReasoningEffort::Minimal => Some("low"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High | ReasoningEffort::Xhigh | ReasoningEffort::Max => Some("high"),
    }
}

pub fn effective_openai_effort(
    reasoning: &ReasoningConfig,
    max_tokens: Option<u32>,
) -> Option<ReasoningEffort> {
    match reasoning.effort.as_ref() {
        Some(ReasoningEffort::Budget(tokens)) => Some(effort_from_budget(*tokens, max_tokens)),
        Some(effort) => Some(effort.clone()),
        None => reasoning
            .budget_tokens
            .map(|tokens| effort_from_budget(tokens, max_tokens))
            .or_else(|| reasoning.enabled.then_some(ReasoningEffort::Medium)),
    }
}

fn effort_from_budget(budget: u32, max_tokens: Option<u32>) -> ReasoningEffort {
    if budget == 0 {
        return ReasoningEffort::None;
    }
    let Some(max_tokens) = max_tokens.filter(|max_tokens| *max_tokens > 0) else {
        return ReasoningEffort::Medium;
    };

    // Match the published OpenRouter budget-to-effort thresholds.
    let budget = u64::from(budget) * 100;
    let max_tokens = u64::from(max_tokens);
    if budget <= max_tokens * 10 {
        ReasoningEffort::Minimal
    } else if budget <= max_tokens * 20 {
        ReasoningEffort::Low
    } else if budget <= max_tokens * 50 {
        ReasoningEffort::Medium
    } else if budget <= max_tokens * 80 {
        ReasoningEffort::High
    } else if budget <= max_tokens * 95 {
        ReasoningEffort::Xhigh
    } else {
        ReasoningEffort::Max
    }
}

pub fn normalize_response_reasoning(resp: &mut AiResponse) {
    if resp.reasoning_content.is_some() {
        return;
    }

    let (reasoning, text) = split_think_tags(&resp.content);
    if reasoning.is_some() {
        resp.reasoning_content = reasoning;
        resp.content = text;
    }
}

pub(crate) fn split_think_tags(content: &str) -> (Option<String>, String) {
    let mut remaining = content;
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();

    loop {
        let Some(start_idx) = remaining.find("<think>") else {
            if !remaining.is_empty() {
                text_parts.push(remaining.to_string());
            }
            break;
        };

        let before = &remaining[..start_idx];
        if !before.is_empty() {
            text_parts.push(before.to_string());
        }

        let after_start = &remaining[start_idx + "<think>".len()..];
        let Some(end_rel_idx) = after_start.find("</think>") else {
            text_parts.push(remaining[start_idx..].to_string());
            break;
        };

        let thought = after_start[..end_rel_idx].trim();
        if !thought.is_empty() {
            reasoning_parts.push(thought.to_string());
        }
        remaining = &after_start[end_rel_idx + "</think>".len()..];
    }

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };
    (reasoning, text_parts.join("").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_resp(content: &str) -> AiResponse {
        let mut r = AiResponse::new("", "");
        r.content = content.to_string();
        r
    }

    #[test]
    fn test_split_think_tags_basic() {
        let (reasoning, text) = split_think_tags("<think>let me think</think>the answer");
        assert_eq!(reasoning.as_deref(), Some("let me think"));
        assert_eq!(text, "the answer");
    }

    #[test]
    fn test_split_think_tags_no_tags() {
        let (reasoning, text) = split_think_tags("just text");
        assert!(reasoning.is_none());
        assert_eq!(text, "just text");
    }

    #[test]
    fn test_split_think_tags_multiple() {
        let (reasoning, text) =
            split_think_tags("<think>step1</think>middle<think>step2</think>end");
        let r = reasoning.unwrap();
        assert!(r.contains("step1"), "expected step1 in reasoning: {r}");
        assert!(r.contains("step2"), "expected step2 in reasoning: {r}");
        assert_eq!(text, "middleend");
    }

    #[test]
    fn test_split_think_tags_unclosed() {
        let (reasoning, text) = split_think_tags("<think>incomplete");
        assert!(
            reasoning.is_none(),
            "unclosed think should produce no reasoning"
        );
        assert!(
            text.contains("<think>"),
            "unclosed think tag should remain in text"
        );
    }

    #[test]
    fn test_normalize_response_reasoning_no_op_when_already_set() {
        let mut resp = make_resp("<think>should be ignored</think>answer");
        resp.reasoning_content = Some("existing reasoning".to_string());
        normalize_response_reasoning(&mut resp);
        assert_eq!(
            resp.reasoning_content.as_deref(),
            Some("existing reasoning")
        );
    }

    #[test]
    fn test_normalize_response_reasoning_extracts_think_tags() {
        let mut resp = make_resp("<think>my reasoning</think>final answer");
        normalize_response_reasoning(&mut resp);
        assert_eq!(resp.reasoning_content.as_deref(), Some("my reasoning"));
        assert_eq!(resp.content, "final answer");
    }

    #[test]
    fn reasoning_effort_names_cover_all_qualitative_levels() {
        let levels = [
            ("none", ReasoningEffort::None),
            ("minimal", ReasoningEffort::Minimal),
            ("low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::Xhigh),
            ("max", ReasoningEffort::Max),
        ];

        for (name, effort) in levels {
            assert_eq!(parse_reasoning_effort(name), Some(effort.clone()));
            assert_eq!(reasoning_effort_name(&effort), Some(name));
        }
        assert_eq!(parse_reasoning_effort("HIGH"), Some(ReasoningEffort::High));
        assert_eq!(parse_reasoning_effort("unknown"), None);
    }

    #[test]
    fn target_effort_names_clamp_only_unsupported_extremes() {
        assert_eq!(
            anthropic_effort_name(&ReasoningEffort::Minimal),
            Some("low")
        );
        assert_eq!(anthropic_effort_name(&ReasoningEffort::Max), Some("max"));
        assert_eq!(google_thinking_level(&ReasoningEffort::Minimal), Some("low"));
        assert_eq!(google_thinking_level(&ReasoningEffort::Xhigh), Some("high"));
        assert_eq!(google_thinking_level(&ReasoningEffort::Max), Some("high"));
    }

    #[test]
    fn token_budgets_map_to_openai_effort_by_output_ratio() {
        let config = |budget_tokens| ReasoningConfig {
            enabled: budget_tokens > 0,
            budget_tokens: Some(budget_tokens),
            ..Default::default()
        };

        assert_eq!(
            effective_openai_effort(&config(0), Some(10_000)),
            Some(ReasoningEffort::None)
        );
        assert_eq!(
            effective_openai_effort(&config(1_000), Some(10_000)),
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(
            effective_openai_effort(&config(2_000), Some(10_000)),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            effective_openai_effort(&config(5_000), Some(10_000)),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            effective_openai_effort(&config(8_000), Some(10_000)),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            effective_openai_effort(&config(9_500), Some(10_000)),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(
            effective_openai_effort(&config(9_501), Some(10_000)),
            Some(ReasoningEffort::Max)
        );
        assert_eq!(
            effective_openai_effort(&config(4_096), None),
            Some(ReasoningEffort::Medium)
        );
    }
}
