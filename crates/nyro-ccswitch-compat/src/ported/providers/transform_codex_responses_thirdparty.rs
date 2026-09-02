//! Third-party strict Responses upstreams (GLM, DeepSeek, …) — Codex
//! Responses-Lite normalization for the native Responses passthrough.
//!
//! Codex Desktop declares its tools through a private Responses-Lite
//! convention: an `{"type":"additional_tools","tools":[…]}` carrier item
//! inside `input` (marked by the `x-openai-internal-codex-responses-lite`
//! header) plus `namespace`/`custom` tool shapes that only OpenAI's own
//! Codex backend understands. When such a request is forwarded verbatim to a
//! third-party Responses-compatible endpoint, the upstream registers **zero**
//! tools: the model then either answers without tools or leaks its native
//! tool-call syntax as plain text (e.g. DeepSeek's `<｜｜DSML｜｜tool_calls>`).
//!
//! This module rewrites the request wire so strict third-party upstreams see
//! only public Responses shapes, and restores the Codex-private shapes on the
//! way back:
//!
//! - **Request**: lift the `additional_tools` carrier into top-level `tools`
//!   (de-duplicated), flatten `namespace` tools into flat `function` tools
//!   (reusing the xAI-proven flatten), convert `custom` (freeform grammar)
//!   tools into `function` tools with a single `input` string parameter, drop
//!   remaining non-function tool types, rewrite replayed
//!   `custom_tool_call`/`custom_tool_call_output` history items into
//!   `function_call`/`function_call_output`, and neutralize a `custom`
//!   `tool_choice`.
//! - **Response**: convert `function_call` items whose name belongs to a
//!   converted custom tool back into `custom_tool_call` items (buffered JSON
//!   and streaming SSE), emitting the `response.custom_tool_call_input`
//!   delta/done events Codex expects, on top of the namespace name restore.
//!
//! The xAI sanitize path ([`super::transform_codex_responses_xai_sanitize`])
//! intentionally stays untouched (cc-switch parity); this module mirrors its
//! carrier-promotion semantics but keeps `custom` tools alive via conversion.

use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use serde_json::{Value, json};

use super::transform_codex_responses_namespace::{
    NamespacedName, flatten_request_namespaces, restore_sse_event_namespaces,
};
use crate::ported::error::ProxyError;
use crate::ported::sse::{append_utf8_safe, strip_sse_field, take_sse_block};

/// Tool types allowed on the third-party wire after rewriting. Everything
/// else (e.g. Codex's private `tool_search`) is dropped rather than risking
/// an upstream 422 on an unknown variant.
const SUPPORTED_TOOL_TYPES: &[&str] = &["function"];

// ─────────────────────────────────────────────────────────────────────────────
// Gating
// ─────────────────────────────────────────────────────────────────────────────

fn is_additional_tools_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str).map(str::trim) == Some("additional_tools")
}

/// Whether the request body carries Codex Responses-Lite artifacts that a
/// strict third-party upstream cannot consume as-is. Best-effort: an
/// unparseable body reports `false` (the normal pipeline surfaces the error).
pub fn request_needs_rewrite(body: &[u8]) -> bool {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => request_needs_rewrite_value(&value),
        Err(_) => false,
    }
}

/// Whether the request body carries Codex Responses-Lite artifacts that a
/// strict third-party upstream cannot consume as-is.
pub(crate) fn request_needs_rewrite_value(body: &Value) -> bool {
    let input_has = |pred: fn(&Value) -> bool| {
        body.get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(pred))
    };
    if input_has(is_additional_tools_item) {
        return true;
    }
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(needs_tool_rewrite))
    {
        return true;
    }
    // Codex Responses-Lite tool outputs without `call_id` (e.g. automation
    // heartbeats carrying only their own `fco_*` id) are invalid wire for a
    // strict upstream; they need the call-id normalization pass.
    if input_has(is_call_id_less_tool_output) {
        return true;
    }
    // Replay history: a prior turn produced Codex-private call items that the
    // upstream never sees as functions unless rewritten.
    input_has(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("custom_tool_call") | Some("custom_tool_call_output")
        )
    })
}

