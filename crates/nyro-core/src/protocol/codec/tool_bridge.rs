//! Request-scoped bridging for tool families that are not shared by all protocols.
//!
//! OpenAI Responses custom tools accept arbitrary text input. Function-only
//! protocols receive an equivalent function with a single string field, then
//! responses are restored to custom-tool semantics before client formatting.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::protocol::ids::{Protocol, ProtocolEndpoint};
use crate::protocol::ir::{
    AiRequest, AiResponse, AiStreamDelta, ResponseItem, ToolCall, ToolCallKind, ToolSpecKind,
};

const CUSTOM_TOOL_KIND_META: &str = "__nyro_tool_call_kind";
const CUSTOM_INPUT_FIELD: &str = "input";
const MAX_WIRE_TOOL_NAME_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToolIdentity {
    namespace: Option<String>,
    name: String,
    kind: ToolCallKind,
}

impl ToolIdentity {
    fn from_spec(tool: &crate::protocol::ir::ToolSpec) -> Self {
        Self {
            namespace: tool.namespace.clone(),
            name: tool.name.clone(),
            kind: if tool.is_custom() {
                ToolCallKind::Custom
            } else {
                ToolCallKind::Function
            },
        }
    }

    fn from_call(call: &ToolCall) -> Self {
        Self {
            namespace: call.namespace.clone(),
            name: call.name.clone(),
            kind: call.kind,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolRoutePlan {
    logical_to_wire: HashMap<ToolIdentity, String>,
    wire_to_logical: HashMap<String, ToolIdentity>,
    flatten_namespaces: bool,
    bridge_custom_tools: bool,
    active: bool,
    pending_stream_inputs: BTreeMap<usize, String>,
    pending_stream_routes: BTreeMap<usize, ToolIdentity>,
}

impl ToolRoutePlan {
    pub fn for_request(request: &AiRequest, egress: ProtocolEndpoint) -> Self {
        let flatten_namespaces = egress.protocol != Protocol::OpenAIResponses;
        if !flatten_namespaces {
            return Self::default();
        }

        let mut identities = BTreeSet::new();

        if let Some(tools) = &request.tools {
            identities.extend(tools.iter().map(ToolIdentity::from_spec));
        }
        for call in request
            .messages
            .iter()
            .filter_map(|message| message.tool_calls.as_deref())
            .flatten()
        {
            identities.insert(ToolIdentity::from_call(call));
        }

        let bridge_custom_tools = identities
            .iter()
            .any(|identity| identity.kind == ToolCallKind::Custom);
        let has_namespaces = identities
            .iter()
            .any(|identity| identity.namespace.is_some());
        let (logical_to_wire, wire_to_logical) = build_route_maps(&identities);
        let renamed = logical_to_wire
            .iter()
            .any(|(identity, wire_name)| identity.name != *wire_name);

        Self {
            logical_to_wire,
            wire_to_logical,
            flatten_namespaces,
            bridge_custom_tools,
            active: has_namespaces || renamed || bridge_custom_tools,
            pending_stream_inputs: BTreeMap::new(),
            pending_stream_routes: BTreeMap::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn prepare_upstream_request(&self, request: &mut AiRequest) {
        if !self.active {
            return;
        }

        let tool_choice_wire_name = match &request.tool_choice {
            Some(crate::protocol::ir::ToolChoice::Named { name, namespace }) => self
                .logical_to_wire
                .iter()
                .find(|(identity, _)| identity.name == *name && identity.namespace == *namespace)
                .map(|(_, wire_name)| wire_name.clone()),
            _ => None,
        };

        if let Some(tools) = &mut request.tools {
            for tool in tools {
                let identity = ToolIdentity::from_spec(tool);
                if self.flatten_namespaces {
                    if let Some(wire_name) = self.logical_to_wire.get(&identity) {
                        tool.name = wire_name.clone();
                    }
                    tool.namespace = None;
                }
                if self.bridge_custom_tools && tool.is_custom() {
                    tool.kind = ToolSpecKind::Function;
                    tool.parameters = custom_tool_schema();
                    tool.strict = Some(true);
                }
            }
        }

        for message in &mut request.messages {
            if let Some(calls) = &mut message.tool_calls {
                for call in calls {
                    let identity = ToolIdentity::from_call(call);
                    if self.flatten_namespaces {
                        if let Some(wire_name) = self.logical_to_wire.get(&identity) {
                            call.name = wire_name.clone();
                        }
                        call.namespace = None;
                    }
                    if self.bridge_custom_tools && call.is_custom() {
                        call.kind = ToolCallKind::Function;
                        call.arguments = wrap_custom_input(&call.arguments);
                    }
                }
            }

            if let Some(meta) = message.meta.as_mut().and_then(Value::as_object_mut) {
                meta.remove(CUSTOM_TOOL_KIND_META);
                if meta.is_empty() {
                    message.meta = None;
                }
            }
        }

        if let (Some(crate::protocol::ir::ToolChoice::Named { name, namespace }), Some(wire_name)) =
            (&mut request.tool_choice, tool_choice_wire_name)
        {
            *name = wire_name;
            *namespace = None;
        }
    }

    pub fn restore_response(&self, response: &mut AiResponse) {
        if !self.active {
            return;
        }

        for call in &mut response.tool_calls {
            self.restore_tool_call(call);
        }

        if let Some(items) = &mut response.items {
            for item in items {
                let replacement = match item {
                    ResponseItem::FunctionCall {
                        call_id,
                        name,
                        namespace,
                        arguments,
                    } => self.restore_response_item(
                        call_id,
                        name,
                        namespace,
                        ToolCallKind::Function,
                        arguments,
                    ),
                    ResponseItem::CustomToolCall {
                        call_id,
                        name,
                        namespace,
                        input,
                    } => self.restore_response_item(
                        call_id,
                        name,
                        namespace,
                        ToolCallKind::Custom,
                        input,
                    ),
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    *item = replacement;
                }
            }
        }
    }

    pub fn restore_stream_deltas(&mut self, deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        if !self.active {
            return deltas;
        }

        let mut restored = Vec::with_capacity(deltas.len());
        for delta in deltas {
            match delta {
                AiStreamDelta::ToolCallStart {
                    index,
                    id,
                    name,
                    namespace,
                    kind,
                } => {
                    if let Some(identity) = self.wire_to_logical.get(&name).cloned() {
                        if identity.kind == ToolCallKind::Custom {
                            self.pending_stream_inputs.entry(index).or_default();
                        }
                        self.pending_stream_routes.insert(index, identity.clone());
                        restored.push(AiStreamDelta::ToolCallStart {
                            index,
                            id,
                            name: identity.name,
                            namespace: identity.namespace,
                            kind: identity.kind,
                        });
                    } else {
                        restored.push(AiStreamDelta::ToolCallStart {
                            index,
                            id,
                            name,
                            namespace,
                            kind,
                        });
                    }
                }
                AiStreamDelta::ToolCallDelta { index, arguments }
                    if self.pending_stream_inputs.contains_key(&index) =>
                {
                    self.pending_stream_inputs
                        .entry(index)
                        .or_default()
                        .push_str(&arguments);
                }
                AiStreamDelta::ToolCallComplete {
                    index,
                    mut tool_call,
                } if self.pending_stream_routes.contains_key(&index)
                    || self.wire_to_logical.contains_key(&tool_call.name) =>
                {
                    let identity = self
                        .pending_stream_routes
                        .remove(&index)
                        .or_else(|| self.wire_to_logical.get(&tool_call.name).cloned())
                        .expect("tool route checked above");
                    tool_call.name = identity.name;
                    tool_call.namespace = identity.namespace;
                    tool_call.kind = identity.kind;
                    if identity.kind == ToolCallKind::Custom {
                        let buffered = self.pending_stream_inputs.remove(&index);
                        let wrapped = if tool_call.arguments.is_empty() {
                            buffered.as_deref().unwrap_or("")
                        } else {
                            &tool_call.arguments
                        };
                        tool_call.arguments = unwrap_custom_input(wrapped);
                    }
                    restored.push(AiStreamDelta::ToolCallComplete { index, tool_call });
                }
                AiStreamDelta::Done { stop_reason } => {
                    restored.extend(self.flush_stream_inputs());
                    restored.push(AiStreamDelta::Done { stop_reason });
                }
                other => restored.push(other),
            }
        }
        restored
    }

    pub fn finish_stream(&mut self) -> Vec<AiStreamDelta> {
        if !self.active {
            return Vec::new();
        }
        self.flush_stream_inputs()
    }

    fn restore_tool_call(&self, call: &mut ToolCall) -> bool {
        let Some(identity) = self.wire_to_logical.get(&call.name) else {
            return false;
        };
        call.name = identity.name.clone();
        call.namespace = identity.namespace.clone();
        call.kind = identity.kind;
        if identity.kind == ToolCallKind::Custom {
            call.arguments = unwrap_custom_input(&call.arguments);
        }
        true
    }

    fn restore_response_item(
        &self,
        call_id: &str,
        name: &str,
        namespace: &Option<String>,
        kind: ToolCallKind,
        arguments: &str,
    ) -> Option<ResponseItem> {
        let mut call = ToolCall {
            id: call_id.to_string(),
            name: name.to_string(),
            namespace: namespace.clone(),
            kind,
            arguments: arguments.to_string(),
        };
        self.restore_tool_call(&mut call)
            .then_some(match call.kind {
                ToolCallKind::Function => ResponseItem::FunctionCall {
                    call_id: call.id,
                    name: call.name,
                    namespace: call.namespace,
                    arguments: call.arguments,
                },
                ToolCallKind::Custom => ResponseItem::CustomToolCall {
                    call_id: call.id,
                    name: call.name,
                    namespace: call.namespace,
                    input: call.arguments,
                },
            })
    }

    fn flush_stream_inputs(&mut self) -> Vec<AiStreamDelta> {
        let deltas = std::mem::take(&mut self.pending_stream_inputs)
            .into_iter()
            .map(|(index, arguments)| AiStreamDelta::ToolCallDelta {
                index,
                arguments: unwrap_custom_input(&arguments),
            })
            .collect();
        self.pending_stream_routes.clear();
        deltas
    }
}

fn build_route_maps(
    identities: &BTreeSet<ToolIdentity>,
) -> (HashMap<ToolIdentity, String>, HashMap<String, ToolIdentity>) {
    let mut leaf_counts: HashMap<&str, usize> = HashMap::new();
    for identity in identities {
        *leaf_counts.entry(identity.name.as_str()).or_default() += 1;
    }

    let mut logical_to_wire = HashMap::new();
    let mut wire_to_logical = HashMap::new();
    let mut used_names = HashSet::new();

    // Reserve real leaf names before generating namespace aliases. Otherwise a
    // generated `crm__lookup` could steal an explicitly declared tool with that name.
    for identity in identities {
        let leaf_is_unique = leaf_counts.get(identity.name.as_str()) == Some(&1);
        if (identity.namespace.is_none() || leaf_is_unique)
            && is_valid_wire_name(&identity.name)
            && used_names.insert(identity.name.clone())
        {
            logical_to_wire.insert(identity.clone(), identity.name.clone());
            wire_to_logical.insert(identity.name.clone(), identity.clone());
        }
    }

    for identity in identities {
        if logical_to_wire.contains_key(identity) {
            continue;
        }
        let leaf_is_unique = leaf_counts.get(identity.name.as_str()) == Some(&1);
        let preferred = if identity.namespace.is_none() || leaf_is_unique {
            identity.name.clone()
        } else {
            format!(
                "{}__{}",
                sanitize_wire_component(identity.namespace.as_deref().unwrap_or("tool")),
                sanitize_wire_component(&identity.name)
            )
        };
        let wire_name = allocate_wire_name(&preferred, identity, &mut used_names);
        logical_to_wire.insert(identity.clone(), wire_name.clone());
        wire_to_logical.insert(wire_name, identity.clone());
    }

    (logical_to_wire, wire_to_logical)
}

fn is_valid_wire_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_WIRE_TOOL_NAME_LEN
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn allocate_wire_name(
    preferred: &str,
    identity: &ToolIdentity,
    used_names: &mut HashSet<String>,
) -> String {
    let normalized = sanitize_wire_component(preferred);
    if normalized.len() <= MAX_WIRE_TOOL_NAME_LEN && used_names.insert(normalized.clone()) {
        return normalized;
    }

    for attempt in 0_u32.. {
        let suffix = stable_route_suffix(identity, attempt);
        let prefix_len = MAX_WIRE_TOOL_NAME_LEN - suffix.len() - 2;
        let prefix = &normalized[..normalized.len().min(prefix_len)];
        let candidate = format!("{prefix}__{suffix}");
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded alias attempts must produce a unique name")
}

fn sanitize_wire_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "tool".to_string()
    } else {
        sanitized
    }
}

fn stable_route_suffix(identity: &ToolIdentity, attempt: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.namespace.as_deref().unwrap_or(""));
    hasher.update([0]);
    hasher.update(&identity.name);
    hasher.update([0]);
    hasher.update(match identity.kind {
        ToolCallKind::Function => b"function".as_slice(),
        ToolCallKind::Custom => b"custom".as_slice(),
    });
    hasher.update(attempt.to_be_bytes());
    hasher
        .finalize()
        .iter()
        .take(5)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn custom_tool_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            CUSTOM_INPUT_FIELD: {
                "type": "string"
            }
        },
        "required": [CUSTOM_INPUT_FIELD],
        "additionalProperties": false
    })
}

