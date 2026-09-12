//! Regressions for the raw native Google request codec and the public tool-result
//! normalizer. These are local IR/wire assertions, not live upstream, provider,
//! subscription-adapter, or gateway-route tests.
//!
//! Tool IDs identify calls; Gemini `functionResponse.name` must instead name the
//! corresponding function. Schema cleanup must retain the meaning of the simple,
//! recoverable fixtures below, not merely delete unsupported keywords. No other
//! converter (including gcli) is used as a correctness oracle.

mod conv_common;

use conv_common::*;
use nyro_core::protocol::codec::tool_correlation::normalize_request_tool_results;

/// Collect only the requested part kind, without fixing message grouping.
fn google_parts<'a>(body: &'a Value, kind: &str) -> Vec<&'a Value> {
    field(body, "/contents")
        .as_array()
        .expect("Google contents array")
        .iter()
        .flat_map(|message| {
            field(message, "/parts")
                .as_array()
                .expect("Google parts array")
        })
        .filter_map(|part| part.get(kind))
        .collect()
}

/// Two differently named calls return in reverse order. Checking both the name
/// and the associated payload prevents an ID-as-name or FIFO-only pseudo-fix.
fn assert_reversed_results_match_calls(body: &Value, expected_payloads: [Value; 2]) {
    let calls = google_parts(body, "functionCall");
    let results = google_parts(body, "functionResponse");
    assert_eq!(calls.len(), 2, "both calls must survive: {body}");
    assert_eq!(results.len(), 2, "both results must survive: {body}");
    field_str_eq(calls[0], "/name", "lookup_weather");
    field_str_eq(calls[1], "/name", "lookup_clock");

    for (result_index, call_index) in [1, 0].into_iter().enumerate() {
        assert_eq!(
            field(results[result_index], "/response"),
            &expected_payloads[result_index],
            "result payload/order must survive: {body}"
        );
        assert_eq!(
            field_str(results[result_index], "/name"),
            field_str(calls[call_index], "/name"),
            "functionResponse.name must match its identified functionCall.name, not the call ID or FIFO position: {body}"
        );
    }
}

#[test]
fn normalized_openai_text_results_use_function_names_in_reverse_call_order() {
    let mut req = decode_request(
        P::OpenAiChat,
        json!({
            "model": "gemini-2.5-flash",
            "messages": [
                {"role": "user", "content": "Get weather and local time"},
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {"id": "call_7", "type": "function", "function": {
                            "name": "lookup_weather", "arguments": "{\"city\":\"Paris\"}"
                        }},
                        {"id": "call_9", "type": "function", "function": {
                            "name": "lookup_clock", "arguments": "{\"city\":\"Paris\"}"
                        }}
                    ]
                },
                {"role": "tool", "tool_call_id": "call_9", "content": "12:00"},
                {"role": "tool", "tool_call_id": "call_7", "content": "22 C"}
            ]
        }),
    );
    normalize_request_tool_results(&mut req);

    assert_eq!(req.messages.len(), 4, "no synthetic calls are needed");
    for (message, id) in req.messages[2..].iter().zip(["call_9", "call_7"]) {
        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.tool_call_id.as_deref(), Some(id));
        assert!(
            matches!(&message.content, MessageContent::Text(_)),
            "fixture must reach the text-result encoder branch"
        );
    }

    let body = encode_request(P::GoogleGemini, &req);
    assert_reversed_results_match_calls(
        &body,
        [json!({"result": "12:00"}), json!({"result": "22 C"})],
    );
}

#[test]
fn normalized_anthropic_tool_result_blocks_use_function_names_in_reverse_call_order() {
    // Decode real Anthropic tool_use blocks, then attach block-form results via
    // the shared IR helper. Decoding Anthropic user tool_result blocks directly
    // would split them into Text messages and miss Google's ToolResult arm.
    let mut req = decode_request(
        P::AnthropicMessages,
        json!({
            "model": "gemini-2.5-flash",
            "max_tokens": 128,
            "messages": [
                {"role": "user", "content": "Get weather and local time"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_7", "name": "lookup_weather",
                     "input": {"city": "Paris"}},
                    {"type": "tool_use", "id": "toolu_9", "name": "lookup_clock",
                     "input": {"city": "Paris"}}
                ]}
            ]
        }),
    );
    req.messages.extend([
        tool_result_msg("toolu_9", json!({"local_time": "12:00"})),
        tool_result_msg("toolu_7", json!({"temperature": 22})),
    ]);
    assert!(req.messages[2..].iter().all(|m| m.tool_call_id.is_none()));
    normalize_request_tool_results(&mut req);

    assert_eq!(req.messages.len(), 4, "block hints identify existing calls");
    for (message, id) in req.messages[2..].iter().zip(["toolu_9", "toolu_7"]) {
        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.tool_call_id.as_deref(), Some(id));
        let MessageContent::Blocks(blocks) = &message.content else {
            panic!("fixture must reach the ToolResult block encoder, not decoder-split text");
        };
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
            "normalization must retain the identified ToolResult block"
        );
    }

    let body = encode_request(P::GoogleGemini, &req);
    assert_reversed_results_match_calls(
        &body,
        [json!({"local_time": "12:00"}), json!({"temperature": 22})],
    );
}

