//! Gemini Native streaming conversion module.
//!
//! Converts Gemini `streamGenerateContent?alt=sse` chunks into Anthropic-style
//! SSE events for Claude-compatible clients.
//!
//! Intentional safety divergence from cc-switch eb69e492: ordinary Gemini SSE
//! functionCall parts are complete calls, not cumulative array slots. Missing
//! IDs never establish replay identity (even for identical/growing arguments).
//! A repeated explicit ID may replay identical name/args and supply a missing
//! thoughtSignature; conflicting payloads/signatures and partial-args dialects
//! fail the stream without committing shadow state. Positional ID upgrades are
//! not inferred. See tests/gemini_stream_tools.rs and the three explicitly
//! divergent parity inventory mappings. Text accumulation remains unchanged.

use super::gemini_shadow::{GeminiShadowStore, GeminiToolCallMeta};
use super::transform_gemini::{
    AnthropicToolSchemaHints, build_anthropic_usage, rectify_tool_call_args,
    synthesize_tool_call_id,
};
use crate::ported::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

fn map_finish_reason(reason: Option<&str>, has_tool_use: bool, blocked: bool) -> &'static str {
    if blocked {
        return "refusal";
    }

    match reason {
        Some("MAX_TOKENS") => "max_tokens",
        Some("SAFETY")
        | Some("RECITATION")
        | Some("SPII")
        | Some("BLOCKLIST")
        | Some("PROHIBITED_CONTENT") => "refusal",
        _ if has_tool_use => "tool_use",
        _ => "end_turn",
    }
}

fn extract_visible_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|part| part.get("thought").and_then(|value| value.as_bool()) != Some(true))
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .collect::<String>()
}

fn extract_tool_calls(parts: &[Value]) -> Result<Vec<GeminiToolCallMeta>, std::io::Error> {
    parts
        .iter()
        .filter_map(|part| part.get("functionCall").map(|call| (part, call)))
        .map(|(part, function_call)| {
            // These fields signal a different, partial-argument dialect. Do
            // not turn an unfinished call (including a final fragment) into
            // an executable empty/partial argument object.
            if [
                "partialArgs",
                "willContinue",
                "partial_args",
                "will_continue",
            ]
            .iter()
            .any(|key| function_call.get(key).is_some())
            {
                return Err(std::io::Error::other(
                    "unsupported partial Gemini functionCall; expected a complete name/args object",
                ));
            }
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| std::io::Error::other("invalid Gemini functionCall.name"))?;
            let args = function_call
                .get("args")
                .filter(|args| args.is_object())
                .ok_or_else(|| {
                    std::io::Error::other(
                        "invalid Gemini functionCall.args; expected a complete object",
                    )
                })?;
            // Empty IDs are absent IDs, never a shared identity. Keep absent
            // IDs absent until stream completion, so a generated client ID
            // cannot be mistaken for evidence supplied by Gemini.
            let id = function_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(ToString::to_string);
            Ok(GeminiToolCallMeta::new(
                id,
                name,
                args.clone(),
                part.get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str),
            ))
        })
        .collect()
}

fn extract_text_thought_signature(parts: &[Value]) -> Option<String> {
    parts
        .iter()
        .filter(|part| part.get("text").is_some() && part.get("functionCall").is_none())
        .filter_map(|part| {
            part.get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .and_then(|value| value.as_str())
        })
        .next_back()
        .map(ToString::to_string)
}

fn merge_tool_call_snapshots(
    tool_call_snapshots: &mut Vec<GeminiToolCallMeta>,
    incoming: Vec<GeminiToolCallMeta>,
) -> Result<(), std::io::Error> {
    // SSE event-local position, responseId, name and even identical args do
    // not identify a call. Only a nonempty upstream ID permits replay dedup.
    // Keep raw args until replay validation completes: schema rectification
    // must not hide conflicting upstream payloads under the same ID.
    for mut tool_call in incoming {
        if tool_call.id.as_deref() == Some("") {
            tool_call.id = None;
        }
        let existing = tool_call.id.as_deref().and_then(|incoming_id| {
            tool_call_snapshots
                .iter_mut()
                .find(|existing| existing.id.as_deref() == Some(incoming_id))
        });
        if let Some(existing) = existing {
            if existing.name != tool_call.name || existing.args != tool_call.args {
                return Err(std::io::Error::other(
                    "conflicting complete Gemini functionCall for repeated explicit id; argument continuation is unsupported",
                ));
            }
            match (&existing.thought_signature, &tool_call.thought_signature) {
                (Some(prior), Some(incoming)) if prior != incoming => {
                    return Err(std::io::Error::other(
                        "conflicting Gemini functionCall thoughtSignature for repeated explicit id",
                    ));
                }
                (None, Some(_)) => existing.thought_signature = tool_call.thought_signature,
                _ => {}
            }
        } else {
            tool_call_snapshots.push(tool_call);
        }
    }
    Ok(())
}

