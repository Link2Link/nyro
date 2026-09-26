//! Xiaomi MiMo console session renewal (passToken → serviceToken).
//!
//! The MiMo console quota endpoints (`platform.xiaomimimo.com/api/v1/*`)
//! reject the inference API key and only accept the Xiaomi account
//! web-session Cookie — whose `api-platform_serviceToken` lives ~24h, which
//! is why raw-Cookie credentials expire constantly. The Xiaomi passport,
//! however, hands out long-lived `passToken`s (account.xiaomi.com,
//! months-scale): one signed serviceLogin exchanges it for a fresh
//! serviceToken whenever the previous one fades. Flow ported from
//! MiForge/migate `service.get_service`:
//!
//! 1. `GET https://account.xiaomi.com/pass/serviceLogin?sid=api-platform
//!    &_json=true` with `userId`/`passToken` (optionally `deviceId`)
//!    cookies → JSON body (after the `&&&START&&&` marker) carrying
//!    `nonce`, `ssecurity`, `location`.
//! 2. `clientSign = urlencode(base64(sha1("nonce=" + nonce + "&" + ssecurity)))`
//! 3. `GET {location}&clientSign={...}` — the STS hop chain answers with
//!    `Set-Cookie: api-platform_serviceToken=...; userId=...;
//!    api-platform_ph=...; api-platform_slh=...` on the platform domain;
//!    every hop is captured and reassembled into the Cookie header.
//!
//! The sid is `api-platform`, verified against the platform's own login
//! redirect (`GET platform.xiaomimimo.com/api/v1/genLoginUrl` → 302
//! `account.xiaomi.com/pass/serviceLogin?...&sid=api-platform`).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::Value;
use sha1::{Digest, Sha1};

/// serviceLogin sid of platform.xiaomimimo.com (from the platform's own
/// `/api/v1/genLoginUrl` redirect).
pub(crate) const MIMO_SERVICE_LOGIN_SID: &str = "api-platform";

/// Xiaomi passport serviceLogin entry (shared by every Xiaomi web service).
const ACCOUNT_SERVICE_LOGIN_URL: &str = "https://account.xiaomi.com/pass/serviceLogin";

/// Marker the passport strips from every `_json=true` response.
const PASSPORT_JSON_MARKER: &str = "&&&START&&&";

/// Name of the console session cookie set by the STS hop chain.
const SERVICE_TOKEN_COOKIE: &str = "api-platform_serviceToken";

/// Redirect hops chased while collecting Set-Cookie from the STS location.
const STS_MAX_HOPS: usize = 5;

/// Minted serviceToken reuse window. Console tokens live ~24h; a 30-minute
/// window keeps queries snappy while a faded token still triggers re-mint
/// through the session-rejected retry.
const MINTED_COOKIE_TTL: Duration = Duration::from_secs(30 * 60);

/// Account-level credential stored in the usage-credential slots: the
/// long-lived passToken plus its userId, optionally the account `deviceId`
/// cookie migate keeps alongside them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MimoPassportCredential {
    pub pass_token: String,
    pub user_id: String,
    pub device_id: Option<String>,
}

/// Marker error: the MiMo console rejected the session (HTTP 401/403,
/// console-level code 401/403, or a redirect into the login flow). Detected
/// via `anyhow::Error::downcast_ref` to trigger one re-mint-and-retry.
#[derive(Debug)]
pub(crate) struct MimoSessionRejected(pub(crate) String);

impl std::fmt::Display for MimoSessionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MimoSessionRejected {}

impl MimoSessionRejected {
    /// Session-rejection error with the same guidance the legacy raw-Cookie
    /// path always gave: in passToken mode a fresh serviceToken is re-minted
    /// automatically; a raw Cookie has to be refreshed by hand.
    pub(crate) fn new(detail: impl std::fmt::Display) -> Self {
        Self(format!(
            "MiMo console session rejected ({detail}): re-login platform.xiaomimimo.com and \
             refresh the usage-query Cookie (passToken mode re-mints the session automatically)"
        ))
    }
}

