// Pure model predicates extracted from cc-switch's thinking optimizer.
// Copyright (c) 2025 Jason Young. Licensed under MIT.
//
// Note: cc-switch's `uses_adaptive_thinking` whitelist is intentionally NOT
// ported. Every Anthropic-protocol egress is treated as effort-capable
// (`thinking.type: "adaptive"` + `output_config.effort`); modern
// Anthropic-compatible endpoints (Kimi, GLM, ...) all accept effort levels.

pub(crate) fn adaptive_thinking_is_default(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    ["fable-5", "mythos-5", "mythos-preview", "sonnet-5"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

pub(crate) fn thinking_cannot_be_disabled(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    ["fable-5", "mythos-5"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn normalize_model_name(model: &str) -> String {
    model.trim().to_ascii_lowercase().replace(['.', '_'], "-")
}
