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

/// Reject failed decoder batches before any call in them reaches a client or
/// custom-tool buffer. A decoder can return Ok(StreamError) rather than Err;
/// formatters do not necessarily understand that IR event. Treat the whole batch
/// atomically so a valid call preceding an invalid part is not executed as success.
pub(super) fn validate_decoded_batch(
    decoded: anyhow::Result<Vec<AiStreamDelta>>,
) -> anyhow::Result<Vec<AiStreamDelta>> {
    let deltas = decoded?;
    for delta in &deltas {
        match delta {
            AiStreamDelta::StreamError { error } => return Err(error.clone().into()),
            AiStreamDelta::UnexpectedEof => {
                return Err(crate::protocol::ir::error::AiError::new(
                    crate::protocol::ir::error::AiErrorKind::UnexpectedEof,
                    "Upstream stream ended before a complete response",
                )
                .into());
            }
            _ => {}
        }
    }
    Ok(deltas)
}

/// Native sources emit `Done` only after wire verification, but the worker may
/// still report a trailing transport failure. Hold the terminal out of the
/// formatter/tool bridge until source EOF; otherwise those consumers can emit a
/// successful terminal or flush partial custom input before the failure arrives.
/// This is conversion state, NOT performance evidence (only the wire worker may
/// establish that evidence).
#[derive(Default)]
pub(super) struct IrTerminalState {
    done: Option<AiStreamDelta>,
    failed: bool,
}

impl IrTerminalState {
    pub(super) fn has_done(&self) -> bool {
        self.done.is_some()
    }

    pub(super) fn accept(
        &mut self,
        deltas: Vec<AiStreamDelta>,
    ) -> anyhow::Result<Vec<AiStreamDelta>> {
        if self.failed {
            anyhow::bail!("Native IR stream has already failed");
        }
        let result = (|| {
            // Validate the entire batch before admitting any payload or terminal.
            let deltas = validate_decoded_batch(Ok(deltas))?;
            let mut seen_done = self.has_done();
            for delta in &deltas {
                match delta {
                    AiStreamDelta::Done { stop_reason } => {
                        if seen_done || stop_reason.eq_ignore_ascii_case("error") {
                            anyhow::bail!("Invalid native IR terminal");
                        }
                        seen_done = true;
                    }
                    AiStreamDelta::Usage(_) => {}
                    _ if seen_done => anyhow::bail!("Native IR payload after terminal"),
                    _ => {}
                }
            }
            let mut output = Vec::with_capacity(deltas.len());
            for delta in deltas {
                if matches!(delta, AiStreamDelta::Done { .. }) {
                    self.done = Some(delta);
                } else {
                    output.push(delta);
                }
            }
            Ok(output)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(super) fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        if self.failed {
            anyhow::bail!("Native IR stream has already failed");
        }
        match self.done.take() {
            Some(done) => Ok(vec![done]),
            None => {
                self.failed = true;
                anyhow::bail!("Native IR stream ended without a verified Done");
            }
        }
    }
}

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

    #[test]
    fn native_terminal_requires_done_and_rejects_error_atomically() {
        let mut terminal = IrTerminalState::default();
        assert!(
            terminal
                .accept(vec![AiStreamDelta::TextDelta("hi".into())])
                .is_ok()
        );
        assert!(
            terminal.finish().is_err(),
            "EOF alone must not become success"
        );
        assert!(
            terminal
                .accept(vec![AiStreamDelta::Done {
                    stop_reason: "stop".into()
                }])
                .is_err(),
            "failure is sticky"
        );

        let mut terminal = IrTerminalState::default();
        assert!(
            terminal
                .accept(vec![
                    AiStreamDelta::TextDelta("do not expose".into()),
                    AiStreamDelta::Done {
                        stop_reason: "stop".into()
                    },
                    AiStreamDelta::UnexpectedEof,
                ])
                .is_err()
        );
        assert!(!terminal.has_done());
        assert!(terminal.finish().is_err());
    }

    #[test]
    fn native_done_is_held_until_eof_and_trailing_usage_survives() {
        let mut terminal = IrTerminalState::default();
        let deltas = terminal
            .accept(vec![
                AiStreamDelta::TextDelta("hi".into()),
                AiStreamDelta::Done {
                    stop_reason: "tool_calls".into(),
                },
            ])
            .unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(terminal.has_done());
        assert_eq!(
            terminal
                .accept(vec![AiStreamDelta::Usage(Default::default())])
                .unwrap()
                .len(),
            1
        );
        assert!(
            matches!(terminal.finish().unwrap().as_slice(), [AiStreamDelta::Done { stop_reason }] if stop_reason == "tool_calls")
        );
        assert!(
            terminal.finish().is_err(),
            "terminal can only be consumed once"
        );
    }

    #[test]
    fn native_error_terminal_and_post_terminal_payload_are_not_success() {
        for deltas in [
            vec![AiStreamDelta::Done {
                stop_reason: "ERROR".into(),
            }],
            vec![
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
                AiStreamDelta::TextDelta("late".into()),
            ],
            vec![
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
            ],
        ] {
            let mut terminal = IrTerminalState::default();
            assert!(terminal.accept(deltas).is_err());
            assert!(terminal.finish().is_err());
        }
    }

    #[test]
    fn invalid_call_discards_the_whole_decoded_batch() {
        use crate::protocol::StreamResponseDecoder;
        use crate::protocol::codec::google::gemini::stream::GoogleStreamParser;
        let mut parser = GoogleStreamParser::new();
        let raw = format!(
            "data: {}\n\n",
            serde_json::json!({"candidates":[{
                "content":{"parts":[
                    {"functionCall":{"name":"valid","args":{"x":1}}},
                    {"functionCall":{"name":"invalid"}}
                ]}, "finishReason":"STOP"
            }]})
        );
        let decoded = parser.parse_chunk(&raw).unwrap();
        assert!(
            decoded
                .iter()
                .any(|d| matches!(d, AiStreamDelta::ToolCallStart { .. }))
        );
        assert!(validate_decoded_batch(Ok(decoded)).is_err());
    }

    #[test]
    fn finish_time_decoder_error_cannot_become_successful_completion() {
        use crate::protocol::StreamResponseDecoder;
        use crate::protocol::codec::google::gemini::stream::GoogleStreamParser;
        let mut parser = GoogleStreamParser::new();
        assert!(
            validate_decoded_batch(parser.parse_chunk("data: {\"candidates\":["))
                .unwrap()
                .is_empty()
        );
        assert!(validate_decoded_batch(parser.finish()).is_err());
    }

    #[test]
    fn valid_deltas_survive_and_both_error_representations_are_rejected() {
        let deltas =
            validate_decoded_batch(Ok(vec![AiStreamDelta::TextDelta("hi".into())])).unwrap();
        assert!(matches!(&deltas[0], AiStreamDelta::TextDelta(text) if text == "hi"));
        assert!(validate_decoded_batch(Ok(vec![AiStreamDelta::UnexpectedEof])).is_err());
        assert!(validate_decoded_batch(Err(anyhow::anyhow!("parse failure"))).is_err());
    }

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
