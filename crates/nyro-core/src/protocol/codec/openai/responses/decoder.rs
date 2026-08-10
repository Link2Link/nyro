//! OpenAI Responses API ingress decoder — produces `AiRequest` directly.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::protocol::RequestDecoder;
use crate::protocol::codec::reasoning::parse_reasoning_effort;
use crate::protocol::ids::OPENAI_RESPONSES_V1;
use crate::protocol::ir::{
    AiRequest, ContentBlock, GenerationConfig, MediaSource, Message, MessageContent,
    OpenAIResponsesExt, ProtocolExt, ReasoningConfig, ReasoningEffort, ResponseFormat, Role,
    StreamConfig, ToolCall, ToolCallKind, ToolChoice, ToolSpec, ToolSpecKind,
};

pub struct ResponsesDecoder;

// Fields decoded into named IR fields (not ingress bag).
const KNOWN_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "max_output_tokens",
    "stream",
    "temperature",
    "top_p",
    "tools",
    "tool_choice",
    "background",
    "previous_response_id",
    "store",
    "include",
    "truncation",
    "metadata",
    "text",
    "reasoning",
    "parallel_tool_calls",
    "service_tier",
    "user",
];

impl RequestDecoder for ResponsesDecoder {
    fn decode_request(&self, body: Value) -> Result<AiRequest> {
        let obj = body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("request body must be a JSON object"))?;

        let model = obj
            .get("model")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'model' field"))?
            .to_string();

        let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        let temperature = obj.get("temperature").and_then(|v| v.as_f64());
        let max_tokens = obj
            .get("max_output_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let top_p = obj.get("top_p").and_then(|v| v.as_f64());
        let parallel_tool_calls = obj.get("parallel_tool_calls").and_then(|v| v.as_bool());

        // ── System (instructions) ─────────────────────────────────────────────
        let mut messages: Vec<Message> = Vec::new();
        let mut additional_tools: Vec<ToolSpec> = Vec::new();

        if let Some(inst) = obj.get("instructions").and_then(|v| v.as_str())
            && !inst.is_empty()
        {
            messages.push(Message {
                role: Role::System,
                content: MessageContent::Text(inst.to_string()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            });
        }

        // ── Input items ───────────────────────────────────────────────────────
        let input = obj
            .get("input")
            .ok_or_else(|| anyhow::anyhow!("missing 'input' field"))?;

        match input {
            Value::String(text) => {
                messages.push(Message {
                    role: Role::User,
                    content: MessageContent::Text(text.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                });
            }
            Value::Array(items) => {
                let mut pending_reasoning: Option<String> = None;
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                        let parsed = parse_tools(item.get("tools"))?.unwrap_or_default();
                        merge_tool_specs(&mut additional_tools, parsed)?;
                        continue;
                    }

                    if item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| t == "reasoning")
                    {
                        if let Some(summary) = item.get("summary").and_then(|v| v.as_array()) {
                            for s in summary {
                                if let Some(text) = s.get("text").and_then(|v| v.as_str())
                                    && !text.is_empty()
                                {
                                    pending_reasoning = Some(text.to_string());
                                    break;
                                }
                            }
                        }
                        continue;
                    }

                    if let Some(mut msg) = decode_input_item(item)? {
                        if msg.role == Role::Assistant {
                            if let Some(ref reasoning) = pending_reasoning {
                                let mut obj = match msg.meta.take() {
                                    Some(Value::Object(m)) => m,
                                    _ => serde_json::Map::new(),
                                };
                                obj.insert(
                                    "reasoning_content".to_string(),
                                    Value::String(reasoning.clone()),
                                );
                                msg.meta = Some(Value::Object(obj));
                            }
                        } else if (msg.role == Role::User || msg.role == Role::System)
                            && let Some(reasoning) = pending_reasoning.take()
                        {
                            let mut extra = serde_json::Map::new();
                            extra.insert("reasoning_content".to_string(), Value::String(reasoning));
                            messages.push(Message {
                                role: Role::Assistant,
                                content: MessageContent::Text(String::new()),
                                tool_calls: None,
                                tool_call_id: None,
                                meta: Some(Value::Object(extra)),
                            });
                        }
                        messages.push(msg);
                    }
                }
                if let Some(reasoning) = pending_reasoning.take() {
                    let mut extra = serde_json::Map::new();
                    extra.insert("reasoning_content".to_string(), Value::String(reasoning));
                    messages.push(Message {
                        role: Role::Assistant,
                        content: MessageContent::Text(String::new()),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: Some(Value::Object(extra)),
                    });
                }
            }
            _ => anyhow::bail!("'input' must be a string or array"),
        }

