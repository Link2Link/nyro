//! Devin CLI's manual/headless PKCE flow. Session tokens have no refresh endpoint.
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;

use super::shared::{
    PkceAuthState, build_authorize_url, expires_at_after, generate_code_challenge,
    generate_code_verifier, generate_state, parse_session_state, required_http_client,
    validate_callback_state,
};
use crate::auth::types::*;
use crate::provider::{OAuthConfig, VendorRegistry};

const TOKEN_PREFIX: &str = "devin-session-token$";
const API_BASE_URL: &str = "https://server.codeium.com";

#[derive(Debug, Default)]
pub struct DevinAuthDriver;

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

enum ManualInput {
    Code(String),
    Token(String),
}

fn normalize_session_token(raw: &str) -> Result<String> {
    let token = raw.trim();
    if token.is_empty()
        || token == TOKEN_PREFIX
        || token.len() > 32768
        || token
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control() || !c.is_ascii())
    {
        bail!("Devin session token is invalid");
    }
    Ok(if token.starts_with("eyJ") {
        format!("{TOKEN_PREFIX}{token}")
    } else {
        token.to_string()
    })
}

fn credential(raw: &str) -> Result<CredentialBundle> {
    Ok(CredentialBundle {
        access_token: Some(normalize_session_token(raw)?),
        resource_url: Some(API_BASE_URL.to_string()),
        // Do not persist the token response again in unstructured metadata.
        // No expiry or refresh token is advertised by this API.
        ..Default::default()
    })
}

fn parse_manual_input(input: &AuthExchangeInput, expected_state: &str) -> Result<ManualInput> {
    let raw = input
        .callback_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(input.code.as_deref())
        .unwrap_or("")
        .trim()
        .trim_matches(['\'', '"'])
        .trim();
    if raw.len() > 32768 {
        bail!("Devin authorization input is too large");
    }
    if raw.starts_with(TOKEN_PREFIX) || raw.starts_with("eyJ") {
        return Ok(ManualInput::Token(normalize_session_token(raw)?));
    }
    let parsed = reqwest::Url::parse(raw).ok().or_else(|| {
        if raw.contains('=') || raw.starts_with(['?', '#']) {
            reqwest::Url::parse(&format!(
                "https://callback.invalid/?{}",
                raw.trim_start_matches(['?', '#'])
            ))
            .ok()
        } else {
            None
        }
    });
    let code = if let Some(url) = parsed {
        let mut code = None;
        let mut state = None;
        let fragment_url = url.fragment().and_then(|fragment| {
            reqwest::Url::parse(&format!("https://callback.invalid/?{fragment}")).ok()
        });
        let pairs = url
            .query_pairs()
            .chain(fragment_url.iter().flat_map(|url| url.query_pairs()));
        for (key, value) in pairs {
            match key.as_ref() {
                "error" | "error_description" => {
                    bail!("Devin authorization was denied; start again")
                }
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                "code" | "state" => bail!("Devin callback contains duplicate parameters"),
                _ => {}
            }
        }
        // The manual CLI flow may return only a code; PKCE binds that code to
        // this session. When a callback includes state it must match exactly.
        if let Some(state) = state {
            validate_callback_state(expected_state, Some(&state), "Devin")?;
        }
        code.ok_or_else(|| anyhow!("Devin callback is missing an authorization code"))?
    } else {
        raw.to_string()
    };
    if code.is_empty()
        || code.len() > 16384
        || code
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "/?#=".contains(c))
    {
        bail!("Paste a Devin authorization code, callback URL, or session token");
    }
    Ok(ManualInput::Code(code))
}

