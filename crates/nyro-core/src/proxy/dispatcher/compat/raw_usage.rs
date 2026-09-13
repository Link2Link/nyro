//! Log-only raw upstream usage observation for the compat dispatcher.
//!
//! Compat conversion rewrites the upstream response into the client's wire
//! format before usage is decoded for logging. Several directions
//! (Anthropic→Gemini, Anthropic→Chat, Anthropic→Responses) drop the upstream
//! reasoning token counts during that rewrite, so the log would under-report
//! reasoning usage even though the upstream reported it.
//!
//! This bypass observes the ORIGINAL upstream bytes and feeds only
//! usage-bearing JSON payloads through the codecs' pure usage extractors
//! (selected by the conversion session's upstream wire protocol — never the
//! client protocol). Response content NEVER enters any stateful decoder, so
//! observer memory is bounded regardless of response shape:
//!
//! - conversion output, client wire, and hook billing behavior are untouched;
//! - best-effort: any parse failure degrades to the existing converted-wire
//!   usage and can never fail an otherwise-successful request;
//! - the pending-block buffer never exceeds `MAX_RAW_USAGE_BLOCK_BYTES`; an
//!   event larger than the cap is skipped (resync at the next block
//!   boundary) instead of being accumulated — including when it arrives as
//!   one huge chunk;
//! - blocks are cut at byte-level SSE delimiters, so UTF-8 sequences split
//!   across chunks are never decoded mid-character; resync paths keep a
//!   ≤3-byte tail so a delimiter straddling any boundary is not missed;
//! - logged-usage priority and the completion/reasoning pair rule live in
//!   [`reconcile_logged_usage`].

use bytes::Bytes;
use futures::Stream;
use nyro_ccswitch_compat::{ConversionSession, WireProtocol};
use serde_json::Value;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use crate::protocol::codec::anthropic::messages::stream::extract_anthropic_usage;
use crate::protocol::codec::google::gemini::stream::extract_gemini_usage;
use crate::protocol::codec::openai::compatible::stream::extract_usage as extract_chat_usage;
use crate::protocol::codec::openai::responses::parser::extract_responses_usage;
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1, ProtocolId,
};
use crate::protocol::ir::Usage;
use crate::protocol::ir::usage::ServerToolUsage;

use super::UpstreamByteStream;

/// Cap for the observer's pending incomplete SSE block. Completed blocks are
/// consumed immediately, so retained memory is proportional to the largest
/// single upstream event still under assembly — never the whole response.
/// Larger events are skipped (usage is best-effort) with a resync at the
/// next block boundary.
pub(super) const MAX_RAW_USAGE_BLOCK_BYTES: usize = 4 * 1024 * 1024;

/// Map the conversion session's actual egress wire protocol to the codec
/// whose usage extractor understands the ORIGINAL upstream payloads. Derived
/// from the session profile (built from the resolved egress endpoint), never
/// from the client-side protocol.
pub(super) fn upstream_wire_protocol(session: &ConversionSession) -> ProtocolId {
    match session.profile.upstream_protocol {
        WireProtocol::AnthropicMessages => ANTHROPIC_MESSAGES_2023_06_01,
        WireProtocol::OpenAiChat => OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        WireProtocol::OpenAiResponses => OPENAI_RESPONSES_V1,
        WireProtocol::GeminiNative => GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    }
}

/// What the observer managed to prove about the upstream's usage. `None`
/// until a TERMINAL usage snapshot was seen — a non-zero output count alone
/// is NOT proof of terminality (Gemini repeats `usageMetadata` on every
/// chunk, and early snapshots can already carry output counts that would go
/// stale if the terminal block is later dropped).
#[derive(Debug, Default, Clone)]
pub(super) struct RawUsageObservation {
    terminal: Option<Usage>,
}

impl RawUsageObservation {
    /// The completion/reasoning pair from the upstream's terminal usage
    /// snapshot, when one was observed.
    fn terminal_pair(&self) -> Option<(u32, Option<u32>)> {
        let usage = self.terminal.as_ref()?;
        (usage.completion_tokens > 0).then_some((usage.completion_tokens, usage.reasoning_tokens))
    }
}

struct RawUsageObserver {
    protocol: ProtocolId,
    terminal: Option<Usage>,
    /// Gemini streams may end with a usage-only tail chunk SEPARATE from the
    /// finishReason chunk; once finishReason was seen, later usage snapshots
    /// count as terminal too.
    gemini_finish_seen: bool,
    /// Pending incomplete SSE block. Always ≤ `MAX_RAW_USAGE_BLOCK_BYTES`,
    /// or ≤ 3 bytes while resyncing (delimiter straddle tail).
    buffer: Vec<u8>,
    /// Set after an oversized block was dropped: discard bytes up to the
    /// next block boundary before trusting block content again.
    resync: bool,
    finished: bool,
}

