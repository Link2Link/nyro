//! Kiro response state machine: AWS event-stream frames in, IR deltas out.
//!
//! Wire behavior follows AIClient2API's Kiro provider (`claude-kiro.js`).
//! Kiro has no explicit terminal frame, so a clean HTTP EOF is the only
//! completion signal and `Done` is produced by [`Session::finish`].
//!
//! Ordering contract with the dispatcher: text/thinking stream immediately,
//! tool calls are buffered and emitted as contiguous start/delta/complete
//! groups, `Usage` precedes the single terminal `Done`.
use std::collections::HashMap;

use serde_json::Value;

use super::eventstream::{self, Decoder, Frame};
use crate::error::GatewayError;
use crate::protocol::ir::{AiStreamDelta, ToolCall, ToolCallKind, Usage};

/// Tool the request encoder injects when a request carries no usable tools.
/// Calls to it are dropped: it exists only to satisfy the upstream schema.
pub(super) const PLACEHOLDER_TOOL: &str = "no_tool_available";

const MAX_TOOLS: usize = 128;
const MAX_BUFFERED: usize = 64 * 1024 * 1024;
/// Identical consecutive content chunks shorter than this are legitimate
/// tokens (`\n`, `=`, `}`); only long exact repeats are treated as upstream replay.
const MIN_DEDUP_BYTES: usize = 16;
pub(super) const THINKING_OPEN: &str = "<thinking>";
pub(super) const THINKING_CLOSE: &str = "</thinking>";
const MAX_HELD_WHITESPACE: usize = 1024;

fn failure(status: u16, message: &str) -> GatewayError {
    GatewayError::UpstreamStatus {
        provider: "kiro".into(),
        status,
        body: Some(message.into()),
    }
}

/// Cheap token estimate used only when Kiro reports no token counts. Prose
/// averages about four ASCII bytes per token; non-ASCII scripts about one.
pub(super) fn estimate_tokens(text: &str) -> u32 {
    let (mut ascii, mut wide) = (0u64, 0u64);
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    u32::try_from(ascii.div_ceil(4) + wide).unwrap_or(u32::MAX)
}

// ── Inline `<thinking>` splitter ─────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Segment {
    Text(String),
    Thinking(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitState {
    /// No thinking block seen yet; text passes through.
    Idle,
    InThinking,
    /// Thinking closed; everything left is plain text.
    After,
}

/// Streaming extractor for the `<thinking>…</thinking>` block Kiro models emit
/// in-band when thinking mode was requested through the system prompt.
pub(super) struct ThinkingSplitter {
    state: SplitState,
    buf: String,
    /// Whitespace-only text held back so no empty text block precedes thinking.
    held: String,
    /// Last character already consumed from `buf`, for quote detection.
    prev: Option<char>,
    strip_open_newline: bool,
    strip_close_newlines: u8,
}

impl ThinkingSplitter {
    pub fn new() -> Self {
        Self {
            state: SplitState::Idle,
            buf: String::new(),
            held: String::new(),
            prev: None,
            strip_open_newline: false,
            strip_close_newlines: 0,
        }
    }

    pub fn push(&mut self, chunk: &str) -> Vec<Segment> {
        self.buf.push_str(chunk);
        self.drain(false)
    }

    pub fn finish(&mut self) -> Vec<Segment> {
        self.drain(true)
    }

    fn consume(&mut self, len: usize) -> String {
        let taken: String = self.buf.drain(..len).collect();
        if let Some(last) = taken.chars().next_back() {
            self.prev = Some(last);
        }
        taken
    }

    fn drain(&mut self, finishing: bool) -> Vec<Segment> {
        let mut out = Vec::new();
        loop {
            match self.state {
                SplitState::Idle => {
                    if let Some(pos) = find_real_tag(&self.buf, THINKING_OPEN, self.prev, None) {
                        let before = self.consume(pos);
                        self.consume(THINKING_OPEN.len());
                        let combined = std::mem::take(&mut self.held) + &before;
                        if !combined.trim().is_empty() {
                            out.push(Segment::Text(combined));
                        }
                        self.state = SplitState::InThinking;
                        self.strip_open_newline = true;
                        continue;
                    }
                    let hold = if finishing {
                        0
                    } else {
                        partial_suffix(&self.buf, THINKING_OPEN)
                    };
                    let safe = self.buf.len() - hold;
                    if safe > 0 {
                        let text = self.consume(safe);
                        if text.trim().is_empty()
                            && self.held.len() + text.len() <= MAX_HELD_WHITESPACE
                        {
                            self.held.push_str(&text);
                        } else {
                            out.push(Segment::Text(std::mem::take(&mut self.held) + &text));
                        }
                    }
                    if finishing {
                        let rest = std::mem::take(&mut self.held);
                        if !rest.is_empty() {
                            out.push(Segment::Text(rest));
                        }
                    }
                    return out;
                }
                SplitState::InThinking => {
                    if self.strip_open_newline {
                        if let Some(rest) = self.buf.strip_prefix("\r\n") {
                            let drop = self.buf.len() - rest.len();
                            self.consume(drop);
                            self.strip_open_newline = false;
                        } else if self.buf.starts_with('\n') {
                            self.consume(1);
                            self.strip_open_newline = false;
                        } else if !self.buf.is_empty() && self.buf != "\r" {
                            self.strip_open_newline = false;
                        }
                    }
                    if let Some(pos) = find_real_close(&self.buf, self.prev) {
                        let thought = self.consume(pos);
                        self.consume(THINKING_CLOSE.len());
                        if !thought.is_empty() {
                            out.push(Segment::Thinking(thought));
                        }
                        self.state = SplitState::After;
                        self.strip_close_newlines = 2;
                        continue;
                    }
                    let hold = if finishing {
                        0
                    } else {
                        partial_suffix(&self.buf, THINKING_CLOSE)
                    };
                    let safe = self.buf.len() - hold;
                    if safe > 0 {
                        out.push(Segment::Thinking(self.consume(safe)));
                    }
                    return out;
                }
                SplitState::After => {
                    let text = std::mem::take(&mut self.buf);
                    let mut rest = text.as_str();
                    while self.strip_close_newlines > 0 {
                        if let Some(next) = rest
                            .strip_prefix("\r\n")
                            .or_else(|| rest.strip_prefix('\n'))
                        {
                            rest = next;
                            self.strip_close_newlines -= 1;
                        } else {
                            if !rest.is_empty() {
                                self.strip_close_newlines = 0;
                            }
                            break;
                        }
                    }
                    if !rest.is_empty() {
                        out.push(Segment::Text(rest.to_string()));
                    }
                    return out;
                }
            }
        }
    }
}

fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '`')
}

