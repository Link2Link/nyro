use axum::response::Response;

use crate::integrations::{HookContext, HookRegistry};
use crate::plugin::phase::{HostContext, Phase, PhaseOutcome, ResponseView};
use crate::protocol::ir::{AiRequest, AiResponse, Usage};
use crate::proxy::context::RequestContext;

use super::{CallCtx, run_phase_hooks};

pub(super) enum BufferedFinalize {
    Continue {
        response: Box<AiResponse>,
        usage: Usage,
        mutated: bool,
    },
    Override(Response),
}

pub(super) async fn finalize_buffered_response(
    call_ctx: &CallCtx<'_>,
    req_ctx: &mut RequestContext,
    req_ir: &mut AiRequest,
    host: &HostContext<'_>,
    mut response: AiResponse,
    ensure_model: Option<&str>,
) -> BufferedFinalize {
    if response.model.is_empty()
        && let Some(model) = ensure_model
    {
        response.model = model.to_string();
    }

    let before = serde_json::to_value(&response).ok();
    let hook_registry = HookRegistry::global();
    if hook_registry.has_response_hooks() {
        let hook_ctx = HookContext {
            model_id: call_ctx.model_id.to_string(),
            provider_name: call_ctx.provider.name.clone(),
            model: response.model.clone(),
            api_key_id: call_ctx.api_key_id.map(str::to_string),
        };
        let latency_ms = call_ctx.start.elapsed().as_millis() as u64;
        for hook in hook_registry.response_hooks() {
            hook.on_response(&hook_ctx, &mut response, latency_ms).await;
        }
    }

    match run_phase_hooks(
        Phase::OnResponse,
        req_ctx,
        req_ir,
        ResponseView::Full(&mut response),
        host,
    )
    .await
    {
        PhaseOutcome::Continue => {}
        PhaseOutcome::ShortCircuit(response) => {
            return BufferedFinalize::Override(response);
        }
        PhaseOutcome::Reject(error) => {
            return BufferedFinalize::Override(error.render(None));
        }
    }

    let usage = response.usage.clone();
    let mutated = before != serde_json::to_value(&response).ok();
    BufferedFinalize::Continue {
        response: Box::new(response),
        usage,
        mutated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_result_exposes_post_hook_usage_shape() {
        let response = AiResponse::new("id", "model");
        let result = BufferedFinalize::Continue {
            usage: response.usage.clone(),
            response: Box::new(response),
            mutated: false,
        };
        let BufferedFinalize::Continue { usage, mutated, .. } = result else {
            panic!("expected continue");
        };
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
        assert!(!mutated);
    }
}
