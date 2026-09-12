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
        let mut tool_names = super::tool_names::ToolNames::default();
        for (position, msg) in req.messages.iter().enumerate() {
            if msg.role == Role::System {
                continue;
            }
            if req.meta.raw_wire_preview {
                // Only the selected raw-wire engine may reject its request.
                let _ = tool_names.observe(msg, position);
            } else {
                if msg
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("__nyro_synthetic_tool_call"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    return Err(crate::error::GatewayError::BadRequest {
                        code: "gemini_unmatched_tool_result",
                        msg: format!("messages[{position}]: cannot recover a real function name for an orphan tool result"),
                    }.into());
                }
                tool_names.observe(msg, position)?;
            }
            contents.push(encode_content(
                msg,
                &mut tool_names,
                position,
                req.meta.raw_wire_preview,
                req.meta.source_protocol
                    == Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA),
            )?);
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
            // Same-protocol passthrough still crosses the schema boundary:
            // sanitize declarations exactly like the IR path below so a
            // client-side converter's OpenAPI-isms cannot 400 the upstream.
            obj.insert(
                "tools".into(),
                prepare_raw_tools(raw, req.meta.raw_wire_preview)?,
            );
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
                        d.insert(
                            "parameters".into(),
                            prepare_parameters(&t.parameters, req.meta.raw_wire_preview)?,
                        );
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

/// Schema keywords the Gemini `Schema` proto rejects (unknown fields surface as
/// `400 INVALID_ARGUMENT "Request contains an invalid argument."` on the
/// subscription/`v1internal` surface). Sourced from OpenAPI-style tool schemas
/// (MCP servers are the main producer) plus cross-checked against the cleanup
/// lists maintained by CLIProxyAPI (`util.CleanJSONSchemaForGemini`) and
/// gcli2api (`antigravity_fix._clean_parameters_json_schema`).
///
/// Kept (supported by the `google.genai.Schema` subset): `type`, `description`,
/// `nullable`, `enum`, `items`, `required`, `properties`, `minimum`, `maximum`,
/// `minItems`, `maxItems`, `minProperties`, `maxProperties`, `pattern`,
/// `default`, `title`, `anyOf`, `propertyOrdering`.
const GEMINI_UNSUPPORTED_SCHEMA_KEYS: &[&str] = &[
    // JSON-Schema dialect furniture.
    "$schema",
    "$id",
    "$comment",
    "$ref",
    "ref",
    "definitions",
    "$defs",
    "additionalProperties",
    "additionalItems",
    "patternProperties",
    "propertyNames",
    "dependentSchemas",
    "dependentRequired",
    "unevaluatedProperties",
    "unevaluatedItems",
    // Validation constraints with no Gemini counterpart.
    "format",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    // Composition/conditionals the endpoint cannot evaluate.
    "allOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    // Metadata/examples.
    "examples",
    "example",
    "deprecated",
    "contentEncoding",
    "contentMediaType",
    "discriminator",
    "readOnly",
    "writeOnly",
    "xml",
    "externalDocs",
];

/// Keys whose value is a *name map* (property name → sub-schema). Keys inside
/// such a map are chosen by the tool author, so a property literally named
/// `format` must survive even though the same word is a stripped keyword.
const SCHEMA_NAME_MAP_KEYS: &[&str] = &["properties", "patternProperties", "definitions", "$defs"];

/// Strip Gemini-unsupported schema keywords anywhere in the tree, recursing
/// through objects/arrays and the values of name maps, while preserving keys
/// that only *look* like keywords because a tool author named a property that
/// way.
fn sanitize_gemini_schema(value: &Value) -> Value {
    sanitize_schema_node(value, false)
}

fn prepare_parameters(value: &Value, preview: bool) -> Result<Value> {
    if preview {
        // Diagnostic/vendor-diff material only, never the selected native wire.
        return Ok(value.clone());
    }
    let lowered = super::schema::lower_parameters(value).map_err(|error| {
        crate::error::GatewayError::ProtocolLossyRejected {
            lost: vec![format!("tools.parameters{}: {}", error.path, error.reason)],
        }
    })?;
    Ok(sanitize_gemini_schema(&lowered))
}