/// Whether the item is a tool-call output item in the Responses wire sense.
fn is_tool_call_output_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call_output") | Some("custom_tool_call_output")
    )
}

/// Whether a tool-call output item lacks a usable `call_id` (Codex
/// Responses-Lite heartbeats carry only their own `id`, e.g. `fco_*`).
fn is_call_id_less_tool_output(item: &Value) -> bool {
    is_tool_call_output_item(item)
        && item
            .get("call_id")
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
}

fn needs_tool_rewrite(tool: &Value) -> bool {
    matches!(
        tool.get("type").and_then(Value::as_str).map(str::trim),
        Some("namespace") | Some("custom") | Some("tool_search")
    )
}

/// Names of `custom` tools (top-level or inside carriers) that the request
/// rewrite will convert into `function` tools. The response side uses this
/// set to convert matching `function_call` items back into
/// `custom_tool_call` items.
pub(crate) fn custom_tool_restore_names(body: &Value) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut collect = |tools: Option<&Vec<Value>>| {
        if let Some(tools) = tools {
            for tool in tools {
                if tool.get("type").and_then(Value::as_str) == Some("custom")
                    && let Some(name) = tool.get("name").and_then(Value::as_str)
                    && !name.trim().is_empty()
                {
                    names.insert(name.trim().to_string());
                }
            }
        }
    };
    collect(body.get("tools").and_then(Value::as_array));
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            if is_additional_tools_item(item) {
                collect(item.get("tools").and_then(Value::as_array));
            }
        }
    }
    names
}

// ─────────────────────────────────────────────────────────────────────────────
// Request rewrite
// ─────────────────────────────────────────────────────────────────────────────

/// Rewrite a Codex Responses request body for a strict third-party upstream.
/// See the module docs for the exact steps.
pub(crate) fn rewrite_request_for_third_party(body: &mut Value) -> Result<(), ProxyError> {
    promote_additional_tools(body);
    flatten_request_namespaces(body)?;
    convert_custom_tools_to_functions(body);
    filter_unsupported_tool_types(body);
    rewrite_custom_history(body);
    normalize_tool_output_call_ids(body);
    Ok(())
}

