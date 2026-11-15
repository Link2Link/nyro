//! Synthetic, offline Gemini terminal/usage ordering regressions.
//!
//! A finishReason is provisional until clean EOF: usage-only tails must reach
//! both client formatters before their one terminal event. Missing finishReason
//! is deliberately strict, even for a syntactically complete functionCall.

use nyro_core::protocol::codec::google::gemini::stream::{
    GoogleStreamFormatter, GoogleStreamParser,
};
use nyro_core::protocol::codec::openai::compatible::stream::OpenAIStreamFormatter;
use nyro_core::protocol::ir::AiStreamDelta;
use nyro_core::protocol::ir::error::AiErrorKind;
use nyro_core::protocol::{SseEvent, StreamResponseDecoder, StreamResponseEncoder};
use serde_json::{Value, json};

fn frame(value: &Value) -> String {
    format!("data: {value}\n\n")
}

fn finish_frame(reason: &str) -> Value {
    json!({"candidates": [{"finishReason": reason}]})
}

fn usage_frame() -> Value {
    json!({
        "usageMetadata": {
            "promptTokenCount": 11,
            "candidatesTokenCount": 17,
            "thoughtsTokenCount": 5,
            "totalTokenCount": 33,
            "cachedContentTokenCount": 7,
            "trafficType": "ON_DEMAND"
        },
        "responseId": "response-split"
    })
}

fn assert_no_terminal(deltas: &[AiStreamDelta]) {
    assert!(
        !deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::Done { .. } | AiStreamDelta::StreamError { .. }
        )),
        "unexpected terminal delta: {deltas:?}"
    );
}

fn assert_one_done(deltas: &[AiStreamDelta], reason: &str) {
    assert_eq!(
        deltas
            .iter()
            .filter(|delta| matches!(delta, AiStreamDelta::Done { .. }))
            .count(),
        1,
        "{deltas:?}"
    );
    assert!(
        matches!(deltas.last(), Some(AiStreamDelta::Done { stop_reason }) if stop_reason == reason)
    );
    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::StreamError { .. }))
    );
}

fn assert_unexpected_eof(deltas: &[AiStreamDelta]) {
    assert!(
        matches!(
            deltas.last(),
            Some(AiStreamDelta::StreamError { error }) if error.kind == AiErrorKind::UnexpectedEof
        ),
        "{deltas:?}"
    );
    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
    );
}

