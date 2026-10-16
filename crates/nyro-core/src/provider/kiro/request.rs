//! Translate the gateway IR into Kiro's `generateAssistantResponse` request.
//!
//! Wire behavior follows AIClient2API's Kiro provider (`claude-kiro.js`),
//! except that this encoder never injects an identity-override system prompt.
//!
//! Kiro's conversation model is strict, so the encoder normalizes the IR:
//! turns strictly alternate, history ends with an assistant turn, the current
//! turn is a user turn with non-empty content, and the tool list is never
//! empty. Constructs the wire cannot carry are rejected, not silently dropped,
//! except client-side sampling knobs (`temperature`, `top_p`, `max_tokens`)
//! that have no Kiro equivalent and that every client sends by default.
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::catalog;
use super::stream::{PLACEHOLDER_TOOL, THINKING_CLOSE, THINKING_OPEN, estimate_tokens};
use crate::error::GatewayError;
use crate::protocol::ir::*;

const MAX_TOOL_NAME: usize = 64;
const MAX_TOOL_DESCRIPTION: usize = 9216;
/// Images older than this many turns before the end are replaced by a note.
const KEEP_IMAGES_WITHIN: usize = 5;
const MIN_THINKING_BUDGET: u32 = 1024;
const MAX_THINKING_BUDGET: u32 = 24_576;
const DEFAULT_THINKING_BUDGET: u32 = 20_000;
/// Flat token cost charged per inline image in the local prompt estimate.
const IMAGE_TOKENS: u32 = 1_600;

/// Everything the transport and response side need from one encoded request.
pub struct Encoded {
    pub body: Vec<u8>,
    /// Kiro-safe tool name -> original client tool name (only renamed tools).
    pub tool_names: HashMap<String, String>,
    /// Whether the in-band `<thinking>` directive was requested.
    pub thinking_tags: bool,
    /// Local prompt-token estimate for when Kiro reports no context usage.
    pub input_estimate: u32,
    /// Upstream model id actually sent.
    pub upstream_model: String,
    /// Content-free request shape for diagnostics.
    pub summary: Value,
}

fn invalid(message: &str) -> GatewayError {
    GatewayError::BadRequest {
        code: "kiro_invalid_request",
        msg: message.into(),
    }
}

fn unsupported(field: &str) -> GatewayError {
    GatewayError::ProtocolLossyRejected {
        lost: vec![field.into()],
    }
}

// ── Turn model ───────────────────────────────────────────────────────────────

struct ToolUse {
    id: String,
    name: String,
    input: Value,
}

struct ToolResult {
    id: String,
    text: String,
    is_error: bool,
}

struct Image {
    format: String,
    data: String,
}

#[derive(Default)]
struct Turn {
    assistant: bool,
    parts: Vec<String>,
    thinking: String,
    images: Vec<Image>,
    tool_uses: Vec<ToolUse>,
    tool_results: Vec<ToolResult>,
    omitted_images: usize,
}

impl Turn {
    fn new(assistant: bool) -> Self {
        Self {
            assistant,
            ..Default::default()
        }
    }

    fn push_text(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.parts.push(text.to_string());
        }
    }

    fn is_empty(&self) -> bool {
        self.parts.is_empty()
            && self.thinking.trim().is_empty()
            && self.images.is_empty()
            && self.tool_uses.is_empty()
            && self.tool_results.is_empty()
    }

    fn merge(&mut self, other: Turn) {
        self.parts.extend(other.parts);
        if !other.thinking.is_empty() {
            if !self.thinking.is_empty() {
                self.thinking.push('\n');
            }
            self.thinking.push_str(&other.thinking);
        }
        self.images.extend(other.images);
        self.tool_uses.extend(other.tool_uses);
        self.tool_results.extend(other.tool_results);
    }

    fn text(&self) -> String {
        self.parts.join("\n")
    }
}

// ── Public entry point ───────────────────────────────────────────────────────

pub fn encode(
    request: &AiRequest,
    actual_model: &str,
    profile_arn: Option<&str>,
) -> Result<Encoded, GatewayError> {
    if actual_model.trim().is_empty() {
        return Err(invalid("Kiro model is required"));
    }
    validate_options(request)?;
    let upstream_model = catalog::resolve(actual_model);

    let (mut turns, mut system) = build_turns(request)?;
    let thinking_prefix = thinking_directive(&request.reasoning);
    if let Some(prefix) = &thinking_prefix
        && !has_thinking_directive(&system)
    {
        system = if system.is_empty() {
            prefix.clone()
        } else {
            format!("{prefix}\n{system}")
        };
    }
    let turns = normalize(&mut turns, &system);
    if turns.is_empty() {
        return Err(invalid("Kiro requires at least one conversation message"));
    }

    let (tools, tool_names) = render_tools(request, &turns)?;
    let has_tool_history = turns
        .iter()
        .any(|turn| !turn.tool_uses.is_empty() || !turn.tool_results.is_empty());
    let _ = has_tool_history;

    let last = turns.len() - 1;
    let mut history = Vec::with_capacity(last);
    for (index, turn) in turns[..last].iter().enumerate() {
        let distance = last - index;
        history.push(render_history_turn(
            turn,
            &upstream_model,
            distance <= KEEP_IMAGES_WITHIN,
            &tool_names,
        ));
    }
    let current = render_current(&turns[last], &upstream_model, tools, &tool_names);

    let mut state = Map::new();
    state.insert("agentTaskType".into(), json!("vibe"));
    state.insert("chatTriggerType".into(), json!("MANUAL"));
    state.insert(
        "conversationId".into(),
        json!(uuid::Uuid::new_v4().to_string()),
    );
    state.insert(
        "currentMessage".into(),
        json!({ "userInputMessage": current }),
    );
    if !history.is_empty() {
        state.insert("history".into(), Value::Array(history));
    }
    let mut root = Map::new();
    root.insert("conversationState".into(), Value::Object(state));
    if let Some(arn) = profile_arn.map(str::trim).filter(|arn| !arn.is_empty()) {
        root.insert("profileArn".into(), json!(arn));
    }
    if let Some(fields) = catalog::additional_fields(&upstream_model, &request.reasoning) {
        root.insert("additionalModelRequestFields".into(), fields);
    }
    let body = serde_json::to_vec(&Value::Object(root))
        .map_err(|_| invalid("Kiro request could not be serialized"))?;

    let input_estimate = estimate_input(&system, &turns, request);
    let summary = json!({
        "message_count": request.messages.len(),
        "history_count": last,
        "tool_count": request.tools.as_ref().map_or(0, Vec::len),
        "tool_result_count": turns.iter().map(|t| t.tool_results.len()).sum::<usize>(),
        "image_count": turns.iter().map(|t| t.images.len()).sum::<usize>(),
        "system_bytes": system.len(),
        "stream": request.stream.enabled,
    });
    Ok(Encoded {
        body,
        tool_names,
        thinking_tags: thinking_prefix.is_some(),
        input_estimate,
        upstream_model,
        summary,
    })
}

