use axum::response::Response;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDisposition {
    DefaultStatusPolicy,
    ForceRetry,
}

impl RetryDisposition {
    pub(crate) const fn should_retry(self, status_retryable: bool) -> bool {
        match self {
            Self::DefaultStatusPolicy => status_retryable,
            Self::ForceRetry => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthDisposition {
    Success,
    Failure,
    Neutral,
    Deferred,
}

pub(crate) struct ConversionAttempt {
    pub(crate) response: Response,
    pub(crate) retry: RetryDisposition,
    pub(crate) health: HealthDisposition,
}

impl ConversionAttempt {
    pub(crate) fn new(
        response: Response,
        retry: RetryDisposition,
        health: HealthDisposition,
    ) -> Self {
        Self {
            response,
            retry,
            health,
        }
    }

    pub(crate) fn default_policy(response: Response, health: HealthDisposition) -> Self {
        Self::new(response, RetryDisposition::DefaultStatusPolicy, health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_disposition_combines_strategy_and_status_policy() {
        assert!(RetryDisposition::ForceRetry.should_retry(false));
        assert!(RetryDisposition::DefaultStatusPolicy.should_retry(true));
        assert!(!RetryDisposition::DefaultStatusPolicy.should_retry(false));
    }

    #[test]
    fn default_attempt_uses_status_policy() {
        let response = axum::response::Response::builder()
            .status(502)
            .body(axum::body::Body::empty())
            .unwrap();
        let attempt = ConversionAttempt::default_policy(response, HealthDisposition::Failure);
        assert_eq!(attempt.retry, RetryDisposition::DefaultStatusPolicy);
        assert_eq!(attempt.health, HealthDisposition::Failure);
    }
}
