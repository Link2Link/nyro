//! Translate the gateway IR into Devin's Connect-protobuf chat request.
//! Transport and model/effort selection belong to the enclosing provider.
use super::wire;
use crate::{error::GatewayError, protocol::ir::*};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use prost::Message as _;
use rand::RngCore as _;
use serde_json::Value;
use sha1::{Digest as _, Sha1};
use std::collections::HashSet;

const CLIENT_VERSION: &str = "3000.10.21";

fn invalid(message: &str) -> GatewayError {
    GatewayError::BadRequest {
        code: "devin_invalid_request",
        msg: message.into(),
    }
}
fn unsupported(field: &str) -> GatewayError {
    GatewayError::ProtocolLossyRejected {
        lost: vec![field.into()],
    }
}

fn metadata(token: &str) -> wire::ClientMetadata {
    let mut fingerprint = [0u8; 366];
    rand::thread_rng().fill_bytes(&mut fingerprint);
    wire::ClientMetadata {
        client: "chisel".into(),
        version: CLIENT_VERSION.into(),
        token: token.into(),
        locale: "en".into(),
        os: if cfg!(target_os = "macos") {
            "darwin"
        } else {
            std::env::consts::OS
        }
        .into(),
        extension_version: CLIENT_VERSION.into(),
        extension: "chisel".into(),
        fingerprint: hex::encode(fingerprint),
    }
}

/// GetUserStatus is unary: raw protobuf with only field 1, NOT a Connect envelope.
pub fn user_status_request(token: &str) -> Vec<u8> {
    wire::ChatRequest {
        metadata: Some(metadata(token)),
        ..Default::default()
    }
    .encode_to_vec()
}