impl DevinAuthDriver {
    fn config() -> Result<&'static OAuthConfig> {
        VendorRegistry::global()
            .metadata("devin")
            .and_then(|vendor| vendor.channels.iter().find(|channel| channel.id == "cli"))
            .and_then(|channel| channel.oauth.as_ref())
            .ok_or_else(|| anyhow!("missing OAuth configuration for devin/cli"))
    }

    async fn exchange_at(
        &self,
        session: &AuthSession,
        input: AuthExchangeInput,
        client: Option<reqwest::Client>,
        endpoint: &str,
    ) -> Result<CredentialBundle> {
        let state: PkceAuthState = parse_session_state(session)?;
        let code = match parse_manual_input(&input, &state.state)? {
            ManualInput::Token(token) => return credential(&token),
            ManualInput::Code(code) => code,
        };
        let client = required_http_client(client)?;
        // Never propagate upstream bodies, URLs, or reqwest error chains into
        // admin errors/logs: they can contain credentials or authorization codes.
        let response = client
            .post(endpoint)
            .timeout(Duration::from_secs(30))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({ "code": code, "code_verifier": state.code_verifier }))
            .send()
            .await
            .map_err(|_| anyhow!("Devin token exchange could not connect"))?;
        if !response.status().is_success() {
            bail!(
                "Devin token exchange failed: HTTP {}",
                response.status().as_u16()
            );
        }
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("Devin token response could not be read"))?
        {
            if body.len().saturating_add(chunk.len()) > 64 * 1024 {
                bail!("Devin token response is too large");
            }
            body.extend_from_slice(&chunk);
        }
        let token: TokenResponse = serde_json::from_slice(&body)
            .map_err(|_| anyhow!("Devin token response is invalid or missing token"))?;
        credential(&token.token)
    }
}