/// Byte length of the longest suffix of `buf` that is a proper prefix of `tag`.
fn partial_suffix(buf: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|len| {
            buf.len() >= *len
                && buf.is_char_boundary(buf.len() - *len)
                && buf.ends_with(&tag[..*len])
        })
        .unwrap_or(0)
}

/// First occurrence of `tag` that is not wrapped in quotes or backticks. A
/// quoted mention such as `` `<thinking>` `` is prose, not structure.
fn find_real_tag(
    buf: &str,
    tag: &str,
    prev: Option<char>,
    accept: Option<&dyn Fn(&str) -> bool>,
) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = buf[from..].find(tag) {
        let pos = from + rel;
        let before = buf[..pos].chars().next_back().or(prev);
        let after = &buf[pos + tag.len()..];
        let quoted = before.is_some_and(is_quote) || after.chars().next().is_some_and(is_quote);
        if !quoted && accept.is_none_or(|accept| accept(after)) {
            return Some(pos);
        }
        from = pos + 1;
    }
    None
}

/// A closing tag is structural only when followed by a blank line, or when it
/// ends the buffer (only whitespace after it).
fn find_real_close(buf: &str, prev: Option<char>) -> Option<usize> {
    find_real_tag(
        buf,
        THINKING_CLOSE,
        prev,
        Some(&|after: &str| {
            after.starts_with("\n\n") || after.starts_with("\r\n\r\n") || after.trim().is_empty()
        }),
    )
}

// ── Session ──────────────────────────────────────────────────────────────────

/// Request-derived facts the response side needs.
#[derive(Default)]
pub struct SessionOptions {
    /// Scan content for in-band `<thinking>` blocks.
    pub thinking_tags: bool,
    /// Kiro-safe tool name -> original client tool name.
    pub tool_names: HashMap<String, String>,
    /// Local estimate of prompt tokens, used when Kiro reports no context usage.
    pub input_estimate: u32,
    /// Model context window, to turn `contextUsagePercentage` into tokens.
    pub context_tokens: u32,
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    input: String,
}

pub struct Session {
    decoder: Decoder,
    model: String,
    id: String,
    options: SessionOptions,
    splitter: Option<ThinkingSplitter>,
    started: bool,
    done: bool,
    failed: bool,
    last_content: String,
    /// Estimated tokens of visible text and of thinking, for local usage.
    text_tokens: u32,
    thinking_tokens: u32,
    thinking_seen: bool,
    visible_text_seen: bool,
    tools: Vec<PendingTool>,
    tool_by_id: HashMap<String, usize>,
    open_tool: Option<usize>,
    dropped_tool_ids: Vec<String>,
    post_tool_text: String,
    buffered: usize,
    context_percentage: Option<f64>,
}

