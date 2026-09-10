//! Helper-model caption client for the vision shim.
//!
//! Calls an OpenAI-compatible `/chat/completions` endpoint with one image
//! plus the caption prompt, and returns the helper's description. Helpers are
//! selected like target models — any provider, any model — and multiple
//! backends fail over in order. Caption traffic is a direct provider call
//! (not a recursive dispatch), so it never re-enters the proxy pipeline.

use std::time::Duration;

use serde_json::Value;

use crate::Gateway;
use crate::db::models::Provider;
use crate::protocol::ir::MediaSource;
use crate::provider::common::openai::openai_build_url;

use super::config::HelperBackend;

/// Outcome of one helper caption call (or a failover chain of them).
pub(crate) struct CaptionCall {
    pub text: Option<String>,
    pub error: Option<String>,
    pub usage_total_tokens: Option<u64>,
    /// The helper backend model that produced `text`, when successful.
    pub used_model: Option<String>,
}

impl CaptionCall {
    fn failed(error: String) -> Self {
        Self {
            text: None,
            error: Some(error),
            usage_total_tokens: None,
            used_model: None,
        }
    }
}

/// Caption one image, trying every helper backend in order until one
/// succeeds. The returned error summarizes the whole chain on total failure.
pub(crate) async fn caption_with_failover(
    gw: &Gateway,
    backends: &[(HelperBackend, Provider)],
    thinking_override: Option<&str>,
    source: &MediaSource,
    prompt: &str,
    max_tokens: u32,
    timeout: Duration,
) -> CaptionCall {
    let mut errors: Vec<String> = Vec::new();
    for (helper, provider) in backends {
        // Explicit override applies to every backend; auto mode keeps a
        // light thinking pass ("low") for GLM-family helpers and leaves
        // other vendors' requests untouched.
        let thinking = thinking_override.map(str::to_string).or_else(|| {
            super::config::provider_is_glm_family(
                provider.vendor.as_deref().unwrap_or(""),
                &provider.base_url,
            )
            .then(|| "low".to_string())
        });
        let call = caption_image(
            gw,
            provider,
            &helper.model,
            thinking.as_deref(),
            source,
            prompt,
            max_tokens,
            timeout,
        )
        .await;
        if let Some(text) = call.text {
            return CaptionCall {
                text: Some(text),
                error: None,
                usage_total_tokens: call.usage_total_tokens,
                used_model: Some(helper.model.clone()),
            };
        }
        errors.push(format!(
            "{}@{}: {}",
            helper.model,
            helper.provider,
            call.error.unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    CaptionCall::failed(format!(
        "all helper backends failed — {}",
        errors.join("; ")
    ))
}

/// Transcribe one image via one helper backend.
///
/// `thinking` is the `thinking.type` value to inject (GLM-family hybrid
/// models): `"low"` keeps a light reasoning pass over the image, while the
/// budget (`max_tokens`) must still cover thinking plus the caption — an
/// unbounded chain of thought starves `message.content` to empty.
#[allow(clippy::too_many_arguments)]
async fn caption_image(
    gw: &Gateway,
    provider: &Provider,
    model: &str,
    thinking: Option<&str>,
    source: &MediaSource,
    prompt: &str,
    max_tokens: u32,
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

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image_url", "image_url": { "url": image_url } },
                { "type": "text", "text": prompt }
            ]
        }]
    });

    if let Some(thinking) = thinking.filter(|value| !value.trim().is_empty()) {
        body["thinking"] = serde_json::json!({ "type": thinking.trim() });
    }

    let mut builder = client.post(&endpoint).timeout(timeout);
    let api_key = provider.api_key.trim();
    if !api_key.is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    // This is a direct provider call (no dispatcher), so the channel-scoped
    // egress headers have to be added here: OpenCode Go rejects requests
    // without its routing identity, seeded per helper model because a caption
    // body carries no conversation to fingerprint.
    if let Some(session_id) = crate::provider::opencode_go::session::seeded_egress_session_id(
        provider,
        &format!("vision-shim:{model}"),
    ) {
        builder = builder.header(
            crate::provider::opencode_go::session::SESSION_HEADER,
            session_id,
        );
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
            used_model: None,
        },
        None => {
            // Hybrid-reasoning helpers can return an empty `content` when the
            // thinking chain consumed the output budget (finish_reason
            // "length") or put everything into `reasoning_content`.
            let finish = parsed
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            CaptionCall {
                text: None,
                error: Some(format!(
                    "helper returned no caption text (finish_reason={finish}; likely thinking consumed the max_tokens budget)"
                )),
                usage_total_tokens,
                used_model: None,
            }
        }
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
