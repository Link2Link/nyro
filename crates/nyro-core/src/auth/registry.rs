use std::sync::Arc;

use crate::auth::drivers::{
    ClaudeOAuthDriver, CursorAuthDriver, DevinAuthDriver, GoogleSubscriptionDriver,
    GrokOAuthDriver, OpenAIOAuthDriver,
};
use crate::auth::types::{AuthDriver, AuthDriverMetadata};

pub fn normalize_driver_key(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "cursor" | "cursor-agent" | "cursor-cli" => "cursor".to_string(),
        "devin" | "devin-cli" => "devin".to_string(),
        "openai-oauth" | "openai_oauth" | "openai" | "codex-cli" | "codex" => "codex".to_string(),
        "claude-code" | "claude_code" | "claude-oauth" | "claude_oauth" | "claude"
        | "anthropic" => "claude-code".to_string(),
        "grok" | "grok-oauth" | "grok_oauth" | "xai" | "xai-oauth" | "xai_oauth" => {
            "grok".to_string()
        }
        "google" | "google-antigravity" | "google_antigravity" | "antigravity"
        | "google-ai-pro" => "google".to_string(),
        "google-gemini-cli" | "google_gemini_cli" | "gemini-cli" | "gemini_cli" | "gemini"
        | "gemini-cli-oauth" | "gemini_cli_oauth" | "code-assist" | "codeassist" => {
            "google-gemini-cli".to_string()
        }
        other => other.to_string(),
    }
}

pub fn build_driver(key: &str) -> Option<Arc<dyn AuthDriver>> {
    match normalize_driver_key(key).as_str() {
        "cursor" => Some(Arc::new(CursorAuthDriver)),
        "devin" => Some(Arc::new(DevinAuthDriver)),
        "codex" => Some(Arc::new(OpenAIOAuthDriver)),
        "claude-code" => Some(Arc::new(ClaudeOAuthDriver)),
        "grok" => Some(Arc::new(GrokOAuthDriver)),
        "google" => Some(Arc::new(GoogleSubscriptionDriver::ANTIGRAVITY)),
        "google-gemini-cli" => Some(Arc::new(GoogleSubscriptionDriver::GEMINI_CLI)),
        _ => None,
    }
}

pub fn list_driver_metadata() -> Vec<AuthDriverMetadata> {
    [
        build_driver("cursor"),
        build_driver("devin"),
        build_driver("codex"),
        build_driver("claude-code"),
        build_driver("grok"),
        build_driver("google"),
        build_driver("google-gemini-cli"),
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
    fn devin_registers_manual_pkce_driver_and_aliases() {
        use crate::auth::{AuthCompletionMode, AuthScheme};
        for alias in ["devin", "DEVIN", "devin-cli"] {
            let driver = build_driver(alias).unwrap();
            assert_eq!(driver.metadata().key, "devin");
            assert_eq!(driver.metadata().scheme, AuthScheme::OAuthAuthCodePkce);
            assert_eq!(driver.completion_mode(), AuthCompletionMode::ManualExchange);
        }
        assert!(
            list_driver_metadata()
                .iter()
                .any(|driver| driver.key == "devin")
        );
    }

    #[test]
    fn grok_aliases_normalize_to_grok_driver() {
        for alias in ["grok", "Grok", "xai", "xai-oauth", "xai_oauth"] {
            assert_eq!(normalize_driver_key(alias), "grok", "alias {alias}");
            assert_eq!(build_driver(alias).unwrap().metadata().key, "grok");
        }
    }

    #[test]
    fn google_aliases_normalize_to_google_driver() {
        for alias in [
            "google",
            "Google",
            "antigravity",
            "google-antigravity",
            "google-ai-pro",
        ] {
            assert_eq!(normalize_driver_key(alias), "google", "alias {alias}");
            assert_eq!(build_driver(alias).unwrap().metadata().key, "google");
        }
    }

    #[test]
    fn gemini_cli_aliases_normalize_to_gemini_cli_driver() {
        for alias in [
            "gemini",
            "Gemini",
            "gemini-cli",
            "gemini_cli",
            "google-gemini-cli",
            "code-assist",
        ] {
            assert_eq!(
                normalize_driver_key(alias),
                "google-gemini-cli",
                "alias {alias}"
            );
            assert_eq!(
                build_driver(alias).unwrap().metadata().key,
                "google-gemini-cli"
            );
        }
    }

    #[test]
    fn cursor_is_pkce_with_poll_completion_and_legacy_drivers_stay_manual() {
        use crate::auth::{AuthCompletionMode, AuthScheme};
        for alias in ["cursor", "CURSOR", "cursor-agent", "cursor-cli"] {
            let driver = build_driver(alias).unwrap();
            assert_eq!(driver.metadata().key, "cursor");
            assert_eq!(driver.metadata().scheme, AuthScheme::OAuthAuthCodePkce);
            assert_eq!(driver.completion_mode(), AuthCompletionMode::Poll);
        }
        for key in [
            "codex",
            "grok",
            "claude-code",
            "google",
            "google-gemini-cli",
        ] {
            assert_eq!(
                build_driver(key).unwrap().completion_mode(),
                AuthCompletionMode::ManualExchange
            );
        }
    }

    #[test]
    fn list_includes_grok() {
        let keys: Vec<&str> = list_driver_metadata().into_iter().map(|m| m.key).collect();
        assert!(keys.contains(&"cursor"));
        assert!(keys.contains(&"grok"));
        assert!(keys.contains(&"codex"));
        assert!(keys.contains(&"claude-code"));
        assert!(keys.contains(&"google"));
        assert!(keys.contains(&"google-gemini-cli"));
    }
}
