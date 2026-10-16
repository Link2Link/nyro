//! Pure Devin response state machine, shared by streaming and buffered transports.
//! A clean Connect EOS is mandatory; stop_reason alone never completes a response.
use super::wire::{self, Frame};
use crate::{
    error::GatewayError,
    protocol::ir::{AiStreamDelta, ToolCall, ToolCallKind, Usage},
};
use prost::Message as _;
use serde_json::Value;
use std::collections::HashMap;

const MAX_TOOLS: usize = 128;
const MAX_BUFFERED: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Utf8 {
    pending: Vec<u8>,
}
impl Utf8 {
    fn push(&mut self, bytes: &[u8]) -> Result<String, GatewayError> {
        self.pending.extend_from_slice(bytes);
        let length = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(wire::protocol_error()),
        };
        let text = String::from_utf8(self.pending[..length].to_vec())
            .map_err(|_| wire::protocol_error())?;
        self.pending.drain(..length);
        Ok(text)
    }
    fn finish(&self) -> Result<(), GatewayError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(wire::protocol_error())
        }
    }
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: Vec<u8>,
    custom: bool,
}

pub struct Session {
    decoder: wire::Decoder,
    model: String,
    id: String,
    started: bool,
    done: bool,
    failed: bool,
    thinking_seen: bool,
    text_utf8: Utf8,
    thinking_utf8: Utf8,
    signature: Vec<u8>,
    pre_tool_text: String,
    post_tool_text: String,
    tools: Vec<PendingTool>,
    tool_by_id: HashMap<String, usize>,
    last_tool: Option<usize>,
    usage: wire::UsageSnapshot,
    usage_seen: bool,
    stop_reason: u32,
    buffered: usize,
}