fn wrap_custom_input(input: &str) -> String {
    serde_json::to_string(&serde_json::json!({CUSTOM_INPUT_FIELD: input}))
        .expect("serializing a string-valued custom tool wrapper cannot fail")
}

fn unwrap_custom_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get(CUSTOM_INPUT_FIELD)
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| arguments.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use crate::protocol::ir::{Message, MessageContent, Role, StreamConfig, ToolChoice, ToolSpec};

    fn namespaced_function(namespace: &str, name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            namespace: Some(namespace.into()),
            description: Some(description.into()),
            kind: ToolSpecKind::Function,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            strict: None,
            cache_control: None,
            meta: None,
        }
    }

    fn custom_request() -> AiRequest {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall::custom(
                    "call_exec",
                    "exec",
                    "const value = \"quoted\";",
                )]),
                tool_call_id: None,
                meta: None,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::Text("ok".into()),
                tool_calls: None,
                tool_call_id: Some("call_exec".into()),
                meta: Some(serde_json::json!({CUSTOM_TOOL_KIND_META: "custom"})),
            },
        ];
        let mut request = AiRequest::new("model", messages);
        request.stream = StreamConfig::default();
        request.tools = Some(vec![ToolSpec {
            name: "exec".into(),
            namespace: None,
            description: Some("Run source".into()),
            kind: ToolSpecKind::Custom { format: None },
            parameters: Value::Object(Default::default()),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        request
    }

    #[test]
    fn bridges_custom_definition_history_and_internal_meta() {
        let mut request = custom_request();
        let plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

        assert!(plan.is_active());
        plan.prepare_upstream_request(&mut request);

        let tool = &request.tools.as_ref().unwrap()[0];
        assert!(!tool.is_custom());
        assert_eq!(tool.parameters["properties"]["input"]["type"], "string");
        assert_eq!(tool.parameters["additionalProperties"], false);

        let call = &request.messages[0].tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.kind, ToolCallKind::Function);
        assert_eq!(
            serde_json::from_str::<Value>(&call.arguments).unwrap()["input"],
            "const value = \"quoted\";"
        );
        assert!(request.messages[1].meta.is_none());
    }

    #[test]
    fn restores_non_stream_custom_call() {
        let request = custom_request();
        let plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        let mut response = AiResponse::new("id", "model");
        response.tool_calls = vec![ToolCall::function(
            "call_exec",
            "exec",
            r#"{"input":"console.log(1);"}"#,
        )];
        response.items = Some(vec![ResponseItem::FunctionCall {
            call_id: "call_exec".into(),
            name: "exec".into(),
            namespace: None,
            arguments: r#"{"input":"console.log(1);"}"#.into(),
        }]);

        plan.restore_response(&mut response);

        assert_eq!(response.tool_calls[0].kind, ToolCallKind::Custom);
        assert_eq!(response.tool_calls[0].arguments, "console.log(1);");
        assert!(matches!(
            &response.items.as_ref().unwrap()[0],
            ResponseItem::CustomToolCall { input, .. } if input == "console.log(1);"
        ));
    }

    #[test]
    fn restores_fragmented_stream_input_before_done() {
        let request = custom_request();
        let mut plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        let restored = plan.restore_stream_deltas(vec![
            AiStreamDelta::ToolCallStart {
                index: 2,
                id: "call_exec".into(),
                name: "exec".into(),
                namespace: None,
                kind: ToolCallKind::Function,
            },
            AiStreamDelta::ToolCallDelta {
                index: 2,
                arguments: r#"{"in"#.into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 2,
                arguments: r#"put":"a\"b"}"#.into(),
            },
            AiStreamDelta::Done {
                stop_reason: "tool_calls".into(),
            },
        ]);

        assert!(matches!(
            &restored[0],
            AiStreamDelta::ToolCallStart {
                index: 2,
                kind: ToolCallKind::Custom,
                ..
            }
        ));
        assert!(matches!(
            &restored[1],
            AiStreamDelta::ToolCallDelta { index: 2, arguments }
                if arguments == "a\"b"
        ));
        assert!(matches!(&restored[2], AiStreamDelta::Done { .. }));
    }

    #[test]
    fn restores_fragmented_stream_input_on_eof() {
        let request = custom_request();
        let mut plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        let restored = plan.restore_stream_deltas(vec![
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_exec".into(),
                name: "exec".into(),
                namespace: None,
                kind: ToolCallKind::Function,
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: r#"{"input":"console."#.into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: r#"log(1);"}"#.into(),
            },
        ]);

        assert!(matches!(
            restored.as_slice(),
            [AiStreamDelta::ToolCallStart {
                kind: ToolCallKind::Custom,
                ..
            }]
        ));
        assert!(matches!(
            plan.finish_stream().as_slice(),
            [AiStreamDelta::ToolCallDelta { index: 0, arguments }]
                if arguments == "console.log(1);"
        ));
    }

    #[test]
    fn namespace_aliases_are_safe_bounded_and_deterministic() {
        let leaf_name = format!("lookup/customer/{}", "x".repeat(80));
        let mut request = AiRequest::new(
            "model",
            vec![Message {
                role: Role::User,
                content: MessageContent::Text("lookup".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.tools = Some(vec![
            namespaced_function("crm east", &leaf_name, "crm"),
            namespaced_function("crm/east", &leaf_name, "support"),
        ]);

        let prepare_names = |request: &AiRequest| {
            let plan = ToolRoutePlan::for_request(request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
            let mut prepared = request.clone();
            plan.prepare_upstream_request(&mut prepared);
            prepared
                .tools
                .unwrap()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        };

        let first = prepare_names(&request);
        let second = prepare_names(&request);
        assert_eq!(first, second);
        assert_ne!(first[0], first[1]);
        assert!(first.iter().all(|name| {
            !name.is_empty()
                && name.len() <= MAX_WIRE_TOOL_NAME_LEN
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        }));
    }

    #[test]
    fn generated_namespace_alias_does_not_steal_a_real_leaf_name() {
        let mut request = AiRequest::new("model", Vec::new());
        request.tools = Some(vec![
            namespaced_function("crm", "lookup", "crm"),
            namespaced_function("support", "lookup", "support"),
            namespaced_function("audit", "crm__lookup", "literal"),
        ]);

        let plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        plan.prepare_upstream_request(&mut request);
        let tools = request.tools.as_ref().unwrap();
        let alias_for = |description: &str| {
            tools
                .iter()
                .find(|tool| tool.description.as_deref() == Some(description))
                .map(|tool| tool.name.as_str())
                .unwrap()
        };

        assert_eq!(alias_for("literal"), "crm__lookup");
        assert_ne!(alias_for("crm"), "crm__lookup");
        assert_ne!(alias_for("crm"), alias_for("support"));
    }

    #[test]
    fn namespace_tool_choice_uses_the_selected_tool_alias() {
        let mut request = AiRequest::new(
            "model",
            vec![Message {
                role: Role::User,
                content: MessageContent::Text("lookup".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.tools = Some(vec![
            namespaced_function("crm", "lookup_customer", "crm"),
            namespaced_function("support", "lookup_customer", "support"),
        ]);
        request.tool_choice = Some(ToolChoice::Named {
            name: "lookup_customer".into(),
            namespace: Some("crm".into()),
        });

        let plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        plan.prepare_upstream_request(&mut request);

        let tools = request.tools.as_ref().unwrap();
        let crm_alias = tools
            .iter()
            .find(|tool| tool.description.as_deref() == Some("crm"))
            .map(|tool| tool.name.as_str())
            .unwrap();
        let support_alias = tools
            .iter()
            .find(|tool| tool.description.as_deref() == Some("support"))
            .map(|tool| tool.name.as_str())
            .unwrap();
        assert_ne!(crm_alias, support_alias);
        assert!(matches!(
            request.tool_choice,
            Some(ToolChoice::Named { ref name, namespace: None }) if name == crm_alias
        ));
    }

    #[test]
    fn unknown_upstream_tool_name_is_not_guessed_as_a_namespace_alias() {
        let mut request = AiRequest::new("model", Vec::new());
        request.tools = Some(vec![namespaced_function("crm", "lookup_customer", "crm")]);
        let plan = ToolRoutePlan::for_request(&request, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        let mut response = AiResponse::new("id", "model");
        response.tool_calls = vec![ToolCall::function(
            "call_unknown",
            "crm__invented_tool",
            "{}",
        )];

        plan.restore_response(&mut response);

        assert_eq!(response.tool_calls[0].name, "crm__invented_tool");
        assert_eq!(response.tool_calls[0].namespace, None);
        assert_eq!(response.tool_calls[0].kind, ToolCallKind::Function);
    }
}
