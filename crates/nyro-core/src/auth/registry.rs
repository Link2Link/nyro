use std::sync::Arc;

use crate::auth::drivers::{ClaudeOAuthDriver, GrokOAuthDriver, OpenAIOAuthDriver};
use crate::auth::types::{AuthDriver, AuthDriverMetadata};

pub fn normalize_driver_key(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "openai-oauth" | "openai_oauth" | "openai" | "codex-cli" | "codex" => "codex".to_string(),
        "claude-code" | "claude_code" | "claude-oauth" | "claude_oauth" | "claude"
        | "anthropic" => "claude-code".to_string(),
        "grok" | "grok-oauth" | "grok_oauth" | "xai" | "xai-oauth" | "xai_oauth" => {
            "grok".to_string()
        }
        other => other.to_string(),
    }
}

pub fn build_driver(key: &str) -> Option<Arc<dyn AuthDriver>> {
    match normalize_driver_key(key).as_str() {
        "codex" => Some(Arc::new(OpenAIOAuthDriver)),
        "claude-code" => Some(Arc::new(ClaudeOAuthDriver)),
        "grok" => Some(Arc::new(GrokOAuthDriver)),
        _ => None,
    }
}

pub fn list_driver_metadata() -> Vec<AuthDriverMetadata> {
    [
        build_driver("codex"),
        build_driver("claude-code"),
        build_driver("grok"),
    ]
    .into_iter()
    .flatten()
    .map(|driver| driver.metadata())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_aliases_normalize_to_grok_driver() {
        for alias in ["grok", "Grok", "xai", "xai-oauth", "xai_oauth"] {
            assert_eq!(normalize_driver_key(alias), "grok", "alias {alias}");
            assert_eq!(build_driver(alias).unwrap().metadata().key, "grok");
        }
    }

    #[test]
    fn list_includes_grok() {
        let keys: Vec<&str> = list_driver_metadata().into_iter().map(|m| m.key).collect();
        assert!(keys.contains(&"grok"));
        assert!(keys.contains(&"codex"));
        assert!(keys.contains(&"claude-code"));
    }
}