/// The returned body already includes the five-byte Connect envelope.
/// `actual_model` must already include the provider's requested reasoning variant.
pub fn encode(
    request: &AiRequest,
    token: &str,
    actual_model: &str,
) -> Result<Vec<u8>, GatewayError> {
    if token.trim().is_empty() {
        return Err(invalid("Devin session token is required"));
    }
    if actual_model.trim().is_empty() {
        return Err(invalid("Devin model is required"));
    }
    validate_options(request)?;
    let mut system = request.system.clone().unwrap_or_default();
    let mut prompts = Vec::new();
    let mut pending_calls = Vec::<String>::new();
    for message in &request.messages {
        if message.role == Role::System {
            append_text(&mut system, &system_text(&message.content)?);
            continue;
        }
        let source = match message.role {
            Role::Assistant => 2,
            Role::Tool => 4,
            _ => 1,
        };
        let mut prompt = new_prompt(source);
        if source == 4 {
            let id = message
                .tool_call_id
                .as_deref()
                .ok_or_else(|| invalid("Devin tool result requires a tool_call_id"))?;
            prompt.call_id = wire::decode_tool_id(id);
        }
        match &message.content {
            MessageContent::Text(text) => prompt.content = Some(text.clone()),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            append_text(prompt.content.as_mut().unwrap(), text)
                        }
                        ContentBlock::Refusal { refusal } if source == 2 => {
                            append_text(prompt.content.as_mut().unwrap(), refusal)
                        }
                        ContentBlock::Image { source: media, .. } => {
                            prompt.images.push(image(media)?)
                        }
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } if source == 2 => {
                            append_text(&mut prompt.thinking, thinking);
                            if let Some(signature) = signature {
                                set_signature(&mut prompt, signature)?;
                            }
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } if source == 2 => {
                            let call = wire::Call {
                                id: wire::decode_tool_id(id),
                                name: name.clone(),
                                arguments: input.to_string(),
                            };
                            validate_call(&call)?;
                            prompt.calls.push(call);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } if source == 1 || source == 4 => {
                            if has_content(&prompt) {
                                finish_prompt(&mut prompts, &mut pending_calls, prompt)?;
                            }
                            let mut result = new_prompt(4);
                            result.call_id = wire::decode_tool_id(tool_use_id);
                            let (text, images) = result_content(content)?;
                            result.content = Some(if *is_error == Some(true) {
                                format!("[Tool error]\n{text}")
                            } else {
                                text
                            });
                            result.images = images;
                            finish_prompt(&mut prompts, &mut pending_calls, result)?;
                            prompt = new_prompt(source);
                        }
                        _ => {
                            return Err(unsupported(
                                "messages.content: unsupported Devin content block",
                            ));
                        }
                    }
                }
            }
        }
        if let Some(calls) = &message.tool_calls {
            if source != 2 && !calls.is_empty() {
                return Err(invalid("Tool calls must belong to an assistant message"));
            }
            for call in calls {
                if call.is_custom() || call.namespace.is_some() {
                    return Err(unsupported("messages.tool_calls.custom_or_namespace"));
                }
                let call = wire::Call {
                    id: wire::decode_tool_id(&call.id),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                };
                validate_call(&call)?;
                if !prompt.calls.iter().any(|old| old.id == call.id) {
                    prompt.calls.push(call);
                }
            }
        }
        if has_content(&prompt) || (source == 4 && !prompt.call_id.is_empty()) {
            finish_prompt(&mut prompts, &mut pending_calls, prompt)?;
        } else if source == 1
            && !matches!(&message.content, MessageContent::Blocks(bs) if bs.iter().any(ContentBlock::is_tool_result))
        {
            return Err(invalid(
                "Devin user message must contain non-blank text, an inline image, or a tool result",
            ));
        }
    }
    if prompts.is_empty() {
        return Err(invalid("Devin requires at least one conversation message"));
    }
    if prompts.last().is_some_and(|p| p.source == 2) {
        return Err(unsupported("assistant_prefill"));
    }
    let mut tools = Vec::new();
    let mut names = HashSet::new();
    if !matches!(request.tool_choice, Some(ToolChoice::None)) {
        for tool in request.tools.iter().flatten() {
            if tool.is_custom() || tool.namespace.is_some() {
                return Err(unsupported("tools.custom_or_namespace"));
            }
            if tool.strict == Some(true) {
                return Err(unsupported("tools.strict"));
            }
            if tool.name.trim().is_empty() || !names.insert(tool.name.clone()) {
                return Err(invalid("Devin tool names must be non-empty and unique"));
            }
            if !tool.parameters.is_object() {
                return Err(invalid(
                    "Devin function parameters must be a JSON Schema object",
                ));
            }
            tools.push(wire::Tool {
                name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
                parameters: tool.parameters.to_string().into_bytes(),
            });
        }
    }
    let session_id = session_id(request);
    // A full-history request has one completed model invocation per assistant turn.
    // No process-wide counter is necessary for correctness of this pure encoder.
    let turn_index = prompts
        .iter()
        .filter(|p| p.source == 2)
        .count()
        .min(u32::MAX as usize) as u32;
    let user_boundary = if prompts.last().is_some_and(|p| p.source == 1)
        && (turn_index == 0 || prompts.len() < 2 || prompts[prompts.len() - 2].source != 1)
    {
        14
    } else {
        0
    };
    let top_k = match &request.ext {
        Some(ProtocolExt::Anthropic(ext)) => ext.top_k,
        Some(ProtocolExt::Google(ext)) => ext.top_k,
        _ => None,
    }
    .unwrap_or(40);
    let body = wire::ChatRequest {
        metadata: Some(metadata(token)),
        system,
        prompts,
        mode: 5,
        completion: Some(wire::Completion {
            enabled: 1,
            max_tokens: request.generation.max_tokens.unwrap_or(64000),
            parameter_3: 400,
            temperature: Some(request.generation.temperature.unwrap_or(1.0)),
            top_k,
            top_p: Some(request.generation.top_p.unwrap_or(f64::from(0.95_f32))),
        }),
        tools,
        thread: Some(wire::Thread {
            session_id: session_id.clone(),
            turn_index,
            mode: 4,
            user_boundary,
        }),
        cascade_id: session_id,
        enabled: 1,
        model: actual_model.into(),
    };
    wire::envelope(&body.encode_to_vec())
}