// ── Validation ───────────────────────────────────────────────────────────────

fn validate_options(request: &AiRequest) -> Result<(), GatewayError> {
    let generation = &request.generation;
    if generation.max_tokens == Some(0) {
        return Err(invalid("max_tokens must be greater than zero"));
    }
    if generation.seed.is_some()
        || generation.stop.as_ref().is_some_and(|v| !v.is_empty())
        || generation.presence_penalty.is_some()
        || generation.frequency_penalty.is_some()
    {
        return Err(unsupported(
            "generation.seed/stop/presence_penalty/frequency_penalty",
        ));
    }
    if !matches!(
        request.tool_choice,
        None | Some(ToolChoice::Auto | ToolChoice::None)
    ) {
        return Err(unsupported("tool_choice"));
    }
    if request.parallel_tool_calls == Some(false)
        || request.disable_parallel_tool_calls == Some(true)
    {
        return Err(unsupported("parallel_tool_calls"));
    }
    if !matches!(request.response_format, None | Some(ResponseFormat::Text)) {
        return Err(unsupported("response_format"));
    }
    if request
        .safety_settings
        .as_ref()
        .is_some_and(|v| !v.is_empty())
    {
        return Err(unsupported("safety_settings"));
    }
    match &request.ext {
        Some(ProtocolExt::OpenAiChat(ext))
            if ext.audio.is_some()
                || ext.logit_bias.is_some()
                || ext.logprobs == Some(true)
                || ext.top_logprobs.is_some()
                || ext
                    .modalities
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|m| m != "text"))
                || ext.n.is_some_and(|n| n != 1)
                || ext.prediction.is_some()
                || ext.web_search_options.is_some() =>
        {
            return Err(unsupported("openai generation extensions"));
        }
        Some(ProtocolExt::OpenAiResponses(ext))
            if ext.background == Some(true)
                || ext.previous_response_id.is_some()
                || ext.prompt.is_some()
                || ext.conversation.is_some()
                || ext.context_management.is_some()
                || ext.tool_choice_ext.is_some()
                || ext.top_logprobs.is_some() =>
        {
            return Err(unsupported("responses stateful/generation extensions"));
        }
        // Server-side tools (e.g. Claude Code's web_search) cannot run on
        // Kiro; clients advertise them on every request, so they are ignored
        // rather than rejected.
        Some(ProtocolExt::Anthropic(ext))
            if ext.container.is_some()
                || ext
                    .output_config
                    .as_ref()
                    .is_some_and(|v| v.get("format").is_some()) =>
        {
            return Err(unsupported("anthropic container/output format"));
        }
        Some(ProtocolExt::Google(ext))
            if ext.candidate_count.is_some_and(|n| n != 1)
                || ext.response_logprobs == Some(true)
                || ext.logprobs.is_some()
                || ext
                    .response_mime_type
                    .as_deref()
                    .is_some_and(|m| m != "text/plain")
                || ext.response_json_schema.is_some()
                || ext.tool_config.is_some()
                || ext.cached_content.is_some()
                || ext.image_config.is_some()
                || ext
                    .response_modalities
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|m| !m.eq_ignore_ascii_case("text"))) =>
        {
            return Err(unsupported("google generation extensions"));
        }
        _ => {}
    }
    if !request.meta.vendor.passthrough_safe.is_empty() || !request.meta.vendor.egress.is_empty() {
        return Err(unsupported("vendor native payload overrides"));
    }
    Ok(())
}

// ── IR -> turns ──────────────────────────────────────────────────────────────

fn build_turns(request: &AiRequest) -> Result<(Vec<Turn>, String), GatewayError> {
    let mut system = request.system.clone().unwrap_or_default();
    let mut turns = Vec::new();
    // Gemini may omit call IDs and identify function responses by name. Keep
    // both the routed name and the original block name: ToolRoutePlan rewrites
    // the typed calls, while ingress blocks retain the client's spelling.
    let mut pending_calls: Vec<(&str, &str)> = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System => {
                let text = system_text(&message.content)?;
                if !text.trim().is_empty() {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&text);
                }
            }
            Role::User => {
                reject_tool_calls(message)?;
                let mut turn = Turn::new(false);
                absorb_user(&message.content, &mut turn)?;
                turns.push(turn);
            }
            Role::Assistant => {
                let mut turn = Turn::new(true);
                // Typed calls are canonical after the dispatcher's tool bridge.
                // Process them first so duplicate ingress blocks cannot replace
                // a routed name or rewritten arguments with stale values.
                for call in message.tool_calls.iter().flatten() {
                    if call.is_custom() || call.namespace.is_some() {
                        return Err(unsupported("messages.tool_calls.custom_or_namespace"));
                    }
                    let input = parse_arguments(&call.arguments)?;
                    push_tool_use(&mut turn, &call.id, &call.name, input)?;
                    pending_calls.push((&call.id, &call.name));
                }
                absorb_assistant(&message.content, &mut turn)?;
                if let MessageContent::Blocks(blocks) = &message.content {
                    for block in blocks {
                        if let ContentBlock::ToolUse { id, name, .. } = block {
                            pending_calls.push((id, name));
                        }
                    }
                }
                turns.push(turn);
            }
            Role::Tool => {
                reject_tool_calls(message)?;
                let mut turn = Turn::new(false);
                if matches!(&message.content, MessageContent::Blocks(blocks)
                    if blocks.iter().any(ContentBlock::is_tool_result))
                {
                    // Gemini puts one or more function responses in blocks,
                    // not in message.tool_call_id. Preserve each result and
                    // any adjacent user text instead of flattening them away.
                    absorb_user(&message.content, &mut turn)?;
                } else {
                    let id = message
                        .tool_call_id
                        .as_deref()
                        .filter(|id| !id.trim().is_empty())
                        .ok_or_else(|| invalid("Kiro tool result requires a tool_call_id"))?;
                    turn.tool_results.push(ToolResult {
                        id: id.to_string(),
                        text: flatten_content(&message.content)?,
                        is_error: false,
                    });
                }
                turns.push(turn);
            }
        }
        if matches!(message.role, Role::User | Role::Tool) {
            let turn = turns.last_mut().expect("user/tool turn was just added");
            for result in &mut turn.tool_results {
                result.id = resolve_tool_result_id(
                    &result.id,
                    &mut pending_calls,
                    request.meta.source_protocol
                        == Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA),
                )?;
            }
        }
    }
    Ok((turns, system))
}

