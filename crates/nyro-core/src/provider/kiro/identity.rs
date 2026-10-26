//! Shared Kiro wire identity for authorization, discovery, usage and generation.
//! Version pairs follow the compatible IDE/streaming-client release observed by
//! kiro-manager-lite. This is protocol compatibility, not a TLS fingerprint emulator.
use std::sync::OnceLock;

use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};

pub(crate) const IDE_VERSION: &str = "0.12.333";
pub(crate) const SDK_VERSION: &str = "1.0.39";
// Compatibility metadata, not the version of a Node process running Nyro.
const COMPAT_NODE_VERSION: &str = "22.12.0";

fn device_hash(raw: &str) -> Option<String> {
    let normalized: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    (!normalized.is_empty())
        .then(|| hex::encode(Sha256::digest(normalized.to_lowercase().as_bytes())))
}

/// A host identity, never a profile ARN/client registration shared by many users.
/// Do not rotate it per request or token refresh. Containers should retain their
/// machine-id/hostname across restarts if they need a stable host identity.
fn machine_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        #[cfg(target_os = "linux")]
        for path in ["/var/lib/dbus/machine-id", "/etc/machine-id"] {
            if let Ok(raw) = std::fs::read_to_string(path)
                && let Some(id) = device_hash(&raw)
            {
                return id;
            }
        }
        // Portable, non-secret fallback; no subprocesses in request handling.
        // Unlike the old fixed fallback this separates ordinary deployments.
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .ok()
            .filter(|host| !host.trim().is_empty())
            .or_else(|| std::fs::read_to_string("/etc/hostname").ok());
        host.as_deref()
            .and_then(device_hash)
            .unwrap_or_else(|| hex::encode(Sha256::digest(uuid::Uuid::new_v4().as_bytes())))
    })
}

pub(crate) fn headers() -> HeaderMap {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "win32",
        _ => "linux",
    };
    let tag = format!("KiroIDE-{IDE_VERSION}-{}", machine_id());
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (
            "user-agent",
            format!(
                "aws-sdk-js/{SDK_VERSION} ua/2.1 os/{os} lang/js md/nodejs#{COMPAT_NODE_VERSION} api/codewhispererstreaming#{SDK_VERSION} m/E {tag}"
            ),
        ),
        (
            "x-amz-user-agent",
            format!("aws-sdk-js/{SDK_VERSION} {tag}"),
        ),
        ("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string()),
    ] {
        headers.insert(
            name,
            HeaderValue::from_str(&value).expect("bounded Kiro identity header"),
        );
    }
    headers.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=1"),
    );
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_but_invocations_are_unique() {
        let a = headers();
        let b = headers();
        for key in ["user-agent", "x-amz-user-agent"] {
            assert_eq!(a[key], b[key]);
            assert!(
                a[key]
                    .to_str()
                    .unwrap()
                    .contains(&format!("KiroIDE-{IDE_VERSION}-"))
            );
            assert!(
                a[key]
                    .to_str()
                    .unwrap()
                    .contains(&format!("aws-sdk-js/{SDK_VERSION}"))
            );
        }
        assert_ne!(a["amz-sdk-invocation-id"], b["amz-sdk-invocation-id"]);
        assert_eq!(machine_id().len(), 64);
        assert!(machine_id().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a["amz-sdk-request"], "attempt=1; max=1");
        assert!(!a.contains_key("authorization"));
    }

    #[test]
    fn device_ids_are_normalized_without_exposing_raw_identity() {
        assert_eq!(device_hash(" AbC\n "), device_hash("abc"));
        assert_ne!(device_hash("host-a"), device_hash("host-b"));
        assert_eq!(device_hash(" \n"), None);
    }
}