/// Normalize tool-call output items for a strict third-party upstream:
/// backfill a missing `call_id` from the item's own `id` and drop outputs
/// that reference no call anywhere in the current input.
///
/// Codex Responses-Lite automation heartbeats send `function_call_output`
/// items carrying only their own `fco_*` id (plus `name`/`namespace`),
/// without the matching `function_call` items being replayed. A strict
/// upstream rejects such an output — either as invalid wire (missing
/// `call_id`) or as an unresolvable reference. Mirrors sub2api's
/// `filterCodexInputWithOptions` call-id backfill plus
/// `sanitizeOpenAIResponsesOrphanToolOutputs` orphan removal.
fn normalize_tool_output_call_ids(body: &mut Value) {
    // With server-side state (`previous_response_id`), outputs may reference
    // calls the upstream resolved earlier — dropping them would lose real
    // tool results. Only backfill call ids in that mode (sub2api parity).
    let has_previous_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };

    // Referenceable identifiers: tool-call items (by `call_id` or `id`) and
    // explicit `item_reference` ids.
    let mut referenceable: HashSet<String> = HashSet::new();
    for item in items.iter() {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") | Some("custom_tool_call") => {
                for field in ["call_id", "id"] {
                    if let Some(id) = item.get(field).and_then(Value::as_str) {
                        let id = id.trim();
                        if !id.is_empty() {
                            referenceable.insert(id.to_string());
                        }
                    }
                }
            }
            Some("item_reference") => {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    let id = id.trim();
                    if !id.is_empty() {
                        referenceable.insert(id.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    let original = std::mem::take(items);
    *items = original
        .into_iter()
        .filter_map(|mut item| {
            if !is_tool_call_output_item(&item) {
                return Some(item);
            }
            let existing_call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            let own_id = item
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string);

            let effective_id = existing_call_id.as_ref().or(own_id.as_ref());
            // An output referencing no call and no item reference is an
            // orphan: keeping it would 400 on strict upstreams, and its
            // payload (a heartbeat notification) carries no tool result.
            let is_orphan = !has_previous_response_id
                && effective_id.is_none_or(|id| !referenceable.contains(id));
            if is_orphan {
                return None;
            }

            // Backfill `call_id` from the item's own `id` when missing so
            // the output stays well-formed wire.
            if existing_call_id.is_none()
                && let Some(id) = own_id
                && let Some(object) = item.as_object_mut()
            {
                object.insert("call_id".to_string(), Value::String(id));
            }
            Some(item)
        })
        .collect();
}

/// Lift `additional_tools` carrier items from `input` into top-level `tools`,
/// preserving top-level order and appending carrier tools de-duplicated.
/// Mirrors the xAI sanitize's `promote_additional_tools` (kept separate to
/// avoid touching the parity-checked module).
fn promote_additional_tools(body: &mut Value) {
    let input_items: Vec<Value> = match body.get("input").and_then(Value::as_array) {
        Some(arr) if arr.iter().any(is_additional_tools_item) => arr.clone(),
        _ => return,
    };

    let mut merged: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            seen.insert(tool_dedup_key(tool));
            merged.push(tool.clone());
        }
    }

    let mut filtered_input: Vec<Value> = Vec::with_capacity(input_items.len());
    for item in input_items {
        if is_additional_tools_item(&item) {
            if let Some(carrier_tools) = item.get("tools").and_then(Value::as_array) {
                for tool in carrier_tools {
                    if seen.insert(tool_dedup_key(tool)) {
                        merged.push(tool.clone());
                    }
                }
            }
            continue;
        }
        filtered_input.push(item);
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("input".to_string(), Value::Array(filtered_input));
        obj.insert("tools".to_string(), Value::Array(merged));
    }
}

/// Stable dedup key: `(type, name)`, falling back to the serialized tool.
fn tool_dedup_key(tool: &Value) -> String {
    let tool_type = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if let Some(name) = tool.get("name").and_then(Value::as_str).map(str::trim)
        && !name.is_empty()
        && !tool_type.is_empty()
    {
        return format!("type:{tool_type}\u{0}name:{name}");
    }
    format!("json:{tool}")
}

/// Convert top-level `custom` (freeform grammar) tools into `function` tools
/// with a single required `input` string parameter. The response side inverts
/// this via [`restore_custom_tool_calls`].
fn convert_custom_tools_to_functions(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut() {
        if tool.get("type").and_then(Value::as_str) != Some("custom") {
            continue;
        }
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let description = tool
            .get("description")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        *tool = json!({
            "type": "function",
            "name": name,
            "description": description,
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "Raw tool input text (freeform)"
                    }
                },
                "required": ["input"],
                "additionalProperties": false
            }
        });
    }
}