fn build_shadow_assistant_parts(
    text: Option<&str>,
    text_thought_signature: Option<&str>,
    tool_calls: &[GeminiToolCallMeta],
) -> Vec<Value> {
    let mut parts = Vec::new();

    if text.filter(|text| !text.is_empty()).is_some() || text_thought_signature.is_some() {
        let mut part = json!({
            "text": text.unwrap_or("")
        });
        if let Some(signature) = text_thought_signature {
            part["thoughtSignature"] = json!(signature);
        }
        parts.push(part);
    }

    for tool_call in tool_calls {
        let mut part = json!({
            "functionCall": {
                "id": tool_call.id.clone().unwrap_or_default(),
                "name": tool_call.name,
                "args": tool_call.args
            }
        });

        if let Some(signature) = &tool_call.thought_signature {
            part["thoughtSignature"] = json!(signature);
        }

        parts.push(part);
    }

    parts
}

fn encode_sse(event_name: &str, payload: &Value) -> Bytes {
    Bytes::from(format!(
        "event: {event_name}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_default()
    ))
}

pub fn create_anthropic_sse_stream_from_gemini<E: std::error::Error + Send + 'static>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
    shadow_store: Option<Arc<GeminiShadowStore>>,
    provider_id: Option<String>,
    session_id: Option<String>,
    tool_schema_hints: Option<AnthropicToolSchemaHints>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder = Vec::new();
        let mut message_id: Option<String> = None;
        let mut current_model: Option<String> = None;
        let mut has_sent_message_start = false;
        let mut accumulated_text = String::new();
        let mut text_block_index: Option<u32> = None;
        let mut next_content_index: u32 = 0;
        let mut open_indices: HashSet<u32> = HashSet::new();
        let mut tool_call_snapshots: Vec<GeminiToolCallMeta> = Vec::new();
        let mut text_thought_signature: Option<String> = None;
        let mut latest_usage: Option<Value> = None;
        let mut latest_finish_reason: Option<String> = None;
        let mut blocked_text: Option<String> = None;
        // Termination-evidence bookkeeping: Gemini's wire contract always ends
        // a stream with a finishReason chunk, a promptFeedback block, or (from
        // OpenAI-style relays) the [DONE] sentinel. An EOF with none of these
        // is a truncated upstream and must be surfaced as an error.
        let mut saw_done = false;
        let mut parsed_chunks: usize = 0;
        // Flush sentinel: chain one synthetic blank-line delimiter after the
        // upstream EOF so a final SSE block that arrived without its trailing
        // "\n\n" is still parsed exactly once — mirroring the native Gemini
        // parser's finish() path. On a clean stream the buffer is empty (or
        // the sentinel yields an empty block) and this is a no-op; on a
        // genuinely truncated wire the flushed block fails the JSON parse and
        // surfaces the truncation error.
        let flushed = stream.chain(futures::stream::once(async {
            Ok::<Bytes, E>(Bytes::from_static(b"\n\n"))
        }));
        tokio::pin!(flushed);

        while let Some(chunk) = flushed.next().await {
            match chunk {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);

                    while let Some(block) = take_sse_block(&mut buffer) {
                        if block.trim().is_empty() {
                            continue;
                        }

                        let mut data_lines: Vec<String> = Vec::new();
                        for line in block.lines() {
                            if let Some(data) = strip_sse_field(line, "data") {
                                data_lines.push(data.to_string());
                            }
                        }

                        if data_lines.is_empty() {
                            continue;
                        }

                        let data = data_lines.join("\n");
                        if data.trim() == "[DONE]" {
                            // OpenAI-style relays fronting Gemini may terminate
                            // with only the sentinel and no finishReason chunk;
                            // the sentinel itself is termination evidence.
                            // Trade-off: `break` only leaves the block loop, so
                            // malformed data bytes AFTER the sentinel still
                            // fail the JSON parse below (and the flush sentinel
                            // reaches them) — deliberate strictness; garbage
                            // after a terminal is treated as wire corruption.
                            saw_done = true;
                            break;
                        }
                        // Empty/whitespace-only data frames are heartbeat
                        // traffic (some relays emit `data:` with no payload;
                        // strip_sse_field yields Some("")). The native Gemini
                        // parser treats them as no-ops — skip them instead of
                        // failing the JSON parse below and killing a healthy
                        // stream.
                        if data.trim().is_empty() {
                            continue;
                        }

                        let chunk_json: Value = match serde_json::from_str(&data) {
                            Ok(value) => {
                                parsed_chunks += 1;
                                value
                            }
                            Err(error) => {
                                // A syntactically broken data line means the
                                // wire is corrupt. Surface the failure instead
                                // of silently dropping the chunk and later
                                // synthesizing a normal completion over the
                                // missing content (and committing the partial
                                // turn to the shared shadow session).
                                // Keep upstream bytes OUT of the message:
                                // logging's sanitize_cause replaces a whole
                                // cause with "Sensitive diagnostic text
                                // omitted" when it merely sees keywords like
                                // "token" inside quoted content. The serde
                                // error already pinpoints line/column without
                                // embedding the payload.
                                yield Err(std::io::Error::other(format!(
                                    "gemini sse data line is not valid JSON: {error}; data_bytes={}, parsed_chunks={parsed_chunks}",
                                    data.len()
                                )));
                                return;
                            }
                        };

                        if message_id.is_none() {
                            message_id = chunk_json
                                .get("responseId")
                                .and_then(|value| value.as_str())
                                .map(ToString::to_string);
                        }
                        if current_model.is_none() {
                            current_model = chunk_json
                                .get("modelVersion")
                                .and_then(|value| value.as_str())
                                .map(ToString::to_string);
                        }
                        if latest_usage.is_none() {
                            latest_usage = chunk_json.get("usageMetadata").cloned();
                        }

                        if !has_sent_message_start {
                            let event = json!({
                                "type": "message_start",
                                "message": {
                                    "id": message_id.clone().unwrap_or_default(),
                                    "type": "message",
                                    "role": "assistant",
                                    "model": current_model.clone().unwrap_or_default(),
                                    "usage": build_anthropic_usage(chunk_json.get("usageMetadata"))
                                }
                            });
                            yield Ok(encode_sse("message_start", &event));
                            has_sent_message_start = true;
                        }

                        if let Some(reason) = chunk_json
                            .get("promptFeedback")
                            .and_then(|value| value.get("blockReason"))
                            .and_then(|value| value.as_str())
                        {
                            blocked_text = Some(format!("Request blocked by Gemini safety filters: {reason}"));
                        }

                        if let Some(candidate) = chunk_json
                            .get("candidates")
                            .and_then(|value| value.as_array())
                            .and_then(|value| value.first())
                        {
                            if let Some(reason) = candidate.get("finishReason").and_then(|value| value.as_str()) {
                                latest_finish_reason = Some(reason.to_string());
                            }
                            if let Some(usage) = chunk_json.get("usageMetadata") {
                                latest_usage = Some(usage.clone());
                            }
                            if let Some(parts) = candidate
                                .get("content")
                                .and_then(|value| value.get("parts"))
                                .and_then(|value| value.as_array())
                            {
                                let result = extract_tool_calls(parts).and_then(|incoming| {
                                    merge_tool_call_snapshots(&mut tool_call_snapshots, incoming)
                                });
                                if let Err(error) = result {
                                    yield Err(error);
                                    return;
                                }
                                if let Some(signature) = extract_text_thought_signature(parts) {
                                    text_thought_signature = Some(signature);
                                }
                                let visible_text = extract_visible_text(parts);
                                if !visible_text.is_empty() {
                                    let is_cumulative = visible_text.starts_with(&accumulated_text);
                                    let delta = if is_cumulative {
                                        visible_text[accumulated_text.len()..].to_string()
                                    } else {
                                        visible_text.clone()
                                    };

                                    if !delta.is_empty() {
                                        let index = *text_block_index.get_or_insert_with(|| {
                                            let assigned = next_content_index;
                                            next_content_index += 1;
                                            assigned
                                        });

                                        if !open_indices.contains(&index) {
                                            let start_event = json!({
                                                "type": "content_block_start",
                                                "index": index,
                                                "content_block": {
                                                    "type": "text",
                                                    "text": ""
                                                }
                                            });
                                            yield Ok(encode_sse("content_block_start", &start_event));
                                            open_indices.insert(index);
                                        }

                                        let delta_event = json!({
                                            "type": "content_block_delta",
                                            "index": index,
                                            "delta": {
                                                "type": "text_delta",
                                                "text": delta
                                            }
                                        });
                                        yield Ok(encode_sse("content_block_delta", &delta_event));
                                        if is_cumulative {
                                            accumulated_text = visible_text;
                                        } else {
                                            accumulated_text.push_str(&delta);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    yield Err(std::io::Error::other(error.to_string()));
                    return;
                }
            }
        }

        // Termination-evidence guard: Gemini's `streamGenerateContent?alt=sse`
        // wire contract always ends a stream with a finishReason chunk, a
        // promptFeedback block, or (from OpenAI-style relays) the [DONE]
        // sentinel. An EOF with none of these means the stream was truncated
        // upstream; fabricating a normal end_turn completion would hide the
        // failure from the client and commit a partial turn to the shared
        // shadow session. Surface the error and stop, mirroring the transport
        // error path above.
        if !(latest_finish_reason.is_some() || saw_done || blocked_text.is_some()) {
            yield Err(std::io::Error::other(format!(
                "gemini stream ended without termination evidence (no finishReason/[DONE]/blockReason); parsed_chunks={parsed_chunks}, accumulated_text_bytes={}",
                accumulated_text.len()
            )));
            return;
        }

        if !has_sent_message_start {
            let event = json!({
                "type": "message_start",
                "message": {
                    "id": message_id.clone().unwrap_or_default(),
                    "type": "message",
                    "role": "assistant",
                    "model": current_model.clone().unwrap_or_default(),
                    "usage": build_anthropic_usage(latest_usage.as_ref())
                }
            });
            yield Ok(encode_sse("message_start", &event));
        }

        if accumulated_text.is_empty() {
            if let Some(blocked_text) = blocked_text.clone() {
                let index = *text_block_index.get_or_insert_with(|| {
                    let assigned = next_content_index;
                    next_content_index += 1;
                    assigned
                });

                if !open_indices.contains(&index) {
                    let start_event = json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "text",
                            "text": ""
                        }
                    });
                    yield Ok(encode_sse("content_block_start", &start_event));
                    open_indices.insert(index);
                }

                let delta_event = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "text_delta",
                        "text": blocked_text
                    }
                });
                yield Ok(encode_sse("content_block_delta", &delta_event));
            }
        }

        if let Some(index) = text_block_index {
            if open_indices.remove(&index) {
                let stop_event = json!({
                    "type": "content_block_stop",
                    "index": index
                });
                yield Ok(encode_sse("content_block_stop", &stop_event));
            }
        }

        // Assign client-only IDs after all upstream identities are known. No
        // synthetic ID participates in replay detection, and no generated ID
        // may collide with another call's explicit ID.
        let mut used_ids: HashSet<String> = tool_call_snapshots.iter()
            .filter_map(|call| call.id.clone()).collect();
        for call in &mut tool_call_snapshots {
            if call.id.is_none() {
                loop {
                    let id = synthesize_tool_call_id();
                    if used_ids.insert(id.clone()) {
                        call.id = Some(id);
                        break;
                    }
                }
            }
            rectify_tool_call_args(&call.name, &mut call.args, tool_schema_hints.as_ref());
        }

        if let (Some(store), Some(provider_id), Some(session_id)) = (
            shadow_store.as_ref(),
            provider_id.as_deref(),
            session_id.as_deref(),
        ) {
            let tool_calls = tool_call_snapshots.clone();
            let shadow_text = if accumulated_text.is_empty() {
                blocked_text.as_deref()
            } else {
                Some(accumulated_text.as_str())
            };
            let shadow_parts = build_shadow_assistant_parts(
                shadow_text,
                text_thought_signature.as_deref(),
                &tool_calls,
            );
            if !shadow_parts.is_empty() {
                store.record_assistant_turn(
                    provider_id,
                    session_id,
                    json!({ "parts": shadow_parts }),
                    tool_calls.clone(),
                );
            }
        }

        // ------------------------------------------------------------------
        // Known trade-off: tool-call ordering vs. interleaved text.
        //
        // We emit all `tool_use` blocks *after* the final text
        // `content_block_stop` above. If Gemini returns parts interleaved
        // like `[text_a, functionCall_1, text_b, functionCall_2]`, the
        // Anthropic-facing stream reorders them into `[text(a+b),
        // tool_use_1, tool_use_2]`, whereas `gemini_to_anthropic_with_shadow_and_hints`
        // (non-streaming) preserves the original part order.
        //
        // This is intentional given the current design:
        //   1. We buffer complete calls until termination so that explicit-ID
        //      replay conflicts fail before any tool becomes executable, and
        //      one-shot signatures can be stored before the client acts.
        //   2. Anthropic clients we target (claude-code and similar) consume
        //      a message's tool calls by scanning for `tool_use` blocks and
        //      do not depend on strict text ↔ tool interleaving for
        //      correctness of tool execution or result routing.
        //
        // If a future client requires strict part-order fidelity in the
        // streaming path, the fix is to track each part's original index,
        // segment the accumulated text into multiple content blocks at
        // tool-call boundaries, and flush in original order.
        // ------------------------------------------------------------------
        let tool_calls = tool_call_snapshots;
        for tool_call in &tool_calls {
            let index = next_content_index;
            next_content_index += 1;

            let start_event = json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_call.id.clone().unwrap_or_default(),
                    "name": tool_call.name
                }
            });
            yield Ok(encode_sse("content_block_start", &start_event));

            let delta_event = json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": serde_json::to_string(&tool_call.args).unwrap_or_else(|_| "{}".to_string())
                }
            });
            yield Ok(encode_sse("content_block_delta", &delta_event));

            let stop_event = json!({
                "type": "content_block_stop",
                "index": index
            });
            yield Ok(encode_sse("content_block_stop", &stop_event));
        }

        let stop_reason = map_finish_reason(
            latest_finish_reason.as_deref(),
            !tool_calls.is_empty(),
            blocked_text.is_some(),
        );
        let usage = build_anthropic_usage(latest_usage.as_ref());
        let message_delta = json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": Value::Null
            },
            "usage": usage
        });
        yield Ok(encode_sse("message_delta", &message_delta));

        let message_stop = json!({ "type": "message_stop" });
        yield Ok(encode_sse("message_stop", &message_stop));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ported::providers::gemini_shadow::GeminiShadowStore;
    use crate::ported::providers::transform_gemini::anthropic_to_gemini_with_shadow;
    use std::sync::Arc;

    fn collect_stream_output(chunks: Vec<&str>) -> String {
        let owned_chunks: Vec<String> = chunks.into_iter().map(ToString::to_string).collect();
        let stream = futures::stream::iter(
            owned_chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
        );
        let converted = create_anthropic_sse_stream_from_gemini(stream, None, None, None, None);
        futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        })
    }

    fn collect_stream_output_with_shadow(
        chunks: Vec<&str>,
        store: Arc<GeminiShadowStore>,
        provider_id: &str,
        session_id: &str,
    ) -> String {
        let owned_chunks: Vec<String> = chunks.into_iter().map(ToString::to_string).collect();
        let stream = futures::stream::iter(
            owned_chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
        );
        let converted = create_anthropic_sse_stream_from_gemini(
            stream,
            Some(store),
            Some(provider_id.to_string()),
            Some(session_id.to_string()),
            None,
        );
        futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        })
    }

    #[test]
    fn converts_text_stream_to_anthropic_sse() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"resp_1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"totalTokenCount\":13}}\n\n",
            "data: {\"responseId\":\"resp_1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"totalTokenCount\":15}}\n\n",
        ]);

        assert!(output.contains("event: message_start"));
        assert!(output.contains("\"type\":\"text_delta\""));
        assert!(output.contains("\"text\":\"Hel\""));
        assert!(output.contains("\"text\":\"lo\""));
        assert!(output.contains("\"stop_reason\":\"end_turn\""));
        assert!(output.contains("event: message_stop"));
    }

    #[test]
    fn converts_function_call_stream_to_tool_use_events() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"resp_2\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}},\"thoughtSignature\":\"sig-1\"}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ]);

        assert!(output.contains("\"type\":\"tool_use\""));
        assert!(output.contains("\"name\":\"get_weather\""));
        assert!(output.contains("\"type\":\"input_json_delta\""));
        assert!(output.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn converts_crlf_delimited_stream_to_anthropic_sse() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"resp_3\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\r\n\r\n",
            "data: {\"responseId\":\"resp_3\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"Hi there\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\r\n\r\n",
        ]);

        assert!(output.contains("event: message_start"));
        assert!(output.contains("\"type\":\"text_delta\""));
        assert!(output.contains("\"text\":\"Hi\""));
        assert!(output.contains("\"text\":\" there\""));
        assert!(output.contains("event: message_stop"));
    }

    #[test]
    fn preserves_utf8_boundaries_when_json_payload_spans_chunks() {
        let payload = json!({
            "responseId": "resp_utf8",
            "modelVersion": "gemini-2.5-pro",
            "candidates": [{
                "finishReason": "STOP",
                "content": {
                    "parts": [{ "text": "你好，Gemini" }]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 4,
                "totalTokenCount": 8
            }
        });
        let chunk = format!("data: {}\n\n", serde_json::to_string(&payload).unwrap());
        let split_at = chunk.find("你好").unwrap() + 1;
        let chunk_bytes = chunk.into_bytes();
        let stream = futures::stream::iter([
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk_bytes[..split_at].to_vec())),
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk_bytes[split_at..].to_vec())),
        ]);
        let converted = create_anthropic_sse_stream_from_gemini(stream, None, None, None, None);
        let output = futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        });

        assert!(output.contains("你好，Gemini"));
        assert!(!output.contains('\u{fffd}'));
    }

    #[test]
    fn stores_full_text_for_shadow_replay_across_delta_chunks() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        let output = collect_stream_output_with_shadow(
            vec![
                "data: {\"responseId\":\"resp_4\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
                "data: {\"responseId\":\"resp_4\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"lo\"},{\"text\":\"\",\"thoughtSignature\":\"sig-1\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":8}}\n\n",
            ],
            store.clone(),
            "provider-a",
            "session-1",
        );

        assert!(output.contains("\"text\":\"Hel\""));
        assert!(output.contains("\"text\":\"lo\""));

        let shadow = store
            .latest_assistant_content("provider-a", "session-1")
            .unwrap();
        assert_eq!(shadow["parts"][0]["text"], "Hello");
        assert_eq!(shadow["parts"][0]["thoughtSignature"], "sig-1");

        let second_turn = anthropic_to_gemini_with_shadow(
            json!({
                "messages": [
                    { "role": "user", "content": "Hi" },
                    { "role": "assistant", "content": [{ "type": "text", "text": "Hello" }] },
                    { "role": "user", "content": "Continue" }
                ]
            }),
            Some(store.as_ref()),
            Some("provider-a"),
            Some("session-1"),
        )
        .unwrap();

        assert_eq!(second_turn["contents"][1]["role"], "model");
        assert_eq!(second_turn["contents"][1]["parts"][0]["text"], "Hello");
        assert_eq!(
            second_turn["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-1"
        );
    }

    #[test]
    fn stores_tool_shadow_before_tool_use_events_are_fully_drained() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        let chunks = vec![
            "data: {\"responseId\":\"resp_tool_shadow\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"Bash\",\"args\":{\"command\":\"ls -R\"}},\"thoughtSignature\":\"sig-tool-1\"}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n".to_string(),
        ];
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
        );
        let mut converted = Box::pin(create_anthropic_sse_stream_from_gemini(
            stream,
            Some(store.clone()),
            Some("provider-a".to_string()),
            Some("session-1".to_string()),
            None,
        ));

        futures::executor::block_on(async {
            while let Some(item) = converted.next().await {
                let event = String::from_utf8(item.unwrap().to_vec()).unwrap();
                if event.contains("\"type\":\"tool_use\"") {
                    break;
                }
            }
        });

        let shadow = store
            .latest_assistant_content("provider-a", "session-1")
            .unwrap();
        assert_eq!(shadow["parts"][0]["functionCall"]["name"], "Bash");
        assert_eq!(shadow["parts"][0]["thoughtSignature"], "sig-tool-1");
    }

    #[test]
    fn rectifies_streamed_tool_call_args_from_tool_schema_hints() {
        let owned_chunks = vec![
            "data: {\"responseId\":\"resp_5\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"Bash\",\"args\":{\"args\":\"git status\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n".to_string(),
        ];
        let stream = futures::stream::iter(
            owned_chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
        );
        let hints = super::super::transform_gemini::extract_anthropic_tool_schema_hints(&json!({
            "tools": [{
                "name": "Bash",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeout": { "type": "number" }
                    },
                    "required": ["command"]
                }
            }]
        }));
        let converted =
            create_anthropic_sse_stream_from_gemini(stream, None, None, None, Some(hints));
        let output = futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        });

        assert!(output.contains("\"partial_json\":\"{\\\"command\\\":\\\"git status\\\"}\""));
    }

    #[test]
    fn rectifies_streamed_skill_args_from_nested_parameters() {
        let payload = json!({
            "responseId": "resp_6",
            "modelVersion": "gemini-2.5-pro",
            "candidates": [{
                "finishReason": "STOP",
                "content": {
                    "parts": [{
                        "functionCall": {
                            "id": "call_1",
                            "name": "Skill",
                            "args": {
                                "name": "git-commit",
                                "parameters": {
                                    "args": ["详细分析内容 编写提交信息 分多次提交代码"]
                                }
                            }
                        }
                    }]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "totalTokenCount": 8
            }
        });
        let owned_chunks = vec![format!(
            "data: {}\n\n",
            serde_json::to_string(&payload).unwrap()
        )];
        let stream = futures::stream::iter(
            owned_chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
        );
        let hints = super::super::transform_gemini::extract_anthropic_tool_schema_hints(&json!({
            "tools": [{
                "name": "Skill",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "skill": { "type": "string" },
                        "args": { "type": "string" }
                    },
                    "required": ["skill"]
                }
            }]
        }));
        let converted =
            create_anthropic_sse_stream_from_gemini(stream, None, None, None, Some(hints));
        let output = futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        });

        assert!(output.contains("git-commit"));
        assert!(output.contains("详细分析内容 编写提交信息 分多次提交代码"));
        assert!(!output.contains("\\\"parameters\\\""));
    }

    /// Regression for the P1 finding: when Gemini emits two parallel calls to
    /// the same function without providing ids, both must be surfaced to the
    /// Anthropic client with distinct synthesized ids. The previous
    /// name-based fallback in `merge_tool_call_snapshots` collapsed them into
    /// a single entry, causing silent data loss for the first call.
    #[test]
    fn parallel_same_name_no_id_calls_preserve_both() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}},{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Osaka\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ]);

        let tool_use_start_count = output.matches("\"type\":\"tool_use\"").count();
        assert_eq!(
            tool_use_start_count, 2,
            "both parallel calls must survive merge_tool_call_snapshots"
        );
        // `input_json_delta.partial_json` is a string, so the city keys appear
        // JSON-escaped inside the outer SSE `data:` payload. Match against
        // the raw escape sequences rather than the canonical JSON form.
        assert!(output.contains("Tokyo"));
        assert!(output.contains("Osaka"));
        // Each tool_use must carry a non-empty synthesized id so Claude Code
        // can disambiguate the two tool_result round-trips.
        let synth_count = output.matches("\"id\":\"gemini_synth_").count();
        assert_eq!(synth_count, 2);
    }

    /// Intentional divergence: the pinned upstream fixture calls this a
    /// cumulative update, but neither responseId nor a superset of args proves
    /// identity. Preserve both complete calls instead of silently losing one.
    #[test]
    fn no_id_calls_with_growing_args_are_not_assumed_cumulative() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r2\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
            "data: {\"responseId\":\"r2\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\",\"units\":\"c\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\n\n",
        ]);

        assert_eq!(output.matches("\"type\":\"tool_use\"").count(), 2);
        assert_eq!(output.matches("\"id\":\"gemini_synth_").count(), 2);
        assert!(output.contains("\"partial_json\":\"{\\\"city\\\":\\\"Tokyo\\\"}\""));
        assert!(output.contains("\"units\\\":\\\"c\\\""));
    }

    /// Regression for the follow-up Codex P1: some Gemini relays serialize
    /// an absent functionCall id as `"id": ""` rather than omitting the
    /// field. Without a filter, `Some("")` would reach
    /// `merge_tool_call_snapshots`, two parallel no-id calls would match
    /// each other on the empty-string id, and the second would overwrite
    /// the first — silently losing a call. Also the emitted Anthropic
    /// `tool_use.id` would be the empty string, so tool_result
    /// correlation from the Claude client would break.
    #[test]
    fn parallel_empty_string_id_calls_are_treated_as_missing_and_preserved() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r3\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}},{\"functionCall\":{\"id\":\"\",\"name\":\"get_weather\",\"args\":{\"city\":\"Osaka\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ]);

        let tool_use_count = output.matches("\"type\":\"tool_use\"").count();
        assert_eq!(
            tool_use_count, 2,
            "both parallel calls must survive even when ids are explicit empty strings"
        );
        assert!(output.contains("Tokyo"));
        assert!(output.contains("Osaka"));
        // No tool_use may emit an empty id — each must get its own
        // synthesized id so tool_result correlation works.
        assert!(
            !output.contains("\"id\":\"\""),
            "empty tool_use id leaked through: {output}"
        );
        let synth_count = output.matches("\"id\":\"gemini_synth_").count();
        assert_eq!(synth_count, 2);
    }

    /// Companion regression: a single-chunk stream whose sole functionCall
    /// carries `"id": ""` must still emit exactly one tool_use with a
    /// synthesized id, not an empty one. This covers the non-parallel
    /// degraded-relay case that the parallel test above subsumes.
    #[test]
    fn single_empty_string_id_tool_call_gets_synthesized_id() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r4\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"totalTokenCount\":5}}\n\n",
        ]);

        assert_eq!(output.matches("\"type\":\"tool_use\"").count(), 1);
        assert!(!output.contains("\"id\":\"\""));
        assert_eq!(output.matches("\"id\":\"gemini_synth_").count(), 1);
    }

    /// Intentional divergence: this upstream fixture supplies no correlation
    /// between its no-ID call and the later real ID. A positional upgrade
    /// would lose an independent complete call and transfer its signature.
    #[test]
    fn later_real_id_does_not_replace_an_unidentified_call() {
        let output = collect_stream_output(vec![
            // Chunk 1: complete call without an upstream identity.
            "data: {\"responseId\":\"rupg\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
            // Chunk 2: a new real ID cannot identify the earlier no-ID call.
            "data: {\"responseId\":\"rupg\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"real_id_abc\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\",\"units\":\"c\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\n\n",
        ]);

        assert_eq!(output.matches("\"type\":\"tool_use\"").count(), 2);
        assert!(output.contains("\"id\":\"real_id_abc\""));
        assert_eq!(output.matches("\"id\":\"gemini_synth_").count(), 1);
        assert!(output.contains("\"partial_json\":\"{\\\"city\\\":\\\"Tokyo\\\"}\""));
        assert!(output.contains("units"));
    }

    /// Preserve the upstream signature assertion for an identical explicit-ID
    /// replay. Intentional fixture divergence: the original also changed args,
    /// which now fails (public same_explicit_id_conflicting_name_or_args_errors_without_shadow_commit).
    #[test]
    fn thought_signature_preserved_when_identical_replay_omits_it() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        collect_stream_output_with_shadow(
            vec![
                // Chunk 1: carries thoughtSignature "sig-keep".
                "data: {\"responseId\":\"rsig\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}},\"thoughtSignature\":\"sig-keep\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
                // Chunk 2: identical explicit-ID replay, signature omitted.
                "data: {\"responseId\":\"rsig\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\n\n",
            ],
            store.clone(),
            "provider-sig",
            "session-sig",
        );

        let shadow = store
            .latest_assistant_content("provider-sig", "session-sig")
            .expect("shadow turn must be recorded");
        assert_eq!(shadow["parts"][0]["functionCall"]["id"], "call_1");
        assert_eq!(
            shadow["parts"][0]["thoughtSignature"], "sig-keep",
            "prior thoughtSignature must survive a later chunk that omits it: {shadow}"
        );
    }
}