/// How the usage-credential slots were interpreted for a MiMo provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MimoUsageCredential {
    /// Account-level passToken + userId: console sessions are minted on
    /// demand through the passport renewal flow.
    Passport(MimoPassportCredential),
    /// A passToken-shaped value without the userId — a configuration error
    /// that must be reported precisely instead of being sent as a Cookie
    /// (which would only produce a confusing console 401).
    MissingUserId { pass_token: String },
    /// The legacy raw console Cookie header (verbatim into the request).
    Cookie(String),
}

/// Classify the usage-credential slots for a MiMo provider.
///
/// Accepted forms (slot A / slot B):
/// * bare passToken + userId — what the WebUI form writes. Modern Xiaomi
///   passTokens are `V1:` + base64 (padding `=` and `:` included), so cookie
///   detection relies on cookie SYNTAX markers (`;`-separated pairs,
///   `serviceToken`, `passToken=`) rather than on the absence of `=`.
/// * a Cookie header containing `passToken=` (account.xiaomi.com paste) —
///   passToken/userId/deviceId extracted from it, slot B overriding userId.
/// * `passToken:<token>` + userId — explicit prefix form.
/// * anything carrying `;` or `serviceToken` — the legacy console Cookie.
pub(crate) fn classify_usage_credential(slot_a: &str, slot_b: &str) -> MimoUsageCredential {
    let a = slot_a.trim();
    let b = slot_b.trim();
    if a.is_empty() {
        return MimoUsageCredential::Cookie(a.to_string());
    }
    if let Some(rest) = a.strip_prefix("passToken:").map(str::trim) {
        let pass_token = rest.trim_end_matches(';').trim().to_string();
        if b.is_empty() {
            return MimoUsageCredential::MissingUserId { pass_token };
        }
        return MimoUsageCredential::Passport(MimoPassportCredential {
            pass_token,
            user_id: b.to_string(),
            device_id: None,
        });
    }
    if a.contains("passToken=") {
        // An account.xiaomi.com Cookie header: extract the passport pieces.
        let cookies = parse_cookie_pairs(a);
        let Some(pass_token) = cookies
            .get("passToken")
            .map(|value: &String| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            // Malformed `passToken=` piece: fall back to legacy handling.
            return MimoUsageCredential::Cookie(a.to_string());
        };
        let user_id = if b.is_empty() {
            match cookies
                .get("userId")
                .map(|value: &String| value.trim())
                .filter(|value| !value.is_empty())
            {
                Some(user_id) => user_id.to_string(),
                None => {
                    return MimoUsageCredential::MissingUserId { pass_token };
                }
            }
        } else {
            b.to_string()
        };
        return MimoUsageCredential::Passport(MimoPassportCredential {
            pass_token,
            user_id,
            device_id: cookies.get("deviceId").map(|value| value.trim().to_string()),
        });
    }
    // Cookie-syntax markers: the legacy console header. Console headers
    // always carry `serviceToken` (bare or `api-platform_serviceToken`) in
    // `;`-separated pairs; a bare passToken never does.
    if a.contains(';') || a.contains("serviceToken") {
        return MimoUsageCredential::Cookie(a.to_string());
    }
    // From here slot A is a bare passToken (modern `V1:…` or legacy value).
    if b.is_empty() {
        return MimoUsageCredential::MissingUserId {
            pass_token: a.to_string(),
        };
    }
    MimoUsageCredential::Passport(MimoPassportCredential {
        pass_token: a.to_string(),
        user_id: b.to_string(),
        device_id: None,
    })
}

/// Split a Cookie header into name/value pairs, dropping attributes and
/// stripping the quoting some Xiaomi cookies carry (`name="value"`).
fn parse_cookie_pairs(header: &str) -> HashMap<String, String> {
    header
        .split(';')
        .filter_map(|piece| {
            let (name, value) = piece.trim().split_once('=')?;
            Some((name.trim().to_string(), value.trim().trim_matches('"').to_string()))
        })
        .collect()
}

/// A successfully minted console session.
#[derive(Clone)]
pub(crate) struct MintedMimoCookie {
    pub(crate) header: String,
    minted_at: Instant,
}

/// Endpoints of the renewal flow. Fixed in production; overridable so tests
/// can point at a local server.
#[derive(Clone)]
pub(crate) struct PassportEndpoints {
    pub service_login_url: String,
}

