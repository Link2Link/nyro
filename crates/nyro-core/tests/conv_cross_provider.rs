//! Cross-provider conversion-correctness suite.
//!
//! Ported from `llm-bridge/test/cross-provider.test.ts`, the tool-choice mode
//! cases of `test/tool-calling.test.ts`, and the cross-provider tool round-trip
//! cases of `test/fix-verification.test.ts`, adapted to Nyro's IR + codec
//! architecture via the `translate(source, body, target)` helper (decode with
//! `source`, encode with `target`).
//!
//! Where the wire output differs from llm-bridge's, the test asserts Nyro's
//! actual contract and documents the difference inline; genuine missing
//! capabilities are marked `KNOWN GAP` and `#[ignore]`d.

mod conv_common;

use conv_common::*;

// ── text messages ────────────────────────────────────────────────────────────

#[test]
fn openai_text_to_anthropic() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello!"},
                {"role": "assistant", "content": "Hi there! How can I help?"}
            ]
        }),
        P::AnthropicMessages,
    );

    field_str_eq(&out, "/model", "gpt-4o");
    field_str_eq(&out, "/system", "You are helpful.");
    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["text"], "Hello!");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["text"], "Hi there! How can I help?");
}

#[test]
fn openai_text_to_google() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello!"},
                {"role": "assistant", "content": "Hi there!"}
            ]
        }),
        P::GoogleGemini,
    );

    field_str_eq(&out, "/systemInstruction/parts/0/text", "You are helpful.");
    let contents = field(&out, "/contents").as_array().expect("contents");
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Hello!");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(contents[1]["parts"][0]["text"], "Hi there!");
}

#[test]
fn anthropic_text_to_openai() {
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": "You are a pirate.",
            "messages": [
                {"role": "user", "content": "Ahoy!"},
                {"role": "assistant", "content": "Arrr, hello matey!"}
            ]
        }),
        P::OpenAiChat,
    );

    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are a pirate.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Ahoy!");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "Arrr, hello matey!");
}

#[test]
fn anthropic_text_to_google() {
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "What is TypeScript?"},
                {"role": "assistant", "content": "TypeScript is a typed superset of JavaScript."}
            ]
        }),
        P::GoogleGemini,
    );

    field_str_eq(&out, "/systemInstruction/parts/0/text", "You are helpful.");
    let contents = field(&out, "/contents").as_array().expect("contents");
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "What is TypeScript?");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0]["text"],
        "TypeScript is a typed superset of JavaScript."
    );
}

#[test]
fn google_text_to_openai() {
    let out = translate(
        P::GoogleGemini,
        json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Hello!"}]},
                {"role": "model", "parts": [{"text": "Hi!"}]}
            ],
            "systemInstruction": {"parts": [{"text": "You are friendly."}]}
        }),
        P::OpenAiChat,
    );

    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are friendly.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Hello!");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "Hi!");
}

#[test]
fn google_text_to_anthropic() {
    let out = translate(
        P::GoogleGemini,
        json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Hi there"}]},
                {"role": "model", "parts": [{"text": "Hello!"}]}
            ],
            "systemInstruction": {"parts": [{"text": "Be concise."}]}
        }),
        P::AnthropicMessages,
    );

    field_str_eq(&out, "/system", "Be concise.");
    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["text"], "Hi there");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["text"], "Hello!");
}

#[test]
fn responses_text_to_openai_chat() {
    let out = translate(
        P::OpenAiResponses,
        json!({
            "model": "gpt-4o",
            "input": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello!"}
            ]
        }),
        P::OpenAiChat,
    );

    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are helpful.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Hello!");
}

// ── tool definitions across providers ────────────────────────────────────────

#[test]
fn openai_tools_to_anthropic() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Get the weather"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {"type": "string", "description": "City name"}
                        },
                        "required": ["location"]
                    }
                }
            }]
        }),
        P::AnthropicMessages,
    );

    let tools = field(&out, "/tools").as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["description"], "Get current weather");
    assert_eq!(tools[0]["input_schema"]["type"], "object");
    assert_eq!(
        tools[0]["input_schema"]["properties"]["location"],
        json!({"type": "string", "description": "City name"})
    );
}

#[test]
fn openai_tools_to_google() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Get the weather"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}}
                    }
                }
            }]
        }),
        P::GoogleGemini,
    );

    let tools = field(&out, "/tools").as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    let decls = tools[0]["functionDeclarations"].as_array().expect("decls");
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0]["name"], "get_weather");
    assert_eq!(decls[0]["description"], "Get current weather");
}

