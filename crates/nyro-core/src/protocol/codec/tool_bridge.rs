//! Request-scoped bridging for tool families that are not shared by all protocols.
//!
//! OpenAI Responses custom tools accept arbitrary text input. Function-only
//! protocols receive an equivalent function with a single string field, then
//! responses are restored to custom-tool semantics before client formatting.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use crate::protocol::ids::{Protocol, ProtocolEndpoint};
use crate::protocol::ir::{
    AiRequest, AiResponse, AiStreamDelta, ResponseItem, ToolCall, ToolCallKind, ToolSpecKind,
};

const CUSTOM_TOOL_KIND_META: &str = "__nyro_tool_call_kind";
const CUSTOM_INPUT_FIELD: &str = "input";

#[derive(Debug, Clone, Default)]
pub struct ToolRoutePlan {
    custom_tool_names: HashSet<String>,
    bridge_custom_tools: bool,
    pending_stream_inputs: BTreeMap<usize, String>,
}

impl ToolRoutePlan {
    pub fn for_request(request: &AiRequest, egress: ProtocolEndpoint) -> Self {
        let mut custom_tool_names = HashSet::new();

        if let Some(tools) = &request.tools {
            custom_tool_names.extend(
                tools
                    .iter()
                    .filter(|tool| tool.is_custom())
                    .map(|tool| tool.name.clone()),
            );
        }
        for call in request
            .messages
            .iter()
            .filter_map(|message| message.tool_calls.as_deref())
            .flatten()
        {
            if call.is_custom() {
                custom_tool_names.insert(call.name.clone());
            }
        }

        Self {
            bridge_custom_tools: egress.protocol != Protocol::OpenAIResponses
                && !custom_tool_names.is_empty(),
            custom_tool_names,
            pending_stream_inputs: BTreeMap::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.bridge_custom_tools
    }

    pub fn prepare_upstream_request(&self, request: &mut AiRequest) {
        if !self.bridge_custom_tools {
            return;
        }

        if let Some(tools) = &mut request.tools {
            for tool in tools {
                if tool.is_custom() {
                    tool.kind = ToolSpecKind::Function;
                    tool.parameters = custom_tool_schema();
                    tool.strict = Some(true);
                }
            }
        }

        for message in &mut request.messages {
            if let Some(calls) = &mut message.tool_calls {
                for call in calls {
                    if call.is_custom() {
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
    }

    pub fn restore_response(&self, response: &mut AiResponse) {
        if !self.bridge_custom_tools {
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
                        arguments,
                    } if self.custom_tool_names.contains(name) => {
                        Some(ResponseItem::CustomToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            input: unwrap_custom_input(arguments),
                        })
                    }
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    *item = replacement;
                }
            }
        }
    }

    pub fn restore_stream_deltas(&mut self, deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        if !self.bridge_custom_tools {
            return deltas;
        }

        let mut restored = Vec::with_capacity(deltas.len());
        for delta in deltas {
            match delta {
                AiStreamDelta::ToolCallStart {
                    index,
                    id,
                    name,
                    kind: _,
                } if self.custom_tool_names.contains(&name) => {
                    self.pending_stream_inputs.entry(index).or_default();
                    restored.push(AiStreamDelta::ToolCallStart {
                        index,
                        id,
                        name,
                        kind: ToolCallKind::Custom,
                    });
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
                } if self.custom_tool_names.contains(&tool_call.name) => {
                    tool_call.kind = ToolCallKind::Custom;
                    tool_call.arguments = unwrap_custom_input(&tool_call.arguments);
                    self.pending_stream_inputs.remove(&index);
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
        if !self.bridge_custom_tools {
            return Vec::new();
        }
        self.flush_stream_inputs()
    }

    fn restore_tool_call(&self, call: &mut ToolCall) {
        if self.custom_tool_names.contains(&call.name) {
            call.kind = ToolCallKind::Custom;
            call.arguments = unwrap_custom_input(&call.arguments);
        }
    }

    fn flush_stream_inputs(&mut self) -> Vec<AiStreamDelta> {
        std::mem::take(&mut self.pending_stream_inputs)
            .into_iter()
            .map(|(index, arguments)| AiStreamDelta::ToolCallDelta {
                index,
                arguments: unwrap_custom_input(&arguments),
            })
            .collect()
    }
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
    use crate::protocol::ir::{Message, MessageContent, Role, StreamConfig, ToolSpec};

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
}