impl RawUsageObserver {
    fn new(protocol: ProtocolId) -> Self {
        Self {
            protocol,
            terminal: None,
            gemini_finish_seen: false,
            buffer: Vec::new(),
            resync: false,
            finished: false,
        }
    }

    fn into_observation(mut self) -> RawUsageObservation {
        self.finish();
        RawUsageObservation {
            terminal: self.terminal,
        }
    }

    /// Observe a whole buffered upstream body. A JSON body is one complete
    /// response — its usage snapshot is terminal by definition. Anything
    /// else (an upstream SSE stream the dispatcher buffered whole, e.g.
    /// forced-stream Responses upstreams behind a non-stream client) runs
    /// through the incremental block scan.
    fn observe_buffered_body(&mut self, body: &[u8]) {
        if self.finished {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(body) {
            if let Some(usage) = extract_raw_usage(self.protocol, &value) {
                self.terminal = Some(usage);
            }
            self.finished = true;
            return;
        }
        self.observe_bytes(body);
        self.finish();
    }

    /// Incremental, bounded scan. The incoming chunk is consumed in slices;
    /// the pending buffer never exceeds the cap, and oversized events are
    /// skipped without ever being accumulated.
    ///
    /// Complete events are extracted with a read cursor in ONE linear pass
    /// per batch (single `drain` at the end) so dense event streams stay
    /// O(bytes), not O(bytes²). Across chunk boundaries the cursor backs up
    /// ≤3 bytes so a delimiter straddling the boundary is still recognized.
    fn observe_bytes(&mut self, mut chunk: &[u8]) {
        if self.finished {
            return;
        }
        while !chunk.is_empty() {
            if self.resync {
                if skip_to_delimiter(&mut self.buffer, &mut chunk) {
                    self.resync = false;
                } else {
                    return; // no delimiter in this chunk; tiny tail kept
                }
                continue;
            }
            let room = MAX_RAW_USAGE_BLOCK_BYTES.saturating_sub(self.buffer.len());
            if room == 0 {
                // Pending block hit the cap with no delimiter: an oversized
                // single event. Drop it and resync at the next boundary,
                // keeping a delimiter-straddle tail of the dropped bytes.
                keep_straddle_tail(&mut self.buffer);
                self.resync = true;
                continue;
            }
            let tail_before = self.buffer.len();
            let take = chunk.len().min(room);
            self.buffer.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            // Resume the scan at the longest delimiter prefix that could
            // still complete in a LATER chunk: everything before this point
            // was already scanned by earlier batches.
            let mut scan = tail_before.saturating_sub(DELIMITER_MAX - 1);
            let mut consumed = 0_usize;
            while let Some((end, delimiter)) = find_sse_block_end(&self.buffer, scan) {
                let block = String::from_utf8_lossy(&self.buffer[consumed..end]).into_owned();
                self.observe_block(&block);
                consumed = end + delimiter;
                scan = consumed;
            }
            if consumed > 0 {
                self.buffer.drain(..consumed);
            }
        }
    }

    fn observe_block(&mut self, block: &str) {
        let mut event: Option<&str> = None;
        let mut data: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(name) = line.strip_prefix("event:") {
                let name = name.trim();
                if !name.is_empty() {
                    // SSE field semantics: the last `event:` line wins.
                    event = Some(name);
                }
            } else if let Some(payload) = line.strip_prefix("data:") {
                data.push(payload.trim());
            }
        }
        let Some(data) = (!data.is_empty()).then(|| data.join("\n")) else {
            return;
        };
        if data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        self.observe_payload(event, &value);
    }