fn validate_options(request: &AiRequest) -> Result<(), GatewayError> {
    let generation = &request.generation;
    if generation.max_tokens == Some(0) {
        return Err(invalid("max_tokens must be greater than zero"));
    }
    for (value, maximum) in [(generation.temperature, 2.0), (generation.top_p, 1.0)] {
        if value.is_some_and(|v| !v.is_finite() || !(0.0..=maximum).contains(&v)) {
            return Err(invalid("Invalid Devin sampling value"));
        }
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
                || ext.web_search_options.is_some()
                || ext.verbosity.is_some() =>
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
        Some(ProtocolExt::Anthropic(ext))
            if ext.container.is_some()
                || ext.server_tools.as_ref().is_some_and(|v| !v.is_empty())
                || ext
                    .output_config
                    .as_ref()
                    .is_some_and(|v| v.get("format").is_some()) =>
        {
            return Err(unsupported("anthropic server tools/output format"));
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

fn new_prompt(source: u32) -> wire::Prompt {
    wire::Prompt {
        id: uuid::Uuid::new_v4().to_string(),
        source,
        content: Some(String::new()),
        ..Default::default()
    }
}
fn append_text(target: &mut String, text: &str) {
    if !target.is_empty() && !text.is_empty() {
        target.push('\n');
    }
    target.push_str(text);
}
fn system_text(content: &MessageContent) -> Result<String, GatewayError> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: t, .. } => append_text(&mut text, t),
                    _ => return Err(unsupported("system non-text content")),
                }
            }
            Ok(text)
        }
    }
}
fn has_content(prompt: &wire::Prompt) -> bool {
    prompt
        .content
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
        || !prompt.images.is_empty()
        || !prompt.calls.is_empty()
        || !prompt.thinking.is_empty()
        || !prompt.signature.is_empty()
}
fn validate_call(call: &wire::Call) -> Result<(), GatewayError> {
    if call.id.is_empty() || call.name.trim().is_empty() {
        return Err(invalid("Devin function calls require an ID and name"));
    }
    if serde_json::from_str::<Value>(&call.arguments).is_err() {
        return Err(invalid("Devin function arguments must be valid JSON"));
    }
    Ok(())
}
fn finish_prompt(
    prompts: &mut Vec<wire::Prompt>,
    pending: &mut Vec<String>,
    mut prompt: wire::Prompt,
) -> Result<(), GatewayError> {
    if prompt.source == 4 {
        if let Some(index) = pending.iter().position(|id| id == &prompt.call_id) {
            pending.remove(index);
        } else {
            prompt.source = 1;
            prompt.call_id.clear();
        } // Orphaned tool results remain readable history.
        if prompt
            .content
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
            && prompt.images.is_empty()
        {
            prompt.content = Some("{}".into());
        }
    }
    for call in &prompt.calls {
        pending.push(call.id.clone());
    }
    if !prompt.images.is_empty() {
        let headers: String = prompt
            .images
            .iter()
            .enumerate()
            .map(|(i, image)| {
                let extension = match image.mime_type.as_str() {
                    "image/jpeg" | "image/jpg" => "jpg",
                    "image/webp" => "webp",
                    "image/gif" => "gif",
                    _ => "png",
                };
                format!("[Image {}: pasted_image_{}.{extension}]", i + 1, i + 1)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = prompt.content.get_or_insert_default();
        if !text.contains("[Image ") {
            *text = if text.is_empty() {
                headers
            } else {
                format!("{headers}\n\n{text}")
            };
        }
    }
    prompts.push(prompt);
    Ok(())
}

fn image(source: &MediaSource) -> Result<wire::Image, GatewayError> {
    let (mime, data) = match source {
        MediaSource::Base64 { media_type, data } => (media_type.as_str(), data.as_str()),
        MediaSource::Url(url) => {
            let (header, data) = url
                .strip_prefix("data:")
                .and_then(|s| s.split_once(','))
                .ok_or_else(|| unsupported("remote image URL"))?;
            let mime = header
                .strip_suffix(";base64")
                .ok_or_else(|| unsupported("non-base64 image data URL"))?;
            (mime, data)
        }
        _ => return Err(unsupported("provider file image reference")),
    };
    if !mime.starts_with("image/")
        || data.is_empty()
        || STANDARD.decode(data).map_or(true, |v| v.is_empty())
    {
        return Err(invalid("Devin requires a valid non-empty base64 image"));
    }
    Ok(wire::Image {
        mime_type: mime.into(),
        data: data.into(),
    })
}

fn result_content(value: &Value) -> Result<(String, Vec<wire::Image>), GatewayError> {
    if let Some(text) = value.as_str() {
        return Ok((
            if text.trim().is_empty() {
                "{}".into()
            } else {
                text.into()
            },
            vec![],
        ));
    }
    if let Some(object) = value.as_object() {
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if ["image", "image_url", "input_image"].contains(&kind) {
            if let Some(source) = object.get("source") {
                if source.get("type").and_then(Value::as_str) == Some("base64") {
                    let source = MediaSource::Base64 {
                        media_type: source
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        data: source
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    };
                    return Ok((String::new(), vec![image(&source)?]));
                }
                if let Some(url) = source.get("url").and_then(Value::as_str) {
                    return Ok((String::new(), vec![image(&MediaSource::Url(url.into()))?]));
                }
            }
            let url = object
                .get("image_url")
                .and_then(|v| v.as_str().or_else(|| v.get("url").and_then(Value::as_str)))
                .or_else(|| object.get("url").and_then(Value::as_str));
            if let Some(url) = url {
                return Ok((String::new(), vec![image(&MediaSource::Url(url.into()))?]));
            }
            return Err(unsupported("tool result image source"));
        }
        if [
            "audio",
            "input_audio",
            "video",
            "document",
            "file",
            "input_file",
        ]
        .contains(&kind)
        {
            return Err(unsupported("tool result media"));
        }
        if kind == "text"
            && object
                .keys()
                .all(|k| ["type", "text", "cache_control"].contains(&k.as_str()))
        {
            return Ok((
                object
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                vec![],
            ));
        }
        for key in ["content", "output", "result"] {
            if let Some(inner) = object.get(key)
                && object.keys().all(|k| {
                    k == key
                        || k == "cache_control"
                        || (kind == "tool_result"
                            && ["type", "tool_use_id", "id", "is_error"].contains(&k.as_str()))
                })
            {
                return result_content(inner);
            }
        }
    }
    if let Some(items) = value.as_array() {
        // Plain business arrays must remain JSON, rather than becoming joined strings.
        if !items
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str).is_some())
        {
            return Ok((value.to_string(), vec![]));
        }
        let mut text = String::new();
        let mut images = Vec::new();
        for item in items {
            let (part, mut attached) = result_content(item)?;
            append_text(&mut text, &part);
            images.append(&mut attached);
        }
        return Ok((text, images));
    }
    Ok((value.to_string(), vec![]))
}

