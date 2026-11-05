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
    let encoded = encode_empty_enum_strings(&lowered);
    Ok(sanitize_gemini_schema(&encoded))
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

/// Google's native Schema rejects empty strings inside `enum` outright
/// (`INVALID_ARGUMENT ... enum[i]: cannot be empty`, observed on the
/// subscription `v1internal` surface). Encode such members losslessly with
/// `pattern: "^$"` instead: as a sibling `anyOf` branch next to the
/// remaining non-empty values, or as the whole constraint when the empty
/// string is the only value. Schemas that already carry `anyOf`, an
/// empty-only `pattern`, or non-string enum members stay exactly as
/// lowered rather than being guessed at. Traversal mirrors the cleanup's
/// schema positions; literal payloads (`default`, `examples`, `required`
/// names, `enum` members) are never rewritten.
fn encode_empty_enum_strings(value: &Value) -> Value {
    encode_empty_enum_node(value, false)
}

fn encode_empty_enum_node(value: &Value, in_name_map: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                if in_name_map {
                    out.insert(key.clone(), encode_empty_enum_node(child, false));
                    continue;
                }
                let child_is_name_map =
                    SCHEMA_NAME_MAP_KEYS.contains(&key.as_str()) && child.is_object();
                let schema_child = child_is_name_map || matches!(key.as_str(), "items" | "anyOf");
                let rewritten = if schema_child {
                    encode_empty_enum_node(child, child_is_name_map)
                } else {
                    child.clone()
                };
                out.insert(key.clone(), rewritten);
            }
            if in_name_map {
                return Value::Object(out);
            }
            split_empty_enum_member(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| encode_empty_enum_node(v, in_name_map))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Replace this schema node's empty-string `enum` members with the
