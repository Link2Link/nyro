use std::pin::Pin;

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use serde_json::Value;
use thiserror::Error;

use crate::ported::error::ProxyError;
use crate::ported::handlers_compat::{chat_sse_to_response_value, responses_sse_to_response_value};
use crate::ported::providers::{
    streaming, streaming_codex_anthropic, streaming_codex_chat, streaming_gemini,
    streaming_responses, transform, transform_codex_anthropic, transform_codex_chat,
    transform_codex_responses_namespace, transform_codex_responses_xai_sanitize, transform_gemini,
    transform_responses,
};
use crate::ported::sse::{strip_sse_field, take_sse_block};
use crate::profile::{ConversionProfile, Direction, UpstreamFlavor};
use crate::session::{ConversionSession, SessionIdentity};
use crate::state::CompatState;
use crate::transport::{BodyKind, ResponseMetadata};

pub type CompatStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStartDecision {
    Pending,
    Ready,
    Failed(String),
}

#[derive(Debug, Error)]
pub enum CompatError {
    #[error("invalid JSON request: {0}")]
    InvalidRequestJson(String),
    #[error("invalid JSON response: {0}")]
    InvalidResponseJson(String),
    #[error(transparent)]
    Conversion(#[from] ProxyError),
    #[error("stream conversion failed: {0}")]
    Stream(String),
    #[error("streaming response requires a 2xx upstream status (got {0})")]
    StreamingHttpError(u16),
}

impl CompatError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidRequestJson(_) => 400,
            Self::InvalidResponseJson(_) | Self::Stream(_) => 422,
            Self::Conversion(error) => crate::ported::error::map_proxy_error_to_status(error),
            Self::StreamingHttpError(status) => *status,
        }
    }

    /// Client-history problems that no provider switch can fix. Mirrors
    /// cc-switch's catch-all `InvalidRequest` → NonRetryable bucket: failing
    /// over would only replay the same broken request at every provider.
    pub fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            Self::InvalidRequestJson(_)
                | Self::Conversion(crate::ported::error::ProxyError::InvalidRequest(_))
        )
    }
}

/// Request bytes plus all state required to invert the conversion.
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub body: Bytes,
    pub force_upstream_stream: bool,
    pub session: ConversionSession,
}

pub enum ResponseBody {
    Buffered(Bytes),
    Stream(CompatStream),
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buffered(body) => f
                .debug_tuple("Buffered")
                .field(&format_args!("{} bytes", body.len()))
                .finish(),
            Self::Stream(_) => f.write_str("Stream(<compat stream>)"),
        }
    }
}

#[derive(Debug)]
pub struct ConvertedResponse {
    pub metadata: ResponseMetadata,
    pub body: ResponseBody,
    /// Ordered usage JSON bytes for optional logging side-parse. This never
    /// exposes this crate's `serde_json::Value` type.
    pub usage: Option<Bytes>,
}

#[derive(Debug, Clone, Default)]
pub struct CompatEngine {
    state: CompatState,
}

impl CompatEngine {
    pub fn new(state: CompatState) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &CompatState {
        &self.state
    }

    pub fn apply_json_patch(&self, body: Bytes, patch: Bytes) -> Result<Bytes, CompatError> {
        let mut ordered: Value = serde_json::from_slice(&body)
            .map_err(|error| CompatError::InvalidRequestJson(error.to_string()))?;
        apply_wire_patch(&mut ordered, &patch)?;
        serde_json::to_vec(&ordered)
            .map(Bytes::from)
            .map_err(|error| CompatError::InvalidRequestJson(error.to_string()))
    }

    pub async fn prepare_request(
        &self,
        profile: ConversionProfile,
        body: Bytes,
        identity: SessionIdentity,
    ) -> Result<PreparedRequest, CompatError> {
        self.prepare_request_with_patch(profile, body, None, identity)
            .await
    }