    fn observe_payload(&mut self, event: Option<&str>, value: &Value) {
        match upstream_wire_kind(self.protocol) {
            UpstreamWireKind::GeminiNative => {
                // Gemini repeats usageMetadata on every chunk; terminal is
                // the chunk carrying finishReason, or any usage snapshot
                // AFTER such a chunk (usage-only tail chunks are common).
                let finish = value
                    .get("candidates")
                    .and_then(Value::as_array)
                    .is_some_and(|candidates| {
                        candidates
                            .iter()
                            .any(|candidate| candidate.get("finishReason").is_some())
                    });
                if finish {
                    self.gemini_finish_seen = true;
                }
                if (finish || self.gemini_finish_seen)
                    && let Some(usage) = extract_raw_usage(self.protocol, value)
                {
                    // Last terminal snapshot wins.
                    self.terminal = Some(usage);
                }
            }
            kind => {
                // The SSE `event:` line (when present) is the authoritative
                // event name — several upstreams send it with data payloads
                // that carry no `type` field (a path the core and ported
                // parsers support), so relying on the payload alone would
                // miss terminal events and their reasoning counts.
                let effective = effective_event_name(event, value);
                if let Some(usage) = extract_raw_usage(self.protocol, value)
                    && payload_is_terminal(kind, effective, value)
                {
                    self.terminal = Some(usage);
                }
            }
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Only flush a well-formed pending tail; a resync tail is garbage by
        // definition and must never be parsed as a block.
        if !self.resync && !self.buffer.is_empty() && self.buffer.len() <= MAX_RAW_USAGE_BLOCK_BYTES
        {
            let block = String::from_utf8_lossy(&self.buffer).into_owned();
            self.observe_block(&block);
        }
        self.buffer = Vec::new();
    }
}

/// Pure usage extraction per upstream wire, reusing the codecs' extractors
/// so the bypass and the protocol decoders share one accounting per wire.
fn extract_raw_usage(protocol: ProtocolId, value: &Value) -> Option<Usage> {
    match upstream_wire_kind(protocol) {
        UpstreamWireKind::AnthropicMessages => {
            // `message_delta` carries usage top-level; `message_start` nests
            // an early input-only snapshot under `message`.
            if value.get("usage").is_some() {
                return Some(extract_anthropic_usage(value));
            }
            value
                .get("message")
                .filter(|message| message.get("usage").is_some())
                .map(extract_anthropic_usage)
        }
        UpstreamWireKind::GeminiNative => {
            let has_usage = ["usageMetadata", "usage_metadata", "usage"]
                .iter()
                .any(|key| value.get(*key).is_some());
            has_usage.then(|| extract_gemini_usage(value))
        }
        UpstreamWireKind::OpenAiChat => {
            let has_usage = value.get("usage").is_some() || value.get("usageMetadata").is_some();
            has_usage.then(|| extract_chat_usage(value))
        }
        UpstreamWireKind::OpenAiResponses => {
            if let Some(response) = value.get("response") {
                return response
                    .get("usage")
                    .map(|usage| extract_responses_usage(Some(usage)));
            }
            value
                .get("usage")
                .map(|usage| extract_responses_usage(Some(usage)))
        }
    }
}

/// The authoritative event name: the SSE `event:` line when present, the
/// payload's `type` field otherwise.
fn effective_event_name<'a>(event: Option<&'a str>, value: &'a Value) -> &'a str {
    event.unwrap_or_else(|| value.get("type").and_then(Value::as_str).unwrap_or(""))
}

/// Whether THIS payload itself proves a terminal usage snapshot. Gemini is
/// handled separately in [`RawUsageObserver::observe_payload`] because its
/// finishReason and final usage may arrive in different chunks.
fn payload_is_terminal(kind: UpstreamWireKind, effective_event: &str, value: &Value) -> bool {
    match kind {
        UpstreamWireKind::AnthropicMessages => {
            // message_delta is the terminal usage event; message_start is
            // the early input-only snapshot.
            effective_event == "message_delta" && value.get("usage").is_some()
        }
        UpstreamWireKind::OpenAiChat => match value.get("choices").and_then(Value::as_array) {
            // The include_usage tail chunk has empty choices; a buffered
            // body has none. Both are final snapshots.
            None => true,
            Some(choices) if choices.is_empty() => true,
            Some(choices) => choices.iter().any(|choice| {
                choice
                    .get("finish_reason")
                    .is_some_and(|reason| !reason.is_null())
            }),
        },
        UpstreamWireKind::OpenAiResponses => {
            matches!(
                effective_event,
                "response.completed" | "response.incomplete",
            )
        }
        UpstreamWireKind::GeminiNative => {
            // Only reachable for buffered JSON bodies (terminal anyway);
            // never used for Gemini stream chunks.
            true
        }
    }
}

enum UpstreamWireKind {
    AnthropicMessages,
    GeminiNative,
    OpenAiChat,
    OpenAiResponses,
}

fn upstream_wire_kind(protocol: ProtocolId) -> UpstreamWireKind {
    match protocol {
        ANTHROPIC_MESSAGES_2023_06_01 => UpstreamWireKind::AnthropicMessages,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA => UpstreamWireKind::GeminiNative,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 => UpstreamWireKind::OpenAiChat,
        OPENAI_RESPONSES_V1 => UpstreamWireKind::OpenAiResponses,
        _ => UpstreamWireKind::OpenAiChat,
    }
}

/// Decide which usage the log entry carries.
///
/// Priority:
/// 1. Hook-adjusted usage wins: when a response hook actually changed the
///    usage decoded from the converted wire, its decision is authoritative
///    for billing and must not be clobbered by the raw observation.
/// 2. Otherwise the converted-wire usage is kept, and ONLY the
///    completion/reasoning pair is overridden — and only when the raw
///    observer saw a TERMINAL upstream usage snapshot carrying a credible
///    reasoning count paired with its output count. The pair moves together
///    so completion and reasoning always come from the same original
///    accounting; unrelated fields (prompt, cache, server tools) keep the
///    post-hook values. A raw snapshot WITHOUT reasoning never erases an
///    existing converted reasoning count (absence is not proof of zero), and
///    missing reasoning is never estimated.
pub(super) fn reconcile_logged_usage(
    observed: &RawUsageObservation,
    hook_adjusted: &Usage,
    pre_hook: &Usage,
) -> Usage {
    if !usage_equivalent(hook_adjusted, pre_hook) {
        return hook_adjusted.clone();
    }
    let mut logged = hook_adjusted.clone();
    if let Some((completion, Some(reasoning))) = observed.terminal_pair() {
        logged.completion_tokens = completion;
        logged.reasoning_tokens = Some(reasoning);
    }
    logged
}

fn usage_equivalent(a: &Usage, b: &Usage) -> bool {
    a.prompt_tokens == b.prompt_tokens
        && a.completion_tokens == b.completion_tokens
        && a.total_tokens == b.total_tokens
        && a.cache_read_tokens == b.cache_read_tokens
        && a.cache_creation_tokens == b.cache_creation_tokens
        && a.reasoning_tokens == b.reasoning_tokens
        && server_tool_use_equivalent(&a.server_tool_use, &b.server_tool_use)
}

fn server_tool_use_equivalent(a: &Option<ServerToolUsage>, b: &Option<ServerToolUsage>) -> bool {
    match (a, b) {
        (None, None) => true,
        (
            Some(ServerToolUsage {
                web_search_requests: a_search,
                web_fetch_requests: a_fetch,
            }),
            Some(ServerToolUsage {
                web_search_requests: b_search,
                web_fetch_requests: b_fetch,
            }),
        ) => a_search == b_search && a_fetch == b_fetch,
        _ => false,
    }
}

/// Shared handle so the stream tee and the logging task can reach the same
/// observer. Locks are only taken inside synchronous steps (never across an
/// await), and observation failures are swallowed so the bypass can never
/// fail a request.
#[derive(Clone)]
pub(super) struct RawUsageHandle(Arc<Mutex<RawUsageObserver>>);

impl RawUsageHandle {
    pub(super) fn new(protocol: ProtocolId) -> Self {
        Self(Arc::new(Mutex::new(RawUsageObserver::new(protocol))))
    }