/// pattern-based encoding. Returns the map unchanged when the rewrite
/// would not be provably equivalent.
fn split_empty_enum_member(mut map: serde_json::Map<String, Value>) -> Value {
    let Some(Value::Array(members)) = map.get("enum") else {
        return Value::Object(map);
    };
    if !members.iter().all(Value::is_string)
        || !members.iter().any(|member| member.as_str() == Some(""))
    {
        return Value::Object(map);
    }
    let mut non_empty = Vec::new();
    for member in members {
        if member.as_str() != Some("") {
            non_empty.push(member.clone());
        }
    }
    if !non_empty.is_empty() {
        // An existing anyOf is another disjunct of this schema, never a
        // field that may be overwritten; keep such nodes exactly as lowered.
        if map.contains_key("anyOf") {
            return Value::Object(map);
        }
        map.remove("enum");
        map.insert(
            "anyOf".into(),
            serde_json::json!([
                {"type": "string", "enum": non_empty},
                {"type": "string", "pattern": "^$"}
            ]),
        );
        return Value::Object(map);
    }
    // RE2 cannot conjoin two patterns into one, so a schema that already
    // constrains with `pattern` keeps its empty-only enum verbatim and the
    // upstream judgement stays with the caller's own composition.
    if map.contains_key("pattern") {
        return Value::Object(map);
    }
    map.remove("enum");
    map.entry("type".to_string())
        .or_insert_with(|| Value::String("string".into()));
    map.insert("pattern".into(), Value::String("^$".into()));
    Value::Object(map)
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

    /// Production shape from request 109a83f1-51ae-446f-8399-fee2eb682a48:
    /// after the v2.1.29 finite-enum merge Google rejected
    /// `permission.enum[3]: cannot be empty` (400 INVALID_ARGUMENT on the
    /// `v1internal` subscription surface). The empty member must travel as
    /// a `pattern: "^$"` anyOf branch instead.
    #[test]
    fn empty_enum_member_becomes_a_pattern_any_of_branch() {
        let lowered = serde_json::json!({
            "type": "object",
            "properties": {"permission": {
                "type": "string",
                "description": "New permission preset; an empty string clears it back to the session default.",
                "default": "",
                "enum": ["read-only", "workspace-write", "danger-full-access", ""]
            }},
            "required": ["permission"]
        });
        let out = encode_empty_enum_strings(&lowered);
        let permission = &out["properties"]["permission"];
        assert_eq!(
            permission["description"],
            lowered["properties"]["permission"]["description"]
        );
        assert_eq!(permission["default"], "");
        assert_eq!(permission["type"], "string");
        assert!(
            permission.get("enum").is_none(),
            "empty member must leave the enum: {permission}"
        );
        assert_eq!(
            permission["anyOf"],
            serde_json::json!([
                {"type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"]},
                {"type": "string", "pattern": "^$"}
            ])
        );
        assert_eq!(
            out["required"],
            serde_json::json!(["permission"]),
            "siblings are untouched"
        );
        // The rewritten form carries no empty member, so re-running is a no-op.
        assert_eq!(encode_empty_enum_strings(&out), out);
    }

    #[test]
    fn empty_only_enum_becomes_a_pattern() {
        let out = encode_empty_enum_strings(&serde_json::json!({
            "type": "string",
            "description": "Keeps the current setting.",
            "enum": [""]
        }));
        assert_eq!(
            out,
            serde_json::json!({
                "type": "string",
                "description": "Keeps the current setting.",
                "pattern": "^$"
            })
        );
        // Type is supplied when the schema relied on enum-only typing.
        assert_eq!(
            encode_empty_enum_strings(&serde_json::json!({"enum": ["", ""]})),
            serde_json::json!({"type": "string", "pattern": "^$"})
        );
        // An existing pattern cannot be conjoined with "^$" in RE2; the
        // composition stays verbatim for the upstream to judge.
        let conflicting = serde_json::json!({"type": "string", "pattern": "^a", "enum": [""]});
        assert_eq!(encode_empty_enum_strings(&conflicting), conflicting.clone());
    }

    #[test]
    fn unprovable_or_unrelated_enums_stay_verbatim() {
        // All-non-empty enums are untouched at every rewritten position.
        let clean = serde_json::json!({
            "type": "object",
            "properties": {"mode": {"type": "string", "enum": ["fast", "slow"]}},
            "items_placeholder": {"type": "string"}
        });
        assert_eq!(encode_empty_enum_strings(&clean), clean);
        // Non-string members are not this encoder's call.
        let mixed = serde_json::json!({"type": "string", "enum": ["", 5]});
        assert_eq!(encode_empty_enum_strings(&mixed), mixed.clone());
        // An existing anyOf is a disjunct, never a field to overwrite.
        let union = serde_json::json!({
            "anyOf": [{"type": "string"}],
            "enum": ["a", ""]
        });
        assert_eq!(encode_empty_enum_strings(&union), union.clone());
    }

    #[test]
    fn empty_members_rewrite_inside_nested_schema_positions() {
        let out = encode_empty_enum_strings(&serde_json::json!({
            "type": "object",
            "properties": {"tags": {
                "type": "array",
                "items": {"type": "string", "enum": ["urgent", ""]}
            }},
            "$defs": {"kept": {"type": "string", "enum": ["x", ""]}}
        }));
        let items = &out["properties"]["tags"]["items"];
        assert_eq!(
            items["anyOf"],
            serde_json::json!([
                {"type": "string", "enum": ["urgent"]},
                {"type": "string", "pattern": "^$"}
            ])
        );
        // Definition catalogs follow the same schema positions.
        assert_eq!(
            out["$defs"]["kept"]["anyOf"][1],
            serde_json::json!({"type": "string", "pattern": "^$"})
        );
    }

    #[test]
    fn literal_payloads_are_never_rewritten() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"note": {"type": "string"}},
            "default": {"enum": [""]},
            "examples": [{"enum": [""]}]
        });
        assert_eq!(encode_empty_enum_strings(&schema), schema.clone());
    }

    /// End-to-end native wire shape for the incident: the lowered
    /// permission union must reach the cleanup with no empty enum member.
    #[test]
    fn permission_union_prepares_without_empty_enum_members() {
        let original = serde_json::json!({
            "type": "object",
            "properties": {"permission": {
                "description": "New permission preset; an empty string clears it back to the session default.",
                "oneOf": [
                    {"type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"]},
                    {"type": "string", "const": ""}
                ]
            }},
            "required": ["permission"],
            "additionalProperties": false
        });
        let prepared = prepare_parameters(&original, false).expect("native parameters");
        let permission = &prepared["properties"]["permission"];
        assert_eq!(
            permission["anyOf"],
            serde_json::json!([
                {"type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"]},
                {"type": "string", "pattern": "^$"}
            ]),
            "prepared wire: {prepared}"
        );
        assert_eq!(
            permission["description"],
            original["properties"]["permission"]["description"]
        );
        assert!(permission.get("oneOf").is_none());
        assert!(permission.get("const").is_none());
        assert!(prepared.get("additionalProperties").is_none());
        // No enum anywhere on the wire still carries an empty member.
        fn assert_no_empty_enum(value: &Value) {
            match value {
                Value::Array(arr) => arr.iter().for_each(assert_no_empty_enum),
                Value::Object(map) => {
                    if let Some(Value::Array(members)) = map.get("enum") {
                        assert!(
                            !members.iter().any(|m| m.as_str() == Some("")),
                            "empty enum member on the wire: {value}"
                        );
                    }
                    map.values().for_each(assert_no_empty_enum);
                }
                _ => {}
            }
        }
        assert_no_empty_enum(&prepared);
    }
}