    /// Prepare a request after applying an optional byte-encoded wire patch.
    ///
    /// The patch is a JSON array of `{op,path,value?}` records. `path` is an
    /// array of object keys; arrays are replaced as whole values. `op` is `set`
    /// or `remove`. This
    /// keeps Nyro request-hook mutations without exposing this crate's ordered
    /// `serde_json::Value` type or reserializing untouched client fields.
    pub async fn prepare_request_with_patch(
        &self,
        profile: ConversionProfile,
        body: Bytes,
        patch: Option<Bytes>,
        identity: SessionIdentity,
    ) -> Result<PreparedRequest, CompatError> {
        let mut ordered: Value = serde_json::from_slice(&body)
            .map_err(|error| CompatError::InvalidRequestJson(error.to_string()))?;
        if let Some(patch) = patch.as_deref() {
            apply_wire_patch(&mut ordered, patch)?;
        }
        if let Some(model) = profile.model.as_ref() {
            ordered["model"] = Value::String(model.clone());
        }

        let tool_context = transform_codex_chat::build_codex_tool_context_from_request(&ordered);
        let namespace_restore =
            transform_codex_responses_namespace::namespace_restore_map(&ordered);
        let gemini_schema_hints = transform_gemini::extract_anthropic_tool_schema_hints(&ordered);

        if matches!(profile.direction, Direction::CodexResponsesToChat) {
            self.state
                .codex_chat_history
                .enrich_request(&mut ordered)
                .await;
        }

        let converted = match profile.direction {
            Direction::AnthropicToChat => {
                let mut converted = transform::anthropic_to_openai_with_reasoning_content(
                    ordered,
                    profile.preserve_chat_reasoning_content,
                )?;
                transform::inject_openai_stream_include_usage(&mut converted);
                // cc-switch injects prompt_cache_key on the Chat path only when
                // the provider explicitly configured one; session-derived keys
                // are reserved for the Responses path.
                if let Some(cache_key) = profile.cache_key.as_deref() {
                    converted["prompt_cache_key"] = Value::String(cache_key.to_string());
                }
                converted
            }
            Direction::AnthropicToResponses => {
                // Explicit provider-configured key wins over the session key,
                // mirroring cc-switch's explicit > session precedence.
                let cache_key = profile
                    .cache_key
                    .clone()
                    .or_else(|| identity.prompt_cache_key().map(str::to_string));
                let mut converted = transform_responses::anthropic_to_responses(
                    ordered,
                    cache_key.as_deref(),
                    matches!(profile.upstream_flavor, UpstreamFlavor::CodexOAuthResponses),
                    profile.codex_fast_mode,
                )?;
                if matches!(profile.upstream_flavor, UpstreamFlavor::XaiStrictResponses) {
                    let marker = Value::String("reasoning.encrypted_content".to_string());
                    let include = converted
                        .as_object_mut()
                        .expect("responses transform always returns an object")
                        .entry("include")
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(include) = include.as_array_mut()
                        && !include.iter().any(|item| item == &marker)
                    {
                        include.push(marker);
                    }
                }
                converted
            }
            Direction::AnthropicToGemini => transform_gemini::anthropic_to_gemini_with_shadow(
                ordered,
                Some(self.state.gemini_shadow.as_ref()),
                profile.provider_id.as_deref(),
                identity.client_provided.then_some(identity.value.as_str()),
            )?,
            Direction::AnthropicToAnthropic => {
                if let Some(hints) = profile.anthropic_normalization.as_ref() {
                    crate::ported::providers::claude_compat::normalize_anthropic_messages_for_provider(
                        &mut ordered,
                        &hints.model,
                        &hints.base_url,
                        &hints.vendor,
                    );
                }
                ordered
            }
            Direction::CodexResponsesToChat => {
                let explicit_cache_key = ordered
                    .get("prompt_cache_key")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mut converted =
                    transform_codex_chat::responses_to_chat_completions_with_reasoning(
                        ordered,
                        profile.chat_reasoning.as_ref(),
                    )?;
                if profile.prompt_cache_key_supported
                    && let Some(cache_key) = explicit_cache_key
                        .or_else(|| profile.cache_key.clone())
                        .or_else(|| identity.prompt_cache_key().map(str::to_string))
                {
                    converted["prompt_cache_key"] = Value::String(cache_key);
                }
                converted
            }
            Direction::CodexResponsesToAnthropic => {
                let mut converted = transform_codex_anthropic::responses_request_to_anthropic(
                    ordered,
                    profile.codex_anthropic_default_max_tokens,
                )?;
                // cc-switch applies the Claude Code identity to the converted
                // Anthropic body (system may be a string from Codex
                // instructions; prepend normalizes it to an array).
                if profile.impersonate_claude_code {
                    crate::ported::providers::claude_compat::prepend_claude_code_system_prompt(
                        &mut converted,
                    );
                }
                converted
            }
            Direction::XaiResponsesNative => {
                transform_codex_responses_namespace::flatten_request_namespaces(&mut ordered)?;
                transform_codex_responses_xai_sanitize::sanitize_xai_responses_request(
                    &mut ordered,
                );
                ordered
            }
        };

        let force_upstream_stream = profile.force_upstream_stream();
        let session = ConversionSession::new(
            profile,
            identity,
            tool_context,
            namespace_restore,
            gemini_schema_hints,
        );
        let body = serde_json::to_vec(&converted)
            .map(Bytes::from)
            .map_err(|error| CompatError::InvalidRequestJson(error.to_string()))?;
        Ok(PreparedRequest {
            body,
            force_upstream_stream,
            session,
        })
    }

    pub async fn convert_buffered_response(
        &self,
        session: &ConversionSession,
        metadata: ResponseMetadata,
        body: Bytes,
    ) -> Result<ConvertedResponse, CompatError> {
        if !(200..300).contains(&metadata.status) {
            return self.convert_error_response(session, metadata, body);
        }
        if let Some(message) = detect_semantic_failure(session, &body) {
            return Err(CompatError::Conversion(ProxyError::TransformError(message)));
        }

        let body_kind = metadata.body_kind(&body);
        if matches!(session.profile.direction, Direction::XaiResponsesNative) {
            let rebuilt = match serde_json::from_slice::<Value>(&body) {
                Ok(mut upstream) => {
                    transform_codex_responses_namespace::restore_response_namespaces(
                        &mut upstream,
                        session.namespace_restore.as_ref(),
                    );
                    serde_json::to_vec(&upstream)
                }
                Err(_) => Ok(body.to_vec()),
            };
            let body = rebuilt
                .map(Bytes::from)
                .map_err(|error| CompatError::InvalidResponseJson(error.to_string()))?;
            return Ok(ConvertedResponse {
                metadata: metadata.rebuilt("application/json"),
                body: ResponseBody::Buffered(body),
                usage: None,
            });
        }
        let mut upstream = match body_kind {
            BodyKind::Sse => self.aggregate_sse(session, &body)?,
            BodyKind::Json => serde_json::from_slice(&body).map_err(|error| {
                CompatError::InvalidResponseJson(format!(
                    "{error} {}",
                    response_field_diagnostics(&metadata, &body)
                ))
            })?,
            BodyKind::Empty | BodyKind::Other => {
                return Err(CompatError::InvalidResponseJson(format!(
                    "upstream body is {:?}, not JSON/SSE {}",
                    body_kind,
                    response_field_diagnostics(&metadata, &body)
                )));
            }
        };

        let converted = match session.profile.direction {
            Direction::AnthropicToChat => transform::openai_to_anthropic(upstream)?,
            Direction::AnthropicToResponses => {
                transform_responses::responses_to_anthropic(upstream)?
            }
            Direction::AnthropicToGemini => {
                transform_gemini::gemini_to_anthropic_with_shadow_and_hints(
                    upstream,
                    Some(self.state.gemini_shadow.as_ref()),
                    session.profile.provider_id.as_deref(),
                    session
                        .identity
                        .client_provided
                        .then_some(session.identity.value.as_str()),
                    Some(session.gemini_schema_hints.as_ref()),
                )?
            }
            Direction::CodexResponsesToChat => {
                let response = transform_codex_chat::chat_completion_to_response_with_context(
                    upstream,
                    session.tool_context.as_ref(),
                )?;
                self.state
                    .codex_chat_history
                    .record_response(&response)
                    .await;
                response
            }
            Direction::CodexResponsesToAnthropic => {
                transform_codex_anthropic::anthropic_response_to_responses_with_context(
                    upstream,
                    session.tool_context.as_ref(),
                )?
            }
            Direction::AnthropicToAnthropic => upstream,
            Direction::XaiResponsesNative => {
                transform_codex_responses_namespace::restore_response_namespaces(
                    &mut upstream,
                    session.namespace_restore.as_ref(),
                );
                upstream
            }
        };

        let usage = usage_bytes(&converted);
        let body = serde_json::to_vec(&converted)
            .map(Bytes::from)
            .map_err(|error| CompatError::InvalidResponseJson(error.to_string()))?;
        Ok(ConvertedResponse {
            metadata: metadata.rebuilt("application/json"),
            body: ResponseBody::Buffered(body),
            usage,
        })
    }

