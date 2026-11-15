//! Bounded, original-wire Claude serving-model evidence; never route attribution.

use serde_json::{Value, json};

/// One bounded model id and its provenance, independent of requested/routed ids.
#[derive(Default)]
pub(super) struct ClaudeResponseModel {
    served: Option<String>,
    source: &'static str,
    fallback_seen: bool,
}

impl ClaudeResponseModel {
    fn record(&mut self, value: Option<&Value>, source: &'static str, fallback: bool) {
        let Some(model) = value.and_then(Value::as_str).map(str::trim) else {
            return;
        };
        // Upstream-controlled identifiers reach diagnostics, not arbitrary text.
        if model.is_empty() || model.len() > 128 || model.chars().any(char::is_control) {
            return;
        }
        if !fallback && self.fallback_seen {
            return;
        }
        self.served = Some(model.to_owned());
        self.source = source;
        self.fallback_seen |= fallback;
    }

    pub(super) fn observe(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.record(value.pointer("/message/model"), "message.model", false);
            }
            Some("content_block_start") => {
                if value.pointer("/content_block/type").and_then(Value::as_str) == Some("fallback")
                {
                    self.record(
                        value.pointer("/content_block/to/model"),
                        "content_block.to.model",
                        true,
                    );
                }
            }
            Some("message" | "message_delta") | None => {
                if value.get("error").is_some_and(|error| !error.is_null()) {
                    return;
                }
                if value.get("type").and_then(Value::as_str) != Some("message_delta") {
                    self.record(value.get("model"), "model", false);
                    if let Some(content) = value.get("content").and_then(Value::as_array) {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("fallback") {
                                self.record(
                                    block.pointer("/to/model"),
                                    "content.fallback.to.model",
                                    true,
                                );
                            }
                        }
                    }
                }
                // The last valid fallback iteration is the final serving model,
                // even if the message-level model still names the starting one.
                if let Some(iterations) =
                    value.pointer("/usage/iterations").and_then(Value::as_array)
                {
                    for iteration in iterations {
                        if iteration.get("type").and_then(Value::as_str) == Some("fallback_message")
                        {
                            self.record(
                                iteration.get("model"),
                                "usage.iterations.fallback_message.model",
                                true,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn metadata(&self) -> Option<Value> {
        self.served.as_ref().map(|model| {
            json!({
                "served_model": model,
                "source": self.source,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_response_model_tracks_fallback_without_reverting_to_start_model() {
        let mut model = ClaudeResponseModel::default();
        model.observe(&json!({"type": "message_start", "message": {"model": "claude-opus-5-5"}}));
        assert_eq!(model.metadata().unwrap()["served_model"], "claude-opus-5-5");
        model.observe(&json!({"type": "content_block_start", "content_block": {"type": "fallback", "to": {"model": "claude-opus-4-8"}}}));
        assert_eq!(model.metadata().unwrap()["served_model"], "claude-opus-4-8");
        assert_eq!(
            model.metadata().unwrap()["source"],
            "content_block.to.model"
        );
        model.observe(&json!({"type": "message", "model": "claude-opus-5-5"}));
        model.observe(&json!({"type": "message_stop"}));
        assert_eq!(model.metadata().unwrap()["served_model"], "claude-opus-4-8");
    }

    #[test]
    fn claude_response_model_prefers_last_valid_fallback_iteration() {
        for kind in ["message", "message_delta"] {
            let mut model = ClaudeResponseModel::default();
            model.observe(
                &json!({"type": kind, "model": "claude-opus-5-5", "usage": {"iterations": [
                    {"type": "fallback_message", "model": "claude-opus-4-8"},
                    {"type": "message", "model": "not-a-fallback"},
                    {"type": "fallback_message", "model": " claude-sonnet-5 "},
                    {"type": "fallback_message", "model": " "},
                    {"type": "fallback_message", "model": 42},
                    {"type": "fallback_message", "model": "x".repeat(129)}
                ]}}),
            );
            assert_eq!(model.metadata().unwrap()["served_model"], "claude-sonnet-5");
            assert_eq!(
                model.metadata().unwrap()["source"],
                "usage.iterations.fallback_message.model"
            );
        }
    }

    #[test]
    fn claude_response_model_reads_buffered_model_and_fallback_content() {
        let mut model = ClaudeResponseModel::default();
        model.observe(&json!({"model": "claude-opus-4-8"}));
        assert_eq!(model.metadata().unwrap()["served_model"], "claude-opus-4-8");
        assert_eq!(model.metadata().unwrap()["source"], "model");
        model.observe(
            &json!({"type": "message", "model": "claude-opus-5-5", "content": [
                {"type": "fallback", "to": {"model": "claude-sonnet-5"}},
                {"type": "text", "text": "done", "model": "not-evidence"}
            ]}),
        );
        assert_eq!(model.metadata().unwrap()["served_model"], "claude-sonnet-5");
        assert_eq!(
            model.metadata().unwrap()["source"],
            "content.fallback.to.model"
        );
    }

    #[test]
    fn claude_response_model_ignores_untrusted_non_model_fields_and_invalid_ids() {
        let mut model = ClaudeResponseModel::default();
        for value in [
            json!({"type": "error", "model": "not-served"}),
            json!({"type": "content_block_start", "content_block": {"type": "tool_use", "to": {"model": "not-served"}}}),
            json!({"type": "message_delta", "usage": {"iterations": [{"type": "message", "model": "not-served"}]}}),
            json!({"type": "message", "model": 42}),
            json!({"type": "message", "model": " "}),
            json!({"type": "message", "model": "x".repeat(129)}),
            json!({"type": "message", "model": "bad\nmodel"}),
        ] {
            model.observe(&value);
            assert!(model.metadata().is_none(), "{value}");
        }
        model.observe(&json!({"type": "message", "model": "x".repeat(128)}));
        assert_eq!(model.metadata().unwrap()["served_model"], "x".repeat(128));
    }
}