        if messages.is_empty() {
            anyhow::bail!("no messages found in input");
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        let mut tools = parse_tools(obj.get("tools"))?.unwrap_or_default();
        merge_tool_specs(&mut tools, additional_tools)?;
        let tools = (!tools.is_empty()).then_some(tools);
        let tool_choice = obj.get("tool_choice").cloned().map(parse_tool_choice);

        // ── Reasoning ─────────────────────────────────────────────────────────
        let reasoning = if let Some(r) = obj.get("reasoning") {
            let effort = r
                .get("effort")
                .and_then(|e| e.as_str())
                .and_then(parse_reasoning_effort);
            let summary = r.get("summary").and_then(|s| s.as_str()).map(String::from);
            ReasoningConfig {
                enabled: !matches!(effort.as_ref(), Some(ReasoningEffort::None)),
                effort,
                display: summary,
                ..Default::default()
            }
        } else {
            ReasoningConfig::default()
        };

        // ── Structured output via `text.format` ───────────────────────────────
        let response_format = obj.get("text").and_then(|t| t.get("format")).map(|fmt| {
            match fmt.get("type").and_then(Value::as_str) {
                Some("json_schema") => ResponseFormat::JsonSchema {
                    name: fmt
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    schema: fmt.get("schema").cloned().unwrap_or(Value::Null),
                    strict: fmt.get("strict").and_then(Value::as_bool),
                },
                Some("json_object") => ResponseFormat::JsonObject,
                _ => ResponseFormat::Text,
            }
        });

        // ── ProtocolExt ───────────────────────────────────────────────────────
        let resp_ext = OpenAIResponsesExt {
            background: obj.get("background").and_then(|v| v.as_bool()),
            previous_response_id: obj
                .get("previous_response_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            truncation: obj
                .get("truncation")
                .and_then(|v| v.as_str())
                .map(String::from),
            include: obj.get("include").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            }),
            ..Default::default()
        };

        // ── Vendor ingress bag — backward compat for old encoders (pre-PR-3) ──
        let mut ingress: HashMap<String, Value> = obj
            .iter()
            .filter(|(k, _)| !KNOWN_FIELDS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for key in &[
            "background",
            "previous_response_id",
            "store",
            "include",
            "truncation",
            "metadata",
            "text",
            "reasoning",
            "parallel_tool_calls",
            "service_tier",
            "user",
        ] {
            if let Some(v) = obj.get(*key) {
                ingress.entry(key.to_string()).or_insert_with(|| v.clone());
            }
        }
        // Keep the original `max_output_tokens` in the ingress bag so a
        // responses→responses round trip re-emits it, while requests built
        // from other protocols (which must not leak the field into the
        // codex-compatible egress) stay untouched.
        if let Some(v) = obj.get("max_output_tokens") {
            ingress
                .entry("max_output_tokens".to_string())
                .or_insert_with(|| v.clone());
        }

        // ── Build AiRequest ───────────────────────────────────────────────────
        let mut ai_req = AiRequest::new(model, messages);
        ai_req.generation = GenerationConfig {
            temperature,
            max_tokens,
            top_p,
            ..Default::default()
        };
        ai_req.stream = StreamConfig {
            enabled: stream,
            include_usage: false,
        };
        ai_req.tools = tools;
        ai_req.tool_choice = tool_choice;
        ai_req.parallel_tool_calls = parallel_tool_calls;
        ai_req.reasoning = reasoning;
        ai_req.response_format = response_format;
        ai_req.ext = Some(ProtocolExt::OpenAiResponses(resp_ext));
        ai_req.meta.source_protocol = Some(OPENAI_RESPONSES_V1);
        ai_req.meta.vendor.ingress = ingress;

        Ok(ai_req)
    }
}

