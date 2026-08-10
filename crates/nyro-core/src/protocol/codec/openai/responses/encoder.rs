//! OpenAI Responses API egress encoder (PR-09).
//!
//! PR-09 adds forwarding for:
//! - `background` (bool)
//! - `previous_response_id` (string)
//! - Built-in tools (`web_search_preview`, `file_search`, `computer_use_preview`)
//! - `store` (bool — default true per spec; we default false for privacy)
//! - `include` (array of field paths)
//! - `truncation` (object)
//! - `metadata` / `text` / `reasoning` / `parallel_tool_calls` / `service_tier` / `user`

use std::collections::HashMap;

use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::RequestEncoder;
use crate::protocol::codec::reasoning::{effective_openai_effort, reasoning_effort_name};
use crate::protocol::ir::AiRequest;
use crate::protocol::ir::request::{
    ContentBlock, MessageContent, ReasoningConfig, Role, ToolCallKind, ToolChoice, ToolSpec,
    ToolSpecKind,
};

/// Encoder for the OpenAI Responses API (`POST /v1/responses`).
///
/// Forces `stream: true` because the Responses backend only supports SSE;
/// non-streaming ingress is aggregated downstream in the proxy handler.
pub struct ResponsesEncoder;

// Fields that must NOT be copied blindly from extra into the egress body.
const SKIP_FROM_EXTRA: &[&str] = &[
    "messages",
    "input",
    "instructions",
    "stream",
    "model",
    "reasoning_effort",
];