impl Session {
    pub fn new(model: &str) -> Self {
        Self {
            decoder: wire::Decoder::new(),
            model: model.into(),
            id: format!("devin-{}", uuid::Uuid::new_v4()),
            started: false,
            done: false,
            failed: false,
            thinking_seen: false,
            text_utf8: Utf8::default(),
            thinking_utf8: Utf8::default(),
            signature: vec![],
            pre_tool_text: String::new(),
            post_tool_text: String::new(),
            tools: vec![],
            tool_by_id: HashMap::new(),
            last_tool: None,
            usage: wire::UsageSnapshot::default(),
            usage_seen: false,
            stop_reason: 0,
            buffered: 0,
        }
    }
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        self.done
    }

    pub(crate) fn usage_provenance(&self) -> crate::performance::NativeUsageProvenance {
        use crate::performance::NativeUsageProvenance;
        if self.usage.input.is_some() && self.usage.output.is_some() {
            NativeUsageProvenance::Reported
        } else if self.usage.input.is_some()
            || self.usage.output.is_some()
            || self.usage.cache_read.is_some()
            || self.usage.cache_write.is_some()
        {
            NativeUsageProvenance::Partial
        } else {
            NativeUsageProvenance::Unknown
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<AiStreamDelta>, GatewayError> {
        if self.failed {
            return Err(wire::protocol_error());
        }
        if self.done {
            return if bytes.is_empty() {
                Ok(vec![])
            } else {
                self.failed = true;
                Err(wire::protocol_error())
            };
        }
        let result = self.push_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<Vec<AiStreamDelta>, GatewayError> {
        let mut events = Vec::new();
        for frame in self.decoder.feed(bytes)? {
            match frame {
                Frame::Data(payload) => {
                    let response = wire::ChatResponse::decode(payload.as_slice())
                        .map_err(|_| wire::protocol_error())?;
                    self.consume(response, &mut events)?;
                }
                Frame::End(payload) => {
                    validate_trailer(&payload)?;
                    self.complete(&mut events)?;
                }
            }
        }
        Ok(events)
    }

    /// Invoke when the HTTP body ends. An empty body or partial frame is not success.
    /// Already completed sessions return no duplicate terminal events.
    pub fn finish(&mut self) -> Result<Vec<AiStreamDelta>, GatewayError> {
        if self.failed || self.decoder.finish().is_err() || !self.done {
            self.failed = true;
            return Err(wire::protocol_error());
        }
        Ok(vec![])
    }

    fn start(&mut self, events: &mut Vec<AiStreamDelta>) {
        if !self.started {
            self.started = true;
            events.push(AiStreamDelta::MessageStart {
                id: self.id.clone(),
                model: self.model.clone(),
            });
        }
    }
    fn reserve(&mut self, size: usize) -> Result<(), GatewayError> {
        self.buffered = self
            .buffered
            .checked_add(size)
            .ok_or_else(wire::protocol_error)?;
        if self.buffered > MAX_BUFFERED {
            return Err(wire::protocol_error());
        }
        Ok(())
    }
    fn consume(
        &mut self,
        response: wire::ChatResponse,
        events: &mut Vec<AiStreamDelta>,
    ) -> Result<(), GatewayError> {
        if !self.started && !response.id.is_empty() {
            self.id = response.id.clone();
        }
        if response.stop_reason != 0 {
            self.stop_reason = response.stop_reason;
        }
        if let Some(usage) = response.usage {
            self.usage_seen = true;
            if usage.input.is_some() {
                self.usage.input = usage.input;
            }
            if usage.output.is_some() {
                self.usage.output = usage.output;
            }
            if usage.cache_read.is_some() {
                self.usage.cache_read = usage.cache_read;
            }
            if usage.cache_write.is_some() {
                self.usage.cache_write = usage.cache_write;
            }
            if !usage.model.is_empty() {
                self.usage.model = usage.model;
            }
        }
        for group in response.dimensions {
            if !group.title.eq_ignore_ascii_case("Token Usage") {
                continue;
            }
            for metric in group.metrics {
                let Some(value) = metric.value else {
                    continue;
                };
                if !value.value.is_finite()
                    || value.value < 0.0
                    || f64::from(value.value) > f64::from(u32::MAX)
                {
                    return Err(wire::protocol_error());
                }
                let target = match metric.key.as_str() {
                    "input_tokens" => &mut self.usage.input,
                    "output_tokens" => &mut self.usage.output,
                    "cached_input_tokens" => &mut self.usage.cache_read,
                    _ => continue,
                };
                if target.is_none() {
                    *target = Some(value.value as u32);
                }
                self.usage_seen = true;
            }
        }
        if !response.thinking.is_empty() {
            self.thinking_seen = true;
            let text = self.thinking_utf8.push(&response.thinking)?;
            if !text.is_empty() {
                self.start(events);
                events.push(AiStreamDelta::ThinkingDelta(text));
            }
        }
        if !response.signature.is_empty() {
            self.thinking_seen = true;
            self.reserve(response.signature.len())?;
            self.signature.extend_from_slice(&response.signature);
        }
        for tool in response.tools {
            self.tool(tool)?;
        }
        if !response.text.is_empty() {
            let text = self.text_utf8.push(&response.text)?;
            if !text.is_empty() {
                if !self.tools.is_empty() {
                    self.reserve(text.len())?;
                    self.post_tool_text.push_str(&text);
                } else if self.thinking_seen {
                    // Block protocols cannot accept a signature after the text closes thinking.
                    self.reserve(text.len())?;
                    self.pre_tool_text.push_str(&text);
                } else {
                    self.start(events);
                    events.push(AiStreamDelta::TextDelta(text));
                }
            }
        }
        Ok(())
    }

    fn tool(&mut self, delta: wire::CallDelta) -> Result<(), GatewayError> {
        let existing = if !delta.id.is_empty() {
            self.tool_by_id.get(&delta.id).copied().or_else(|| {
                self.last_tool
                    .filter(|index| self.tools[*index].id.is_empty())
            })
        } else {
            self.last_tool
        };
        let index = if let Some(index) = existing {
            index
        } else {
            if self.tools.len() >= MAX_TOOLS {
                return Err(wire::protocol_error());
            }
            self.tools.push(PendingTool::default());
            self.tools.len() - 1
        };
        let arguments = if delta.arguments.is_empty() {
            delta.invalid_json
        } else {
            delta.arguments
        };
        self.reserve(arguments.len() + delta.id.len() + delta.name.len())?;
        let tool = &mut self.tools[index];
        if !delta.id.is_empty() {
            if !tool.id.is_empty() && tool.id != delta.id {
                return Err(wire::protocol_error());
            }
            tool.id = delta.id.clone();
            self.tool_by_id.insert(delta.id, index);
        }
        if !delta.name.is_empty() {
            if !tool.name.is_empty() && tool.name != delta.name {
                return Err(wire::protocol_error());
            }
            tool.name = delta.name;
        }
        tool.custom |= delta.custom;
        tool.arguments.extend_from_slice(&arguments);
        self.last_tool = Some(index);
        Ok(())
    }

    fn complete(&mut self, events: &mut Vec<AiStreamDelta>) -> Result<(), GatewayError> {
        self.text_utf8.finish()?;
        self.thinking_utf8.finish()?;
        let signature = String::from_utf8(std::mem::take(&mut self.signature))
            .map_err(|_| wire::protocol_error())?;
        let usage = self.final_usage()?;
        let mut calls = Vec::new();
        for (index, tool) in self.tools.iter_mut().enumerate() {
            if tool.id.is_empty() || tool.name.is_empty() {
                return Err(wire::protocol_error());
            }
            let arguments = String::from_utf8(std::mem::take(&mut tool.arguments))
                .map_err(|_| wire::protocol_error())?;
            calls.push((
                index,
                ToolCall {
                    id: wire::encode_tool_id(&tool.id),
                    name: tool.name.clone(),
                    namespace: None,
                    kind: if tool.custom {
                        ToolCallKind::Custom
                    } else {
                        ToolCallKind::Function
                    },
                    arguments: if arguments.is_empty() && !tool.custom {
                        "{}".into()
                    } else {
                        arguments
                    },
                },
            ));
        }
        self.start(events);
        if !signature.is_empty() {
            events.push(AiStreamDelta::ThinkingSignature(signature));
        }
        if !self.pre_tool_text.is_empty() {
            events.push(AiStreamDelta::TextDelta(std::mem::take(
                &mut self.pre_tool_text,
            )));
        }
        // Contiguous per-call events work for both indexed OpenAI and block-based Anthropic.
        for (index, call) in calls {
            events.push(AiStreamDelta::ToolCallStart {
                index,
                id: call.id.clone(),
                name: call.name.clone(),
                namespace: None,
                kind: call.kind,
            });
            events.push(AiStreamDelta::ToolCallDelta {
                index,
                arguments: call.arguments.clone(),
            });
            events.push(AiStreamDelta::ToolCallComplete {
                index,
                tool_call: call,
            });
        }
        if !self.post_tool_text.is_empty() {
            events.push(AiStreamDelta::TextDelta(std::mem::take(
                &mut self.post_tool_text,
            )));
        }
        if let Some(usage) = usage {
            events.push(AiStreamDelta::Usage(usage));
        }
        let reason = match self.stop_reason {
            1 | 3 => "length",
            11 => "content_filter",
            10 => "tool_calls",
            _ if !self.tools.is_empty() => "tool_calls",
            _ => "stop",
        };
        events.push(AiStreamDelta::Done {
            stop_reason: reason.into(),
        });
        self.done = true;
        Ok(())
    }

    fn final_usage(&self) -> Result<Option<Usage>, GatewayError> {
        if !self.usage_seen {
            return Ok(None);
        }
        // Nyro's IR input is GROSS; Devin field 7.2 is NET of reads and writes.
        let input = self
            .usage
            .input
            .unwrap_or(0)
            .checked_add(self.usage.cache_read.unwrap_or(0))
            .and_then(|n| n.checked_add(self.usage.cache_write.unwrap_or(0)))
            .ok_or_else(wire::protocol_error)?;
        let output = self.usage.output.unwrap_or(0);
        let total = input.checked_add(output).ok_or_else(wire::protocol_error)?;
        Ok(Some(Usage {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: total,
            cache_read_tokens: self.usage.cache_read,
            cache_creation_tokens: self.usage.cache_write,
            ..Default::default()
        }))
    }
}

fn validate_trailer(payload: &[u8]) -> Result<(), GatewayError> {
    if payload.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let trailer: Value = serde_json::from_slice(payload).map_err(|_| wire::protocol_error())?;
    if !trailer.is_object() {
        return Err(wire::protocol_error());
    }
    let Some(error) = trailer.get("error").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(wire::protocol_error)?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let status = match code {
        "invalid_argument" if !message.contains("internal error") => 400,
        "unauthenticated" => 401,
        "permission_denied" if message.contains("high demand") => 429,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "unavailable" => 503,
        "canceled" => 499,
        "deadline_exceeded" => 504,
        "failed_precondition"
            if ["quota", "credit", "acu", "exhausted", "limit"]
                .iter()
                .any(|word| message.contains(word)) =>
        {
            429
        }
        "failed_precondition" => 400,
        _ => 502,
    };
    // Do not echo opaque upstream diagnostics: they can contain request text/credentials.
    Err(GatewayError::UpstreamStatus {
        provider: "devin".into(),
        status,
        body: Some("Devin Connect stream returned an upstream error".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(response: wire::ChatResponse) -> Vec<u8> {
        wire::envelope(&response.encode_to_vec()).unwrap()
    }
    fn eos(payload: &[u8]) -> Vec<u8> {
        let mut bytes = wire::envelope(payload).unwrap();
        bytes[0] = 2;
        bytes
    }
    fn text(value: &[u8]) -> Vec<u8> {
        frame(wire::ChatResponse {
            text: value.to_vec(),
            ..Default::default()
        })
    }
    fn tool(id: &str, name: &str, args: &str) -> wire::CallDelta {
        wire::CallDelta {
            id: id.into(),
            name: name.into(),
            arguments: args.as_bytes().into(),
            ..Default::default()
        }
    }
    fn tools(items: Vec<wire::CallDelta>) -> Vec<u8> {
        frame(wire::ChatResponse {
            tools: items,
            ..Default::default()
        })
    }

    #[test]
    fn fragmented_text_and_utf8_then_terminal() {
        let mut session = Session::new("swe-2");
        let mut bytes = text(&[0xe7]);
        bytes.extend(text(&[0x94, 0xb2]));
        bytes.extend(eos(b"{}"));
        let mut events = vec![];
        for byte in bytes {
            events.extend(session.push(&[byte]).unwrap());
        }
        assert!(matches!(
            events.first(),
            Some(AiStreamDelta::MessageStart { .. })
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AiStreamDelta::TextDelta(t) if t == "甲"))
        );
        assert!(
            matches!(events.last(), Some(AiStreamDelta::Done { stop_reason }) if stop_reason == "stop")
        );
        assert!(session.is_done());
        assert!(session.finish().unwrap().is_empty());
    }

    #[test]
    fn eos_required_and_no_completion_on_errors() {
        let mut session = Session::new("test");
        session.push(&text(b"hello")).unwrap();
        assert!(session.finish().is_err());
        let mut session = Session::new("test");
        session.push(&[0, 0]).unwrap();
        assert!(session.finish().is_err());
        assert!(Session::new("test").finish().is_err());
        assert!(Session::new("test").push(&eos(b"not JSON")).is_err());
        assert!(Session::new("test").push(&[0, 0, 0, 0, 1, 0xff]).is_err());
        let mut session = Session::new("test");
        session.push(&text(&[0xe7])).unwrap();
        assert!(session.push(&eos(b"{}")).is_err());
    }

    #[test]
    fn trailer_status_mapping_and_redaction() {
        for (code, message, status) in [
            ("unauthenticated", "token SECRET", 401),
            ("permission_denied", "high demand", 429),
            ("permission_denied", "denied", 403),
            ("resource_exhausted", "limit", 429),
            ("failed_precondition", "no credit", 429),
            ("failed_precondition", "bad state", 400),
            ("invalid_argument", "internal error", 502),
            ("invalid_argument", "bad input", 400),
            ("unavailable", "busy", 503),
            ("canceled", "canceled", 499),
            ("deadline_exceeded", "late", 504),
        ] {
            let trailer = serde_json::json!({"error":{"code":code,"message":message}});
            let mut session = Session::new("test");
            let error = session
                .push(&eos(trailer.to_string().as_bytes()))
                .unwrap_err();
            assert_eq!(error.http_status().as_u16(), status);
            assert!(!error.to_string().contains("SECRET"));
            assert!(!session.is_done());
        }
    }

    #[test]
    fn interleaved_tool_calls_are_aggregated_by_id_and_emitted_contiguously() {
        let mut session = Session::new("test");
        session
            .push(&tools(vec![
                tool("one /", "first", "{\"x\":"),
                tool("two", "second", "{\"y\":"),
            ]))
            .unwrap();
        session
            .push(&tools(vec![tool("one /", "", "1}"), tool("two", "", "2}")]))
            .unwrap();
        session.push(&text(b"after tools")).unwrap();
        let events = session.push(&eos(b"{}")).unwrap();
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let AiStreamDelta::ToolCallComplete { tool_call, .. } = e {
                    Some(tool_call)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, "{\"x\":1}");
        assert_eq!(calls[1].arguments, "{\"y\":2}");
        assert_eq!(wire::decode_tool_id(&calls[0].id), "one /");
        let text_index = events
            .iter()
            .position(|e| matches!(e, AiStreamDelta::TextDelta(_)))
            .unwrap();
        let last_tool_index = events
            .iter()
            .rposition(|e| matches!(e, AiStreamDelta::ToolCallComplete { .. }))
            .unwrap();
        assert!(text_index > last_tool_index);
        assert!(
            matches!(events.last(), Some(AiStreamDelta::Done { stop_reason }) if stop_reason == "tool_calls")
        );
    }

    #[test]
    fn anonymous_continuations_late_ids_and_custom_tool_fallback() {
        let mut session = Session::new("test");
        session.push(&tools(vec![tool("", "bash", "")])).unwrap();
        let delta = wire::CallDelta {
            id: "late".into(),
            invalid_json: b"echo hi".to_vec(),
            custom: true,
            ..Default::default()
        };
        session.push(&tools(vec![delta])).unwrap();
        let events = session.push(&eos(b"{}")).unwrap();
        assert!(events.iter().any(|e| matches!(e, AiStreamDelta::ToolCallComplete { tool_call, .. } if tool_call.id == "late" && tool_call.arguments == "echo hi" && tool_call.kind == ToolCallKind::Custom)));
    }

    #[test]
    fn late_split_signature_precedes_buffered_text() {
        let mut session = Session::new("test");
        let first = session
            .push(&frame(wire::ChatResponse {
                thinking: b"plan".into(),
                signature: b"sealed.".into(),
                ..Default::default()
            }))
            .unwrap();
        assert!(
            first
                .iter()
                .any(|e| matches!(e, AiStreamDelta::ThinkingDelta(t) if t == "plan"))
        );
        assert!(session.push(&text(b"answer")).unwrap().is_empty());
        session
            .push(&frame(wire::ChatResponse {
                signature: b"v1.signature".into(),
                ..Default::default()
            }))
            .unwrap();
        let last = session.push(&eos(b"{}")).unwrap();
        assert!(
            matches!(&last[0], AiStreamDelta::ThinkingSignature(s) if s == "sealed.v1.signature")
        );
        assert!(matches!(&last[1], AiStreamDelta::TextDelta(s) if s == "answer"));
    }

    #[test]
    fn usage_snapshots_are_not_summed_and_cache_write_is_gross_input() {
        let mut session = Session::new("test");
        for input in [Some(3), None] {
            session
                .push(&frame(wire::ChatResponse {
                    usage: Some(wire::UsageSnapshot {
                        input,
                        output: Some(5),
                        cache_read: Some(7),
                        cache_write: Some(11),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
                .unwrap();
        }
        let events = session.push(&eos(b"{}")).unwrap();
        let usage = events
            .iter()
            .find_map(|e| {
                if let AiStreamDelta::Usage(u) = e {
                    Some(u)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens
            ),
            (21, 5, 26)
        );
        assert_eq!(usage.cache_read_tokens, Some(7));
        assert_eq!(usage.cache_creation_tokens, Some(11));
    }

    #[test]
    fn dimension_usage_and_stop_reasons() {
        for (reason, expected) in [
            (1, "length"),
            (3, "length"),
            (11, "content_filter"),
            (2, "stop"),
        ] {
            let mut session = Session::new("test");
            let dimensions = vec![wire::DimensionGroup {
                title: "Token Usage".into(),
                metrics: vec![wire::DimensionMetric {
                    key: "input_tokens".into(),
                    value: Some(wire::DimensionValue { value: 10.0 }),
                }],
            }];
            session
                .push(&frame(wire::ChatResponse {
                    stop_reason: reason,
                    dimensions,
                    ..Default::default()
                }))
                .unwrap();
            let events = session.push(&eos(b"{}")).unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AiStreamDelta::Usage(u) if u.prompt_tokens == 10))
            );
            assert!(
                matches!(events.last(), Some(AiStreamDelta::Done { stop_reason }) if stop_reason == expected)
            );
        }
    }

    #[test]
    fn tool_limit_and_usage_overflow_fail_closed() {
        let mut session = Session::new("test");
        let many: Vec<_> = (0..=MAX_TOOLS)
            .map(|i| tool(&format!("call_{i}"), "test", "{}"))
            .collect();
        assert!(session.push(&tools(many)).is_err());
        let mut session = Session::new("test");
        session
            .push(&frame(wire::ChatResponse {
                usage: Some(wire::UsageSnapshot {
                    input: Some(u32::MAX),
                    cache_read: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();
        assert!(session.push(&eos(b"{}")).is_err());
    }
}