// ── Input item decoding ───────────────────────────────────────────────────────

fn decode_input_item(item: &Value) -> Result<Option<Message>> {
    let item_type = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");

    match item_type {
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("tool_call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if call_id.trim().is_empty() {
                anyhow::bail!("function_call_output missing call_id");
            }
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            let output_text = match output {
                Value::String(s) => s,
                Value::Null => String::new(),
                other => other.to_string(),
            };
            Ok(Some(Message {
                role: Role::Tool,
                content: MessageContent::Text(output_text),
                tool_calls: None,
                tool_call_id: Some(call_id),
                meta: (item_type == "custom_tool_call_output")
                    .then(|| serde_json::json!({"__nyro_tool_call_kind": "custom"})),
            }))
        }

        "function_call" | "custom_tool_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = item
                .get(if item_type == "custom_tool_call" {
                    "input"
                } else {
                    "arguments"
                })
                .and_then(|v| v.as_str())
                .unwrap_or(if item_type == "custom_tool_call" {
                    ""
                } else {
                    "{}"
                })
                .to_string();
            if call_id.trim().is_empty() || name.trim().is_empty() {
                // Tolerate malformed `function_call` input items instead of
                // rejecting the whole request. Some clients (e.g. Codex
                // Desktop) emit a duplicate item that carries the arguments
                // under a fresh `call_id` but with an empty `name`; skipping
                // matches the streaming parser (see parser.rs) and lets any
                // orphaned `function_call_output` be reconciled downstream by
                // `normalize_request_tool_results`.
                return Ok(None);
            }
            Ok(Some(Message {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall {
                    id: call_id,
                    name,
                    namespace: item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .map(String::from),
                    kind: if item_type == "custom_tool_call" {
                        ToolCallKind::Custom
                    } else {
                        ToolCallKind::Function
                    },
                    arguments,
                }]),
                tool_call_id: None,
                meta: None,
            }))
        }

        "web_search_call" | "file_search_call" | "computer_call" | "reasoning" => Ok(None),

        "message" => decode_message_item(item),

        _ => Ok(None),
    }
}

