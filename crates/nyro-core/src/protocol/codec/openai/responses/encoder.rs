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
    ReasoningConfig, Role, ToolCallKind, ToolChoice, ToolSpec, ToolSpecKind,
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
                            let item = match tool_call.kind {
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

        // ── Tools (function + custom + built-in) ──────────────────────────────
        if let Some(ref tools) = req.tools {
            let tools_val: Vec<Value> = tools
                .iter()
                .map(|t| {
                    if t.name.starts_with("__builtin__") {
                        t.parameters.clone()
                    } else {
                        match &t.kind {
                            ToolSpecKind::Function => {
                                let mut tool = serde_json::json!({
                                    "type": "function",
                                    "name": t.name,
                                    "parameters": t.parameters,
                                });
                                let fields = tool.as_object_mut().expect("function tool object");
                                if let Some(description) = &t.description {
                                    fields.insert(
                                        "description".into(),
                                        Value::String(description.clone()),
                                    );
                                }
                                if let Some(strict) = t.strict {
                                    fields.insert("strict".into(), Value::Bool(strict));
                                }
                                tool
                            }
                            ToolSpecKind::Custom { format } => {
                                let mut tool = serde_json::json!({
                                    "type": "custom",
                                    "name": t.name,
                                });
                                if let Some(description) = &t.description {
                                    tool.as_object_mut().expect("custom tool object").insert(
                                        "description".into(),
                                        Value::String(description.clone()),
                                    );
                                }
                                if let Some(format) = format {
                                    tool.as_object_mut()
                                        .expect("custom tool object")
                                        .insert("format".into(), format.clone());
                                }
                                tool
                            }
                        }
                    }
                })
                .collect();
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
        ToolChoice::Named { name } => {
            let tool_type = if tools
                .and_then(|tools| tools.iter().find(|tool| tool.name == *name))
                .is_some_and(ToolSpec::is_custom)
            {
                "custom"
            } else {
                "function"
            };
            serde_json::json!({"type": tool_type, "name": name})
        }
        ToolChoice::Raw(v) => v.clone(),
    }
}