fn prepare_raw_tools(value: &Value, preview: bool) -> Result<Value> {
    let mut tools = value.clone();
    if let Some(entries) = tools.as_array_mut() {
        for tool in entries {
            if let Some(declarations) = tool
                .get_mut("functionDeclarations")
                .and_then(Value::as_array_mut)
            {
                for declaration in declarations {
                    if let Some(fields) = declaration.as_object_mut() {
                        if fields.contains_key("parameters")
                            && fields.contains_key("parametersJsonSchema")
                            && !preview
                        {
                            return Err(crate::error::GatewayError::BadRequest {
                                code: "gemini_schema_channel_conflict",
                                msg: "Tool declaration cannot contain both parameters and parametersJsonSchema".into(),
                            }.into());
                        }
                        // An explicitly supplied rich schema is already a
                        // native wire choice. Don't apply Schema-proto cleanup
                        // to its definitions, defaults, or property names.
                        if let Some(parameters) = fields.get_mut("parameters") {
                            *parameters = prepare_parameters(parameters, preview)?;
                        }
                    }
                }
            }
        }
    }
    Ok(tools)
}

fn sanitize_schema_node(value: &Value, in_name_map: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if in_name_map {
                    // Property name, never a keyword: keep and recurse into
                    // its schema value.
                    out.insert(k.clone(), sanitize_schema_node(v, false));
                    continue;
                }
                if GEMINI_UNSUPPORTED_SCHEMA_KEYS.contains(&k.as_str()) || k.starts_with("x-") {
                    continue;
                }
                let child_is_name_map = SCHEMA_NAME_MAP_KEYS.contains(&k.as_str()) && v.is_object();
                let schema_child = child_is_name_map || matches!(k.as_str(), "items" | "anyOf");
                out.insert(
                    k.clone(),
                    if schema_child {
                        sanitize_schema_node(v, child_is_name_map)
                    } else {
                        // enum/default values are literal data, not schema nodes.
                        v.clone()
                    },
                );
            }
            Value::Object(out)
        }
        // Array elements (e.g. `items`, `anyOf` branches, `required` names)
        // inherit the surrounding name-map context only positionally; entries
        // themselves are plain schema nodes, so the flag does not apply to the
        // array's own keys — only nested objects matter.
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| sanitize_schema_node(v, in_name_map))
                .collect(),
        ),
        _ => value.clone(),
    }
}

// ── Content encoding ──────────────────────────────────────────────────────────

