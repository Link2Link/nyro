//! Deterministic protocol negotiation (PR-04).
//!
//! `negotiate()` is a pure function — given the ingress protocol, any
//! route-level preference, the provider's declared endpoints, and the
//! capability matrix (PR-07), it returns a `ProtocolPlan` or a typed error.
//!
//! Determinism guarantee: for identical inputs the output is always identical.
//! Provider declarations preserve endpoint order and resolve exact matches
//! before falling back to the configured default endpoint.

use crate::db::models::ModelBackend;
use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::registry::ProtocolRegistry;
use crate::protocol::{EndpointDescriptor, ExecutionKind, ProviderProtocols};
use crate::proxy::context::RequestContext;

// ── ProtocolPlan ──────────────────────────────────────────────────────────────

/// The fully-resolved protocol decision for one request.
#[derive(Debug, Clone)]
pub struct ProtocolPlan {
    /// The protocol the client used.
    pub ingress: ProtocolId,
    /// The protocol to use when talking to the upstream provider.
    pub egress: ProtocolId,
    /// Transport executor; vendor streams must not enter JSON codec dispatch.
    pub execution: ExecutionKind,
    /// The operational mode.
    pub mode: ProtocolMode,
    /// The base URL for the upstream call (may be overridden by the provider
    /// adapter in PR-13).
    pub base_url: String,
    /// Normalized provider endpoint row selected for adaptive routing.
    pub endpoint_id: Option<String>,
    /// Endpoint-specific authentication strategy.
    pub auth_scheme: String,
    /// Whether the codec must do a lossy transform (fields will be dropped).
    pub needs_conversion: bool,
}

/// How the gateway handles the codec path for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolMode {
    /// Ingress == egress, no codec translation needed.
    Native,
    /// Codec translation required; all fields can be represented.
    Transform,
    /// Codec translation required; some fields will be dropped (only allowed
    /// when `route.allow_lossy == true`).
    LossyTransform,
    /// Bypass codec; forward body verbatim (P2-A, not active yet).
    PassThrough,
    /// No valid egress found; request must be rejected.
    Reject,
}

// ── RoutingStrategy trait ─────────────────────────────────────────────────────

/// Pluggable target-ordering strategy.
///
/// `OrderedStrategy` preserves the DB declaration order (current behaviour).
pub trait RoutingStrategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn select_ordered(&self, targets: &[ModelBackend], _ctx: &RequestContext) -> Vec<ModelBackend>;
}

/// Ordered strategy: target order == DB row order. Preserves the pre-PR-04
/// behaviour of `TargetSelector::select_ordered`.
pub struct OrderedStrategy;

impl RoutingStrategy for OrderedStrategy {
    fn name(&self) -> &'static str {
        "ordered"
    }

    fn select_ordered(&self, targets: &[ModelBackend], _ctx: &RequestContext) -> Vec<ModelBackend> {
        targets.to_vec()
    }
}

/// Resolve the `RoutingStrategy` implementation by name.
///
/// P2-H will expand this to a proper registry.
pub fn get_routing_strategy(name: &str) -> Box<dyn RoutingStrategy> {
    match name.to_ascii_lowercase().as_str() {
        "ordered" | "" => Box::new(OrderedStrategy),
        other => {
            tracing::warn!(
                strategy = other,
                "unknown routing strategy; falling back to ordered"
            );
            Box::new(OrderedStrategy)
        }
    }
}

// ── negotiate() ───────────────────────────────────────────────────────────────

/// Resolve metadata without assuming an egress endpoint owns JSON codecs.
/// Vendor streams are egress-only and cannot service embeddings requests.
fn describe_egress(
    ingress: ProtocolId,
    egress: ProtocolId,
) -> Result<&'static EndpointDescriptor, GatewayError> {
    let registry = ProtocolRegistry::global();
    let unsupported = || GatewayError::ProtocolUnsupported {
        ingress: ingress.to_string(),
        egress: egress.to_string(),
    };
    let input = registry.describe(&ingress).ok_or_else(unsupported)?;
    let output = registry.describe(&egress).ok_or_else(unsupported)?;
    if input.execution != ExecutionKind::JsonHttp
        || (output.execution == ExecutionKind::VendorStream && input.capabilities.embeddings)
    {
        return Err(unsupported());
    }
    Ok(output)
}

