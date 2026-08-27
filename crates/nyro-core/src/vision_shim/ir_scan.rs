//! IR scanning and rewriting for the vision shim.
//!
//! Two symmetric traversals over the unified IR request:
//! - `collect_images` walks every location an image can appear (message
//!   blocks, nested search results, and the raw JSON of tool results) and
//!   returns them in a stable order with content digests;
//! - `replace_images` walks the same locations in the same order and swaps
//!   each image for a "[Image n: ...]" text block using pre-computed captions.
//!
//! The traversal orders must stay identical so image numbering (and the
//! `max_images` cut) is consistent between the two passes.

use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::protocol::ir::{AiRequest, ContentBlock, MediaSource, MessageContent, Role};

/// Placeholder used when no caption is available (helper failure, unsupported
/// source, limit reached). The facade never drops an image silently.
pub(crate) const PLACEHOLDER_MARK: &str = "image content could not be processed";

/// One collected image occurrence, identified by content digest.
#[derive(Debug, Clone)]
pub(crate) struct ScannedImage {
    pub digest: [u8; 32],
    pub source: MediaSource,
    /// Approximate decoded size in bytes (base64 length x 3/4; URL length).
    pub approx_bytes: usize,
    /// Whether the image sits inside the latest user message (question-aware
    /// caption territory) or earlier in the history (generic captions).
    pub question_aware: bool,
}

/// SHA-256 digest plus a size estimate for one media source.
pub(crate) fn digest_media(source: &MediaSource) -> ([u8; 32], usize) {
    let mut hasher = Sha256::new();
    let approx_bytes = match source {
        MediaSource::Base64 { media_type, data } => {
            hasher.update(b"b64:");
            hasher.update(media_type.as_bytes());
            hasher.update(b":");
            hasher.update(data.as_bytes());
            data.len() / 4 * 3
        }
        MediaSource::Url(url) => {
            hasher.update(b"url:");
            hasher.update(url.as_bytes());
            url.len()
        }
        MediaSource::FileId { file_id, .. } => {
            hasher.update(b"file:");
            hasher.update(file_id.as_bytes());
            0
        }
    };
    (hasher.finalize().into(), approx_bytes)
}

/// Index of the last user message in the request, if any.
fn latest_user_message_index(request: &AiRequest) -> Option<usize> {
    request.messages.iter().rposition(|m| m.role == Role::User)
}

/// The latest user message's text, trimmed — the "question" fed to
/// question-aware captions.
pub(crate) fn latest_user_text(request: &AiRequest) -> Option<String> {
    let idx = latest_user_message_index(request)?;
    let text = request.messages[idx].content.to_text().trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Collect every image occurrence in stable traversal order.
pub(crate) fn collect_images(request: &AiRequest) -> Vec<ScannedImage> {
    let latest_user = latest_user_message_index(request);
    let mut out = Vec::new();
    for (idx, message) in request.messages.iter().enumerate() {
        let question_aware = latest_user.is_some_and(|latest| idx >= latest);
        if let MessageContent::Blocks(blocks) = &message.content {
            collect_blocks(blocks, question_aware, &mut out);
        }
    }
    out
}

fn collect_blocks(blocks: &[ContentBlock], question_aware: bool, out: &mut Vec<ScannedImage>) {
    for block in blocks {
        match block {
            ContentBlock::Image { source, .. } => {
                push_scanned(source, question_aware, out);
            }
            ContentBlock::ToolResult { content, .. }
            | ContentBlock::ServerToolResult { content, .. } => {
                collect_json(content, question_aware, out);
            }
            ContentBlock::SearchResult { content, .. } => {
                collect_blocks(content, question_aware, out);
            }
            _ => {}
        }
    }
}

fn collect_json(value: &Value, question_aware: bool, out: &mut Vec<ScannedImage>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_json(item, question_aware, out);
            }
        }
        Value::Object(map) => {
            if let Some(source) = json_image_source(map) {
                push_scanned(&source, question_aware, out);
                return;
            }
            for (_, nested) in map {
                collect_json(nested, question_aware, out);
            }
        }
        _ => {}
    }
}

fn push_scanned(source: &MediaSource, question_aware: bool, out: &mut Vec<ScannedImage>) {
    let (digest, approx_bytes) = digest_media(source);
    out.push(ScannedImage {
        digest,
        source: source.clone(),
        approx_bytes,
        question_aware,
    });
}

/// Replace every image occurrence with a numbered text block.
///
/// `captions` maps content digest to caption text (or placeholder text).
/// Returns the number of replacements performed.
pub(crate) fn replace_images(
    request: &mut AiRequest,
    captions: &HashMap<[u8; 32], String>,
) -> usize {
    let mut counter = 0usize;
    for message in request.messages.iter_mut() {
        if let MessageContent::Blocks(blocks) = &mut message.content {
            replace_blocks(blocks, captions, &mut counter);
        }
    }
    counter
}

fn replace_blocks(
    blocks: &mut [ContentBlock],
    captions: &HashMap<[u8; 32], String>,
    counter: &mut usize,
) {
    for block in blocks.iter_mut() {
        match block {
            ContentBlock::Image {
                source,
                cache_control,
            } => {
                let (digest, _) = digest_media(source);
                let replacement = replacement_text(&digest, captions, counter);
                *block = ContentBlock::Text {
                    text: replacement,
                    cache_control: cache_control.clone(),
                };
            }
            ContentBlock::ToolResult { content, .. }
            | ContentBlock::ServerToolResult { content, .. } => {
                replace_json(content, captions, counter);
            }
            ContentBlock::SearchResult { content, .. } => {
                replace_blocks(content, captions, counter);
            }
            _ => {}
        }
    }
}

