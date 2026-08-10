//! Streaming conversion-correctness suite.
//!
//! Ported from `llm-bridge/test/streaming.test.ts`, adapted to Nyro's IR
//! delta model (`AiStreamDelta`) and per-protocol stream parsers/formatters:
//!
//! * `parseOpenAIStream`        → `OpenAIStreamParser` / `parse_stream_chunks`
//! * `emitOpenAIStream`         → `OpenAIStreamFormatter` / `format_stream`
//! * …same for Anthropic / Google / OpenAI Responses
//!
//! Stop-reason vocabulary: Nyro's IR normalises to OpenAI-style reasons
//! (`stop` / `tool_calls` / `length` / `content_filter`) at every parser
//! boundary and restores the target wire vocabulary at every formatter.
//! llm-bridge's `"end_turn"` therefore appears as `"stop"` in the IR.

mod conv_common;

use conv_common::*;
use nyro_core::protocol::ir::ToolCallKind;

// ── OpenAI parser ────────────────────────────────────────────────────────────

#[test]
fn openai_parser_basic_content_deltas() {
    let deltas = parse_stream(
        P::OpenAiChat,
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\
         \ndata: [DONE]",
    );

    assert_delta_eq(&deltas[0], &StreamDelta::MessageStart {
        id: "chatcmpl-1".to_string(),
        model: "gpt-4".to_string(),
    });
    let texts = delta_text(&deltas);
    assert_eq!(texts, "Hello world");
    assert_last_delta(&deltas, &StreamDelta::Done {
        stop_reason: "stop".to_string(),
    });
}

#[test]
fn openai_parser_tool_call_deltas() {
    let chunks = [
        "data: {\"id\":\"chatcmpl-2\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-2\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-2\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"loc\"}}]},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-2\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ation\\\":\\\"NYC\\\"}\"}}]},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-2\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
        "data: [DONE]",
    ];
    let deltas = parse_stream_chunks(P::OpenAiChat, &chunks);

    assert_delta_eq_msg(
        &deltas[0],
        &StreamDelta::MessageStart {
            id: "chatcmpl-2".to_string(),
            model: "gpt-4".to_string(),
        },
        "{trace}"
    );
    assert_delta_eq_msg(
        &deltas[1],
        &StreamDelta::ToolCallStart {
            index: 0,
            id: "call_abc".to_string(),
            name: "get_weather".to_string(),
            kind: ToolCallKind::Function,
        },
        "{trace}"
    );
    // Arguments accumulate across fragments.
    let joined: String = deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(joined, "{\"location\":\"NYC\"}");
    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "tool_calls".to_string(),
        }
    );
}

#[test]
fn openai_parser_done_marker() {
    let deltas = parse_stream(
        P::OpenAiChat,
        "data: {\"id\":\"chatcmpl-3\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
         \ndata: [DONE]",
    );

    assert_last_delta(&deltas, &StreamDelta::Done {
        stop_reason: "stop".to_string(),
    });
}

#[test]
fn openai_parser_without_done_marker() {
    let deltas = parse_stream(
        P::OpenAiChat,
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
    );

    assert_last_delta(&deltas, &StreamDelta::Done {
        stop_reason: "stop".to_string(),
    });
}

#[test]
fn openai_parser_usage_chunk_no_double_end() {
    let deltas = parse_stream(
        P::OpenAiChat,
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\
         \ndata: [DONE]",
    );

    let ends: Vec<_> = deltas
        .iter()
        .filter(|d| matches!(d, StreamDelta::Done { .. }))
        .collect();
    assert_eq!(ends.len(), 1, "exactly one Done: {}", delta_trace(&deltas));
    assert_delta_eq(ends[0], &StreamDelta::Done {
        stop_reason: "stop".to_string(),
    });
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 10 && u.completion_tokens == 5)),
        "usage captured: {}",
        delta_trace(&deltas)
    );
}

#[test]
fn openai_parser_preserves_finish_reason_when_usage_arrives_separately() {
    let chunks = [
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}",
        "data: [DONE]",
    ];
    let deltas = parse_stream_chunks(P::OpenAiChat, &chunks);

    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "stop".to_string(),
        }
    );
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 12 && u.completion_tokens == 3))
    );
}

#[test]
fn openai_parser_preserves_tool_calls_finish_reason_with_usage() {
    let chunks = [
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"loc\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8}}",
        "data: [DONE]",
    ];
    let deltas = parse_stream_chunks(P::OpenAiChat, &chunks);

    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "tool_calls".to_string(),
        }
    );
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 20 && u.completion_tokens == 8))
    );
}

