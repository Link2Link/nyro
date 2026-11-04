//! Diagnostics for failures before an outbound request can reach transport.
//!
//! Allocate a lifecycle only on failure: successful builds retain the existing
//! single HTTP-attempt log, and local errors do not consume its retry budget.

use super::{AttemptCount, CallCtx, LogBuilder, RequestContext, RequestExtras};
use crate::error::GatewayError;
use axum::response::Response;

pub(super) fn failed_response(
    mut call_ctx: CallCtx<'_>,
    req_extras: &RequestExtras,
    ctx: &RequestContext,
    error: &GatewayError,
    builder: &'static str,
    conversion_strategy: &str,
    conversion_rule: &str,
) -> Response {
    let performance = crate::performance::Attempt::new(
        call_ctx.gw.log_tx.clone(),
        ctx.cancellation.clone(),
        ctx.deadline.clone(),
    );
    let attempt_index = ctx.extensions.get::<AttemptCount>().unwrap_or_default().0 + 1;
    ctx.extensions.insert(AttemptCount(attempt_index));
    performance.correlate(&ctx.request_id, attempt_index);
    ctx.extensions.insert(performance.clone());
    let (outcome, kind, cause) = classification(error);
    performance.record_message(outcome, kind, "request_build", cause);
    performance.with_diagnostic(|diagnostic| {
        diagnostic.payload_metadata["request_build"] = serde_json::json!({
            "gateway_error_code": error.stable_code(),
            "builder": builder,
            "conversion_strategy": conversion_strategy,
            "conversion_rule": conversion_rule,
            "upstream_contacted": false,
        });
        if let GatewayError::ProtocolLossyRejected { lost } = error {
            diagnostic.payload_metadata["request_build"]["lost_field_count"] =
                serde_json::json!(lost.len());
        }
    });
    // Keep this attempt response-owned, not a preflight/final-request result:
    // only dispatch_pipeline selects the response actually returned to the caller.
    let response = performance.wrap(error.render_with_ctx(ctx));
    call_ctx.performance = Some(performance);
    // Deliberately no upstream URL/status/headers/body/usage: a planned or
    // partially encoded request is not evidence of an actual upstream call.
    LogBuilder::from_ctx(&call_ctx)
        .with_req_extras(req_extras)
        .status(error.http_status().as_u16())
        .emit();
    response
}

/// GatewayError may contain arbitrary vendor messages, request field names,
/// upstream bodies or credential-bearing source chains. Only variant-owned
/// facts belong in scalar diagnostics. The real rendered response is captured
/// by the existing delivery observer under the normal payload retention gate.
fn classification(error: &GatewayError) -> (&'static str, &'static str, &'static str) {
    match error {
        GatewayError::ProtocolLossyRejected { .. } => (
            "failed",
            "protocol_lossy_rejected",
            "Request encoding rejected a lossy protocol conversion before upstream dispatch.",
        ),
        GatewayError::ProtocolUnsupported { .. } => (
            "failed",
            "protocol_unsupported",
            "The selected protocol cannot encode this request before upstream dispatch.",
        ),
        GatewayError::BadRequest { .. } => (
            "failed",
            "request_invalid",
            "Request construction rejected invalid input before upstream dispatch.",
        ),
        GatewayError::Unauthorized { .. } | GatewayError::Forbidden { .. } => (
            "failed",
            "request_auth_rejected",
            "Request construction rejected authentication or access before upstream dispatch.",
        ),
        GatewayError::QuotaExceeded { .. } => (
            "failed",
            "request_quota_rejected",
            "Request construction rejected a local quota before upstream dispatch.",
        ),
        GatewayError::ModelNotFound { .. } => (
            "failed",
            "request_model_not_found",
            "Request construction could not resolve the model before upstream dispatch.",
        ),
        GatewayError::ProviderUnavailable { .. } => (
            "failed",
            "request_provider_unavailable",
            "Request construction found an unavailable provider before upstream dispatch.",
        ),
        GatewayError::UpstreamTimeout { .. } => (
            "timed_out",
            "timeout",
            "Request construction timed out before upstream dispatch.",
        ),
        // The existing lifecycle classifies every delivered HTTP 4xx/5xx as
        // failed, including the synthetic 499. Record that outcome up front so
        // wrap/log cannot replace the more specific local kind and stage.
        GatewayError::ClientCancelled => (
            "failed",
            "client_cancelled",
            "The client cancelled during request construction before upstream dispatch.",
        ),
        GatewayError::Internal { .. }
        | GatewayError::UpstreamStatus { .. }
        | GatewayError::AccountUpstream { .. }
        | GatewayError::StreamParseError { .. } => (
            "failed",
            "request_build",
            "Local request construction failed before upstream dispatch.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{
        AccessDenial, AccountFailure, AccountFailureReason, AuthFailure, QuotaWindow, TimeoutPhase,
    };
    use crate::proxy::context::{CancellationToken, Deadline};

    #[test]
    fn typed_errors_keep_local_safe_diagnostics_after_response_wrapping() {
        const SECRET: &str = "opaque-credential-without-sanitizer-keywords";
        let errors = [
            GatewayError::bad_request("untrusted-code", SECRET),
            GatewayError::Unauthorized {
                reason: AuthFailure::Invalid,
            },
            GatewayError::Forbidden {
                reason: AccessDenial::Custom(SECRET.into()),
            },
            GatewayError::QuotaExceeded {
                window: QuotaWindow {
                    window_type: SECRET.into(),
                    reset_at_secs: None,
                },
            },
            GatewayError::ModelNotFound {
                model: SECRET.into(),
            },
            GatewayError::ProtocolUnsupported {
                ingress: SECRET.into(),
                egress: SECRET.into(),
            },
            GatewayError::ProtocolLossyRejected {
                lost: vec![SECRET.into()],
            },
            GatewayError::upstream_status(SECRET, 503, Some(SECRET.into())),
            GatewayError::AccountUpstream {
                provider: "synthetic",
                status: 400,
                message: Some(SECRET.into()),
                failure: AccountFailure {
                    reason: AccountFailureReason::Other,
                    rejected_before_execution: false,
                    retry_after_seconds: None,
                },
            },
            GatewayError::UpstreamTimeout {
                phase: TimeoutPhase::Total,
            },
            GatewayError::StreamParseError {
                provider: SECRET.into(),
                raw_chunk: Some(SECRET.into()),
            },
            GatewayError::ClientCancelled,
            GatewayError::provider_unavailable(SECRET, SECRET),
            GatewayError::internal(anyhow::anyhow!(SECRET).context(SECRET)),
        ];
        for error in errors {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let attempt =
                crate::performance::Attempt::new(tx, CancellationToken::new(), Deadline::never());
            let (outcome, kind, cause) = classification(&error);
            attempt.record_message(outcome, kind, "request_build", cause);
            let response = attempt.wrap(error.render(Some("local-request")));
            let diagnostic = attempt.diagnostic();
            assert_eq!(
                diagnostic.attempt_outcome,
                outcome,
                "{}",
                error.stable_code()
            );
            assert_eq!(diagnostic.failure_kind.as_deref(), Some(kind));
            assert_eq!(diagnostic.failure_stage.as_deref(), Some("request_build"));
            assert!(!serde_json::to_string(&diagnostic).unwrap().contains(SECRET));
            if matches!(error, GatewayError::ClientCancelled) {
                assert_eq!(response.status().as_u16(), 499);
                assert_eq!(outcome, "failed");
                assert_eq!(kind, "client_cancelled");
            }
        }
    }
}