#[async_trait]
impl AuthDriver for DevinAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "devin",
            label: "Devin CLI",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = Self::config()?;
        let code_verifier = generate_code_verifier();
        let state = generate_state();
        let auth_url = build_authorize_url(
            config.authorize_url,
            &[
                ("state", &state),
                ("prompt", "select_account"),
                ("code_challenge", &generate_code_challenge(&code_verifier)),
                ("code_challenge_method", "S256"),
                ("cli_pkce_marker", "1"),
            ],
        )?;
        Ok(CreateAuthSession {
            provider_id: ctx.provider_id,
            driver_key: "devin".into(),
            scheme: self.metadata().scheme.as_str().into(),
            status: "pending".into(),
            use_proxy: ctx.use_proxy,
            user_code: None,
            verification_uri: Some(config.auth_base_url.into()),
            verification_uri_complete: Some(auth_url),
            state_json: Some(serde_json::to_string(&PkceAuthState {
                code_verifier,
                state,
                redirect_uri: String::new(),
            })?),
            context_json: None,
            result_json: None,
            expires_at: Some(expires_at_after(10 * 60)),
            poll_interval_seconds: None,
            last_error: None,
        })
    }

    async fn exchange(
        &self,
        session: &AuthSession,
        input: AuthExchangeInput,
        ctx: ExchangeAuthContext,
    ) -> Result<CredentialBundle> {
        self.exchange_at(session, input, ctx.http_client, Self::config()?.token_url)
            .await
    }

    fn bind_runtime(
        &self,
        _provider: &crate::db::models::Provider,
        _credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        // The native Devin transport wraps the session token in its Basic auth
        // envelope; never also attach the generic Bearer credential.
        Ok(RuntimeBinding {
            disable_default_auth: true,
            ..Default::default()
        })
    }

    async fn refresh(
        &self,
        _credential: &StoredCredential,
        _ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        bail!(
            "Devin session tokens cannot be refreshed; sign in again using Devin CLI authorization"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn input(value: &str) -> AuthExchangeInput {
        AuthExchangeInput {
            code: Some(value.into()),
            ..Default::default()
        }
    }

    async fn session() -> AuthSession {
        let init = DevinAuthDriver
            .start(StartAuthContext::default())
            .await
            .unwrap();
        AuthSession {
            generation: 0,
            id: "test-devin".into(),
            provider_id: None,
            driver_key: init.driver_key,
            scheme: init.scheme,
            status: init.status,
            use_proxy: false,
            user_code: None,
            verification_uri: init.verification_uri,
            verification_uri_complete: init.verification_uri_complete,
            state_json: init.state_json,
            context_json: None,
            result_json: None,
            expires_at: init.expires_at,
            poll_interval_seconds: None,
            last_error: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[tokio::test]
    async fn devin_manual_url_has_pkce_and_independent_state() {
        assert_eq!(
            generate_code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let first = session().await;
        let state: PkceAuthState = parse_session_state(&first).unwrap();
        let next: PkceAuthState = parse_session_state(&session().await).unwrap();
        assert_ne!(state.state, next.state);
        assert_ne!(state.code_verifier, next.code_verifier);
        assert!(state.code_verifier.len() >= 43);
        let url = reqwest::Url::parse(first.verification_uri_complete.as_deref().unwrap()).unwrap();
        assert_eq!(url.origin().ascii_serialization(), "https://app.devin.ai");
        assert_eq!(url.path(), "/auth/cli/continue");
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 5);
        assert_eq!(pairs["state"], state.state);
        assert_eq!(
            pairs["code_challenge"],
            generate_code_challenge(&state.code_verifier)
        );
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["prompt"], "select_account");
        assert_eq!(pairs["cli_pkce_marker"], "1");
        assert!(!url.as_str().contains(&state.code_verifier));
    }

    #[test]
    fn devin_manual_input_checks_state_and_errors() {
        for value in [
            "manual-code",
            "https://app.devin.ai/callback?code=manual-code&state=expected",
            "code=manual-code&state=expected",
        ] {
            assert!(
                matches!(parse_manual_input(&input(value), "expected").unwrap(), ManualInput::Code(code) if code == "manual-code")
            );
        }
        for value in [
            "",
            "not a code",
            "https://app.devin.ai/callback?code=secret&state=wrong",
            "code=secret&state=",
            "code=secret&state=expected&state=wrong",
            "https://app.devin.ai/callback?error=secret",
            "https://app.devin.ai/callback?code=secret&error=secret",
        ] {
            let err = parse_manual_input(&input(value), "expected")
                .err()
                .expect("must reject");
            assert!(!err.to_string().contains("secret"));
        }
    }

    #[test]
    fn devin_normalizes_tokens_without_refresh_or_raw_secrets() {
        for (raw, expected) in [
            (
                " eyJfixture.payload.signature ",
                "devin-session-token$eyJfixture.payload.signature",
            ),
            ("devin-session-token$opaque", "devin-session-token$opaque"),
            ("opaque", "opaque"),
        ] {
            let bundle = credential(raw).unwrap();
            assert_eq!(bundle.access_token.as_deref(), Some(expected));
            assert!(bundle.refresh_token.is_none() && bundle.expires_at.is_none());
            assert!(bundle.raw.is_null());
        }
        for invalid in ["", "devin-session-token$", "bad\nsecret", "bad secret"] {
            assert!(credential(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn devin_session_token_import_needs_no_network() {
        let bundle = DevinAuthDriver
            .exchange_at(
                &session().await,
                input("'eyJfixture.payload.signature'"),
                None,
                "http://invalid",
            )
            .await
            .unwrap();
        assert_eq!(
            bundle.access_token.as_deref(),
            Some("devin-session-token$eyJfixture.payload.signature")
        );
        assert!(
            DevinAuthDriver
                .refresh(&StoredCredential::default(), RefreshAuthContext::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn devin_token_exchange_uses_json_and_redacts_failures() {
        for (status, body) in [
            (200, r#"{"token":"eyJfixture.payload.signature"}"#),
            (400, r#"{"error":"secret-auth-code"}"#),
            (200, r#"{"unexpected":"secret-auth-code"}"#),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/auth/cli/token", listener.local_addr().unwrap());
            let session = session().await;
            let state: PkceAuthState = parse_session_state(&session).unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() < split + 4 + length {
                            continue;
                        }
                        assert!(headers.starts_with("post /auth/cli/token "));
                        assert!(headers.contains("content-type: application/json"));
                        let body: serde_json::Value =
                            serde_json::from_slice(&bytes[split + 4..split + 4 + length]).unwrap();
                        assert_eq!(
                            body,
                            serde_json::json!({"code":"secret-auth-code", "code_verifier":state.code_verifier})
                        );
                        break;
                    }
                }
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let result = DevinAuthDriver
                .exchange_at(
                    &session,
                    input("secret-auth-code"),
                    Some(reqwest::Client::builder().no_proxy().build().unwrap()),
                    &endpoint,
                )
                .await;
            server.await.unwrap();
            if status == 200 && body.contains("\"token\"") {
                assert_eq!(
                    result.unwrap().access_token.as_deref(),
                    Some("devin-session-token$eyJfixture.payload.signature")
                );
            } else {
                let error = format!("{:#}", result.unwrap_err());
                assert!(!error.contains("secret-auth-code") && !error.contains(body));
            }
        }
    }
}
