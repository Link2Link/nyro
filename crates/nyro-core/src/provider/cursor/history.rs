//! Stateless replay: JSON prompt blobs + typed display turns, all content-addressed.
//! Caller configuration is represented as USER global rules, not fabricated
//! assistant acknowledgements. No host files, native tools, or persisted session.
use super::{
    errors::CursorError,
    framing::{MAX_BLOB_STORE, MAX_INFLATED_FRAME},
    wire,
};
use crate::protocol::ir::{AiRequest, ContentBlock, MessageContent, Role, ToolChoice};
use prost::Message as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Default)]
pub struct BlobStore {
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    total: usize,
}
impl BlobStore {
    pub fn insert(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, CursorError> {
        let id = Sha256::digest(&bytes).to_vec();
        self.set(id.clone(), bytes)?;
        Ok(id)
    }
    pub fn set(&mut self, id: Vec<u8>, bytes: Vec<u8>) -> Result<(), CursorError> {
        if id.len() != 32 || Sha256::digest(&bytes).as_slice() != id.as_slice() {
            return Err(CursorError::BlobHashMismatch);
        }
        if bytes.len() > MAX_INFLATED_FRAME - 1024 {
            return Err(CursorError::LimitExceeded {
                resource: "blob",
                limit: MAX_INFLATED_FRAME - 1024,
            });
        }
        if self.blobs.contains_key(&id) {
            return Ok(());
        }
        if bytes.len() > MAX_BLOB_STORE - self.total {
            return Err(CursorError::LimitExceeded {
                resource: "blob store",
                limit: MAX_BLOB_STORE,
            });
        }
        self.total += bytes.len();
        self.blobs.insert(id, bytes);
        Ok(())
    }
    pub fn get(&self, id: &[u8]) -> Result<&[u8], CursorError> {
        self.blobs
            .get(id)
            .map(Vec::as_slice)
            .ok_or(CursorError::MissingBlob)
    }
    #[cfg(test)]
    pub fn total_bytes(&self) -> usize {
        self.total
    }
    fn proto(&mut self, value: &impl prost::Message) -> Result<Vec<u8>, CursorError> {
        self.insert(value.encode_to_vec())
    }
    fn json(&mut self, value: &Value) -> Result<Vec<u8>, CursorError> {
        self.insert(serde_json::to_vec(value)?)
    }
}

pub struct BuiltHistory {
    pub state: wire::ConversationStateStructure,
    pub action: wire::ConversationAction,
    pub context: wire::RequestContext,
    pub blobs: BlobStore,
    pub tools: HashSet<String>,
}

/// Native first-version capability gate; callable before opening any transport.
pub fn preflight(request: &AiRequest) -> Result<(), CursorError> {
    if request.model.is_empty() {
        return Err(CursorError::InvalidRequest("model ID is required".into()));
    }
    if request.messages.is_empty() {
        return Err(CursorError::InvalidRequest("conversation is empty".into()));
    }
    let g = &request.generation;
    // Anthropic requires max_tokens. Accepted for compatibility, not enforced
    // by this protocol; callers must expose token_budget_not_enforced.
    if g.max_tokens == Some(0) {
        return Err(CursorError::InvalidRequest(
            "max_tokens must be positive".into(),
        ));
    }
    if g.temperature.is_some()
        || g.top_p.is_some()
        || g.seed.is_some()
        || g.stop.is_some()
        || g.presence_penalty.is_some()
        || g.frequency_penalty.is_some()
    {
        return Err(CursorError::Unsupported(
            "generation controls have no verified Run mapping".into(),
        ));
    }
    if request
        .response_format
        .as_ref()
        .is_some_and(|format| !matches!(format, crate::protocol::ir::ResponseFormat::Text))
        || request.safety_settings.is_some()
    {
        return Err(CursorError::Unsupported(
            "structured output or safety controls".into(),
        ));
    }
    if request.reasoning.enabled
        || request.reasoning.effort.is_some()
        || request.reasoning.budget_tokens.is_some()
        || request.reasoning.display.is_some()
    {
        return Err(CursorError::Unsupported(
            "reasoning controls require an authoritative catalog route".into(),
        ));
    }
    match &request.tool_choice {
        Some(ToolChoice::Raw(_))
        | Some(ToolChoice::Named {
            namespace: Some(_), ..
        }) => {
            return Err(CursorError::Unsupported(
                "raw or namespaced tool choice".into(),
            ));
        }
        Some(ToolChoice::Named { name, .. })
            if !request.tools.iter().flatten().any(|t| t.name == *name) =>
        {
            return Err(CursorError::InvalidRequest(
                "named tool choice is undeclared".into(),
            ));
        }
        Some(ToolChoice::Required) if request.tools.as_ref().is_none_or(Vec::is_empty) => {
            return Err(CursorError::InvalidRequest(
                "required tool choice has no tools".into(),
            ));
        }
        _ => {}
    }
    let mut names = HashSet::new();
    for tool in request.tools.iter().flatten() {
        if tool.is_custom() || tool.namespace.is_some() {
            return Err(CursorError::Unsupported(
                "custom or namespaced tools".into(),
            ));
        }
        if tool.name.is_empty() || !names.insert(&tool.name) {
            return Err(CursorError::InvalidRequest(
                "empty or duplicate tool name".into(),
            ));
        }
        if !tool.parameters.is_object() {
            return Err(CursorError::InvalidRequest(
                "tool schema must be an object".into(),
            ));
        }
        json_to_proto(&tool.parameters)?;
    }
    Ok(())
}

/// google.protobuf.Value mapping preserves null, booleans, arrays and objects.
/// JSON numbers outside exact f64 integer precision are rejected rather than rounded.
pub fn json_to_proto(value: &Value) -> Result<prost_types::Value, CursorError> {
    use prost_types::value::Kind;
    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(v) => Kind::BoolValue(*v),
        Value::String(v) => Kind::StringValue(v.clone()),
        Value::Number(v) => {
            let n = v.as_f64().ok_or_else(|| {
                CursorError::InvalidRequest("unrepresentable protobuf number".into())
            })?;
            if v.as_i64()
                .is_some_and(|i| (i as i128).abs() > (1i128 << 53))
                || v.as_u64().is_some_and(|i| i > (1u64 << 53))
            {
                return Err(CursorError::InvalidRequest(
                    "integer exceeds exact protobuf Value precision".into(),
                ));
            }
            Kind::NumberValue(n)
        }
        Value::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values.iter().map(json_to_proto).collect::<Result<_, _>>()?,
        }),
        Value::Object(values) => Kind::StructValue(prost_types::Struct {
            fields: values
                .iter()
                .map(|(k, v)| Ok((k.clone(), json_to_proto(v)?)))
                .collect::<Result<_, CursorError>>()?,
        }),
    };
    Ok(prost_types::Value { kind: Some(kind) })
}
pub fn proto_to_json(value: prost_types::Value) -> Result<Value, CursorError> {
    use prost_types::value::Kind;
    Ok(
        match value
            .kind
            .ok_or_else(|| CursorError::Protocol("MCP argument Value missing kind".into()))?
        {
            Kind::NullValue(0) => Value::Null,
            Kind::NullValue(_) => {
                return Err(CursorError::Protocol("invalid protobuf null enum".into()));
            }
            Kind::BoolValue(v) => Value::Bool(v),
            Kind::StringValue(v) => Value::String(v),
            Kind::NumberValue(v) => {
                if !v.is_finite() {
                    return Err(CursorError::Protocol("non-finite MCP number".into()));
                }
                if v.fract() == 0.0 {
                    if v.abs() > (1u64 << 53) as f64 {
                        return Err(CursorError::Protocol("inexact MCP integer".into()));
                    }
                    Value::Number(serde_json::Number::from(v as i64))
                } else {
                    Value::Number(serde_json::Number::from_f64(v).unwrap())
                }
            }
            Kind::ListValue(v) => Value::Array(
                v.values
                    .into_iter()
                    .map(proto_to_json)
                    .collect::<Result<_, _>>()?,
            ),
            Kind::StructValue(v) => Value::Object(
                v.fields
                    .into_iter()
                    .map(|(k, v)| Ok((k, proto_to_json(v)?)))
                    .collect::<Result<_, CursorError>>()?,
            ),
        },
    )
}
pub fn encode_arguments(args: &Value) -> Result<BTreeMap<String, Vec<u8>>, CursorError> {
    args.as_object()
        .ok_or_else(|| CursorError::InvalidRequest("tool arguments must be a JSON object".into()))?
        .iter()
        .map(|(k, v)| Ok((k.clone(), json_to_proto(v)?.encode_to_vec())))
        .collect()
}
pub fn decode_arguments(args: &BTreeMap<String, Vec<u8>>) -> Result<Value, CursorError> {
    Ok(Value::Object(
        args.iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    proto_to_json(prost_types::Value::decode(v.as_slice())?)?,
                ))
            })
            .collect::<Result<_, CursorError>>()?,
    ))
}

