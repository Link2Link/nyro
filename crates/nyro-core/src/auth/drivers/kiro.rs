//! Kiro browser/device login, credential import, and token refresh.
//!
//! Google/GitHub use Kiro's social device endpoints; Builder ID and enterprise
//! IAM Identity Center use AWS SSO OIDC. Neither needs a callback to this gateway.
//! Legacy IDE/CLI credential, refresh-token, and API-key import remains available.
//! Social and AWS OIDC credentials retain their distinct refresh protocols.
mod device;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use super::shared::{expires_at_after, required_http_client};
use crate::auth::types::*;
use crate::provider::kiro::credential::{KiroCredential, arn_region, is_api_key, parse_import};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY: usize = 256 * 1024;
const DEFAULT_LIFETIME_SECONDS: i64 = 3600;
/// An imported access token must outlive this margin to be used as-is.
const IMPORT_MARGIN_SECONDS: i64 = 300;
const IDE_USER_AGENT: &str = "KiroIDE";

#[derive(Debug, Default)]
pub struct KiroAuthDriver;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "profileArn")]
    profile_arn: Option<String>,
    #[serde(rename = "expiresIn")]
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ProfilesResponse {
    #[serde(default)]
    profiles: Vec<Profile>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    arn: Option<String>,
}

/// Parse RFC3339 or the millisecond ISO strings Kiro writes.
fn parse_expiry(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    mut response: reqwest::Response,
    what: &str,
) -> Result<T> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("{what} response could not be read"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            bail!("{what} response is too large");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| anyhow!("{what} response is invalid"))
}

fn refresh_request(
    client: &reqwest::Client,
    credential: &KiroCredential,
    refresh_token: &str,
) -> Result<reqwest::RequestBuilder> {
    let request = if credential.refreshes_through_oidc() {
        let (Some(client_id), Some(client_secret)) = (
            credential.client_id.as_deref(),
            credential.client_secret.as_deref(),
        ) else {
            bail!(
                "Kiro Builder ID / IAM Identity Center credentials need clientId and clientSecret to refresh"
            );
        };
        client.post(credential.oidc_token_url()).json(&json!({
            "refreshToken": refresh_token,
            "clientId": client_id,
            "clientSecret": client_secret,
            "grantType": "refresh_token",
        }))
    } else {
        client
            .post(credential.social_refresh_url())
            .json(&json!({ "refreshToken": refresh_token }))
    };
    Ok(request
        .timeout(HTTP_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, IDE_USER_AGENT))
}

/// Refresh `credential` with `refresh_token`. Upstream bodies are never put in
/// errors: they can echo credentials or authorization material.
async fn refresh_tokens(
    client: &reqwest::Client,
    credential: &KiroCredential,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let response = refresh_request(client, credential, refresh_token)?
        .send()
        .await
        .map_err(|_| anyhow!("Kiro token refresh could not connect"))?;
    let status = response.status();
    if matches!(status.as_u16(), 400 | 401 | 403) {
        bail!(
            "Kiro rejected the refresh token (HTTP {}); sign in to Kiro again and re-import the credential",
            status.as_u16()
        );
    }
    if !status.is_success() {
        bail!("Kiro token refresh failed: HTTP {}", status.as_u16());
    }
    let token: TokenResponse = read_json(response, "Kiro token").await?;
    if token
        .access_token
        .as_deref()
        .is_none_or(|token| token.trim().is_empty())
    {
        bail!("Kiro token response is missing an access token");
    }
    Ok(token)
}

