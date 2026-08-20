//! Special-case upstream parameter rewrites.
//!
//! Some providers reject parameter values that are valid elsewhere in the
//! ecosystem. When a (provider, model) pair is known to reject a value, the
//! gateway rewrites it to the closest accepted alternative before the request
//! leaves, instead of letting the upstream fail the call with a 400.

use crate::db::models::Provider;
use crate::protocol::ir::{AiRequest, ReasoningEffort};

/// Volcengine Ark rejects `reasoning_effort: none` for GLM models with
/// `InvalidParameter` (live finding 2026-08-17, upstream request id
/// `021786975785068cb7351d9044201d9f0a73472d4e357aec10a67`). Rewriting to
/// `low` keeps cheap no-reasoning calls (title generation, classification)
/// working; the model then runs at its lowest reasoning level.
const VOLCENGINE_NONE_EFFORT_MODEL: &str = "glm-5.3";

fn is_volcengine(provider: &Provider) -> bool {
    if provider
        .vendor
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("volcengine"))
    {
        return true;
    }
    provider.base_url.contains("volces.com")
}

/// Rewrite known-rejected upstream parameters for the selected target.
///
/// Returns `true` when the IR was mutated. The caller must then skip the
/// native request passthrough so the rewritten IR is re-encoded onto the
/// wire: passthrough forwards the raw client body verbatim, and the original
/// body still carries the rejected value.
pub(crate) fn apply_upstream_param_overrides(
    request: &mut AiRequest,
    provider: &Provider,
    model: &str,
) -> bool {
    let reasoning = &mut request.reasoning;
    if is_volcengine(provider)
        && model.eq_ignore_ascii_case(VOLCENGINE_NONE_EFFORT_MODEL)
        && matches!(reasoning.effort.as_ref(), Some(ReasoningEffort::None))
    {
        reasoning.effort = Some(ReasoningEffort::Low);
        tracing::info!(
            provider = %provider.name,
            model = %model,
            "rewrote reasoning effort 'none' to 'low' for upstream compatibility"
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(vendor: Option<&str>, base_url: &str) -> Provider {
        Provider {
            id: "provider-1".to_string(),
            name: "Volcengine".to_string(),
            vendor: vendor.map(str::to_string),
            protocol: "openai-responses".to_string(),
            base_url: base_url.to_string(),
            protocol_mode: "fixed".to_string(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "key".to_string(),
            auth_mode: "apikey".to_string(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn request_with_effort(effort: ReasoningEffort) -> AiRequest {
        let mut request = AiRequest::new("glm-5.3", vec![]);
        request.reasoning.effort = Some(effort);
        request
    }

    #[test]
    fn rewrites_none_to_low_for_volcengine_glm() {
        let provider = provider(None, "https://ark.cn-beijing.volces.com/api/coding/v3");
        let mut request = request_with_effort(ReasoningEffort::None);

        assert!(apply_upstream_param_overrides(
            &mut request,
            &provider,
            "glm-5.3"
        ));
        assert_eq!(request.reasoning.effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn detects_volcengine_via_vendor_field() {
        let provider = provider(Some("Volcengine"), "https://ark.example.com/api/v3");
        let mut request = request_with_effort(ReasoningEffort::None);

        assert!(apply_upstream_param_overrides(
            &mut request,
            &provider,
            "GLM-5.3"
        ));
        assert_eq!(request.reasoning.effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn keeps_other_efforts_and_models() {
        let provider = provider(None, "https://ark.cn-beijing.volces.com/api/coding/v3");

        let mut high = request_with_effort(ReasoningEffort::High);
        assert!(!apply_upstream_param_overrides(
            &mut high, &provider, "glm-5.3"
        ));
        assert_eq!(high.reasoning.effort, Some(ReasoningEffort::High));

        let mut other_model = request_with_effort(ReasoningEffort::None);
        assert!(!apply_upstream_param_overrides(
            &mut other_model,
            &provider,
            "glm-4.6"
        ));
        assert_eq!(other_model.reasoning.effort, Some(ReasoningEffort::None));
    }

    #[test]
    fn never_rewrites_providers_that_accept_none() {
        let provider = provider(None, "https://api.openai.com/v1");
        let mut request = request_with_effort(ReasoningEffort::None);

        assert!(!apply_upstream_param_overrides(
            &mut request,
            &provider,
            "glm-5.3"
        ));
        assert_eq!(request.reasoning.effort, Some(ReasoningEffort::None));
    }
}
