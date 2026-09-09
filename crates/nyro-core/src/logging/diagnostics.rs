//! Versioned, authoritative request outcomes. Old performance observations are not
//! proof of success/failure and must never silently widen deletion/statistics.
use serde::{Deserialize, Serialize};

pub const OUTCOME_VERSION: i32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, sqlx::FromRow)]
pub struct RequestResult {
    pub client_request_id: String,
    pub final_outcome: String,
    pub final_attempt_id: Option<String>,
    pub attempt_count: i32,
    pub finished_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogDiagnostic {
    pub log_id: String,
    pub client_request_id: Option<String>,
    pub attempt_index: Option<i32>,
    pub outcome_version: i32,
    pub attempt_outcome: String,
    pub failure_kind: Option<String>,
    pub failure_stage: Option<String>,
    pub error_message: Option<String>,
    pub error_causes: Vec<String>,
    pub payload_metadata: serde_json::Value,
    pub final_result: Option<RequestResult>,
}
impl Default for LogDiagnostic {
    fn default() -> Self {
        Self {
            log_id: uuid::Uuid::new_v4().to_string(),
            client_request_id: None,
            attempt_index: None,
            outcome_version: 0,
            attempt_outcome: "unknown".into(),
            failure_kind: None,
            failure_stage: None,
            error_message: None,
            error_causes: vec![],
            payload_metadata: serde_json::json!({}),
            final_result: None,
        }
    }
}

pub fn is_http_error(status: Option<i32>) -> bool {
    status.is_some_and(|code| (400..=599).contains(&code))
}
pub fn is_error(client: Option<i32>, upstream: Option<i32>, version: i32, outcome: &str) -> bool {
    is_http_error(client)
        || is_http_error(upstream)
        || (version == OUTCOME_VERSION && matches!(outcome, "failed" | "timed_out"))
}
pub fn effective_outcome(
    client: Option<i32>,
    upstream: Option<i32>,
    version: i32,
    outcome: &str,
) -> &'static str {
    if is_error(client, upstream, version, outcome) {
        return "error";
    }
    if version != OUTCOME_VERSION {
        return "unknown";
    }
    match outcome {
        "completed" => "completed",
        "cancelled" => "cancelled",
        "output_limited" => "output_limited",
        _ => "unknown",
    }
}
/// Force bounded payload retention even when ordinary recording is disabled.
///
/// A versioned `unknown` outcome is an actively classified ambiguity — the
/// observer looked and could not determine what happened (for example a stream
/// that ended without a confirmable terminal). The payload is then the only
/// remaining evidence, so it is retained for debugging. The un-upgraded
/// `version 0` default is deliberately excluded: legacy rows and paths without
/// lifecycle observation are all version-0 `unknown`, and blanket-retaining
/// them would defeat the payload-recording switch.
pub fn force_payload(
    client: Option<i32>,
    upstream: Option<i32>,
    diagnostic: &LogDiagnostic,
) -> bool {
    is_error(
        client,
        upstream,
        diagnostic.outcome_version,
        &diagnostic.attempt_outcome,
    ) || (diagnostic.outcome_version == OUTCOME_VERSION
        && matches!(
            diagnostic.attempt_outcome.as_str(),
            "cancelled" | "output_limited" | "unknown"
        ))
}
fn qualifier(alias: &str) -> String {
    assert!(
        alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "SQL alias must be an internal identifier"
    );
    if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    }
}
/// Portable SQL boolean expression; aliases are internal constants, never user input.
pub fn error_sql(alias: &str) -> String {
    let p = qualifier(alias);
    format!(
        "(COALESCE({p}client_status_code BETWEEN 400 AND 599, FALSE) OR COALESCE({p}upstream_status_code BETWEEN 400 AND 599, FALSE) OR COALESCE(({p}outcome_version = 1 AND {p}attempt_outcome IN ('failed', 'timed_out')), FALSE))"
    )
}
pub fn outcome_sql(alias: &str, outcome: &str) -> String {
    let p = qualifier(alias);
    let error = error_sql(alias);
    match outcome {
        "error" => error,
        "completed" | "cancelled" | "output_limited" => format!(
            "(NOT {error} AND COALESCE(({p}outcome_version = 1 AND {p}attempt_outcome = '{outcome}'), FALSE))"
        ),
        "unknown" => format!(
            "(NOT {error} AND NOT COALESCE(({p}outcome_version = 1 AND {p}attempt_outcome IN ('completed', 'cancelled', 'output_limited')), FALSE))"
        ),
        _ => panic!("outcome SQL accepts only validated canonical outcomes"),
    }
}

fn capped(mut value: String, limit: usize) -> String {
    if value.len() > limit {
        let mut end = limit.saturating_sub(3);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push_str("...");
    }
    value
}