    pub fn convert_error_response(
        &self,
        session: &ConversionSession,
        metadata: ResponseMetadata,
        body: Bytes,
    ) -> Result<ConvertedResponse, CompatError> {
        if matches!(session.profile.direction, Direction::CodexResponsesToChat) {
            let parsed = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
                let text = truncate_lossy(&body, 1024);
                Value::String(text)
            });
            let converted = transform_codex_chat::chat_error_to_response_error(Some(&parsed));
            let body = serde_json::to_vec(&converted)
                .map(Bytes::from)
                .map_err(|error| CompatError::InvalidResponseJson(error.to_string()))?;
            return Ok(ConvertedResponse {
                metadata: metadata.rebuilt("application/json"),
                body: ResponseBody::Buffered(body),
                usage: None,
            });
        }
        Ok(ConvertedResponse {
            metadata,
            body: ResponseBody::Buffered(body),
            usage: None,
        })
    }

    pub fn convert_stream_response<S, E>(
        &self,
        session: &ConversionSession,
        metadata: ResponseMetadata,
        stream: S,
    ) -> Result<ConvertedResponse, CompatError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: std::error::Error + Send + 'static,
    {
        if !(200..300).contains(&metadata.status) {
            return Err(CompatError::StreamingHttpError(metadata.status));
        }

        let stream: CompatStream = match session.profile.direction {
            Direction::AnthropicToChat => Box::pin(streaming::create_anthropic_sse_stream(stream)),
            Direction::AnthropicToResponses => {
                Box::pin(streaming_responses::create_anthropic_sse_stream_from_responses(stream))
            }
            Direction::AnthropicToGemini => {
                Box::pin(streaming_gemini::create_anthropic_sse_stream_from_gemini(
                    stream,
                    Some(self.state.gemini_shadow.clone()),
                    session.profile.provider_id.clone(),
                    session
                        .identity
                        .client_provided
                        .then(|| session.identity.value.clone()),
                    Some((*session.gemini_schema_hints).clone()),
                ))
            }
            Direction::CodexResponsesToChat => {
                let converted =
                    streaming_codex_chat::create_responses_sse_stream_from_chat_with_context(
                        stream,
                        (*session.tool_context).clone(),
                    );
                Box::pin(
                    crate::ported::providers::codex_chat_history::record_responses_sse_stream(
                        converted,
                        self.state.codex_chat_history.clone(),
                    ),
                )
            }
            Direction::CodexResponsesToAnthropic => Box::pin(
                streaming_codex_anthropic::create_responses_sse_stream_from_anthropic_with_context(
                    stream,
                    (*session.tool_context).clone(),
                ),
            ),
            Direction::AnthropicToAnthropic => {
                Box::pin(stream.map_err(|error| std::io::Error::other(error.to_string())))
            }
            Direction::XaiResponsesNative => Box::pin(
                transform_codex_responses_namespace::create_namespace_restore_sse_stream(
                    stream,
                    (*session.namespace_restore).clone(),
                ),
            ),
        };
        Ok(ConvertedResponse {
            metadata: metadata.rebuilt("text/event-stream"),
            body: ResponseBody::Stream(stream),
            usage: None,
        })
    }

    pub async fn convert_response_auto(
        &self,
        session: &ConversionSession,
        metadata: ResponseMetadata,
        body: Bytes,
    ) -> Result<ConvertedResponse, CompatError> {
        if session.profile.client_stream {
            let stream = futures::stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
            self.convert_stream_response(session, metadata, stream)
        } else {
            self.convert_buffered_response(session, metadata, body)
                .await
        }
    }

    pub fn inspect_stream_start(
        &self,
        session: &ConversionSession,
        buffered: &[u8],
    ) -> StreamStartDecision {
        if !matches!(session.profile.direction, Direction::AnthropicToResponses) {
            return if buffered.is_empty() {
                StreamStartDecision::Pending
            } else {
                StreamStartDecision::Ready
            };
        }

        inspect_responses_stream_start(buffered, false)
    }

    pub fn inspect_stream_end(
        &self,
        session: &ConversionSession,
        buffered: &[u8],
    ) -> StreamStartDecision {
        if !matches!(session.profile.direction, Direction::AnthropicToResponses) {
            return if buffered.is_empty() {
                StreamStartDecision::Failed(
                    "upstream stream ended before producing a first chunk".to_string(),
                )
            } else {
                StreamStartDecision::Ready
            };
        }

        match inspect_responses_stream_start(buffered, true) {
            StreamStartDecision::Pending => StreamStartDecision::Failed(
                "Responses stream ended before producing output or a terminal event".to_string(),
            ),
            decision => decision,
        }
    }

    fn aggregate_sse(
        &self,
        session: &ConversionSession,
        body: &[u8],
    ) -> Result<Value, CompatError> {
        let text = String::from_utf8_lossy(body);
        match session.profile.direction {
            Direction::AnthropicToChat | Direction::CodexResponsesToChat => {
                Ok(chat_sse_to_response_value(&text)?)
            }
            Direction::AnthropicToResponses => Ok(responses_sse_to_response_value(&text)?),
            Direction::CodexResponsesToAnthropic => Ok(
                transform_codex_anthropic::anthropic_sse_to_message_value(&text)?,
            ),
            Direction::AnthropicToAnthropic => Ok(
                transform_codex_anthropic::anthropic_sse_to_message_value(&text)?,
            ),
            Direction::AnthropicToGemini => Err(CompatError::InvalidResponseJson(
                "Gemini SSE cannot be aggregated by cc-switch's buffered path".to_string(),
            )),
            Direction::XaiResponsesNative => {
                let mut response = responses_sse_to_response_value(&text)?;
                transform_codex_responses_namespace::restore_response_namespaces(
                    &mut response,
                    session.namespace_restore.as_ref(),
                );
                Ok(response)
            }
        }
    }
}

pub fn detect_semantic_failure(session: &ConversionSession, body: &[u8]) -> Option<String> {
    match session.profile.direction {
        Direction::AnthropicToResponses => responses_error_envelope_message(body)
            .map(|message| format!("Responses upstream returned a 2xx failure: {message}")),
        Direction::CodexResponsesToAnthropic => codex_anthropic_error_envelope_message(body)
            .map(|message| format!("Anthropic upstream returned a 2xx error envelope: {message}")),
        _ => None,
    }
}