fn set_signature(prompt: &mut wire::Prompt, signature: &str) -> Result<(), GatewayError> {
    if signature.is_empty() {
        return Ok(());
    }
    let mut signature = signature.to_owned();
    if let Ok(decoded) = STANDARD.decode(&signature)
        && let Ok(decoded) = String::from_utf8(decoded)
        && ["sealed.v1.", "CAQS", "CAIS", "gAAAA"]
            .iter()
            .any(|p| decoded.starts_with(p))
    {
        signature = decoded;
    }
    let (raw, kind) = if let Some(s) = signature.strip_prefix("claude#") {
        (s, "anthropic")
    } else if let Some(s) = signature.strip_prefix("gpt#") {
        (s, "openai")
    } else if let Some(s) = signature.strip_prefix("gemini#") {
        (s, "gemini")
    } else if signature.starts_with("CAQS") || signature.starts_with("CAIS") {
        (signature.as_str(), "anthropic")
    } else if signature.starts_with("gAAAA") {
        (signature.as_str(), "openai")
    } else if signature.starts_with("AY") {
        (signature.as_str(), "gemini")
    } else {
        (signature.as_str(), "sealed")
    };
    if !prompt.signature.is_empty() && prompt.signature != raw.as_bytes() {
        return Err(unsupported("multiple signatures in one assistant turn"));
    }
    prompt.signature = raw.as_bytes().to_vec();
    prompt.signature_type = kind.into();
    Ok(())
}