fn decode_message_item(item: &Value) -> Result<Option<Message>> {
    let role_str = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
    let role = match role_str {
        "system" | "developer" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => anyhow::bail!("unsupported role in responses input: {other}"),
    };

    let content = match item.get("content") {
        Some(Value::String(text)) => MessageContent::Text(text.clone()),
        Some(Value::Array(blocks)) => {
            let mut texts = Vec::new();
            let mut content_blocks: Vec<ContentBlock> = Vec::new();
            for block in blocks {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match block_type {
                    "input_text" | "output_text" | "text" => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            texts.push(text.to_string());
                            content_blocks.push(ContentBlock::Text {
                                text: text.to_string(),
                                cache_control: None,
                            });
                        }
                    }
                    "input_image" => {
                        let url = match block.get("image_url").cloned() {
                            Some(Value::String(s)) => s,
                            Some(Value::Object(o)) => o
                                .get("url")
                                .and_then(|u| u.as_str())
                                .unwrap_or("")
                                .to_string(),
                            _ => String::new(),
                        };
                        if !url.is_empty() {
                            content_blocks.push(ContentBlock::Image {
                                source: MediaSource::Url(url),
                                cache_control: None,
                            });
                        }
                    }
                    _ => {}
                }
            }
            if content_blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
            {
                // Keep multimodal input as blocks; the image must survive.
                MessageContent::Blocks(content_blocks)
            } else {
                let text = texts.join("");
                if text.is_empty() {
                    return Ok(None);
                }
                MessageContent::Text(text)
            }
        }
        Some(_) => anyhow::bail!("unsupported content type in responses input item"),
        None => return Ok(None),
    };

    Ok(Some(Message {
        role,
        content,
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_tools(raw_tools: Option<&Value>) -> Result<Option<Vec<ToolSpec>>> {
    let Some(raw_tools) = raw_tools else {
        return Ok(None);
    };
    let items = raw_tools
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("'tools' must be an array"))?;

    let mut parsed_tools = Vec::new();
    for item in items {
        parse_tool_item(item, None, &mut parsed_tools)?;
    }
    let mut tools = Vec::new();
    merge_tool_specs(&mut tools, parsed_tools)?;

    if tools.is_empty() {
        Ok(None)
    } else {
        Ok(Some(tools))
    }
}

fn parse_tool_item(item: &Value, namespace: Option<&str>, tools: &mut Vec<ToolSpec>) -> Result<()> {
    let tool_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");

    match tool_type {
        "namespace" => {
            if namespace.is_some() {
                anyhow::bail!("nested tool namespaces are not supported");
            }
            let name = required_non_empty_tool_name(item, "namespace")?;
            let children = item
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("namespace '{name}' missing array 'tools' field"))?;
            for child in children {
                parse_tool_item(child, Some(&name), tools)?;
            }
        }
        "function" => {
            let name = required_non_empty_tool_name(item, "function")?;
            tools.push(ToolSpec {
                name,
                namespace: namespace.map(String::from),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
                kind: ToolSpecKind::Function,
                parameters: item
                    .get("parameters")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default())),
                strict: item.get("strict").and_then(Value::as_bool),
                cache_control: None,
                meta: None,
            });
        }
        "custom" => {
            let name = required_non_empty_tool_name(item, "custom")?;
            tools.push(ToolSpec {
                name,
                namespace: namespace.map(String::from),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
                kind: ToolSpecKind::Custom {
                    format: item.get("format").cloned(),
                },
                parameters: Value::Object(Default::default()),
                strict: None,
                cache_control: None,
                meta: None,
            });
        }
        "web_search_preview" | "file_search" | "computer_use_preview" | "code_interpreter" => {
            if let Some(namespace) = namespace {
                anyhow::bail!("unsupported tool type '{tool_type}' in namespace '{namespace}'");
            }
            tools.push(ToolSpec {
                name: format!("__builtin__{tool_type}"),
                namespace: None,
                description: Some(format!("built-in tool: {tool_type}")),
                kind: ToolSpecKind::Function,
                parameters: item.clone(),
                strict: None,
                cache_control: None,
                meta: None,
            });
        }
        // A flat target cannot reproduce hosted discovery. Namespace children are
        // already exposed eagerly, so the marker itself is intentionally omitted.
        "tool_search" if namespace.is_none() => {}
        unsupported if namespace.is_some() => {
            anyhow::bail!(
                "unsupported tool type '{unsupported}' in namespace '{}'",
                namespace.expect("checked above")
            );
        }
        _ => {}
    }

    Ok(())
}

fn required_non_empty_tool_name(item: &Value, tool_type: &str) -> Result<String> {
    item.get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("{tool_type} tool missing non-empty 'name' field"))
}

fn merge_tool_specs(existing: &mut Vec<ToolSpec>, incoming: Vec<ToolSpec>) -> Result<()> {
    for tool in incoming {
        let Some(current) = existing.iter().find(|candidate| {
            candidate.name == tool.name
                && candidate.namespace == tool.namespace
                && candidate.is_custom() == tool.is_custom()
        }) else {
            existing.push(tool);
            continue;
        };

        if serde_json::to_value(current)? != serde_json::to_value(&tool)? {
            let qualified_name = tool
                .namespace
                .as_ref()
                .map(|namespace| format!("{namespace}.{}", tool.name))
                .unwrap_or_else(|| tool.name.clone());
            anyhow::bail!("conflicting tool definitions for '{qualified_name}'");
        }
    }
    Ok(())
}

fn parse_tool_choice(v: Value) -> ToolChoice {
    match &v {
        Value::String(s) => match s.as_str() {
            "none" => ToolChoice::None,
            "auto" => ToolChoice::Auto,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Raw(v),
        },
        Value::Object(obj) => {
            if matches!(
                obj.get("type").and_then(Value::as_str),
                Some("function" | "custom")
            ) && let Some(name) = obj.get("name").and_then(Value::as_str).or_else(|| {
                obj.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            }) {
                return ToolChoice::Named {
                    name: name.to_string(),
                    namespace: obj
                        .get("namespace")
                        .and_then(Value::as_str)
                        .map(String::from),
                };
            }
            ToolChoice::Raw(v)
        }
        _ => ToolChoice::Raw(v),
    }
}