/// Keep only supported tool types and clean a dangling `tool_choice`.
fn filter_unsupported_tool_types(body: &mut Value) {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return;
    };
    let filtered: Vec<Value> = tools
        .iter()
        .filter(|tool| {
            let t = tool
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            SUPPORTED_TOOL_TYPES.contains(&t)
        })
        .cloned()
        .collect();

    let tools_changed = filtered.len() != tools.len();
    if tools_changed && let Some(obj) = body.as_object_mut() {
        if filtered.is_empty() {
            obj.remove("tools");
        } else {
            obj.insert("tools".to_string(), Value::Array(filtered.clone()));
        }
    }

    // A `custom` tool_choice references a tool that is now a function.
    // (Namespace choices were already degraded by the flatten step.)
    if let Some(choice) = body.get_mut("tool_choice")
        && choice.get("type").and_then(Value::as_str) == Some("custom")
    {
        let name = choice.get("name").cloned().unwrap_or(Value::Null);
        *choice = json!({"type": "function", "name": name});
    }
    // Drop a named choice whose tool no longer exists.
    if let Some(choice) = body.get("tool_choice").and_then(Value::as_object) {
        if let Some(name) = choice.get("name").and_then(Value::as_str) {
            let exists = body
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|t| t.get("name").and_then(Value::as_str) == Some(name))
                });
            if !exists && let Some(obj) = body.as_object_mut() {
                obj.remove("tool_choice");
            }
        }
    }
}

/// Rewrite replayed Codex-private history items into public shapes:
/// `custom_tool_call` → `function_call` (arguments `{"input": …}`) and
/// `custom_tool_call_output` → `function_call_output`.
fn rewrite_custom_history(body: &mut Value) {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in input.iter_mut() {
        match item.get("type").and_then(Value::as_str) {
            Some("custom_tool_call") => {
                let mut converted = json!({
                    "type": "function_call",
                    "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": json!({
                        "input": item.get("input").cloned().unwrap_or_else(|| json!(""))
                    })
                    .to_string(),
                });
                if let Some(id) = item.get("id") {
                    converted["id"] = id.clone();
                }
                if let Some(obj) = converted.as_object_mut() {
                    obj.retain(|_, v| !v.is_null());
                }
                *item = converted;
            }
            Some("custom_tool_call_output") => {
                let mut converted = json!({
                    "type": "function_call_output",
                    "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "output": item.get("output").cloned().unwrap_or_else(|| json!("")),
                });
                if let Some(id) = item.get("id") {
                    converted["id"] = id.clone();
                }
                if let Some(obj) = converted.as_object_mut() {
                    obj.retain(|_, v| !v.is_null());
                }
                *item = converted;
            }
            _ => {}
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Response restore
// ─────────────────────────────────────────────────────────────────────────────

/// Extract the freeform input from a converted tool call's `arguments`
/// (a JSON string `{"input": "…"}`). Falls back to the raw arguments text
/// when it is not the expected shape, so no bytes are silently lost.
fn custom_input_from_arguments(arguments: &Value) -> String {
    let raw = match arguments {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(obj)) => match obj.get("input") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => raw,
        },
        _ => raw,
    }
}

/// Convert a `function_call` item into a `custom_tool_call` item.
fn function_call_to_custom_item(item: &mut Value) -> bool {
    let Some(obj) = item.as_object_mut() else {
        return false;
    };
    if obj.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    let input = custom_input_from_arguments(obj.get("arguments").unwrap_or(&Value::Null));
    obj.insert("type".to_string(), json!("custom_tool_call"));
    obj.insert("input".to_string(), json!(input));
    obj.remove("arguments");
    true
}

/// Whether a `function_call` item names a converted custom tool.
fn is_custom_call(item: &Value, custom_names: &HashSet<String>) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call")
        && item
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| custom_names.contains(name.trim()))
}

/// Restore converted custom tool calls in a full (non-streaming) Responses
/// payload. Walks the whole tree so `response.completed`'s embedded output
/// array is covered too. Returns whether anything changed.
pub(crate) fn restore_custom_tool_calls(value: &mut Value, custom_names: &HashSet<String>) -> bool {
    if custom_names.is_empty() {
        return false;
    }
    let mut changed = false;
    match value {
        Value::Array(items) => {
            for item in items {
                changed |= restore_custom_tool_calls(item, custom_names);
            }
        }
        Value::Object(_) => {
            if is_custom_call(value, custom_names) {
                changed |= function_call_to_custom_item(value);
            }
            if let Value::Object(obj) = value {
                for child in obj.values_mut() {
                    changed |= restore_custom_tool_calls(child, custom_names);
                }
            }
        }
        _ => {}
    }
    changed
}

