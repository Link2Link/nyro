use serde_json::Value;

pub mod decoder;
pub mod encoder;
pub mod formatter;
pub mod parser;
#[allow(clippy::module_inception)]
pub mod responses;
pub mod stream;

/// Materialize OpenAI Responses defaults for function tools.
///
/// Native Responses requests can bypass the codec, so this operates on the
/// final wire body and covers both top-level tools and dynamically supplied
/// `additional_tools`. Explicit values and non-function tools are preserved.
pub(crate) fn normalize_function_tool_defaults(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools") {
        normalize_tool_list(tools);
    }

    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools")
                && let Some(tools) = item.get_mut("tools")
            {
                normalize_tool_list(tools);
            }
        }
    }
}

fn normalize_tool_list(value: &mut Value) {
    let Some(tools) = value.as_array_mut() else {
        return;
    };

    for tool in tools {
        let tool_type = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function")
            .to_string();
        match tool_type.as_str() {
            "function" => {
                if let Some(fields) = tool.as_object_mut() {
                    fields.entry("strict").or_insert_with(|| Value::Bool(false));
                    let parameters = fields
                        .entry("parameters")
                        .or_insert_with(|| Value::Object(Default::default()));
                    if let Some(schema) = parameters.as_object_mut() {
                        schema
                            .entry("type")
                            .or_insert_with(|| Value::String("object".to_string()));
                    }
                }
            }
            "namespace" => {
                if let Some(children) = tool.get_mut("tools") {
                    normalize_tool_list(children);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn normalizes_function_tool_defaults_without_overwriting_explicit_values() {
        let automation_parameters = json!({
            "$defs": {
                "view": {
                    "type": "object",
                    "properties": {
                        "mode": {"const": "view"},
                        "id": {"type": "string"}
                    },
                    "required": ["mode", "id"]
                }
            },
            "oneOf": [
                {"$ref": "#/$defs/view"},
                {
                    "type": "object",
                    "properties": {"mode": {"const": "delete"}},
                    "required": ["mode"]
                }
            ]
        });
        let mut body = json!({
            "tools": [
                {"type": "function", "name": "defaulted", "parameters": {"required": ["value"]}},
                {"type": "function", "name": "disabled", "strict": false},
                {"type": "function", "name": "enabled", "strict": true},
                {"type": "function", "name": "explicit_array", "parameters": {"type": "array"}},
                {"type": "web_search_preview"},
                {
                    "type": "namespace",
                    "name": "codex_app",
                    "tools": [{
                        "type": "function",
                        "name": "automation_update",
                        "parameters": automation_parameters.clone()
                    }]
                }
            ],
            "input": [{
                "type": "additional_tools",
                "tools": [
                    {"type": "function", "name": "dynamic"},
                    {"type": "custom", "name": "raw"}
                ]
            }]
        });

        normalize_function_tool_defaults(&mut body);

        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert_eq!(body["tools"][0]["parameters"]["required"], json!(["value"]));
        assert_eq!(body["tools"][1]["strict"], false);
        assert_eq!(body["tools"][1]["parameters"]["type"], "object");
        assert_eq!(body["tools"][2]["strict"], true);
        assert_eq!(body["tools"][3]["parameters"]["type"], "array");
        assert!(body["tools"][4].get("strict").is_none());
        assert_eq!(body["tools"][5]["tools"][0]["strict"], false);
        let mut expected_automation_parameters = automation_parameters;
        expected_automation_parameters["type"] = json!("object");
        assert_eq!(
            body["tools"][5]["tools"][0]["parameters"],
            expected_automation_parameters
        );
        assert_eq!(body["input"][0]["tools"][0]["strict"], false);
        assert_eq!(body["input"][0]["tools"][0]["parameters"]["type"], "object");
        assert!(body["input"][0]["tools"][1].get("strict").is_none());
        assert!(body["input"][0]["tools"][1].get("parameters").is_none());
    }
}