#[test]
fn openai_parser_done_fallback_uses_last_finish_reason() {
    let deltas = parse_stream(
        P::OpenAiChat,
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\
         \ndata: [DONE]",
    );

    assert_last_delta(&deltas, &StreamDelta::Done {
        stop_reason: "stop".to_string(),
    });
}

// ── Anthropic parser ─────────────────────────────────────────────────────────

#[test]
fn anthropic_parser_message_start() {
    // Nyro's Anthropic parser deliberately emits `Usage` BEFORE `MessageStart`
    // (stream.rs: `Usage BEFORE MessageStart so the formatter has the correct
    // input_tokens available when it emits the message_start SSE event`).
    let deltas = parse_stream(
        P::AnthropicMessages,
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\
         \nevent: message_stop\n\
         data: {\"type\":\"message_stop\"}",
    );

    assert!(
        matches!(&deltas[0], StreamDelta::Usage(u) if u.prompt_tokens == 10),
        "usage surfaces from message_start: {}",
        delta_trace(&deltas)
    );
    assert_delta_eq(&deltas[1], &StreamDelta::MessageStart {
        id: "msg_01".to_string(),
        model: "claude-3-5-sonnet".to_string(),
    });
}

#[test]
fn anthropic_parser_text_deltas() {
    let deltas = parse_stream(
        P::AnthropicMessages,
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\"model\":\"claude-3-5-sonnet\",\"content\":[]}}\n\
         \nevent: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
         \nevent: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
         \nevent: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\
         \nevent: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\
         \nevent: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\
         \nevent: message_stop\n\
         data: {\"type\":\"message_stop\"}",
    );

    assert_eq!(delta_text(&deltas), "Hi there");
    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            // IR normalises Anthropic `end_turn` to the OpenAI vocabulary.
            stop_reason: "stop".to_string(),
        }
    );
    assert!(
        deltas
            .iter()
            .any(|d| matches!(d, StreamDelta::Usage(u) if u.completion_tokens == 5))
    );
}

#[test]
fn anthropic_parser_usage_from_message_delta() {
    let deltas = parse_stream(
        P::AnthropicMessages,
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_usage\",\"model\":\"claude-3-5-sonnet\",\"content\":[]}}\n\
         \nevent: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
         \nevent: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
         \nevent: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\
         \nevent: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":25,\"output_tokens\":13}}\n\
         \nevent: message_stop\n\
         data: {\"type\":\"message_stop\"}",
    );

    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "stop".to_string(),
        }
    );
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 25 && u.completion_tokens == 13)),
        "usage from message_delta: {}",
        delta_trace(&deltas)
    );
}

#[test]
fn anthropic_parser_thinking_deltas() {
    let deltas = parse_stream(
        P::AnthropicMessages,
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_03\",\"model\":\"claude-3-5-sonnet\",\"content\":[]}}\n\
         \nevent: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\
         \nevent: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think...\"}}\n\
         \nevent: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\
         \nevent: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\
         \nevent: message_stop\n\
         data: {\"type\":\"message_stop\"}",
    );

    let thinking: Vec<_> = deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ThinkingDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, vec!["Let me think..."]);
}

// ── Google parser ────────────────────────────────────────────────────────────

#[test]
fn google_parser_content_deltas() {
    let deltas = parse_stream(
        P::GoogleGemini,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}],\"role\":\"model\"}}]}\n\
         \ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}],\"role\":\"model\"}}]}\n\
         \ndata: {\"candidates\":[{\"content\":{\"parts\":[],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}",
    );

    // Nyro synthesises `gen-<uuid>` ids and reads the model from `modelVersion`
    // (absent in these chunks), so the start delta has an empty model.
    assert!(matches!(&deltas[0], StreamDelta::MessageStart { id, model } if id.starts_with("gen-") && model.is_empty()));
    assert_eq!(delta_text(&deltas), "Hi there");
    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "stop".to_string(),
        }
    );
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 5 && u.completion_tokens == 2)),
        "{}",
        delta_trace(&deltas)
    );
}

#[test]
fn google_parser_finish_reason_without_usage() {
    let deltas = parse_stream(
        P::GoogleGemini,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}]}\n\
         \ndata: {\"candidates\":[{\"content\":{\"parts\":[],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}",
    );

    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "stop".to_string(),
        }
    );
}

