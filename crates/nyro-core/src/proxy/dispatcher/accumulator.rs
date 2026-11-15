//! Stream response accumulator: buffers streaming deltas into a complete
//! `AiResponse` for caching and formatted response aggregation.

use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::response::ResponseItem;
use crate::protocol::ir::{AiResponse, AiStreamDelta, Usage};

#[derive(Default)]
pub(super) struct StreamResponseAccumulator {
    pub(super) id: String,
    pub(super) model: String,
    pub(super) content: String,
    pub(super) reasoning_content: String,
    pub(super) reasoning_signature: String,
    pub(super) tool_calls: Vec<Option<ToolCall>>,
    pub(super) unknown_items: Vec<ResponseItem>,
    /// Gemini response metadata smuggled through `Unknown` deltas. It is not
    /// an output item: it belongs where the non-stream Gemini parser puts it.
    pub(super) google_response_metadata: serde_json::Map<String, serde_json::Value>,
    pub(super) stop_reason: Option<String>,
    pub(super) usage: Usage,
}

impl StreamResponseAccumulator {
    pub(super) fn apply_all(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            self.apply(delta);
        }
    }

    pub(super) fn apply(&mut self, delta: &AiStreamDelta) {
        match delta {
            AiStreamDelta::MessageStart { id, model } => {
                if self.id.is_empty() {
                    self.id = id.clone();
                }
                if self.model.is_empty() {
                    self.model = model.clone();
                }
            }
            AiStreamDelta::ThinkingDelta(text) => self.reasoning_content.push_str(text),
            AiStreamDelta::ThinkingSignature(sig) => self.reasoning_signature.push_str(sig),
            AiStreamDelta::TextDelta(text) => self.content.push_str(text),
            AiStreamDelta::ToolCallStart {
                index,
                id,
                name,
                namespace,
                kind,
            } => {
                ensure_tool_index(&mut self.tool_calls, *index);
                self.tool_calls[*index] = Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    namespace: namespace.clone(),
                    kind: *kind,
                    arguments: String::new(),
                });
            }
            AiStreamDelta::ToolCallDelta { index, arguments } => {
                ensure_tool_index(&mut self.tool_calls, *index);
                if let Some(tc) = self.tool_calls[*index].as_mut() {
                    tc.arguments.push_str(arguments);
                } else {
                    self.tool_calls[*index] = Some(ToolCall {
                        id: format!("tool-{index}"),
                        name: String::new(),
                        namespace: None,
                        kind: Default::default(),
                        arguments: arguments.clone(),
                    });
                }
            }
            AiStreamDelta::ToolCallComplete { index, tool_call } => {
                ensure_tool_index(&mut self.tool_calls, *index);
                self.tool_calls[*index] = Some(tool_call.clone());
            }
            AiStreamDelta::Usage(usage) => self.usage.merge_partial(usage),
            AiStreamDelta::Done { stop_reason } => self.stop_reason = Some(stop_reason.clone()),
            AiStreamDelta::StreamError { error } => {
                self.stop_reason = Some("error".to_string());
                tracing::warn!(error = ?error, "stream error delta received");
            }
            AiStreamDelta::UnexpectedEof => {
                if self.stop_reason.is_none() {
                    self.stop_reason = Some("error".to_string());
                }
            }
            AiStreamDelta::Unknown { raw } => {
                let Ok(raw) = serde_json::from_str::<serde_json::Value>(raw) else {
                    return;
                };
                if let Some(metadata) = raw
                    .as_object()
                    .filter(|object| object.len() == 1)
                    .and_then(|object| object.get("__google_response_metadata"))
                    .and_then(serde_json::Value::as_object)
                {
                    // Later frames refine earlier snapshots (usage counters
                    // grow); nested objects merge one level, scalars replace.
                    for (key, value) in metadata {
                        match (self.google_response_metadata.get_mut(key), value) {
                            (
                                Some(serde_json::Value::Object(old)),
                                serde_json::Value::Object(new),
                            ) => {
                                old.extend(new.clone());
                            }
                            _ => {
                                self.google_response_metadata
                                    .insert(key.clone(), value.clone());
                            }
                        }
                    }
                    return;
                }
                self.unknown_items.push(ResponseItem::Unknown { raw });
            }
        }
    }

    pub(super) fn into_ai_response(self) -> AiResponse {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .flatten()
            .filter(|tc| !tc.name.is_empty())
            .collect::<Vec<_>>();
        let mut resp = AiResponse::new(self.id, self.model);
        resp.content = self.content;
        resp.reasoning_content = if self.reasoning_content.is_empty() {
            None
        } else {
            Some(self.reasoning_content)
        };
        resp.reasoning_signature = if self.reasoning_signature.is_empty() {
            None
        } else {
            Some(self.reasoning_signature)
        };
        resp.tool_calls = tool_calls;
        if !self.unknown_items.is_empty() {
            // Consumers that see `items` treat it as the complete output graph
            // (Responses/Gemini formatters, cached replay), so it must carry the
            // tool calls too — otherwise they silently vanish.
            let mut items = Vec::new();
            if !resp.content.is_empty() {
                items.push(ResponseItem::OutputText {
                    text: resp.content.clone(),
                });
            }
            items.extend(resp.tool_calls.iter().map(|call| match call.kind {
                crate::protocol::ir::ToolCallKind::Function => ResponseItem::FunctionCall {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    namespace: call.namespace.clone(),
                    arguments: call.arguments.clone(),
                },
                crate::protocol::ir::ToolCallKind::Custom => ResponseItem::CustomToolCall {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    namespace: call.namespace.clone(),
                    input: call.arguments.clone(),
                },
            }));
            items.extend(self.unknown_items);
            resp.items = Some(items);
        }
        if !self.google_response_metadata.is_empty() {
            resp.vendor.ingress.insert(
                "__google_response_metadata".to_string(),
                serde_json::Value::Object(self.google_response_metadata),
            );
        }
        resp.stop_reason = self.stop_reason;
        resp.usage = self.usage;
        resp
    }
}