/// Build the client-facing error envelope for Codex-semantics clients, ported
/// from cc-switch's `codex_proxy_error_json`. Normalizes nonstandard upstream
/// error bodies (`base_resp`, raw HTML, ...) through the Chat error converter,
/// replaces upstream 413 HTML pages with an actionable upstream-size message,
/// and attaches provider/model/endpoint context. Product-specific tokens use
/// Nyro naming.
pub fn codex_client_error_json(
    provider_name: &str,
    model: &str,
    endpoint: &str,
    upstream_status: Option<u16>,
    upstream_body: Option<&[u8]>,
    local_cause: Option<&str>,
) -> Bytes {
    let mut body = match (upstream_status, upstream_body) {
        (Some(_), Some(body)) => {
            let parsed = serde_json::from_slice::<Value>(body)
                .unwrap_or_else(|_| Value::String(truncate_lossy(body, 2048)));
            crate::ported::providers::transform_codex_chat::chat_error_to_response_error(Some(
                &parsed,
            ))
        }
        _ => serde_json::json!({
            "error": {
                "message": local_cause.unwrap_or("local proxy failure"),
                "type": "proxy_error",
                "code": "nyro_proxy_error",
                "param": Value::Null,
            }
        }),
    };

    let Some(error_obj) = body.get_mut("error").and_then(Value::as_object_mut) else {
        return serde_json::to_vec(&body)
            .map(Bytes::from)
            .unwrap_or_default();
    };

    let message = if upstream_status == Some(413) {
        // 413 comes from the provider's gateway (typically nginx
        // client_max_body_size), not from this proxy. The upstream response is
        // usually a full nginx HTML page, worthless to the user; replace it
        // with an upstream-pointing, actionable message.
        format!(
            concat!(
                "Upstream provider rejected the request with HTTP 413 (Payload Too Large). ",
                "The request body exceeds the upstream gateway's size limit; this is the ",
                "provider's server-side limit, not a Nyro limit. ",
                "Provider: {provider}; model: {model}; endpoint: {endpoint}. ",
                "To recover, shrink the request: run /compact, remove large pasted logs or ",
                "inline images, or ask the provider to raise its request body limit ",
                "(e.g. nginx client_max_body_size)."
            ),
            provider = provider_name,
            model = model,
            endpoint = endpoint,
        )
    } else {
        let cause = error_obj
            .get("message")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| local_cause.unwrap_or("unknown failure").to_string());
        let status_fragment = upstream_status
            .map(|status| format!("; upstream_status: HTTP {status}"))
            .unwrap_or_default();
        format!(
            "Nyro local proxy failed while handling Codex endpoint {endpoint}. Provider: {provider_name}; model: {model}{status_fragment}; cause: {cause}"
        )
    };

    error_obj.insert(
        "message".to_string(),
        Value::String(compact_error_message(&message, 1800)),
    );

    if error_obj
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        error_obj.insert("type".to_string(), Value::String("proxy_error".to_string()));
    }
    if error_obj.get("code").map(Value::is_null).unwrap_or(true) && local_cause.is_some() {
        error_obj.insert(
            "code".to_string(),
            Value::String("nyro_forward_failed".to_string()),
        );
    }
    if !error_obj.contains_key("param") {
        error_obj.insert("param".to_string(), Value::Null);
    }
    error_obj.insert(
        "provider".to_string(),
        Value::String(provider_name.to_string()),
    );
    error_obj.insert("model".to_string(), Value::String(model.to_string()));
    // Only used for local routing; never reuse on endpoints whose query may
    // carry credentials.
    error_obj.insert("endpoint".to_string(), Value::String(endpoint.to_string()));
    if let Some(status) = upstream_status {
        error_obj.insert(
            "upstream_status".to_string(),
            Value::Number(serde_json::Number::from(status)),
        );
    }

    serde_json::to_vec(&body)
        .map(Bytes::from)
        .unwrap_or_default()
}

fn compact_error_message(message: &str, max_chars: usize) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }

    let truncated = normalized
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string();
    format!("{truncated}…(truncated)")
}

fn codex_anthropic_error_envelope_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") && value.get("error").is_none() {
        return None;
    }
    let error = value.get("error").unwrap_or(&value);
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    Some(format!("{error_type}: {message}"))
}

fn responses_error_envelope_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let status = value.get("status").and_then(Value::as_str);
    let has_error = value.get("error").is_some_and(|error| !error.is_null());
    if !matches!(status, Some("failed" | "cancelled")) && !has_error {
        return None;
    }

    let error = value.get("error").unwrap_or(&value);
    let error_type = error
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| error.get("code").and_then(Value::as_str))
        .unwrap_or_else(|| status.unwrap_or("error"));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(match status {
            Some("cancelled") => "response generation was cancelled",
            _ => "response generation failed",
        });
    Some(format!("{error_type}: {message}"))
}

fn inspect_responses_stream_start(buffered: &[u8], eof: bool) -> StreamStartDecision {
    let text = String::from_utf8_lossy(buffered);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'[')) {
        if serde_json::from_str::<Value>(trimmed).is_ok() {
            return match responses_error_envelope_message(trimmed.as_bytes()) {
                Some(message) => StreamStartDecision::Failed(format!(
                    "Responses upstream returned a 2xx failure: {message}"
                )),
                None => StreamStartDecision::Ready,
            };
        }
        return StreamStartDecision::Pending;
    }

    let mut blocks = text.into_owned();
    while let Some(block) = take_sse_block(&mut blocks) {
        match inspect_responses_start_event(&block) {
            StreamStartDecision::Pending => {}
            decision => return decision,
        }
    }
    if eof && !blocks.trim().is_empty() {
        return inspect_responses_start_event(blocks.trim());
    }
    StreamStartDecision::Pending
}

fn inspect_responses_start_event(block: &str) -> StreamStartDecision {
    let mut named_event = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(event) = strip_sse_field(line, "event") {
            named_event = Some(event.trim().to_string());
        } else if let Some(data) = strip_sse_field(line, "data") {
            data_lines.push(data);
        }
    }
    if data_lines.is_empty() {
        return StreamStartDecision::Pending;
    }
    let value: Value = match serde_json::from_str(&data_lines.join("\n")) {
        Ok(value) => value,
        Err(_) => return StreamStartDecision::Pending,
    };
    let event = named_event
        .as_deref()
        .filter(|event| !event.is_empty())
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");

    let response = value.get("response").unwrap_or(&value);
    if matches!(
        response.get("status").and_then(Value::as_str),
        Some("failed" | "cancelled")
    ) || response.get("error").is_some_and(|error| !error.is_null())
    {
        return StreamStartDecision::Failed(responses_stream_error_message(
            response,
            "Responses upstream failed before output",
        ));
    }

    match event {
        "response.failed" | "error" => StreamStartDecision::Failed(responses_stream_error_message(
            response,
            "Responses upstream emitted an error before output",
        )),
        "response.created" | "response.in_progress" | "response.queued" | "" => {
            StreamStartDecision::Pending
        }
        _ => StreamStartDecision::Ready,
    }
}