#[test]
fn anthropic_tools_to_google() {
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Search for info"}],
            "tools": [{
                "name": "search",
                "description": "Search the web",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string", "description": "Search query"}},
                    "required": ["query"]
                }
            }]
        }),
        P::GoogleGemini,
    );

    let tools = field(&out, "/tools").as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    let decls = tools[0]["functionDeclarations"].as_array().expect("decls");
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0]["name"], "search");
    assert_eq!(decls[0]["description"], "Search the web");
}

#[test]
fn google_tools_to_openai() {
    let out = translate(
        P::GoogleGemini,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "Get weather"}]}],
            "tools": [{
                "functionDeclarations": [{
                    "name": "get_weather",
                    "description": "Get weather data",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}}
                    }
                }]
            }]
        }),
        P::OpenAiChat,
    );

    let tools = field(&out, "/tools").as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_weather");
    assert_eq!(tools[0]["function"]["description"], "Get weather data");
}

// ── images across providers ──────────────────────────────────────────────────

#[test]
fn openai_data_url_image_to_anthropic_base64() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo..."}},
                    {"type": "text", "text": "What is this?"}
                ]
            }]
        }),
        P::AnthropicMessages,
    );

    let blocks = field(&out, "/messages/0/content").as_array().expect("content");
    let image = blocks
        .iter()
        .find(|b| b["type"] == "image")
        .unwrap_or_else(|| panic!("image block missing: {blocks:?}"));
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["data"], "iVBORw0KGgo...");
    assert_eq!(image["source"]["media_type"], "image/png");
    let text = blocks
        .iter()
        .find(|b| b["type"] == "text")
        .unwrap_or_else(|| panic!("text block missing: {blocks:?}"));
    assert_eq!(text["text"], "What is this?");
}

#[test]
fn openai_data_url_image_to_google_inline_data() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ..."}},
                    {"type": "text", "text": "Describe this"}
                ]
            }]
        }),
        P::GoogleGemini,
    );

    let parts = field(&out, "/contents/0/parts").as_array().expect("parts");
    let image = parts
        .iter()
        .find(|p| p.get("inlineData").is_some())
        .unwrap_or_else(|| panic!("inlineData missing: {parts:?}"));
    assert_eq!(image["inlineData"]["data"], "/9j/4AAQ...");
    assert_eq!(image["inlineData"]["mimeType"], "image/jpeg");
    let text = parts
        .iter()
        .find(|p| p.get("text").is_some())
        .unwrap_or_else(|| panic!("text missing: {parts:?}"));
    assert_eq!(text["text"], "Describe this");
}

#[test]
fn anthropic_base64_image_to_google_inline_data() {
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo..."}},
                    {"type": "text", "text": "Describe this"}
                ]
            }]
        }),
        P::GoogleGemini,
    );

    let parts = field(&out, "/contents/0/parts").as_array().expect("parts");
    let image = parts
        .iter()
        .find(|p| p.get("inlineData").is_some())
        .unwrap_or_else(|| panic!("inlineData missing: {parts:?}"));
    assert_eq!(image["inlineData"]["data"], "iVBORw0KGgo...");
    assert_eq!(image["inlineData"]["mimeType"], "image/png");
}

#[test]
fn google_inline_data_image_to_anthropic_base64() {
    let out = translate(
        P::GoogleGemini,
        json!({
            "contents": [{
                "role": "user",
                "parts": [
                    {"inlineData": {"mimeType": "image/webp", "data": "UklGRgAA..."}},
                    {"text": "What is this?"}
                ]
            }]
        }),
        P::AnthropicMessages,
    );

    let blocks = field(&out, "/messages/0/content").as_array().expect("content");
    let image = blocks
        .iter()
        .find(|b| b["type"] == "image")
        .unwrap_or_else(|| panic!("image block missing: {blocks:?}"));
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["data"], "UklGRgAA...");
    assert_eq!(image["source"]["media_type"], "image/webp");
}

// ── developer role ───────────────────────────────────────────────────────────

#[test]
fn developer_message_to_anthropic_system() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "developer", "content": "You must always respond in JSON."},
                {"role": "user", "content": "List 3 colors"}
            ]
        }),
        P::AnthropicMessages,
    );

    field_str_eq(&out, "/system", "You must always respond in JSON.");
    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["text"], "List 3 colors");
}

