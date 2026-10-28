//! Claude Code subscription model catalog.
//!
//! Claude Code never lists models through `/v1/models` for a claude.ai OAuth
//! login (it only queries `/v1/models` in gateway mode behind
//! `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`). Its `/model` picker comes
//! from a catalog compiled into the CLI; this module mirrors that catalog and
//! publishes it as the `claude-code` channel's `static_models`, which admin
//! model discovery returns as-is.
//!
//! Refresh [`STATIC_MODELS`] when a new Claude Code release changes its `cc`
//! picker (`model_selector_config` entry `id:"cc"` in the CLI binary).

/// Claude Code 2.1.312's built-in `cc` picker (main section first, then
/// overflow), followed by the older dated ids CLIProxyAPI keeps so existing
/// routes stay discoverable. Newest first.
pub(crate) const STATIC_MODELS: &[&str] = &[
    // cc picker — main
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-fable-5-1",
    "claude-haiku-5-5",
    // cc picker — overflow
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-haiku-4-5-20251001",
    "claude-opus-4-1-20250805",
    // older ids kept for existing routes
    "claude-opus-4-5-20251101",
    "claude-sonnet-4-5-20250929",
    "claude-opus-4-20250514",
    "claude-sonnet-4-20250514",
    "claude-3-7-sonnet-20250219",
    "claude-3-5-haiku-20241022",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_catalog_leads_with_the_current_picker() {
        assert_eq!(
            &STATIC_MODELS[..4],
            &[
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "claude-fable-5-1",
                "claude-haiku-5-5"
            ]
        );
        let mut seen = std::collections::HashSet::new();
        assert!(
            STATIC_MODELS.iter().all(|m| seen.insert(*m)),
            "no duplicates"
        );
    }
}