fn encode_content(
    msg: &Message,
    names: &mut super::tool_names::ToolNames,
    position: usize,
    preview: bool,
    name_only: bool,
) -> Result<Value> {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "model",
        Role::System => unreachable!("system handled separately"),
    };

    let parts = match &msg.content {
        MessageContent::Text(t) => {
            if let Some(id) = msg.tool_call_id.as_deref() {
                let (id, name) = names.resolve(id, position, preview, name_only)?;
                vec![serde_json::json!({
                    "functionResponse": {
                        "id": id,
                        "name": name,
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
                        .push(serde_json::json!({"functionCall": {"id": tc.id, "name": tc.name, "args": args}}));
                }
                parts
            } else {
                vec![serde_json::json!({"text": t})]
            }
        }
        MessageContent::Blocks(blocks) => {
            blocks
                .iter()
                .map(|block| {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        // The normalizer may have correlated a name-only Gemini
                        // result to its real ID; prefer that message-level ID for
                        // a single-result message, but not for multiple blocks.
                        let id = if blocks.len() == 1 {
                            msg.tool_call_id.as_deref().unwrap_or(tool_use_id)
                        } else {
                            tool_use_id
                        };
                        let (id, name) = names.resolve(id, position, preview, name_only)?;
                        return Ok(serde_json::json!({"functionResponse": {
                            "id": id, "name": name, "response": content
                        }}));
                    }
                    Ok(encode_content_block_for_gemini(block))
                })
                .collect::<Result<Vec<_>>>()?
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
        ContentBlock::ToolUse {
            id, name, input, ..
        }
        | ContentBlock::ServerToolUse {
            id, name, input, ..
        } => {
            serde_json::json!({"functionCall": {"id": id, "name": name, "args": input}})
        }
        ContentBlock::Thinking { thinking, .. } => serde_json::json!({"text": thinking}),
        ContentBlock::Unknown { raw } => raw.clone(),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the production 400 INVALID_ARGUMENT on the Google
    /// subscription (`v1internal`) surface: an MCP tool schema carrying the
    /// OpenAPI `"format": "int32"` keyword reached the upstream verbatim and
    /// the endpoint rejected the unknown field.
    #[test]
    fn sanitizes_openapi_format_keyword() {
        // Verbatim shape from the failing request log (request
        // ebf53944-0168-4473-b6a2-e86c7475f36c): mcp__web-reader__webReader.
        let schema = serde_json::json!({
            "properties": {
                "timeout": {
                    "description": "Request timeout(unit is second), default is 20",
                    "format": "int32",
                    "type": "integer"
                },
                "url": {
                    "description": "The URL of the website to fetch and read",
                    "type": "string"
                }
            },
            "required": ["url"],
            "type": "object"
        });

        let out = sanitize_gemini_schema(&schema);
        assert!(
            out.to_string().find("\"format\"").is_none(),
            "format keyword must be stripped, got {out}"
        );
        assert_eq!(out["properties"]["timeout"]["type"], "integer");
        assert_eq!(out["properties"]["url"]["type"], "string");
        assert_eq!(out["required"], serde_json::json!(["url"]));
    }

    #[test]
    fn property_named_like_a_keyword_survives() {
        // A tool author may legitimately name a parameter `format`; only the
        // schema keyword layer may be stripped, never the property itself.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "format": {"type": "string", "description": "output format"},
                "example": {"type": "string"},
                "default": {"type": "boolean"}
            },
            "required": ["format", "default"]
        });

        let out = sanitize_gemini_schema(&schema);
        assert_eq!(out["properties"]["format"]["type"], "string");
        assert_eq!(out["properties"]["example"]["type"], "string");
        assert_eq!(out["properties"]["default"]["type"], "boolean");
        assert_eq!(out["required"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn strips_unsupported_keywords_at_every_depth() {
        let schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "count": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 99,
                                "exclusiveMinimum": 0,
                                "pattern": "^[0-9]+$",
                                "default": 1,
                                "x-google-hint": "keep me out"
                            }
                        }
                    }
                },
                "mode": {
                    "anyOf": [
                        {"type": "string", "enum": ["fast", "slow"]},
                        {"type": "null"}
                    ]
                }
            }
        });

        let out = sanitize_gemini_schema(&schema);
        let text = out.to_string();
        for absent in [
            "$schema",
            "additionalProperties",
            "exclusiveMinimum",
            "x-google-hint",
        ] {
            assert!(
                text.find(absent).is_none(),
                "`{absent}` must be stripped: {text}"
            );
        }
        // Supported constraints and unions survive.
        let count = &out["properties"]["items"]["items"]["properties"]["count"];
        assert_eq!(count["minimum"], 1);
        assert_eq!(count["maximum"], 99);
        assert_eq!(count["pattern"], "^[0-9]+$");
        assert_eq!(count["default"], 1);
        assert_eq!(
            out["properties"]["mode"]["anyOf"].as_array().map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn required_and_enum_values_are_untouched() {
        // Array members are values (names, enum members), not keyword maps —
        // a member that happens to spell a keyword must never be dropped.
        let schema = serde_json::json!({
            "type": "string",
            "enum": ["format", "default", "pattern"],
            "examples": ["format"]
        });

        let out = sanitize_gemini_schema(&schema);
        assert_eq!(
            out["enum"],
            serde_json::json!(["format", "default", "pattern"])
        );
        assert!(out.get("examples").is_none());
    }

    #[test]
    fn raw_tools_envelope_survives_sanitization() {
        // The same-protocol passthrough runs the whole tools array through the
        // sanitizer; the Gemini envelope (builtin tools included) must pass
        // through structurally intact.
        let tools = serde_json::json!([
            {
                "functionDeclarations": [
                    {
                        "name": "mcp__web-reader__webReader",
                        "description": "Fetch and Convert URL",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "timeout": {"type": "integer", "format": "int32"}
                            }
                        }
                    }
                ]
            },
            {"googleSearch": {}}
        ]);

        let out = prepare_raw_tools(&tools, false).expect("raw tool schemas");
        let text = out.to_string();
        assert!(text.find("\"format\"").is_none(), "format leaked: {text}");
        assert_eq!(
            out[0]["functionDeclarations"][0]["name"],
            "mcp__web-reader__webReader"
        );
        assert_eq!(
            out[0]["functionDeclarations"][0]["parameters"]["properties"]["timeout"]["type"],
            "integer"
        );
        assert_eq!(out[1]["googleSearch"], serde_json::json!({}));
    }
}
