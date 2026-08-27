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

/// One helper backend: a provider row plus the upstream multimodal model
/// on it. Helper selection mirrors target-model selection — any provider,
/// any model — and multiple entries fail over in order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelperBackend {
    /// Provider row id.
    pub provider: String,
    /// Upstream model name on that provider (must accept images).
    pub model: String,
}

/// Per-model vision-shim configuration (`models.vision_shim` JSON column).
///
/// Presence of a parsed configuration with at least one resolvable helper
/// means the shim is enabled for that model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VisionShimConfig {
    /// Legacy single-helper form: upstream model name on the helper provider
    /// (e.g. `"glm-5.3-flash"`). Superseded by `helper_backends` when that
    /// list is non-empty.
    pub helper_model: String,
    /// Legacy single-helper form: helper provider row id. Defaults to the
    /// model route's primary `target_provider` when absent.
    pub helper_provider: Option<String>,
    /// Preferred helper list: provider + model pairs tried in order until one
    /// captions successfully. Lets the helper span different vendors.
    #[serde(default)]
    pub helper_backends: Vec<HelperBackend>,
    /// Maximum images transcribed per request; extras become placeholders.
    pub max_images: usize,
    /// Maximum approximate decoded image size in bytes; larger images become
    /// placeholders instead of being sent to the helper.
    pub max_image_bytes: usize,
    /// Caption cache TTL in seconds.
    pub cache_ttl_secs: u64,
    /// `max_tokens` for the helper caption call. Hybrid-reasoning helpers
    /// spend this budget on thinking first, so it must cover the chain of
    /// thought plus the caption itself.
    pub caption_max_tokens: u32,
    /// Whether to request the helper with thinking disabled
    /// (`thinking: {"type":"disabled"}`), which GLM-family models accept.
    /// `None` (default) auto-detects: disabled for GLM-family vendors
    /// (zhipuai / zai / bigmodel / z.ai), left alone elsewhere.
    pub helper_disable_thinking: Option<bool>,
    /// Failure policy for helper errors.
    pub on_failure: VisionShimFailureMode,
    /// Optional full override of the caption prompt template. `{question}`
    /// is substituted with the user's latest question when present.
    pub prompt_override: Option<String>,
}

/// Auto-detect whether a helper provider belongs to the GLM family (whose
/// OpenAI-compatible endpoints accept `thinking` and whose hybrid models
/// otherwise burn the caption budget on chain-of-thought).
pub(crate) fn provider_is_glm_family(vendor: &str, base_url: &str) -> bool {
    let vendor = vendor.trim().to_ascii_lowercase();
    if matches!(vendor.as_str(), "zhipuai" | "zai" | "glm" | "bigmodel") {
        return true;
    }
    let base = base_url.trim().to_ascii_lowercase();
    base.contains("bigmodel.cn") || base.contains("z.ai")
}

impl Default for VisionShimConfig {
    fn default() -> Self {
        Self {
            helper_model: String::new(),
            helper_provider: None,
            helper_backends: Vec::new(),
            max_images: 8,
            max_image_bytes: 10 * 1024 * 1024,
            cache_ttl_secs: 24 * 60 * 60,
            caption_max_tokens: 2048,
            helper_disable_thinking: None,
            on_failure: VisionShimFailureMode::default(),
            prompt_override: None,
        }
    }
}

impl VisionShimConfig {
    /// A configuration is enabled when any helper is configured.
    pub fn is_enabled(&self) -> bool {
        !self.helper_model.trim().is_empty() || !self.helper_backends.is_empty()
    }

    /// Resolve the ordered helper list. Explicit `helper_backends` take
    /// priority; the legacy single form (`helper_model` + optional
    /// `helper_provider`, defaulting to `default_provider`) is used only
    /// when the list is empty. Empty/incomplete entries are dropped.
    pub fn helper_list(&self, default_provider: &str) -> Vec<HelperBackend> {
        let mut list: Vec<HelperBackend> = self
            .helper_backends
            .iter()
            .map(|backend| HelperBackend {
                provider: backend.provider.trim().to_string(),
                model: backend.model.trim().to_string(),
            })
            .filter(|backend| !backend.provider.is_empty() && !backend.model.is_empty())
            .collect();
        if list.is_empty() && !self.helper_model.trim().is_empty() {
            let provider = self
                .helper_provider
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_provider)
                .to_string();
            list.push(HelperBackend {
                provider,
                model: self.helper_model.trim().to_string(),
            });
        }
        list
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
        assert_eq!(cfg.caption_max_tokens, 2048);
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
    fn helper_backends_take_priority_over_legacy_single_form() {
        let cfg = VisionShimConfig::parse(
            r#"{"helper_model":"legacy-flash","helper_backends":[{"provider":"pB","model":"b-vl"},{"provider":"","model":"broken"},{"provider":"pC","model":""}]}"#,
        )
        .unwrap();
        let list = cfg.helper_list("pDefault");
        assert_eq!(
            list,
            vec![HelperBackend {
                provider: "pB".to_string(),
                model: "b-vl".to_string()
            }]
        );
    }

    #[test]
    fn legacy_single_form_falls_back_to_route_provider() {
        let cfg = VisionShimConfig::parse(r#"{"helper_model":"glm-5.3-flash"}"#).unwrap();
        assert_eq!(
            cfg.helper_list("pRoute"),
            vec![HelperBackend {
                provider: "pRoute".to_string(),
                model: "glm-5.3-flash".to_string()
            }]
        );

        let cfg =
            VisionShimConfig::parse(r#"{"helper_model":"x","helper_provider":"pOther"}"#).unwrap();
        assert_eq!(cfg.helper_list("pRoute")[0].provider, "pOther");
    }

    #[test]
    fn glm_family_detection_covers_vendor_and_base_url() {
        assert!(provider_is_glm_family("zhipuai", "https://example.com"));
        assert!(provider_is_glm_family("zai", "https://example.com"));
        assert!(provider_is_glm_family(
            "",
            "https://open.bigmodel.cn/api/coding/paas/v4"
        ));
        assert!(provider_is_glm_family("", "https://api.z.ai/api/paas/v4"));
        assert!(!provider_is_glm_family(
            "deepseek",
            "https://api.deepseek.com/v1"
        ));
        assert!(!provider_is_glm_family("", "https://api.openai.com/v1"));
    }

    #[test]
    fn backends_only_config_is_enabled() {
        let cfg = VisionShimConfig::parse(r#"{"helper_backends":[{"provider":"p","model":"m"}]}"#)
            .unwrap();
        assert!(cfg.is_enabled());
        assert_eq!(cfg.helper_list("p").len(), 1);
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
