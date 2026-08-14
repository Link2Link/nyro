use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use crate::ported::providers::transform_codex_chat::CodexToolContext;
use crate::ported::providers::transform_codex_responses_namespace::NamespacedName;
use crate::ported::providers::transform_gemini::AnthropicToolSchemaHints;
use crate::profile::ConversionProfile;
use crate::transport::Header;

/// Source of a stable conversation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    Header,
    MetadataUserId,
    MetadataSessionId,
    Generated,
}

/// Client-specific session extraction rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionClient {
    Anthropic,
    CodexResponses,
    GrokBuildResponses,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("invalid session request JSON: {0}")]
    InvalidJson(String),
}

/// Session identity carried from request preparation into response conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    pub value: String,
    pub source: SessionSource,
    pub client_provided: bool,
}

impl SessionIdentity {
    pub fn generated(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            source: SessionSource::Generated,
            client_provided: false,
        }
    }

    /// Only stable client-provided identities are eligible as prompt-cache keys.
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.client_provided.then_some(self.value.as_str())
    }
}

/// Extract the same stable session identity used by cc-switch while accepting
/// only raw body bytes and simple header entries at the public boundary.
pub fn extract_session_identity(
    client: SessionClient,
    headers: &[Header],
    body: &[u8],
) -> Result<SessionIdentity, SessionError> {
    let body: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| SessionError::InvalidJson(error.to_string()))?;

    if matches!(client, SessionClient::Anthropic) {
        for name in ["x-claude-code-session-id", "claude-code-session-id"] {
            if let Some(value) = header_value(headers, name).filter(|value| !value.is_empty()) {
                return Ok(SessionIdentity {
                    value: value.to_string(),
                    source: SessionSource::Header,
                    client_provided: true,
                });
            }
        }
        if let Some(identity) = extract_from_metadata(&body) {
            return Ok(identity);
        }
    } else {
        let (prefix, names): (&str, &[&str]) = match client {
            SessionClient::CodexResponses => ("codex", &["session_id", "x-session-id"]),
            SessionClient::GrokBuildResponses => {
                ("grokbuild", &["x-grok-conv-id", "x-grok-session-id"])
            }
            SessionClient::Anthropic => unreachable!(),
        };
        for name in names {
            if let Some(value) = header_value(headers, name).map(str::trim)
                && value.len() > 20
            {
                return Ok(SessionIdentity {
                    value: format!("{prefix}_{value}"),
                    source: SessionSource::Header,
                    client_provided: true,
                });
            }
        }
        if let Some(value) = body
            .get("metadata")
            .and_then(|metadata| metadata.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| value.len() > 10)
        {
            return Ok(SessionIdentity {
                value: format!("{prefix}_{value}"),
                source: SessionSource::MetadataSessionId,
                client_provided: true,
            });
        }
    }

    Ok(SessionIdentity {
        value: uuid::Uuid::new_v4().to_string(),
        source: SessionSource::Generated,
        client_provided: false,
    })
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .and_then(|header| std::str::from_utf8(&header.value).ok())
}

fn extract_from_metadata(body: &serde_json::Value) -> Option<SessionIdentity> {
    let metadata = body.get("metadata")?;
    if let Some(user_id) = metadata.get("user_id").and_then(serde_json::Value::as_str)
        && let Some(value) = parse_session_from_user_id(user_id)
    {
        return Some(SessionIdentity {
            value,
            source: SessionSource::MetadataUserId,
            client_provided: true,
        });
    }
    metadata
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| SessionIdentity {
            value: value.to_string(),
            source: SessionSource::MetadataSessionId,
            client_provided: true,
        })
}

fn parse_session_from_user_id(user_id: &str) -> Option<String> {
    user_id
        .find("_session_")
        .map(|position| &user_id[position + 9..])
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string)
}

/// Request-derived conversion state. Its internals intentionally stay private
/// so no ordered `serde_json::Value` crosses into `nyro-core`.
#[derive(Clone)]
pub struct ConversionSession {
    pub profile: ConversionProfile,
    pub identity: SessionIdentity,
    pub(crate) tool_context: Arc<CodexToolContext>,
    pub(crate) namespace_restore: Arc<HashMap<String, NamespacedName>>,
    pub(crate) gemini_schema_hints: Arc<AnthropicToolSchemaHints>,
}

impl std::fmt::Debug for ConversionSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionSession")
            .field("profile", &self.profile)
            .field("identity", &self.identity)
            .field("namespace_entries", &self.namespace_restore.len())
            .field("gemini_schema_hints", &self.gemini_schema_hints.len())
            .finish_non_exhaustive()
    }
}

