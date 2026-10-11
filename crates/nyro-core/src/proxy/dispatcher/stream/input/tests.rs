use super::*;
use crate::protocol::ids::*;
use crate::proxy::dispatcher::stream_probe::{ProbeOutcome, probe_stream};
use serde_json::json;

fn google_hook(channel: &str) -> StreamRawChunkHook {
    StreamRawChunkHook {
        vendor: std::sync::Arc::new(crate::provider::google::GoogleVendor),
        provider: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "vendor": "google",
            "protocol": "google-gemini", "base_url": "http://127.0.0.1",
            "channel": channel, "api_key": "fixture", "auth_mode": "oauth",
            "is_enabled": true, "created_at": "", "updated_at": ""
        }))
        .unwrap(),
        protocol_id: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        api_key: "fixture".into(),
        actual_model: "gemini-fixture".into(),
        credential: None,
    }
}

fn google_wire(newline: &str, terminated: bool, enveloped: bool) -> String {
    let mut body = json!({
        "candidates": [{"content":{"parts":[
            {"text":"你好🌍"},
            {"functionCall":{"name":"read","args":{"file":"目录/🌍.txt"}},"thoughtSignature":"signature"}
        ]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}
    });
    if enveloped {
        body = json!({"response": body, "traceId":"trace"});
    }
    format!(
        ": ping{newline}{newline}event: message{newline}data: {body}{}",
        if terminated {
            newline.repeat(2)
        } else {
            String::new()
        }
    )
}

fn assert_google_output(deltas: &[AiStreamDelta]) {
    assert_eq!(
        deltas
            .iter()
            .filter_map(|d| match d {
                AiStreamDelta::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<String>(),
        "你好🌍",
        "{deltas:?}"
    );
    let args: Vec<_> = deltas
        .iter()
        .filter_map(|d| match d {
            AiStreamDelta::ToolCallDelta { arguments, .. } => {
                Some(serde_json::from_str::<serde_json::Value>(arguments).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(args, vec![json!({"file":"目录/🌍.txt"})]);
    assert_eq!(
        deltas
            .iter()
            .filter(|d| matches!(d, AiStreamDelta::ToolCallStart { .. }))
            .count(),
        1
    );
    assert_eq!(
        deltas
            .iter()
            .filter(|d| matches!(d, AiStreamDelta::Done { .. }))
            .count(),
        1
    );
    assert!(deltas.iter().any(
        |d| matches!(d, AiStreamDelta::Usage(u) if u.prompt_tokens == 7 && u.completion_tokens == 3)
    ));
    assert!(
        deltas
            .iter()
            .any(|d| matches!(d, AiStreamDelta::ThinkingSignature(s) if s == "signature"))
    );
    assert!(!deltas.iter().any(|d| matches!(
        d,
        AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
    )));
}

async fn decode_chunks(
    protocol: ProtocolId,
    chunks: &[&[u8]],
    hook: Option<&StreamRawChunkHook>,
) -> Vec<AiStreamDelta> {
    let mut parser = protocol.handler().make_stream_response_decoder();
    let mut input = StreamInput::default();
    let mut deltas = Vec::new();
    for chunk in chunks {
        deltas.extend(
            input
                .decode_chunk(chunk, parser.as_mut(), hook, None)
                .await
                .unwrap(),
        );
    }
    deltas.extend(input.finish(parser.as_mut(), hook, None).await.unwrap());
    deltas
}

#[tokio::test]
async fn google_subscription_envelope_survives_every_byte_split_and_eof() {
    for channel in ["antigravity", "gemini-cli"] {
        let hook = google_hook(channel);
        for newline in ["\n", "\r\n", "\r"] {
            for terminated in [true, false] {
                let wire = google_wire(newline, terminated, true);
                for split in 0..=wire.len() {
                    let deltas = decode_chunks(
                        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                        &[&wire.as_bytes()[..split], &wire.as_bytes()[split..]],
                        Some(&hook),
                    )
                    .await;
                    assert_google_output(&deltas);
                }
                let chunks: Vec<_> = wire.as_bytes().chunks(1).collect();
                assert_google_output(
                    &decode_chunks(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, &chunks, Some(&hook))
                        .await,
                );
            }
        }
    }
}

#[tokio::test]
async fn native_ir_utf8_text_and_tool_arguments_survive_every_byte_split() {
    let args = json!({"file":"目录/🌍.txt"}).to_string();
    let mut fixtures = vec![(
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        google_wire("\n", true, false),
    )];
    fixtures.push((OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, format!("data: {}\n\ndata: [DONE]\n\n", json!({
        "id":"chat-1","model":"fixture","choices":[{"index":0,"delta":{"content":"你好🌍","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":args}}]},"finish_reason":"tool_calls"}]
    }))));
    fixtures.push((ANTHROPIC_MESSAGES_2023_06_01, format!(
        "event: content_block_delta\ndata: {}\n\nevent: content_block_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好🌍"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call-1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":args}})
    )));
    fixtures.push((OPENAI_RESPONSES_V1, format!(
        "event: response.output_text.delta\ndata: {}\n\nevent: response.output_item.added\ndata: {}\n\nevent: response.function_call_arguments.delta\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"你好🌍"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"read","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc-1","delta":args})
    )));
    for (protocol, wire) in fixtures {
        for newline in ["\n", "\r\n", "\r"] {
            let wire = wire.replace('\n', newline);
            for split in 0..=wire.len() {
                let deltas = decode_chunks(
                    protocol,
                    &[&wire.as_bytes()[..split], &wire.as_bytes()[split..]],
                    None,
                )
                .await;
                let text: String = deltas
                    .iter()
                    .filter_map(|d| match d {
                        AiStreamDelta::TextDelta(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(text, "你好🌍", "{protocol} split={split}: {deltas:?}");
                let actual: String = deltas
                    .iter()
                    .filter_map(|d| match d {
                        AiStreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&actual).unwrap(),
                    json!({"file":"目录/🌍.txt"}),
                    "{protocol} split={split}"
                );
            }
        }
    }
}

#[tokio::test]
async fn probe_handoff_keeps_pending_envelope_and_utf8_tail() {
    let hook = google_hook("antigravity");
    let first = b"data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"first\"}]}}]}}\r\n\r\n";
    let second = google_wire("\r\n", false, true);
    let split = second.find('你').unwrap() + 1;
    let mut chunk = first.to_vec();
    chunk.extend_from_slice(&second.as_bytes()[..split]);
    let mut stream = futures::stream::iter(vec![
        Ok::<_, crate::proxy::client::UpstreamReadError>(bytes::Bytes::from(chunk)),
        Ok(bytes::Bytes::copy_from_slice(&second.as_bytes()[split..])),
    ]);
    let mut parser = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
        .handler()
        .make_stream_response_decoder();
    let mut input = StreamInput::default();
    let result = probe_stream(
        &mut stream,
        parser.as_mut(),
        &mut input,
        Some(&hook),
        None,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "split-test",
        std::time::Instant::now(),
    )
    .await;
    let ProbeOutcome::Commit {
        buffered,
        terminal_error,
        parser_finished,
        ..
    } = result
    else {
        panic!("first payload must commit")
    };
    assert!(terminal_error.is_none());
    assert!(!parser_finished);
    assert!(
        buffered
            .iter()
            .any(|d| matches!(d, AiStreamDelta::TextDelta(t) if t == "first"))
    );
    use futures::StreamExt;
    let rest = stream.next().await.unwrap().unwrap();
    let mut remaining = input
        .decode_chunk(&rest, parser.as_mut(), Some(&hook), None)
        .await
        .unwrap();
    remaining.extend(
        input
            .finish(parser.as_mut(), Some(&hook), None)
            .await
            .unwrap(),
    );
    assert_google_output(&remaining);
}

#[tokio::test]
async fn invalid_and_truncated_utf8_fail_instead_of_replacing_tool_input() {
    let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let mut parser = protocol.handler().make_stream_response_decoder();
    let mut input = StreamInput::default();
    assert!(
        input
            .decode_chunk(b"data: \xff\n\n", parser.as_mut(), None, None)
            .await
            .is_err()
    );
    let mut input = StreamInput::default();
    assert!(
        input
            .decode_chunk(b"data: \xe4", parser.as_mut(), None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(input.finish(parser.as_mut(), None, None).await.is_err());
}