impl Session {
    pub fn new(model: &str, options: SessionOptions) -> Self {
        let splitter = options.thinking_tags.then(ThinkingSplitter::new);
        Self {
            decoder: Decoder::new(),
            model: model.into(),
            id: format!("kiro-{}", uuid::Uuid::new_v4()),
            options,
            splitter,
            started: false,
            done: false,
            failed: false,
            last_content: String::new(),
            text_tokens: 0,
            thinking_tokens: 0,
            thinking_seen: false,
            visible_text_seen: false,
            tools: Vec::new(),
            tool_by_id: HashMap::new(),
            open_tool: None,
            dropped_tool_ids: Vec::new(),
            post_tool_text: String::new(),
            buffered: 0,
            context_percentage: None,
        }
    }

    /// Whether the reported usage came from Kiro's context percentage rather
    /// than a pure local estimate.
    pub(crate) fn usage_provenance(&self) -> crate::performance::NativeUsageProvenance {
        use crate::performance::NativeUsageProvenance;
        if self.context_percentage.is_some() {
            NativeUsageProvenance::Partial
        } else {
            NativeUsageProvenance::Unknown
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<AiStreamDelta>, GatewayError> {
        if self.failed || self.done {
            return Err(eventstream::protocol_error());
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
            self.frame(frame, &mut events)?;
        }
        Ok(events)
    }

    /// Call once when the HTTP body ends cleanly. Produces the buffered tool
    /// calls, `Usage` and the single terminal `Done`.
    pub fn finish(&mut self) -> Result<Vec<AiStreamDelta>, GatewayError> {
        if self.failed || self.done {
            return Err(eventstream::protocol_error());
        }
        let result = self.finish_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn finish_inner(&mut self) -> Result<Vec<AiStreamDelta>, GatewayError> {
        self.decoder.finish()?;
        let mut events = Vec::new();
        if let Some(mut splitter) = self.splitter.take() {
            let segments = splitter.finish();
            self.emit_segments(segments, &mut events);
        }
        let mut calls = Vec::new();
        for (index, tool) in self.tools.iter().enumerate() {
            if tool.id.is_empty() || tool.name.is_empty() {
                return Err(failure(502, "Kiro returned an incomplete tool call"));
            }
            let arguments = if tool.input.trim().is_empty() {
                "{}".to_string()
            } else {
                tool.input.clone()
            };
            if serde_json::from_str::<Value>(&arguments).is_err() {
                return Err(failure(
                    502,
                    "Kiro returned a tool call with invalid JSON arguments",
                ));
            }
            let name = self
                .options
                .tool_names
                .get(&tool.name)
                .cloned()
                .unwrap_or_else(|| tool.name.clone());
            calls.push((index, ToolCall::function(tool.id.clone(), name, arguments)));
        }
        if !self.started && self.post_tool_text.is_empty() && calls.is_empty() {
            return Err(failure(502, "Kiro returned an empty response"));
        }
        self.start(&mut events);
        for (index, call) in &calls {
            events.push(AiStreamDelta::ToolCallStart {
                index: *index,
                id: call.id.clone(),
                name: call.name.clone(),
                namespace: None,
                kind: ToolCallKind::Function,
            });
            events.push(AiStreamDelta::ToolCallDelta {
                index: *index,
                arguments: call.arguments.clone(),
            });
            events.push(AiStreamDelta::ToolCallComplete {
                index: *index,
                tool_call: call.clone(),
            });
        }
        if !self.post_tool_text.is_empty() {
            events.push(AiStreamDelta::TextDelta(std::mem::take(
                &mut self.post_tool_text,
            )));
        }
        events.push(AiStreamDelta::Usage(self.final_usage(&calls)));
        let reason = if !calls.is_empty() {
            "tool_calls"
        } else if self.thinking_seen && !self.visible_text_seen {
            // Thinking consumed the whole budget; the answer was cut off.
            "length"
        } else {
            "stop"
        };
        events.push(AiStreamDelta::Done {
            stop_reason: reason.into(),
        });
        self.done = true;
        Ok(events)
    }

    fn final_usage(&self, calls: &[(usize, ToolCall)]) -> Usage {
        let text = self.text_tokens.saturating_add(self.thinking_tokens);
        let tool: u32 = calls
            .iter()
            .map(|(_, call)| estimate_tokens(&call.name) + estimate_tokens(&call.arguments))
            .sum();
        let output = text.saturating_add(tool);
        let input = match self.context_percentage {
            Some(pct) if pct > 0.0 && self.options.context_tokens > 0 => {
                let total = (f64::from(self.options.context_tokens) * pct / 100.0).round() as u64;
                u32::try_from(total.saturating_sub(u64::from(output))).unwrap_or(u32::MAX)
            }
            _ => self.options.input_estimate,
        };
        Usage {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: input.saturating_add(output),
            reasoning_tokens: self.thinking_seen.then_some(self.thinking_tokens),
            ..Default::default()
        }
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
        self.buffered = self.buffered.saturating_add(size);
        if self.buffered > MAX_BUFFERED {
            return Err(failure(502, "Kiro response exceeded the buffering limit"));
        }
        Ok(())
    }

    fn frame(&mut self, frame: Frame, events: &mut Vec<AiStreamDelta>) -> Result<(), GatewayError> {
        match frame.message_type() {
            "exception" => return Err(exception(frame.exception_type(), &frame.payload)),
            "error" => {
                let code = frame
                    .headers
                    .get(":error-code")
                    .map(String::as_str)
                    .unwrap_or("");
                return Err(failure(502, &format!("Kiro stream error: {code}")));
            }
            _ => {}
        }
        let kind = frame.event_type();
        if !matches!(
            kind,
            "assistantResponseEvent"
                | "reasoningContentEvent"
                | "toolUseEvent"
                | "contextUsageEvent"
                | ""
        ) {
            // Unknown bookkeeping events remain forward-compatible. Known
            // events (including the headerless shape fallback) must carry a
            // valid object: CRC validation alone cannot prove semantic success.
            return Ok(());
        }
        let payload: Value =
            serde_json::from_slice(&frame.payload).map_err(|_| eventstream::protocol_error())?;
        if !payload.is_object() {
            return Err(eventstream::protocol_error());
        }
        match kind {
            "assistantResponseEvent" => self.content(&payload, events),
            "reasoningContentEvent" => self.reasoning(&payload, events),
            "toolUseEvent" => self.tool_event(&payload),
            "contextUsageEvent" => {
                self.context_usage(&payload);
                Ok(())
            }
            // Without an event-type header fall back to the payload shape.
            "" => {
                if payload.get("contextUsagePercentage").is_some() {
                    self.context_usage(&payload);
                    Ok(())
                } else if payload.get("content").is_some() {
                    self.content(&payload, events)
                } else if payload.get("text").is_some() {
                    self.reasoning(&payload, events)
                } else if payload.get("toolUseId").is_some()
                    || payload.get("input").is_some()
                    || payload.get("stop").is_some()
                {
                    self.tool_event(&payload)
                } else {
                    Ok(())
                }
            }
            // Metering, metadata, citations and other bookkeeping events.
            _ => Ok(()),
        }
    }

    fn context_usage(&mut self, payload: &Value) {
        if let Some(pct) = payload
            .get("contextUsagePercentage")
            .and_then(Value::as_f64)
            && pct.is_finite()
            && pct > 0.0
        {
            self.context_percentage = Some(pct);
        }
    }

    fn content(
        &mut self,
        payload: &Value,
        events: &mut Vec<AiStreamDelta>,
    ) -> Result<(), GatewayError> {
        if payload.get("followupPrompt").is_some() {
            return Ok(());
        }
        let Some(text) = payload.get("content").and_then(Value::as_str) else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }
        // Kiro occasionally replays a whole chunk; drop long exact repeats only.
        if text.len() >= MIN_DEDUP_BYTES && text == self.last_content {
            return Ok(());
        }
        self.last_content = text.to_string();
        if !self.tools.is_empty() {
            self.reserve(text.len())?;
            self.post_tool_text.push_str(text);
            self.visible_text_seen = true;
            self.text_tokens = self.text_tokens.saturating_add(estimate_tokens(text));
            return Ok(());
        }
        match self.splitter.as_mut() {
            Some(splitter) => {
                let segments = splitter.push(text);
                self.emit_segments(segments, events);
            }
            None => self.emit_text(text.to_string(), events),
        }
        Ok(())
    }

    fn reasoning(
        &mut self,
        payload: &Value,
        events: &mut Vec<AiStreamDelta>,
    ) -> Result<(), GatewayError> {
        let Some(text) = payload.get("text").and_then(Value::as_str) else {
            return Ok(());
        };
        if !text.is_empty() {
            self.emit_thinking(text.to_string(), events);
        }
        Ok(())
    }

    fn emit_segments(&mut self, segments: Vec<Segment>, events: &mut Vec<AiStreamDelta>) {
        for segment in segments {
            match segment {
                Segment::Text(text) => self.emit_text(text, events),
                Segment::Thinking(text) => self.emit_thinking(text, events),
            }
        }
    }

    fn emit_text(&mut self, text: String, events: &mut Vec<AiStreamDelta>) {
        if text.is_empty() {
            return;
        }
        self.start(events);
        self.visible_text_seen = true;
        self.text_tokens = self.text_tokens.saturating_add(estimate_tokens(&text));
        events.push(AiStreamDelta::TextDelta(text));
    }

    fn emit_thinking(&mut self, text: String, events: &mut Vec<AiStreamDelta>) {
        if text.is_empty() {
            return;
        }
        self.start(events);
        self.thinking_seen = true;
        self.thinking_tokens = self.thinking_tokens.saturating_add(estimate_tokens(&text));
        events.push(AiStreamDelta::ThinkingDelta(text));
    }

    fn tool_event(&mut self, payload: &Value) -> Result<(), GatewayError> {
        let id = payload
            .get("toolUseId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
        let input = match payload.get("input") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        };
        let stop = payload
            .get("stop")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !name.is_empty() && !id.is_empty() && name == PLACEHOLDER_TOOL {
            if !self.dropped_tool_ids.iter().any(|dropped| dropped == id) {
                self.dropped_tool_ids.push(id.to_string());
            }
            return Ok(());
        }
        if !id.is_empty() && self.dropped_tool_ids.iter().any(|dropped| dropped == id) {
            return Ok(());
        }

        let index = if !id.is_empty() && !name.is_empty() {
            match self.tool_by_id.get(id) {
                Some(index) => *index,
                None => {
                    if self.tools.len() >= MAX_TOOLS {
                        return Err(failure(502, "Kiro returned too many tool calls"));
                    }
                    self.tools.push(PendingTool {
                        id: id.to_string(),
                        name: name.to_string(),
                        input: String::new(),
                    });
                    let index = self.tools.len() - 1;
                    self.tool_by_id.insert(id.to_string(), index);
                    index
                }
            }
        } else if !id.is_empty() {
            match self.tool_by_id.get(id) {
                Some(index) => *index,
                None => return Ok(()),
            }
        } else if let Some(index) = self.open_tool {
            index
        } else {
            return Ok(());
        };
        self.reserve(input.len())?;
        self.tools[index].input.push_str(&input);
        self.open_tool = if stop { None } else { Some(index) };
        Ok(())
    }
}

/// Map an event-stream exception to the status a client should see.
fn exception(kind: &str, payload: &[u8]) -> GatewayError {
    let status = match kind {
        "throttlingException" | "ThrottlingException" => 429,
        "validationException" | "ValidationException" => 400,
        "accessDeniedException" | "AccessDeniedException" => 403,
        "serviceUnavailableException" | "ServiceUnavailableException" => 503,
        _ => 502,
    };
    let message = serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let message: String = message.chars().take(300).collect();
    failure(status, &format!("Kiro {kind}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::eventstream::{encode_event, encode_frame};
    use super::*;

    fn session(thinking: bool) -> Session {
        Session::new(
            "claude-sonnet-4.5",
            SessionOptions {
                thinking_tags: thinking,
                input_estimate: 7,
                context_tokens: 200_000,
                ..Default::default()
            },
        )
    }

    fn content(text: &str) -> Vec<u8> {
        encode_event(
            "assistantResponseEvent",
            &serde_json::json!({ "content": text }).to_string(),
        )
    }

    fn tool(json: serde_json::Value) -> Vec<u8> {
        encode_event("toolUseEvent", &json.to_string())
    }

    fn run(session: &mut Session, wire: &[u8]) -> Vec<AiStreamDelta> {
        let mut out = session.push(wire).unwrap();
        out.extend(session.finish().unwrap());
        out
    }

    fn text(deltas: &[AiStreamDelta]) -> String {
        deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn thinking(deltas: &[AiStreamDelta]) -> String {
        deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ThinkingDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn stop_reason(deltas: &[AiStreamDelta]) -> &str {
        match deltas.last() {
            Some(AiStreamDelta::Done { stop_reason }) => stop_reason,
            other => panic!("stream must end with Done, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_streams_with_start_usage_and_done_last() {
        let mut s = session(false);
        let mut wire = content("Hello, ");
        wire.extend(content("world"));
        let out = run(&mut s, &wire);
        assert!(
            matches!(out.first(), Some(AiStreamDelta::MessageStart { model, .. }) if model == "claude-sonnet-4.5")
        );
        assert_eq!(text(&out), "Hello, world");
        assert_eq!(stop_reason(&out), "stop");
        let usage = out.iter().find_map(|d| match d {
            AiStreamDelta::Usage(u) => Some(u),
            _ => None,
        });
        let usage = usage.expect("usage is always reported");
        assert_eq!(usage.prompt_tokens, 7, "falls back to the local estimate");
        assert!(usage.completion_tokens > 0);
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens
        );
        assert_eq!(
            s.usage_provenance(),
            crate::performance::NativeUsageProvenance::Unknown
        );
    }

    #[test]
    fn frames_split_across_arbitrary_chunk_boundaries_decode_identically() {
        let mut wire = content("héllo ");
        wire.extend(content("wörld"));
        let mut s = session(false);
        let mut out = Vec::new();
        for byte in &wire {
            out.extend(s.push(std::slice::from_ref(byte)).unwrap());
        }
        out.extend(s.finish().unwrap());
        assert_eq!(text(&out), "héllo wörld");
    }

    #[test]
    fn context_percentage_becomes_prompt_tokens_net_of_output() {
        let mut s = session(false);
        let mut wire = content("hi");
        wire.extend(encode_event(
            "contextUsageEvent",
            r#"{"contextUsagePercentage":10.0}"#,
        ));
        let out = run(&mut s, &wire);
        let usage = out
            .iter()
            .find_map(|d| match d {
                AiStreamDelta::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.prompt_tokens + usage.completion_tokens, 20_000);
        assert_eq!(
            s.usage_provenance(),
            crate::performance::NativeUsageProvenance::Partial
        );
    }

    #[test]
    fn tool_call_is_buffered_restored_and_marks_tool_calls() {
        let mut names = HashMap::new();
        names.insert(
            "short_abc".to_string(),
            "mcp__server__very_long_original".to_string(),
        );
        let mut s = Session::new(
            "m",
            SessionOptions {
                tool_names: names,
                ..Default::default()
            },
        );
        let mut wire = content("Let me check.");
        wire.extend(tool(
            serde_json::json!({"name":"short_abc","toolUseId":"tu_1","input":"{\"a\":"}),
        ));
        wire.extend(tool(serde_json::json!({"toolUseId":"tu_1","input":"1}"})));
        wire.extend(tool(serde_json::json!({"toolUseId":"tu_1","stop":true})));
        let out = run(&mut s, &wire);
        assert_eq!(text(&out), "Let me check.");
        assert_eq!(stop_reason(&out), "tool_calls");
        let starts: Vec<_> = out
            .iter()
            .filter_map(|d| match d {
                AiStreamDelta::ToolCallStart {
                    id, name, index, ..
                } => Some((id.as_str(), name.as_str(), *index)),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![("tu_1", "mcp__server__very_long_original", 0)]);
        let complete = out.iter().find_map(|d| match d {
            AiStreamDelta::ToolCallComplete { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        });
        let complete = complete.unwrap();
        assert_eq!(complete.arguments, r#"{"a":1}"#);
        // Start/Delta/Complete are contiguous and precede Usage and Done.
        let at = |wanted: fn(&AiStreamDelta) -> bool| out.iter().position(wanted).unwrap();
        let start = at(|d| matches!(d, AiStreamDelta::ToolCallStart { .. }));
        assert!(matches!(
            out[start + 1],
            AiStreamDelta::ToolCallDelta { .. }
        ));
        assert!(matches!(
            out[start + 2],
            AiStreamDelta::ToolCallComplete { .. }
        ));
        assert!(matches!(out[start + 3], AiStreamDelta::Usage(_)));
        assert!(matches!(out[start + 4], AiStreamDelta::Done { .. }));
    }

    #[test]
    fn two_interleaved_tools_keep_their_own_arguments() {
        let mut s = session(false);
        let mut wire = tool(serde_json::json!({"name":"a","toolUseId":"1","input":"{\"x\":"}));
        wire.extend(tool(
            serde_json::json!({"name":"b","toolUseId":"2","input":"{\"y\":2}","stop":true}),
        ));
        wire.extend(tool(
            serde_json::json!({"toolUseId":"1","input":"1}","stop":true}),
        ));
        let out = run(&mut s, &wire);
        let calls: Vec<_> = out
            .iter()
            .filter_map(|d| match d {
                AiStreamDelta::ToolCallComplete { tool_call, .. } => {
                    Some((tool_call.name.clone(), tool_call.arguments.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            vec![
                ("a".to_string(), r#"{"x":1}"#.to_string()),
                ("b".to_string(), r#"{"y":2}"#.to_string())
            ]
        );
    }

    #[test]
    fn empty_tool_input_defaults_to_an_empty_object() {
        let mut s = session(false);
        let wire = tool(serde_json::json!({"name":"noargs","toolUseId":"1","stop":true}));
        let out = run(&mut s, &wire);
        let call = out.iter().find_map(|d| match d {
            AiStreamDelta::ToolCallComplete { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        });
        assert_eq!(call.unwrap().arguments, "{}");
    }

    #[test]
    fn invalid_tool_json_fails_instead_of_reaching_the_client() {
        let mut s = session(false);
        s.push(&tool(
            serde_json::json!({"name":"a","toolUseId":"1","input":"{\"x\":"}),
        ))
        .unwrap();
        assert!(s.finish().is_err());
    }

    #[test]
    fn placeholder_tool_calls_are_dropped_with_their_continuations() {
        let mut s = session(false);
        let mut wire = content("answer");
        wire.extend(tool(
            serde_json::json!({"name":PLACEHOLDER_TOOL,"toolUseId":"p","input":"{}"}),
        ));
        wire.extend(tool(serde_json::json!({"toolUseId":"p","stop":true})));
        let out = run(&mut s, &wire);
        assert_eq!(text(&out), "answer");
        assert_eq!(stop_reason(&out), "stop");
        assert!(
            !out.iter()
                .any(|d| matches!(d, AiStreamDelta::ToolCallStart { .. }))
        );
    }

    #[test]
    fn text_after_a_tool_call_is_emitted_after_the_tool_group() {
        let mut s = session(false);
        let mut wire =
            tool(serde_json::json!({"name":"a","toolUseId":"1","input":"{}","stop":true}));
        wire.extend(content("late"));
        let out = run(&mut s, &wire);
        let complete = out
            .iter()
            .position(|d| matches!(d, AiStreamDelta::ToolCallComplete { .. }))
            .unwrap();
        let late = out
            .iter()
            .position(|d| matches!(d, AiStreamDelta::TextDelta(t) if t == "late"))
            .unwrap();
        assert!(complete < late);
    }

    #[test]
    fn long_exact_repeats_are_dropped_but_short_tokens_are_kept() {
        let mut s = session(false);
        let long = "0123456789abcdefXYZ";
        let mut wire = content(long);
        wire.extend(content(long));
        wire.extend(content("\n"));
        wire.extend(content("\n"));
        let out = run(&mut s, &wire);
        assert_eq!(text(&out), format!("{long}\n\n"));
    }

    #[test]
    fn followup_prompts_and_unknown_events_are_ignored() {
        let mut s = session(false);
        let mut wire = encode_event(
            "assistantResponseEvent",
            r#"{"content":"suggested","followupPrompt":{"content":"x"}}"#,
        );
        wire.extend(encode_event("meteringEvent", r#"{"usage":0.01}"#));
        wire.extend(content("real"));
        let out = run(&mut s, &wire);
        assert_eq!(text(&out), "real");
    }

    #[test]
    fn malformed_known_event_payloads_fail_after_partial_output_without_done() {
        for kind in [
            "assistantResponseEvent",
            "reasoningContentEvent",
            "toolUseEvent",
            "contextUsageEvent",
            "",
        ] {
            for payload in [r#"{"content":"truncated"#, "null", "[]", "42"] {
                let mut s = session(false);
                let partial = s.push(&content("partial answer")).unwrap();
                assert_eq!(text(&partial), "partial answer");
                assert!(
                    !partial
                        .iter()
                        .any(|d| matches!(d, AiStreamDelta::Done { .. }))
                );
                // encode_event supplies valid lengths and CRCs. Corruption is
                // semantic, not a framing failure; even fragmented input fails.
                let wire = encode_event(kind, payload);
                let split = wire.len() / 2;
                assert!(s.push(&wire[..split]).unwrap().is_empty());
                assert!(
                    matches!(
                        s.push(&wire[split..]),
                        Err(GatewayError::StreamParseError { .. })
                    ),
                    "{kind}: {payload}"
                );
                assert!(
                    s.finish().is_err(),
                    "EOF must not turn a malformed event into Done"
                );
                assert!(
                    s.push(&content("later")).is_err(),
                    "failed sessions stay failed"
                );
            }
        }
    }

    #[test]
    fn unknown_bookkeeping_payloads_do_not_require_a_known_json_schema() {
        let mut s = session(false);
        let mut wire = content("answer");
        wire.extend(encode_event("futureBookkeepingEvent", "not-json"));
        let out = run(&mut s, &wire);
        assert_eq!(text(&out), "answer");
        assert_eq!(stop_reason(&out), "stop");
    }

    #[test]
    fn empty_stream_is_an_error_not_a_success() {
        let mut s = session(false);
        s.push(&encode_event("meteringEvent", r#"{"usage":0.01}"#))
            .unwrap();
        let error = s.finish().unwrap_err();
        assert!(matches!(
            error,
            GatewayError::UpstreamStatus { status: 502, .. }
        ));
    }

    #[test]
    fn exceptions_map_to_client_visible_statuses() {
        for (kind, status) in [
            ("throttlingException", 429),
            ("validationException", 400),
            ("accessDeniedException", 403),
            ("serviceUnavailableException", 503),
            ("somethingElse", 502),
        ] {
            let mut s = session(false);
            let frame = encode_frame(
                &[(":message-type", "exception"), (":exception-type", kind)],
                br#"{"message":"boom"}"#,
            );
            match s.push(&frame).unwrap_err() {
                GatewayError::UpstreamStatus {
                    status: got, body, ..
                } => {
                    assert_eq!(got, status, "{kind}");
                    assert!(body.unwrap().contains("boom"));
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(s.push(&[]).is_err(), "a failed session stays failed");
        }
    }

    #[test]
    fn truncated_wire_is_a_protocol_error_at_eof() {
        let mut s = session(false);
        let wire = content("hi");
        s.push(&wire[..wire.len() - 3]).unwrap();
        assert!(s.finish().is_err());
    }

    #[test]
    fn native_reasoning_events_become_thinking_deltas() {
        let mut s = session(false);
        let mut wire = encode_event("reasoningContentEvent", r#"{"text":"hmm "}"#);
        wire.extend(encode_event("reasoningContentEvent", r#"{"text":"ok"}"#));
        wire.extend(content("answer"));
        let out = run(&mut s, &wire);
        assert_eq!(thinking(&out), "hmm ok");
        assert_eq!(text(&out), "answer");
        let usage = out.iter().find_map(|d| match d {
            AiStreamDelta::Usage(u) => Some(u.clone()),
            _ => None,
        });
        assert!(usage.unwrap().reasoning_tokens.is_some());
    }

    #[test]
    fn thinking_only_response_reports_length() {
        let mut s = session(false);
        let wire = encode_event("reasoningContentEvent", r#"{"text":"only thoughts"}"#);
        let out = run(&mut s, &wire);
        assert_eq!(stop_reason(&out), "length");
    }

    #[test]
    fn inline_thinking_tags_are_split_across_any_chunking() {
        let full = "<thinking>\nplan the answer\n</thinking>\n\nFinal answer.";
        for step in 1..=full.len() {
            let mut s = session(true);
            let mut wire = Vec::new();
            let mut i = 0;
            while i < full.len() {
                let mut j = (i + step).min(full.len());
                while !full.is_char_boundary(j) {
                    j += 1;
                }
                wire.extend(content(&full[i..j]));
                i = j;
            }
            let out = run(&mut s, &wire);
            assert_eq!(thinking(&out), "plan the answer\n", "step {step}");
            assert_eq!(text(&out), "Final answer.", "step {step}");
            assert_eq!(stop_reason(&out), "stop");
        }
    }

    #[test]
    fn leading_whitespace_before_thinking_creates_no_text_block() {
        let mut s = session(true);
        let mut wire = content("\n\n");
        wire.extend(content("<thinking>think</thinking>\n\nanswer"));
        let out = run(&mut s, &wire);
        assert_eq!(thinking(&out), "think");
        assert_eq!(text(&out), "answer");
    }

    #[test]
    fn quoted_or_unterminated_tags_stay_plain_text_or_thinking() {
        let mut s = session(true);
        let out = run(&mut s, &content("use the `<thinking>` tag to reason"));
        assert_eq!(text(&out), "use the `<thinking>` tag to reason");
        assert_eq!(thinking(&out), "");

        // A quoted closing tag inside thinking does not end the block.
        let mut s = session(true);
        let out = run(
            &mut s,
            &content("<thinking>mention `</thinking>` safely</thinking>\n\ndone"),
        );
        assert_eq!(thinking(&out), "mention `</thinking>` safely");
        assert_eq!(text(&out), "done");

        // Unterminated thinking is flushed as thinking at EOF.
        let mut s = session(true);
        let out = run(&mut s, &content("<thinking>cut off"));
        assert_eq!(thinking(&out), "cut off");
        assert_eq!(stop_reason(&out), "length");
    }

    #[test]
    fn tags_are_ignored_when_thinking_was_not_requested() {
        let mut s = session(false);
        let out = run(&mut s, &content("<thinking>x</thinking>\n\nhi"));
        assert_eq!(text(&out), "<thinking>x</thinking>\n\nhi");
        assert_eq!(thinking(&out), "");
    }

    #[test]
    fn splitter_unit_handles_a_partial_open_tag_at_the_chunk_tail() {
        let mut splitter = ThinkingSplitter::new();
        assert_eq!(
            splitter.push("see <thi"),
            vec![Segment::Text("see ".into())]
        );
        assert_eq!(
            splitter.push("nking>deep"),
            vec![Segment::Thinking("deep".into())]
        );
        assert_eq!(
            splitter.push("</thinking>\n\nbye"),
            vec![Segment::Text("bye".into())]
        );
        assert!(splitter.finish().is_empty());
    }

    #[test]
    fn estimate_weights_ascii_and_wide_text_differently() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("你好"), 2);
    }
}