fn json_events(events: &[SseEvent]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

fn assert_terminal_usage(events: &[SseEvent], google: bool, reason: &str) {
    let values = json_events(events);
    let terminal: Vec<_> = values
        .iter()
        .filter(|value| {
            if google {
                value["candidates"][0]["finishReason"].is_string()
            } else {
                value["choices"][0]["finish_reason"].is_string()
            }
        })
        .collect();
    assert_eq!(terminal.len(), 1, "{values:?}");
    assert_eq!(Some(terminal[0]), values.last());
    if google {
        assert_eq!(terminal[0]["candidates"][0]["finishReason"], reason);
        assert_eq!(terminal[0]["usageMetadata"], usage_frame()["usageMetadata"]);
        assert_eq!(terminal[0]["responseId"], "response-split");
    } else {
        assert_eq!(terminal[0]["choices"][0]["finish_reason"], reason);
        assert_eq!(terminal[0]["usage"]["prompt_tokens"], 11);
        assert_eq!(terminal[0]["usage"]["completion_tokens"], 22);
        assert_eq!(terminal[0]["usage"]["total_tokens"], 33);
        assert_eq!(
            terminal[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
            7
        );
        assert_eq!(events.last().unwrap().data, "[DONE]");
        assert_eq!(
            events.iter().filter(|event| event.data == "[DONE]").count(),
            1
        );
    }
}

#[test]
fn gemini_terminal_usage_orderings_reach_openai_and_google_once() {
    let terminal = finish_frame("STOP");
    let usage = usage_frame();
    let mut combined = terminal.clone();
    combined["usageMetadata"] = usage["usageMetadata"].clone();
    combined["responseId"] = usage["responseId"].clone();
    for frames in [
        vec![terminal.clone(), usage.clone()],
        vec![usage.clone(), terminal.clone()],
        vec![
            terminal.clone(),
            terminal.clone(),
            usage.clone(),
            terminal.clone(),
        ],
        vec![combined.clone()],
        // A later partial metadata snapshot must not erase earlier counters or details.
        vec![
            combined.clone(),
            json!({"usageMetadata": {"cachedContentTokenCount": 7}}),
        ],
        vec![json!({"usageMetadata": {"promptTokenCount": 3}}), combined],
        // Even finish + usage can be followed by a newer usage snapshot.
        vec![
            json!({
                "candidates": [{"finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1}
            }),
            usage.clone(),
        ],
    ] {
        let mut parser = GoogleStreamParser::new();
        let mut chat = OpenAIStreamFormatter::new();
        let mut google = GoogleStreamFormatter::new();
        let mut chat_events = Vec::new();
        let mut google_events = Vec::new();
        let mut all = Vec::new();
        for value in frames {
            let deltas = parser.parse_chunk(&frame(&value)).unwrap();
            assert_no_terminal(&deltas);
            chat_events.extend(chat.format_deltas(&deltas));
            google_events.extend(google.format_deltas(&deltas));
            all.extend(deltas);
        }
        let final_deltas = parser.finish().unwrap();
        chat_events.extend(chat.format_deltas(&final_deltas));
        google_events.extend(google.format_deltas(&final_deltas));
        all.extend(final_deltas);
        assert_one_done(&all, "stop");
        let usage = all
            .iter()
            .rev()
            .find_map(|delta| match delta {
                AiStreamDelta::Usage(usage) => Some(usage),
                _ => None,
            })
            .expect("usage must precede Done");
        assert_eq!(usage.completion_tokens, 22);
        assert_eq!(usage.reasoning_tokens, Some(5));
        assert_eq!(usage.cache_read_tokens, Some(7));
        assert_terminal_usage(&chat_events, false, "stop");
        assert_terminal_usage(&google_events, true, "STOP");
        assert!(parser.finish().unwrap().is_empty());
        assert!(parser.parse_chunk(&frame(&terminal)).unwrap().is_empty());
        assert!(chat.format_done().is_empty());
        assert!(google.format_done().is_empty());
    }
}

#[test]
fn gemini_terminal_usage_tail_survives_every_transport_split_and_line_ending() {
    for newline in ["\n", "\r\n", "\r"] {
        for eof_ending in ["", newline] {
            let raw = format!(
                "data:{}{newline}{newline}data:{}{eof_ending}",
                finish_frame("MAX_TOKENS"),
                usage_frame()
            );
            for split in (0..=raw.len()).filter(|index| raw.is_char_boundary(*index)) {
                let mut parser = GoogleStreamParser::new();
                let mut deltas = parser.parse_chunk(&raw[..split]).unwrap();
                deltas.extend(parser.parse_chunk("").unwrap());
                deltas.extend(parser.parse_chunk(&raw[split..]).unwrap());
                assert_no_terminal(&deltas);
                deltas.extend(parser.finish().unwrap());
                assert_one_done(&deltas, "length");
                let mut formatter = OpenAIStreamFormatter::new();
                assert_terminal_usage(&formatter.format_deltas(&deltas), false, "length");
            }
        }
    }
}

#[test]
fn gemini_terminal_signed_tool_calls_keep_identity_arguments_and_signatures() {
    let mut parser = GoogleStreamParser::new();
    let content = json!({"candidates": [{"content": {"parts": [
        {"text": "checking", "thought": true, "thoughtSignature": "SIG-THOUGHT"},
        {"functionCall": {"id": "call-fixed", "name": "ping", "args": {}}, "thoughtSignature": "SIG-CALL"}
    ]}, "finishReason": "STOP"}]});
    let mut deltas = parser.parse_chunk(&frame(&content)).unwrap();
    assert_no_terminal(&deltas);
    deltas.extend(parser.parse_chunk(&frame(&usage_frame())).unwrap());
    deltas.extend(parser.finish().unwrap());
    assert_one_done(&deltas, "stop");
    assert!(
        deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::ThinkingDelta(text) if text == "checking"))
    );
    assert!(deltas.iter().any(|delta| matches!(delta, AiStreamDelta::ToolCallStart { index: 0, id, name, .. } if id == "call-fixed" && name == "ping")));
    assert!(deltas.iter().any(|delta| matches!(delta, AiStreamDelta::ToolCallDelta { index: 0, arguments } if arguments == "{}")));
    let signatures: Vec<_> = deltas
        .iter()
        .filter_map(|delta| match delta {
            AiStreamDelta::ThinkingSignature(signature) => Some(signature.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(signatures, ["SIG-THOUGHT", "SIG-CALL"]);
    let mut formatter = OpenAIStreamFormatter::new();
    assert_terminal_usage(&formatter.format_deltas(&deltas), false, "tool_calls");
}

#[test]
fn gemini_terminal_clean_eof_requires_explicit_finish_even_for_complete_tools() {
    let tool = json!({"candidates": [{"content": {"parts": [{
        "functionCall": {"id": "complete-call", "name": "ping", "args": {}}
    }]}}]});
    for value in [
        tool.clone(),
        json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]}),
        usage_frame(),
        finish_frame(""),
    ] {
        let mut parser = GoogleStreamParser::new();
        let mut deltas = parser.parse_chunk(&frame(&value)).unwrap();
        assert_no_terminal(&deltas);
        if value == tool {
            assert!(deltas.iter().any(|delta| matches!(delta, AiStreamDelta::ToolCallDelta { arguments, .. } if arguments == "{}")));
        }
        deltas.extend(parser.finish().unwrap());
        assert_unexpected_eof(&deltas);
        let mut formatter = OpenAIStreamFormatter::new();
        assert!(
            !formatter
                .format_deltas(&deltas)
                .iter()
                .any(|event| event.data == "[DONE]")
        );
        assert!(parser.finish().unwrap().is_empty());
        assert!(
            parser
                .parse_chunk(&frame(&finish_frame("STOP")))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn gemini_terminal_empty_comment_and_sentinel_only_streams_are_not_successful() {
    for raw in [
        "",
        " \n",
        ": keepalive\n\n",
        "data: [DONE]\n\n",
        "data: {}\n\n",
    ] {
        let mut parser = GoogleStreamParser::new();
        let mut deltas = parser.parse_chunk(raw).unwrap();
        assert_no_terminal(&deltas);
        deltas.extend(parser.finish().unwrap());
        assert_unexpected_eof(&deltas);
        assert!(parser.finish().unwrap().is_empty());
    }
}

#[test]
fn gemini_terminal_pending_finish_does_not_mask_malformed_or_truncated_tail() {
    for (tail, kind) in [
        ("data: {", AiErrorKind::UnexpectedEof),
        ("data: {\n\n", AiErrorKind::UnexpectedEof),
        ("data: {invalid}\n\n", AiErrorKind::StreamMidError),
        (
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"ping\"}}]}}]}\n\n",
            AiErrorKind::StreamMidError,
        ),
    ] {
        let mut parser = GoogleStreamParser::new();
        let mut deltas = parser.parse_chunk(&frame(&finish_frame("STOP"))).unwrap();
        assert_no_terminal(&deltas);
        deltas.extend(parser.parse_chunk(tail).unwrap());
        deltas.extend(parser.finish().unwrap());
        assert_eq!(
            deltas
                .iter()
                .filter(|delta| matches!(delta, AiStreamDelta::StreamError { .. }))
                .count(),
            1
        );
        assert!(
            matches!(deltas.last(), Some(AiStreamDelta::StreamError { error }) if error.kind == kind)
        );
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
        );
        assert!(parser.finish().unwrap().is_empty());
        assert!(
            parser
                .parse_chunk(&frame(&finish_frame("STOP")))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn gemini_terminal_finish_without_usage_finalizes_once_at_clean_eof() {
    for (native, expected) in [
        ("STOP", "stop"),
        ("MAX_TOKENS", "length"),
        ("SAFETY", "SAFETY"),
    ] {
        let mut parser = GoogleStreamParser::new();
        let deltas = parser.parse_chunk(&frame(&finish_frame(native))).unwrap();
        assert_no_terminal(&deltas);
        assert_one_done(&parser.finish().unwrap(), expected);
        assert!(parser.finish().unwrap().is_empty());
    }
}