impl ConversionSession {
    pub(crate) fn new(
        profile: ConversionProfile,
        identity: SessionIdentity,
        tool_context: CodexToolContext,
        namespace_restore: HashMap<String, NamespacedName>,
        gemini_schema_hints: AnthropicToolSchemaHints,
    ) -> Self {
        Self {
            profile,
            identity,
            tool_context: Arc::new(tool_context),
            namespace_restore: Arc::new(namespace_restore),
            gemini_schema_hints: Arc::new(gemini_schema_hints),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn header(name: &str, value: &str) -> Header {
        Header::new(name, Bytes::copy_from_slice(value.as_bytes()))
    }

    #[test]
    fn test_extract_session_from_claude_metadata_user_id() {
        let result = extract_session_identity(
            SessionClient::Anthropic,
            &[],
            br#"{"metadata":{"user_id":"user_john_doe_session_abc123def456"}}"#,
        )
        .unwrap();
        assert_eq!(result.value, "abc123def456");
        assert_eq!(result.source, SessionSource::MetadataUserId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_metadata_session_id() {
        let result = extract_session_identity(
            SessionClient::Anthropic,
            &[],
            br#"{"metadata":{"session_id":"my-session-123"}}"#,
        )
        .unwrap();
        assert_eq!(result.value, "my-session-123");
        assert_eq!(result.source, SessionSource::MetadataSessionId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header() {
        let result = extract_session_identity(
            SessionClient::Anthropic,
            &[header(
                "x-claude-code-session-id",
                "d937243f-2702-4f20-97b6-c9682235ab81",
            )],
            b"{}",
        )
        .unwrap();
        assert_eq!(result.value, "d937243f-2702-4f20-97b6-c9682235ab81");
        assert_eq!(result.source, SessionSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header_precedes_metadata() {
        let result = extract_session_identity(
            SessionClient::Anthropic,
            &[header("claude-code-session-id", "header-session-123")],
            br#"{"metadata":{"session_id":"my-session-123"}}"#,
        )
        .unwrap();
        assert_eq!(result.value, "header-session-123");
    }

    #[test]
    fn test_codex_previous_response_id_is_not_stable_session_identity() {
        let result = extract_session_identity(
            SessionClient::CodexResponses,
            &[],
            br#"{"previous_response_id":"resp_abc123def456789"}"#,
        )
        .unwrap();
        assert_eq!(result.source, SessionSource::Generated);
        assert!(!result.client_provided);
        assert_eq!(result.prompt_cache_key(), None);
    }

    #[test]
    fn test_codex_keeps_existing_response_session_headers() {
        for name in ["session_id", "x-session-id"] {
            let result = extract_session_identity(
                SessionClient::CodexResponses,
                &[header(name, "d937243f-2702-4f20-97b6-c9682235ab81")],
                b"{}",
            )
            .unwrap();
            assert_eq!(result.value, "codex_d937243f-2702-4f20-97b6-c9682235ab81");
            assert_eq!(result.source, SessionSource::Header);
            assert_eq!(result.prompt_cache_key(), Some(result.value.as_str()));
        }
    }

    #[test]
    fn test_grokbuild_prefers_conversation_header() {
        let result = extract_session_identity(
            SessionClient::GrokBuildResponses,
            &[
                header(
                    "x-grok-conv-id",
                    "conv-724f4275-584e-43af-ad46-b5e7509a3ca2",
                ),
                header(
                    "x-grok-session-id",
                    "session-d937243f-2702-4f20-97b6-c9682235ab81",
                ),
            ],
            b"{}",
        )
        .unwrap();
        assert_eq!(
            result.value,
            "grokbuild_conv-724f4275-584e-43af-ad46-b5e7509a3ca2"
        );
    }

    #[test]
    fn test_grokbuild_falls_back_to_session_header() {
        let result = extract_session_identity(
            SessionClient::GrokBuildResponses,
            &[
                header("x-grok-conv-id", ""),
                header(
                    "x-grok-session-id",
                    "session-d937243f-2702-4f20-97b6-c9682235ab81",
                ),
            ],
            b"{}",
        )
        .unwrap();
        assert_eq!(
            result.value,
            "grokbuild_session-d937243f-2702-4f20-97b6-c9682235ab81"
        );
    }

    #[test]
    fn test_grokbuild_ignores_request_and_codex_session_headers() {
        let result = extract_session_identity(
            SessionClient::GrokBuildResponses,
            &[
                header(
                    "x-grok-req-id",
                    "request-724f4275-584e-43af-ad46-b5e7509a3ca2",
                ),
                header("x-session-id", "codex-d937243f-2702-4f20-97b6-c9682235ab81"),
            ],
            b"{}",
        )
        .unwrap();
        assert_eq!(result.source, SessionSource::Generated);
    }

    #[test]
    fn test_extract_session_generates_new_when_not_found() {
        let result = extract_session_identity(SessionClient::Anthropic, &[], b"{}").unwrap();
        assert!(!result.value.is_empty());
        assert_eq!(result.source, SessionSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_parse_session_from_user_id() {
        assert_eq!(
            parse_session_from_user_id("user_john_session_abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_session_from_user_id("my_app_session_xyz789"),
            Some("xyz789".to_string())
        );
        assert_eq!(
            parse_session_from_user_id("no_session_marker"),
            Some("marker".to_string())
        );
        assert_eq!(parse_session_from_user_id("user_john_abc123"), None);
        assert_eq!(parse_session_from_user_id("_session_"), None);
    }
}
