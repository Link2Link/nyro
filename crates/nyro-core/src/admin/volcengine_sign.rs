//! Volcengine Signature V4 request signing (an AWS SigV4 variant).
//!
//! Used to call the Volcengine control-plane OpenAPI
//! (`open.volcengineapi.com`) — e.g. Ark coding-plan usage queries — which
//! requires account-level IAM AccessKey/SecretKey signing. The inference
//! API key is NOT accepted there.
//!
//! Algorithm (per Volcengine API signing docs):
//! 1. Canonical request — method, path, sorted query, canonical headers
//!    (sorted, lowercase, trimmed values, fixed `host;x-content-sha256;x-date`
//!    set), and the hex SHA-256 of the payload.
//! 2. String to sign — `HMAC-SHA256\n{x-date}\n{date}/{region}/{service}/request\n
//!    {hex-sha256(canonical-request)}`.
//! 3. Signing key — chained HMAC-SHA256 of the raw SecretKey (no prefix):
//!    date → region → service → `"request"`.
//! 4. Authorization — `HMAC-SHA256 Credential={ak}/{date}/{region}/{service}/request,
//!    SignedHeaders=host;x-content-sha256;x-date, Signature={hex}`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Signed request headers and the final URL (query included).
pub(super) struct SignedRequest {
    pub url: String,
    pub x_date: String,
    pub x_content_sha256: String,
    pub authorization: String,
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode a query component per the canonical-form rules
/// (unreserved `A-Z a-z 0-9 - _ . ~` stay; everything else becomes %XX).
fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Build the canonical query string: entries sorted by encoded key then
/// encoded value, joined with `&`.
fn canonical_query(params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    encoded.sort();
    encoded
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Sign a GET request to a Volcengine OpenAPI endpoint.
///
/// * `host` — e.g. `open.volcengineapi.com`
/// * `path` — e.g. `/`
/// * `params` — action/version plus any filters (sorted internally)
/// * `region` / `service` — e.g. `cn-beijing` / `ark`
pub(super) fn sign_get(
    ak: &str,
    sk: &str,
    host: &str,
    path: &str,
    params: &[(String, String)],
    region: &str,
    service: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> SignedRequest {
    let x_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &x_date[..8];
    let payload_hash = sha256_hex(b"");

    let query = canonical_query(params);
    let url = format!("https://{host}{path}?{query}");

    let canonical_headers =
        format!("host:{host}\nx-content-sha256:{payload_hash}\nx-date:{x_date}\n");
    let signed_headers = "host;x-content-sha256;x-date";

    let canonical_request =
        format!("GET\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let credential_scope = format!("{date}/{region}/{service}/request");
    let string_to_sign = format!(
        "HMAC-SHA256\n{x_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(sk.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    SignedRequest {
        url,
        x_date,
        x_content_sha256: payload_hash,
        authorization: format!(
            "HMAC-SHA256 Credential={ak}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_keeps_unreserved_and_encodes_rest() {
        assert_eq!(uri_encode("abcXYZ09-_.~"), "abcXYZ09-_.~");
        assert_eq!(uri_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(uri_encode("值"), "%E5%80%BC");
    }

    #[test]
    fn canonical_query_sorts_by_encoded_key() {
        let q = canonical_query(&[
            ("Version".to_string(), "2024-01-01".to_string()),
            ("Action".to_string(), "GetCodingPlanUsage".to_string()),
        ]);
        assert_eq!(q, "Action=GetCodingPlanUsage&Version=2024-01-01");
    }

    #[test]
    fn sign_get_is_deterministic_and_well_shaped() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-16T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let params = vec![
            ("Action".to_string(), "GetCodingPlanUsage".to_string()),
            ("Version".to_string(), "2024-01-01".to_string()),
        ];
        let signed = sign_get(
            "AK-test",
            "SK-test",
            "open.volcengineapi.com",
            "/",
            &params,
            "cn-beijing",
            "ark",
            now,
        );
        assert_eq!(signed.x_date, "20260816T080000Z");
        assert_eq!(
            signed.url,
            "https://open.volcengineapi.com/?Action=GetCodingPlanUsage&Version=2024-01-01"
        );
        // Empty GET payload hashes to the well-known SHA-256 of "".
        assert_eq!(
            signed.x_content_sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(signed.authorization.starts_with(
            "HMAC-SHA256 Credential=AK-test/20260816/cn-beijing/ark/request, \
             SignedHeaders=host;x-content-sha256;x-date, Signature="
        ));
        let signature = signed
            .authorization
            .rsplit("Signature=")
            .next()
            .unwrap()
            .to_string();
        assert_eq!(signature.len(), 64);
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));

        // Same inputs → same signature; different key → different signature.
        let again = sign_get(
            "AK-test",
            "SK-test",
            "open.volcengineapi.com",
            "/",
            &params,
            "cn-beijing",
            "ark",
            now,
        );
        assert_eq!(again.authorization, signed.authorization);
        let other = sign_get(
            "AK-test",
            "SK-other",
            "open.volcengineapi.com",
            "/",
            &params,
            "cn-beijing",
            "ark",
            now,
        );
        assert_ne!(other.authorization, signed.authorization);
    }
}