#[derive(Clone)]
enum Part {
    Text(String),
    Thinking(String),
    Call {
        id: String,
        name: String,
        args: Value,
    },
    Result {
        id: String,
        value: Value,
        error: bool,
    },
}
struct Entry {
    role: Role,
    parts: Vec<Part>,
}
fn normalize(request: &AiRequest) -> Result<(Vec<String>, Vec<Entry>), CursorError> {
    let mut systems: Vec<String> = request.system.iter().cloned().collect();
    let mut entries = Vec::new();
    for message in &request.messages {
        let mut parts = Vec::new();
        match &message.content {
            MessageContent::Text(text) => parts.push(Part::Text(text.clone())),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    parts.push(match block {
                        ContentBlock::Text { text, .. } => Part::Text(text.clone()),
                        ContentBlock::Thinking { thinking, .. }
                            if message.role == Role::Assistant =>
                        {
                            Part::Thinking(thinking.clone())
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } if message.role == Role::Assistant => Part::Call {
                            id: id.clone(),
                            name: name.clone(),
                            args: input.clone(),
                        },
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } if matches!(message.role, Role::User | Role::Tool) => Part::Result {
                            id: tool_use_id.clone(),
                            value: content.clone(),
                            error: is_error.unwrap_or(false),
                        },
                        _ => {
                            return Err(CursorError::Unsupported(
                                "multimodal, server tool, or unsupported history block".into(),
                            ));
                        }
                    });
                }
            }
        }
        for call in message.tool_calls.iter().flatten() {
            if call.is_custom() || call.namespace.is_some() || message.role != Role::Assistant {
                return Err(CursorError::Unsupported(
                    "custom, namespaced, or non-assistant tool call".into(),
                ));
            }
            parts.push(Part::Call {
                id: call.id.clone(),
                name: call.name.clone(),
                args: serde_json::from_str(&call.arguments).map_err(|_| {
                    CursorError::InvalidRequest("invalid tool arguments JSON in history".into())
                })?,
            });
        }
        if message.role == Role::Tool && !parts.iter().any(|p| matches!(p, Part::Result { .. })) {
            let id = message
                .tool_call_id
                .clone()
                .ok_or_else(|| CursorError::InvalidRequest("tool result missing call ID".into()))?;
            let value = match &message.content {
                MessageContent::Text(t) => Value::String(t.clone()),
                MessageContent::Blocks(_) => Value::String(
                    parts
                        .iter()
                        .filter_map(|p| {
                            if let Part::Text(s) = p {
                                Some(s.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
            };
            let error = message
                .meta
                .as_ref()
                .and_then(|m| m.get("is_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            parts = vec![Part::Result { id, value, error }];
        }
        if message.role == Role::System {
            systems.push(
                parts
                    .iter()
                    .map(|p| match p {
                        Part::Text(s) => Ok(s.as_str()),
                        _ => Err(CursorError::Unsupported(
                            "non-text system configuration".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join(""),
            );
        } else {
            entries.push(Entry {
                role: message.role,
                parts,
            });
        }
    }
    Ok((systems, entries))
}
fn text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| {
            if let Part::Text(t) = p {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}
fn result_text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        s.into()
    } else {
        value.to_string()
    }
}

pub fn build_history(request: &AiRequest) -> Result<BuiltHistory, CursorError> {
    preflight(request)?;
    let (systems, entries) = normalize(request)?;
    if entries.is_empty() {
        return Err(CursorError::InvalidRequest("no user conversation".into()));
    }
    let active = entries.last().filter(|e| {
        e.role == Role::User && !e.parts.iter().any(|p| matches!(p, Part::Result { .. }))
    });
    if active.is_none()
        && !entries
            .last()
            .is_some_and(|e| e.parts.iter().any(|p| matches!(p, Part::Result { .. })))
    {
        return Err(CursorError::Unsupported(
            "Run requires a current user message or trailing tool results".into(),
        ));
    }
    let end = entries.len() - usize::from(active.is_some());
    let history = &entries[..end];
    let mut calls = HashMap::<String, (String, Value)>::new();
    let mut results = HashMap::<String, (Value, bool)>::new();
    for entry in history {
        for part in &entry.parts {
            match part {
                Part::Call { id, name, args } => {
                    if id.is_empty()
                        || name.is_empty()
                        || calls
                            .insert(id.clone(), (name.clone(), args.clone()))
                            .is_some()
                    {
                        return Err(CursorError::InvalidRequest(
                            "empty or duplicate historical tool call ID".into(),
                        ));
                    }
                    encode_arguments(args)?;
                }
                Part::Result { id, value, error } => {
                    if !calls.contains_key(id) {
                        return Err(CursorError::InvalidRequest(
                            "orphan tool result in history".into(),
                        ));
                    }
                    if results
                        .insert(id.clone(), (value.clone(), *error))
                        .is_some()
                    {
                        return Err(CursorError::InvalidRequest(
                            "duplicate tool result in history".into(),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    if calls.keys().any(|id| !results.contains_key(id)) {
        return Err(CursorError::InvalidRequest(
            "unanswered tool call in history".into(),
        ));
    }
    let mut blobs = BlobStore::default();
    let mut root = Vec::new();
    let mut turns = Vec::new();
    for system in &systems {
        root.push(blobs.json(&json!({"role":"system","content":system}))?);
    }
    let mut turn: Option<wire::AgentConversationTurnStructure> = None;
    for (index, entry) in history.iter().enumerate() {
        // A user message containing only tool-result blocks is not a new user turn.
        let has_user_text =
            entry.parts.is_empty() || entry.parts.iter().any(|p| matches!(p, Part::Text(_)));
        if entry.role == Role::User && has_user_text {
            if let Some(previous) = turn.take() {
                turns.push(blobs.proto(&wire::ConversationTurnStructure {
                    agent_conversation_turn: Some(previous),
                })?);
            }
            let user = wire::UserMessage {
                text: text(&entry.parts),
                message_id: format!("history-user-{index}"),
            };
            turn = Some(wire::AgentConversationTurnStructure {
                user_message: blobs.proto(&user)?,
                steps: vec![],
            });
        }
        let mut content = Vec::new();
        for part in &entry.parts {
            match part {
                Part::Text(t) => content.push(json!({"type":"text","text":t})),
                Part::Thinking(t) => content.push(json!({"type":"reasoning","text":t})),
                Part::Call { id, name, args } => content
                    .push(json!({"type":"tool-call","toolCallId":id,"toolName":name,"args":args})),
                Part::Result { .. } => {}
            }
        }
        if !content.is_empty() || entry.parts.is_empty() {
            let role = match entry.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                _ => {
                    return Err(CursorError::InvalidRequest(
                        "non-result content in tool history".into(),
                    ));
                }
            };
            root.push(blobs.json(&json!({"role":role,"content":content}))?);
        }
        for part in &entry.parts {
            if let Part::Result { id, value, error } = part {
                root.push(blobs.json(&json!({"role":"tool","id":id,"content":[{"type":"tool-result","toolCallId":id,"toolName":calls[id].0,"result":value,"isError":error}]}))?);
            }
            let step = match part {
                Part::Text(t) if entry.role == Role::Assistant => {
                    Some(wire::conversation_step::Message::Assistant(
                        wire::TextMessage { text: t.clone() },
                    ))
                }
                Part::Thinking(t) => Some(wire::conversation_step::Message::Thinking(
                    wire::TextMessage { text: t.clone() },
                )),
                Part::Call { id, name, args } => {
                    let result = results.get(id).map(|(v, error)| wire::McpToolResult {
                        result: Some(if *error {
                            wire::mcp_tool_result::Result::Error(wire::ErrorText {
                                error: result_text(v),
                            })
                        } else {
                            wire::mcp_tool_result::Result::Success(wire::McpSuccess {
                                content: vec![wire::McpContent {
                                    text: Some(wire::TextMessage {
                                        text: result_text(v),
                                    }),
                                }],
                                is_error: false,
                            })
                        }),
                    });
                    Some(wire::conversation_step::Message::Tool(wire::ToolCall {
                        tool_call_id: Some(id.clone()),
                        mcp_tool_call: Some(wire::McpToolCall {
                            args: Some(wire::McpArgs {
                                name: name.clone(),
                                tool_name: name.clone(),
                                tool_call_id: id.clone(),
                                provider_identifier: wire::MCP_PROVIDER.into(),
                                args: encode_arguments(args)?,
                                ..Default::default()
                            }),
                            result,
                        }),
                    }))
                }
                _ => None,
            };
            if let Some(step) = step {
                let turn = turn.as_mut().ok_or_else(|| {
                    CursorError::InvalidRequest(
                        "assistant history has no preceding user turn".into(),
                    )
                })?;
                turn.steps.push(blobs.proto(&wire::ConversationStep {
                    message: Some(step),
                })?);
            }
        }
        if entry.role == Role::Assistant && entry.parts.is_empty() {
            let turn = turn.as_mut().ok_or_else(|| {
                CursorError::InvalidRequest("assistant history has no preceding user turn".into())
            })?;
            turn.steps.push(blobs.proto(&wire::ConversationStep {
                message: Some(wire::conversation_step::Message::Assistant(
                    wire::TextMessage {
                        text: String::new(),
                    },
                )),
            })?);
        }
    }
    if let Some(turn) = turn {
        turns.push(blobs.proto(&wire::ConversationTurnStructure {
            agent_conversation_turn: Some(turn),
        })?);
    }
    let action = if let Some(entry) = active {
        wire::conversation_action::Action::User(wire::UserMessageAction {
            user_message: Some(wire::UserMessage {
                text: text(&entry.parts),
                message_id: uuid::Uuid::new_v4().to_string(),
            }),
        })
    } else {
        wire::conversation_action::Action::Resume(wire::ResumeAction {})
    };
    let mut definitions = Vec::new();
    let mut tools = HashSet::new();
    if !matches!(request.tool_choice, Some(ToolChoice::None)) {
        for tool in request.tools.iter().flatten() {
            if let Some(ToolChoice::Named { name, .. }) = &request.tool_choice {
                if &tool.name != name {
                    continue;
                }
            }
            tools.insert(tool.name.clone());
            definitions.push(wire::McpToolDefinition {
                name: tool.name.clone(),
                tool_name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
                provider_identifier: wire::MCP_PROVIDER.into(),
                input_schema: json_to_proto(&tool.parameters)?.encode_to_vec(),
            });
        }
    }
    let rules = systems
        .into_iter()
        .enumerate()
        .map(|(i, content)| wire::CursorRule {
            // Protocol label only. Nothing is read from or written to this path.
            full_path: format!("/nyro/caller-configuration/{i}.mdc"),
            content,
            source: 2,
            r#type: Some(wire::CursorRuleType {
                global: Some(wire::Empty {}),
            }),
        })
        .collect();
    Ok(BuiltHistory {
        state: wire::ConversationStateStructure {
            root_prompt_messages_json: root,
            turns,
        },
        action: wire::ConversationAction {
            action: Some(action),
        },
        context: wire::RequestContext {
            rules,
            tools: definitions,
            web_search_enabled: Some(false),
        },
        blobs,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{Message, ToolCall};
    fn msg(role: Role, s: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(s.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }
    fn history_request(error: bool) -> AiRequest {
        let mut assistant = msg(Role::Assistant, "");
        assistant.tool_calls = Some(vec![ToolCall::function(
            "call-1",
            "lookup",
            r#"{"x":null,"v":[true,2]}"#,
        )]);
        let mut result = msg(Role::Tool, "");
        result.tool_call_id = Some("call-1".into());
        result.meta = Some(json!({"is_error":error}));
        let mut req = AiRequest::new(
            "exact-model",
            vec![msg(Role::User, "previous"), assistant, result],
        );
        req.system = Some("caller policy".into());
        req
    }
    #[test]
    fn complete_resume_keeps_empty_results_and_typed_errors() {
        for error in [false, true] {
            let built = build_history(&history_request(error)).unwrap();
            assert!(matches!(
                built.action.action,
                Some(wire::conversation_action::Action::Resume(_))
            ));
            let root: Vec<Value> = built
                .state
                .root_prompt_messages_json
                .iter()
                .map(|id| serde_json::from_slice(built.blobs.get(id).unwrap()).unwrap())
                .collect();
            assert_eq!(root[0], json!({"role":"system","content":"caller policy"}));
            assert_eq!(root.last().unwrap()["content"][0]["result"], "");
            assert_eq!(root.last().unwrap()["content"][0]["isError"], error);
            let turn = wire::ConversationTurnStructure::decode(
                built.blobs.get(&built.state.turns[0]).unwrap(),
            )
            .unwrap()
            .agent_conversation_turn
            .unwrap();
            let step = wire::ConversationStep::decode(
                built.blobs.get(turn.steps.last().unwrap()).unwrap(),
            )
            .unwrap();
            let Some(wire::conversation_step::Message::Tool(call)) = step.message else {
                panic!()
            };
            assert_eq!(
                matches!(
                    call.mcp_tool_call.unwrap().result.unwrap().result,
                    Some(wire::mcp_tool_result::Result::Error(_))
                ),
                error
            );
            assert_eq!(built.context.rules[0].source, 2);
        }
    }
    #[test]
    fn latest_user_is_action_not_duplicated_and_empty_user_preserved() {
        let mut req = history_request(false);
        req.messages.push(msg(Role::User, ""));
        let built = build_history(&req).unwrap();
        let Some(wire::conversation_action::Action::User(action)) = built.action.action else {
            panic!()
        };
        assert_eq!(action.user_message.unwrap().text, "");
        assert_eq!(built.state.turns.len(), 1);
        assert_eq!(built.state.root_prompt_messages_json.len(), 4);
    }
    #[test]
    fn value_not_struct_or_json_and_bad_args_fail() {
        let value = json!({"a":[true,null,"s",{"n":1.25}],"empty":{}});
        assert_eq!(
            decode_arguments(&encode_arguments(&value).unwrap()).unwrap(),
            value
        );
        assert!(encode_arguments(&json!([])).is_err());
        assert!(decode_arguments(&BTreeMap::from([("x".into(), vec![0xff])])).is_err());
        assert!(decode_arguments(&BTreeMap::from([("x".into(), vec![])])).is_err());
        assert!(json_to_proto(&json!(9007199254740993u64)).is_err());
    }
    #[test]
    fn content_hash_and_missing_blob_are_checked() {
        let mut store = BlobStore::default();
        let id = store.insert(b"abc".to_vec()).unwrap();
        assert_eq!(id, Sha256::digest(b"abc").to_vec());
        store.insert(b"abc".to_vec()).unwrap();
        assert_eq!(store.total_bytes(), 3);
        assert_eq!(store.get(&[0; 32]), Err(CursorError::MissingBlob));
        assert_eq!(
            store.set(vec![0; 32], b"abc".to_vec()),
            Err(CursorError::BlobHashMismatch)
        );
    }
    #[test]
    fn rejects_orphan_results_and_unsupported_blocks() {
        let mut req = history_request(false);
        req.messages.remove(1);
        assert!(build_history(&req).is_err());
        let mut req = AiRequest::new("x", vec![msg(Role::User, "hi")]);
        req.generation.max_tokens = Some(5);
        assert!(preflight(&req).is_ok());
        req.generation.max_tokens = Some(0);
        assert!(preflight(&req).is_err());
    }
}
