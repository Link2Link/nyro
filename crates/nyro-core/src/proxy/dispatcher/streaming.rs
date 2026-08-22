//! Shared streaming-response lifecycle helpers.
//!
//! Native IR streaming and Raw-Wire Compat streaming both run the per-delta
//! `OnResponse` phase inside a spawned task that outlives the request handler.
//! This module centralizes the owned hook state that crosses into those tasks
//! and the per-delta application loop, so both strategies share one identical
//! implementation. Strategy-specific stream conversion state machines remain
//! untouched: each caller still owns parsing, formatting, terminal handling,
//! and health settlement.

use std::sync::Arc;

use crate::Gateway;
use crate::plugin::phase::{HostContext, Phase, PhaseHook, PhaseHookRegistry, ResponseView};
use crate::protocol::ir::{AiRequest, AiStreamDelta};
use crate::proxy::context::RequestContext;

/// Owned `OnResponse` hook state for a spawned streaming task.
///
/// The spawned task outlives the handler borrow, so the request context, IR,
/// and gateway are cloned into owned copies — but only when at least one
/// `OnResponse` hook is registered. With no hooks registered nothing is
/// cloned and the per-delta application is a zero-overhead no-op.
pub(super) struct StreamHookState {
    hooks: Vec<&'static Arc<dyn PhaseHook>>,
    req_ctx: Option<RequestContext>,
    req_ir: Option<AiRequest>,
    gateway: Option<Gateway>,
}

impl StreamHookState {
    /// Capture the streaming hook state for one request attempt.
    pub(super) fn capture(req_ctx: &RequestContext, req_ir: &AiRequest, gateway: &Gateway) -> Self {
        let hooks = PhaseHookRegistry::global().for_phase(Phase::OnResponse);
        if hooks.is_empty() {
            return Self {
                hooks,
                req_ctx: None,
                req_ir: None,
                gateway: None,
            };
        }
        Self {
            hooks,
            req_ctx: Some(req_ctx.clone()),
            req_ir: Some(req_ir.clone()),
            gateway: Some(gateway.clone()),
        }
    }

    /// Whether any OnResponse hook is registered for this stream.
    pub(super) fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Apply the `OnResponse` phase to each streamed delta in place.
    ///
    /// Streaming `OnResponse` is mutation-only: a hook may reshape the
    /// [`AiStreamDelta`], but `ShortCircuit` / `Reject` cannot replace an
    /// already-streaming response, so non-`Continue` outcomes are ignored.
    /// No-op when no hooks are registered or the owned context was not cloned in.
    pub(super) async fn apply(&mut self, deltas: &mut [AiStreamDelta]) {
        if self.hooks.is_empty() {
            return;
        }
        // Clone the gateway handle out so the host boundary borrows a local
        // value; `Gateway` is a cheap Arc-based clone.
        let Some(gateway) = self.gateway.clone() else {
            return;
        };
        let host = HostContext::new(&gateway);
        let Self {
            hooks,
            req_ctx,
            req_ir,
            gateway: _,
        } = self;
        let (Some(req_ctx), Some(req_ir)) = (req_ctx.as_mut(), req_ir.as_mut()) else {
            return;
        };
        for delta in deltas.iter_mut() {
            let _ = super::run_phase_hooks_slice(
                hooks.as_slice(),
                req_ctx,
                req_ir,
                ResponseView::Stream(delta),
                &host,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hook_state_without_hooks_is_a_no_op() {
        // With no inventory-registered OnResponse hooks in the test binary's
        // registry beyond the sentinel ones, capture on a default context and
        // assert apply() does not panic and leaves deltas untouched.
        let req_ctx = RequestContext::new(
            crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        );
        let req_ir = AiRequest::new("model", Vec::new());
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-stream-hook-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (gateway, _log_rx) = Gateway::new(config).await.expect("gateway init");
        let mut state = StreamHookState::capture(&req_ctx, &req_ir, &gateway);
        let mut deltas = vec![AiStreamDelta::Done {
            stop_reason: "stop".to_string(),
        }];
        state.apply(&mut deltas).await;
        let AiStreamDelta::Done { stop_reason } = &deltas[0] else {
            panic!("delta shape unchanged");
        };
        assert_eq!(stop_reason, "stop");
    }
}