fn request_with_parameters(parameters: Value) -> AiRequest {
    let mut req = request("gemini-2.5-flash", vec![user_msg("Use the tool")]);
    req.tools = Some(vec![ToolSpec {
        name: "example_tool".to_string(),
        description: Some("Schema regression fixture".to_string()),
        kind: Default::default(),
        namespace: None,
        parameters,
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    req
}

/// Deliberately compare two explicit fixture representations rather than build a
/// JSON Schema validator. The native Schema channel must contain the lowered
/// shape; the richer JSON Schema channel may preserve the self-contained input
/// or use the equivalent lowered shape. Only one channel may be emitted.
fn assert_google_schema_encoding(
    body: &Value,
    expected_parameters: Value,
    expected_json_schema: Value,
) {
    let declaration = field(body, "/tools/0/functionDeclarations/0");
    field_str_eq(declaration, "/name", "example_tool");
    match (
        declaration.get("parameters"),
        declaration.get("parametersJsonSchema"),
    ) {
        (Some(actual), None) => assert_eq!(
            actual, &expected_parameters,
            "lowering must preserve the fixture's constraints"
        ),
        (None, Some(actual)) => assert!(
            actual == &expected_json_schema || actual == &expected_parameters,
            "parametersJsonSchema must retain an equivalent, self-contained schema; got {actual}, expected {expected_json_schema} or {expected_parameters}"
        ),
        _ => panic!("expected exactly one parameters schema channel: {declaration}"),
    }
}

#[test]
fn local_ref_definition_retains_nested_properties_and_required_fields() {
    let original = json!({
        "type": "object",
        "properties": {"destination": {"$ref": "#/$defs/address"}},
        "required": ["destination"],
        "$defs": {"address": {
            "type": "object",
            "properties": {"city": {"type": "string", "enum": ["Paris", "Rome"]}},
            "required": ["city"]
        }}
    });
    let lowered = json!({
        "type": "object",
        "properties": {"destination": {
            "type": "object",
            "properties": {"city": {"type": "string", "enum": ["Paris", "Rome"]}},
            "required": ["city"]
        }},
        "required": ["destination"]
    });

    let body = encode_request(P::GoogleGemini, &request_with_parameters(original.clone()));
    assert_google_schema_encoding(&body, lowered, original);
}

#[test]
fn repeated_sibling_refs_both_retain_the_shared_definition() {
    // Reusing a definition is not recursion. A global "seen refs" set must not
    // erase the second sibling when local-reference resolution is introduced.
    let original = json!({
        "type": "object",
        "properties": {
            "origin": {"$ref": "#/$defs/city"},
            "destination": {"$ref": "#/$defs/city"}
        },
        "required": ["origin", "destination"],
        "$defs": {"city": {"type": "string", "enum": ["Paris", "Rome"]}}
    });
    let lowered = json!({
        "type": "object",
        "properties": {
            "origin": {"type": "string", "enum": ["Paris", "Rome"]},
            "destination": {"type": "string", "enum": ["Paris", "Rome"]}
        },
        "required": ["origin", "destination"]
    });

    let body = encode_request(P::GoogleGemini, &request_with_parameters(original.clone()));
    assert_google_schema_encoding(&body, lowered, original);
}

#[test]
fn all_of_disjoint_object_branches_retain_properties_and_required_union() {
    // Disjoint properties, compatible object types, and no closed-object or
    // conflicting constraints: flattening this intersection is unambiguous.
    let original = json!({
        "type": "object",
        "allOf": [
            {"type": "object", "properties": {"city": {"type": "string"}},
             "required": ["city"]},
            {"type": "object", "properties": {
                "limit": {"type": "integer"}, "label": {"type": "string"}
             }, "required": ["limit"]}
        ]
    });
    let lowered = json!({
        "type": "object",
        "properties": {
            "city": {"type": "string"},
            "limit": {"type": "integer"},
            "label": {"type": "string"}
        },
        "required": ["city", "limit"]
    });

    let body = encode_request(P::GoogleGemini, &request_with_parameters(original.clone()));
    assert_google_schema_encoding(&body, lowered, original);
}

#[test]
fn one_of_distinct_string_constants_retains_finite_enum_choices() {
    // Distinct constants are mutually exclusive, so this oneOf is exactly an
    // enum, not an arbitrary union whose overlapping alternatives need analysis.
    let original = json!({
        "type": "object",
        "properties": {"mode": {
            "type": "string",
            "oneOf": [{"const": "fast"}, {"const": "safe"}]
        }},
        "required": ["mode"]
    });
    let lowered = json!({
        "type": "object",
        "properties": {"mode": {"type": "string", "enum": ["fast", "safe"]}},
        "required": ["mode"]
    });

    let body = encode_request(P::GoogleGemini, &request_with_parameters(original.clone()));
    assert_google_schema_encoding(&body, lowered, original);
}

#[test]
fn nullable_properties_keep_explicit_required_membership_independent() {
    // Required means present, nullable means null is a valid value. A required
    // nullable property must not become optional, nor the optional one required.
    let parameters = json!({
        "type": "object",
        "properties": {
            "required_note": {"type": "string", "nullable": true},
            "optional_note": {"type": "string", "nullable": true}
        },
        "required": ["required_note"]
    });
    let json_schema = json!({
        "type": "object",
        "properties": {
            "required_note": {"type": ["string", "null"]},
            "optional_note": {"type": ["string", "null"]}
        },
        "required": ["required_note"]
    });

    let body = encode_request(
        P::GoogleGemini,
        &request_with_parameters(parameters.clone()),
    );
    // Unlike ordinary JSON Schema fixtures, nullable:true requires conversion
    // if the richer JSON Schema channel is chosen; it is not a JSON Schema type.
    let declaration = field(&body, "/tools/0/functionDeclarations/0");
    if let Some(actual) = declaration.get("parametersJsonSchema") {
        assert!(declaration.get("parameters").is_none());
        assert_eq!(actual, &json_schema);
    } else {
        assert_eq!(field(declaration, "/parameters"), &parameters);
    }
}

#[test]
fn simple_valid_schema_preserves_types_constraints_and_optional_properties() {
    let parameters = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search terms"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 10},
            "tags": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["query"]
    });

    let body = encode_request(
        P::GoogleGemini,
        &request_with_parameters(parameters.clone()),
    );
    assert_google_schema_encoding(&body, parameters.clone(), parameters);
}

#[test]
fn explicit_google_call_and_result_ids_round_trip_with_real_function_name() {
    let body = json!({"contents":[
        {"role":"model","parts":[{"functionCall":{"id":"upstream-1","name":"lookup","args":{"city":"Paris"}}}]},
        {"role":"user","parts":[{"functionResponse":{"id":"upstream-1","name":"lookup","response":{"ok":true}}}]}
    ]});
    let out = round_trip_request(P::GoogleGemini, body);
    let calls = google_parts(&out, "functionCall");
    let results = google_parts(&out, "functionResponse");
    assert_eq!(calls[0]["id"], "upstream-1");
    assert_eq!(results[0]["id"], "upstream-1");
    assert_eq!(results[0]["name"], "lookup");
}

#[test]
fn legacy_google_same_name_results_keep_separate_call_ids_in_order() {
    let body = json!({"contents":[
        {"role":"model","parts":[
            {"functionCall":{"name":"lookup","args":{"city":"Paris"}}},
            {"functionCall":{"name":"lookup","args":{"city":"Rome"}}}
        ]},
        {"role":"user","parts":[
            {"functionResponse":{"name":"lookup","response":{"city":"Paris"}}},
            {"functionResponse":{"name":"lookup","response":{"city":"Rome"}}}
        ]}
    ]});
    let out = round_trip_request(P::GoogleGemini, body);
    let calls = google_parts(&out, "functionCall");
    let results = google_parts(&out, "functionResponse");
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    for (call, result) in calls.iter().zip(results) {
        assert_eq!(call["id"], result["id"]);
        assert_eq!(call["name"], result["name"]);
        assert_eq!(call["args"]["city"], result["response"]["city"]);
    }
}

#[test]
fn unresolvable_result_is_a_typed_error_even_after_generic_orphan_repair() {
    use nyro_core::error::GatewayError;
    use nyro_core::protocol::RequestEncoder;
    use nyro_core::protocol::codec::google::gemini::encoder::GoogleEncoder;
    let mut req = request(
        "gemini-review",
        vec![tool_result_msg("missing-id", json!({"ok":true}))],
    );
    let error = GoogleEncoder.encode_request(&req).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<GatewayError>(),
        Some(GatewayError::BadRequest { .. })
    ));
    normalize_request_tool_results(&mut req);
    let error = GoogleEncoder.encode_request(&req).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<GatewayError>(),
        Some(GatewayError::BadRequest { .. })
    ));
}