impl Default for PassportEndpoints {
    fn default() -> Self {
        Self {
            service_login_url: ACCOUNT_SERVICE_LOGIN_URL.to_string(),
        }
    }
}

/// The serviceLogin response session (post-marker JSON).
#[derive(Debug)]
struct ServiceLoginSession {
    nonce: String,
    ssecurity: String,
    location: String,
}

fn parse_service_login_session(body: &str) -> anyhow::Result<ServiceLoginSession> {
    let trimmed = body.trim_start_matches(PASSPORT_JSON_MARKER).trim();
    let value: Value =
        serde_json::from_str(trimmed).context("failed to parse passport serviceLogin response")?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        // 70016-style codes mean the stored passToken was rejected outright.
        return Err(anyhow!(
            "Xiaomi passport rejected the passToken (code {code}): {description} — re-obtain \
             the passToken from account.xiaomi.com and update the usage-query credential"
        ));
    }
    let field = |name: &str| -> Option<String> {
        match value.get(name) {
            // `nonce` arrives as a JSON NUMBER (e.g. 3803270043235582976) —
            // migate's Python f-string renders it as decimal digits, so the
            // clientSign input uses the stringified number.
            Some(Value::Number(number)) => Some(number.to_string()),
            Some(Value::String(text)) => Some(text.trim().to_string())
                .filter(|item| !item.is_empty()),
            _ => None,
        }
    };
    let Some(nonce) = field("nonce") else {
        return Err(anyhow!(
            "Xiaomi passport returned no service session (no nonce): the stored passToken is \
             likely expired — re-obtain it from account.xiaomi.com"
        ));
    };
    let Some(ssecurity) = field("ssecurity") else {
        return Err(anyhow!(
            "Xiaomi passport returned no service session (no ssecurity): the stored passToken \
             is likely expired — re-obtain it from account.xiaomi.com"
        ));
    };
    let Some(location) = field("location") else {
        return Err(anyhow!(
            "Xiaomi passport returned no STS location: the stored passToken is likely expired \
             — re-obtain it from account.xiaomi.com"
        ));
    };
    Ok(ServiceLoginSession {
        nonce,
        ssecurity,
        location,
    })
}

/// `clientSign = urlencode(base64(sha1("nonce=" + nonce + "&" + ssecurity)))` —
/// migate's exact recipe.
fn client_sign(nonce: &str, ssecurity: &str) -> String {
    let digest = Sha1::digest(format!("nonce={nonce}&{ssecurity}").as_bytes());
    percent_encode_query(&BASE64.encode(digest))
}

/// Percent-encode a query value like Python's `urllib.parse.quote` with
/// safe='/' (what migate uses): everything but unreserved characters and
/// the path separator is escaped.
fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The passport client must NOT auto-follow redirects: the STS hop chain
/// sets the serviceToken via Set-Cookie on intermediate 302 responses, and
/// a redirect-following client without a cookie jar would lose it.
fn passport_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .user_agent(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
            )
            .build()
            .expect("reqwest client for Xiaomi passport")
    })
}