/// Determine the egress protocol and operational mode for a single request.
///
/// Priority order:
/// 1. Route-level `egress_protocol` preference (if set and supported).
/// 2. Same protocol suite as the provider.
/// 3. Provider default.
/// 4. Reject.
///
/// The `provider_decl` may be `None` when the provider hasn't declared any
/// protocol (old-style config), in which case we fall back to the ingress
/// protocol and assume `base_url = ""` (the provider adapter fills it in).
pub fn negotiate(
    ingress: ProtocolId,
    route_pref: Option<ProtocolId>,
    provider_decl: Option<&ProviderProtocols>,
    ctx: &mut RequestContext,
) -> Result<ProtocolPlan, GatewayError> {
    // If no provider declarations, pass-through on ingress protocol.
    let Some(decl) = provider_decl else {
        let descriptor = describe_egress(ingress, ingress)?;
        let plan = ProtocolPlan {
            ingress,
            egress: ingress,
            execution: descriptor.execution,
            mode: ProtocolMode::Native,
            base_url: String::new(),
            endpoint_id: None,
            auth_scheme: "auto".to_string(),
            needs_conversion: false,
        };
        ctx.egress_protocol = Some(ingress);
        return Ok(plan);
    };

    // Tier 1: route-level preference.
    if let Some(pref) = route_pref {
        if decl.supports(pref) {
            let descriptor = describe_egress(ingress, pref)?;
            let endpoint = decl
                .get(pref)
                .expect("supported provider endpoint must be retrievable");
            let mode = if pref == ingress {
                ProtocolMode::Native
            } else {
                ProtocolMode::Transform
            };
            ctx.egress_protocol = Some(pref);
            ctx.trace("negotiate", format!("route_pref exact: {pref}"));
            return Ok(ProtocolPlan {
                ingress,
                egress: pref,
                execution: descriptor.execution,
                mode,
                base_url: endpoint.base_url.clone(),
                endpoint_id: endpoint.record_id.clone(),
                auth_scheme: endpoint.auth_scheme.clone(),
                needs_conversion: pref != ingress,
            });
        }
        ctx.trace(
            "negotiate",
            format!("route_pref {pref} not supported by provider; falling through"),
        );
    }

    // Tiers 2–4: delegate to ProviderProtocols::resolve_egress.
    let resolved =
        decl.resolve_egress(ingress)
            .ok_or_else(|| GatewayError::ProtocolUnsupported {
                ingress: ingress.to_string(),
                egress: decl.default.to_string(),
            })?;

    // Check lossy-reject policy from the egress endpoint's capability matrix.
    let descriptor = describe_egress(ingress, resolved.protocol)?;
    let egress_caps = descriptor.capabilities;

    if resolved.needs_conversion && egress_caps.lossy_default_reject {
        // Lossy transform — rejected by default unless the route explicitly
        // opts in via `allow_lossy` (to be threaded in a later PR).
        // For now, we allow it but set the mode to `LossyTransform` so the
        // dispatcher can surface a warning.
        ctx.trace(
            "negotiate",
            format!("lossy transform: {} → {}", ingress, resolved.protocol),
        );
    }

    let mode = if !resolved.needs_conversion {
        ProtocolMode::Native
    } else {
        ProtocolMode::Transform
    };

    ctx.egress_protocol = Some(resolved.protocol);
    ctx.trace(
        "negotiate",
        format!(
            "resolved: {} → {} ({})",
            ingress,
            resolved.protocol,
            if resolved.needs_conversion {
                "transform"
            } else {
                "native"
            }
        ),
    );

    Ok(ProtocolPlan {
        ingress,
        egress: resolved.protocol,
        execution: descriptor.execution,
        mode,
        base_url: resolved.base_url,
        endpoint_id: resolved.endpoint_id,
        auth_scheme: resolved.auth_scheme,
        needs_conversion: resolved.needs_conversion,
    })
}