    pub(super) fn observe(&self, bytes: &[u8]) {
        Self::lock(&self.0).observe_bytes(bytes);
    }

    pub(super) fn finish(&self) {
        Self::lock(&self.0).finish();
    }

    /// Finish the observation and return what could be proven about the
    /// upstream usage.
    pub(super) fn observation(&self) -> RawUsageObservation {
        let mut observer = Self::lock(&self.0);
        observer.finish();
        RawUsageObservation {
            terminal: observer.terminal.clone(),
        }
    }

    fn lock(mutex: &Mutex<RawUsageObserver>) -> MutexGuard<'_, RawUsageObserver> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Passthrough stream that feeds every ORIGINAL upstream chunk into the raw
/// usage observer on its way into the compat conversion. Byte order, chunk
/// boundaries, errors, and termination are forwarded verbatim; observation
/// is best-effort and invisible to the conversion.
pub(super) struct RawUsageTee {
    inner: UpstreamByteStream,
    usage: RawUsageHandle,
}

impl RawUsageTee {
    pub(super) fn new(inner: UpstreamByteStream, usage: RawUsageHandle) -> Self {
        Self { inner, usage }
    }
}

impl Stream for RawUsageTee {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.usage.observe(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.usage.finish();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.usage.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Observe a complete buffered upstream body.
pub(super) fn observe_buffered_usage(protocol: ProtocolId, body: &[u8]) -> RawUsageObservation {
    let mut observer = RawUsageObserver::new(protocol);
    observer.observe_buffered_body(body);
    observer.into_observation()
}

/// Longest SSE block delimiter (`\r\n\r\n`). Cursor backtracks never need
/// more than `DELIMITER_MAX - 1` bytes to re-detect a delimiter straddling
/// a chunk boundary — for LF (`\n\n`) and CRLF alike.
const DELIMITER_MAX: usize = 4;

/// Byte-level SSE block boundary: `(offset, delimiter_len)` of the earliest
/// `\n\n` or `\r\n\r\n` whose start is at or after `from`. Single linear
/// pass (both delimiter shapes checked at each position) so dense event
/// streams stay O(bytes). Neither delimiter byte can appear inside a UTF-8
/// multi-byte sequence, so blocks cut here are always whole characters.
fn find_sse_block_end(buffer: &[u8], from: usize) -> Option<(usize, usize)> {
    // `from` is a HARD lower bound: never clamp it backward. A backward
    // clamp could re-find an already-consumed delimiter and make the caller
    // loop forever or slice `buffer[consumed..end]` with end < consumed.
    // Chunk-boundary backtracking is the CALLER's job (it rewinds the scan
    // start only when new bytes arrived). `from >= len - 1` naturally
    // yields `None` because the remaining slice cannot hold a delimiter.
    let mut index = from;
    while index < buffer.len() {
        if buffer[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

/// Reduce `buffer` to its last ≤3 bytes (the longest delimiter is 4 bytes,
/// so a straddling delimiter can still be recognized across the boundary).
fn keep_straddle_tail(buffer: &mut Vec<u8>) {
    let keep = buffer.len().min(3);
    let drop = buffer.len() - keep;
    buffer.drain(..drop);
}

/// While resyncing, discard bytes up to and including the next SSE block
/// delimiter WITHOUT accumulating the skipped data. `buffer` may hold a ≤3
/// byte tail (from a dropped oversized block or a previous short chunk) so
/// a delimiter straddling the boundary is not missed (which would eat the
/// next well-formed event). Returns `true` once a delimiter was consumed
/// and the caller may resume block parsing.
fn skip_to_delimiter(buffer: &mut Vec<u8>, chunk: &mut &[u8]) -> bool {
    if !buffer.is_empty() {
        let head = chunk.len().min(3);
        let mut join = Vec::with_capacity(buffer.len() + head);
        join.extend_from_slice(buffer);
        join.extend_from_slice(&chunk[..head]);
        if let Some((end, delimiter)) = find_sse_block_end(&join, 0) {
            let consumed = (end + delimiter).saturating_sub(buffer.len());
            *chunk = &chunk[consumed.min(chunk.len())..];
            buffer.clear();
            return true;
        }
    }
    if let Some((end, delimiter)) = find_sse_block_end(chunk, 0) {
        *chunk = &chunk[end + delimiter..];
        buffer.clear();
        return true;
    }
    // No delimiter anywhere: keep the last ≤3 bytes of buffer+chunk so a
    // straddling delimiter in tiny follow-up chunks is still recognized.
    if chunk.len() >= 3 {
        buffer.clear();
        buffer.extend_from_slice(&chunk[chunk.len() - 3..]);
    } else {
        buffer.extend_from_slice(chunk);
        keep_straddle_tail(buffer);
    }
    *chunk = &[];
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streamed_observation(protocol: ProtocolId, chunks: &[&[u8]]) -> RawUsageObservation {
        let handle = RawUsageHandle::new(protocol);
        for chunk in chunks {
            handle.observe(chunk);
        }
        handle.observation()
    }

    #[test]
    fn stream_observer_extracts_gemini_terminal_reasoning_across_chunk_splits() {
        // Gemini repeats usageMetadata per chunk; only the finishReason
        // chunk (or a usage-only tail after it) is terminal. The stream is
        // split at arbitrary byte offsets, including inside a multi-byte
        // character.
        let first = r#"data: {"candidates":[{"content":{"parts":[{"text":"思"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5,"totalTokenCount":105}}"#;
        let terminal = r#"data: {"candidates":[{"content":{"parts":[{"text":"考"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"thoughtsTokenCount":60,"totalTokenCount":180},"modelVersion":"gemini-test"}"#;
        let mut stream = Vec::new();
        stream.extend_from_slice(first.as_bytes());
        stream.extend_from_slice(b"\n\n");
        stream.extend_from_slice(terminal.as_bytes());
        stream.extend_from_slice(b"\n\n");

        let split = first.len() + 2 + 40;
        let observed = streamed_observation(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            &[&stream[..split], &stream[split..]],
        );
        assert_eq!(
            observed.terminal_pair(),
            Some((80, Some(60))),
            "terminal pair must come from the finishReason chunk: {observed:?}"
        );
    }

    #[test]
    fn gemini_usage_only_tail_chunk_after_finish_is_terminal() {
        // finishReason and the final usageMetadata may arrive in different
        // chunks; the usage-only tail after finishReason must count as
        // terminal and win over the finishReason chunk's own snapshot.
        let finish = "data: {\"candidates\":[{\"content\":{},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"totalTokenCount\":110}}\n\n";
        let tail = "data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":20,\"thoughtsTokenCount\":60,\"totalTokenCount\":180}}\n\n";
        let observed = streamed_observation(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            &[finish.as_bytes(), tail.as_bytes()],
        );
        assert_eq!(observed.terminal_pair(), Some((80, Some(60))));
    }

    #[test]
    fn non_terminal_usage_snapshots_are_not_treated_as_terminal() {
        // Mid-stream Gemini chunk with output counts but no finishReason
        // (and no finishReason seen before it): NOT terminal — its counts
        // could be stale if the real terminal block is later dropped.
        let early = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":50,\"thoughtsTokenCount\":10,\"totalTokenCount\":160}}\n\n";
        let observed =
            streamed_observation(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, &[early.as_bytes()]);
        assert_eq!(observed.terminal_pair(), None);
    }

    #[test]
    fn anthropic_message_start_is_early_message_delta_is_terminal() {
        let start = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1}}}\n\n",
        );
        let delta = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":32}}}\n\n",
        );
        let observed = streamed_observation(
            ANTHROPIC_MESSAGES_2023_06_01,
            &[start.as_bytes(), delta.as_bytes()],
        );
        assert_eq!(observed.terminal_pair(), Some((80, Some(32))));
    }