/// Wrap a native Responses SSE byte stream, restoring both namespace-flattened
/// names and converted custom tool calls in each event.
///
/// Custom-call restore is stateful across blocks: `output_item.added` marks
/// the item ids belonging to converted tools so their
/// `response.function_call_arguments.delta` events can be dropped; at
/// `output_item.done` the full item is converted and preceded by synthetic
/// `response.custom_tool_call_input.delta`/`.done` events, matching the event
/// sequence Codex expects for custom tools.
pub(crate) fn create_third_party_restore_sse_stream<E>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
    namespace_map: HashMap<String, NamespacedName>,
    custom_names: HashSet<String>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    E: std::error::Error + Send + 'static,
{
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder: Vec<u8> = Vec::new();
        let mut custom_item_ids: HashSet<String> = HashSet::new();

        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);
                    while let Some(block) = take_sse_block(&mut buffer) {
                        if block.trim().is_empty() {
                            continue;
                        }
                        for out in restore_block(
                            block.as_str(),
                            &namespace_map,
                            &custom_names,
                            &mut custom_item_ids,
                        ) {
                            yield Ok(out);
                        }
                    }
                }
                Err(e) => {
                    yield Err(std::io::Error::other(e.to_string()));
                    return;
                }
            }
        }

        if !utf8_remainder.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&utf8_remainder));
        }
        let tail = std::mem::take(&mut buffer);
        if !tail.trim().is_empty() {
            for out in restore_block(&tail, &namespace_map, &custom_names, &mut custom_item_ids) {
                yield Ok(out);
            }
        }
    }
}

/// Restore one SSE block; returns the blocks to emit (0 = dropped, 1 =
/// passthrough, 3 = synthetic input events + converted done).
fn restore_block(
    block: &str,
    namespace_map: &HashMap<String, NamespacedName>,
    custom_names: &HashSet<String>,
    custom_item_ids: &mut HashSet<String>,
) -> Vec<Bytes> {
    let mut event_name: Option<String> = None;
    let mut data_parts: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(event) = strip_sse_field(line, "event") {
            event_name = Some(event.trim().to_string());
        }
        if let Some(data) = strip_sse_field(line, "data") {
            data_parts.push(data);
        }
    }

    if data_parts.is_empty() {
        return vec![Bytes::from(format!("{block}\n\n"))];
    }
    let data = data_parts.join("\n");
    if data.trim() == "[DONE]" {
        return vec![Bytes::from(format!("{block}\n\n"))];
    }

    let mut event: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(_) => return vec![Bytes::from(format!("{block}\n\n"))],
    };

    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Namespace name restore applies to any event carrying call items.
    let namespace_changed = restore_sse_event_namespaces(&mut event, namespace_map);

    match event_type.as_str() {
        "response.function_call_arguments.delta" => {
            let item_id = event.get("item_id").and_then(Value::as_str).unwrap_or("");
            if !item_id.is_empty() && custom_item_ids.contains(item_id) {
                // Argument fragments for a converted custom tool: dropped;
                // the done item carries the full input.
                return Vec::new();
            }
            if namespace_changed {
                return vec![rebuild_block(event_name.as_deref(), &event)];
            }
            return vec![Bytes::from(format!("{block}\n\n"))];
        }
        "response.output_item.added" => {
            if let Some(item) = event.get_mut("item") {
                if is_custom_call(item, custom_names) {
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !item_id.is_empty() {
                        custom_item_ids.insert(item_id);
                    }
                    function_call_to_custom_item(item);
                    return vec![rebuild_block(event_name.as_deref(), &event)];
                }
            }
        }
        "response.output_item.done" => {
            let mut converted: Option<(String, String)> = None;
            if let Some(item) = event.get_mut("item")
                && is_custom_call(item, custom_names)
            {
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                function_call_to_custom_item(item);
                let input = item
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                converted = Some((item_id, input));
            }
            if let Some((item_id, input)) = converted {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let mut out = Vec::with_capacity(3);
                if !item_id.is_empty() {
                    out.push(custom_input_event(
                        "response.custom_tool_call_input.delta",
                        &item_id,
                        output_index,
                        &input,
                    ));
                    out.push(custom_input_event(
                        "response.custom_tool_call_input.done",
                        &item_id,
                        output_index,
                        &input,
                    ));
                }
                out.push(rebuild_block(event_name.as_deref(), &event));
                return out;
            }
        }
        _ => {
            // response.completed and any other event carrying call items.
            if restore_custom_tool_calls(&mut event, custom_names) || namespace_changed {
                return vec![rebuild_block(event_name.as_deref(), &event)];
            }
        }
    }

    if namespace_changed {
        return vec![rebuild_block(event_name.as_deref(), &event)];
    }
    vec![Bytes::from(format!("{block}\n\n"))]
}