/// Social credentials receive a profile ARN with every refresh; Builder ID and
/// IdC credentials must look it up. A failed lookup is soft: Builder ID falls
/// back to the profile-less CodeWhisperer host at request time.
async fn discover_profile(
    client: &reqwest::Client,
    credential: &KiroCredential,
    access_token: &str,
) -> Option<String> {
    let response = client
        .post(credential.list_profiles_url())
        .timeout(HTTP_TIMEOUT)
        .header(reqwest::header::USER_AGENT, IDE_USER_AGENT)
        .bearer_auth(access_token)
        .json(&json!({ "maxResults": 10 }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let profiles: ProfilesResponse = read_json(response, "Kiro profile").await.ok()?;
    let arns: Vec<String> = profiles
        .profiles
        .into_iter()
        .filter_map(|p| p.arn)
        .collect();
    let marker = format!(":{}:", credential.region);
    arns.iter()
        .find(|arn| arn.contains(&marker))
        .or_else(|| arns.first())
        .cloned()
}

fn bundle(
    credential: &KiroCredential,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<String>,
) -> CredentialBundle {
    CredentialBundle {
        access_token: Some(access_token),
        refresh_token,
        expires_at,
        // The provider's base URL must always be the Q origin of the region,
        // even for Builder ID accounts that generate through CodeWhisperer.
        resource_url: Some(format!("https://q.{}.amazonaws.com", credential.region)),
        subject_id: credential.profile_arn.clone(),
        scopes: Vec::new(),
        raw: credential.to_meta(),
    }
}

impl KiroAuthDriver {
    async fn import(&self, input: &str, client: &reqwest::Client) -> Result<CredentialBundle> {
        let imported = parse_import(input).map_err(|error| anyhow!("{error}"))?;
        let mut credential = imported.credential;
        let mut access_token = imported.access_token;
        let mut refresh_token = imported.refresh_token;
        let mut expires_at = imported.expires_at.as_deref().and_then(parse_expiry);

        let usable = access_token.as_deref().is_some_and(|token| {
            is_api_key(token)
                || expires_at.is_some_and(|at| {
                    at > Utc::now() + chrono::Duration::seconds(IMPORT_MARGIN_SECONDS)
                })
        });
        if !usable {
            let Some(token) = refresh_token.clone() else {
                bail!("The pasted access token has expired and no refresh token was provided");
            };
            let refreshed = refresh_tokens(client, &credential, &token).await?;
            access_token = refreshed.access_token;
            if let Some(next) = refreshed
                .refresh_token
                .filter(|value| !value.trim().is_empty())
            {
                refresh_token = Some(next);
            }
            if let Some(arn) = refreshed.profile_arn.filter(|arn| !arn.trim().is_empty()) {
                credential.profile_arn = Some(arn);
            }
            expires_at = Some(
                Utc::now()
                    + chrono::Duration::seconds(
                        refreshed
                            .expires_in
                            .unwrap_or(DEFAULT_LIFETIME_SECONDS)
                            .max(60),
                    ),
            );
        }
        let access_token =
            access_token.ok_or_else(|| anyhow!("Kiro credential has no access token"))?;
        if let Some(region) = credential.profile_arn.as_deref().and_then(arn_region) {
            credential.region = region.to_string();
        }
        if credential.profile_arn.is_none() && !is_api_key(&access_token) {
            credential.profile_arn = discover_profile(client, &credential, &access_token).await;
        }
        Ok(bundle(
            &credential,
            access_token,
            refresh_token,
            expires_at.map(|at| at.to_rfc3339()),
        ))
    }
}

#[async_trait]
impl AuthDriver for KiroAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "kiro",
            label: "Kiro",
            // Keep the historical stored scheme for credential compatibility;
            // browser sessions explicitly select OAuthDeviceCode in start().
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }

    fn supports_session_scheme(&self, scheme: &str) -> bool {
        matches!(scheme, "oauth_auth_code_pkce" | "oauth_device_code")
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        if ctx
            .kiro
            .as_ref()
            .is_some_and(|options| options.method != KiroLoginMethod::Import)
        {
            return device::start(ctx).await;
        }
        Ok(CreateAuthSession {
            provider_id: ctx.provider_id,
            driver_key: "kiro".into(),
            scheme: self.metadata().scheme.as_str().into(),
            status: "pending".into(),
            use_proxy: ctx.use_proxy,
            user_code: None,
            verification_uri: None,
            verification_uri_complete: None,
            state_json: None,
            context_json: Some(json!({"completion_mode":"manual_exchange"}).to_string()),
            result_json: None,
            expires_at: Some(expires_at_after(10 * 60)),
            poll_interval_seconds: None,
            last_error: None,
        })
    }

    async fn poll(&self, session: &AuthSession, ctx: RefreshAuthContext) -> Result<AuthPollState> {
        device::poll(session, ctx).await
    }

    async fn exchange(
        &self,
        session: &AuthSession,
        input: AuthExchangeInput,
        ctx: ExchangeAuthContext,
    ) -> Result<CredentialBundle> {
        if session.scheme == AuthScheme::OAuthDeviceCode.as_str() {
            bail!(
                "Complete Kiro authorization in the browser; this session is polled automatically"
            );
        }
        let pasted = input
            .code
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or(input.callback_url.as_deref())
            .unwrap_or("");
        let client = required_http_client(ctx.http_client)?;
        self.import(pasted, &client).await
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow!("Kiro credential has no refresh token; import it again"))?;
        let client = required_http_client(ctx.http_client)?;
        let mut kiro = KiroCredential::from_stored(credential);
        let refreshed = refresh_tokens(&client, &kiro, refresh_token).await?;
        if let Some(arn) = refreshed.profile_arn.filter(|arn| !arn.trim().is_empty()) {
            kiro.profile_arn = Some(arn);
        }
        let access_token = refreshed.access_token.unwrap_or_default();
        if kiro.profile_arn.is_none() {
            kiro.profile_arn = discover_profile(&client, &kiro, &access_token).await;
        }
        let expires_at = Utc::now()
            + chrono::Duration::seconds(
                refreshed
                    .expires_in
                    .unwrap_or(DEFAULT_LIFETIME_SECONDS)
                    .max(60),
            );
        // Tokens rotate; a response without a new refresh token keeps the old one.
        let refresh_token = refreshed
            .refresh_token
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| refresh_token.to_string());
        Ok(bundle(
            &kiro,
            access_token,
            Some(refresh_token),
            Some(expires_at.to_rfc3339()),
        ))
    }

    fn bind_runtime(
        &self,
        _provider: &crate::db::models::Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let kiro = KiroCredential::from_stored(credential);
        // The native transport builds its own Bearer header; never also attach
        // the generic credential. The base URL follows the credential's region.
        Ok(RuntimeBinding {
            base_url_override: Some(format!("https://q.{}.amazonaws.com", kiro.region)),
            disable_default_auth: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    fn test_provider() -> crate::db::models::Provider {
        crate::db::models::Provider {
            keys: Vec::new(),
            id: "test".into(),
            name: "Kiro".into(),
            vendor: Some("kiro".into()),
            protocol: "kiro-chat/generate-assistant-response/v1".into(),
            base_url: "https://q.us-east-1.amazonaws.com".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("kiro".into()),
            channel: Some("ide".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn driver_is_a_manual_exchange_kiro_driver_with_aliases() {
        use crate::auth::{AuthCompletionMode, build_driver};
        let driver = build_driver("kiro").unwrap();
        assert_eq!(driver.metadata().key, "kiro");
        assert_eq!(driver.completion_mode(), AuthCompletionMode::ManualExchange);
        for alias in ["Kiro", "kiro-ide", "amazon-q", "codewhisperer"] {
            assert_eq!(crate::auth::normalize_driver_key(alias), "kiro", "{alias}");
        }
    }

    #[tokio::test]
    async fn start_creates_a_pending_paste_session_without_any_url() {
        let created = KiroAuthDriver
            .start(StartAuthContext::default())
            .await
            .unwrap();
        assert_eq!(created.status, "pending");
        assert_eq!(created.driver_key, "kiro");
        assert!(created.verification_uri_complete.is_none());
        assert!(created.state_json.is_none());
    }

    #[test]
    fn bind_runtime_follows_the_credential_region_and_disables_default_auth() {
        let credential = StoredCredential {
            meta: json!({"region":"eu-central-1"}),
            ..Default::default()
        };
        let binding = KiroAuthDriver
            .bind_runtime(&test_provider(), &credential)
            .unwrap();
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://q.eu-central-1.amazonaws.com")
        );
        assert!(binding.disable_default_auth);
    }

    #[test]
    fn expiry_parser_accepts_kiro_millisecond_iso_strings() {
        assert!(parse_expiry("2030-01-01T00:00:00.000Z").is_some());
        assert!(parse_expiry("2030-01-01T00:00:00+08:00").is_some());
        assert!(parse_expiry("yesterday").is_none());
    }

    #[test]
    fn fresh_tokens_import_without_any_network_call() {
        // `from_str` import only: a valid access token needs no refresh.
        let future = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let imported = parse_import(&format!(
            r#"{{"accessToken":"AT","refreshToken":"RT","expiresAt":"{future}","authMethod":"social",
                "profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/X"}}"#
        ))
        .unwrap();
        let at = imported
            .expires_at
            .as_deref()
            .and_then(parse_expiry)
            .unwrap();
        assert!(at > Utc::now() + chrono::Duration::seconds(IMPORT_MARGIN_SECONDS));
    }

    #[test]
    fn bundle_stores_client_secret_and_a_q_origin_resource_url() {
        let credential = KiroCredential {
            auth_method: crate::provider::kiro::credential::AuthMethod::BuilderId,
            region: "us-east-1".into(),
            idc_region: "us-east-1".into(),
            client_id: Some("cid".into()),
            client_secret: Some("secret".into()),
            profile_arn: None,
            start_url: None,
        };
        let bundle = bundle(&credential, "AT".into(), Some("RT".into()), None);
        // Builder ID without a profile generates through CodeWhisperer, but the
        // provider's base URL must stay on the Q origin.
        assert_eq!(
            bundle.resource_url.as_deref(),
            Some("https://q.us-east-1.amazonaws.com")
        );
        assert_eq!(bundle.raw["client_secret"], "secret");
        assert_eq!(bundle.raw["auth_method"], "builder-id");
        // The token columns, not the meta blob, carry the tokens.
        assert!(bundle.raw.get("access_token").is_none());
        assert!(bundle.raw.get("refresh_token").is_none());
    }

    #[tokio::test]
    async fn refresh_requires_a_refresh_token_and_a_client_for_oidc() {
        let none = StoredCredential::default();
        let error = KiroAuthDriver
            .refresh(&none, RefreshAuthContext::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no refresh token"));

        let missing_client = StoredCredential {
            refresh_token: Some("RT".into()),
            meta: json!({"auth_method":"builder-id"}),
            ..Default::default()
        };
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = KiroAuthDriver
            .refresh(
                &missing_client,
                RefreshAuthContext {
                    http_client: Some(http),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("clientId and clientSecret"));
    }

    #[test]
    fn refresh_requests_use_kiro_identity_and_keep_each_auth_protocol() {
        use crate::provider::kiro::credential::AuthMethod;

        let client = reqwest::Client::builder()
            .no_proxy()
            .user_agent("Nyro Kiro Gateway")
            .build()
            .unwrap();
        for method in [AuthMethod::Social, AuthMethod::BuilderId, AuthMethod::Idc] {
            let mut credential = KiroCredential::from_stored(&StoredCredential::default());
            credential.auth_method = method;
            credential.idc_region = "eu-north-1".into();
            credential.client_id = Some("CID".into());
            credential.client_secret = Some("SECRET".into());
            let request = refresh_request(&client, &credential, "REFRESH")
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(request.headers()["user-agent"], "KiroIDE");
            assert_eq!(request.headers()["accept"], "application/json");
            assert_eq!(request.headers()["content-type"], "application/json");
            assert_eq!(request.timeout(), Some(&HTTP_TIMEOUT));
            let body: Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            if method == AuthMethod::Social {
                assert_eq!(
                    request.url().as_str(),
                    "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
                );
                assert_eq!(body, json!({"refreshToken":"REFRESH"}));
            } else {
                assert_eq!(
                    request.url().as_str(),
                    "https://oidc.eu-north-1.amazonaws.com/token"
                );
                assert_eq!(
                    body,
                    json!({
                        "refreshToken":"REFRESH", "clientId":"CID",
                        "clientSecret":"SECRET", "grantType":"refresh_token"
                    })
                );
            }
        }
    }

    #[tokio::test]
    async fn pasting_nothing_is_rejected_without_a_request() {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let session = AuthSession {
            generation: 0,
            id: "s".into(),
            provider_id: None,
            driver_key: "kiro".into(),
            scheme: "oauth_auth_code_pkce".into(),
            status: "pending".into(),
            use_proxy: false,
            user_code: None,
            verification_uri: None,
            verification_uri_complete: None,
            state_json: None,
            context_json: None,
            result_json: None,
            expires_at: None,
            poll_interval_seconds: None,
            last_error: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let error = KiroAuthDriver
            .exchange(
                &session,
                AuthExchangeInput::default(),
                ExchangeAuthContext {
                    http_client: Some(http),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Paste"));
    }

    // The real endpoints are fixed HTTPS hosts, so these tests exercise the
    // response parsing helpers against a local server's own URLs.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<Value>>>);

    #[tokio::test]
    async fn token_response_parsing_and_error_mapping() {
        async fn ok(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
            seen.0.lock().unwrap().push(body);
            Json(json!({
                "accessToken": "NEW-AT", "refreshToken": "NEW-RT",
                "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/X", "expiresIn": 1800
            }))
        }
        let seen = Seen::default();
        let app = Router::new()
            .route("/refresh", post(ok))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = http
            .post(format!("http://{address}/refresh"))
            .json(&json!({"refreshToken":"RT"}))
            .send()
            .await
            .unwrap();
        let token: TokenResponse = read_json(response, "Kiro token").await.unwrap();
        server.abort();
        assert_eq!(token.access_token.as_deref(), Some("NEW-AT"));
        assert_eq!(token.refresh_token.as_deref(), Some("NEW-RT"));
        assert_eq!(token.expires_in, Some(1800));
        assert_eq!(seen.0.lock().unwrap()[0]["refreshToken"], "RT");
    }

    #[tokio::test]
    async fn oversized_and_invalid_bodies_are_rejected() {
        let app = Router::new()
            .route(
                "/big",
                axum::routing::get(|| async { "x".repeat(MAX_BODY + 10) }),
            )
            .route("/bad", axum::routing::get(|| async { "not json" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for path in ["big", "bad"] {
            let response = http
                .get(format!("http://{address}/{path}"))
                .send()
                .await
                .unwrap();
            assert!(
                read_json::<TokenResponse>(response, "Kiro token")
                    .await
                    .is_err(),
                "{path}"
            );
        }
        server.abort();
    }
}
