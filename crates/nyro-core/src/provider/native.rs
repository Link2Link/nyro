//! Provider-owned, native duplex execution returning protocol-neutral events.
//! Standard JSON/SSE providers continue to use OutboundRequest and codecs.
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::Stream;
use reqwest::header::HeaderMap;

use crate::error::GatewayError;
use crate::protocol::ir::AiStreamDelta;
use crate::proxy::context::{CancellationToken, Deadline};

pub(crate) type IrEventStream =
    Pin<Box<dyn Stream<Item = Result<Vec<AiStreamDelta>, GatewayError>> + Send>>;

pub(crate) struct IrUpstream {
    pub status: u16,
    pub headers: HeaderMap,
    pub url: String,
    /// Exact selected model tuple, not the caller alias.
    pub actual_model: String,
    pub model_metadata: serde_json::Value,
    pub events: IrEventStream,
    /// Owns the work after HTTP headers are received; drop always stops it.
    pub guard: SessionGuard,
}

#[derive(Clone)]
pub(crate) struct AttemptControl {
    pub deadline: Deadline,
    pub cancellation: CancellationToken,
    pub performance: crate::performance::Attempt,
    /// Becomes true before the first Run bytes are handed to the transport.
    /// An ambiguous failure after this point cannot be replayed automatically.
    pub run_submitted: Arc<AtomicBool>,
    /// Local admission rejection must not penalize upstream provider health.
    pub local_failure: Arc<AtomicBool>,
    /// Explicit pre-execution HTTP rejection; unlike an ambiguous dropped Run.
    pub rejected: Arc<AtomicBool>,
}

impl AttemptControl {
    pub fn mark_submitted(&self) {
        self.run_submitted.store(true, Ordering::Release);
    }
    pub fn is_submitted(&self) -> bool {
        self.run_submitted.load(Ordering::Acquire)
    }
}

pub(crate) struct SessionGuard {
    cancellation: CancellationToken,
    tasks: Vec<tokio::task::AbortHandle>,
}
impl SessionGuard {
    pub fn new(cancellation: CancellationToken, tasks: Vec<tokio::task::AbortHandle>) -> Self {
        Self {
            cancellation,
            tasks,
        }
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.cancellation.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}