fn responses_stream_error_message(response: &Value, fallback: &str) -> String {
    let error = response.get("error").unwrap_or(response);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or(fallback);
    let error_type = error
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| error.get("code").and_then(Value::as_str))
        .or_else(|| response.get("status").and_then(Value::as_str))
        .unwrap_or("upstream_error");
    format!("Responses upstream {error_type}: {message}")
}

fn apply_wire_patch(target: &mut Value, patch: &[u8]) -> Result<(), CompatError> {
    let operations: Value = serde_json::from_slice(patch)
        .map_err(|error| CompatError::InvalidRequestJson(error.to_string()))?;
    let operations = operations.as_array().ok_or_else(|| {
        CompatError::InvalidRequestJson("wire patch must be a JSON array".to_string())
    })?;
    for operation in operations {
        let operation = operation.as_object().ok_or_else(|| {
            CompatError::InvalidRequestJson("wire patch entry must be an object".to_string())
        })?;
        let op = operation.get("op").and_then(Value::as_str).ok_or_else(|| {
            CompatError::InvalidRequestJson("wire patch entry is missing op".to_string())
        })?;
        let path = operation
            .get("path")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CompatError::InvalidRequestJson(
                    "wire patch entry path must be an array".to_string(),
                )
            })?;
        let path = path
            .iter()
            .map(|segment| {
                segment.as_str().ok_or_else(|| {
                    CompatError::InvalidRequestJson(
                        "wire patch path segments must be strings".to_string(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        match op {
            "set" => {
                let value = operation.get("value").cloned().ok_or_else(|| {
                    CompatError::InvalidRequestJson(
                        "wire patch set entry is missing value".to_string(),
                    )
                })?;
                set_patch_value(target, &path, value)?;
            }
            "remove" => remove_patch_value(target, &path)?,
            other => {
                return Err(CompatError::InvalidRequestJson(format!(
                    "unsupported wire patch op: {other}"
                )));
            }
        }
    }
    Ok(())
}

fn set_patch_value(target: &mut Value, path: &[&str], value: Value) -> Result<(), CompatError> {
    let Some((last, parents)) = path.split_last() else {
        *target = value;
        return Ok(());
    };
    let parent = patch_parent_mut(target, parents)?;
    let object = parent.as_object_mut().ok_or_else(|| {
        CompatError::InvalidRequestJson("wire patch parent is not an object".to_string())
    })?;
    object.insert((*last).to_string(), value);
    Ok(())
}

fn remove_patch_value(target: &mut Value, path: &[&str]) -> Result<(), CompatError> {
    let Some((last, parents)) = path.split_last() else {
        *target = Value::Null;
        return Ok(());
    };
    let parent = patch_parent_mut(target, parents)?;
    let object = parent.as_object_mut().ok_or_else(|| {
        CompatError::InvalidRequestJson("wire patch parent is not an object".to_string())
    })?;
    object.remove(*last);
    Ok(())
}

fn patch_parent_mut<'a>(
    mut target: &'a mut Value,
    path: &[&str],
) -> Result<&'a mut Value, CompatError> {
    for segment in path {
        let object = target.as_object_mut().ok_or_else(|| {
            CompatError::InvalidRequestJson("wire patch parent is not an object".to_string())
        })?;
        target = object.get_mut(*segment).ok_or_else(|| {
            CompatError::InvalidRequestJson(format!(
                "wire patch parent path does not exist: {segment}"
            ))
        })?;
    }
    Ok(target)
}

fn usage_bytes(value: &Value) -> Option<Bytes> {
    let usage = value.get("usage")?;
    serde_json::to_vec(usage).ok().map(Bytes::from)
}

/// Field diagnostics for a failed upstream parse: content-type, encoding,
/// length and a safe body classification — the content itself is never
/// included, matching cc-switch's `upstream_body_parse_error`.
fn response_field_diagnostics(metadata: &ResponseMetadata, body: &[u8]) -> String {
    let encoding = metadata
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-encoding"))
        .and_then(|header| std::str::from_utf8(&header.value).ok());
    let text = String::from_utf8_lossy(body);
    crate::transport::body_diagnostics_suffix(metadata.content_type.as_deref(), encoding, &text)
}

fn truncate_lossy(bytes: &[u8], max_bytes: usize) -> String {
    let lossy = String::from_utf8_lossy(bytes);
    if lossy.len() <= max_bytes {
        return lossy.into_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !lossy.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &lossy[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ConversionProfile;
    use crate::session::SessionIdentity;

    #[tokio::test]
    async fn facade_round_trips_anthropic_chat_buffered() {
        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(false),
                Bytes::from_static(
                    br#"{"model":"gpt-4","max_tokens":128,"messages":[{"role":"user","content":"hello"}]}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        assert!(
            prepared
                .body
                .windows(10)
                .any(|window| window == b"\"messages\"")
        );

        let response = engine
            .convert_buffered_response(
                &prepared.session,
                ResponseMetadata::new(200),
                Bytes::from_static(
                    br#"{"id":"chatcmpl_1","model":"gpt-4","choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
                ),
            )
            .await
            .unwrap();
        let ResponseBody::Buffered(body) = response.body else {
            panic!("expected buffered response")
        };
        assert!(body.windows(13).any(|window| window == b"\"stop_reason\""));
    }

    #[tokio::test]
    async fn codex_chat_stream_commits_completed_tool_history() {
        use futures::StreamExt;

        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request(
                ConversionProfile::codex_responses_to_chat(true),
                Bytes::from_static(
                    br#"{"model":"gpt-5","stream":true,"tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}],"input":"read it"}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let upstream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
            b"data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-5\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        ))]);
        let converted = engine
            .convert_stream_response(&prepared.session, ResponseMetadata::new(200), upstream)
            .unwrap();
        let ResponseBody::Stream(stream) = converted.body else {
            panic!("expected stream")
        };
        let output = stream.collect::<Vec<_>>().await;
        assert!(output.iter().all(Result::is_ok));
        assert_eq!(engine.state().codex_history_response_count().await, 1);
    }

    #[tokio::test]
    async fn failed_stream_status_cannot_commit_state() {
        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request(
                ConversionProfile::codex_responses_to_chat(true),
                Bytes::from_static(br#"{"model":"gpt-5","stream":true,"input":"hello"}"#),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let upstream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        let error = engine
            .convert_stream_response(&prepared.session, ResponseMetadata::new(500), upstream)
            .unwrap_err();
        assert!(matches!(error, CompatError::StreamingHttpError(500)));
        assert_eq!(engine.state().codex_history_response_count().await, 0);
    }

    #[tokio::test]
    async fn prompt_cache_key_prefers_explicit_key_then_real_session() {
        let engine = CompatEngine::default();
        let identity = crate::SessionIdentity {
            value: "codex_real_session_1234567890".to_string(),
            source: crate::SessionSource::Header,
            client_provided: true,
        };
        let prepared = engine
            .prepare_request(
                ConversionProfile::codex_responses_to_chat(false)
                    .with_prompt_cache_key_support(true),
                Bytes::from_static(
                    br#"{"model":"gpt-5","input":"hello","prompt_cache_key":"explicit"}"#,
                ),
                identity.clone(),
            )
            .await
            .unwrap();
        assert!(
            prepared
                .body
                .windows(10)
                .any(|window| window == b"explicit\"}")
        );

        let prepared = engine
            .prepare_request(
                ConversionProfile::codex_responses_to_chat(false)
                    .with_prompt_cache_key_support(true),
                Bytes::from_static(br#"{"model":"gpt-5","input":"hello"}"#),
                identity,
            )
            .await
            .unwrap();
        assert!(
            prepared
                .body
                .windows("codex_real_session_1234567890".len())
                .any(|window| window == b"codex_real_session_1234567890")
        );
    }

    #[tokio::test]
    async fn wire_patch_preserves_unknown_fields_and_order() {
        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request_with_patch(
                ConversionProfile::xai_responses_native(false),
                Bytes::from_static(
                    br#"{ "unknown_before":1, "model":"grok-4", "metadata":{"keep":true,"remove":1}, "input":"old", "unknown_after":2 }"#,
                ),
                Some(Bytes::from_static(
                    br#"[{"op":"set","path":["input"],"value":"new"},{"op":"remove","path":["metadata","remove"]}]"#,
                )),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let body = String::from_utf8(prepared.body.to_vec()).unwrap();
        assert!(body.contains(r#""input":"new""#));
        assert!(body.contains(r#""metadata":{"keep":true}"#));
        assert!(!body.contains(r#""remove""#));
        assert!(body.find("unknown_before").unwrap() < body.find("model").unwrap());
        assert!(body.find("model").unwrap() < body.find("metadata").unwrap());
        assert!(body.find("metadata").unwrap() < body.find("input").unwrap());
        assert!(body.find("input").unwrap() < body.find("unknown_after").unwrap());
    }

    #[tokio::test]
    async fn prompt_cache_key_is_not_injected_without_real_session_or_support() {
        let engine = CompatEngine::default();
        for profile in [
            ConversionProfile::codex_responses_to_chat(false),
            ConversionProfile::codex_responses_to_chat(false).with_prompt_cache_key_support(true),
        ] {
            let prepared = engine
                .prepare_request(
                    profile,
                    Bytes::from_static(br#"{"model":"gpt-5","input":"hello"}"#),
                    SessionIdentity::generated("generated-not-cacheable"),
                )
                .await
                .unwrap();
            assert!(
                !prepared
                    .body
                    .windows(16)
                    .any(|window| window == b"prompt_cache_key")
            );
        }
    }

    #[tokio::test]
    async fn gemini_stream_state_commits_when_consumed() {
        use futures::StreamExt;

        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request(
                ConversionProfile::anthropic_to_gemini(true).with_provider_id("provider-a"),
                Bytes::from_static(
                    br#"{"model":"gemini-2.5-pro","messages":[{"role":"user","content":"hello"}]}"#,
                ),
                crate::SessionIdentity {
                    value: "session-a".to_string(),
                    source: crate::SessionSource::Header,
                    client_provided: true,
                },
            )
            .await
            .unwrap();
        let upstream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
            b"data: {\"responseId\":\"resp_1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"done\"}]}}]}\n\n",
        ))]);
        let converted = engine
            .convert_stream_response(&prepared.session, ResponseMetadata::new(200), upstream)
            .unwrap();
        let ResponseBody::Stream(stream) = converted.body else {
            panic!("expected stream")
        };
        assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
        assert_eq!(engine.state().gemini_session_count(), 1);
    }

    #[tokio::test]
    async fn gemini_stream_transport_error_does_not_commit_state() {
        use futures::StreamExt;

        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request(
                ConversionProfile::anthropic_to_gemini(true).with_provider_id("provider-a"),
                Bytes::from_static(
                    br#"{"model":"gemini-2.5-pro","messages":[{"role":"user","content":"hello"}]}"#,
                ),
                crate::SessionIdentity {
                    value: "session-a".to_string(),
                    source: crate::SessionSource::Header,
                    client_provided: true,
                },
            )
            .await
            .unwrap();
        let upstream = futures::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"responseId\":\"resp_1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            )),
            Err(std::io::Error::other("upstream stream failed")),
        ]);
        let converted = engine
            .convert_stream_response(&prepared.session, ResponseMetadata::new(200), upstream)
            .unwrap();
        let ResponseBody::Stream(stream) = converted.body else {
            panic!("expected stream")
        };
        let output = stream.collect::<Vec<_>>().await;
        assert!(output.iter().any(Result::is_err));
        assert_eq!(engine.state().gemini_session_count(), 0);
    }

    fn client_session(value: &str) -> crate::SessionIdentity {
        crate::SessionIdentity {
            value: value.to_string(),
            source: crate::SessionSource::Header,
            client_provided: true,
        }
    }

    async fn prepared_body(
        profile: ConversionProfile,
        body: &'static [u8],
        identity: crate::SessionIdentity,
    ) -> Bytes {
        CompatEngine::default()
            .prepare_request(profile, Bytes::from_static(body), identity)
            .await
            .unwrap()
            .body
    }

    #[tokio::test]
    async fn claude_transform_for_responses_uses_session_cache_key() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::StandardResponses,
            ),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            client_session("claude-session-123"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["prompt_cache_key"], "claude-session-123");
    }

    #[tokio::test]
    async fn claude_transform_for_responses_without_session_omits_cache_key() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::StandardResponses,
            ),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("prompt_cache_key").is_none());
    }

    #[tokio::test]
    async fn claude_transform_for_codex_oauth_keeps_explicit_cache_key() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::CodexOAuthResponses,
            )
            .with_cache_key("explicit-cache-key"),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            client_session("session-123"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["prompt_cache_key"], "explicit-cache-key");
    }

    #[tokio::test]
    async fn claude_transform_for_codex_oauth_uses_session_cache_key() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::CodexOAuthResponses,
            ),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            client_session("session-123"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["prompt_cache_key"], "session-123");
    }

    #[tokio::test]
    async fn claude_transform_for_codex_oauth_without_session_omits_cache_key() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::CodexOAuthResponses,
            ),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("prompt_cache_key").is_none());
    }

    #[tokio::test]
    async fn claude_transform_for_api_format_responses_shape() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::StandardResponses,
            ),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["model"], "gpt-5.4");
        assert!(value.get("input").is_some());
        assert!(value.get("max_output_tokens").is_some());
    }

    #[tokio::test]
    async fn claude_transform_for_codex_oauth_fast_mode_off() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_responses(
                false,
                crate::UpstreamFlavor::CodexOAuthResponses,
            ),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["store"], false);
        assert!(value.get("service_tier").is_none());
        assert_eq!(
            value["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
    }

    #[tokio::test]
    async fn claude_transform_for_api_format_gemini_native() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_gemini(false),
            br#"{"model":"gemini-2.5-pro","system":"You are helpful.","messages":[{"role":"user","content":"hello"}],"max_tokens":64}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("contents").is_some());
        assert_eq!(
            value["systemInstruction"]["parts"][0]["text"],
            "You are helpful."
        );
        assert_eq!(value["generationConfig"]["maxOutputTokens"], 64);
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_keeps_explicit_prompt_cache_key() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_chat(false)
                .with_cache_key("claude-cache-route"),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":64}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["prompt_cache_key"], "claude-cache-route");
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_skips_prompt_cache_key_by_default() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_chat(false),
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"}],"max_tokens":64}"#,
            client_session("session-123"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        // Session keys are Responses-only on the Chat path; without an
        // explicit provider key nothing is injected.
        assert!(value.get("prompt_cache_key").is_none());
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_streaming_injects_include_usage() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_chat(true),
            br#"{"model":"moonshotai/kimi-k2","messages":[{"role":"user","content":"hello"}],"max_tokens":128,"stream":true}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["stream"], true);
        assert_eq!(value["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_non_streaming_omits_stream_options() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_chat(false),
            br#"{"model":"moonshotai/kimi-k2","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("stream_options").is_none());
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_preserves_reasoning_content_for_deepseek() {
        let body = prepared_body(
            ConversionProfile {
                preserve_chat_reasoning_content: true,
                ..ConversionProfile::anthropic_to_chat(false)
            },
            br#"{"model":"deepseek-v4-flash","max_tokens":64,"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"I should call the tool."},{"type":"tool_use","id":"call_123","name":"get_weather","input":{"location":"Tokyo"}}]}]}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let msg = &value["messages"][0];
        assert_eq!(msg["reasoning_content"], "I should call the tool.");
        assert!(msg.get("tool_calls").is_some());
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_skips_reasoning_content_for_generic_provider() {
        let body = prepared_body(
            ConversionProfile::anthropic_to_chat(false),
            br#"{"model":"gpt-5.4","max_tokens":64,"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"I should call the tool."},{"type":"tool_use","id":"call_123","name":"get_weather","input":{"location":"Tokyo"}}]}]}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let msg = &value["messages"][0];
        assert!(msg.get("tool_calls").is_some());
        assert!(msg.get("reasoning_content").is_none());
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_preserves_reasoning_content_for_mimo() {
        let body = prepared_body(
            ConversionProfile {
                preserve_chat_reasoning_content: true,
                ..ConversionProfile::anthropic_to_chat(false)
            },
            br#"{"model":"mimo-v2.5-pro","max_tokens":64,"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"I should call the tool."},{"type":"tool_use","id":"call_123","name":"get_weather","input":{"location":"Tokyo"}}]}]}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let msg = &value["messages"][0];
        assert_eq!(msg["reasoning_content"], "I should call the tool.");
        assert!(msg.get("tool_calls").is_some());
    }

    #[tokio::test]
    async fn claude_transform_openai_chat_skips_reasoning_content_for_kimi() {
        // Kimi/Moonshot are outside REASONING_VENDOR_HINTS (2026-08 feedback),
        // so Kimi behaves like a generic provider and never sees
        // reasoning_content injected.
        let body = prepared_body(
            ConversionProfile {
                preserve_chat_reasoning_content: false,
                ..ConversionProfile::anthropic_to_chat(false)
            },
            br#"{"model":"kimi-k2.6","max_tokens":64,"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"I should call the tool."},{"type":"tool_use","id":"call_123","name":"get_weather","input":{"location":"Tokyo"}}]}]}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let msg = &value["messages"][0];
        assert!(msg.get("tool_calls").is_some());
        assert!(msg.get("reasoning_content").is_none());
    }

    #[tokio::test]
    async fn anthropic_passthrough_normalization_applies_deepseek_fixups() {
        let body = prepared_body(
            ConversionProfile::anthropic_passthrough_normalized(false)
                .with_anthropic_normalization(
                    "deepseek-v4-pro",
                    "https://api.deepseek.com/anthropic",
                    "",
                ),
            br#"{"model":"deepseek-v4-pro","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_123","name":"read_file","input":{"path":"README.md"}}]}]}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["messages"][0]["content"][0]["type"], "thinking");
    }

    #[tokio::test]
    async fn anthropic_passthrough_response_is_returned_verbatim() {
        let engine = CompatEngine::default();
        let prepared = engine
            .prepare_request(
                ConversionProfile::anthropic_passthrough_normalized(false),
                Bytes::from_static(
                    br#"{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"hello"}]}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap();
        let upstream = Bytes::from_static(
            br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"model":"deepseek-v4-pro","usage":{"input_tokens":3,"output_tokens":2}}"#,
        );
        let converted = engine
            .convert_buffered_response(
                &prepared.session,
                ResponseMetadata::new(200),
                upstream.clone(),
            )
            .await
            .unwrap();
        let ResponseBody::Buffered(body) = converted.body else {
            panic!("expected buffered response")
        };
        assert_eq!(body.as_ref(), upstream);
        assert!(
            converted
                .usage
                .as_deref()
                .is_some_and(|usage| usage.windows(12).any(|w| w == b"input_tokens"))
        );
    }

    #[tokio::test]
    async fn codex_responses_to_anthropic_impersonation_prepends_claude_code_identity() {
        let body = prepared_body(
            ConversionProfile::codex_responses_to_anthropic(false)
                .with_impersonate_claude_code(),
            br#"{"model":"claude-sonnet-5","instructions":"You are a Codex agent.","input":[{"role":"user","content":"hello"}],"max_output_tokens":128}"#,
            SessionIdentity::generated("test"),
        )
        .await;
        let value: Value = serde_json::from_slice(&body).unwrap();
        let system = value["system"].as_array().unwrap();
        assert_eq!(
            system[0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(system.len(), 2);
    }

    #[tokio::test]
    async fn responses_2xx_failure_is_detected_for_failover() {
        let engine = CompatEngine::default();
        let session = engine
            .prepare_request(
                ConversionProfile::anthropic_to_responses(
                    false,
                    crate::UpstreamFlavor::StandardResponses,
                ),
                Bytes::from_static(
                    br#"{"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap()
            .session;
        assert_eq!(
            detect_semantic_failure(
                &session,
                br#"{"status":"failed","error":{"type":"server_error","message":"busy"},"output":[]}"#
            )
            .as_deref(),
            Some("Responses upstream returned a 2xx failure: server_error: busy")
        );
        assert_eq!(
            detect_semantic_failure(&session, br#"{"status":"cancelled","output":[]}"#).as_deref(),
            Some(
                "Responses upstream returned a 2xx failure: cancelled: response generation was cancelled"
            )
        );
        assert!(detect_semantic_failure(
            &session,
            br#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}"#
        )
        .is_none());
        assert!(
            detect_semantic_failure(
                &session,
                br#"{"status":"completed","error":null,"output":[]}"#
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn codex_anthropic_2xx_error_envelope_is_detected_for_failover() {
        let engine = CompatEngine::default();
        let session = engine
            .prepare_request(
                ConversionProfile::codex_responses_to_anthropic(false),
                Bytes::from_static(
                    br#"{"model":"claude-sonnet-5","input":[{"role":"user","content":"hi"}]}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap()
            .session;
        assert_eq!(
            detect_semantic_failure(
                &session,
                br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
            )
            .as_deref(),
            Some("Anthropic upstream returned a 2xx error envelope: overloaded_error: busy")
        );
        assert!(detect_semantic_failure(&session, br#"{"type":"message","content":[]}"#).is_none());
    }

    #[tokio::test]
    async fn responses_stream_start_accepts_unlabelled_whole_json() {
        let engine = CompatEngine::default();
        let session = engine
            .prepare_request(
                ConversionProfile::anthropic_to_responses(
                    true,
                    crate::UpstreamFlavor::StandardResponses,
                ),
                Bytes::from_static(
                    br#"{"model":"gpt-5","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap()
            .session;
        let whole = "{\n    \"status\": \"completed\",\n\n    \"output\": []\n}";
        assert!(matches!(
            engine.inspect_stream_start(&session, whole.as_bytes()),
            StreamStartDecision::Ready
        ));
        assert!(matches!(
            engine.inspect_stream_start(&session, br#"{"status":"completed""#),
            StreamStartDecision::Pending
        ));
        assert!(matches!(
            engine.inspect_stream_start(
                &session,
                br#"{"status":"failed","error":{"message":"backend unavailable"}}"#
            ),
            StreamStartDecision::Failed(message)
            if message.contains("backend unavailable")
        ));
    }

    #[tokio::test]
    async fn responses_stream_start_semantic_failure_is_detected() {
        let engine = CompatEngine::default();
        let session = engine
            .prepare_request(
                ConversionProfile::anthropic_to_responses(
                    true,
                    crate::UpstreamFlavor::StandardResponses,
                ),
                Bytes::from_static(
                    br#"{"model":"gpt-5","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap()
            .session;
        let created = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n"
        );
        assert!(matches!(
            engine.inspect_stream_start(&session, created.as_bytes()),
            StreamStartDecision::Pending
        ));
        let failed = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}}\n\n"
        );
        assert!(matches!(
            engine.inspect_stream_start(&session, failed.as_bytes()),
            StreamStartDecision::Failed(message)
            if message.contains("boom")
        ));
        let delta = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n"
        );
        assert!(matches!(
            engine.inspect_stream_start(&session, delta.as_bytes()),
            StreamStartDecision::Ready
        ));
    }

    #[tokio::test]
    async fn invalid_client_history_is_not_retryable() {
        let engine = CompatEngine::default();
        // Historical tool arguments that cannot parse fail on every provider
        // identically; the dispatcher must fail fast instead of failing over.
        let error = engine
            .prepare_request(
                ConversionProfile::codex_responses_to_anthropic(false),
                Bytes::from_static(
                    br#"{"model":"claude-sonnet-5","input":[{"type":"function_call","call_id":"c1","name":"t","arguments":"not json"}]} "#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap_err();
        assert!(error.is_invalid_request(), "{error}");
        assert_eq!(error.http_status(), 400);

        // Malformed request JSON is equally provider-independent.
        let error = engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(false),
                Bytes::from_static(br#"{"model": nope}"#),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap_err();
        assert!(error.is_invalid_request(), "{error}");
    }

    #[tokio::test]
    async fn upstream_body_parse_error_carries_field_diagnostics() {
        let engine = CompatEngine::default();
        let session = engine
            .prepare_request(
                ConversionProfile::anthropic_to_chat(false),
                Bytes::from_static(
                    br#"{"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}"#,
                ),
                SessionIdentity::generated("test"),
            )
            .await
            .unwrap()
            .session;
        let mut metadata = ResponseMetadata::new(200);
        metadata.content_type = Some("text/html".to_string());
        metadata.headers.push(crate::transport::Header::new(
            "content-encoding",
            Bytes::from_static(b"gzip"),
        ));
        let error = engine
            .convert_buffered_response(&session, metadata, Bytes::from_static(br#"<html></html>"#))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("content-type: text/html"), "{message}");
        assert!(message.contains("content-encoding: gzip"), "{message}");
        assert!(message.contains("body-kind: html"), "{message}");
    }
}