    #[test]
    fn chat_include_usage_tail_chunk_is_terminal() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":40}}}\n\n",
        );
        let observed =
            streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(40))));
    }

    #[test]
    fn chat_usage_without_terminal_marker_is_not_terminal() {
        // Usage on a chunk that is still streaming (no finish_reason,
        // non-empty choices) must not be treated as terminal.
        let sse = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n";
        let observed =
            streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), None);
    }

    #[test]
    fn responses_completed_event_is_terminal() {
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":64}}}}\n\n",
        );
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(64))));
    }

    #[test]
    fn responses_in_progress_usage_is_not_terminal() {
        let sse = concat!(
            "event: response.in_progress\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":9}}}}\n\n",
        );
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), None);
    }

    #[test]
    fn sse_comments_do_not_split_data_payload() {
        // Two `data:` lines of one event, with comment lines interleaved,
        // must join into a single JSON payload.
        let block = concat!(
            ": keep-alive comment\n",
            "data: {\"id\":\"c1\",\n",
            ": interleaved comment\n",
            "data: \"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":6}}}\n\n",
        );
        let observed =
            streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[block.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(6))));
    }

    #[test]
    fn buffered_json_body_is_terminal_by_definition() {
        let body = br#"{"id":"chatcmpl_1","model":"m","choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":80,"completion_tokens_details":{"reasoning_tokens":44}}}"#;
        let observed = observe_buffered_usage(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, body);
        assert_eq!(observed.terminal_pair(), Some((80, Some(44))));
    }

    #[test]
    fn buffered_gemini_json_body_is_terminal() {
        let body = br#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"thoughtsTokenCount":60,"totalTokenCount":180}}"#;
        let observed = observe_buffered_usage(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, body);
        assert_eq!(observed.terminal_pair(), Some((80, Some(60))));
    }

    #[test]
    fn buffered_sse_body_falls_back_to_stream_scan() {
        // Forced-stream Responses upstream buffered behind a non-stream
        // client: body is SSE, not JSON.
        let body = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_9\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":70}}}}\n\n",
        );
        let observed = observe_buffered_usage(OPENAI_RESPONSES_V1, body.as_bytes());
        assert_eq!(observed.terminal_pair(), Some((80, Some(70))));
    }

    #[test]
    fn oversized_single_event_chunk_is_skipped_not_accumulated() {
        // One huge chunk: an oversized event followed by a normal terminal
        // event in the SAME chunk. The oversized event must be skipped (no
        // accumulation, no panic) and the terminal event still observed.
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"data: {\"payload\":\"");
        chunk.extend(std::iter::repeat_n(b'A', MAX_RAW_USAGE_BLOCK_BYTES + 1024));
        chunk.extend_from_slice(b"\"}\n\n");
        let terminal = "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":12}}}\n\n";
        chunk.extend_from_slice(terminal.as_bytes());
        let observed = streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[&chunk]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(12))));
    }

    #[test]
    fn oversized_event_split_across_chunks_resyncs_at_delimiter() {
        // The oversized event straddles two chunks; the delimiter after it
        // also straddles the cut. Resync must eat exactly up to that
        // delimiter (using the ≤3-byte straddle tail) and keep the FOLLOWING
        // terminal event intact.
        let mut oversized = Vec::new();
        oversized.extend_from_slice(b"data: {\"pad\":\"");
        oversized.extend(std::iter::repeat_n(b'B', MAX_RAW_USAGE_BLOCK_BYTES + 4096));
        oversized.extend_from_slice(b"\"}\n\n");
        let terminal = "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\n";

        let cut = oversized.len() - 1; // delimiter tail straddles the cut
        let mut first = oversized.clone();
        first.truncate(cut);
        let mut second = oversized[cut..].to_vec();
        second.extend_from_slice(terminal.as_bytes());

        let observed =
            streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[&first, &second]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(7))));
    }

    #[test]
    fn garbage_stream_does_not_parse_tail_at_eof() {
        // Pure garbage without any delimiter: EOF must not parse the tail as
        // a block, and nothing must be reported.
        let garbage = vec![0xFF_u8; 8192];
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[&garbage]);
        assert_eq!(observed.terminal_pair(), None);
    }

    #[test]
    fn reconcile_prefers_hook_adjusted_usage() {
        let observed = RawUsageObservation {
            terminal: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 80,
                reasoning_tokens: Some(60),
                ..Usage::default()
            }),
        };
        let pre_hook = Usage {
            prompt_tokens: 100,
            completion_tokens: 80,
            ..Usage::default()
        };
        let hooked = Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            ..Usage::default()
        };
        let logged = reconcile_logged_usage(&observed, &hooked, &pre_hook);
        assert_eq!(logged.completion_tokens, 20);
        assert_eq!(logged.reasoning_tokens, None);
    }

    #[test]
    fn reconcile_overrides_only_the_terminal_reasoning_pair() {
        let observed = RawUsageObservation {
            terminal: Some(Usage {
                prompt_tokens: 999,
                completion_tokens: 80,
                reasoning_tokens: Some(60),
                ..Usage::default()
            }),
        };
        let converted = Usage {
            prompt_tokens: 100,
            completion_tokens: 80,
            cache_read_tokens: Some(30),
            ..Usage::default()
        };
        let logged = reconcile_logged_usage(&observed, &converted, &converted);
        // The pair comes from the raw terminal snapshot ...
        assert_eq!(logged.completion_tokens, 80);
        assert_eq!(logged.reasoning_tokens, Some(60));
        // ... while unrelated fields keep the converted values (raw prompt
        // 999 must NOT leak into the logged prompt).
        assert_eq!(logged.prompt_tokens, 100);
        assert_eq!(logged.cache_read_tokens, Some(30));
    }

    #[test]
    fn reconcile_without_terminal_proof_keeps_converted_usage() {
        let observed = RawUsageObservation::default();
        let converted = Usage {
            prompt_tokens: 100,
            completion_tokens: 80,
            ..Usage::default()
        };
        let logged = reconcile_logged_usage(&observed, &converted, &converted);
        assert_eq!(logged.completion_tokens, 80);
        assert_eq!(logged.reasoning_tokens, None);
    }

    #[test]
    fn reconcile_without_raw_reasoning_keeps_converted_reasoning() {
        // A raw terminal snapshot lacking reasoning is a PRESENCE LOSS, not
        // proof of zero: it must not erase a converted reasoning count nor
        // estimate one.
        let observed = RawUsageObservation {
            terminal: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 80,
                reasoning_tokens: None,
                ..Usage::default()
            }),
        };
        let converted = Usage {
            prompt_tokens: 100,
            completion_tokens: 80,
            reasoning_tokens: Some(25),
            ..Usage::default()
        };
        let logged = reconcile_logged_usage(&observed, &converted, &converted);
        assert_eq!(logged.completion_tokens, 80);
        assert_eq!(logged.reasoning_tokens, Some(25));
    }

    #[test]
    fn responses_event_line_without_payload_type_is_terminal() {
        // `event: response.completed` with a data payload carrying no
        // `type` field — a wire shape the core/ported parsers support. The
        // bypass must honor the event name.
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_e1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":55}}}}\n\n",
        );
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(55))));
    }

    #[test]
    fn responses_event_line_wins_over_conflicting_payload_type() {
        // Event name present → authoritative, even when the payload `type`
        // disagrees (both directions).
        let completed_by_event = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":33}}}}\n\n",
        );
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[completed_by_event.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(33))));

        let in_progress_by_event = concat!(
            "event: response.in_progress\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":33}}}}\n\n",
        );
        let observed =
            streamed_observation(OPENAI_RESPONSES_V1, &[in_progress_by_event.as_bytes()]);
        assert_eq!(observed.terminal_pair(), None);
    }

    #[test]
    fn anthropic_event_line_message_delta_without_type_is_terminal() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":21}}}\n\n",
        );
        let observed = streamed_observation(ANTHROPIC_MESSAGES_2023_06_01, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(21))));
    }

    #[test]
    fn crlf_blocks_with_event_names_are_parsed() {
        let sse = concat!(
            "event: response.completed\r\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":44}}}}\r\n",
            "\r\n",
        );
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[sse.as_bytes()]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(44))));
    }

    #[test]
    fn lf_delimiter_prefix_straddling_chunk_boundary_is_recognized() {
        // chunk1 ends with a lone `\n` — the FIRST half of a `\n\n`
        // delimiter; chunk2 completes the delimiter and then carries the
        // terminal event. The cursor backtrack must recover the boundary
        // for LF delimiters.
        let chunk1 = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
        let terminal = "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":9}}}\n\n";
        let mut chunk2 = b"\n".to_vec(); // completes the straddled \n\n
        chunk2.extend_from_slice(terminal.as_bytes());
        let observed = streamed_observation(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &[chunk1.as_bytes(), &chunk2],
        );
        assert_eq!(observed.terminal_pair(), Some((80, Some(9))));
    }

    #[test]
    fn crlf_delimiter_split_across_chunk_boundary_is_recognized() {
        // A CRLF block is `line\r\nline\r\n\r\n`; here chunk1 ends with
        // the final line's `\r\n` (first half of the blank-line delimiter)
        // and chunk2 begins with the completing `\r\n`.
        let chunk1 = concat!(
            "event: response.in_progress\r\n",
            "data: {\"response\":{\"id\":\"resp_crlf\"}}\r\n",
        );
        let mut chunk2 = b"\r\n".to_vec(); // completes the delimiter
        chunk2.extend_from_slice(
            b"event: response.completed\r\ndata: {\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":13}}}}\r\n\r\n",
        );
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &[chunk1.as_bytes(), &chunk2]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(13))));
    }

    #[test]
    fn single_byte_fragments_reassemble_crlf_events() {
        // Byte-by-byte fragmentation of a CRLF stream with an event-name
        // terminal block: the ≤3-byte cursor backtrack must reassemble both
        // the 4-byte CRLF delimiters and the payloads.
        let stream = concat!(
            "data: {\"response\":{\"id\":\"r1\"}}\r\n\r\n",
            "event: response.completed\r\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":14}}}}\r\n\r\n",
        );
        let chunks: Vec<Vec<u8>> = stream.bytes().map(|byte| vec![byte]).collect();
        let refs: Vec<&[u8]> = chunks.iter().map(|chunk| chunk.as_slice()).collect();
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &refs);
        assert_eq!(observed.terminal_pair(), Some((80, Some(14))));
    }

    #[test]
    fn single_byte_fragments_reassemble_events() {
        // A whole stream fed one byte at a time: every delimiter, event
        // name, and JSON payload must reassemble (structural check that
        // cursor backtracks lose nothing).
        let mut stream = Vec::new();
        stream.extend_from_slice(
            b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        );
        stream.extend_from_slice(
            b"event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":80,\"output_tokens_details\":{\"reasoning_tokens\":17}}}}\n\n",
        );
        let chunks: Vec<Vec<u8>> = stream.iter().map(|byte| vec![*byte]).collect();
        let refs: Vec<&[u8]> = chunks.iter().map(|chunk| chunk.as_slice()).collect();
        let observed = streamed_observation(OPENAI_RESPONSES_V1, &refs);
        assert_eq!(observed.terminal_pair(), Some((80, Some(17))));
    }

    #[test]
    fn single_complete_event_ending_exactly_at_delimiter() {
        // The delimiter sits at the very END of the buffer with no trailing
        // bytes: after consuming it, the scan cursor equals the buffer
        // length. The scan must return None (no backward clamp, no panic,
        // no infinite loop).
        let terminal = "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":8}}}\n\n";
        let observed = streamed_observation(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &[terminal.as_bytes()],
        );
        assert_eq!(observed.terminal_pair(), Some((80, Some(8))));
    }

    #[test]
    fn trailing_complete_events_across_separate_chunks_each_parse_once() {
        // Several complete events per chunk, each chunk ending exactly on a
        // delimiter boundary: every event must parse exactly once and the
        // terminal snapshot must survive (guards against the consumed-
        // cursor regressing below already-consumed bytes).
        let first = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n",
        );
        let second = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"c\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n",
        );
        let observed = streamed_observation(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &[first.as_bytes(), second.as_bytes()],
        );
        assert_eq!(observed.terminal_pair(), Some((80, Some(5))));
    }

    #[test]
    fn dense_single_chunk_events_all_processed() {
        // 31k events × ~120B ≈ 3.7 MiB of small events in ONE chunk, then
        // the terminal event. The cursor scan must process every event with
        // one compaction per batch (structural regression for the old
        // per-event drain).
        let event = format!(
            "data: {{\"index\":{:05},\"content\":\"{}\"}}\n\n",
            12345,
            "a".repeat(90),
        );
        assert!(event.len() >= 110 && event.len() <= 130, "~120B event");
        let filler = event.repeat(31_000);
        assert!(
            filler.len() > 3 * 1024 * 1024,
            "dense batch must be MiB-scale"
        );
        let terminal = "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":3}}}\n\n";
        let mut chunk = filler.into_bytes();
        chunk.extend_from_slice(terminal.as_bytes());
        let observed = streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[&chunk]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(3))));
    }

    #[test]
    fn dense_chunk_larger_than_cap_still_processes_events() {
        // A single chunk LARGER than the block cap (small events, so no
        // single event is oversized): the batch slicing must keep parsing
        // events across cap-sized batches without triggering resync.
        let event = format!(
            "data: {{\"index\":{:05},\"content\":\"{}\"}}\n\n",
            99999,
            "b".repeat(90),
        );
        let filler = event.repeat(40_000); // ~4.8 MiB > 4 MiB cap
        assert!(filler.len() > MAX_RAW_USAGE_BLOCK_BYTES);
        let terminal = "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":80,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n";
        let mut chunk = filler.into_bytes();
        chunk.extend_from_slice(terminal.as_bytes());
        let observed = streamed_observation(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &[&chunk]);
        assert_eq!(observed.terminal_pair(), Some((80, Some(2))));
    }
}
