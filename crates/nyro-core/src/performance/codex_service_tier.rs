//! Codex speed evidence from one outbound Responses request and its original response.

use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;

fn canonical(wire: &str) -> Option<&'static str> {
    match wire {
        "normal" | "default" => Some("normal"),
        "fast" | "priority" => Some("fast"),
        "ultrafast" => Some("ultrafast"),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct CodexServiceTier {
    outbound: Option<&'static str>,
    reported: ReportedTier,
}

#[derive(Default, PartialEq)]
enum ReportedTier {
    #[default]
    Absent,
    Known(&'static str),
    // Don't price an explicitly different or conflicting response tier as
    // the requested one.
    Unknown,
}

impl CodexServiceTier {
    pub(super) fn observe_request(&mut self, bytes: &[u8]) {
        #[derive(Deserialize)]
        struct Envelope<'a> {
            #[serde(borrow, default = "normal")]
            service_tier: Cow<'a, str>,
        }
        fn normal() -> Cow<'static, str> {
            Cow::Borrowed("normal")
        }
        // Skips arbitrarily large input/history rather than materializing a
        // second body. Only a valid JSON object with the field ABSENT defaults.
        self.outbound = if bytes.trim_ascii_start().starts_with(b"{") {
            serde_json::from_slice::<Envelope>(bytes)
                .ok()
                .and_then(|body| canonical(&body.service_tier))
        } else {
            None
        };
    }

    pub(super) fn observe_response(&mut self, envelope: &Value, sse: bool) {
        let response = if sse {
            // A response.created may echo the selection before the backend
            // has served the request. Only the terminal envelope is evidence.
            let event_status = match envelope.get("type").and_then(Value::as_str) {
                Some("response.completed") => "completed",
                Some("response.incomplete") => "incomplete",
                _ => return,
            };
            let Some(response) = envelope.get("response").filter(|v| v.is_object()) else {
                return;
            };
            if response
                .get("status")
                .is_some_and(|v| v.as_str() != Some(event_status))
            {
                return;
            }
            response
        } else {
            if envelope.get("object").and_then(Value::as_str) != Some("response")
                || !matches!(
                    envelope.get("status").and_then(Value::as_str),
                    Some("completed" | "incomplete")
                )
            {
                return;
            }
            envelope
        };
        if response.get("error").is_some_and(|v| !v.is_null()) {
            return;
        }
        match response.get("service_tier").and_then(Value::as_str) {
            None | Some("" | "auto") => {} // No actual serving tier reported.
            Some(wire) => {
                let tier = canonical(wire).map_or(ReportedTier::Unknown, ReportedTier::Known);
                // Contradictory terminal observations are not reliable evidence.
                self.reported = if self.reported == ReportedTier::Absent || self.reported == tier {
                    tier
                } else {
                    ReportedTier::Unknown
                };
            }
        }
    }

    pub(super) fn metadata(&self, reliable_response: bool) -> Option<Value> {
        let (value, source) = match self.reported {
            ReportedTier::Known(value) if reliable_response => (value, "upstream_response"),
            ReportedTier::Unknown if reliable_response => return None,
            _ => (self.outbound?, "upstream_request"),
        };
        Some(json!({"value": value, "source": source}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_service_tier_requires_actual_valid_request_object() {
        let mut tier = CodexServiceTier::default();
        assert_eq!(tier.metadata(true), None);
        for (body, expected) in [
            (r#" {"input":[{"service_tier":"fast"}]}"#, Some("normal")),
            (r#"{"service_tier":"default"}"#, Some("normal")),
            (r#"{"service_tier":"priority"}"#, Some("fast")),
            (r#"{"service_tier":"ultrafast"}"#, Some("ultrafast")),
            (r#"{"service_tier":"auto"}"#, None),
            (r#"{"service_tier":"scale"}"#, None),
            (r#"{"service_tier":null}"#, None),
            (r#"{"service_tier":"fast","service_tier":"default"}"#, None),
            (r#"{"service_tier":"fast""#, None),
            (r#"[]"#, None),
        ] {
            tier.observe_request(body.as_bytes());
            assert_eq!(
                tier.metadata(true),
                expected.map(|value| json!({"value": value,"source":"upstream_request"})),
                "{body}"
            );
        }
        // Large image/history requests still count as observed; this observer
        // does not consult the bounded, potentially truncated payload capture.
        let large = format!(
            r#"{{"input":"{}","service_tier":"fast"}}"#,
            "x".repeat(1024 * 1024)
        );
        tier.observe_request(large.as_bytes());
        assert_eq!(tier.metadata(true).unwrap()["value"], "fast");
    }

    #[test]
    fn codex_service_tier_ignores_echoes_and_rejects_unpriced_responses() {
        let mut tier = CodexServiceTier::default();
        tier.observe_request(br#"{"service_tier":"fast"}"#);
        for (event, sse) in [
            (
                json!({"type":"response.created","response":{"object":"response","status":"completed","service_tier":"default"}}),
                true,
            ),
            (
                json!({"type":"response.completed","response":{"status":"in_progress","service_tier":"default"}}),
                true,
            ),
            (
                json!({"object":"response","status":"completed","service_tier":"auto"}),
                false,
            ),
            (
                json!({"object":"response","status":"failed","service_tier":"default"}),
                false,
            ),
        ] {
            tier.observe_response(&event, sse);
            assert_eq!(
                tier.metadata(true),
                Some(json!({"value":"fast","source":"upstream_request"}))
            );
        }
        tier.observe_response(
            &json!({"object":"response","status":"completed","service_tier":"flex"}),
            false,
        );
        assert_eq!(tier.metadata(true), None);
    }
}