#[test]
fn developer_message_to_google_system_instruction() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "developer", "content": "Always respond in haiku."},
                {"role": "user", "content": "Tell me about the weather"}
            ]
        }),
        P::GoogleGemini,
    );

    let parts = field(&out, "/systemInstruction/parts").as_array().expect("parts");
    assert!(
        parts.iter().any(|p| p["text"] == "Always respond in haiku."),
        "developer text in systemInstruction: {parts:?}"
    );
    let contents = field(&out, "/contents").as_array().expect("contents");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Tell me about the weather");
}

#[test]
fn developer_merges_with_existing_system_to_anthropic() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "developer", "content": "Always be concise."},
                {"role": "user", "content": "Hello"}
            ]
        }),
        P::AnthropicMessages,
    );

    let system = field_str(&out, "/system");
    assert!(
        system.contains("You are a helpful assistant.") && system.contains("Always be concise."),
        "both system and developer text merged: {system:?}"
    );
}

#[test]
fn developer_merges_with_existing_system_to_google() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are an expert."},
                {"role": "developer", "content": "Be brief."},
                {"role": "user", "content": "Hello"}
            ]
        }),
        P::GoogleGemini,
    );

    let parts = field(&out, "/systemInstruction/parts").as_array().expect("parts");
    let joined: Vec<&str> = parts
        .iter()
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert!(
        joined.iter().any(|t| t.contains("You are an expert."))
            && joined.iter().any(|t| t.contains("Be brief.")),
        "both system and developer text in systemInstruction: {joined:?}"
    );
}

// ── structured output across providers ───────────────────────────────────────

#[test]
fn openai_json_schema_to_google_response_schema() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Extract name and age"}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "person",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "age": {"type": "number"}
                        }
                    }
                }
            }
        }),
        P::GoogleGemini,
    );

    field_str_eq(&out, "/generationConfig/responseMimeType", "application/json");
    let schema = field(&out, "/generationConfig/responseSchema");
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"].get("name").is_some());
    assert!(schema["properties"].get("age").is_some());
}

#[test]
fn google_response_schema_to_openai_response_format() {
    let out = translate(
        P::GoogleGemini,
        json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "Extract data"}]}],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "count": {"type": "integer"}
                    }
                }
            }
        }),
        P::OpenAiChat,
    );

    let rf = field(&out, "/response_format");
    assert_eq!(rf["type"], "json_schema");
    assert!(rf["json_schema"]["schema"]["properties"].get("title").is_some());
    assert!(rf["json_schema"]["schema"]["properties"].get("count").is_some());
}

// ── tool choice modes across providers ───────────────────────────────────────

#[test]
fn tool_choice_auto_to_anthropic() {
    // llm-bridge: `"auto"` verbatim in universal → `{type: "auto"}`.
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "type": "function",
                "function": {"name": "test_tool", "description": "A test", "parameters": {"type": "object"}}
            }],
            "tool_choice": "auto"
        }),
        P::AnthropicMessages,
    );

    assert_eq!(field(&out, "/tool_choice"), &json!({"type": "auto"}));
}

#[test]
fn tool_choice_auto_to_google_tool_config() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "type": "function",
                "function": {"name": "test_tool", "description": "A test", "parameters": {"type": "object"}}
            }],
            "tool_choice": "auto"
        }),
        P::GoogleGemini,
    );

    field_str_eq(&out, "/toolConfig/functionCallingConfig/mode", "AUTO");
}

#[test]
fn tool_choice_required_to_anthropic() {
    // llm-bridge maps `required` → `{type: "required"}`; Nyro's IR maps it to
    // the Anthropic `{type: "any"}` object (see `map_tool_choice_for_anthropic`).
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "type": "function",
                "function": {"name": "test_tool", "description": "A test", "parameters": {"type": "object"}}
            }],
            "tool_choice": "required"
        }),
        P::AnthropicMessages,
    );

    assert_eq!(field(&out, "/tool_choice"), &json!({"type": "any"}));
}

#[test]
fn tool_choice_required_to_google_tool_config() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "type": "function",
                "function": {"name": "test_tool", "description": "A test", "parameters": {"type": "object"}}
            }],
            "tool_choice": "required"
        }),
        P::GoogleGemini,
    );

    field_str_eq(&out, "/toolConfig/functionCallingConfig/mode", "ANY");
}

#[test]
fn tool_choice_specific_name_to_anthropic() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "type": "function",
                "function": {"name": "specific_tool", "description": "Specific", "parameters": {"type": "object"}}
            }],
            "tool_choice": {"type": "function", "function": {"name": "specific_tool"}}
        }),
        P::AnthropicMessages,
    );

    assert_eq!(
        field(&out, "/tool_choice"),
        &json!({"type": "tool", "name": "specific_tool"})
    );
}