fn custom_input_event(event_type: &str, item_id: &str, output_index: u64, text: &str) -> Bytes {
    let field = if event_type.ends_with("delta") {
        "delta"
    } else {
        "input"
    };
    let data = json!({
        "type": event_type,
        "item_id": item_id,
        "output_index": output_index,
        field: text,
    });
    Bytes::from(format!("event: {event_type}\ndata: {data}\n\n"))
}

fn rebuild_block(event_name: Option<&str>, event: &Value) -> Bytes {
    let body = serde_json::to_string(event).unwrap_or_default();
    let mut out = String::new();
    if let Some(name) = event_name {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    out.push_str("data: ");
    out.push_str(&body);
    out.push_str("\n\n");
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use serde_json::json;

    fn codex_lite_request() -> Value {
        json!({
            "model": "glm-5.3",
            "stream": true,
            "tool_choice": "auto",
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "custom",
                            "name": "exec",
                            "description": "Run JavaScript code",
                            "format": {"type": "grammar", "syntax": "lark", "definition": "start: /[^]+/"}
                        },
                        {"type": "function", "name": "wait", "parameters": {"type": "object"}},
                        {
                            "type": "namespace",
                            "name": "collab",
                            "tools": [{"type": "function", "name": "spawn", "parameters": {"type": "object"}}]
                        }
                    ]
                },
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {
                    "type": "custom_tool_call",
                    "call_id": "c1",
                    "name": "exec",
                    "input": "text('hello')"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "c1",
                    "output": "hello"
                }
            ]
        })
    }

    #[test]
    fn gate_detects_carrier_namespaces_and_history() {
        assert!(request_needs_rewrite_value(&codex_lite_request()));
        assert!(!request_needs_rewrite_value(&json!({
            "model": "glm-5.3",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        })));
        assert!(!request_needs_rewrite_value(&json!({
            "model": "glm-5.3",
            "tools": [{"type": "function", "name": "plain", "parameters": {"type": "object"}}],
            "input": "hi"
        })));
        // Replay history alone (no carrier) still needs rewriting.
        assert!(request_needs_rewrite_value(&json!({
            "model": "glm-5.3",
            "input": [{"type": "custom_tool_call", "call_id": "c", "name": "exec", "input": "x"}]
        })));
        // Codex Responses-Lite heartbeat outputs carry only their own id —
        // no call_id (线上 400 dbc1cff5)。
        assert!(request_needs_rewrite_value(&json!({
            "model": "glm-5.3",
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call_output", "id": "fco_1", "name": "automation_update", "output": "<heartbeat/>"}
            ]
        })));
    }

    #[test]
    fn rewrite_drops_orphan_lite_heartbeat_outputs() {
        // 线上 400 dbc1cff5 的形态：automation heartbeat 输出只带自身
        // `fco_*` id，配对的 function_call 项并未回放进 input。
        let mut body = json!({
            "model": "glm-5.3",
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call_output", "id": "fco_01a05a7d-18e8-7be3-aeee-6530b96f76ff", "name": "automation_update", "namespace": "codex_app", "output": "<heartbeat/>"},
                {"type": "message", "role": "assistant", "content": "ok"},
                {"type": "function_call_output", "id": "fco_01a05fa8-37e1-75a1-895b-6b0270202e01", "name": "automation_update", "namespace": "codex_app", "output": "<heartbeat/>"}
            ]
        });
        rewrite_request_for_third_party(&mut body).unwrap();

        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "both orphan heartbeats dropped: {input:?}");
        assert!(
            input
                .iter()
                .all(|i| { i.get("type").and_then(Value::as_str) != Some("function_call_output") })
        );
    }

    #[test]
    fn rewrite_backfills_call_id_for_outputs_matching_a_call() {
        // 输出只带自身 id，且该 id 恰与调用项的 call_id 一致：回填后
        // 形成合法配对，保留。
        let mut body = json!({
            "model": "glm-5.3",
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "c9", "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "id": "c9", "output": "done"}
            ]
        });
        rewrite_request_for_third_party(&mut body).unwrap();

        let input = body["input"].as_array().unwrap();
        let output = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("output kept: id matches the call");
        assert_eq!(output["call_id"], "c9", "backfilled from own id");
    }

    #[test]
    fn rewrite_keeps_paired_output_with_explicit_call_id() {
        let mut body = json!({
            "model": "glm-5.3",
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "c9", "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c9", "output": "done"}
            ]
        });
        rewrite_request_for_third_party(&mut body).unwrap();

        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3, "nothing dropped: {input:?}");
        let output = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["call_id"], "c9");
    }

    #[test]
    fn rewrite_keeps_orphans_when_previous_response_id_present() {
        let mut body = json!({
            "model": "glm-5.3",
            "previous_response_id": "resp_123",
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call_output", "id": "fco_1", "output": "done"}
            ]
        });
        rewrite_request_for_third_party(&mut body).unwrap();

        let input = body["input"].as_array().unwrap();
        let output = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("server-side state keeps orphan");
        assert_eq!(output["call_id"], "fco_1", "call_id still backfilled");
    }

    #[test]
    fn custom_tool_names_cover_top_level_and_carrier() {
        let names = custom_tool_restore_names(&codex_lite_request());
        assert!(names.contains("exec"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn rewrite_promotes_carrier_and_converts_custom() {
        let mut body = codex_lite_request();
        rewrite_request_for_third_party(&mut body).unwrap();

        // Carrier removed from input; tools promoted to top level.
        let input = body["input"].as_array().unwrap();
        assert!(
            input
                .iter()
                .all(|i| i.get("type").and_then(Value::as_str) != Some("additional_tools"))
        );
        let tools = body["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"wait"));
        assert!(names.contains(&"collab__spawn"));
        assert!(tools.iter().all(|t| t["type"] == "function"));

        // exec became a function with a single input parameter.
        let exec = tools.iter().find(|t| t["name"] == "exec").unwrap();
        assert_eq!(exec["parameters"]["properties"]["input"]["type"], "string");

        // History rewritten into public shapes.
        let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(call["name"], "exec");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap()["input"],
            "text('hello')"
        );
        let output = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["output"], "hello");
    }

    #[test]
    fn rewrite_drops_unsupported_tools_and_fixes_custom_choice() {
        let mut body = json!({
            "tools": [
                {"type": "custom", "name": "exec"},
                {"type": "tool_search", "name": "ts"},
                {"type": "function", "name": "wait", "parameters": {"type": "object"}}
            ],
            "tool_choice": {"type": "custom", "name": "exec"}
        });
        rewrite_request_for_third_party(&mut body).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| t["type"] == "function"));
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "exec"})
        );
    }

    #[test]
    fn rewrite_drops_dangling_named_choice() {
        let mut body = json!({
            "tools": [{"type": "tool_search", "name": "ts"}],
            "tool_choice": {"type": "function", "name": "ts"}
        });
        rewrite_request_for_third_party(&mut body).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn buffered_restore_converts_custom_calls() {
        let names = custom_tool_restore_names(&codex_lite_request());
        let mut response = json!({
            "output": [
                {"type": "function_call", "call_id": "c9", "name": "exec", "arguments": "{\"input\":\"text('hi')\"}"},
                {"type": "function_call", "call_id": "c8", "name": "wait", "arguments": "{}"}
            ]
        });
        assert!(restore_custom_tool_calls(&mut response, &names));
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["input"], "text('hi')");
        assert!(response["output"][0].get("arguments").is_none());
        // Non-custom calls untouched.
        assert_eq!(response["output"][1]["type"], "function_call");
    }

    #[test]
    fn arguments_fallback_keeps_raw_text() {
        let names = HashSet::from(["exec".to_string()]);
        let mut response = json!({
            "output": [{"type": "function_call", "name": "exec", "arguments": "not json"}]
        });
        assert!(restore_custom_tool_calls(&mut response, &names));
        assert_eq!(response["output"][0]["input"], "not json");
    }

    #[tokio::test]
    async fn sse_stream_restores_custom_call_sequence() {
        let names = HashSet::from(["exec".to_string()]);
        let map = HashMap::new();

        let added = "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"c1\",\"name\":\"exec\"}}\n\n";
        let delta = "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"input\\\":\\\"js\\\"}\"}\n\n";
        let done = "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"c1\",\"name\":\"exec\",\"arguments\":\"{\\\"input\\\":\\\"js\\\"}\"}}\n\n";
        let text = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        let eod = "data: [DONE]\n\n";

        let input = stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from(added.to_string())),
            Ok(Bytes::from(delta.to_string())),
            Ok(Bytes::from(done.to_string())),
            Ok(Bytes::from(text.to_string())),
            Ok(Bytes::from(eod.to_string())),
        ]);
        let out = create_third_party_restore_sse_stream(input, map, names);
        futures::pin_mut!(out);

        let mut collected = String::new();
        while let Some(chunk) = out.next().await {
            collected.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert!(collected.contains("response.custom_tool_call_input.delta"));
        assert!(collected.contains("\"delta\":\"js\""));
        assert!(collected.contains("response.custom_tool_call_input.done"));
        assert!(collected.contains("\"type\":\"custom_tool_call\""));
        assert!(collected.contains("\"input\":\"js\""));
        // Argument delta for the converted call is dropped.
        assert!(!collected.contains("response.function_call_arguments.delta"));
        // Unrelated events pass through.
        assert!(collected.contains("\"delta\":\"hi\""));
        assert!(collected.contains("[DONE]"));
    }

    #[tokio::test]
    async fn sse_stream_restores_namespaces_and_completed_payload() {
        let mut map = HashMap::new();
        map.insert(
            "mcp__files____read".to_string(),
            NamespacedName {
                namespace: "mcp__files__".into(),
                name: "read".into(),
            },
        );
        let custom = HashSet::new();

        let added = "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"c1\",\"name\":\"mcp__files____read\"}}\n\n";
        let completed = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"name\":\"mcp__files____read\",\"call_id\":\"c1\"}]}}\n\n";

        let input = stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from(added.to_string())),
            Ok(Bytes::from(completed.to_string())),
        ]);
        let out = create_third_party_restore_sse_stream(input, map, custom);
        futures::pin_mut!(out);

        let mut collected = String::new();
        while let Some(chunk) = out.next().await {
            collected.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        assert!(collected.contains("\"name\":\"read\""));
        assert!(collected.contains("\"namespace\":\"mcp__files__\""));
        assert!(!collected.contains("mcp__files____read"));
    }
}