#[test]
fn synthetic_tool_provenance_never_leaks_to_other_protocol_wires() {
    let mut req = request(
        "model",
        vec![tool_result_msg("missing-id", json!({"ok":true}))],
    );
    normalize_request_tool_results(&mut req);
    assert!(req.messages[0].meta.as_ref().unwrap()["__nyro_synthetic_tool_call"] == true);
    for target in [P::OpenAiChat, P::OpenAiResponses, P::AnthropicMessages] {
        let out = encode_request(target, &req);
        assert!(
            !out.to_string().contains("__nyro_synthetic_tool_call"),
            "{target}: {out}"
        );
    }
}

#[test]
fn ambiguous_existing_id_cannot_silently_change_its_function_name() {
    use nyro_core::protocol::RequestEncoder;
    use nyro_core::protocol::codec::google::gemini::encoder::GoogleEncoder;
    let req = request(
        "gemini-review",
        vec![
            assistant_tool_call_msg("same-id", "weather", "{}"),
            assistant_tool_call_msg("same-id", "clock", "{}"),
            tool_result_msg("same-id", json!({"ok":true})),
        ],
    );
    let error = GoogleEncoder.encode_request(&req).unwrap_err();
    assert!(error.to_string().contains("conflicting function names"));
}

#[test]
fn rich_google_schema_channel_and_default_literals_are_not_cleaned_as_schema_keywords() {
    let parameters = json!({"type":"object","properties":{"target":{"$ref":"#/$defs/target"}},"$defs":{"target":{"type":"string"}}});
    let body = json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{"functionDeclarations":[{"name":"lookup","parametersJsonSchema":parameters}]}]});
    let out = round_trip_request(P::GoogleGemini, body);
    assert_eq!(
        out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        parameters
    );
    let literal = json!({"$ref":"not a schema ref", "format":"plain", "allOf":[1,2]});
    let out = encode_request(
        P::GoogleGemini,
        &request_with_parameters(json!({"type":"object","default":literal})),
    );
    assert_eq!(
        out["tools"][0]["functionDeclarations"][0]["parameters"]["default"],
        literal
    );
}