fn resolve_tool_result_id(
    hint: &str,
    pending: &mut Vec<(&str, &str)>,
    allow_name: bool,
) -> Result<String, GatewayError> {
    if hint.trim().is_empty() {
        return Err(invalid("Kiro tool result requires a tool call ID"));
    }
    let matched = pending
        .iter()
        .find(|(id, _)| *id == hint)
        .or_else(|| {
            if allow_name {
                pending.iter().find(|(_, name)| *name == hint)
            } else {
                None
            }
        })
        .map(|(id, _)| (*id).to_string());
    if let Some(id) = matched {
        // A call may appear as both a typed call and an ingress content block.
        pending.retain(|(pending_id, _)| *pending_id != id);
        Ok(id)
    } else if allow_name {
        Err(invalid(
            "Kiro tool result has no matching preceding function call",
        ))
    } else {
        Ok(hint.to_string())
    }
}

fn reject_tool_calls(message: &Message) -> Result<(), GatewayError> {
    if message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
    {
        return Err(invalid("Tool calls must belong to an assistant message"));
    }
    Ok(())
}

fn system_text(content: &MessageContent) -> Result<String, GatewayError> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => parts.push(text.as_str()),
                    _ => return Err(unsupported("system non-text content")),
                }
            }
            Ok(parts.join("\n"))
        }
    }
}

