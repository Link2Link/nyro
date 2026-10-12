//! Authoritative legacy IDs with optional, exact-tuple discovery enrichment.
//! No aliases, suffix parsing, family fallback, synthetic variants or price guesses.
use super::{discovery, errors::CursorError, framing::MAX_INFLATED_FRAME, wire};
use crate::protocol::ir::{ReasoningConfig, ReasoningEffort};
use prost::Message;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct CursorModelRoute {
    pub legacy_model_id: String,
    pub requested_model_id: String,
    pub parameters: Vec<wire::ModelParameter>,
    pub max_mode: Option<bool>,
}
impl CursorModelRoute {
    /// Exact-ID passthrough; does not infer reasoning, max mode, or family.
    pub fn exact(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            legacy_model_id: id.clone(),
            requested_model_id: id,
            parameters: vec![],
            max_mode: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct CursorModel {
    pub id: String,
    pub display_name: Option<String>,
    pub route: CursorModelRoute,
    /// Only present after a unique, exact variant.legacy_slug binding.
    pub metadata: Option<CursorModelMetadata>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorModelMetadata {
    pub supports_agent: Option<bool>,
    pub supports_thinking: Option<bool>,
    pub supports_images: Option<bool>,
    /// Server-reported base/max limit; never inferred from names or prices.
    pub context_token_limit: Option<u32>,
    definitions: Arc<[discovery::ModelParameterDefinition]>,
}
/// This is a *bare protobuf* unary body, not a Connect envelope.
pub fn encode_get_usable_models_request() -> Vec<u8> {
    wire::GetUsableModelsRequest::default().encode_to_vec()
}
pub fn decode_get_usable_models_response(body: &[u8]) -> Result<Vec<CursorModel>, CursorError> {
    if body.len() > MAX_INFLATED_FRAME {
        return Err(CursorError::LimitExceeded {
            resource: "catalog body",
            limit: MAX_INFLATED_FRAME,
        });
    }
    let response = wire::GetUsableModelsResponse::decode(body)?;
    let mut seen = std::collections::HashSet::new();
    response
        .models
        .into_iter()
        .map(|model| {
            if model.model_id.is_empty() {
                return Err(CursorError::Protocol(
                    "catalog contains empty model ID".into(),
                ));
            }
            if !seen.insert(model.model_id.clone()) {
                return Err(CursorError::Protocol(
                    "catalog contains duplicate model ID".into(),
                ));
            }
            Ok(CursorModel {
                route: CursorModelRoute {
                    max_mode: model.max_mode,
                    ..CursorModelRoute::exact(model.model_id.clone())
                },
                id: model.model_id,
                display_name: (!model.display_name.is_empty()).then_some(model.display_name),
                metadata: None,
            })
        })
        .collect()
}
/// Rich discovery is optional; this request/response is bare application/proto.
pub fn encode_available_models_request() -> Vec<u8> {
    discovery::AvailableModelsRequest {
        include_long_context_models: true,
        use_model_parameters: Some(true),
        do_not_use_markdown: Some(true),
        use_cloud_agent_effort_modes: Some(true),
    }
    .encode_to_vec()
}

pub fn encode_get_default_model_for_cli_request() -> Vec<u8> {
    discovery::GetDefaultModelForCliRequest {}.encode_to_vec()
}

/// Informational only: this must never replace an explicit `auto` or other ID.
pub fn decode_get_default_model_for_cli_response(
    body: &[u8],
) -> Result<Option<String>, CursorError> {
    check_body(body)?;
    let response = discovery::GetDefaultModelForCliResponse::decode(body)?;
    match response.model {
        Some(model) if model.model_id.is_empty() => {
            Err(CursorError::Protocol("empty default model ID".into()))
        }
        model => Ok(model.map(|model| model.model_id)),
    }
}

fn check_body(body: &[u8]) -> Result<(), CursorError> {
    if body.len() > MAX_INFLATED_FRAME {
        return Err(CursorError::LimitExceeded {
            resource: "catalog body",
            limit: MAX_INFLATED_FRAME,
        });
    }
    Ok(())
}

/// Enrich only exact legacy IDs from a successfully decoded GetUsableModels list.
/// All changes are staged: malformed/ambiguous enrichment leaves the input intact.
/// Empty authority remains empty; rich base names and aliases grant no entitlement.
pub fn decode_enrich_models(body: &[u8], models: &mut [CursorModel]) -> Result<(), CursorError> {
    check_body(body)?;
    let response = discovery::AvailableModelsResponse::decode(body)?;
    let indices: HashMap<_, _> = models
        .iter()
        .enumerate()
        .map(|(index, model)| (model.id.as_str(), index))
        .collect();
    let mut pending = Vec::new();
    let mut bound = HashSet::new();
    for details in response.models {
        // Definitions are shared, not duplicated for every variant in a base.
        let definitions: Arc<[discovery::ModelParameterDefinition]> =
            details.parameter_definitions.into();
        let mut validated = false;
        for variant in details.variants {
            let Some(index) = variant
                .legacy_slug
                .as_deref()
                .and_then(|id| indices.get(id))
                .copied()
            else {
                continue;
            };
            if details.name.is_empty() || !bound.insert(index) {
                return Err(CursorError::Protocol(
                    "rich catalog has an empty base or ambiguous legacy binding".into(),
                ));
            }
            if !validated {
                validate_definitions(&definitions)?;
                validated = true;
            }
            let original = &models[index];
            if let (Some(authoritative), Some(rich)) =
                (original.route.max_mode, variant.is_max_mode)
            {
                if authoritative != rich {
                    return Err(CursorError::Protocol(
                        "rich variant conflicts with authoritative max mode".into(),
                    ));
                }
            }
            validate_parameters(&variant.parameter_values, &definitions)?;
            let max_mode = variant.is_max_mode.or(original.route.max_mode);
            let metadata = CursorModelMetadata {
                supports_agent: details.supports_agent,
                supports_thinking: details.supports_thinking,
                supports_images: details.supports_images,
                context_token_limit: if max_mode == Some(true) {
                    details.context_token_limit_for_max_mode
                } else {
                    details.context_token_limit
                },
                definitions: definitions.clone(),
            };
            let route = CursorModelRoute {
                legacy_model_id: original.id.clone(),
                requested_model_id: details.name.clone(),
                parameters: variant.parameter_values,
                max_mode,
            };
            let display_name = variant
                .display_name
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    details
                        .client_display_name
                        .clone()
                        .filter(|name| !name.is_empty())
                });
            pending.push((index, route, metadata, display_name));
        }
    }
    for (index, route, metadata, display_name) in pending {
        models[index].route = route;
        models[index].metadata = Some(metadata);
        // Prefer the authoritative variant's existing label over a base label.
        if models[index].display_name.is_none() {
            models[index].display_name = display_name;
        }
    }
    Ok(())
}

fn validate_definitions(
    definitions: &[discovery::ModelParameterDefinition],
) -> Result<(), CursorError> {
    let mut ids = HashSet::new();
    for definition in definitions {
        if definition.id.is_empty() || !ids.insert(&definition.id) {
            return Err(CursorError::Protocol(
                "ambiguous rich parameter definition".into(),
            ));
        }
        let Some(kind) = &definition.parameter_type else {
            continue;
        };
        if kind.boolean_parameter.is_some() && kind.enum_parameter.is_some() {
            return Err(CursorError::Protocol(
                "ambiguous rich parameter type".into(),
            ));
        }
        let mut values = HashSet::new();
        for (value, _, _) in definition_values(definition) {
            if !values.insert(value) {
                return Err(CursorError::Protocol(
                    "duplicate rich parameter value".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_parameters(
    parameters: &[wire::ModelParameter],
    definitions: &[discovery::ModelParameterDefinition],
) -> Result<(), CursorError> {
    let mut ids = HashSet::new();
    for parameter in parameters {
        if parameter.id.is_empty() || !ids.insert(&parameter.id) {
            return Err(CursorError::Protocol(
                "ambiguous rich variant parameter".into(),
            ));
        }
        if let Some(definition) = definitions.iter().find(|d| d.id == parameter.id) {
            let values = definition_values(definition);
            if !values.is_empty() && !values.iter().any(|(v, _, _)| *v == parameter.value) {
                return Err(CursorError::Protocol(
                    "undeclared rich variant parameter value".into(),
                ));
            }
        }
    }
    Ok(())
}

/// (literal wire value, cost-bearing flag, blocked flag). No price estimation.
fn definition_values(definition: &discovery::ModelParameterDefinition) -> Vec<(&str, bool, bool)> {
    let Some(kind) = &definition.parameter_type else {
        return vec![];
    };
    if let Some(boolean) = &kind.boolean_parameter {
        return boolean
            .values
            .iter()
            .map(|value| {
                (
                    value.value.as_str(),
                    value.increases_model_cost == Some(true),
                    value.blocked_by_admin_allowlist == Some(true),
                )
            })
            .collect();
    }
    kind.enum_parameter
        .as_ref()
        .map(|enumeration| {
            enumeration
                .values
                .iter()
                .map(|value| {
                    (
                        value.value.as_str(),
                        value.increases_model_cost == Some(true),
                        value.blocked_by_admin_allowlist == Some(true),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredReasoning {
    Off,
    Enabled,
    Effort(u8),
}

fn effort_rank(effort: &ReasoningEffort) -> Option<u8> {
    match effort {
        ReasoningEffort::Minimal => Some(1),
        ReasoningEffort::Low => Some(2),
        ReasoningEffort::Medium => Some(3),
        ReasoningEffort::High => Some(4),
        ReasoningEffort::Xhigh => Some(5),
        ReasoningEffort::Max => Some(6),
        ReasoningEffort::None | ReasoningEffort::Budget(_) => None,
    }
}

/// Gateway-wide budget→tier translation, threshold-identical to the Claude 5.5
/// relay rewrite in `provider::common::effort_policy` (`budget_to_level`) so the
/// same client budget behaves consistently across providers. Callers > 0 only.
fn budget_effort(budget: u32) -> ReasoningEffort {
    match budget {
        b if b > 10240 => ReasoningEffort::Max,
        b if b > 8192 => ReasoningEffort::Xhigh,
        b if b > 1024 => ReasoningEffort::High,
        _ => ReasoningEffort::Medium,
    }
}

// These are protocol parameter IDs recognized by the pinned discovery client,
// not inferred model families or substrings of a model/parameter display name.
fn is_reasoning_definition(definition: &discovery::ModelParameterDefinition) -> bool {
    is_reasoning_parameter_id(&definition.id)
}

fn is_reasoning_parameter_id(id: &str) -> bool {
    matches!(
        id,
        "effort" | "reasoning" | "reasoning_effort" | "thinking" | "thinking_effort"
    )
}

fn is_blocked(model: &CursorModel) -> bool {
    model.metadata.as_ref().is_some_and(|metadata| {
        model.route.parameters.iter().any(|parameter| {
            metadata
                .definitions
                .iter()
                .find(|d| d.id == parameter.id)
                .is_some_and(|definition| {
                    definition_values(definition)
                        .iter()
                        .any(|(v, _, blocked)| *v == parameter.value && *blocked)
                })
        })
    })
}

fn declared_value(
    definition: &discovery::ModelParameterDefinition,
    value: &str,
) -> Option<DeclaredReasoning> {
    if !is_reasoning_definition(definition)
        || !definition_values(definition)
            .iter()
            .any(|(v, _, blocked)| *v == value && !blocked)
    {
        return None;
    }
    let normalized = value.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "none" | "off" | "disabled" | "false" => Some(DeclaredReasoning::Off),
        "true" | "on" | "enabled" => Some(DeclaredReasoning::Enabled),
        "minimal" => Some(DeclaredReasoning::Effort(1)),
        "low" => Some(DeclaredReasoning::Effort(2)),
        "medium" => Some(DeclaredReasoning::Effort(3)),
        "high" => Some(DeclaredReasoning::Effort(4)),
        "xhigh" | "x-high" | "extra-high" => Some(DeclaredReasoning::Effort(5)),
        "max" => Some(DeclaredReasoning::Effort(6)),
        _ => None,
    }
}

fn declared_reasoning(model: &CursorModel) -> Option<DeclaredReasoning> {
    let metadata = model.metadata.as_ref()?;
    let mut state = None;
    for parameter in &model.route.parameters {
        let Some(definition) = metadata.definitions.iter().find(|d| d.id == parameter.id) else {
            // A recognized reasoning ID without its declaration is not proof
            // of an effort, even when another declared reasoning axis exists.
            if is_reasoning_parameter_id(&parameter.id) {
                return None;
            }
            continue;
        };
        if definition_values(definition)
            .iter()
            .any(|(v, _, blocked)| *v == parameter.value && *blocked)
        {
            return None;
        }
        if !is_reasoning_definition(definition) {
            continue;
        }
        let selected = declared_value(definition, &parameter.value)?;
        state = match (state, selected) {
            (None, value) => Some(value),
            (Some(DeclaredReasoning::Enabled), value)
            | (Some(value), DeclaredReasoning::Enabled)
                if value != DeclaredReasoning::Off =>
            {
                Some(value)
            }
            (Some(value), other) if value == other => Some(value),
            _ => return None, // conflicting toggles/efforts are not guessed
        };
    }
    state
}

fn lane_parameters(model: &CursorModel) -> BTreeMap<&str, &str> {
    model
        .route
        .parameters
        .iter()
        .filter(|parameter| {
            let definition = model
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.definitions.iter().find(|d| d.id == parameter.id));
            // Keep every unknown/non-reasoning axis, and even a reasoning axis if
            // the server declares any of its values cost-bearing.
            !definition.is_some_and(|definition| {
                is_reasoning_definition(definition)
                    && !definition_values(definition)
                        .iter()
                        .any(|(_, cost, _)| *cost)
            })
        })
        .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
        .collect()
}

fn same_lane(left: &CursorModel, right: &CursorModel) -> bool {
    match (&left.metadata, &right.metadata) {
        (Some(a), Some(b)) => {
            left.route.requested_model_id == right.route.requested_model_id
                && left.route.max_mode == right.route.max_mode
                && a.definitions == b.definitions
                && lane_parameters(left) == lane_parameters(right)
        }
        _ => false,
    }
}

/// Resolve an exact entitled ID, then apply only server-declared reasoning tuples.
/// `force_max` selects the highest supported effort in the same cost/context lane;
/// it never enables Cursor's separately billed `max_mode`. Unsupported controls
/// and ambiguous selections fail closed instead of reverting to the raw route.
///
/// A client thinking budget (`budget_tokens` / effort=`budget`) has no Cursor wire
/// equivalent: the Run protocol exposes discrete declared tiers, not token counts.
/// It is translated to a qualitative tier with the same gateway-wide threshold
/// table as the Claude 5.5 relay rewrite (`effort_policy::budget_to_level`), and a
/// budget-derived tier overrides a concurrently supplied qualitative effort (the
/// documented CPA extraction priority). The translated tier must still be declared
/// by authoritative lane metadata; absent tiers fail closed below instead of
/// silently degrading to the raw route.
pub fn resolve_route(
    models: &[CursorModel],
    exactid: &str,
    reasoning: &ReasoningConfig,
    force_max: bool,
) -> Result<CursorModelRoute, CursorError> {
    let unsupported = || {
        CursorError::Unsupported(
            "reasoning has no unambiguous declared Cursor route in this exact model lane".into(),
        )
    };
    if reasoning.display.is_some() {
        return Err(CursorError::Unsupported(
            "reasoning display/summary mode is not mapped by Cursor metadata".into(),
        ));
    }
    let budget_tokens = match (reasoning.budget_tokens, reasoning.effort.as_ref()) {
        (budget, Some(ReasoningEffort::Budget(tokens))) => {
            budget.filter(|tokens| *tokens > 0).or(Some(*tokens))
        }
        (budget, _) => budget,
    }
    .filter(|tokens| *tokens > 0);
    let budget_tier = budget_tokens.map(budget_effort);
    // Budget implies thinking; a translated tier overrides a supplied effort.
    // The explicit `None` effort (thinking off) is kept — only `Budget(_)`
    // is absorbed into the tier translation above.
    let enabled = reasoning.enabled || budget_tier.is_some();
    let effort = budget_tier.or_else(|| {
        reasoning.effort.as_ref().and_then(|effort| match effort {
            ReasoningEffort::Budget(_) => None,
            qualitative => Some(qualitative.clone()),
        })
    });
    let mut matching = models.iter().filter(|model| model.id == exactid);
    let exact = matching.next().ok_or_else(|| {
        CursorError::Unsupported(
            "exact model ID is absent from authoritative Cursor catalog".into(),
        )
    })?;
    if matching.next().is_some() {
        return Err(unsupported());
    }
    if !force_max && !enabled && effort.is_none() {
        return Ok(exact.route.clone());
    }
    let current = declared_reasoning(exact);
    let mut candidates: Vec<_> = models
        .iter()
        .filter(|model| same_lane(exact, model))
        .filter_map(|model| declared_reasoning(model).map(|state| (model, state)))
        .collect();
    if force_max {
        // Schema drift must not make "highest" silently mean "highest we
        // happened to recognize". A runnable unknown effort/toggle in this
        // lane makes the ordering unprovable; blocked variants do not count.
        if models.iter().any(|model| {
            same_lane(exact, model)
                && !is_blocked(model)
                && model
                    .route
                    .parameters
                    .iter()
                    .any(|p| is_reasoning_parameter_id(&p.id))
                && declared_reasoning(model).is_none()
        }) {
            return Err(unsupported());
        }
        let highest = candidates
            .iter()
            .filter_map(|(_, state)| match state {
                DeclaredReasoning::Effort(rank) => Some(*rank),
                _ => None,
            })
            .max();
        candidates.retain(|(_, state)| match highest {
            Some(rank) => *state == DeclaredReasoning::Effort(rank),
            None => *state == DeclaredReasoning::Enabled,
        });
    } else if let Some(effort) = &effort {
        let wanted = if *effort == ReasoningEffort::None {
            DeclaredReasoning::Off
        } else {
            DeclaredReasoning::Effort(effort_rank(effort).ok_or_else(unsupported)?)
        };
        candidates.retain(|(_, state)| *state == wanted);
    } else {
        // enabled=true carries no effort preference: preserve an already active
        // exact tuple; otherwise a unique declared active tuple is required.
        if matches!(
            current,
            Some(DeclaredReasoning::Enabled | DeclaredReasoning::Effort(_))
        ) {
            return Ok(exact.route.clone());
        }
        candidates.retain(|(_, state)| *state != DeclaredReasoning::Off);
    }
    if let Some((selected, _)) = candidates.iter().find(|(model, _)| model.id == exactid) {
        return Ok(selected.route.clone());
    }
    if candidates.len() != 1 {
        return Err(unsupported());
    }
    Ok(candidates[0].0.route.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_ids_and_unknown_fields_without_aliases() {
        let mut body = wire::GetUsableModelsResponse {
            models: vec![wire::ModelDetails {
                model_id: "Some-Exact-Xhigh-Fast".into(),
                aliases: vec!["not-a-route".into()],
                max_mode: Some(true),
                ..Default::default()
            }],
        }
        .encode_to_vec();
        body.extend_from_slice(&[0xf8, 0x07, 0x01]); // forward-compatible unknown field 127
        let models = decode_get_usable_models_response(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "Some-Exact-Xhigh-Fast");
        assert_eq!(models[0].route.requested_model_id, models[0].id);
        assert!(models[0].route.parameters.is_empty());
        assert_eq!(models[0].route.max_mode, Some(true));
        assert!(encode_get_usable_models_request().is_empty());
    }
    fn authority(ids: &[&str]) -> Vec<CursorModel> {
        decode_get_usable_models_response(
            &wire::GetUsableModelsResponse {
                models: ids
                    .iter()
                    .map(|id| wire::ModelDetails {
                        model_id: (*id).into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .encode_to_vec(),
        )
        .unwrap()
    }

    fn definition(id: &str, values: &[&str], cost: bool) -> discovery::ModelParameterDefinition {
        discovery::ModelParameterDefinition {
            id: id.into(),
            parameter_type: Some(discovery::ModelParameterType {
                boolean_parameter: None,
                enum_parameter: Some(discovery::EnumParameterDefinition {
                    values: values
                        .iter()
                        .map(|value| discovery::EnumParameterValue {
                            value: (*value).into(),
                            increases_model_cost: Some(cost),
                            blocked_by_admin_allowlist: None,
                        })
                        .collect(),
                }),
            }),
        }
    }

    fn variant(id: &str, parameters: &[(&str, &str)]) -> discovery::ModelVariantConfig {
        discovery::ModelVariantConfig {
            parameter_values: parameters
                .iter()
                .map(|(id, value)| wire::ModelParameter {
                    id: (*id).into(),
                    value: (*value).into(),
                })
                .collect(),
            display_name: None,
            is_max_mode: Some(false),
            legacy_slug: Some(id.into()),
        }
    }

    fn rich(base: &str, variants: Vec<discovery::ModelVariantConfig>) -> discovery::ModelDetails {
        discovery::ModelDetails {
            name: base.into(),
            parameter_definitions: vec![
                definition("reasoning", &["none", "low", "high", "xhigh", "max"], false),
                definition("fast", &["false", "true"], true),
                definition("context", &["200k", "1m"], true),
            ],
            variants,
            ..Default::default()
        }
    }

    fn enrich(
        models: &mut [CursorModel],
        details: Vec<discovery::ModelDetails>,
    ) -> Result<(), CursorError> {
        decode_enrich_models(
            &discovery::AvailableModelsResponse {
                models: details,
                use_model_parameters: true,
            }
            .encode_to_vec(),
            models,
        )
    }

    fn effort(effort: ReasoningEffort) -> ReasoningConfig {
        ReasoningConfig {
            enabled: true,
            effort: Some(effort),
            ..Default::default()
        }
    }

    fn assert_unsupported(result: Result<CursorModelRoute, CursorError>) {
        assert!(
            matches!(result, Err(CursorError::Unsupported(_))),
            "{result:?}"
        );
    }

    #[test]
    fn empty_authority_is_not_expanded_by_rich_or_default_models() {
        let mut models = decode_get_usable_models_response(&[]).unwrap();
        enrich(
            &mut models,
            vec![rich("base", vec![variant("rich-only", &[])])],
        )
        .unwrap();
        assert!(models.is_empty());
        assert_eq!(
            decode_get_default_model_for_cli_response(&[]).unwrap(),
            None
        );
        assert_unsupported(resolve_route(
            &models,
            "rich-only",
            &ReasoningConfig::default(),
            false,
        ));
    }

    #[test]
    fn unary_golden_bytes_and_default_are_metadata_only() {
        assert_eq!(
            encode_available_models_request(),
            vec![0x10, 1, 0x28, 1, 0x38, 1, 0x60, 1]
        );
        assert!(encode_get_default_model_for_cli_request().is_empty());
        // Agent response model=1, ModelDetails.model_id=1 (no Connect envelope).
        assert_eq!(
            decode_get_default_model_for_cli_response(b"\x0a\x03\x0a\x01x").unwrap(),
            Some("x".into())
        );
        assert!(decode_get_default_model_for_cli_response(b"\x0a\x00").is_err());
        // Available response models=2; rich name=1, variant=30, legacy_slug=11.
        let body = b"\x12\x09\x0a\x01b\xf2\x01\x03\x5a\x01x";
        let mut models = authority(&["x", "auto"]);
        decode_enrich_models(body, &mut models).unwrap();
        assert_eq!(models[0].route.requested_model_id, "b");
        assert_eq!(models[0].route.legacy_model_id, "x");
        assert_eq!(
            resolve_route(&models, "auto", &ReasoningConfig::default(), false).unwrap(),
            CursorModelRoute::exact("auto")
        );
    }

    #[test]
    fn enrichment_keeps_exact_raw_ids_and_binds_only_explicit_legacy_slug() {
        let mut models = authority(&["Model-HIGH-fast", "model", " raw ID "]);
        enrich(
            &mut models,
            vec![rich(
                "base",
                vec![
                    variant(
                        "Model-HIGH-fast",
                        &[("reasoning", "high"), ("fast", "true")],
                    ),
                    variant("model-not-entitled", &[("reasoning", "low")]),
                    variant("raw ID", &[]),
                ],
            )],
        )
        .unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["Model-HIGH-fast", "model", " raw ID "]
        );
        assert_eq!(models[0].route.legacy_model_id, "Model-HIGH-fast");
        assert_eq!(models[0].route.requested_model_id, "base");
        assert_eq!(models[0].route.parameters[0].id, "reasoning");
        assert_eq!(models[0].route.parameters[0].value, "high");
        assert_eq!(models[1].route, CursorModelRoute::exact("model"));
        assert_eq!(models[2].route, CursorModelRoute::exact(" raw ID "));
        assert_unsupported(resolve_route(
            &models,
            "model-high-fast",
            &ReasoningConfig::default(),
            false,
        ));
    }

    #[test]
    fn malformed_and_ambiguous_enrichment_are_atomic() {
        let mut models = authority(&["one", "two"]);
        let original = models.clone();
        assert!(decode_enrich_models(&[0x12, 0xff], &mut models).is_err());
        assert_eq!(models, original);
        let duplicate = rich(
            "base",
            vec![
                variant("one", &[]),
                variant("two", &[]),
                variant("two", &[]),
            ],
        );
        assert!(enrich(&mut models, vec![duplicate]).is_err());
        assert_eq!(models, original);
        let invalid = rich(
            "base",
            vec![
                variant("one", &[]),
                variant("two", &[("reasoning", "invented")]),
            ],
        );
        assert!(enrich(&mut models, vec![invalid]).is_err());
        assert_eq!(models, original);
        models[1].route.max_mode = Some(true);
        let original = models.clone();
        assert!(
            enrich(
                &mut models,
                vec![rich("base", vec![variant("one", &[]), variant("two", &[])])]
            )
            .is_err()
        );
        assert_eq!(models, original);
    }

    #[test]
    fn failed_enrichment_keeps_raw_route_but_never_accepts_unmapped_effort() {
        let mut models = authority(&["familiar-model-high"]);
        assert!(decode_enrich_models(&[0x12, 0xff], &mut models).is_err());
        assert_eq!(
            resolve_route(
                &models,
                "familiar-model-high",
                &ReasoningConfig::default(),
                false
            )
            .unwrap(),
            CursorModelRoute::exact("familiar-model-high")
        );
        assert_unsupported(resolve_route(
            &models,
            "familiar-model-high",
            &effort(ReasoningEffort::High),
            false,
        ));
        assert_unsupported(resolve_route(
            &models,
            "familiar-model-high",
            &ReasoningConfig::default(),
            true,
        ));
    }

    fn lane_fixture() -> Vec<CursorModel> {
        let mut models = authority(&[
            "fast-low",
            "fast-high",
            "slow-max",
            "long-max",
            "max-mode",
            "another-max",
        ]);
        let mut max_variant = variant(
            "max-mode",
            &[("reasoning", "max"), ("fast", "true"), ("context", "200k")],
        );
        max_variant.is_max_mode = Some(true);
        enrich(
            &mut models,
            vec![
                rich(
                    "exact-base",
                    vec![
                        variant(
                            "fast-low",
                            &[("reasoning", "low"), ("fast", "true"), ("context", "200k")],
                        ),
                        // Parameter ordering does not create a different lane.
                        variant(
                            "fast-high",
                            &[("context", "200k"), ("fast", "true"), ("reasoning", "high")],
                        ),
                        variant(
                            "slow-max",
                            &[("reasoning", "max"), ("fast", "false"), ("context", "200k")],
                        ),
                        variant(
                            "long-max",
                            &[("reasoning", "max"), ("fast", "true"), ("context", "1m")],
                        ),
                        max_variant,
                        // Optional discovery cannot confer authority to use this tuple.
                        variant(
                            "unlisted-fast-max",
                            &[("reasoning", "max"), ("fast", "true"), ("context", "200k")],
                        ),
                    ],
                ),
                rich(
                    "another-base",
                    vec![variant(
                        "another-max",
                        &[("reasoning", "max"), ("fast", "true"), ("context", "200k")],
                    )],
                ),
            ],
        )
        .unwrap();
        models
    }

    #[test]
    fn explicit_effort_changes_only_same_base_same_exact_cost_and_context_lane() {
        let models = lane_fixture();
        let route =
            resolve_route(&models, "fast-high", &effort(ReasoningEffort::Low), false).unwrap();
        assert_eq!(route.legacy_model_id, "fast-low");
        assert_eq!(route.requested_model_id, "exact-base");
        assert_eq!(
            route
                .parameters
                .iter()
                .find(|p| p.id == "fast")
                .unwrap()
                .value,
            "true"
        );
        assert_eq!(
            route
                .parameters
                .iter()
                .find(|p| p.id == "context")
                .unwrap()
                .value,
            "200k"
        );
        assert_unsupported(resolve_route(
            &models,
            "fast-low",
            &effort(ReasoningEffort::Max),
            false,
        ));
        assert_unsupported(resolve_route(
            &models,
            "fast-low",
            &effort(ReasoningEffort::None),
            false,
        ));
    }

    #[test]
    fn force_max_selects_highest_entitled_effort_without_cost_or_context_escalation() {
        let models = lane_fixture();
        let route = resolve_route(&models, "fast-low", &ReasoningConfig::default(), true).unwrap();
        assert_eq!(route.legacy_model_id, "fast-high");
        assert_eq!(route.max_mode, Some(false));
        assert_eq!(
            resolve_route(&models, "fast-high", &effort(ReasoningEffort::Low), true).unwrap(),
            route
        );
        assert_eq!(
            resolve_route(&models, "slow-max", &ReasoningConfig::default(), true)
                .unwrap()
                .legacy_model_id,
            "slow-max"
        );
    }

    #[test]
    fn unknown_reasoning_definitions_are_not_guessed_from_model_names() {
        let mut models = authority(&["high", "low"]);
        let mut details = rich(
            "famous-family",
            vec![
                variant("high", &[("reasoning", "high")]),
                variant("low", &[("reasoning", "low")]),
            ],
        );
        details.parameter_definitions.clear();
        enrich(&mut models, vec![details]).unwrap();
        assert_unsupported(resolve_route(
            &models,
            "high",
            &effort(ReasoningEffort::High),
            false,
        ));
        assert_unsupported(resolve_route(
            &models,
            "high",
            &ReasoningConfig::default(),
            true,
        ));
        assert_eq!(
            resolve_route(&models, "high", &ReasoningConfig::default(), false).unwrap(),
            models[0].route
        );
    }

    #[test]
    fn boolean_thinking_never_claims_an_unmapped_qualitative_effort() {
        let mut models = authority(&["off", "on"]);
        let mut details = rich(
            "base",
            vec![
                variant("off", &[("thinking", "false")]),
                variant("on", &[("thinking", "true")]),
            ],
        );
        details.parameter_definitions = vec![discovery::ModelParameterDefinition {
            id: "thinking".into(),
            parameter_type: Some(discovery::ModelParameterType {
                boolean_parameter: Some(discovery::BooleanParameterDefinition {
                    values: ["false", "true"]
                        .into_iter()
                        .map(|v| discovery::BooleanParameterValue {
                            value: v.into(),
                            increases_model_cost: None,
                            blocked_by_admin_allowlist: None,
                        })
                        .collect(),
                }),
                enum_parameter: None,
            }),
        }];
        enrich(&mut models, vec![details]).unwrap();
        assert_eq!(
            resolve_route(
                &models,
                "off",
                &ReasoningConfig {
                    enabled: true,
                    ..Default::default()
                },
                false
            )
            .unwrap()
            .legacy_model_id,
            "on"
        );
        assert_eq!(
            resolve_route(&models, "off", &ReasoningConfig::default(), true)
                .unwrap()
                .legacy_model_id,
            "on"
        );
        assert_eq!(
            resolve_route(&models, "on", &effort(ReasoningEffort::None), false)
                .unwrap()
                .legacy_model_id,
            "off"
        );
        assert_unsupported(resolve_route(
            &models,
            "on",
            &effort(ReasoningEffort::High),
            false,
        ));
    }

    #[test]
    fn budget_translates_to_gateway_tier_and_resolves_only_declared_lanes() {
        let models = lane_fixture();
        // The fast lane declares low/high only; budget tiers beyond high fail
        // closed instead of degrading to an undeclared route.
        for config in [
            ReasoningConfig {
                budget_tokens: Some(16384),
                ..Default::default()
            },
            effort(ReasoningEffort::Budget(16384)),
        ] {
            assert_unsupported(resolve_route(&models, "fast-high", &config, false));
        }
        // force_max overrides the translated tier with the lane's highest.
        assert_eq!(
            resolve_route(
                &models,
                "fast-high",
                &ReasoningConfig {
                    budget_tokens: Some(16384),
                    ..Default::default()
                },
                true,
            )
            .unwrap()
            .legacy_model_id,
            "fast-high"
        );
        // Zero/negative-equivalent budgets request no thinking.
        assert_eq!(
            resolve_route(
                &models,
                "fast-high",
                &ReasoningConfig {
                    budget_tokens: Some(0),
                    ..Default::default()
                },
                false,
            )
            .unwrap(),
            models
                .iter()
                .find(|model| model.id == "fast-high")
                .unwrap()
                .route
        );

        // A full five-tier lane resolves each budget threshold, and the
        // budget-derived tier overrides a simultaneously supplied effort.
        let mut full = authority(&["m-low", "m-med", "m-high", "m-xhigh", "m-max"]);
        let mut rich_tiers = rich(
            "m-base",
            vec![
                variant("m-low", &[("reasoning", "low")]),
                variant("m-med", &[("reasoning", "medium")]),
                variant("m-high", &[("reasoning", "high")]),
                variant("m-xhigh", &[("reasoning", "xhigh")]),
                variant("m-max", &[("reasoning", "max")]),
            ],
        );
        rich_tiers.parameter_definitions = vec![definition(
            "reasoning",
            &["none", "low", "medium", "high", "xhigh", "max"],
            false,
        )];
        enrich(&mut full, vec![rich_tiers]).unwrap();
        for (budget, expected) in [
            (1u32, "m-med"),
            (1024, "m-med"),
            (1025, "m-high"),
            (8193, "m-xhigh"),
            (16384, "m-max"),
        ] {
            assert_eq!(
                resolve_route(
                    &full,
                    "m-low",
                    &ReasoningConfig {
                        budget_tokens: Some(budget),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap()
                .legacy_model_id,
                expected,
                "budget {budget}"
            );
        }
        assert_eq!(
            resolve_route(
                &full,
                "m-max",
                &ReasoningConfig {
                    enabled: true,
                    budget_tokens: Some(16384),
                    effort: Some(ReasoningEffort::Low),
                    ..Default::default()
                },
                false,
            )
            .unwrap()
            .legacy_model_id,
            "m-max"
        );
        // effort=budget shares the same translation as budget_tokens.
        assert_eq!(
            resolve_route(
                &full,
                "m-med",
                &effort(ReasoningEffort::Budget(1025)),
                false
            )
            .unwrap()
            .legacy_model_id,
            "m-high"
        );
    }

    #[test]
    fn display_requests_fail_instead_of_fallback() {
        let models = lane_fixture();
        let config = ReasoningConfig {
            display: Some("summary".into()),
            ..Default::default()
        };
        assert_unsupported(resolve_route(&models, "fast-high", &config, false));
        assert_unsupported(resolve_route(&models, "fast-high", &config, true));
        let mut models = authority(&["off", "low", "high"]);
        enrich(
            &mut models,
            vec![rich(
                "base",
                vec![
                    variant("off", &[("reasoning", "none")]),
                    variant("low", &[("reasoning", "low")]),
                    variant("high", &[("reasoning", "high")]),
                ],
            )],
        )
        .unwrap();
        assert_unsupported(resolve_route(
            &models,
            "off",
            &ReasoningConfig {
                enabled: true,
                ..Default::default()
            },
            false,
        ));
        assert_eq!(
            resolve_route(
                &models,
                "low",
                &ReasoningConfig {
                    enabled: true,
                    ..Default::default()
                },
                false
            )
            .unwrap()
            .legacy_model_id,
            "low"
        );
    }

    #[test]
    fn server_declared_cost_bearing_reasoning_axis_is_not_crossed() {
        let mut models = authority(&["low", "max"]);
        let mut details = rich(
            "base",
            vec![
                variant("low", &[("reasoning", "low")]),
                variant("max", &[("reasoning", "max")]),
            ],
        );
        details.parameter_definitions = vec![definition("reasoning", &["low", "max"], true)];
        enrich(&mut models, vec![details]).unwrap();
        assert_unsupported(resolve_route(
            &models,
            "low",
            &effort(ReasoningEffort::Max),
            false,
        ));
        assert_eq!(
            resolve_route(&models, "low", &ReasoningConfig::default(), true)
                .unwrap()
                .legacy_model_id,
            "low"
        );
    }

    #[test]
    fn force_max_does_not_claim_highest_when_server_has_unknown_effort() {
        let mut models = authority(&["low", "high", "ultra"]);
        let mut details = rich(
            "base",
            vec![
                variant("low", &[("reasoning", "low")]),
                variant("high", &[("reasoning", "high")]),
                variant("ultra", &[("reasoning", "ultra")]),
            ],
        );
        details.parameter_definitions =
            vec![definition("reasoning", &["low", "high", "ultra"], false)];
        enrich(&mut models, vec![details]).unwrap();
        assert_unsupported(resolve_route(
            &models,
            "low",
            &ReasoningConfig::default(),
            true,
        ));
        assert_eq!(
            resolve_route(&models, "low", &effort(ReasoningEffort::High), false)
                .unwrap()
                .legacy_model_id,
            "high"
        );
    }

    #[test]
    fn malformed_and_empty_id_fail() {
        assert!(decode_get_usable_models_response(&[0x0a, 0x08, 0x01]).is_err());
        assert!(decode_get_usable_models_response(&[0x0a, 0x00]).is_err());
    }
}
