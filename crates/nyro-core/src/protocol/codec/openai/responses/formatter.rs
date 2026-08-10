use serde_json::Value;
use uuid::Uuid;

use crate::protocol::ResponseEncoder;
use crate::protocol::ir::AiResponse;
use crate::protocol::ir::request::ToolCallKind;
use crate::protocol::ir::response::ResponseItem;

pub struct ResponsesResponseFormatter;

impl ResponseEncoder for ResponsesResponseFormatter {
    fn format_response(&self, resp: &AiResponse) -> Value {
        let resp_id = if resp.id.is_empty() {
            format!("resp_{}", Uuid::new_v4().simple())
        } else {
            resp.id.clone()
        };
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());

        let mut output: Vec<Value> = Vec::new();
        let mut output_text = String::new();

        if let Some(items) = &resp.items {
            for item in items {
                match item {
                    ResponseItem::Thinking { text } => {
                        output.push(serde_json::json!({
                            "type": "reasoning",
                            "id": format!("rs_{}", Uuid::new_v4().simple()),
                            "summary": [{
                                "type": "summary_text",
                                "text": text
                            }]
                        }));
                    }
                    ResponseItem::FunctionCall {
                        call_id,
                        name,
                        namespace,
                        arguments,
                    } => {
                        let mut item = serde_json::json!({
                            "type": "function_call",
                            "id": format!("fc_{}", Uuid::new_v4().simple()),
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                            "status": "completed"
                        });
                        insert_optional_namespace(&mut item, namespace.as_deref());
                        output.push(item);
                    }
                    ResponseItem::CustomToolCall {
                        call_id,
                        name,
                        namespace,
                        input,
                    } => {
                        let mut item = serde_json::json!({
                            "type": "custom_tool_call",
                            "id": format!("ctc_{}", Uuid::new_v4().simple()),
                            "call_id": call_id,
                            "name": name,
                            "input": input,
                            "status": "completed"
                        });
                        insert_optional_namespace(&mut item, namespace.as_deref());
                        output.push(item);
                    }
                    ResponseItem::OutputText { text } => {
                        output_text.push_str(text);
                    }
                    _ => {}
                }
            }
        } else {
            if let Some(reasoning) = &resp.reasoning_content {
                output.push(serde_json::json!({
                    "type": "reasoning",
                    "id": format!("rs_{}", Uuid::new_v4().simple()),
                    "summary": [{
                        "type": "summary_text",
                        "text": reasoning
                    }]
                }));
            }
            for tc in &resp.tool_calls {
                let mut item = match tc.kind {
                    ToolCallKind::Function => serde_json::json!({
                        "type": "function_call",
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                        "status": "completed"
                    }),
                    ToolCallKind::Custom => serde_json::json!({
                        "type": "custom_tool_call",
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "call_id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                        "status": "completed"
                    }),
                };
                insert_optional_namespace(&mut item, tc.namespace.as_deref());
                output.push(item);
            }
            output_text.push_str(&resp.content);
        }

        if !output_text.is_empty() {
            output.push(serde_json::json!({
                "type": "message",
                "id": msg_id,
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": output_text,
                    "annotations": []
                }]
            }));
        }

        serde_json::json!({
            "id": resp_id,
            "object": "response",
            "status": "completed",
            "model": resp.model,
            "output": output,
            "output_text": output_text,
            "usage": {
                "input_tokens": resp.usage.prompt_tokens,
                "output_tokens": resp.usage.completion_tokens,
                "total_tokens": resp.usage.prompt_tokens + resp.usage.completion_tokens
            }
        })
    }
}

fn insert_optional_namespace(value: &mut Value, namespace: Option<&str>) {
    if let Some(namespace) = namespace {
        value
            .as_object_mut()
            .expect("tool call object")
            .insert("namespace".into(), Value::String(namespace.to_string()));
    }
}
