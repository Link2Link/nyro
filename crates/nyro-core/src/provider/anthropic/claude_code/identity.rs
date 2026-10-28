//! Claude Code client identity: `metadata.user_id` and the per-conversation
//! session id.
//!
//! Claude Code 2.1.220+ sends `metadata.user_id` as a JSON *string*
//! `{"device_id":<64 lowercase hex>,"account_uuid":<uuid>,"session_id":<uuid>}`
//! and repeats the session id in `X-Claude-Code-Session-Id`. The identity has
//! to describe the selected OAuth account, so it is always rebuilt from the
//! credential (CLIProxyAPI `ApplyClaudeCredentialMetadata`).

use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::auth::types::StoredCredential;

use super::profile::header_str;

pub(crate) const SESSION_HEADER: &str = "x-claude-code-session-id";

/// RFC 4122 v4-shaped UUID from the first 16 bytes of `digest`.
fn uuid_from_digest(digest: &[u8]) -> String {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).hyphenated().to_string()
}

pub(crate) fn deterministic_uuid(domain: &str, seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(seed.as_bytes());
    uuid_from_digest(&hasher.finalize())
}

fn parse_uuid(value: &str) -> Option<String> {
    uuid::Uuid::parse_str(value.trim())
        .ok()
        .map(|parsed| parsed.hyphenated().to_string())
}

fn is_device_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `user_id` is the Claude Code 2.1.220+ JSON identity string.
pub(crate) fn is_valid_user_id(user_id: &str) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(user_id) else {
        return false;
    };
    let field = |key: &str| map.get(key).and_then(Value::as_str);
    field("device_id").is_some_and(is_device_id)
        && field("session_id").and_then(parse_uuid).is_some()
        && field("account_uuid").is_some_and(|v| v.is_empty() || parse_uuid(v).is_some())
}

/// Stable 64-hex device id for one upstream credential. Derived rather than
/// stored so token refreshes and restarts keep presenting the same device.
pub(crate) fn device_id(identity_seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"nyro-claude-code-device-v1\x00");
    hasher.update(identity_seed.as_bytes());
    hex::encode(hasher.finalize())
}