impl RequestEncoder for ResponsesEncoder {
    fn encode_request(&self, req: &AiRequest) -> Result<(Value, HeaderMap)> {
        let ingress = &req.meta.vendor.ingress;

        let mut instructions: Vec<String> = Vec::new();
        let mut input: Vec<Value> = Vec::new();
        let call_kinds: HashMap<&str, ToolCallKind> = req
            .messages
            .iter()
            .filter_map(|message| message.tool_calls.as_deref())
            .flatten()
            .map(|call| (call.id.as_str(), call.kind))
            .collect();

        for message in &req.messages {
            match message.role {
                Role::System => {
                    let text = message.content.to_text();
                    if !text.is_empty() {
                        instructions.push(text);
                    }
                }
                Role::User | Role::Assistant => {
                    // Block-form tool results (llm-bridge keeps them in user
                    // messages): emit one `*_tool_call_output` item per block.
                    let default_call_id = message.tool_call_id.clone().unwrap_or_default();
                    let custom_from_meta = message
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("__nyro_tool_call_kind"))
                        .and_then(Value::as_str)
                        == Some("custom");
                    if push_tool_result_items(
                        &mut input,
                        message,
                        &default_call_id,
                        custom_from_meta,
                        &call_kinds,
                    ) {
                        continue;
                    }

                    let text = message.content.to_text();
                    if !text.is_empty() {
                        let role_str = match message.role {
                            Role::User => "user",
                            _ => "assistant",
                        };
                        let content_type = if message.role == Role::Assistant {
                            "output_text"
                        } else {
                            "input_text"
                        };
                        input.push(serde_json::json!({
                            "type": "message",
                            "role": role_str,
                            "content": [{"type": content_type, "text": text}]
                        }));
                    }
                    if let Some(tool_calls) = &message.tool_calls {
                        for tool_call in tool_calls {
                            let mut item = match tool_call.kind {
                                ToolCallKind::Function => serde_json::json!({
                                    "type": "function_call",
                                    "call_id": tool_call.id,
                                    "name": tool_call.name,
                                    "arguments": tool_call.arguments,
                                }),
                                ToolCallKind::Custom => serde_json::json!({
                                    "type": "custom_tool_call",
                                    "call_id": tool_call.id,
                                    "name": tool_call.name,
                                    "input": tool_call.arguments,
                                }),
                            };
                            insert_optional_namespace(&mut item, tool_call.namespace.as_deref());
                            input.push(item);
                        }
                    }
                }
                Role::Tool => {
                    let call_id = message.tool_call_id.clone().unwrap_or_default();
                    let custom_from_meta = message
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("__nyro_tool_call_kind"))
                        .and_then(Value::as_str)
                        == Some("custom");

                    // Block-form tool results: one item per result block
                    // (llm-bridge semantics); a plain-text tool message still
                    // collapses to a single item.
                    if push_tool_result_items(
                        &mut input,
                        message,
                        &call_id,
                        custom_from_meta,
                        &call_kinds,
                    ) {
                        continue;
                    }

                    let item_type = if custom_from_meta
                        || call_kinds.get(call_id.as_str()) == Some(&ToolCallKind::Custom)
                    {
                        "custom_tool_call_output"
                    } else {
                        "function_call_output"
                    };
                    input.push(serde_json::json!({
                        "type": item_type,
                        "call_id": call_id,
                        "output": message.content.to_text(),
                    }));
                }
            }
        }

        if input.is_empty() {
            anyhow::bail!("responses request requires at least one input item");
        }

        let instructions_value = if instructions.is_empty() {
            Value::String("You are a helpful assistant.".to_string())
        } else {
            Value::String(instructions.join("\n\n"))
        };

        // Determine `store` — default false unless the request explicitly set it.
        let store = ingress
            .get("store")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut body = serde_json::json!({
            "model": req.model,
            "store": store,
            "stream": true,
            "instructions": instructions_value,
            "input": input,
        });
        let obj = body.as_object_mut().unwrap();

        if let Some(t) = req.generation.temperature {
            obj.insert("temperature".into(), t.into());
        }
        if let Some(p) = req.generation.top_p {
            obj.insert("top_p".into(), p.into());
        }
        // `max_output_tokens` round-trips only when the request itself carried
        // it (kept in the ingress bag by the decoder); the codex-compatible
        // egress must not emit it for requests built from other protocols.
        if let Some(v) = ingress.get("max_output_tokens") {
            obj.insert("max_output_tokens".into(), v.clone());
        }

        // ── Tools (function + custom + built-in) ──────────────────────────────
        if let Some(ref tools) = req.tools {
            let mut tools_val: Vec<Value> = Vec::new();
            let mut namespace_indexes: HashMap<&str, usize> = HashMap::new();
            for tool in tools {
                let encoded = encode_tool(tool);
                if let Some(namespace) = tool.namespace.as_deref() {
                    if let Some(index) = namespace_indexes.get(namespace).copied() {
                        tools_val[index]["tools"]
                            .as_array_mut()
                            .expect("namespace tools array")
                            .push(encoded);
                    } else {
                        namespace_indexes.insert(namespace, tools_val.len());
                        tools_val.push(serde_json::json!({
                            "type": "namespace",
                            "name": namespace,
                            "tools": [encoded]
                        }));
                    }
                } else {
                    tools_val.push(encoded);
                }
            }
            obj.insert("tools".into(), Value::Array(tools_val));
        }
        if let Some(ref tc) = req.tool_choice {
            obj.insert(
                "tool_choice".into(),
                tool_choice_to_value(tc, req.tools.as_deref()),
            );
        }

        for key in &[
            "background",
            "previous_response_id",
            "include",
            "truncation",
            "metadata",
            "text",
            "reasoning",
            "parallel_tool_calls",
            "service_tier",
            "user",
        ] {
            if let Some(v) = ingress.get(*key) {
                obj.entry(key.to_string()).or_insert_with(|| v.clone());
            }
        }

        if !obj.contains_key("reasoning")
            && let Some(reasoning) = reasoning_to_value(&req.reasoning, req.generation.max_tokens)
        {
            obj.insert("reasoning".into(), reasoning);
        }

        // Passthrough remaining unknown extra fields.
        // Skip cross-protocol internal keys (e.g. __anthropic_*, __google_*)
        // that are only meaningful to their respective codecs.
        for (k, v) in ingress {
            if SKIP_FROM_EXTRA.contains(&k.as_str())
                || k.starts_with("__anthropic_")
                || k.starts_with("__google_")
            {
                continue;
            }
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }

        Ok((body, HeaderMap::new()))
    }

    fn egress_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/responses".to_string()
    }
}