#[test]
fn unrepresentable_native_schema_is_typed_lossy_rejection_not_empty_success() {
    use nyro_core::error::GatewayError;
    use nyro_core::protocol::RequestEncoder;
    use nyro_core::protocol::codec::google::gemini::encoder::GoogleEncoder;
    for schema in [
        json!({"$ref":"#"}),
        json!({"$ref":"https://invalid.test/schema.json"}),
        json!({"oneOf":[{"type":"string"},{"type":"integer"}]}),
    ] {
        let error = GoogleEncoder
            .encode_request(&request_with_parameters(schema))
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<GatewayError>(),
            Some(GatewayError::ProtocolLossyRejected { .. })
        ));
    }
}

#[test]
fn openai_strict_local_ref_schema_is_not_subject_to_google_lowering() {
    // Scope control: Google's schema repair must not strip standard OpenAI
    // strict JSON Schema. This is the raw codec, not provider passthrough routing.
    let parameters = json!({
        "type": "object",
        "properties": {"city": {"$ref": "#/$defs/city"}},
        "required": ["city"],
        "additionalProperties": false,
        "$defs": {"city": {"type": "string"}}
    });
    let mut req = request_with_parameters(parameters.clone());
    req.tools.as_mut().expect("tool fixture")[0].strict = Some(true);

    let body = encode_request(P::OpenAiChat, &req);
    let function = field(&body, "/tools/0/function");
    assert_eq!(field(function, "/strict"), &json!(true));
    assert_eq!(field(function, "/parameters"), &parameters);
}
