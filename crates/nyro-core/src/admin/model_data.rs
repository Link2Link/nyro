use super::*;

pub(super) fn normalize_model_balance(balance: Option<&str>) -> anyhow::Result<String> {
    let normalized = balance.unwrap_or("weighted").trim().to_ascii_lowercase();
    match normalized.as_str() {
        "weighted" | "priority" | "latency" | "usage" => Ok(normalized),
        _ => anyhow::bail!("unsupported model balance: {normalized}"),
    }
}

pub(super) fn normalize_create_model_backends(
    input: &CreateModel,
) -> anyhow::Result<Vec<CreateModelBackend>> {
    if !input.targets.is_empty() {
        return Ok(input.targets.clone());
    }
    if !input.target_provider.trim().is_empty() && !input.target_model.trim().is_empty() {
        return Ok(vec![CreateModelBackend {
            provider_id: input.target_provider.clone(),
            model: input.target_model.clone(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: None,
        }]);
    }
    anyhow::bail!("at least one model backend is required")
}

pub(super) fn normalize_update_model_backends(
    current: &Model,
    input: &UpdateModel,
) -> anyhow::Result<Vec<CreateModelBackend>> {
    if let Some(targets) = &input.targets {
        let mapped = targets
            .iter()
            .map(|target| CreateModelBackend {
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                weight: target.weight,
                priority: target.priority,
                is_fallback: target.is_fallback,
            })
            .collect();
        return Ok(mapped);
    }

    let provider = input
        .target_provider
        .clone()
        .unwrap_or_else(|| current.target_provider.clone());
    let model = input
        .target_model
        .clone()
        .unwrap_or_else(|| current.target_model.clone());
    if provider.trim().is_empty() || model.trim().is_empty() {
        anyhow::bail!("model backend cannot be empty");
    }
    Ok(vec![CreateModelBackend {
        provider_id: provider,
        model,
        weight: Some(100),
        priority: Some(1),
        is_fallback: None,
    }])
}

pub(super) fn ensure_model_backends_valid(backends: &[CreateModelBackend]) -> anyhow::Result<()> {
    if backends.is_empty() {
        anyhow::bail!("at least one model backend is required");
    }
    for backend in backends {
        if backend.provider_id.trim().is_empty() {
            anyhow::bail!("backend provider_id cannot be empty");
        }
        if backend.model.trim().is_empty() {
            anyhow::bail!("backend model cannot be empty");
        }
        let weight = backend.weight.unwrap_or(100);
        if weight < 0 {
            anyhow::bail!("backend weight must be >= 0");
        }
        let priority = backend.priority.unwrap_or(1);
        if priority < 1 {
            anyhow::bail!("backend priority must be a positive integer");
        }
    }
    // Last-resort fallback rows: exactly one allowed, and never the only row.
    let fallback_count = backends
        .iter()
        .filter(|backend| backend.is_fallback.unwrap_or(false))
        .count();
    if fallback_count > 1 {
        anyhow::bail!("only one fallback backend per model is allowed");
    }
    if fallback_count > 0 && backends.len() == fallback_count {
        anyhow::bail!("at least one non-fallback backend is required");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ensure_model_backends_valid, normalize_model_balance};

    #[test]
    fn usage_balance_is_case_normalized_and_unknown_values_are_rejected() {
        assert_eq!(normalize_model_balance(Some(" Usage ")).unwrap(), "usage");
        assert!(normalize_model_balance(Some("quota-score")).is_err());
    }

    fn backend_row(provider: &str, is_fallback: Option<bool>) -> super::CreateModelBackend {
        super::CreateModelBackend {
            provider_id: provider.to_string(),
            model: format!("{provider}-model"),
            weight: Some(100),
            priority: Some(1),
            is_fallback,
        }
    }

    #[test]
    fn single_fallback_backend_is_valid() {
        let backends = vec![backend_row("a", None), backend_row("z", Some(true))];
        assert!(ensure_model_backends_valid(&backends).is_ok());
    }

    #[test]
    fn duplicate_fallback_backends_are_rejected() {
        let backends = vec![backend_row("a", Some(true)), backend_row("z", Some(true))];
        let err = ensure_model_backends_valid(&backends)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("only one fallback backend"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn fallback_only_backend_list_is_rejected() {
        let backends = vec![backend_row("z", Some(true))];
        let err = ensure_model_backends_valid(&backends)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-fallback"), "unexpected: {err}");
    }
}