/// Build a plan for a model-required endpoint already validated by the vendor
/// resolver. Unlike `negotiate`'s soft route preference, this never falls back.
/// Keeping the selected endpoint intact makes dispatch and probes agree on the
/// exact endpoint, URL and authentication scheme.
pub(crate) fn negotiate_required(
    ingress: ProtocolId,
    endpoint: crate::protocol::ProviderProtocolTarget,
    ctx: &mut RequestContext,
) -> Result<ProtocolPlan, GatewayError> {
    let egress = endpoint.protocol;
    let descriptor = describe_egress(ingress, egress)?;
    let ingress_caps = ProtocolRegistry::global()
        .describe(&ingress)
        .expect("ingress descriptor validated above")
        .capabilities;
    if ingress != egress && (ingress_caps.embeddings || descriptor.capabilities.embeddings) {
        return Err(GatewayError::ProtocolUnsupported {
            ingress: ingress.to_string(),
            egress: egress.to_string(),
        });
    }
    ctx.egress_protocol = Some(egress);
    ctx.trace("negotiate", format!("model_required exact: {egress}"));
    Ok(ProtocolPlan {
        ingress,
        egress,
        execution: descriptor.execution,
        mode: if ingress == egress {
            ProtocolMode::Native
        } else {
            ProtocolMode::Transform
        },
        base_url: endpoint.base_url,
        endpoint_id: endpoint.record_id,
        auth_scheme: endpoint.auth_scheme,
        needs_conversion: ingress != egress,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPENAI_COMPATIBLE_EMBEDDINGS_V1,
    };
    use std::time::Duration;

    fn make_decl(default: ProtocolId, base_url: &str) -> ProviderProtocols {
        ProviderProtocols {
            default,
            endpoints: vec![crate::protocol::ProviderProtocolTarget {
                record_id: None,
                protocol: default,
                base_url: base_url.to_string(),
                auth_scheme: "auto".to_string(),
            }],
            adaptive: true,
        }
    }

    fn ctx() -> RequestContext {
        RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Duration::from_secs(30),
        )
    }

    #[test]
    fn native_when_ingress_exact_match() {
        let decl = make_decl(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "https://api.openai.com",
        );
        let mut c = ctx();
        let plan = negotiate(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            None,
            Some(&decl),
            &mut c,
        )
        .unwrap();
        assert_eq!(plan.mode, ProtocolMode::Native);
        assert_eq!(plan.egress, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        assert!(!plan.needs_conversion);
    }

    #[test]
    fn embeddings_require_an_exact_endpoint() {
        let decl = make_decl(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "https://api.openai.com",
        );
        let mut c = ctx();
        let error = negotiate(OPENAI_COMPATIBLE_EMBEDDINGS_V1, None, Some(&decl), &mut c)
            .expect_err("embeddings must not fall back to chat");
        assert!(matches!(error, GatewayError::ProtocolUnsupported { .. }));
    }

    #[test]
    fn route_pref_in_same_protocol_wins_over_ingress_match() {
        let mut decl = make_decl(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "https://api.openai.com",
        );
        decl.endpoints
            .push(crate::protocol::ProviderProtocolTarget {
                record_id: Some("embeddings-endpoint".to_string()),
                protocol: OPENAI_COMPATIBLE_EMBEDDINGS_V1,
                base_url: "https://embeddings.example/v1".to_string(),
                auth_scheme: "bearer".to_string(),
            });
        let mut c = ctx();
        let plan = negotiate(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Some(OPENAI_COMPATIBLE_EMBEDDINGS_V1),
            Some(&decl),
            &mut c,
        )
        .unwrap();
        assert_eq!(plan.egress, OPENAI_COMPATIBLE_EMBEDDINGS_V1);
        assert_eq!(plan.base_url, "https://embeddings.example/v1");
        assert_eq!(plan.endpoint_id.as_deref(), Some("embeddings-endpoint"));
        assert_eq!(plan.auth_scheme, "bearer");
    }

    #[test]
    fn required_plan_preserves_endpoint_and_rejects_embeddings_conversion() {
        let endpoint = crate::protocol::ProviderProtocolTarget {
            record_id: Some("messages".into()),
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            base_url: "https://example.test/go".into(),
            auth_scheme: "x-api-key".into(),
        };
        let mut c = ctx();
        for (ingress, mode) in [
            (ANTHROPIC_MESSAGES_2023_06_01, ProtocolMode::Native),
            (
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                ProtocolMode::Transform,
            ),
        ] {
            let plan = negotiate_required(ingress, endpoint.clone(), &mut c).unwrap();
            assert_eq!(plan.egress, endpoint.protocol);
            assert_eq!(plan.mode, mode);
            assert_eq!(plan.endpoint_id, endpoint.record_id);
            assert_eq!(plan.auth_scheme, "x-api-key");
            assert_eq!(plan.base_url, endpoint.base_url);
        }
        assert!(negotiate_required(OPENAI_COMPATIBLE_EMBEDDINGS_V1, endpoint, &mut c).is_err());
    }

    #[test]
    fn no_decl_returns_native() {
        let mut c = ctx();
        let plan = negotiate(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, None, None, &mut c).unwrap();
        assert_eq!(plan.mode, ProtocolMode::Native);
        assert_eq!(plan.egress, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    }

    #[test]
    fn kiro_plans_use_vendor_execution_for_every_json_ingress() {
        use crate::protocol::ids::{KIRO_CHAT_V1, KIRO_DEFAULT_BASE_URL, OPENAI_RESPONSES_V1};
        let decl = make_decl(KIRO_CHAT_V1, KIRO_DEFAULT_BASE_URL);
        let mut c = ctx();
        for ingress in [
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
        ] {
            for pref in [None, Some(KIRO_CHAT_V1)] {
                let plan = negotiate(ingress, pref, Some(&decl), &mut c).unwrap();
                assert_eq!(plan.egress, KIRO_CHAT_V1);
                assert_eq!(plan.execution, ExecutionKind::VendorStream);
                assert!(plan.needs_conversion);
            }
        }
        // Kiro is egress-only: it has no ingress route and no embeddings.
        assert!(negotiate(KIRO_CHAT_V1, None, Some(&decl), &mut c).is_err());
        assert!(negotiate(OPENAI_COMPATIBLE_EMBEDDINGS_V1, None, Some(&decl), &mut c).is_err());
    }

    #[test]
    fn devin_default_preferred_and_required_plans_use_vendor_execution() {
        use crate::protocol::ids::{DEVIN_BASE_URL, DEVIN_CHAT_V1};
        let decl = make_decl(DEVIN_CHAT_V1, DEVIN_BASE_URL);
        let mut c = ctx();
        for pref in [None, Some(DEVIN_CHAT_V1)] {
            let plan = negotiate(ANTHROPIC_MESSAGES_2023_06_01, pref, Some(&decl), &mut c).unwrap();
            assert_eq!(plan.egress, DEVIN_CHAT_V1);
            assert_eq!(plan.execution, ExecutionKind::VendorStream);
            assert_eq!(plan.mode, ProtocolMode::Transform);
            assert!(plan.needs_conversion);
        }
        let plan = negotiate_required(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            decl.endpoints[0].clone(),
            &mut c,
        )
        .unwrap();
        assert_eq!(plan.execution, ExecutionKind::VendorStream);
        for pref in [None, Some(DEVIN_CHAT_V1)] {
            assert!(negotiate(OPENAI_COMPATIBLE_EMBEDDINGS_V1, pref, Some(&decl), &mut c).is_err());
            assert!(negotiate(DEVIN_CHAT_V1, pref, Some(&decl), &mut c).is_err());
        }
        assert!(negotiate(DEVIN_CHAT_V1, None, None, &mut c).is_err());
        assert!(negotiate_required(DEVIN_CHAT_V1, decl.endpoints[0].clone(), &mut c).is_err());
        assert!(
            negotiate_required(
                OPENAI_COMPATIBLE_EMBEDDINGS_V1,
                decl.endpoints[0].clone(),
                &mut c
            )
            .is_err()
        );
    }

    #[test]
    fn all_legacy_protocols_keep_json_http_execution() {
        use crate::protocol::ids::{GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPENAI_RESPONSES_V1};
        let mut c = ctx();
        for id in [
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_RESPONSES_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_EMBEDDINGS_V1,
        ] {
            let decl = make_decl(id, "https://example.test");
            for pref in [None, Some(id)] {
                assert_eq!(
                    negotiate(id, pref, Some(&decl), &mut c).unwrap().execution,
                    ExecutionKind::JsonHttp
                );
            }
            assert_eq!(
                negotiate(id, None, None, &mut c).unwrap().execution,
                ExecutionKind::JsonHttp
            );
            assert_eq!(
                negotiate_required(id, decl.endpoints[0].clone(), &mut c)
                    .unwrap()
                    .execution,
                ExecutionKind::JsonHttp
            );
        }
    }

    #[test]
    fn different_protocol_falls_back_to_default() {
        let decl = make_decl(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "https://api.openai.com",
        );
        let mut c = ctx();
        let plan = negotiate(ANTHROPIC_MESSAGES_2023_06_01, None, Some(&decl), &mut c).unwrap();
        assert_eq!(plan.egress, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        assert_eq!(plan.mode, ProtocolMode::Transform);
        assert!(plan.needs_conversion);
    }
}