/// The OAuth account UUID: the token response's `account.uuid` (kept in
/// credential `meta`), else a deterministic UUID for the credential so the
/// identity is still well-formed and stable (CPA's setup-token fallback).
pub(crate) fn account_uuid(credential: Option<&StoredCredential>, identity_seed: &str) -> String {
    let from_meta = credential.and_then(|credential| {
        let meta = &credential.meta;
        [
            meta.pointer("/account/uuid"),
            meta.get("account_uuid"),
            meta.get("accountUuid"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(parse_uuid)
        .or_else(|| credential.subject_id.as_deref().and_then(parse_uuid))
    });
    from_meta.unwrap_or_else(|| deterministic_uuid("nyro-claude-code-account-v1", identity_seed))
}

fn first_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// The agent-conversation session UUID (`ClaudeAgentSessionUUIDForRequest`).
///
/// A confirmed native client keeps its own `X-Claude-Code-Session-Id` /
/// `user_id.session_id`. Every other caller is mapped from generic session
/// headers, or else from a fingerprint of the conversation's opening turn,
/// which stays stable for the whole conversation.
pub(crate) fn session_uuid(headers: &HeaderMap, body: &Value, confirmed_native: bool) -> String {
    if confirmed_native {
        if let Some(id) = header_str(headers, SESSION_HEADER).and_then(parse_uuid) {
            return id;
        }
        if let Some(id) = body
            .pointer("/metadata/user_id")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|v| {
                v.get("session_id")
                    .and_then(Value::as_str)
                    .and_then(parse_uuid)
            })
        {
            return id;
        }
    }
    for name in ["session-id", "x-session-id", "x-session-affinity"] {
        if let Some(value) = header_str(headers, name).filter(|v| !v.is_empty()) {
            return parse_uuid(value).unwrap_or_else(|| {
                deterministic_uuid("nyro-claude-code-session-v1:header", value)
            });
        }
    }
    let first_user = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .map(|m| first_text(m.get("content")))
        .unwrap_or_default();
    if first_user.trim().is_empty() {
        return uuid::Uuid::new_v4().hyphenated().to_string();
    }
    let system = first_text(body.get("system"));
    let seed = format!("sys:{system}\x00usr:{first_user}");
    deterministic_uuid("nyro-claude-code-session-v1:conversation", &seed)
}

/// Rebuild `metadata.user_id` for the selected credential. Extra keys a
/// native client put in its identity (e.g. `parent_session_id`) survive.
pub(crate) fn apply_user_id(
    body: &mut Value,
    device_id: &str,
    account_uuid: &str,
    session_id: &str,
    keep_extras: bool,
) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let existing: Option<Map<String, Value>> = obj
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|v| v.as_object().cloned());

    // Claude Code's key order: device_id, account_uuid, session_id, extras.
    let mut user_id = String::from("{");
    let mut push = |key: &str, value: &Value| {
        if user_id.len() > 1 {
            user_id.push(',');
        }
        user_id.push_str(&serde_json::to_string(key).unwrap_or_default());
        user_id.push(':');
        user_id.push_str(&serde_json::to_string(value).unwrap_or_default());
    };
    push("device_id", &Value::String(device_id.to_string()));
    push("account_uuid", &Value::String(account_uuid.to_string()));
    push("session_id", &Value::String(session_id.to_string()));
    if keep_extras && let Some(existing) = existing {
        for (key, value) in &existing {
            if !matches!(key.as_str(), "device_id" | "account_uuid" | "session_id") {
                push(key, value);
            }
        }
    }
    user_id.push('}');

    let metadata = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(Map::new());
    }
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert("user_id".to_string(), Value::String(user_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    #[test]
    fn user_id_shape_validation() {
        let good = json!({"device_id": "a".repeat(64), "account_uuid": "", "session_id": "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d"}).to_string();
        assert!(is_valid_user_id(&good));
        let bad_device = json!({"device_id": "A".repeat(64), "account_uuid": "", "session_id": "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d"}).to_string();
        assert!(!is_valid_user_id(&bad_device));
        assert!(!is_valid_user_id("user_abc_account__session_x"));
    }

    #[test]
    fn account_uuid_prefers_token_metadata() {
        let credential = StoredCredential {
            meta: json!({"account": {"uuid": "1B4E28BA-2FA1-11D2-883F-0016D3CCA427"}}),
            ..Default::default()
        };
        assert_eq!(
            account_uuid(Some(&credential), "seed"),
            "1b4e28ba-2fa1-11d2-883f-0016d3cca427"
        );
        let fallback = account_uuid(None, "seed");
        assert_eq!(fallback, account_uuid(None, "seed"));
        assert_ne!(fallback, account_uuid(None, "other"));
        assert!(parse_uuid(&fallback).is_some());
    }

    #[test]
    fn session_uuid_is_stable_across_turns_for_non_native_callers() {
        let turn1 =
            json!({"system": "be brief", "messages": [{"role": "user", "content": "hello"}]});
        let turn2 = json!({"system": "be brief", "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "next"}
        ]});
        let headers = HeaderMap::new();
        assert_eq!(
            session_uuid(&headers, &turn1, false),
            session_uuid(&headers, &turn2, false)
        );
        let other = json!({"messages": [{"role": "user", "content": "different"}]});
        assert_ne!(
            session_uuid(&headers, &turn1, false),
            session_uuid(&headers, &other, false)
        );
    }

    #[test]
    fn native_session_header_wins_only_for_confirmed_callers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            SESSION_HEADER,
            HeaderValue::from_static("8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d"),
        );
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(
            session_uuid(&headers, &body, true),
            "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d"
        );
        assert_ne!(
            session_uuid(&headers, &body, false),
            "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d"
        );
    }

    #[test]
    fn user_id_is_rebuilt_in_claude_code_order() {
        let mut body = json!({"metadata": {"user_id": json!({"session_id": "x", "parent_session_id": "p"}).to_string()}});
        apply_user_id(&mut body, &"b".repeat(64), "acc", "sess", true);
        assert_eq!(
            body["metadata"]["user_id"],
            format!(
                r#"{{"device_id":"{}","account_uuid":"acc","session_id":"sess","parent_session_id":"p"}}"#,
                "b".repeat(64)
            )
        );
        // A foreign plain-string user_id is replaced, extras are not carried.
        let mut plain = json!({"metadata": {"user_id": "openai-user-7"}});
        let session = "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d";
        let account = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        apply_user_id(&mut plain, &device_id("seed"), account, session, false);
        let user_id = plain["metadata"]["user_id"].as_str().unwrap();
        assert!(is_valid_user_id(user_id), "{user_id}");
        assert!(user_id.starts_with(r#"{"device_id":"#));
    }
}
