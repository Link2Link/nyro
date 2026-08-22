use bytes::Bytes;
use serde_json::{Value, json};

use crate::protocol::ir::AiRequest;

pub(crate) fn request_patch(
    baseline: &AiRequest,
    current: &AiRequest,
) -> Result<Option<Bytes>, String> {
    let source_protocol = baseline
        .meta
        .source_protocol
        .ok_or_else(|| "request is missing its source protocol".to_string())?;
    let encoder = source_protocol.handler().make_request_encoder();
    // Replay only intentional IR mutations onto an exact raw-wire request. If
    // either snapshot cannot re-encode, let the selected conversion strategy
    // surface the real client-facing error instead of manufacturing a patch.
    let (Ok(baseline), Ok(current)) = (
        encoder.encode_request(baseline).map(|encoded| encoded.0),
        encoder.encode_request(current).map(|encoded| encoded.0),
    ) else {
        return Ok(None);
    };
    value_patch(&baseline, &current)
}

pub(crate) fn value_patch(baseline: &Value, current: &Value) -> Result<Option<Bytes>, String> {
    let mut operations = Vec::new();
    diff_values(baseline, current, &mut Vec::new(), &mut operations);
    if operations.is_empty() {
        return Ok(None);
    }
    serde_json::to_vec(&operations)
        .map(Bytes::from)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn diff_values(
    baseline: &Value,
    current: &Value,
    path: &mut Vec<String>,
    operations: &mut Vec<Value>,
) {
    if baseline == current {
        return;
    }
    match (baseline, current) {
        (Value::Object(before), Value::Object(after)) => {
            for (key, before_value) in before {
                path.push(key.clone());
                if let Some(after_value) = after.get(key) {
                    diff_values(before_value, after_value, path, operations);
                } else {
                    operations.push(json!({"op": "remove", "path": path}));
                }
                path.pop();
            }
            for (key, after_value) in after {
                if before.contains_key(key) {
                    continue;
                }
                path.push(key.clone());
                operations.push(json!({"op": "set", "path": path, "value": after_value}));
                path.pop();
            }
        }
        _ => operations.push(json!({"op": "set", "path": path, "value": current})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::OPENAI_RESPONSES_V1;
    use crate::protocol::ir::{AiRequest, Message, MessageContent, Role};

    #[test]
    fn value_patch_records_nested_set_remove_and_array_replace() {
        let before = json!({
            "keep": 1,
            "nested": {"remove": true, "change": "before"},
            "array": [1, 2]
        });
        let after = json!({
            "keep": 1,
            "nested": {"change": "after", "add": 2},
            "array": [3]
        });
        let patch = value_patch(&before, &after).unwrap().unwrap();
        let operations: Value = serde_json::from_slice(&patch).unwrap();
        assert!(operations.as_array().is_some_and(|items| items.len() == 4));
        assert!(
            operations.as_array().unwrap().iter().any(|item| {
                item["op"] == "remove" && item["path"] == json!(["nested", "remove"])
            })
        );
        assert!(operations.as_array().unwrap().iter().any(|item| {
            item["op"] == "set"
                && item["path"] == json!(["nested", "change"])
                && item["value"] == "after"
        }));
    }

    #[test]
    fn request_patch_uses_ingress_encoder_and_only_changed_fields() {
        let message = Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let mut baseline = AiRequest::new("virtual", vec![message]);
        baseline.meta.source_protocol = Some(OPENAI_RESPONSES_V1);
        let mut current = baseline.clone();
        current.generation.temperature = Some(0.25);

        let patch = request_patch(&baseline, &current).unwrap().unwrap();
        let operations: Value = serde_json::from_slice(&patch).unwrap();
        assert_eq!(operations.as_array().unwrap().len(), 1);
        assert_eq!(operations[0]["op"], "set");
        assert_eq!(operations[0]["path"], json!(["temperature"]));
        assert_eq!(operations[0]["value"], 0.25);
    }
}
