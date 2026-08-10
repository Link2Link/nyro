use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::RequestEncoder;
use crate::protocol::codec::reasoning::google_thinking_level;
use crate::protocol::ir::AiRequest;
use crate::protocol::ir::request::{
    ContentBlock, MediaSource, Message, MessageContent, ReasoningConfig, ReasoningEffort,
    ResponseFormat, Role, ToolChoice,
};

pub struct GoogleEncoder;

impl RequestEncoder for GoogleEncoder {
    fn encode_request(&self, req: &AiRequest) -> Result<(Value, HeaderMap)> {
        let ingress = &req.meta.vendor.ingress;

        // ── System instruction ────────────────────────────────────────────────
        let system_val: Option<Value> =
            if let Some(v) = ingress.get("__google_raw_system_instruction") {
                Some(v.clone())
            } else {
                let mut system_parts: Vec<Value> = Vec::new();
                for msg in &req.messages {
                    if msg.role == Role::System {
                        system_parts.push(serde_json::json!({"text": msg.content.to_text()}));
                    }
                }
                if system_parts.is_empty() {
                    None
                } else {
                    Some(serde_json::json!({"parts": system_parts}))
                }
            };

        // ── Contents ─────────────────────────────────────────────────────────
        let mut contents: Vec<Value> = Vec::new();
        for msg in &req.messages {
            if msg.role == Role::System {
                continue;
            }
            contents.push(encode_content(msg)?);
        }

        let mut body = serde_json::json!({ "contents": contents });
        let obj = body.as_object_mut().unwrap();

        if let Some(sv) = system_val {
            obj.insert("systemInstruction".into(), sv);
        }

        // ── generationConfig ──────────────────────────────────────────────────
        let mut gen_config: serde_json::Map<String, Value> =
            if let Some(Value::Object(m)) = ingress.get("__google_generation_config") {
                m.clone()
            } else {
                serde_json::Map::new()
            };

        if let Some(t) = req.generation.temperature {
            gen_config.insert("temperature".into(), t.into());
        }
        if let Some(m) = req.generation.max_tokens {
            gen_config.insert("maxOutputTokens".into(), m.into());
        }
        if let Some(p) = req.generation.top_p {
            gen_config.insert("topP".into(), p.into());
        }
        if !gen_config.contains_key("thinkingConfig")
            && let Some(thinking_config) = google_reasoning_config(&req.reasoning)
        {
            gen_config.insert("thinkingConfig".into(), thinking_config);
        }

        // ── Structured output: IR `response_format` → `responseMimeType` /
        //    `responseSchema` (cross-provider bridge; same-protocol raw values
        //    already present in the ingress bag win).
        if !gen_config.contains_key("responseMimeType")
            && let Some(rf) = &req.response_format
        {
            match rf {
                ResponseFormat::JsonSchema { schema, .. } => {
                    gen_config.insert("responseMimeType".into(), "application/json".into());
                    if !gen_config.contains_key("responseSchema") {
                        gen_config.insert("responseSchema".into(), schema.clone());
                    }
                }
                ResponseFormat::JsonObject => {
                    gen_config.insert("responseMimeType".into(), "application/json".into());
                }
                ResponseFormat::Text => {}
            }
        }

        if !gen_config.is_empty() {
            obj.insert("generationConfig".into(), Value::Object(gen_config));
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        if let Some(raw) = ingress.get("__google_raw_tools") {
            obj.insert("tools".into(), raw.clone());
        } else if let Some(ref tools) = req.tools {
            let mut fn_decls: Vec<Value> = Vec::new();
            let mut builtin_entries: Vec<Value> = Vec::new();

            for t in tools {
                match t.name.as_str() {
                    "__builtin__google_search" => {
                        builtin_entries.push(serde_json::json!({"googleSearch": {}}));
                    }
                    "__builtin__code_execution" => {
                        builtin_entries.push(serde_json::json!({"codeExecution": {}}));
                    }
                    "__builtin__google_search_retrieval" => {
                        builtin_entries.push(serde_json::json!({"googleSearchRetrieval": {}}));
                    }
                    _ => {
                        let mut decl = serde_json::json!({"name": t.name});
                        let d = decl.as_object_mut().unwrap();
                        if let Some(ref desc) = t.description {
                            d.insert("description".into(), Value::String(desc.clone()));
                        }
                        d.insert("parameters".into(), sanitize_gemini_schema(&t.parameters));
                        fn_decls.push(decl);
                    }
                }
            }

            let mut tool_array: Vec<Value> = Vec::new();
            if !fn_decls.is_empty() {
                tool_array.push(serde_json::json!({"functionDeclarations": fn_decls}));
            }
            tool_array.extend(builtin_entries);

            if !tool_array.is_empty() {
                obj.insert("tools".into(), Value::Array(tool_array));
            }
        }

        // ── Extra passthrough fields ───────────────────────────────────────────
        if let Some(v) = ingress.get("__google_tool_config") {
            obj.insert("toolConfig".into(), v.clone());
        }
        if let Some(v) = ingress.get("__google_safety_settings") {
            obj.insert("safetySettings".into(), v.clone());
        }
        if let Some(v) = ingress.get("__google_cached_content") {
            obj.insert("cachedContent".into(), v.clone());
        }

        // ── Tool choice: IR `tool_choice` → `toolConfig.functionCallingConfig`
        //    (cross-provider bridge; a same-protocol raw `__google_tool_config`
        //    already takes precedence above).
        if !obj.contains_key("toolConfig")
            && let Some(tc) = &req.tool_choice
        {
            let fcc: Option<Value> = match tc {
                ToolChoice::Auto => Some(serde_json::json!({"mode": "AUTO"})),
                ToolChoice::Required => Some(serde_json::json!({"mode": "ANY"})),
                ToolChoice::None => Some(serde_json::json!({"mode": "NONE"})),
                ToolChoice::Named { name, .. } => Some(serde_json::json!({
                    "mode": "ANY",
                    "allowed_function_names": [name]
                })),
                ToolChoice::Raw(v) => match v.as_str() {
                    Some("auto") => Some(serde_json::json!({"mode": "AUTO"})),
                    Some("any") | Some("required") => Some(serde_json::json!({"mode": "ANY"})),
                    Some("none") => Some(serde_json::json!({"mode": "NONE"})),
                    _ => None,
                },
            };
            if let Some(fcc) = fcc {
                obj.insert(
                    "toolConfig".into(),
                    serde_json::json!({"functionCallingConfig": fcc}),
                );
            }
        }

        Ok((body, HeaderMap::new()))
    }

    fn egress_path(&self, model: &str, stream: bool) -> String {
        if stream {
            format!("/v1beta/models/{}:streamGenerateContent?alt=sse", model)
        } else {
            format!("/v1beta/models/{}:generateContent", model)
        }
    }
}

fn google_reasoning_config(reasoning: &ReasoningConfig) -> Option<Value> {
    let budget = reasoning.budget_tokens.or(match reasoning.effort.as_ref() {
        Some(ReasoningEffort::Budget(tokens)) => Some(*tokens),
        _ => None,
    });
    let level = reasoning.effort.as_ref().and_then(google_thinking_level);

    match (budget, level) {
        // Keep both dimensions when the IR carries both: llm-bridge writes
        // `thinkingBudget` + `thinkingLevel` together.
        (Some(tokens), Some(level)) => Some(serde_json::json!({
            "thinkingBudget": tokens,
            "thinkingLevel": level
        })),
        (Some(tokens), None) => Some(serde_json::json!({"thinkingBudget": tokens})),
        (None, Some(level)) => Some(serde_json::json!({"thinkingLevel": level})),
        (None, None) => {
            if matches!(reasoning.effort.as_ref(), Some(ReasoningEffort::None)) {
                Some(serde_json::json!({"thinkingBudget": 0}))
            } else {
                reasoning
                    .enabled
                    .then_some(Value::Object(serde_json::Map::new()))
            }
        }
    }
}

// ── Schema sanitisation ───────────────────────────────────────────────────────

fn sanitize_gemini_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema" | "additionalProperties" | "$ref" | "ref" | "definitions" | "$defs"
                ) {
                    continue;
                }
                out.insert(k.clone(), sanitize_gemini_schema(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_gemini_schema).collect()),
        _ => value.clone(),
    }
}