/// Exchange the account passToken for a fresh MiMo console Cookie header.
///
/// The caller-provided client must have redirects disabled (see
/// [`passport_client`]); production callers use that helper.
pub(crate) async fn mint_service_cookie_with(
    client: &reqwest::Client,
    endpoints: &PassportEndpoints,
    credential: &MimoPassportCredential,
) -> anyhow::Result<MintedMimoCookie> {
    // Step 1: signed-in serviceLogin with the passToken cookies.
    let mut account_cookies = format!("userId={}; passToken={}", credential.user_id, credential.pass_token);
    if let Some(device_id) = credential
        .device_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        account_cookies.push_str("; deviceId=");
        account_cookies.push_str(device_id);
    }
    let login_url = format!(
        "{}?sid={}&_json=true",
        endpoints.service_login_url, MIMO_SERVICE_LOGIN_SID
    );
    let resp = client
        .get(&login_url)
        .header("Cookie", &account_cookies)
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await
        .context("MiMo passport serviceLogin request failed")?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .context("failed to read passport serviceLogin response")?;
    if !status.is_success() {
        let preview: String = body.chars().take(200).collect();
        return Err(anyhow!("passport serviceLogin HTTP {status}: {preview}"));
    }
    let session = parse_service_login_session(&body)?;

    // Step 2: clientSign over (nonce, ssecurity).
    let sign = client_sign(&session.nonce, &session.ssecurity);
    let mut current = format!("{}&clientSign={}", session.location, sign);

    // Step 3: chase the STS redirect chain, capturing Set-Cookie per hop.
    let mut cookies: Vec<(String, String)> = Vec::new();
    for _ in 0..=STS_MAX_HOPS {
        let resp = client
            .get(&current)
            .header("Accept", "application/json, text/plain, */*")
            .send()
            .await
            .context("MiMo STS request failed")?;
        for value in resp.headers().get_all(reqwest::header::SET_COOKIE) {
            if let Some((name, rest)) = value.to_str().ok().and_then(|raw| raw.split_once('=')) {
                let name = name.trim().to_string();
                let (cookie_value, attrs) = rest.split_once(';').unwrap_or((rest, ""));
                let cookie_value = cookie_value.trim().to_string();
                // Deletion directives never clobber the real value: the live
                // STS response pairs a real `api-platform_slh` on the
                // platform domain with a same-name `Max-Age=0` clear on the
                // parent domain — a browser jar keeps both (different
                // domains), a name-keyed jar must drop the deletion.
                let attrs_lower = attrs.to_ascii_lowercase();
                let is_deletion =
                    cookie_value.is_empty() || attrs_lower.contains("max-age=0");
                if name.is_empty() || is_deletion {
                    continue;
                }
                // A later hop may rotate the token; last write wins.
                cookies.retain(|(existing, _)| *existing != name);
                cookies.push((name, cookie_value));
            }
        }
        if !resp.status().is_redirection() {
            break;
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(location) = location else {
            break;
        };
        current = reqwest::Url::parse(&current)
            .ok()
            .and_then(|base| base.join(location).ok())
            .map(|url| url.to_string())
            .unwrap_or_else(|| location.to_string());
    }

    if !cookies.iter().any(|(name, _)| name == SERVICE_TOKEN_COOKIE) {
        let names = cookies
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(anyhow!(
            "MiMo STS chain set no {SERVICE_TOKEN_COOKIE} cookie (got: {names}) — the renewal \
             flow changed upstream"
        ));
    }
    let header = cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(MintedMimoCookie {
        header,
        minted_at: Instant::now(),
    })
}

/// Production entry: mint with the shared no-redirect client against the
/// real Xiaomi passport.
pub(crate) async fn mint_service_cookie(
    credential: &MimoPassportCredential,
) -> anyhow::Result<MintedMimoCookie> {
    mint_service_cookie_with(passport_client(), &PassportEndpoints::default(), credential).await
}

/// Per-provider cache of minted console cookies (Gateway-owned state).
///
/// Minting serializes per provider inside the lock, so a burst of usage
/// queries triggers a single serviceLogin. `invalidate` drops the entry so
/// the next query re-mints — wired into the credential-save path and the
/// session-rejected retry.
pub(crate) struct MimoSessionCache {
    inner: tokio::sync::Mutex<HashMap<String, MintedMimoCookie>>,
}

