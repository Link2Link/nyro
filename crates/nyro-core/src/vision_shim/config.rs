//! Vision-shim configuration types.
//!
//! The vision shim gives a text-only target model a multimodal facade: images
//! in the incoming request are transcribed to dense text by a helper
//! multimodal model before the request reaches the target. The configuration
//! is stored per-model as a JSON object in the `models.vision_shim` column.

use serde::{Deserialize, Serialize};

/// Bumped whenever the built-in caption prompt changes in a way that would
/// produce different captions; part of the caption cache key.
pub const CAPTION_PROMPT_VERSION: u8 = 1;

/// What happens to an image when the helper model cannot transcribe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VisionShimFailureMode {
    /// Replace the image with a placeholder note and let the request proceed
    /// (default — the facade must never leak "unsupported image" errors).
    #[default]
    Placeholder,
    /// Reject the whole request with an upstream error.
    Reject,
}

/// Per-model vision-shim configuration (`models.vision_shim` JSON column).
///
/// Presence of a parsed configuration with a non-empty `helper_model` means
/// the shim is enabled for that model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VisionShimConfig {
    /// Upstream model name on the helper provider that accepts images
    /// (e.g. `"glm-5.3-flash"`).
    pub helper_model: String,
    /// Helper provider row id. Defaults to the model route's primary
    /// `target_provider` when absent.
    pub helper_provider: Option<String>,
    /// Maximum images transcribed per request; extras become placeholders.
    pub max_images: usize,
    /// Maximum approximate decoded image size in bytes; larger images become
    /// placeholders instead of being sent to the helper.
    pub max_image_bytes: usize,
    /// Caption cache TTL in seconds.
    pub cache_ttl_secs: u64,
    /// `max_tokens` for the helper caption call.
    pub caption_max_tokens: u32,
    /// Failure policy for helper errors.
    pub on_failure: VisionShimFailureMode,
    /// Optional full override of the caption prompt template. `{question}`
    /// is substituted with the user's latest question when present.
    pub prompt_override: Option<String>,
}

impl Default for VisionShimConfig {
    fn default() -> Self {
        Self {
            helper_model: String::new(),
            helper_provider: None,
            max_images: 8,
            max_image_bytes: 10 * 1024 * 1024,
            cache_ttl_secs: 24 * 60 * 60,
            caption_max_tokens: 1024,
            on_failure: VisionShimFailureMode::default(),
            prompt_override: None,
        }
    }
}

impl VisionShimConfig {
    /// A configuration is enabled when a helper model is configured.
    pub fn is_enabled(&self) -> bool {
        !self.helper_model.trim().is_empty()
    }

    /// Parse the raw JSON string stored on the model row. Returns `None` for
    /// empty/invalid payloads or configurations without a helper model.
    pub fn parse(raw: &str) -> Option<Self> {
        let cfg: Self = serde_json::from_str(raw.trim()).ok()?;
        cfg.is_enabled().then_some(cfg)
    }

    /// Build the caption prompt for one image.
    ///
    /// Two flavours share one cache namespace version:
    /// - generic (history images): stable across turns, maximises cache hits;
    /// - question-aware (latest user message images): folds the user's current
    ///   question in so the helper emphasises the relevant details.
    pub fn caption_prompt(&self, question: Option<&str>) -> String {
        if let Some(template) = self
            .prompt_override
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return match question {
                Some(q) => template.replace("{question}", q),
                None => template.replace("{question}", ""),
            };
        }
        let mut prompt = String::from(
            "You are a visual information extractor. A text-only reasoning model will answer \
about this image using only your description, so be precise and complete:\n\
1) Transcribe all visible text verbatim, preserving reading order and layout cues.\n\
2) Describe objects, people, UI elements, layout, and colors precisely.\n\
3) For charts, tables, or diagrams, enumerate the data values.\n\
4) Reproduce any code, commands, or error messages exactly.\n\
Reply with dense plain text only. No preamble, no headings.",
        );
        if let Some(question) = question.map(str::trim).filter(|q| !q.is_empty()) {
            let question = truncate_chars(question, 2000);
            prompt.push_str(&format!(
                "\n\nThe user's current request about this image:\n\"{question}\"\n\
Emphasize the details needed to answer it, while still covering the rest of the image."
            ));
        }
        prompt
    }
}

/// Character-boundary truncation that never splits a char.
fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_partial_config_with_defaults() {
        let cfg = VisionShimConfig::parse(r#"{"helper_model":"glm-5.3-flash"}"#)
            .expect("minimal config must parse");
        assert_eq!(cfg.helper_model, "glm-5.3-flash");
        assert!(cfg.helper_provider.is_none());
        assert_eq!(cfg.max_images, 8);
        assert_eq!(cfg.max_image_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.cache_ttl_secs, 86_400);
        assert_eq!(cfg.caption_max_tokens, 1024);
        assert_eq!(cfg.on_failure, VisionShimFailureMode::Placeholder);
        assert!(cfg.is_enabled());
    }

    #[test]
    fn parse_rejects_empty_and_helperless_config() {
        assert!(VisionShimConfig::parse("").is_none());
        assert!(VisionShimConfig::parse("{}").is_none());
        assert!(VisionShimConfig::parse("not json").is_none());
        assert!(VisionShimConfig::parse(r#"{"max_images":4}"#).is_none());
    }

    #[test]
    fn caption_prompt_is_question_aware_when_question_present() {
        let cfg = VisionShimConfig::default();
        let generic = cfg.caption_prompt(None);
        let aware = cfg.caption_prompt(Some("what error is shown?"));
        assert!(generic.contains("visual information extractor"));
        assert!(!generic.contains("what error is shown?"));
        assert!(aware.contains("what error is shown?"));
    }

    #[test]
    fn prompt_override_replaces_question_placeholder() {
        let cfg = VisionShimConfig {
            prompt_override: Some("Describe for: {question}".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.caption_prompt(Some("chart")), "Describe for: chart");
        assert_eq!(cfg.caption_prompt(None), "Describe for: ");
    }
}