// ── Content encoding ──────────────────────────────────────────────────────────

fn encode_content(msg: &Message) -> Result<Value> {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "model",
        Role::System => unreachable!("system handled separately"),
    };

    let parts = match &msg.content {
        MessageContent::Text(t) => {
            if msg.tool_call_id.is_some() {
                vec![serde_json::json!({
                    "functionResponse": {
                        "name": msg.tool_call_id,
                        "response": {"result": t}
                    }
                })]
            } else if let Some(ref tcs) = msg.tool_calls {
                let mut parts = Vec::new();
                if !t.is_empty() {
                    parts.push(serde_json::json!({"text": t}));
                }
                for tc in tcs {
                    let args: Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or(Value::Object(Default::default()));
                    parts
                        .push(serde_json::json!({"functionCall": {"name": tc.name, "args": args}}));
                }
                parts
            } else {
                vec![serde_json::json!({"text": t})]
            }
        }
        MessageContent::Blocks(blocks) => {
            blocks.iter().map(encode_content_block_for_gemini).collect()
        }
    };

    Ok(serde_json::json!({"role": role, "parts": parts}))
}

fn encode_content_block_for_gemini(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text, .. } => serde_json::json!({"text": text}),
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Base64 { media_type, data } => serde_json::json!({
                "inlineData": {
                    "mimeType": media_type,
                    "data": data,
                }
            }),
            MediaSource::Url(url) => serde_json::json!({"fileData": {"fileUri": url}}),
            MediaSource::FileId { file_id, .. } => {
                serde_json::json!({"fileData": {"fileUri": file_id}})
            }
        },
        ContentBlock::File { source, media_type } => match source {
            MediaSource::Url(url) => {
                let mut fd = serde_json::json!({
                    "fileData": {
                        "fileUri": url,
                    }
                });
                if let Some(mt) = media_type {
                    fd["fileData"]["mimeType"] = serde_json::Value::String(mt.clone());
                }
                fd
            }
            MediaSource::FileId { file_id, .. } => {
                let mut fd = serde_json::json!({
                    "fileData": {
                        "fileUri": file_id,
                    }
                });
                if let Some(mt) = media_type {
                    fd["fileData"]["mimeType"] = serde_json::Value::String(mt.clone());
                }
                fd
            }
            MediaSource::Base64 {
                media_type: b64_mime,
                data,
            } => {
                let mut fd = serde_json::json!({
                    "inlineData": {
                        "mimeType": b64_mime,
                        "data": data,
                    }
                });
                if let Some(mt) = media_type {
                    fd["inlineData"]["mimeType"] = serde_json::Value::String(mt.clone());
                }
                fd
            }
        },
        ContentBlock::ToolUse { name, input, .. } => {
            serde_json::json!({"functionCall": {"name": name, "args": input}})
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            serde_json::json!({
                "functionResponse": {"name": tool_use_id, "response": content}
            })
        }
        ContentBlock::Thinking { thinking, .. } => serde_json::json!({"text": thinking}),
        ContentBlock::Unknown { raw } => raw.clone(),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}