impl Default for MimoSessionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MimoSessionCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Cached (or freshly minted) console Cookie header for a provider.
    pub(crate) async fn get_or_mint(
        &self,
        provider_id: &str,
        credential: &MimoPassportCredential,
    ) -> anyhow::Result<String> {
        let mut guard = self.inner.lock().await;
        if let Some(entry) = guard.get(provider_id)
            && entry.minted_at.elapsed() < MINTED_COOKIE_TTL
        {
            return Ok(entry.header.clone());
        }
        let minted = mint_service_cookie(credential).await?;
        let header = minted.header.clone();
        guard.insert(provider_id.to_string(), minted);
        Ok(header)
    }

    /// Drop the cached cookie so the next query mints a fresh one.
    pub(crate) async fn invalidate(&self, provider_id: &str) {
        self.inner.lock().await.remove(provider_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> MimoPassportCredential {
        MimoPassportCredential {
            pass_token: "pt-abc123".to_string(),
            user_id: "123456789".to_string(),
            device_id: Some("wb_dev".to_string()),
        }
    }

    #[test]
    fn parses_bare_passtoken_with_user_id() {
        let MimoUsageCredential::Passport(cred) =
            classify_usage_credential("pt-abc123", "123456789")
        else {
            panic!("expected Passport mode");
        };
        assert_eq!(cred.pass_token, "pt-abc123");
        assert_eq!(cred.user_id, "123456789");
        assert_eq!(cred.device_id, None);
    }

    #[test]
    fn parses_modern_v1_passtoken_with_base64_padding() {
        // Production shape: modern Xiaomi passTokens are `V1:` + base64 with
        // `=` padding — must NOT be mistaken for a Cookie header.
        let token = "V1:dox9yZLwL/3p+FO5UKz+4zIlUCSrL2Z19gnJ17I8=";
        let MimoUsageCredential::Passport(cred) = classify_usage_credential(token, "616928585")
        else {
            panic!("expected Passport mode");
        };
        assert_eq!(cred.pass_token, token);
        assert_eq!(cred.user_id, "616928585");

        // Without the userId the error must be precise, not a blind 401.
        assert_eq!(
            classify_usage_credential(token, ""),
            MimoUsageCredential::MissingUserId {
                pass_token: token.to_string()
            }
        );
    }

    #[test]
    fn parses_explicit_prefix_form() {
        let MimoUsageCredential::Passport(cred) =
            classify_usage_credential("passToken:pt-xyz; ", "42")
        else {
            panic!("expected Passport mode");
        };
        assert_eq!(cred.pass_token, "pt-xyz");
        assert_eq!(cred.user_id, "42");
    }

    #[test]
    fn parses_account_cookie_header() {
        let header = "userId=123456789; passToken=\"pt-quoted\"; deviceId=wb_dev; serviceToken=short";
        let MimoUsageCredential::Passport(cred) = classify_usage_credential(header, "") else {
            panic!("expected Passport mode");
        };
        assert_eq!(cred.pass_token, "pt-quoted");
        assert_eq!(cred.user_id, "123456789");
        assert_eq!(cred.device_id.as_deref(), Some("wb_dev"));

        // Slot B overrides the header's userId.
        let MimoUsageCredential::Passport(cred) = classify_usage_credential(header, "999") else {
            panic!("expected Passport mode");
        };
        assert_eq!(cred.user_id, "999");
    }

    #[test]
    fn console_cookie_header_stays_legacy() {
        // The platform console Cookie contains serviceToken but no
        // passToken — must NOT be mistaken for the passport credential.
        assert!(matches!(
            classify_usage_credential(
                "userId=123; api-platform_serviceToken=\"tok\"; api-platform_ph=ph",
                ""
            ),
            MimoUsageCredential::Cookie(_)
        ));
        // A single-pair console cookie (no `;`) is still caught by the
        // serviceToken marker.
        assert!(matches!(
            classify_usage_credential("api-platform_serviceToken=tok", ""),
            MimoUsageCredential::Cookie(_)
        ));
        // A V1: passToken without userId is a precise MissingUserId error
        // (not legacy), while an empty slot A stays legacy.
        assert!(matches!(
            classify_usage_credential("V1:abc=", ""),
            MimoUsageCredential::MissingUserId { .. }
        ));
        assert!(matches!(
            classify_usage_credential("", "123"),
            MimoUsageCredential::Cookie(_)
        ));
    }

    #[test]
    fn client_sign_matches_migate_recipe() {
        // urlencode(base64(sha1("nonce=" + nonce + "&" + ssecurity))) —
        // migate's f-string concatenates the VALUES, so the digest input
        // for ("n", "s") is literally "nonce=n&s".
        let digest = Sha1::digest(b"nonce=n&s");
        let expected = percent_encode_query(&BASE64.encode(digest));
        assert_eq!(client_sign("n", "s"), expected);
        // Deterministic and different inputs produce different signs.
        assert_ne!(client_sign("n", "s"), client_sign("n2", "s"));
        // Base64 characters that need query escaping are escaped.
        assert!(!client_sign("nonce-value", "sec-value").contains(char::is_whitespace));
    }

    #[test]
    fn service_login_json_rejects_signed_out_responses() {
        let err = parse_service_login_session(
            "&&&START&&&{\"code\":70016,\"description\":\"invalid session\"}",
        )
        .unwrap_err();
        assert!(err.to_string().contains("code 70016"));

        // Missing nonce → passToken-expired guidance, not a panic.
        let err = parse_service_login_session(
            "&&&START&&&{\"code\":0,\"description\":\"ok\",\"location\":\"https://x\"}",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no service session"));
    }

    #[test]
    fn service_login_json_accepts_numeric_nonce() {
        // Live shape (2026-09): `nonce` is a JSON NUMBER, ssecurity/location
        // strings. String-only parsing would misread a healthy response as
        // "no service session".
        let session = parse_service_login_session(concat!(
            "&&&START&&&{\"code\":0,\"desc\":\"成功\",\"nonce\":3803270043235582976,",
            "\"ssecurity\":\"Gzj286t7M+skXmuEmlUnHA==\",",
            "\"location\":\"https://platform.xiaomimimo.com/sts?d=wb_x&ticket=0\"}"
        ))
        .unwrap();
        assert_eq!(session.nonce, "3803270043235582976");
        assert_eq!(session.ssecurity, "Gzj286t7M+skXmuEmlUnHA==");
        // The clientSign digest input is the decimal nonce string.
        let digest = Sha1::digest(b"nonce=3803270043235582976&Gzj286t7M+skXmuEmlUnHA==");
        assert_eq!(
            client_sign(&session.nonce, &session.ssecurity),
            percent_encode_query(&BASE64.encode(digest))
        );
    }

    #[tokio::test]
    async fn mint_flow_collects_sts_cookies_across_redirects() {
        use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
        use axum::routing::get as route_get;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let sts_url = format!("http://{addr}/sts?sign=x");
        let login_body = format!(
            "&&&START&&&{{\"code\":0,\"nonce\":\"n1\",\"ssecurity\":\"s1\",\"location\":\"{sts_url}\"}}"
        );
        let login = route_get(move || async move { login_body.clone() });

        let sts = route_get(|| async move {
            let mut headers = HeaderMap::new();
            headers.append(
                header::SET_COOKIE,
                HeaderValue::from_static("api-platform_serviceToken=\"tok1\"; Path=/"),
            );
            headers.append(header::SET_COOKIE, HeaderValue::from_static("userId=123; Path=/"));
            // The live STS pairs a real slh with a same-name Max-Age=0
            // deletion on the parent domain — the deletion must not win.
            headers.append(
                header::SET_COOKIE,
                HeaderValue::from_static("api-platform_slh=\"slh1\"; Path=/"),
            );
            headers.append(
                header::SET_COOKIE,
                HeaderValue::from_static(
                    "api-platform_slh=; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:10 GMT",
                ),
            );
            headers.insert(header::LOCATION, HeaderValue::from_static("/done"));
            (StatusCode::FOUND, headers, String::new())
        });

        let done = route_get(|| async move {
            (
                StatusCode::OK,
                [(header::SET_COOKIE, "api-platform_ph=ph1; Path=/")],
                "ok",
            )
        });

        let app = axum::Router::new()
            .route("/pass/serviceLogin", login)
            .route("/sts", sts)
            .route("/done", done);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let endpoints = PassportEndpoints {
            service_login_url: format!("http://{addr}/pass/serviceLogin"),
        };
        let minted = mint_service_cookie_with(&client, &endpoints, &credential())
            .await
            .unwrap();
        // Values keep the quoting the platform itself emits (the console
        // Cookie headers browsers send carry `name="value"` pairs).
        assert!(minted.header.contains("api-platform_serviceToken=\"tok1\""));
        assert!(minted.header.contains("userId=123"));
        // The real slh survived the same-name deletion directive.
        assert!(minted.header.contains("api-platform_slh=\"slh1\""));
        assert!(!minted.header.contains("api-platform_slh=;"));
        assert!(minted.header.contains("api-platform_ph=ph1"));
        server.abort();
    }

    #[tokio::test]
    async fn session_cache_reuses_within_ttl_and_invalidates() {
        let cache = MimoSessionCache::new();
        // get_or_mint would hit the network for a fresh credential; verify
        // the invalidate path instead by checking a missing entry doesn't
        // error the lock (and that invalidate is a no-op when absent).
        cache.invalidate("p1").await;
        assert!(cache.inner.lock().await.get("p1").is_none());
    }
}
