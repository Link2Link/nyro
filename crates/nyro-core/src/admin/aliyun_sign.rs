//! Alibaba Cloud ACS3-HMAC-SHA256 request signing (ROA-style OpenAPI V3).
//!
//! Used to call the Model Studio control-plane OpenAPI
//! (`modelstudio.cn-beijing.aliyuncs.com`) — e.g. minting the Bailian CLI
//! access token for token-plan usage queries — which requires account-level
//! RAM AccessKeyId/SecretKey signing. The inference API key is NOT accepted
//! there.
//!
//! Algorithm (per Alibaba Cloud API signing docs, ACS3-HMAC-SHA256):
//! 1. Canonical request — method, path, canonical query (sorted, RFC3986),
//!    canonical headers (sorted `host` / `content-type` / `x-acs-*`,
//!    each line terminated by a newline), the signed-header list, and the
//!    hex SHA-256 of the payload.
//! 2. String to sign — `ACS3-HMAC-SHA256` + newline + hex-sha256 of the
//!    canonical request.
//! 3. Signature — HMAC-SHA256 keyed with the raw SecretKey.
//! 4. Authorization — `ACS3-HMAC-SHA256 Credential={ak},SignedHeaders=...,
//!    Signature={hex}`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Signed request headers for an ACS3 OpenAPI call.
pub(super) struct AcsSignedRequest {
    pub authorization: String,
    pub x_acs_date: String,
    pub x_acs_signature_nonce: String,
    pub x_acs_content_sha256: String,
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    hex::encode(mac.finalize().into_bytes())
}

/// The request being signed.
pub(super) struct AcsRequest<'a> {
    pub host: &'a str,
    pub path: &'a str,
    /// `x-acs-action` value.
    pub action: &'a str,
    /// `x-acs-version` value.
    pub version: &'a str,
    pub method: &'a str,
    /// Raw request payload bytes (empty for no-body POSTs).
    pub body: &'a [u8],
    /// Already-canonicalized query string WITHOUT the leading `?`
    /// (empty when there are no query params).
    pub query: &'a str,
    /// Optional STS token (`x-acs-security-token`).
    pub security_token: Option<&'a str>,
}

/// Sign an OpenAPI request with ACS3-HMAC-SHA256.
pub(super) fn sign(
    ak: &str,
    sk: &str,
    request: AcsRequest<'_>,
    now: chrono::DateTime<chrono::Utc>,
) -> AcsSignedRequest {
    let AcsRequest {
        host,
        path,
        action,
        version,
        method,
        body,
        query,
        security_token,
    } = request;
    let x_acs_date = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let nonce = uuid::Uuid::new_v4().to_string();
    let payload_hash = sha256_hex(body);

    // Canonical headers: host + content-type + x-acs-* entries, sorted by
    // header name, each line `name:value` terminated by a newline.
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), host.to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("x-acs-action".to_string(), action.to_string()),
        ("x-acs-version".to_string(), version.to_string()),
        ("x-acs-date".to_string(), x_acs_date.clone()),
        ("x-acs-signature-nonce".to_string(), nonce.clone()),
        ("x-acs-content-sha256".to_string(), payload_hash.clone()),
    ];
    if let Some(token) = security_token {
        headers.push(("x-acs-security-token".to_string(), token.to_string()));
    }
    headers.sort();
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}:{v}
"
            )
        })
        .collect::<String>();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = [
        method,
        path,
        query,
        canonical_headers.as_str(),
        signed_headers.as_str(),
        payload_hash.as_str(),
    ]
    .join(
        "
",
    );

    let algorithm = "ACS3-HMAC-SHA256";
    let string_to_sign = format!(
        "{algorithm}
{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = hmac_sha256_hex(sk.as_bytes(), string_to_sign.as_bytes());

    AcsSignedRequest {
        authorization: format!(
            "{algorithm} Credential={ak},SignedHeaders={signed_headers},Signature={signature}"
        ),
        x_acs_date,
        x_acs_signature_nonce: nonce,
        x_acs_content_sha256: payload_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_builds_expected_structure() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-28T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // The signature depends on the random nonce, so assert structure only.
        let signed = sign(
            "LTAI5test",
            "secret",
            AcsRequest {
                host: "modelstudio.cn-beijing.aliyuncs.com",
                path: "/modelstudio/cli/generateAccessToken",
                action: "GenerateCLIAccessToken",
                version: "2026-02-10",
                method: "POST",
                body: b"",
                query: "",
                security_token: None,
            },
            now,
        );
        assert!(signed.authorization.starts_with(
            "ACS3-HMAC-SHA256 Credential=LTAI5test,SignedHeaders=content-type;host;x-acs-action;x-acs-content-sha256;x-acs-date;x-acs-signature-nonce;x-acs-version,Signature="
        ));
        assert_eq!(signed.x_acs_date, "2026-08-28T12:00:00Z");
        assert_eq!(signed.x_acs_content_sha256, sha256_hex(b""));
    }
}
