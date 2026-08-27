//! Helper-model caption client for the vision shim.
//!
//! Calls an OpenAI-compatible `/chat/completions` endpoint on the helper
//! provider with one image plus the caption prompt, and returns the helper's
//! description. This is a direct provider call (not a recursive dispatch), so
//! caption traffic never re-enters the proxy pipeline.

use std::time::Duration;

use serde_json::Value;

use crate::Gateway;
use crate::db::models::Provider;
use crate::protocol::ir::MediaSource;
use crate::provider::common::openai::openai_build_url;

use super::config::VisionShimConfig;

/// Outcome of one helper caption call.
pub(crate) struct CaptionCall {
    pub text: Option<String>,
    pub error: Option<String>,
    pub usage_total_tokens: Option<u64>,
}

impl CaptionCall {
    fn failed(error: String) -> Self {
        Self {
            text: None,
            error: Some(error),
            usage_total_tokens: None,
        }
    }
}

/// Transcribe one image via the helper model.
pub(crate) async fn caption_image(
    gw: &Gateway,
    provider: &Provider,
    cfg: &VisionShimConfig,
    source: &MediaSource,
    prompt: &str,
    timeout: Duration,
) -> CaptionCall {
    let image_url = match image_wire_url(source) {
        Ok(url) => url,
        Err(err) => return CaptionCall::failed(err),
    };
    let endpoint = openai_build_url(&provider.base_url, "/v1/chat/completions");
    let client = match gw.http_client_for_provider(provider.use_proxy).await {
        Ok(client) => client,
        Err(err) => return CaptionCall::failed(format!("helper http client: {err}")),
    };

    let body = serde_json::json!({
        "model": cfg.helper_model,
        "max_tokens": cfg.caption_max_tokens,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image_url", "image_url": { "url": image_url } },
                { "type": "text", "text": prompt }
            ]
        }]
    });

    let mut builder = client.post(&endpoint).timeout(timeout);
    let api_key = provider.api_key.trim();
    if !api_key.is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let response = match builder.json(&body).send().await {
        Ok(response) => response,
        Err(err) => return CaptionCall::failed(format!("helper request failed: {err}")),
    };
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => return CaptionCall::failed(format!("helper body read failed: {err}")),
    };
    let parsed: Value = match serde_json::from_slice(&bytes) {
        Ok(parsed) => parsed,
        Err(err) => {
            return CaptionCall::failed(format!(
                "helper returned non-JSON body ({err}): {}",
                truncate(&String::from_utf8_lossy(&bytes), 200)
            ));
        }
    };

    if !status.is_success() {
        return CaptionCall::failed(format!(
            "helper status {status}: {}",
            truncate(&parsed.to_string(), 400)
        ));
    }

    let usage_total_tokens = parsed
        .pointer("/usage/total_tokens")
        .and_then(Value::as_u64);
    let text = extract_content_text(parsed.pointer("/choices/0/message/content"))
        .filter(|text| !text.trim().is_empty());
    match text {
        Some(text) => CaptionCall {
            text: Some(text),
            error: None,
            usage_total_tokens,
        },
        None => CaptionCall {
            text: None,
            error: Some("helper returned no caption text".to_string()),
            usage_total_tokens,
        },
    }
}

/// Convert a media source into the `image_url.url` wire value.
fn image_wire_url(source: &MediaSource) -> Result<String, String> {
    match source {
        MediaSource::Base64 { media_type, data } => Ok(format!("data:{media_type};base64,{data}")),
        MediaSource::Url(url) => Ok(url.clone()),
        MediaSource::FileId { file_id, .. } => Err(format!(
            "file-referenced image '{file_id}' cannot be sent to the helper model"
        )),
    }
}

/// Extract assistant text from an OpenAI `message.content` value (plain
/// string or content-part array).
fn extract_content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter(|part| {
                    part.get("type").and_then(Value::as_str) == Some("text")
                        || part.get("type").is_none()
                })
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &text[..idx]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_content_text_handles_string_and_parts() {
        assert_eq!(
            extract_content_text(Some(&json!("a caption"))).as_deref(),
            Some("a caption")
        );
        assert_eq!(
            extract_content_text(Some(&json!([
                { "type": "text", "text": "part one " },
                { "type": "text", "text": "part two" }
            ])))
            .as_deref(),
            Some("part one part two")
        );
        assert_eq!(extract_content_text(Some(&Value::Null)), None);
        assert_eq!(extract_content_text(None), None);
    }

    #[test]
    fn image_wire_url_maps_sources() {
        assert_eq!(
            image_wire_url(&MediaSource::Base64 {
                media_type: "image/png".to_string(),
                data: "QUJD".to_string(),
            })
            .as_deref(),
            Ok("data:image/png;base64,QUJD")
        );
        assert_eq!(
            image_wire_url(&MediaSource::Url("https://x/i.png".to_string())).as_deref(),
            Ok("https://x/i.png")
        );
        assert!(
            image_wire_url(&MediaSource::FileId {
                file_id: "f-1".to_string(),
                detail: None,
            })
            .is_err()
        );
    }
}