fn session_id(request: &AiRequest) -> String {
    let raw = request.meta.raw.as_ref();
    let body = raw.and_then(|r| r.body.as_ref());
    let id = [
        "session_id",
        "sessionId",
        "conversation_id",
        "previous_interaction_id",
    ]
    .iter()
    .find_map(|k| {
        body.and_then(|b| b.get(*k))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    })
    .or_else(|| {
        raw.and_then(|r| {
            ["x-session-id", "session-id", "x-conversation-id"]
                .iter()
                .find_map(|k| r.headers.get(*k).map(String::as_str))
        })
    });
    let Some(id) = id else {
        return uuid::Uuid::new_v4().to_string();
    };
    if let Ok(uuid) = uuid::Uuid::parse_str(id.trim()) {
        return uuid.to_string();
    }
    // UUID v5 / OID namespace, without requiring the uuid v5 feature.
    let mut hash = Sha1::new();
    hash.update(uuid::Uuid::NAMESPACE_OID.as_bytes());
    hash.update(id.trim().as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn message(role: Role, content: MessageContent) -> Message {
        Message {
            role,
            content,
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }
    fn request() -> AiRequest {
        AiRequest::new(
            "devin/swe-2",
            vec![message(Role::User, MessageContent::Text("Hello".into()))],
        )
    }
    fn decoded(request: &AiRequest) -> wire::ChatRequest {
        let bytes = encode(request, "not-a-real-token", "swe-2-high").unwrap();
        assert_eq!(bytes[0], 0);
        assert_eq!(
            u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize,
            bytes.len() - 5
        );
        wire::ChatRequest::decode(&bytes[5..]).unwrap()
    }
    #[test]
    fn native_metadata_fields_and_zero_temperature() {
        let mut request = request();
        request.generation.temperature = Some(0.0);
        request.system = Some("System instruction".into());
        let body = decoded(&request);
        let meta = body.metadata.unwrap();
        assert_eq!(
            (
                meta.client.as_str(),
                meta.version.as_str(),
                meta.token.as_str()
            ),
            ("chisel", "3000.10.21", "not-a-real-token")
        );
        assert_eq!(meta.fingerprint.len(), 732);
        assert!(meta.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(body.system, "System instruction");
        assert_eq!(body.model, "swe-2-high");
        assert_eq!(body.completion.unwrap().temperature, Some(0.0));
        assert_eq!(body.prompts[0].content.as_deref(), Some("Hello"));
        assert_eq!(body.thread.unwrap().user_boundary, 14);
        let unary = wire::ChatRequest::decode(user_status_request("mock").as_slice()).unwrap();
        assert_eq!(unary.metadata.unwrap().token, "mock");
        assert!(unary.prompts.is_empty());
    }
    #[test]
    fn preserves_tools_results_business_json_and_reasoning() {
        let mut request = request();
        request.messages.push(message(
            Role::Assistant,
            MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "Plan".into(),
                    signature: Some("claude#CAQS-sig".into()),
                },
                ContentBlock::ToolUse {
                    id: "call 1".into(),
                    name: "lookup".into(),
                    input: json!({"x":1}),
                    cache_control: None,
                },
            ]),
        ));
        request.messages.push(message(
            Role::User,
            MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call 1".into(),
                content: json!({"text":"business","other":1}),
                is_error: None,
                cache_control: None,
            }]),
        ));
        request.tools = Some(vec![ToolSpec {
            name: "lookup".into(),
            namespace: None,
            description: Some("Lookup".into()),
            kind: ToolSpecKind::Function,
            parameters: json!({"type":"object"}),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        let body = decoded(&request);
        assert_eq!(body.prompts[1].signature, b"CAQS-sig");
        assert_eq!(body.prompts[1].signature_type, "anthropic");
        assert_eq!(body.prompts[2].source, 4);
        assert_eq!(body.prompts[2].call_id, "call 1");
        assert_eq!(
            serde_json::from_str::<Value>(body.prompts[2].content.as_ref().unwrap()).unwrap(),
            json!({"text":"business","other":1})
        );
        assert_eq!(body.tools[0].parameters, br#"{"type":"object"}"#);
    }
    #[test]
    fn inline_images_and_unsupported_inputs() {
        let mut request = request();
        request.messages[0].content = MessageContent::Blocks(vec![ContentBlock::Image {
            source: MediaSource::Url("data:image/png;base64,aGk=".into()),
            cache_control: None,
        }]);
        let body = decoded(&request);
        assert_eq!(body.prompts[0].images[0].data, "aGk=");
        request.messages[0].content = MessageContent::Blocks(vec![ContentBlock::Image {
            source: MediaSource::Url("https://example.test/image.png".into()),
            cache_control: None,
        }]);
        assert!(encode(&request, "secret", "swe-2-high").is_err());
        request.messages[0].content = MessageContent::Text(" \n".into());
        assert!(encode(&request, "secret", "swe-2-high").is_err());
        request.messages[0].content = MessageContent::Text("hello".into());
        request.tool_choice = Some(ToolChoice::Required);
        let error = encode(&request, "secret", "swe-2-high").unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
    #[test]
    fn stable_session_ids_and_orphan_results() {
        let mut request = request();
        request.meta.raw = Some(RawEnvelope {
            body: Some(json!({"session_id":"conversation-example"})),
            ..Default::default()
        });
        assert_eq!(decoded(&request).cascade_id, decoded(&request).cascade_id);
        request.messages.push(message(
            Role::User,
            MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "orphan".into(),
                content: json!("result"),
                is_error: None,
                cache_control: None,
            }]),
        ));
        let body = decoded(&request);
        assert_eq!(body.prompts[1].source, 1);
        assert!(body.prompts[1].call_id.is_empty());
    }
}