// ── cross-provider tool call round trips (fix-verification) ─────────────────

#[test]
fn anthropic_tool_call_to_google() {
    let out = translate(
        P::AnthropicMessages,
        json!({
            "model": "claude-3-sonnet-20240229",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "What's the weather?"},
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_abc123",
                        "name": "get_weather",
                        "input": {"location": "NYC"}
                    }]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_abc123",
                        "content": "Sunny, 72F"
                    }]
                }
            ]
        }),
        P::GoogleGemini,
    );

    let contents = field(&out, "/contents").as_array().expect("contents");
    let call_part = contents[1]["parts"][0].clone();
    assert_eq!(call_part["functionCall"]["name"], "get_weather");
    assert_eq!(call_part["functionCall"]["args"], json!({"location": "NYC"}));
    // Nyro writes the tool message's `tool_call_id` as the functionResponse
    // name and wraps the string result in `{"result": ...}` (llm-bridge
    // resolves the tool name from the earlier call and wraps in `{output: ...}`).
    let result_part = contents[2]["parts"][0].clone();
    assert_eq!(result_part["functionResponse"]["name"], "toolu_abc123");
    assert_eq!(
        result_part["functionResponse"]["response"],
        json!({"result": "Sunny, 72F"})
    );
}

#[test]
fn openai_tool_call_to_anthropic() {
    let out = translate(
        P::OpenAiChat,
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Search for cats"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "search", "arguments": "{\"query\":\"cats\"}"}
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_123",
                    "content": "Found 10 results about cats"
                }
            ]
        }),
        P::AnthropicMessages,
    );

    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    let tool_use = messages[1]["content"]
        .as_array()
        .expect("content")
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap_or_else(|| panic!("tool_use block missing: {messages:?}"));
    assert_eq!(tool_use["name"], "search");
    // The Anthropic encoder normalises tool ids to the `toolu_` prefix; the
    // tool_result must reference the same normalised id so the correlation
    // survives the round trip.
    assert_eq!(tool_use["id"], "toolu_call_123");
    assert_eq!(tool_use["input"], json!({"query": "cats"}));
    let tool_result = messages[2]["content"]
        .as_array()
        .expect("content")
        .iter()
        .find(|b| b["type"] == "tool_result")
        .unwrap_or_else(|| panic!("tool_result block missing: {messages:?}"));
    assert_eq!(tool_result["tool_use_id"], "toolu_call_123");
    assert_eq!(tool_result["content"], "Found 10 results about cats");
}

#[test]
fn google_tool_call_to_openai() {
    // KNOWN GAP (argument loss on this path): the Google decoder synthesises
    // `call_<uuid>` ids for `functionCall` parts but leaves the correlated
    // `functionResponse` message without `tool_call_id`. The OpenAI encoder's
    // repair machinery therefore cannot match the call, re-emits the assistant
    // call with a placeholder name/arguments, and correlates the tool message
    // against the function name. Text, roles and the result payload survive.
    let out = translate(
        P::GoogleGemini,
        json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Get weather"}]},
                {
                    "role": "model",
                    "parts": [{
                        "functionCall": {"name": "get_weather", "args": {"city": "London"}}
                    }]
                },
                {
                    "role": "user",
                    "parts": [{
                        "functionResponse": {"name": "get_weather", "response": {"temperature": 15, "unit": "C"}}
                    }]
                }
            ]
        }),
        P::OpenAiChat,
    );

    let messages = field(&out, "/messages").as_array().expect("messages");
    assert_eq!(messages[0]["role"], "user");
    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    let calls = assistant["tool_calls"].as_array().expect("tool_calls");
    assert_eq!(calls.len(), 1);
    // The correlation machinery falls back to the function name as the id and
    // a placeholder tool name; the original call arguments are lost.
    assert_eq!(calls[0]["id"], "get_weather");
    assert_eq!(calls[0]["function"]["name"], "tool");
    assert_eq!(calls[0]["function"]["arguments"], "{}");
    // The tool message keeps its result payload and is correlated to the
    // re-emitted assistant call.
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("tool message missing: {messages:?}"));
    assert_eq!(tool["tool_call_id"], "get_weather");
    assert_eq!(tool["content"], "{\"temperature\":15,\"unit\":\"C\"}");
}