/// Emit one `*_tool_call_output` item per `ToolResult` block of a message.
/// Returns `true` when the message was consumed as tool results (the caller
/// should not also emit it as a plain message item).
fn push_tool_result_items(
    input: &mut Vec<Value>,
    message: &crate::protocol::ir::Message,
    default_call_id: &str,
    custom_from_meta: bool,
    call_kinds: &HashMap<&str, ToolCallKind>,
) -> bool {
    let MessageContent::Blocks(blocks) = &message.content else {
        return false;
    };
    let results: Vec<&ContentBlock> = blocks
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .collect();
    if results.is_empty() {
        return false;
    }
    for block in results {
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        else {
            continue;
        };
        let block_call_id = if tool_use_id.trim().is_empty() {
            default_call_id.to_string()
        } else {
            tool_use_id.clone()
        };
        let item_type = if custom_from_meta
            || call_kinds.get(block_call_id.as_str()) == Some(&ToolCallKind::Custom)
        {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        let output = match content {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        input.push(serde_json::json!({
            "type": item_type,
            "call_id": block_call_id,
            "output": output,
        }));
    }
    true
}

fn reasoning_to_value(reasoning: &ReasoningConfig, max_tokens: Option<u32>) -> Option<Value> {
    let mut value = serde_json::Map::new();

    if let Some(effort) = effective_openai_effort(reasoning, max_tokens)
        .as_ref()
        .and_then(reasoning_effort_name)
    {
        value.insert("effort".into(), Value::String(effort.into()));
    }

    if let Some(summary) = &reasoning.display {
        value.insert("summary".into(), Value::String(summary.clone()));
    }

    (!value.is_empty()).then_some(Value::Object(value))
}

fn tool_choice_to_value(tc: &ToolChoice, tools: Option<&[ToolSpec]>) -> Value {
    match tc {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name, namespace } => {
            let tool_type = if tools
                .and_then(|tools| {
                    tools.iter().find(|tool| {
                        tool.name == *name && tool.namespace.as_ref() == namespace.as_ref()
                    })
                })
                .is_some_and(ToolSpec::is_custom)
            {
                "custom"
            } else {
                "function"
            };
            let mut value = serde_json::json!({"type": tool_type, "name": name});
            insert_optional_namespace(&mut value, namespace.as_deref());
            value
        }
        ToolChoice::Raw(v) => v.clone(),
    }
}

fn encode_tool(tool: &ToolSpec) -> Value {
    if tool.name.starts_with("__builtin__") {
        return tool.parameters.clone();
    }

    match &tool.kind {
        ToolSpecKind::Function => {
            let mut value = serde_json::json!({
                "type": "function",
                "name": tool.name,
                "parameters": tool.parameters,
            });
            let fields = value.as_object_mut().expect("function tool object");
            if let Some(description) = &tool.description {
                fields.insert("description".into(), Value::String(description.clone()));
            }
            if let Some(strict) = tool.strict {
                fields.insert("strict".into(), Value::Bool(strict));
            }
            value
        }
        ToolSpecKind::Custom { format } => {
            let mut value = serde_json::json!({
                "type": "custom",
                "name": tool.name,
            });
            let fields = value.as_object_mut().expect("custom tool object");
            if let Some(description) = &tool.description {
                fields.insert("description".into(), Value::String(description.clone()));
            }
            if let Some(format) = format {
                fields.insert("format".into(), format.clone());
            }
            value
        }
    }
}

fn insert_optional_namespace(value: &mut Value, namespace: Option<&str>) {
    if let Some(namespace) = namespace {
        value
            .as_object_mut()
            .expect("tool value object")
            .insert("namespace".into(), Value::String(namespace.to_string()));
    }
}