#[test]
fn google_parser_no_done_without_finish_reason() {
    // llm-bridge emits a synthetic `message_end` when a Google stream ends
    // without a finishReason; Nyro's parser does not — a stream truncated
    // mid-flight is surfaced as `UnexpectedEof` by the accumulator instead.
    let deltas = parse_stream(
        P::GoogleGemini,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Partial\"}],\"role\":\"model\"}}]}",
    );

    assert_eq!(delta_text(&deltas), "Partial", "{}", delta_trace(&deltas));
    assert!(
        !deltas.iter().any(|d| matches!(d, StreamDelta::Done { .. })),
        "no Done without finishReason: {}",
        delta_trace(&deltas)
    );
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::MessageStart { .. })),
        "start delta present: {}",
        delta_trace(&deltas)
    );
}

#[test]
fn google_parser_no_duplicate_message_end() {
    let chunks = [
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}]}",
        "data: {\"candidates\":[{\"content\":{\"parts\":[],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}",
        "data: {\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"totalTokenCount\":13}}",
    ];
    let deltas = parse_stream_chunks(P::GoogleGemini, &chunks);

    let ends: Vec<_> = deltas
        .iter()
        .filter(|d| matches!(d, StreamDelta::Done { .. }))
        .collect();
    assert_eq!(ends.len(), 1, "exactly one Done: {}", delta_trace(&deltas));
    assert_delta_eq(ends[0], &StreamDelta::Done {
        stop_reason: "stop".to_string(),
    });
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 10 && u.completion_tokens == 3))
    );
}

// ── OpenAI Responses parser ──────────────────────────────────────────────────

#[test]
fn responses_parser_created_and_text_deltas() {
    let deltas = parse_stream(
        P::OpenAiResponses,
        "event: response.created\n\
         data: {\"response\":{\"id\":\"resp_01\",\"model\":\"gpt-4o\",\"status\":\"in_progress\"}}\n\
         \nevent: response.output_text.delta\n\
         data: {\"delta\":\"Hello\"}\n\
         \nevent: response.output_text.delta\n\
         data: {\"delta\":\" world\"}\n\
         \nevent: response.completed\n\
         data: {\"response\":{\"id\":\"resp_01\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}",
    );

    assert_delta_eq(&deltas[0], &StreamDelta::MessageStart {
        id: "resp_01".to_string(),
        model: "gpt-4o".to_string(),
    });
    assert_eq!(delta_text(&deltas), "Hello world");
    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "stop".to_string(),
        }
    );
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 10 && u.completion_tokens == 5))
    );
}

#[test]
fn responses_parser_function_call_events() {
    let deltas = parse_stream(
        P::OpenAiResponses,
        "event: response.created\n\
         data: {\"response\":{\"id\":\"resp_02\",\"model\":\"gpt-4o\"}}\n\
         \nevent: response.output_item.added\n\
         data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"call_xyz\",\"name\":\"get_weather\"}}\n\
         \nevent: response.function_call_arguments.delta\n\
         data: {\"delta\":\"{\\\"loc\"}\n\
         \nevent: response.function_call_arguments.delta\n\
         data: {\"delta\":\"ation\\\":\\\"NYC\\\"}\"}\n\
         \nevent: response.output_item.done\n\
         data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"call_xyz\"}}\n\
         \nevent: response.completed\n\
         data: {\"response\":{\"id\":\"resp_02\",\"status\":\"completed\"}}",
    );

    let trace = delta_trace(&deltas);
    assert!(matches!(
        &deltas[1],
        StreamDelta::ToolCallStart {
            index: 0,
            id,
            name,
            kind: ToolCallKind::Function,
        } if id == "call_xyz" && name == "get_weather"
    ), "{trace}");
    let joined: String = deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(joined, "{\"location\":\"NYC\"}");
    // KNOWN GAP: the Responses parser never emits `ToolCallComplete` — the
    // nyro IR surface for this protocol is ToolCallStart + ToolCallDelta only
    // (llm-bridge's parser completes calls from `output_item.done`).
    assert!(
        !deltas.iter().any(|d| matches!(d, StreamDelta::ToolCallComplete { .. })),
        "no ToolCallComplete from the Responses parser: {trace}"
    );
}

