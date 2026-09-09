//! Strict, payload-independent performance evidence. Historical rows never prove delivery.
mod lifecycle;
pub(crate) use lifecycle::{Attempt, ProducerGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PerformanceMetadata {
    pub version: i32,
    pub effort_status: String,
    pub effort_raw: Option<String>,
    pub effort_tier: Option<String>,
    pub completion: String,
    pub completion_reason: Option<String>,
    pub response_mode: String,
    pub upstream_duration_ms: Option<i64>,
    pub first_chunk_ms: Option<i64>,
    pub completed_at: Option<i64>,
}
impl Default for PerformanceMetadata {
    fn default() -> Self {
        Self {
            version: 0,
            effort_status: "unknown".into(),
            effort_raw: None,
            effort_tier: None,
            completion: "unknown".into(),
            completion_reason: None,
            response_mode: "unknown".into(),
            upstream_duration_ms: None,
            first_chunk_ms: None,
            completed_at: None,
        }
    }
}
fn canonical(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

/// Recover only effort from a bounded historical outgoing payload. No old payload
/// can establish cancellation-free downstream delivery, even if it contains Done.
pub fn recover_historical_effort(body: &str, protocol: Option<&str>) -> PerformanceMetadata {
    let mut metadata = PerformanceMetadata {
        version: 1,
        ..Default::default()
    };
    if body.len() > 1024 * 1024 {
        return metadata;
    }
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return metadata;
    };
    if !value.is_object() {
        return metadata;
    }
    // Vendor envelopes can legitimately contain another protocol's spelling.
    let _ = protocol;
    let value = value
        .get("request")
        .filter(|v| v.is_object())
        .unwrap_or(&value);
    let tier_paths = [
        "/reasoning_effort",
        "/reasoning/effort",
        "/output_config/effort",
        "/generationConfig/thinkingConfig/thinkingLevel",
        "/generationConfig/thinkingConfig/thinking_level",
        "/generation_config/thinking_config/thinking_level",
    ];
    let mut labels = Vec::new();
    let mut malformed = false;
    for path in tier_paths {
        if let Some(v) = value.pointer(path) {
            if let Some(s) = v.as_str().filter(|s| !s.trim().is_empty()) {
                labels.push(s.to_owned());
            } else {
                malformed = true;
            }
        }
    }
    for path in ["/enable_thinking"] {
        if let Some(v) = value.pointer(path) {
            if let Some(b) = v.as_bool() {
                labels.push(if b { "enabled" } else { "none" }.into());
            } else {
                malformed = true;
            }
        }
    }
    for path in [
        "/thinking",
        "/generationConfig/thinkingConfig",
        "/generation_config/thinking_config",
    ] {
        if let Some(v) = value.pointer(path) {
            if !v.is_object() {
                malformed = true;
                continue;
            }
            for key in ["budget_tokens", "thinkingBudget", "thinking_budget"] {
                if let Some(b) = v.get(key) {
                    if let Some(n) = b.as_i64() {
                        labels.push(format!("budget:{n}"));
                    } else {
                        malformed = true;
                    }
                }
            }
            if let Some(kind) = v.get("type") {
                if let Some(s) = kind.as_str() {
                    labels.push(s.into());
                } else {
                    malformed = true;
                }
            }
        }
    }
    // Preserve explicit but unrecognized reasoning objects as mixed, not absent.
    if labels.is_empty() && !malformed {
        if let Some(v) = [
            "/thinking",
            "/reasoning",
            "/generationConfig/thinkingConfig",
            "/generation_config/thinking_config",
        ]
        .iter()
        .find_map(|p| value.pointer(p))
        {
            if !v.is_object() {
                malformed = true;
            } else {
                labels.push(v.to_string());
            }
        }
    }
    metadata.effort_raw = if labels.len() == 1 {
        labels.first().cloned()
    } else if !labels.is_empty() {
        Some(serde_json::to_string(&labels).unwrap())
    } else {
        None
    };
    if malformed {
        return metadata;
    }
    metadata.effort_status = if labels.is_empty() {
        "absent"
    } else {
        "present"
    }
    .into();
    let tiers: Vec<_> = labels.iter().filter_map(|s| canonical(s)).collect();
    // Enabled/adaptive is an orthogonal switch, but budgets or conflicting tiers
    // cannot prove one exact upstream tier. Never choose the first conflicting field.
    if let Some(first) = tiers.first() {
        let compatible = labels.iter().all(|s| {
            canonical(s) == Some(*first)
                || matches!(s.to_ascii_lowercase().as_str(), "enabled" | "adaptive")
        });
        if compatible {
            metadata.effort_tier = Some((*first).into());
        } else {
            metadata.effort_status = "unknown".into();
        }
    }
    metadata
}

/// Original wire evidence, never fed converted or synthetic terminal events.
#[derive(Default)]
pub(crate) struct Terminal {
    buffer: Vec<u8>,
    event_data: String,
    pub completion: Option<&'static str>,
    pub reason: Option<String>,
    pub error_message: Option<String>,
    pub(crate) sse: bool,
    invalid: bool,
    unknown_reason: bool,
    branches: std::collections::BTreeMap<String, bool>,
    anthropic: bool,
    message_stop: bool,
    chat: bool,
    done: bool,
}
impl Terminal {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.buffer.len().saturating_add(bytes.len()) > 1024 * 1024 {
            self.invalid = true;
            self.buffer.clear();
            return;
        }
        self.buffer.extend_from_slice(bytes);
        if !self.sse {
            let text = String::from_utf8_lossy(&self.buffer);
            let text = text.trim_start();
            self.sse =
                text.starts_with("data:") || text.starts_with("event:") || text.starts_with(':');
        }
        if self.sse {
            while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=end).collect();
                self.line(&line);
            }
        }
    }
    fn event(&mut self) {
        let data = std::mem::take(&mut self.event_data);
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return;
        }
        if data.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(value) => self.json(&value),
            Err(_) => self.invalid = true,
        }
    }
    fn line(&mut self, line: &[u8]) {
        let Ok(line) = std::str::from_utf8(line) else {
            self.invalid = true;
            return;
        };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            self.event();
        } else if let Some(data) = line.strip_prefix("data:") {
            if !self.event_data.is_empty() {
                self.event_data.push('\n');
            }
            self.event_data
                .push_str(data.strip_prefix(' ').unwrap_or(data));
            if self.event_data.len() > 1024 * 1024 {
                self.invalid = true;
                self.event_data.clear();
            }
        }
    }
    fn reason(&mut self, reason: &str) {
        let state = match reason.to_ascii_lowercase().as_str() {
            "length" | "max_tokens" | "max_output_tokens" => "output_limited",
            "response.incomplete" | "incomplete" => "unknown",
            "error" | "failed" | "response.failed" => "failed",
            "cancelled" | "canceled" => "cancelled",
            "stop" | "end_turn" | "endturn" | "tool_calls" | "tool_use" | "function_call"
            | "stop_sequence" | "completed" | "response.completed" => "completed",
            _ => {
                self.unknown_reason = true;
                return;
            }
        };
        let rank = |s| match s {
            "failed" => 5,
            "cancelled" => 4,
            "output_limited" => 3,
            "unknown" => 2,
            _ => 1,
        };
        if self.completion.is_none_or(|old| rank(state) > rank(old)) {
            self.completion = Some(state);
            self.reason = Some(reason.into());
        }
    }
    fn json(&mut self, value: &Value) {
        if let Some(array) = value.as_array() {
            for v in array {
                self.json(v);
            }
            return;
        }
        if value.get("error").is_some_and(|e| !e.is_null()) {
            self.reason("error");
            self.error_message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(|s| {
                    let mut end = s.len().min(2048);
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    s[..end].to_owned()
                });
        }
        for p in ["/finish_reason", "/stop_reason", "/delta/stop_reason"] {
            if let Some(v) = value.pointer(p).filter(|v| !v.is_null()) {
                if let Some(s) = v.as_str() {
                    self.reason(s);
                } else {
                    self.invalid = true;
                }
            }
        }
        if let Some(status) = value.get("status").and_then(Value::as_str) {
            if !matches!(status, "in_progress" | "queued") {
                self.reason(status);
            }
        }
        if let Some(kind) = value.get("type").and_then(Value::as_str) {
            if matches!(kind, "message_start" | "message_delta" | "message_stop") {
                self.anthropic = true;
            }
            if kind == "message_stop" {
                self.message_stop = true;
            }
            if matches!(
                kind,
                "response.completed" | "response.incomplete" | "response.failed" | "error"
            ) {
                self.reason(kind);
            }
        }
        for field in ["choices", "candidates"] {
            if let Some(items) = value.get(field).and_then(Value::as_array) {
                if field == "choices" {
                    self.chat = true;
                }
                for (position, item) in items.iter().enumerate() {
                    let index = item
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(position as u64);
                    let key = format!("{field}:{index}");
                    if self.branches.len() >= 256 && !self.branches.contains_key(&key) {
                        self.invalid = true;
                        continue;
                    }
                    self.branches.entry(key.clone()).or_insert(false);
                    for field in ["finish_reason", "finishReason"] {
                        if let Some(v) = item.get(field).filter(|v| !v.is_null()) {
                            if let Some(s) = v.as_str() {
                                self.reason(s);
                                self.branches.insert(key.clone(), true);
                            } else {
                                self.invalid = true;
                            }
                        }
                    }
                }
            }
        }
        if let Some(reason) = value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            self.reason(reason);
        }
        if let Some(response) = value.get("response") {
            self.json(response);
        }
    }
    /// Unambiguous incremental evidence that the upstream stream reached a
    /// completed terminal. Unlike reading `completion` directly, this stays
    /// false when the bounded observer hit an unrecognized dialect or overflow
    /// that `finish()` would reconcile away — such evidence alone must not
    /// confirm completion.
    pub fn confirmed_completed(&self) -> bool {
        self.completion == Some("completed") && !self.invalid && !self.unknown_reason
    }
    pub fn finish(&mut self) {
        let buffer = std::mem::take(&mut self.buffer);
        if self.sse {
            self.line(&buffer);
            self.event();
        } else if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
            self.json(&value);
        } else {
            self.invalid = true;
        }
        // This is an optional observer, not the conversion parser. Unsupported
        // encodings/dialects and bounded-parser overflow cannot prove failure.
        // Conversely explicit upstream errors survive any later observer issue.
        if !matches!(
            self.completion,
            Some("failed" | "cancelled" | "output_limited")
        ) {
            if self.invalid || self.unknown_reason {
                if self.completion != Some("unknown") {
                    self.completion = None;
                    self.reason = Some("unrecognized original terminal evidence".into());
                }
            } else if self.sse && self.anthropic && !self.message_stop {
                // Anthropic's known message framing requires message_stop.
                self.completion = Some("failed");
                self.reason = Some("missing_terminal".into());
            } else if self.branches.values().any(|done| !done)
                || (self.sse && self.chat && !self.done)
            {
                // Chat-compatible dialects may terminate by finish_reason OR
                // [DONE]. Without sufficient branch evidence remain unknown.
                self.completion = None;
                self.reason = Some("ambiguous terminal evidence".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_effort_strict_and_never_completed() {
        for (raw, tier) in [
            ("minimal", Some("low")),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", Some("xhigh")),
            ("max", Some("max")),
            ("none", None),
            ("adaptive", None),
            ("enabled", None),
        ] {
            let m = recover_historical_effort(
                &serde_json::json!({"reasoning_effort":raw}).to_string(),
                None,
            );
            assert_eq!(m.effort_tier.as_deref(), tier);
            assert_eq!(m.effort_status, "present");
            assert_eq!(m.completion, "unknown");
            assert_eq!(m.completed_at, None);
        }
        assert_eq!(
            recover_historical_effort("{}", None).effort_status,
            "absent"
        );
        for body in [
            "invalid",
            "[]",
            r#"{"reasoning_effort":4}"#,
            r#"{"reasoning_effort":"high","reasoning":{"effort":"low"}}"#,
        ] {
            let m = recover_historical_effort(body, None);
            assert_eq!(m.effort_status, "unknown");
            assert_eq!(m.effort_tier, None);
        }
        assert_eq!(
            recover_historical_effort(
                r#"{"thinking":{"type":"enabled","budget_tokens":5000}}"#,
                None
            )
            .effort_tier,
            None
        );
    }
    #[test]
    fn original_terminals_reject_truncation_and_synthetic_done() {
        for reason in ["length", "max_tokens", "MAX_TOKENS", "max_output_tokens"] {
            let mut t = Terminal::default();
            t.push(
                serde_json::json!({"choices":[{"finish_reason":reason}]})
                    .to_string()
                    .as_bytes(),
            );
            t.finish();
            assert_eq!(t.completion, Some("output_limited"));
        }
        for data in [
            "data: [DONE]\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: broken\n\n",
        ] {
            let mut t = Terminal::default();
            t.push(data.as_bytes());
            t.finish();
            assert_ne!(t.completion, Some("completed"));
        }
        let mut t = Terminal::default();
        for b in
            b"data: {\"choices\":\ndata: [{\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"
        {
            t.push(&[*b]);
        }
        t.finish();
        assert_eq!(t.completion, Some("completed"));
        let mut t = Terminal::default();
        t.push(br#"{"choices":[{"finish_reason":"stop"},{"finish_reason":"safety"}]}"#);
        t.finish();
        assert_ne!(t.completion, Some("completed"));
    }
}
