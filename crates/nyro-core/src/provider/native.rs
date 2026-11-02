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

/// Admin model probes must classify native errors before formatting them. Never
/// copy arbitrary upstream bodies, credential echoes or error chains into results.
pub(crate) fn model_probe_error(error: &anyhow::Error) -> String {
    if let Some(failure) = error.downcast_ref::<ModelProbeFailure>() {
        return failure.to_string();
    }
    let Some(error) = error.downcast_ref::<GatewayError>() else {
        return "Model probe failed; check model access, account status and network settings"
            .into();
    };
    gateway_probe_error(error)
}

fn gateway_probe_error(error: &GatewayError) -> String {
    use crate::error::AccountFailureReason as Reason;

    if let GatewayError::UpstreamTimeout { phase } = error {
        return format!("Model probe timed out during {}", phase.as_str());
    }
    let message = match error {
        GatewayError::AccountUpstream { failure, .. } => match failure.reason {
            Reason::AuthInvalid => "Account authentication was rejected; reauthorize the account",
            Reason::ModelForbidden => {
                "Model access was denied; verify the account's profile and model entitlement"
            }
            Reason::DailyQuota | Reason::WeeklyQuota | Reason::MonthlyQuota => {
                "Account quota is exhausted; check its plan and usage"
            }
            Reason::RateLimited => "Account is rate limited; retry later",
            Reason::Capacity => "Upstream model capacity is unavailable; retry later",
            Reason::RequestInvalid => "Upstream rejected the model probe request",
            Reason::Other => "Upstream rejected the model probe",
        },
        GatewayError::UpstreamStatus { status, .. } => match status {
            401 => "Account authentication was rejected; reauthorize the account",
            403 => "Model access was denied; verify the account's profile and model entitlement",
            402 | 429 => "Account is rate limited or lacks quota; check its plan and usage",
            400 | 422 => {
                "Upstream rejected the model probe request; check model and client compatibility"
            }
            404 => "Requested model or upstream endpoint was not found",
            _ => "Upstream call failed; check service availability and network settings",
        },
        GatewayError::BadRequest { .. } | GatewayError::ProtocolLossyRejected { .. } => {
            "Model probe request is incompatible with this provider"
        }
        GatewayError::StreamParseError { .. } => {
            "Upstream returned an invalid or incomplete model response"
        }
        GatewayError::ProviderUnavailable { .. } => {
            "Model probe could not start; check provider configuration and account credentials"
        }
        GatewayError::ClientCancelled => "Model probe was cancelled before completion",
        _ => "Model probe failed; check model access, account status and network settings",
    };
    // This can be a locally mapped status (e.g. HTTP 402 -> 429), not a wire fact.
    format!(
        "{message} (gateway status {})",
        error.http_status().as_u16()
    )
}

/// Safe probe-only failure with a journal correlation ID. This is deliberately
/// not a request-log entry: model checks must not pollute inference statistics.
pub(crate) struct ModelProbeFailure {
    source: GatewayError,
    probe_id: String,
    stage: &'static str,
    http_status: Option<u16>,
}

impl ModelProbeFailure {
    pub fn new(
        source: GatewayError,
        probe_id: String,
        stage: &'static str,
        http_status: Option<u16>,
    ) -> Self {
        Self {
            source,
            probe_id,
            stage,
            http_status,
        }
    }
}

impl std::fmt::Display for ModelProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; probe_id={}, stage={}",
            gateway_probe_error(&self.source),
            self.probe_id,
            self.stage
        )?;
        if let Some(status) = self.http_status {
            write!(f, ", upstream HTTP {status}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ModelProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

// Do not expose the original cause through anyhow's alternate/debug formatter:
// provider payloads and arbitrary transport chains are not public probe output.
impl std::error::Error for ModelProbeFailure {}

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