fn absorb_user(content: &MessageContent, turn: &mut Turn) -> Result<(), GatewayError> {
    match content {
        MessageContent::Text(text) => turn.push_text(text),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => turn.push_text(text),
                    ContentBlock::Image { source, .. } => turn.images.push(image(source)?),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        // Resolve IDs after collecting the whole turn. Gemini
                        // can return the same function name multiple times for
                        // distinct pending calls, so do not deduplicate hints.
                        turn.tool_results.push(ToolResult {
                            id: tool_use_id.clone(),
                            text: flatten_value(content),
                            is_error: *is_error == Some(true),
                        });
                    }
                    // Replayed reasoning cannot be signed for Kiro; skip it.
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
                    _ => {
                        return Err(unsupported(
                            "messages.content: unsupported Kiro content block",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn absorb_assistant(content: &MessageContent, turn: &mut Turn) -> Result<(), GatewayError> {
    match content {
        MessageContent::Text(text) => turn.push_text(text),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => turn.push_text(text),
                    ContentBlock::Refusal { refusal } => turn.push_text(refusal),
                    ContentBlock::Thinking { thinking, .. } => {
                        if !turn.thinking.is_empty() {
                            turn.thinking.push('\n');
                        }
                        turn.thinking.push_str(thinking);
                    }
                    ContentBlock::RedactedThinking { .. } => {}
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => push_tool_use(turn, id, name, input.clone())?,
                    _ => {
                        return Err(unsupported(
                            "messages.content: unsupported Kiro content block",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn push_tool_use(turn: &mut Turn, id: &str, name: &str, input: Value) -> Result<(), GatewayError> {
    if id.trim().is_empty() || name.trim().is_empty() {
        return Err(invalid("Kiro tool calls require an ID and name"));
    }
    if turn.tool_uses.iter().any(|existing| existing.id == id) {
        return Ok(());
    }
    turn.tool_uses.push(ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: if input.is_object() { input } else { json!({}) },
    });
    Ok(())
}

fn parse_arguments(arguments: &str) -> Result<Value, GatewayError> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments).map_err(|_| invalid("Kiro tool arguments must be valid JSON"))
}

fn image(source: &MediaSource) -> Result<Image, GatewayError> {
    match source {
        MediaSource::Base64 { media_type, data } => {
            let format = match media_type.trim().to_ascii_lowercase().as_str() {
                "image/png" => "png",
                "image/jpeg" | "image/jpg" => "jpeg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => return Err(unsupported("messages.content: unsupported Kiro image type")),
            };
            Ok(Image {
                format: format.into(),
                data: data.clone(),
            })
        }
        _ => Err(unsupported(
            "messages.content: Kiro accepts inline images only",
        )),
    }
}

/// Text of a tool result: string, or the text blocks of a block array.
fn flatten_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                    Some("text") | None => object
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    Some("image") => Some("[image omitted]".to_string()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn flatten_content(content: &MessageContent) -> Result<String, GatewayError> {
    Ok(match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

// ── Normalization ────────────────────────────────────────────────────────────

/// Produce an alternating turn list that starts with a user turn and ends with
/// a user turn, folding the system prompt into the first user turn.
fn normalize(turns: &mut Vec<Turn>, system: &str) -> Vec<Turn> {
    let all = std::mem::take(turns);
    let had_any = !all.is_empty();
    let mut kept: Vec<Turn> = all.into_iter().filter(|turn| !turn.is_empty()).collect();
    if kept.is_empty() && had_any {
        kept.push(Turn::new(false));
    }
    let mut merged: Vec<Turn> = Vec::with_capacity(kept.len());
    for turn in kept {
        match merged.last_mut() {
            Some(last) if last.assistant == turn.assistant => last.merge(turn),
            _ => merged.push(turn),
        }
    }
    if merged.first().is_some_and(|turn| turn.assistant) {
        let mut lead = Turn::new(false);
        lead.parts.push("Continue".into());
        merged.insert(0, lead);
    }
    if !system.trim().is_empty() {
        match merged.first_mut() {
            Some(first) if !first.assistant => first.parts.insert(0, system.to_string()),
            _ => {
                let mut lead = Turn::new(false);
                lead.parts.push(system.to_string());
                merged.insert(0, lead);
            }
        }
    }
    if merged.last().is_some_and(|turn| turn.assistant) {
        let mut tail = Turn::new(false);
        tail.parts.push("Continue".into());
        merged.push(tail);
    }
    merged
}

// ── Tools ────────────────────────────────────────────────────────────────────

fn short_tool_name(name: &str) -> String {
    if name.len() <= MAX_TOOL_NAME {
        return name.to_string();
    }
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    let suffix = &digest[..12];
    let mut cut = MAX_TOOL_NAME - suffix.len() - 1;
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}_{suffix}", &name[..cut])
}

fn truncate_description(description: &str) -> String {
    if description.len() <= MAX_TOOL_DESCRIPTION {
        return description.to_string();
    }
    let mut cut = MAX_TOOL_DESCRIPTION;
    while !description.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &description[..cut])
}

fn placeholder_tool() -> Value {
    json!({ "toolSpecification": {
        "name": PLACEHOLDER_TOOL,
        "description": "Internal no-op placeholder. Never call this tool. Answer directly in plain text instead of announcing or promising tool actions.",
        "inputSchema": { "json": { "type": "object", "properties": {} } },
    }})
}

fn render_tools(
    request: &AiRequest,
    _turns: &[Turn],
) -> Result<(Vec<Value>, HashMap<String, String>), GatewayError> {
    let mut specs = Vec::new();
    let mut aliases = HashMap::new();
    let mut seen = HashSet::new();
    if !matches!(request.tool_choice, Some(ToolChoice::None)) {
        for tool in request.tools.iter().flatten() {
            if tool.is_custom() || tool.namespace.is_some() {
                return Err(unsupported("tools.custom_or_namespace"));
            }
            let lowered = tool.name.trim().to_ascii_lowercase();
            if lowered.is_empty() || !seen.insert(tool.name.clone()) {
                return Err(invalid("Kiro tool names must be non-empty and unique"));
            }
            // Kiro runs web search itself; a client-declared one only confuses it.
            if lowered == "web_search" || lowered == "websearch" {
                continue;
            }
            if !tool.parameters.is_object() {
                return Err(invalid(
                    "Kiro function parameters must be a JSON Schema object",
                ));
            }
            let alias = short_tool_name(&tool.name);
            if alias != tool.name {
                aliases.insert(alias.clone(), tool.name.clone());
            }
            let description = tool
                .description
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .unwrap_or(&tool.name);
            specs.push(json!({ "toolSpecification": {
                "name": alias,
                "description": truncate_description(description),
                "inputSchema": { "json": tool.parameters },
            }}));
        }
    }
    if specs.is_empty() {
        specs.push(placeholder_tool());
    }
    Ok((specs, aliases))
}

/// Kiro-side name for a client tool name, matching [`render_tools`].
fn wire_name(name: &str) -> String {
    short_tool_name(name)
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn image_values(images: &[Image]) -> Vec<Value> {
    images
        .iter()
        .map(|image| json!({ "format": image.format, "source": { "bytes": image.data } }))
        .collect()
}

fn tool_use_values(uses: &[ToolUse]) -> Vec<Value> {
    uses.iter()
        .map(|call| json!({ "toolUseId": call.id, "name": wire_name(&call.name), "input": call.input }))
        .collect()
}

fn tool_result_values(results: &[ToolResult]) -> Vec<Value> {
    let mut seen = HashSet::new();
    results
        .iter()
        .filter(|result| seen.insert(result.id.clone()))
        .map(|result| {
            let text = if result.text.trim().is_empty() {
                "(no output)".to_string()
            } else {
                result.text.clone()
            };
            json!({
                "toolUseId": result.id,
                "status": if result.is_error { "error" } else { "success" },
                "content": [{ "text": text }],
            })
        })
        .collect()
}

fn user_content(turn: &Turn, keep_images: bool) -> String {
    let mut text = turn.text();
    let omitted = turn.omitted_images + if keep_images { 0 } else { turn.images.len() };
    if omitted > 0 {
        let note = format!("[{omitted} image(s) omitted from earlier history]");
        text = if text.is_empty() {
            note
        } else {
            format!("{text}\n{note}")
        };
    }
    if text.trim().is_empty() {
        text = if !turn.tool_results.is_empty() {
            "Tool results provided.".into()
        } else if !turn.images.is_empty() && keep_images {
            "Image provided.".into()
        } else {
            "Continue".into()
        };
    }
    text
}

fn render_history_turn(
    turn: &Turn,
    model: &str,
    keep_images: bool,
    _names: &HashMap<String, String>,
) -> Value {
    if turn.assistant {
        let mut content = turn.text();
        if !turn.thinking.trim().is_empty() {
            let thought = format!("{THINKING_OPEN}{}{THINKING_CLOSE}", turn.thinking);
            content = if content.is_empty() {
                thought
            } else {
                format!("{thought}\n\n{content}")
            };
        }
        if content.trim().is_empty() && turn.tool_uses.is_empty() {
            content = "Continue".into();
        }
        let mut message = Map::new();
        message.insert("content".into(), json!(content));
        if !turn.tool_uses.is_empty() {
            message.insert(
                "toolUses".into(),
                Value::Array(tool_use_values(&turn.tool_uses)),
            );
        }
        return json!({ "assistantResponseMessage": message });
    }
    let mut message = Map::new();
    message.insert("content".into(), json!(user_content(turn, keep_images)));
    message.insert("modelId".into(), json!(model));
    message.insert("origin".into(), json!("AI_EDITOR"));
    if keep_images && !turn.images.is_empty() {
        message.insert("images".into(), Value::Array(image_values(&turn.images)));
    }
    if !turn.tool_results.is_empty() {
        message.insert(
            "userInputMessageContext".into(),
            json!({ "toolResults": tool_result_values(&turn.tool_results) }),
        );
    }
    json!({ "userInputMessage": message })
}

fn render_current(
    turn: &Turn,
    model: &str,
    tools: Vec<Value>,
    _names: &HashMap<String, String>,
) -> Value {
    let mut message = Map::new();
    message.insert("content".into(), json!(user_content(turn, true)));
    message.insert("modelId".into(), json!(model));
    message.insert("origin".into(), json!("AI_EDITOR"));
    if !turn.images.is_empty() {
        message.insert("images".into(), Value::Array(image_values(&turn.images)));
    }
    let mut context = Map::new();
    if !turn.tool_results.is_empty() {
        context.insert(
            "toolResults".into(),
            Value::Array(tool_result_values(&turn.tool_results)),
        );
    }
    context.insert("tools".into(), Value::Array(tools));
    message.insert("userInputMessageContext".into(), Value::Object(context));
    Value::Object(message)
}

// ── Thinking directive ───────────────────────────────────────────────────────

/// In-band thinking directive Kiro models obey. Fixed budgets use the
/// `enabled` form; qualitative effort uses `adaptive`.
fn thinking_directive(reasoning: &ReasoningConfig) -> Option<String> {
    let budget = |tokens: u32| tokens.clamp(MIN_THINKING_BUDGET, MAX_THINKING_BUDGET);
    match reasoning.effort.as_ref() {
        Some(ReasoningEffort::None) => None,
        Some(ReasoningEffort::Budget(0)) => None,
        Some(ReasoningEffort::Budget(tokens)) => Some(enabled_directive(budget(*tokens))),
        Some(effort) => {
            let level = match effort {
                ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                _ => "high",
            };
            Some(format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{level}</thinking_effort>"
            ))
        }
        None => match reasoning.budget_tokens {
            Some(0) => None,
            Some(tokens) => Some(enabled_directive(budget(tokens))),
            None if reasoning.enabled => Some(enabled_directive(DEFAULT_THINKING_BUDGET)),
            None => None,
        },
    }
}

fn enabled_directive(tokens: u32) -> String {
    format!(
        "<thinking_mode>enabled</thinking_mode><max_thinking_length>{tokens}</max_thinking_length>"
    )
}

fn has_thinking_directive(system: &str) -> bool {
    [
        "<thinking_mode>",
        "<max_thinking_length>",
        "<thinking_effort>",
    ]
    .iter()
    .any(|tag| system.contains(tag))
}

// ── Local estimate ───────────────────────────────────────────────────────────

fn estimate_input(system: &str, turns: &[Turn], request: &AiRequest) -> u32 {
    let mut total = estimate_tokens(system);
    for turn in turns {
        total = total
            .saturating_add(estimate_tokens(&turn.text()))
            .saturating_add(estimate_tokens(&turn.thinking))
            .saturating_add(IMAGE_TOKENS.saturating_mul(turn.images.len() as u32));
        for call in &turn.tool_uses {
            total = total
                .saturating_add(estimate_tokens(&call.name))
                .saturating_add(estimate_tokens(&call.input.to_string()));
        }
        for result in &turn.tool_results {
            total = total.saturating_add(estimate_tokens(&result.text));
        }
    }
    for tool in request.tools.iter().flatten() {
        total = total
            .saturating_add(estimate_tokens(&tool.name))
            .saturating_add(estimate_tokens(tool.description.as_deref().unwrap_or("")))
            .saturating_add(estimate_tokens(&tool.parameters.to_string()));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            ..user(text)
        }
    }

    fn request(messages: Vec<Message>) -> AiRequest {
        AiRequest::new("claude-sonnet-4-5", messages)
    }

    fn tool_spec(name: &str, description: Option<&str>) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            namespace: None,
            description: description.map(str::to_string),
            kind: ToolSpecKind::Function,
            parameters: json!({"type":"object","properties":{"q":{"type":"string"}}}),
            strict: None,
            cache_control: None,
            meta: None,
        }
    }

    fn decode(encoded: &Encoded) -> Value {
        serde_json::from_slice(&encoded.body).unwrap()
    }

    fn encode_ok(request: &AiRequest) -> Encoded {
        encode(request, "claude-sonnet-4-5", None).unwrap()
    }

    #[test]
    fn single_user_message_becomes_the_current_message_with_a_placeholder_tool() {
        let encoded = encode_ok(&request(vec![user("hello")]));
        let body = decode(&encoded);
        let state = &body["conversationState"];
        assert_eq!(state["agentTaskType"], "vibe");
        assert_eq!(state["chatTriggerType"], "MANUAL");
        assert!(state["history"].is_null());
        let current = &state["currentMessage"]["userInputMessage"];
        assert_eq!(current["content"], "hello");
        assert_eq!(current["modelId"], "claude-sonnet-4.5");
        assert_eq!(current["origin"], "AI_EDITOR");
        let tools = current["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["toolSpecification"]["name"], PLACEHOLDER_TOOL);
        assert!(body.get("profileArn").is_none());
        assert!(body.get("additionalModelRequestFields").is_none());
        assert_eq!(encoded.upstream_model, "claude-sonnet-4.5");
    }

    #[test]
    fn system_prompt_prefixes_the_first_user_turn() {
        let mut req = request(vec![user("question")]);
        req.system = Some("be brief".into());
        let body = decode(&encode_ok(&req));
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "be brief\nquestion"
        );

        let mut req = request(vec![user("first"), assistant("answer"), user("second")]);
        req.system = Some("rules".into());
        let body = decode(&encode_ok(&req));
        let history = body["conversationState"]["history"].as_array().unwrap();
        assert_eq!(history[0]["userInputMessage"]["content"], "rules\nfirst");
        assert_eq!(history[1]["assistantResponseMessage"]["content"], "answer");
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "second"
        );
    }

    #[test]
    fn system_role_messages_fold_into_the_system_prompt() {
        let mut system = user("extra rules");
        system.role = Role::System;
        let body = decode(&encode_ok(&request(vec![system, user("hi")])));
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "extra rules\nhi"
        );
    }

    #[test]
    fn adjacent_same_role_messages_merge_and_alternation_is_enforced() {
        let body = decode(&encode_ok(&request(vec![
            user("a"),
            user("b"),
            assistant("c"),
            assistant("d"),
            user("e"),
        ])));
        let history = body["conversationState"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["userInputMessage"]["content"], "a\nb");
        assert_eq!(history[1]["assistantResponseMessage"]["content"], "c\nd");
    }

    #[test]
    fn leading_and_trailing_assistant_turns_get_continue_user_turns() {
        let body = decode(&encode_ok(&request(vec![
            assistant("start"),
            user("q"),
            assistant("tail"),
        ])));
        let history = body["conversationState"]["history"].as_array().unwrap();
        assert_eq!(history[0]["userInputMessage"]["content"], "Continue");
        assert_eq!(history[1]["assistantResponseMessage"]["content"], "start");
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "Continue"
        );
        assert!(
            history
                .iter()
                .any(|h| h["assistantResponseMessage"]["content"] == "tail")
        );
    }

    #[test]
    fn empty_turns_are_dropped_before_merging() {
        let body = decode(&encode_ok(&request(vec![
            user("a"),
            assistant("  "),
            user("b"),
        ])));
        assert!(body["conversationState"]["history"].is_null());
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "a\nb"
        );
    }

    #[test]
    fn anthropic_style_tool_use_and_result_round_trip() {
        let mut req = request(vec![
            user("search"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "looking".into(),
                        cache_control: None,
                    },
                    ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "lookup".into(),
                        input: json!({"q":"x"}),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: json!([{"type":"text","text":"found"}]),
                    is_error: Some(false),
                    cache_control: None,
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ]);
        req.tools = Some(vec![tool_spec("lookup", Some("Look things up"))]);
        let body = decode(&encode_ok(&req));
        let history = body["conversationState"]["history"].as_array().unwrap();
        let uses = &history[1]["assistantResponseMessage"]["toolUses"];
        assert_eq!(uses[0]["toolUseId"], "tu_1");
        assert_eq!(uses[0]["name"], "lookup");
        assert_eq!(uses[0]["input"], json!({"q":"x"}));
        let current = &body["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(current["content"], "Tool results provided.");
        let results = &current["userInputMessageContext"]["toolResults"];
        assert_eq!(results[0]["toolUseId"], "tu_1");
        assert_eq!(results[0]["status"], "success");
        assert_eq!(results[0]["content"][0]["text"], "found");
        let tools = current["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["toolSpecification"]["name"], "lookup");
        assert_eq!(
            tools[0]["toolSpecification"]["description"],
            "Look things up"
        );
    }

    #[test]
    fn openai_style_tool_calls_and_tool_role_results_are_mapped() {
        let mut call_message = assistant("");
        call_message.tool_calls =
            Some(vec![ToolCall::function("call_1", "lookup", r#"{"q":"x"}"#)]);
        let mut result = user("42");
        result.role = Role::Tool;
        result.tool_call_id = Some("call_1".into());
        let mut req = request(vec![user("go"), call_message, result]);
        req.tools = Some(vec![tool_spec("lookup", None)]);
        let body = decode(&encode_ok(&req));
        let history = body["conversationState"]["history"].as_array().unwrap();
        assert_eq!(
            history[1]["assistantResponseMessage"]["toolUses"][0]["toolUseId"],
            "call_1"
        );
        let current = &body["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(
            current["userInputMessageContext"]["toolResults"][0]["content"][0]["text"],
            "42"
        );
        // A missing description falls back to the tool name instead of being dropped.
        assert_eq!(
            current["userInputMessageContext"]["tools"][0]["toolSpecification"]["description"],
            "lookup"
        );
    }

    #[test]
    fn tool_errors_and_empty_results_are_marked() {
        let req = request(vec![
            user("go"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "run".into(),
                    input: json!({}),
                    cache_control: None,
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t".into(),
                    content: json!(""),
                    is_error: Some(true),
                    cache_control: None,
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ]);
        let body = decode(&encode_ok(&req));
        let result = &body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
            ["toolResults"][0];
        assert_eq!(result["status"], "error");
        assert_eq!(result["content"][0]["text"], "(no output)");
    }

    #[test]
    fn gemini_decoder_tool_results_keep_ids_payloads_and_parallel_same_name_calls() {
        use crate::protocol::codec::google::gemini::decoder::GoogleDecoder;
        use crate::protocol::codec::tool_bridge::ToolRoutePlan;
        use crate::protocol::ids::KIRO_CHAT_V1;

        // Both calls use a name the dispatcher must rewrite. ID-less Gemini
        // results still carry the original name and must match in call order.
        let name = format!("mcp__lookup__{}", "x".repeat(80));
        for explicit_ids in [false, true] {
            let mut first = json!({"name": name, "args": {"q": "first"}});
            let mut second = json!({"name": name, "args": {"q": "second"}});
            let mut first_result =
                json!({"name": name, "response": {"answer": 1, "items": ["a", "b"]}});
            let mut second_result = json!({"name": name, "response": {"answer": 2}});
            if explicit_ids {
                first["id"] = json!("call_1");
                second["id"] = json!("call_2");
                first_result["id"] = json!("call_1");
                second_result["id"] = json!("call_2");
                // Explicit IDs take priority over same-name FIFO matching.
                std::mem::swap(&mut first_result, &mut second_result);
            }
            let mut req = GoogleDecoder
                .decode_with_model(
                    json!({
                        "contents": [
                            {"role": "user", "parts": [{"text": "look up both"}]},
                            {"role": "model", "parts": [
                                {"functionCall": first}, {"functionCall": second}
                            ]},
                            {"role": "user", "parts": [
                                {"functionResponse": first_result},
                                {"text": "compare the results"},
                                {"functionResponse": second_result}
                            ]}
                        ],
                        "tools": [{"functionDeclarations": [{
                            "name": name, "parameters": {"type": "object"}
                        }]}]
                    }),
                    "claude-sonnet-4-5",
                    true,
                )
                .unwrap();
            assert_eq!(req.messages[2].role, Role::Tool);
            assert!(req.messages[2].tool_call_id.is_none());
            let ids: Vec<String> = req.messages[1]
                .tool_calls
                .as_ref()
                .unwrap()
                .iter()
                .map(|call| call.id.clone())
                .collect();
            let plan = ToolRoutePlan::for_request(&req, KIRO_CHAT_V1);
            assert!(plan.is_active());
            plan.prepare_upstream_request(&mut req);

            let body = decode(&encode_ok(&req));
            let state = &body["conversationState"];
            let calls = state["history"][1]["assistantResponseMessage"]["toolUses"]
                .as_array()
                .unwrap();
            assert_eq!(calls.len(), 2, "duplicate IR blocks are not extra calls");
            let current = &state["currentMessage"]["userInputMessage"];
            assert_eq!(current["content"], "compare the results");
            let context = &current["userInputMessageContext"];
            for call in calls {
                assert_eq!(
                    call["name"],
                    context["tools"][0]["toolSpecification"]["name"]
                );
            }
            let results = context["toolResults"].as_array().unwrap();
            assert_eq!(
                results.len(),
                2,
                "same-name responses must not be deduplicated"
            );
            let order = if explicit_ids { [1, 0] } else { [0, 1] };
            for (result, index) in results.iter().zip(order) {
                assert_eq!(result["toolUseId"], ids[index]);
                assert_eq!(result["status"], "success");
                let payload: Value =
                    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(payload["answer"], index + 1);
                if index == 0 {
                    assert_eq!(payload["items"], json!(["a", "b"]));
                }
            }
        }
    }

    #[test]
    fn gemini_unmatched_function_response_is_rejected_instead_of_inventing_an_id() {
        use crate::protocol::codec::google::gemini::decoder::GoogleDecoder;
        use crate::protocol::codec::tool_bridge::ToolRoutePlan;
        use crate::protocol::ids::KIRO_CHAT_V1;

        let mut req = GoogleDecoder
            .decode_with_model(
                json!({
                    "contents": [{"role": "user", "parts": [{
                        "functionResponse": {"name": "missing", "response": {"answer": 1}}
                    }]}]
                }),
                "claude-sonnet-4-5",
                false,
            )
            .unwrap();
        ToolRoutePlan::for_request(&req, KIRO_CHAT_V1).prepare_upstream_request(&mut req);
        assert!(matches!(
            encode(&req, "m", None),
            Err(GatewayError::BadRequest { .. })
        ));
    }

    #[test]
    fn anthropic_decoder_history_uses_the_dispatchers_routed_mcp_name() {
        use crate::protocol::RequestDecoder;
        use crate::protocol::codec::anthropic::messages::decoder::AnthropicDecoder;
        use crate::protocol::codec::tool_bridge::ToolRoutePlan;
        use crate::protocol::ids::KIRO_CHAT_V1;

        let name = format!("mcp__plugin_server__{}", "x".repeat(80));
        let mut req = AnthropicDecoder
            .decode_request(json!({
                "model": "claude-sonnet-4-5", "max_tokens": 4096,
                "messages": [
                    {"role": "user", "content": "look it up"},
                    {"role": "assistant", "content": [{
                        "type": "tool_use", "id": "toolu_1", "name": name, "input": {"q": "x"}
                    }]},
                    {"role": "user", "content": [{
                        "type": "tool_result", "tool_use_id": "toolu_1", "content": "found"
                    }]}
                ],
                "tools": [{"name": name, "input_schema": {"type": "object"}}]
            }))
            .unwrap();
        let plan = ToolRoutePlan::for_request(&req, KIRO_CHAT_V1);
        assert!(plan.is_active());
        plan.prepare_upstream_request(&mut req);
        let routed_name = &req.tools.as_ref().unwrap()[0].name;
        assert_ne!(routed_name, &name);
        assert!(
            matches!(&req.messages[1].content, MessageContent::Blocks(blocks)
            if matches!(&blocks[0], ContentBlock::ToolUse { name: original, .. } if original == &name))
        );

        let body = decode(&encode_ok(&req));
        let calls = body["conversationState"]["history"][1]["assistantResponseMessage"]["toolUses"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], routed_name.as_str());
        assert_eq!(calls[0]["input"], json!({"q": "x"}));
        let context = &body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        assert_eq!(
            calls[0]["name"],
            context["tools"][0]["toolSpecification"]["name"]
        );
        assert_eq!(context["toolResults"][0]["toolUseId"], "toolu_1");
        assert_eq!(context["toolResults"][0]["content"][0]["text"], "found");
    }

    #[test]
    fn long_tool_names_are_shortened_and_reversible() {
        let long = "mcp__plugin_server__".to_string() + &"x".repeat(80);
        let mut req = request(vec![user("hi")]);
        req.tools = Some(vec![tool_spec(&long, Some("d"))]);
        let encoded = encode_ok(&req);
        let wire = decode(&encoded)["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"][0]["toolSpecification"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(wire.len() <= MAX_TOOL_NAME);
        assert_ne!(wire, long);
        assert_eq!(encoded.tool_names.get(&wire), Some(&long));
        // Deterministic, so history tool_use names match the declared spec.
        assert_eq!(short_tool_name(&long), wire);
        assert_eq!(short_tool_name("short"), "short");
    }

    #[test]
    fn web_search_tools_are_dropped_and_duplicates_rejected() {
        let mut req = request(vec![user("hi")]);
        req.tools = Some(vec![
            tool_spec("WebSearch", Some("x")),
            tool_spec("lookup", Some("y")),
        ]);
        let tools = decode(&encode_ok(&req))["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["toolSpecification"]["name"], "lookup");

        req.tools = Some(vec![tool_spec("a", None), tool_spec("a", None)]);
        assert!(encode(&req, "m", None).is_err());
    }

    #[test]
    fn only_a_dropped_tool_still_leaves_the_placeholder() {
        let mut req = request(vec![user("hi")]);
        req.tools = Some(vec![tool_spec("web_search", Some("x"))]);
        let tools = decode(&encode_ok(&req))["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools[0]["toolSpecification"]["name"], PLACEHOLDER_TOOL);
    }

    #[test]
    fn oversized_descriptions_are_truncated_on_a_char_boundary() {
        let mut req = request(vec![user("hi")]);
        req.tools = Some(vec![tool_spec("t", Some(&"é".repeat(6000)))]);
        let description = decode(&encode_ok(&req))["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"][0]["toolSpecification"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(description.len() <= MAX_TOOL_DESCRIPTION + 3);
        assert!(description.ends_with("..."));
    }

    #[test]
    fn inline_images_map_and_old_history_images_are_replaced_by_a_note() {
        let png = |data: &str| ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/png".into(),
                data: data.into(),
            },
            cache_control: None,
        };
        let with_image = |text: &str, data: &str| Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: text.into(),
                    cache_control: None,
                },
                png(data),
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let mut messages = vec![with_image("old", "OLDDATA")];
        for i in 0..7 {
            messages.push(assistant(&format!("a{i}")));
            messages.push(user(&format!("u{i}")));
        }
        messages.push(assistant("last"));
        messages.push(with_image("new", "NEWDATA"));
        let body = decode(&encode_ok(&request(messages)));
        let history = body["conversationState"]["history"].as_array().unwrap();
        let first = &history[0]["userInputMessage"];
        assert!(first.get("images").is_none());
        assert!(
            first["content"]
                .as_str()
                .unwrap()
                .contains("1 image(s) omitted")
        );
        let current = &body["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(current["images"][0]["format"], "png");
        assert_eq!(current["images"][0]["source"]["bytes"], "NEWDATA");
    }

    #[test]
    fn remote_images_and_unknown_blocks_are_rejected() {
        let url_image = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::Url("https://example.com/a.png".into()),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        assert!(matches!(
            encode(&request(vec![url_image]), "m", None),
            Err(GatewayError::ProtocolLossyRejected { .. })
        ));
        let audio = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Audio {
                source: MediaSource::Url("x".into()),
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        assert!(matches!(
            encode(&request(vec![audio]), "m", None),
            Err(GatewayError::ProtocolLossyRejected { .. })
        ));
    }

    #[test]
    fn assistant_thinking_is_replayed_in_tags() {
        let thought = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "plan".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let body = decode(&encode_ok(&request(vec![
            user("q"),
            thought,
            user("again"),
        ])));
        assert_eq!(
            body["conversationState"]["history"][1]["assistantResponseMessage"]["content"],
            "<thinking>plan</thinking>\n\nanswer"
        );
    }

    #[test]
    fn thinking_directive_variants() {
        let enabled = |tokens| ReasoningConfig {
            enabled: true,
            budget_tokens: Some(tokens),
            ..Default::default()
        };
        assert_eq!(
            thinking_directive(&enabled(8000)).unwrap(),
            "<thinking_mode>enabled</thinking_mode><max_thinking_length>8000</max_thinking_length>"
        );
        assert!(
            thinking_directive(&enabled(10))
                .unwrap()
                .contains("<max_thinking_length>1024<")
        );
        assert!(
            thinking_directive(&enabled(999_999))
                .unwrap()
                .contains("<max_thinking_length>24576<")
        );
        assert!(thinking_directive(&enabled(0)).is_none());
        let on = ReasoningConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(
            thinking_directive(&on)
                .unwrap()
                .contains("<max_thinking_length>20000<")
        );
        let adaptive = ReasoningConfig {
            enabled: true,
            effort: Some(ReasoningEffort::Medium),
            ..Default::default()
        };
        assert_eq!(
            thinking_directive(&adaptive).unwrap(),
            "<thinking_mode>adaptive</thinking_mode><thinking_effort>medium</thinking_effort>"
        );
        assert!(thinking_directive(&ReasoningConfig::default()).is_none());
        let off = ReasoningConfig {
            effort: Some(ReasoningEffort::None),
            ..Default::default()
        };
        assert!(thinking_directive(&off).is_none());
    }

    #[test]
    fn thinking_directive_is_prepended_once_and_flags_tag_parsing() {
        let mut req = request(vec![user("q")]);
        req.reasoning = ReasoningConfig {
            enabled: true,
            budget_tokens: Some(4000),
            ..Default::default()
        };
        req.system = Some("rules".into());
        let encoded = encode_ok(&req);
        assert!(encoded.thinking_tags);
        let content =
            decode(&encoded)["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .unwrap()
                .to_string();
        assert!(content.starts_with("<thinking_mode>enabled</thinking_mode>"));
        assert_eq!(content.matches("<thinking_mode>").count(), 1);

        // A caller-supplied directive is respected, not duplicated.
        req.system = Some("<thinking_mode>enabled</thinking_mode> mine".into());
        let content = decode(&encode_ok(&req))["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(content.matches("<thinking_mode>").count(), 1);
    }

    #[test]
    fn profile_arn_and_model_effort_fields_are_included_when_known() {
        let mut req = request(vec![user("q")]);
        req.reasoning = ReasoningConfig {
            enabled: true,
            effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let encoded = encode(
            &req,
            "claude-opus-4-7",
            Some("arn:aws:codewhisperer:us-east-1:1:profile/X"),
        )
        .unwrap();
        let value = decode(&encoded);
        assert_eq!(
            value["profileArn"],
            "arn:aws:codewhisperer:us-east-1:1:profile/X"
        );
        assert_eq!(
            value["additionalModelRequestFields"]["output_config"]["effort"],
            "high"
        );
        assert_eq!(encoded.upstream_model, "claude-opus-4.7");
        // Blank ARNs are omitted.
        assert!(
            decode(&encode(&req, "claude-opus-4-7", Some("  ")).unwrap())
                .get("profileArn")
                .is_none()
        );
    }

    #[test]
    fn unsupported_options_are_rejected_but_sampling_knobs_are_ignored() {
        let mut req = request(vec![user("hi")]);
        req.generation.temperature = Some(0.2);
        req.generation.top_p = Some(0.9);
        req.generation.max_tokens = Some(4096);
        assert!(encode(&req, "m", None).is_ok());

        for mutate in [
            (|r: &mut AiRequest| r.generation.stop = Some(vec!["x".into()])) as fn(&mut AiRequest),
            |r| r.generation.seed = Some(1),
            |r| r.generation.presence_penalty = Some(0.1),
            |r| r.tool_choice = Some(ToolChoice::Required),
            |r| r.parallel_tool_calls = Some(false),
            |r| r.response_format = Some(ResponseFormat::JsonObject),
            |r| r.generation.max_tokens = Some(0),
        ] {
            let mut req = request(vec![user("hi")]);
            mutate(&mut req);
            assert!(encode(&req, "m", None).is_err());
        }
    }

    #[test]
    fn tool_choice_none_hides_tools_and_empty_conversations_fail() {
        let mut req = request(vec![user("hi")]);
        req.tools = Some(vec![tool_spec("lookup", Some("d"))]);
        req.tool_choice = Some(ToolChoice::None);
        let tools = decode(&encode_ok(&req))["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools[0]["toolSpecification"]["name"], PLACEHOLDER_TOOL);

        assert!(encode(&request(vec![]), "m", None).is_err());
        assert!(encode(&request(vec![user("hi")]), "  ", None).is_err());
    }

    #[test]
    fn estimate_scales_with_content_and_charges_images_flatly() {
        let small = encode_ok(&request(vec![user("hi")])).input_estimate;
        let large = encode_ok(&request(vec![user(&"word ".repeat(2000))])).input_estimate;
        assert!(large > small + 1000);
        let image = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::Base64 {
                    media_type: "image/jpeg".into(),
                    data: "A".repeat(500_000),
                },
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let with_image = encode_ok(&request(vec![image])).input_estimate;
        assert!((IMAGE_TOKENS..IMAGE_TOKENS + 200).contains(&with_image));
    }

    #[test]
    fn summary_is_content_free() {
        let encoded = encode_ok(&request(vec![user("super secret prompt")]));
        let text = encoded.summary.to_string();
        assert!(!text.contains("secret"));
        assert_eq!(encoded.summary["message_count"], 1);
    }
}