#[test]
fn responses_parser_multiple_concurrent_function_calls() {
    let deltas = parse_stream(
        P::OpenAiResponses,
        "event: response.created\n\
         data: {\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-4o\"}}\n\
         \nevent: response.output_item.added\n\
         data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc_1\",\"name\":\"get_weather\"}}\n\
         \nevent: response.output_item.added\n\
         data: {\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc_2\",\"name\":\"get_time\"}}\n\
         \nevent: response.function_call_arguments.delta\n\
         data: {\"output_index\":0,\"delta\":\"{\\\"loc\\\":\\\"SF\\\"}\"}\n\
         \nevent: response.function_call_arguments.delta\n\
         data: {\"output_index\":1,\"delta\":\"{\\\"tz\\\":\\\"PST\\\"}\"}\n\
         \nevent: response.output_item.done\n\
         data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc_1\"}}\n\
         \nevent: response.output_item.done\n\
         data: {\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc_2\"}}\n\
         \nevent: response.completed\n\
         data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":20,\"output_tokens\":15}}}",
    );

    let trace = delta_trace(&deltas);
    let starts: Vec<_> = deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCallStart { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec!["fc_1", "fc_2"], "{trace}");
    let deltas_by_index: Vec<_> = deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCallDelta { index, arguments } => {
                Some((*index, arguments.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas_by_index,
        vec![(0, "{\"loc\":\"SF\"}"), (1, "{\"tz\":\"PST\"}")],
        "{trace}"
    );
    // KNOWN GAP: as with the single-call case, the Responses parser never
    // emits `ToolCallComplete`; completion state is implied by the deltas.
    assert!(
        !deltas.iter().any(|d| matches!(d, StreamDelta::ToolCallComplete { .. })),
        "no ToolCallComplete from the Responses parser: {trace}"
    );
}

// ── OpenAI emitter ───────────────────────────────────────────────────────────

#[test]
fn openai_emitter_format() {
    let events = format_stream(
        P::OpenAiChat,
        &[
            StreamDelta::MessageStart {
                id: "chatcmpl-test".to_string(),
                model: "gpt-4".to_string(),
            },
            StreamDelta::TextDelta("Hello".to_string()),
            StreamDelta::TextDelta(" world".to_string()),
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let out = sse_string(&events);
    assert!(out.contains("data: [DONE]"), "{out}");
    let jsons = sse_jsons(&events);
    assert_eq!(jsons[0]["id"], "chatcmpl-test");
    assert_eq!(jsons[0]["model"], "gpt-4");
    assert_eq!(jsons[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(jsons[1]["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(jsons[2]["choices"][0]["delta"]["content"], " world");
    assert_eq!(jsons[3]["choices"][0]["finish_reason"], "stop");
}

// ── Anthropic emitter ────────────────────────────────────────────────────────

#[test]
fn anthropic_emitter_format() {
    let events = format_stream(
        P::AnthropicMessages,
        &[
            StreamDelta::MessageStart {
                id: "msg_test".to_string(),
                model: "claude-3-5-sonnet".to_string(),
            },
            StreamDelta::TextDelta("Hi".to_string()),
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let out = sse_string(&events);
    for needle in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: message_delta",
        "event: message_stop",
    ] {
        assert!(out.contains(needle), "missing {needle} in: {out}");
    }
    let jsons = sse_jsons(&events);
    let start = jsons
        .iter()
        .find(|j| j.get("type").and_then(|t| t.as_str()) == Some("message_start"))
        .expect("message_start");
    assert_eq!(start["message"]["id"], "msg_test");
    assert_eq!(start["message"]["model"], "claude-3-5-sonnet");
    let delta = jsons
        .iter()
        .find(|j| j.get("delta").and_then(|d| d.get("type")).and_then(|t| t.as_str()) == Some("text_delta"))
        .expect("text_delta");
    assert_eq!(delta["delta"]["text"], "Hi");
}

#[test]
fn anthropic_emitter_parallel_tool_calls_unique_block_indices() {
    let events = format_stream(
        P::AnthropicMessages,
        &[
            StreamDelta::MessageStart {
                id: "msg_parallel".to_string(),
                model: "claude-3-5-sonnet".to_string(),
            },
            StreamDelta::TextDelta("Let me call two tools.".to_string()),
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{\"city\":\"NYC\"}".to_string(),
            },
            StreamDelta::ToolCallStart {
                index: 1,
                id: "call_2".to_string(),
                name: "get_time".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamDelta::ToolCallDelta {
                index: 1,
                arguments: "{\"tz\":\"EST\"}".to_string(),
            },
            StreamDelta::ToolCallComplete {
                index: 0,
                tool_call: ToolCall::function("call_1", "get_weather", "{\"city\":\"NYC\"}"),
            },
            StreamDelta::ToolCallComplete {
                index: 1,
                tool_call: ToolCall::function("call_2", "get_time", "{\"tz\":\"EST\"}"),
            },
            StreamDelta::Done {
                stop_reason: "tool_calls".to_string(),
            },
        ],
    );

    let jsons = sse_jsons(&events);
    let tool_starts: Vec<_> = jsons
        .iter()
        .filter(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
                && j.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("tool_use")
        })
        .collect();
    assert_eq!(tool_starts.len(), 2);
    let i0 = tool_starts[0]["index"].as_u64().expect("index");
    let i1 = tool_starts[1]["index"].as_u64().expect("index");
    assert_ne!(i0, i1, "parallel tool blocks must have distinct indices");
    assert_eq!(tool_starts[0]["content_block"]["id"], "call_1");
    assert_eq!(tool_starts[1]["content_block"]["id"], "call_2");

    let text_start = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
                && j.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("text")
        })
        .expect("text block start");
    assert_eq!(text_start["index"], 0);
}

#[test]
fn anthropic_emitter_multiple_thinking_deltas_one_block() {
    let events = format_stream(
        P::AnthropicMessages,
        &[
            StreamDelta::MessageStart {
                id: "msg_1".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
            },
            StreamDelta::ThinkingDelta("Let me think".to_string()),
            StreamDelta::ThinkingDelta(" about this".to_string()),
            StreamDelta::ThinkingDelta(" carefully.".to_string()),
            StreamDelta::TextDelta("Here is my answer.".to_string()),
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let jsons = sse_jsons(&events);
    let thinking_starts = jsons
        .iter()
        .filter(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
                && j.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("thinking")
        })
        .count();
    let thinking_deltas = jsons
        .iter()
        .filter(|j| {
            j.get("delta").and_then(|d| d.get("type")).and_then(|t| t.as_str())
                == Some("thinking_delta")
        })
        .count();
    let text_starts = jsons
        .iter()
        .filter(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
                && j.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("text")
        })
        .count();

    assert_eq!(thinking_starts, 1, "one thinking block: {:?}", jsons);
    assert_eq!(thinking_deltas, 3, "three thinking deltas");
    assert_eq!(text_starts, 1, "one text block");
}

#[test]
fn anthropic_emitter_sequential_tool_calls() {
    let events = format_stream(
        P::AnthropicMessages,
        &[
            StreamDelta::MessageStart {
                id: "msg_1".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
            },
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{\"loc\":\"SF\"}".to_string(),
            },
            StreamDelta::ToolCallComplete {
                index: 0,
                tool_call: ToolCall::function("call_1", "get_weather", "{\"loc\":\"SF\"}"),
            },
            StreamDelta::ToolCallStart {
                index: 1,
                id: "call_2".to_string(),
                name: "get_time".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamDelta::ToolCallDelta {
                index: 1,
                arguments: "{\"tz\":\"PST\"}".to_string(),
            },
            StreamDelta::ToolCallComplete {
                index: 1,
                tool_call: ToolCall::function("call_2", "get_time", "{\"tz\":\"PST\"}"),
            },
            StreamDelta::Done {
                stop_reason: "tool_calls".to_string(),
            },
        ],
    );

    let out = sse_string(&events);
    for needle in ["get_weather", "get_time", "call_1", "call_2"] {
        assert!(out.contains(needle), "missing {needle} in: {out}");
    }
    let jsons = sse_jsons(&events);
    let tool_starts = jsons
        .iter()
        .filter(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("content_block_start")
                && j.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("tool_use")
        })
        .count();
    assert_eq!(tool_starts, 2);
}

// ── OpenAI Responses emitter ─────────────────────────────────────────────────

#[test]
fn responses_emitter_format() {
    let events = format_stream(
        P::OpenAiResponses,
        &[
            StreamDelta::MessageStart {
                id: "resp_test".to_string(),
                model: "gpt-4o".to_string(),
            },
            StreamDelta::TextDelta("Hello".to_string()),
            StreamDelta::TextDelta(" world".to_string()),
            StreamDelta::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Usage::default()
            }),
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let out = sse_string(&events);
    for needle in [
        "event: response.created",
        "event: response.output_text.delta",
        "event: response.completed",
    ] {
        assert!(out.contains(needle), "missing {needle} in: {out}");
    }
    let jsons = sse_jsons(&events);
    let created = jsons
        .iter()
        .find(|j| j.get("type").and_then(|t| t.as_str()) == Some("response.created"))
        .expect("response.created");
    assert_eq!(created["response"]["id"], "resp_test");
    assert_eq!(created["response"]["model"], "gpt-4o");
    assert_eq!(created["response"]["status"], "in_progress");

    let text_deltas: Vec<_> = jsons
        .iter()
        .filter(|j| j.get("type").and_then(|t| t.as_str()) == Some("response.output_text.delta"))
        .collect();
    assert_eq!(text_deltas.len(), 2);
    assert_eq!(text_deltas[0]["delta"], "Hello");
    assert_eq!(text_deltas[1]["delta"], " world");

    let completed = jsons
        .iter()
        .find(|j| j.get("type").and_then(|t| t.as_str()) == Some("response.completed"))
        .expect("response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 10);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 5);
}

#[test]
fn responses_emitter_tool_calls() {
    let events = format_stream(
        P::OpenAiResponses,
        &[
            StreamDelta::MessageStart {
                id: "resp_tools".to_string(),
                model: "gpt-4o".to_string(),
            },
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_abc".to_string(),
                name: "get_weather".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{\"loc\":".to_string(),
            },
            StreamDelta::ToolCallDelta {
                index: 0,
                arguments: "\"SF\"}".to_string(),
            },
            StreamDelta::ToolCallComplete {
                index: 0,
                tool_call: ToolCall::function("call_abc", "get_weather", "{\"loc\":\"SF\"}"),
            },
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let out = sse_string(&events);
    for needle in [
        "event: response.output_item.added",
        "event: response.function_call_arguments.delta",
        "event: response.output_item.done",
    ] {
        assert!(out.contains(needle), "missing {needle} in: {out}");
    }
    let jsons = sse_jsons(&events);
    let fc_added = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("response.output_item.added")
                && j.pointer("/item/type").and_then(|t| t.as_str()) == Some("function_call")
        })
        .expect("function_call added");
    assert_eq!(fc_added["item"]["call_id"], "call_abc");
    assert_eq!(fc_added["item"]["name"], "get_weather");

    // nyro closes tool calls in the terminal `response.output_item.done` event
    // carrying the accumulated arguments — there is no dedicated
    // `response.function_call_arguments.done` on the wire (llm-bridge emits
    // both).
    let args_done = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done")
                && j.pointer("/item/type").and_then(|t| t.as_str()) == Some("function_call")
        })
        .expect("tool output_item.done");
    assert_eq!(args_done["item"]["arguments"], "{\"loc\":\"SF\"}");
}

#[test]
fn responses_emitter_text_closed_before_tool_calls() {
    // The nyro Responses formatter emits `output_text.done` / `output_item.done`
    // once, in the terminal `response.completed` block, rather than closing the
    // text item the moment a tool call opens (llm-bridge closes it eagerly).
    let events = format_stream(
        P::OpenAiResponses,
        &[
            StreamDelta::MessageStart {
                id: "resp_mixed".to_string(),
                model: "gpt-4o".to_string(),
            },
            StreamDelta::TextDelta("Let me check.".to_string()),
            StreamDelta::ToolCallStart {
                index: 0,
                id: "call_1".to_string(),
                name: "search".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{\"q\":\"test\"}".to_string(),
            },
            StreamDelta::ToolCallComplete {
                index: 0,
                tool_call: ToolCall::function("call_1", "search", "{\"q\":\"test\"}"),
            },
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let jsons = sse_jsons(&events);
    let text_done = jsons
        .iter()
        .find(|j| j.get("type").and_then(|t| t.as_str()) == Some("response.output_text.done"))
        .expect("text must be closed at completion");
    assert_eq!(text_done["text"], "Let me check.");
    let fc_done = jsons
        .iter()
        .find(|j| {
            j.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done")
                && j.pointer("/item/type").and_then(|t| t.as_str()) == Some("function_call")
        })
        .expect("function_call must be completed");
    assert_eq!(fc_done["item"]["arguments"], "{\"q\":\"test\"}");
    // Both closers must be emitted inside the terminal `response.completed`
    // event, so they appear after `response.completed` in the stream.
    let completed_idx = jsons
        .iter()
        .position(|j| j.get("type").and_then(|t| t.as_str()) == Some("response.completed"));
    assert!(completed_idx.is_some(), "completed present: {:?}", jsons);
}

// ── Google emitter ───────────────────────────────────────────────────────────

#[test]
fn google_emitter_format() {
    let events = format_stream(
        P::GoogleGemini,
        &[
            StreamDelta::MessageStart {
                id: "gemini-test".to_string(),
                model: "gemini-pro".to_string(),
            },
            StreamDelta::TextDelta("Hi".to_string()),
            StreamDelta::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                ..Usage::default()
            }),
            StreamDelta::Done {
                stop_reason: "stop".to_string(),
            },
        ],
    );

    let jsons = sse_jsons(&events);
    let content = jsons
        .iter()
        .find(|j| {
            j.pointer("/candidates/0/content/parts/0/text")
                .and_then(|t| t.as_str())
                == Some("Hi")
        })
        .expect("content chunk");
    assert_eq!(content["candidates"][0]["content"]["role"], "model");
    let end = jsons
        .iter()
        .find(|j| j.get("usageMetadata").is_some())
        .expect("usage chunk");
    assert_eq!(end["usageMetadata"]["promptTokenCount"], 5);
    assert_eq!(end["usageMetadata"]["candidatesTokenCount"], 2);
}

// ── stop-reason wire mappings ────────────────────────────────────────────────

fn emit_with_stop_reason(p: P, stop_reason: &str) -> String {
    sse_string(&format_stream(
        p,
        &[
            StreamDelta::MessageStart {
                id: "m".to_string(),
                model: "m".to_string(),
            },
            StreamDelta::TextDelta("hi".to_string()),
            StreamDelta::Done {
                stop_reason: stop_reason.to_string(),
            },
        ],
    ))
}

#[test]
fn openai_emitter_stop_reason_mappings() {
    // IR → OpenAI wire: identity (IR already speaks OpenAI vocabulary).
    let out = emit_with_stop_reason(P::OpenAiChat, "stop");
    assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
    let out = emit_with_stop_reason(P::OpenAiChat, "tool_calls");
    assert!(out.contains("\"finish_reason\":\"tool_calls\""), "{out}");
    let out = emit_with_stop_reason(P::OpenAiChat, "length");
    assert!(out.contains("\"finish_reason\":\"length\""), "{out}");
}

#[test]
fn anthropic_emitter_stop_reason_mappings() {
    let out = emit_with_stop_reason(P::AnthropicMessages, "stop");
    assert!(out.contains("\"stop_reason\":\"end_turn\""), "{out}");
    let out = emit_with_stop_reason(P::AnthropicMessages, "tool_calls");
    assert!(out.contains("\"stop_reason\":\"tool_use\""), "{out}");
    // nyro passes unrecognised reasons through verbatim; llm-bridge would
    // emit `max_tokens` for `length`.
    let out = emit_with_stop_reason(P::AnthropicMessages, "length");
    assert!(out.contains("\"stop_reason\":\"length\""), "{out}");
}

#[test]
fn google_emitter_stop_reason_mappings() {
    let out = emit_with_stop_reason(P::GoogleGemini, "stop");
    assert!(out.contains("\"finishReason\":\"STOP\""), "{out}");
    let out = emit_with_stop_reason(P::GoogleGemini, "length");
    assert!(out.contains("\"finishReason\":\"MAX_TOKENS\""), "{out}");
    // nyro maps only stop/length; IR `tool_calls` passes through verbatim
    // (llm-bridge maps the anthropic `tool_use` vocabulary to `STOP`).
    let out = emit_with_stop_reason(P::GoogleGemini, "tool_calls");
    assert!(out.contains("\"finishReason\":\"tool_calls\""), "{out}");
}

// ── round trips ──────────────────────────────────────────────────────────────

#[test]
fn round_trip_openai_to_anthropic_stream() {
    let deltas = parse_stream(
        P::OpenAiChat,
        "data: {\"id\":\"chatcmpl-rt\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\
         \ndata: {\"id\":\"chatcmpl-rt\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Round-trip test\"},\"finish_reason\":null}]}\n\
         \ndata: [DONE]",
    );

    let out = sse_string(&format_stream(P::AnthropicMessages, &deltas));
    for needle in [
        "event: message_start",
        "event: content_block_delta",
        "event: message_stop",
        "Round-trip test",
    ] {
        assert!(out.contains(needle), "missing {needle} in: {out}");
    }
}

#[test]
fn responses_stream_round_trip_parse_emit_parse() {
    let raw = "event: response.created\n\
         data: {\"response\":{\"id\":\"resp_rt\",\"model\":\"gpt-4o\"}}\n\
         \nevent: response.output_item.added\n\
         data: {\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"item_0\",\"role\":\"assistant\",\"content\":[]}}\n\
         \nevent: response.output_text.delta\n\
         data: {\"output_index\":0,\"content_index\":0,\"delta\":\"Round\"}\n\
         \nevent: response.output_text.delta\n\
         data: {\"output_index\":0,\"content_index\":0,\"delta\":\"-trip\"}\n\
         \nevent: response.output_text.delta\n\
         data: {\"output_index\":0,\"content_index\":0,\"delta\":\" test\"}\n\
         \nevent: response.completed\n\
         data: {\"response\":{\"id\":\"resp_rt\",\"status\":\"completed\",\"usage\":{\"input_tokens\":8,\"output_tokens\":3}}}";

    let deltas = parse_stream(P::OpenAiResponses, raw);
    assert_delta_eq(&deltas[0], &StreamDelta::MessageStart {
        id: "resp_rt".to_string(),
        model: "gpt-4o".to_string(),
    });
    assert_eq!(delta_text(&deltas), "Round-trip test");

    let re_emitted = sse_string(&format_stream(P::OpenAiResponses, &deltas));
    let reparsed = parse_stream(P::OpenAiResponses, &re_emitted);
    assert_eq!(delta_text(&reparsed), "Round-trip test");
    assert_delta_eq(&reparsed[0], &StreamDelta::MessageStart {
        id: "resp_rt".to_string(),
        model: "gpt-4o".to_string(),
    });
    assert!(
        reparsed.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 8 && u.completion_tokens == 3)),
        "usage survives: {}",
        delta_trace(&reparsed)
    );
}

#[test]
fn responses_tool_call_stream_round_trip() {
    let raw = "event: response.created\n\
         data: {\"response\":{\"id\":\"resp_tc\",\"model\":\"gpt-4o\"}}\n\
         \nevent: response.output_item.added\n\
         data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_rt\",\"name\":\"get_weather\"}}\n\
         \nevent: response.function_call_arguments.delta\n\
         data: {\"output_index\":0,\"delta\":\"{\\\"city\\\":\"}\n\
         \nevent: response.function_call_arguments.delta\n\
         data: {\"output_index\":0,\"delta\":\"\\\"NYC\\\"}\"}\n\
         \nevent: response.output_item.done\n\
         data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_rt\"}}\n\
         \nevent: response.completed\n\
         data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":15,\"output_tokens\":10}}}";

    let deltas = parse_stream(P::OpenAiResponses, raw);
    let trace = delta_trace(&deltas);
    assert!(matches!(
        &deltas[1],
        StreamDelta::ToolCallStart {
            id, name, ..
        } if id == "call_rt" && name == "get_weather"
    ), "{trace}");
    let joined: String = deltas
        .iter()
        .filter_map(|d| match d {
            StreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(joined, "{\"city\":\"NYC\"}", "{trace}");
    // KNOWN GAP: the Responses parser never emits `ToolCallComplete`
    // (see `responses_parser_function_call_events`).
    assert!(
        !deltas.iter().any(|d| matches!(d, StreamDelta::ToolCallComplete { .. })),
        "no ToolCallComplete from the Responses parser: {trace}"
    );

    let re_emitted = sse_string(&format_stream(P::OpenAiResponses, &deltas));
    assert!(re_emitted.contains("event: response.output_item.added"), "{re_emitted}");
    assert!(re_emitted.contains("\"arguments\":\"{\\\"city\\\":\\\"NYC\\\"}\""), "{re_emitted}");
}

// ── fix-verification Google stream cases ─────────────────────────────────────

#[test]
fn google_parser_usage_carried_forward_when_finish_chunk_has_no_usage() {
    // fix-verification "should use last usageMetadata when finishReason chunk
    // has no usageMetadata": the Usage delta from the earlier chunk survives
    // alongside the terminal Done.
    let chunks = [
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}],\"usageMetadata\":{\"promptTokenCount\":15,\"candidatesTokenCount\":3,\"totalTokenCount\":18}}",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" done\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}",
    ];
    let deltas = parse_stream_chunks(P::GoogleGemini, &chunks);

    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 15 && u.completion_tokens == 3)),
        "usage from earlier chunk: {}",
        delta_trace(&deltas)
    );
    assert_last_delta(
        &deltas,
        &StreamDelta::Done {
            stop_reason: "stop".to_string(),
        }
    );
}

#[test]
fn google_parser_no_message_end_on_intermediate_usage_chunks() {
    // fix-verification "should not emit message_end on intermediate chunks
    // with usageMetadata but no finishReason": Gemini 2.5-style streams carry
    // usage on every chunk; only the finishReason chunk may close the stream.
    let chunks = [
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1}}",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":8}}",
    ];
    let deltas = parse_stream_chunks(P::GoogleGemini, &chunks);

    let ends: Vec<_> = deltas
        .iter()
        .filter(|d| matches!(d, StreamDelta::Done { .. }))
        .collect();
    assert_eq!(ends.len(), 1, "exactly one Done: {}", delta_trace(&deltas));
    assert_eq!(delta_text(&deltas), "Hello world!");
    assert!(
        deltas.iter().any(|d| matches!(d, StreamDelta::Usage(u) if u.prompt_tokens == 10 && u.completion_tokens == 8)),
        "final usage captured: {}",
        delta_trace(&deltas)
    );
}