/// Deliberately conservative cause sanitizer. URLs are redacted token-by-token;
/// ambiguous credential-bearing text is omitted rather than leaked. Body data is
/// kept separately under admin permissions, never embedded into error chains.
pub fn sanitize_cause(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if [
        "bearer ",
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "secret=",
        "token=",
    ]
    .iter()
    .any(|word| lower.contains(word))
    {
        return "Sensitive diagnostic text omitted".into();
    }
    let words: Vec<_> = message
        .split_whitespace()
        .map(|word| {
            if word.contains("http://")
                || word.contains("https://")
                || word.contains("postgres://")
                || word.contains("mysql://")
            {
                "[URL omitted]".to_string()
            } else {
                word.to_string()
            }
        })
        .collect();
    capped(words.join(" "), 512)
}
fn public_message(kind: &str) -> &'static str {
    match kind {
        "upstream_connect" => "Could not connect to the upstream provider",
        "upstream_read" => "Failed to read the upstream response",
        "upstream_decompress" => "Failed to decompress the upstream response",
        "upstream_protocol_error" => "The upstream response reported an error",
        "response_parse" => "Failed to parse the response",
        "conversion" => "Failed to convert the response protocol",
        "missing_terminal" => "The response ended without its required terminal event",
        "ambiguous_terminal" => "Could not confirm the response terminal event",
        "downstream_disconnect" => "The downstream response closed before completion",
        "output_limit" => "The response reached its output token limit",
        "timeout" => "The request timed out",
        _ => "The request could not be completed",
    }
}
impl LogDiagnostic {
    pub fn record_message(&mut self, outcome: &str, kind: &str, stage: &str, message: &str) {
        // A later cancellation/limit/unknown must not overwrite a confirmed error.
        if self.outcome_version == OUTCOME_VERSION
            && matches!(self.attempt_outcome.as_str(), "failed" | "timed_out")
        {
            return;
        }
        self.outcome_version = OUTCOME_VERSION;
        self.attempt_outcome = outcome.to_string();
        self.failure_kind = Some(capped(kind.to_string(), 64));
        self.failure_stage = Some(capped(stage.to_string(), 64));
        self.error_message = Some(public_message(kind).to_string());
        self.error_causes = vec![sanitize_cause(message)];
    }
    pub fn record_failure(
        &mut self,
        outcome: &str,
        kind: &str,
        stage: &str,
        error: &(dyn std::error::Error + 'static),
    ) {
        if self.outcome_version == OUTCOME_VERSION
            && matches!(self.attempt_outcome.as_str(), "failed" | "timed_out")
        {
            return;
        }
        self.record_message(outcome, kind, stage, &error.to_string());
        let mut causes = vec![];
        let mut next = Some(error);
        for _ in 0..8 {
            let Some(cause) = next else {
                break;
            };
            causes.push(sanitize_cause(&cause.to_string()));
            next = cause.source();
        }
        self.error_causes = causes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authoritative_results_never_promote_legacy_or_future_markers() {
        for version in [0, 2, 99] {
            assert!(!is_error(Some(200), Some(200), version, "failed"));
            assert_eq!(
                effective_outcome(Some(200), Some(200), version, "completed"),
                "unknown"
            );
        }
        for status in [400, 429, 500, 599] {
            assert!(is_error(Some(status), None, 0, "unknown"));
            assert!(is_error(None, Some(status), 0, "unknown"));
        }
        assert!(!is_error(Some(600), None, 0, "unknown"));
        assert!(is_error(Some(200), Some(200), 1, "failed"));
        assert!(is_error(Some(200), Some(200), 1, "timed_out"));
        for outcome in ["cancelled", "output_limited", "unknown", "completed"] {
            assert!(!is_error(Some(200), Some(200), 1, outcome));
        }
        assert_eq!(
            effective_outcome(Some(500), Some(200), 1, "completed"),
            "error"
        );
    }

    #[test]
    fn force_payload_retains_versioned_unknown_but_not_legacy_default() {
        let diagnostic = |version: i32, outcome: &str| LogDiagnostic {
            outcome_version: version,
            attempt_outcome: outcome.into(),
            ..Default::default()
        };
        // Actively classified ambiguity: 200 OK but completion undeterminable.
        assert!(force_payload(
            Some(200),
            Some(200),
            &diagnostic(OUTCOME_VERSION, "unknown")
        ));
        // Version-0 unknown is the un-upgraded default (legacy rows, paths
        // without lifecycle observation); retaining it would record everything.
        assert!(!force_payload(
            Some(200),
            Some(200),
            &diagnostic(0, "unknown")
        ));
        // Confirmed successes stay excluded, as before.
        assert!(!force_payload(
            Some(200),
            Some(200),
            &diagnostic(OUTCOME_VERSION, "completed")
        ));
        // Existing forced outcomes are unchanged.
        assert!(force_payload(
            Some(200),
            Some(200),
            &diagnostic(OUTCOME_VERSION, "cancelled")
        ));
        assert!(force_payload(
            Some(200),
            Some(200),
            &diagnostic(OUTCOME_VERSION, "output_limited")
        ));
        // Future versions never force through the outcome channel.
        assert!(!force_payload(
            Some(200),
            Some(200),
            &diagnostic(2, "unknown")
        ));
    }
    #[test]
    fn diagnostic_messages_are_bounded_and_credentials_are_not_reflected() {
        assert_eq!(
            sanitize_cause("read failed for https://user:pass@host/path"),
            "read failed for [URL omitted]"
        );
        assert_eq!(
            sanitize_cause("bad Authorization: Bearer secret"),
            "Sensitive diagnostic text omitted"
        );
        assert!(sanitize_cause(&"错".repeat(900)).len() <= 512);
        let mut d = LogDiagnostic::default();
        d.record_message("failed", "upstream_read", "read", "decode failed");
        d.record_message(
            "cancelled",
            "downstream_disconnect",
            "delivery",
            "cancelled",
        );
        assert_eq!(d.attempt_outcome, "failed");
    }
}