pub(super) fn ensure_tool_index(tool_calls: &mut Vec<Option<ToolCall>>, index: usize) {
    if tool_calls.len() <= index {
        tool_calls.resize(index + 1, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::codec::google::gemini::stream::GoogleStreamParser;
    use crate::protocol::codec::openai::compatible::stream::OpenAIResponseFormatter;
    use crate::protocol::ir::ToolCallKind;
    use crate::protocol::{ResponseEncoder, StreamResponseDecoder};
    use serde_json::{Value, json};

    /// Real Antigravity tool turn shape: signed parallel calls plus usage
    /// metadata. The metadata used to become an opaque item, which made every
    /// item-graph consumer (Responses, Gemini, cached replay) drop the calls.
    #[test]
    fn signed_parallel_calls_with_usage_metadata_reach_every_formatter() {
        use crate::protocol::codec::google::gemini::stream::GoogleResponseFormatter;
        use crate::protocol::codec::openai::responses::formatter::ResponsesResponseFormatter;
        let raw = format!(
            "data: {}\r\n\r\n",
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": "read_file", "args": {"path": "main.py"}}, "thoughtSignature": "c2ln"},
                {"functionCall": {"name": "read_file", "args": {"path": "utils.py"}}}]},
                "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
                "modelVersion": "gemini-4-hydrogen"})
        );
        let mut parser = GoogleStreamParser::new();
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&parser.parse_chunk(&raw).unwrap());
        accumulator.apply_all(&parser.finish().unwrap());
        let response = accumulator.into_ai_response();
        assert_eq!(response.tool_calls.len(), 2);
        assert!(response.items.is_none(), "{:?}", response.items);
        assert!(
            response
                .vendor
                .ingress
                .contains_key("__google_response_metadata")
        );

        let responses = ResponsesResponseFormatter.format_response(&response);
        let calls: Vec<&str> = responses["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["arguments"].as_str().unwrap())
            .collect();
        assert_eq!(
            calls,
            vec![r#"{"path":"main.py"}"#, r#"{"path":"utils.py"}"#]
        );

        let gemini = GoogleResponseFormatter.format_response(&response);
        let parts = gemini["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2, "{gemini}");
        assert!(parts.iter().all(|part| part.get("functionCall").is_some()));
        assert_eq!(gemini["usageMetadata"]["promptTokenCount"], 10);
    }

    #[test]
    fn opaque_parts_keep_tool_calls_in_the_item_graph() {
        use crate::protocol::codec::openai::responses::formatter::ResponsesResponseFormatter;
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::ThinkingDelta("plan".into()),
            AiStreamDelta::TextDelta("Reading.".into()),
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_a".into(),
                name: "read_file".into(),
                namespace: None,
                kind: ToolCallKind::Function,
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{}".into(),
            },
            AiStreamDelta::Unknown {
                raw: json!({"executableCode": {"code": "print(1)"}}).to_string(),
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let response = accumulator.into_ai_response();
        let items = response.items.as_ref().unwrap();
        assert!(matches!(items[0], ResponseItem::OutputText { .. }));
        assert!(
            matches!(&items[1], ResponseItem::FunctionCall { call_id, .. } if call_id == "call_a")
        );
        assert!(matches!(items[2], ResponseItem::Unknown { .. }));
        let out = ResponsesResponseFormatter.format_response(&response);
        let kinds: Vec<&str> = out["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect();
        // Reasoning from the convenience field is not lost; the call appears once.
        assert_eq!(kinds, vec!["reasoning", "function_call", "message"]);
    }

    // Exercise the actual private accumulator used by the forced-upstream-SSE
    // nonstream path. Do not expose it or include its source in integration tests.
    #[test]
    fn gemini_parallel_calls_survive_forced_stream_aggregation() {
        let cities = ["Beijing", "Shanghai", "Guangzhou"];
        let parts: Vec<_> = cities
            .iter()
            .map(|city| json!({"functionCall": {"name": "get_weather", "args": {"city": city}}}))
            .collect();
        let raw = format!(
            "data: {}\n\n",
            json!({"candidates": [{
                "content": {"role": "model", "parts": parts},
                "finishReason": "STOP"
            }]})
        );
        let mut parser = GoogleStreamParser::new();
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&parser.parse_chunk(&raw).expect("parse Gemini SSE"));
        accumulator.apply_all(&parser.finish().expect("finish Gemini SSE"));
        let response = accumulator.into_ai_response();

        assert_eq!(response.tool_calls.len(), 3, "{:?}", response.tool_calls);
        let args: Vec<Value> = response
            .tool_calls
            .iter()
            .map(|call| serde_json::from_str(&call.arguments).unwrap())
            .collect();
        assert_eq!(args, cities.map(|city| json!({"city": city})));

        // The Chat nonstream client must receive all three, not just the final
        // entry from slot zero. This is not a streaming-log accumulator test.
        let body = OpenAIResponseFormatter.format_response(&response);
        let calls = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("client tool calls");
        assert_eq!(calls.len(), 3);
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        for (call, city) in calls.iter().zip(cities) {
            assert_eq!(call["function"]["name"], "get_weather");
            let args: Value =
                serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(args, json!({"city": city}));
        }
    }

    #[test]
    fn distinct_sparse_indices_and_argument_fragments_remain_independent() {
        let start = |index, id: &str, name: &str| AiStreamDelta::ToolCallStart {
            index,
            id: id.into(),
            name: name.into(),
            namespace: None,
            kind: ToolCallKind::Function,
        };
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            start(2, "call_a", "read"),
            start(5, "call_b", "search"),
            AiStreamDelta::ToolCallDelta {
                index: 2,
                arguments: r#"{"path":"#.into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 5,
                arguments: r#"{"query":"rust"}"#.into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 2,
                arguments: r#""a.rs"}"#.into(),
            },
            AiStreamDelta::Done {
                stop_reason: "tool_calls".into(),
            },
        ]);
        let response = accumulator.into_ai_response();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "call_a");
        assert_eq!(response.tool_calls[1].id, "call_b");
        assert_eq!(response.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(response.tool_calls[1].arguments, r#"{"query":"rust"}"#);
    }
}