fn replace_json(value: &mut Value, captions: &HashMap<[u8; 32], String>, counter: &mut usize) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                replace_json(item, captions, counter);
            }
        }
        Value::Object(map) => {
            if let Some(source) = json_image_source(map) {
                let (digest, _) = digest_media(&source);
                let replacement = replacement_text(&digest, captions, counter);
                *value = serde_json::json!({ "type": "text", "text": replacement });
                return;
            }
            for (_, nested) in map.iter_mut() {
                replace_json(nested, captions, counter);
            }
        }
        _ => {}
    }
}

fn replacement_text(
    digest: &[u8; 32],
    captions: &HashMap<[u8; 32], String>,
    counter: &mut usize,
) -> String {
    *counter += 1;
    let body = captions
        .get(digest)
        .cloned()
        .unwrap_or_else(|| format!("({PLACEHOLDER_MARK})"));
    format!("[Image {counter}: {body}]")
}

/// Recognise an image inside raw tool-result JSON.
///
/// Covers both wire shapes that reach tool results:
/// - Anthropic: `{"type":"image","source":{"type":"base64"|"url", ...}}`
/// - OpenAI:   `{"type":"image_url","image_url":{"url": ...}}`
fn json_image_source(map: &serde_json::Map<String, Value>) -> Option<MediaSource> {
    let block_type = map.get("type").and_then(Value::as_str)?;
    match block_type {
        "image" => {
            let source = map.get("source")?.as_object()?;
            match source.get("type").and_then(Value::as_str)? {
                "base64" => Some(MediaSource::Base64 {
                    media_type: source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png")
                        .to_string(),
                    data: source
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                "url" => Some(MediaSource::Url(
                    source
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )),
                _ => None,
            }
        }
        "image_url" => {
            let url = map.get("image_url")?.get("url").and_then(Value::as_str)?;
            Some(MediaSource::Url(url.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::cache::CacheControl;
    use crate::protocol::ir::{AiRequest, Message};
    use serde_json::json;

    fn image_block(data: &str, cache_control: Option<CacheControl>) -> ContentBlock {
        ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/png".to_string(),
                data: data.to_string(),
            },
            cache_control,
        }
    }

    #[test]
    fn collect_then_replace_covers_blocks_and_tool_json_in_order() {
        let request = AiRequest::new(
            "glm-5.3",
            vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "look at this".to_string(),
                            cache_control: None,
                        },
                        image_block("aGk=", Some(CacheControl::ephemeral())),
                    ]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "screenshot".to_string(),
                        input: json!({}),
                        cache_control: None,
                    }]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".to_string(),
                        content: json!([
                            { "type": "text", "text": "screen" },
                            {
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": "image/png",
                                    "data": "c2NyZWVu"
                                }
                            }
                        ]),
                        is_error: None,
                        cache_control: None,
                    }]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );

        let images = collect_images(&request);
        assert_eq!(images.len(), 2);
        assert!(
            !images[0].question_aware,
            "first message is history once a later user message exists"
        );
        assert!(
            images[1].question_aware,
            "tool result sits in the latest user message"
        );

        let mut captions = HashMap::new();
        captions.insert(images[0].digest, "a friendly picture".to_string());
        // images[1] intentionally left without a caption → placeholder fallback.

        let mut replaced = request;
        let count = replace_images(&mut replaced, &captions);
        assert_eq!(count, 2);

        let blocks = match &replaced.messages[0].content {
            MessageContent::Blocks(blocks) => blocks.clone(),
            other => panic!("expected blocks, got {other:?}"),
        };
        match &blocks[1] {
            ContentBlock::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "[Image 1: a friendly picture]");
                assert_eq!(cache_control.as_ref(), Some(&CacheControl::ephemeral()));
            }
            other => panic!("expected text block, got {other:?}"),
        }

        match &replaced.messages[2].content {
            MessageContent::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolResult { content, .. } => {
                    assert_eq!(
                        content,
                        &json!([
                            { "type": "text", "text": "screen" },
                            {
                                "type": "text",
                                "text": format!("[Image 2: ({PLACEHOLDER_MARK})]")
                            }
                        ])
                    );
                }
                other => panic!("expected tool result, got {other:?}"),
            },
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    #[test]
    fn identical_images_share_a_digest_but_count_twice() {
        let request = AiRequest::new(
            "m",
            vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    image_block("repeat", None),
                    image_block("repeat", None),
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let images = collect_images(&request);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].digest, images[1].digest);

        let mut captions = HashMap::new();
        captions.insert(images[0].digest, "same picture".to_string());
        let mut replaced = request;
        assert_eq!(replace_images(&mut replaced, &captions), 2);
    }

    #[test]
    fn history_images_are_not_question_aware() {
        let request = AiRequest::new(
            "m",
            vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![image_block("old", None)]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("sure".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Text("what color was it?".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );
        let images = collect_images(&request);
        assert_eq!(images.len(), 1);
        assert!(!images[0].question_aware);
        assert_eq!(
            latest_user_text(&request).as_deref(),
            Some("what color was it?")
        );
    }

    #[test]
    fn openai_shaped_tool_images_are_recognised() {
        let request = AiRequest::new(
            "m",
            vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t".to_string(),
                    content: json!([{ "type": "image_url", "image_url": { "url": "https://x/y.png" } }]),
                    is_error: None,
                    cache_control: None,
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let images = collect_images(&request);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].approx_bytes, "https://x/y.png".len());
    }
}
