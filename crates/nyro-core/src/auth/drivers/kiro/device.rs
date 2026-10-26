//! Device login wire contracts (no browser redirect or local callback listener).
//!
//! AWS JSON APIs: https://docs.aws.amazon.com/singlesignon/latest/OIDCAPIReference/
//! API_RegisterClient.html, API_StartDeviceAuthorization.html, API_CreateToken.html.
//! Kiro social device fields are implementation evidence, not a public AWS API:
//! https://github.com/diegosouzapw/OmniRoute/issues/2471
//! https://github.com/diegosouzapw/OmniRoute/blob/main/src/app/api/oauth/kiro/social-authorize/route.ts
//! https://github.com/diegosouzapw/OmniRoute/blob/main/src/app/api/oauth/kiro/social-exchange/route.ts
//! https://github.com/diegosouzapw/OmniRoute/blob/main/src/lib/oauth/kiroSocialPoll.ts
//! Kiro confirms Google/GitHub/Builder ID/IdC device login at
//! https://kiro.dev/docs/getting-started/authentication/ .
use super::*;
use crate::provider::kiro::credential::{AuthMethod, DEFAULT_REGION, valid_region};
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde_json::Value;

const SOCIAL_ORIGIN: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
const SOCIAL_CLIENT_ID: &str = "kiro-cli";
// Client registration name used by the AIClient2API Kiro auth reference:
// https://github.com/justlovemaki/AIClient-2-API/blob/master/src/auth/kiro-oauth.js
// Only new registrations use this name; refreshing an old client cannot rename it.
const OIDC_CLIENT_NAME: &str = "Kiro IDE";
const BUILDER_START_URL: &str = "https://view.awsapps.com/start";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SCOPES: [&str; 3] = [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
];

// This state is server-only. In particular, never put deviceCode/clientSecret
// in AuthProgress, context_json, logs, error text, or a browser-facing DTO.
#[derive(Serialize, Deserialize)]
struct DeviceState {
    method: KiroLoginMethod,
    device_code: String,
    region: String,
    idc_region: String,
    start_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    client_secret_expires_at: Option<i64>,
}

impl DeviceState {
    fn is_social(&self) -> bool {
        matches!(
            self.method,
            KiroLoginMethod::Google | KiroLoginMethod::Github
        )
    }

    fn credential(&self) -> KiroCredential {
        KiroCredential {
            auth_method: match self.method {
                KiroLoginMethod::BuilderId => AuthMethod::BuilderId,
                KiroLoginMethod::Idc => AuthMethod::Idc,
                _ => AuthMethod::Social,
            },
            region: self.region.clone(),
            idc_region: self.idc_region.clone(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            profile_arn: None,
            start_url: self.start_url.clone(),
        }
    }
}

// Only built from validated regions in production. Private helpers allow local
// mock URLs in tests, without exposing an endpoint override in the admin API.
struct Endpoints {
    social: String,
    oidc: String,
}

impl Endpoints {
    fn production(region: &str) -> Result<Self> {
        validate_region(region)?;
        Ok(Self {
            social: SOCIAL_ORIGIN.into(),
            oidc: format!("https://oidc.{region}.amazonaws.com"),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    client_id: String,
    client_secret: String,
    client_secret_expires_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
    expires_in_milliseconds: Option<u64>,
    interval_in_milliseconds: Option<u64>,
}

fn validate_region(region: &str) -> Result<()> {
    if !valid_region(region)
        || !region
            .rsplit('-')
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        bail!("Invalid Kiro AWS region");
    }
    Ok(())
}

fn https_url(raw: &str) -> Result<Url> {
    if raw.len() > 2048 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("Invalid Kiro authorization URL");
    }
    let url = Url::parse(raw).map_err(|_| anyhow!("Invalid Kiro authorization URL"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.host_str().is_none()
    {
        bail!("Invalid Kiro authorization URL");
    }
    Ok(url)
}

fn aws_portal_host(host: &str) -> bool {
    host.ends_with(".awsapps.com")
}

fn validate_start_url(raw: &str) -> Result<String> {
    let url = https_url(raw)?;
    if !aws_portal_host(url.host_str().unwrap_or_default())
        || url.fragment().is_some()
        || url.query().is_some()
    {
        bail!("IAM Identity Center Start URL must be an HTTPS AWS access portal URL");
    }
    Ok(url.to_string())
}

fn validate_verification_url(raw: &str, social: bool) -> Result<String> {
    let url = https_url(raw)?;
    let host = url.host_str().unwrap_or_default();
    let allowed = if social {
        host == "app.kiro.dev"
    } else {
        let aws_device = host
            .strip_prefix("device.sso.")
            .and_then(|value| value.strip_suffix(".amazonaws.com"))
            .is_some_and(|region| validate_region(region).is_ok());
        // Alternate regional sign-in host documented by Kiro's firewall guide.
        let alternate_signin = host
            .strip_suffix(".signin.aws.amazon.com")
            .is_some_and(|region| validate_region(region).is_ok());
        aws_portal_host(host)
            || host == "signin.aws"
            || host.ends_with(".signin.aws")
            || aws_device
            || alternate_signin
    };
    if !allowed {
        bail!("Kiro returned an unsupported verification URL");
    }
    Ok(url.to_string())
}

fn nonempty_secret(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64 * 1024
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn login_state(options: &KiroLoginOptions) -> Result<DeviceState> {
    if options.method == KiroLoginMethod::Import {
        bail!("Credential import does not use device login");
    }
    let region = options.region.as_deref().unwrap_or(DEFAULT_REGION);
    let idc_region = options.idc_region.as_deref().unwrap_or(DEFAULT_REGION);
    validate_region(region)?;
    validate_region(idc_region)?;
    let start_url = match options.method {
        KiroLoginMethod::BuilderId => {
            if options.start_url.is_some() {
                bail!("Use IAM Identity Center for a custom Start URL");
            }
            Some(BUILDER_START_URL.into())
        }
        KiroLoginMethod::Idc => Some(validate_start_url(
            options
                .start_url
                .as_deref()
                .ok_or_else(|| anyhow!("IAM Identity Center requires a Start URL"))?,
        )?),
        _ => None,
    };
    Ok(DeviceState {
        method: options.method,
        device_code: String::new(),
        region: region.into(),
        idc_region: idc_region.into(),
        start_url,
        client_id: None,
        client_secret: None,
        client_secret_expires_at: None,
    })
}

async fn post_json(client: &reqwest::Client, url: &str, body: &Value) -> Result<reqwest::Response> {
    client
        .post(url)
        .timeout(HTTP_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .headers(crate::provider::kiro::identity::headers())
        .json(body)
        .send()
        .await
        .map_err(|_| anyhow!("Kiro authorization network is temporarily unavailable"))
}

async fn start_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    stage: &str,
) -> Result<T> {
    if !response.status().is_success() {
        // Do not expose error_description or body: upstream may echo secrets.
        bail!("Kiro {stage} failed (HTTP {})", response.status().as_u16());
    }
    read_json(response, "Kiro authorization").await
}

pub(super) async fn start(ctx: StartAuthContext) -> Result<CreateAuthSession> {
    let options = ctx.kiro.clone().unwrap_or_default();
    let state = login_state(&options)?;
    let endpoints = Endpoints::production(&state.idc_region)?;
    start_with(ctx, state, &endpoints).await
}

async fn start_with(
    ctx: StartAuthContext,
    mut state: DeviceState,
    endpoints: &Endpoints,
) -> Result<CreateAuthSession> {
    let client = required_http_client(ctx.http_client)?;
    let response = if state.is_social() {
        let provider = if state.method == KiroLoginMethod::Google {
            "Google"
        } else {
            "Github"
        };
        post_json(
            &client,
            &format!("{}/oauth/device/authorization", endpoints.social),
            &json!({"clientId":SOCIAL_CLIENT_ID,"loginProvider":provider}),
        )
        .await?
    } else {
        let response = post_json(
            &client,
            &format!("{}/client/register", endpoints.oidc),
            &json!({
                "clientName":OIDC_CLIENT_NAME, "clientType":"public",
                "scopes":SCOPES, "grantTypes":[DEVICE_GRANT,"refresh_token"]
            }),
        )
        .await?;
        let registration: Registration = start_response(response, "client registration").await?;
        if !nonempty_secret(&registration.client_id)
            || !nonempty_secret(&registration.client_secret)
        {
            bail!("Kiro returned an invalid client registration");
        }
        state.client_id = Some(registration.client_id);
        state.client_secret = Some(registration.client_secret);
        state.client_secret_expires_at = registration.client_secret_expires_at;
        post_json(
            &client,
            &format!("{}/device_authorization", endpoints.oidc),
            &json!({
                "clientId":state.client_id,"clientSecret":state.client_secret,
                "startUrl":state.start_url
            }),
        )
        .await?
    };
    let auth: DeviceAuthorization = start_response(response, "device authorization").await?;
    if !nonempty_secret(&auth.device_code)
        || !(4..=32).contains(&auth.user_code.len())
        || !auth
            .user_code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("Kiro returned an invalid device authorization");
    }
    let verification_uri = validate_verification_url(&auth.verification_uri, state.is_social())?;
    let verification_uri_complete = auth
        .verification_uri_complete
        .as_deref()
        .map(|url| validate_verification_url(url, state.is_social()))
        .transpose()?;
    let (expires, interval) = if state.is_social() {
        // Kiro's device start fields use milliseconds; token expiresIn uses seconds.
        let expires = auth
            .expires_in_milliseconds
            .ok_or_else(|| anyhow!("Kiro device expiry is missing"))?;
        let interval = auth.interval_in_milliseconds.unwrap_or(5000);
        (expires / 1000, interval.div_ceil(1000))
    } else {
        (
            auth.expires_in
                .ok_or_else(|| anyhow!("Kiro device expiry is missing"))?,
            auth.interval.unwrap_or(5),
        )
    };
    if expires == 0 || interval == 0 || interval > 3600 {
        bail!("Kiro returned invalid device timing");
    }
    state.device_code = auth.device_code;
    Ok(CreateAuthSession {
        provider_id: ctx.provider_id,
        driver_key: "kiro".into(),
        scheme: AuthScheme::OAuthDeviceCode.as_str().into(),
        status: "pending".into(),
        use_proxy: ctx.use_proxy,
        user_code: Some(auth.user_code),
        verification_uri: Some(verification_uri),
        verification_uri_complete,
        state_json: Some(serde_json::to_string(&state)?),
        context_json: Some(json!({"completion_mode":"poll"}).to_string()),
        result_json: None,
        expires_at: Some(expires_at_after(expires.min(300) as i64)),
        poll_interval_seconds: Some(interval as i32),
        last_error: None,
    })
}

fn pending(session: &AuthSession, backoff: bool) -> AuthPollState {
    let interval = session.poll_interval_seconds.unwrap_or(5).max(1);
    AuthPollState::Pending(AuthProgress {
        user_code: session.user_code.clone(),
        verification_uri: session.verification_uri.clone(),
        verification_uri_complete: session.verification_uri_complete.clone(),
        expires_at: session.expires_at.clone(),
        poll_interval_seconds: Some(if backoff {
            interval.saturating_add(5)
        } else {
            interval
        }),
    })
}

fn terminal(code: &str, message: &str) -> AuthPollState {
    AuthPollState::Error {
        code: code.into(),
        message: message.into(),
    }
}

fn rejected(code: &str) -> AuthPollState {
    match code {
        "access_denied" | "denied" => terminal(
            "access_denied",
            "Kiro authorization was denied. Start a new login to try again.",
        ),
        "expired_token" | "expired" => terminal(
            "expired_token",
            "Kiro authorization expired. Start a new login.",
        ),
        "invalid_client" | "unauthorized_client" => terminal(
            "invalid_client",
            "Kiro client registration is invalid or expired. Start a new login.",
        ),
        "invalid_grant" | "invalid_request" | "invalid_scope" | "unsupported_grant_type" => {
            terminal(
                "authorization_failed",
                "Kiro rejected the authorization request. Start a new login.",
            )
        }
        _ => terminal(
            "authorization_failed",
            "Kiro authorization failed. Start a new login.",
        ),
    }
}

pub(super) async fn poll(session: &AuthSession, ctx: RefreshAuthContext) -> Result<AuthPollState> {
    let state: DeviceState = serde_json::from_str(session.state_json.as_deref().unwrap_or(""))
        .map_err(|_| anyhow!("Kiro device session is invalid; start a new login"))?;
    let endpoints = Endpoints::production(&state.idc_region)?;
    let client = required_http_client(ctx.http_client)?;
    poll_with(session, &state, &client, &endpoints).await
}

async fn poll_with(
    session: &AuthSession,
    state: &DeviceState,
    client: &reqwest::Client,
    endpoints: &Endpoints,
) -> Result<AuthPollState> {
    if session.scheme != AuthScheme::OAuthDeviceCode.as_str()
        || state.method == KiroLoginMethod::Import
        || !nonempty_secret(&state.device_code)
    {
        bail!("Kiro session does not support device polling");
    }
    if session
        .expires_at
        .as_deref()
        .and_then(parse_expiry)
        .is_none_or(|at| at <= Utc::now())
    {
        return Ok(rejected("expired_token"));
    }
    let (url, body) = if state.is_social() {
        (
            format!("{}/oauth/device/poll", endpoints.social),
            json!({"deviceCode":state.device_code,"clientId":SOCIAL_CLIENT_ID}),
        )
    } else {
        if state
            .client_id
            .as_deref()
            .is_none_or(|v| !nonempty_secret(v))
            || state
                .client_secret
                .as_deref()
                .is_none_or(|v| !nonempty_secret(v))
        {
            bail!("Kiro device session is missing its client registration");
        }
        (
            format!("{}/token", endpoints.oidc),
            json!({
                "clientId":state.client_id,"clientSecret":state.client_secret,
                "deviceCode":state.device_code,"grantType":DEVICE_GRANT
            }),
        )
    };
    let response = match post_json(client, &url, &body).await {
        Ok(response) => response,
        Err(_) => return Ok(pending(session, true)),
    };
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Ok(pending(session, true));
    }
    let value: Value = match read_json(response, "Kiro device token").await {
        Ok(value) => value,
        Err(_) => {
            return Ok(terminal(
                "invalid_token_response",
                "Kiro returned an invalid authorization response.",
            ));
        }
    };
    // Social progress may use status rather than the AWS OAuth error field.
    // Never echo arbitrary server strings (or error_description) into admin output.
    if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
        match error.as_str() {
            Some("authorization_pending") => return Ok(pending(session, false)),
            Some("slow_down") => return Ok(pending(session, true)),
            Some(code) => return Ok(rejected(code)),
            None => return Ok(rejected("authorization_failed")),
        }
    }
    if let Some(progress) = value.get("status").filter(|v| !v.is_null()) {
        match progress.as_str() {
            Some("authorization_pending" | "pending") => return Ok(pending(session, false)),
            Some("slow_down") => return Ok(pending(session, true)),
            Some(
                code @ ("access_denied"
                | "denied"
                | "expired_token"
                | "expired"
                | "invalid_client"
                | "unauthorized_client"
                | "invalid_grant"
                | "invalid_request"
                | "invalid_scope"
                | "unsupported_grant_type"
                | "authorization_failed"
                | "error"
                | "failed"
                | "cancelled"),
            ) => {
                return Ok(rejected(code));
            }
            // The social API has no published success-status enum. Follow the
            // reference client: unknown string status alone is never success;
            // an HTTP 2xx and validated access/refresh token pair below are.
            Some(_) => {}
            None => {
                return Ok(terminal(
                    "invalid_token_response",
                    "Kiro returned an invalid authorization response.",
                ));
            }
        }
    }
    if !status.is_success() {
        return Ok(rejected("authorization_failed"));
    }
    let token: TokenResponse = match serde_json::from_value(value) {
        Ok(token) => token,
        Err(_) => {
            return Ok(terminal(
                "invalid_token_response",
                "Kiro returned an invalid token response.",
            ));
        }
    };
    let Some(access_token) = token.access_token.filter(|v| nonempty_secret(v)) else {
        return Ok(terminal(
            "invalid_token_response",
            "Kiro authorization returned no access token.",
        ));
    };
    let Some(refresh_token) = token.refresh_token.filter(|v| nonempty_secret(v)) else {
        return Ok(terminal(
            "invalid_token_response",
            "Kiro authorization returned no refresh token.",
        ));
    };
    let lifetime = token.expires_in.unwrap_or(DEFAULT_LIFETIME_SECONDS);
    if !(1..=31 * 86400).contains(&lifetime) {
        return Ok(terminal(
            "invalid_token_response",
            "Kiro returned an invalid token lifetime.",
        ));
    }
    let mut credential = state.credential();
    credential.profile_arn = token.profile_arn.filter(|arn| {
        arn.len() <= 512
            && arn.starts_with("arn:")
            && arn.contains(":codewhisperer:")
            && arn.contains(":profile/")
            && arn_region(arn).is_some()
            && !arn.chars().any(char::is_whitespace)
    });
    if let Some(region) = credential.profile_arn.as_deref().and_then(arn_region) {
        credential.region = region.into();
    }
    // Do not invent profile ARNs or borrow an unrelated locally installed CLI
    // credential. IdC profile lookup is optional, uses only this new token.
    if credential.auth_method == AuthMethod::Idc && credential.profile_arn.is_none() {
        credential.profile_arn = discover_profile(client, &credential, &access_token).await;
        if let Some(region) = credential.profile_arn.as_deref().and_then(arn_region) {
            credential.region = region.into();
        }
    }
    let mut result = bundle(
        &credential,
        access_token,
        Some(refresh_token),
        Some(expires_at_after(lifetime)),
    );
    if !state.is_social() {
        result.scopes = SCOPES.iter().map(|scope| (*scope).into()).collect();
    }
    if let Some(expires) = state.client_secret_expires_at {
        result.raw["client_secret_expires_at"] = json!(expires);
    }
    if state.is_social() {
        result.raw["provider"] = json!(if state.method == KiroLoginMethod::Google {
            "Google"
        } else {
            "Github"
        });
    }
    Ok(AuthPollState::Ready(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::Uri, response::IntoResponse};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Mock {
        responses: Arc<Mutex<VecDeque<(StatusCode, Value)>>>,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
    }

    async fn mock(
        State(mock): State<Mock>,
        uri: Uri,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        // Exercise the shared helper for registration, authorization and polling
        // across Google, GitHub, Builder ID and IAM Identity Center.
        assert_eq!(
            headers["user-agent"],
            crate::provider::kiro::identity::headers()["user-agent"]
        );
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(headers["content-type"], "application/json");
        mock.requests
            .lock()
            .unwrap()
            .push((uri.path().into(), body));
        let (status, value) = mock
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        (status, Json(value))
    }

    async fn server(
        responses: Vec<(StatusCode, Value)>,
    ) -> (Endpoints, Mock, tokio::task::JoinHandle<()>) {
        let mock_state = Mock {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Default::default(),
        };
        let app = Router::new().fallback(mock).with_state(mock_state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            Endpoints {
                social: origin.clone(),
                oidc: origin,
            },
            mock_state,
            task,
        )
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .user_agent("Nyro Kiro Gateway")
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    fn options(method: KiroLoginMethod) -> KiroLoginOptions {
        KiroLoginOptions {
            method,
            ..Default::default()
        }
    }

    fn context(options: KiroLoginOptions) -> StartAuthContext {
        StartAuthContext {
            kiro: Some(options),
            http_client: Some(client()),
            ..Default::default()
        }
    }

    fn session(created: CreateAuthSession) -> AuthSession {
        AuthSession {
            generation: 0,
            id: "test-session".into(),
            provider_id: created.provider_id,
            driver_key: created.driver_key,
            scheme: created.scheme,
            status: created.status,
            use_proxy: created.use_proxy,
            user_code: created.user_code,
            verification_uri: created.verification_uri,
            verification_uri_complete: created.verification_uri_complete,
            state_json: created.state_json,
            context_json: created.context_json,
            result_json: None,
            expires_at: created.expires_at,
            poll_interval_seconds: created.poll_interval_seconds,
            last_error: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn authorized(social: bool) -> Value {
        let mut value = json!({
            "deviceCode":"PRIVATE-DEVICE-CODE", "userCode":"ABCD-EFGH",
            "verificationUri":if social {"https://app.kiro.dev/account/device"} else {"https://device.sso.us-east-1.amazonaws.com/"}
        });
        if social {
            value["expiresInMilliseconds"] = json!(120000);
            value["intervalInMilliseconds"] = json!(5500);
            value["verificationUriComplete"] = json!(
                "https://app.kiro.dev/account/device?user_code=ABCD-EFGH&login_provider=Google"
            );
        } else {
            value["expiresIn"] = json!(600);
            value["interval"] = json!(5);
            value["verificationUriComplete"] =
                json!("https://view.awsapps.com/start/#/device?user_code=ABCD-EFGH");
        }
        value
    }

    fn tokens() -> Value {
        json!({"accessToken":"ACCESS", "refreshToken":"REFRESH", "expiresIn":1800,
            "profileArn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/test"})
    }

    #[tokio::test]
    async fn social_start_pending_slow_down_then_ready_keeps_private_state() {
        for method in [KiroLoginMethod::Google, KiroLoginMethod::Github] {
            let (endpoints, seen, task) = server(vec![
                (StatusCode::OK, authorized(true)),
                (StatusCode::OK, json!({"status":"authorization_pending", "accessToken":null,"refreshToken":null,"profileArn":null,"identityProvider":null})),
                (StatusCode::BAD_REQUEST, json!({"error":"slow_down"})),
                (StatusCode::BAD_REQUEST, json!({"error":"slow_down"})),
                (StatusCode::OK, tokens()),
            ])
            .await;
            let opts = options(method);
            let created = start_with(
                context(opts.clone()),
                login_state(&opts).unwrap(),
                &endpoints,
            )
            .await
            .unwrap();
            assert_eq!(created.scheme, "oauth_device_code");
            assert_eq!(created.poll_interval_seconds, Some(6)); // round up milliseconds
            assert_eq!(
                created.context_json.as_deref(),
                Some("{\"completion_mode\":\"poll\"}")
            );
            let remaining =
                parse_expiry(created.expires_at.as_deref().unwrap()).unwrap() - Utc::now();
            assert!(remaining.num_seconds() <= 120 && remaining.num_seconds() >= 118);
            let mut session = session(created);
            let state: DeviceState =
                serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
            assert_eq!(state.device_code, "PRIVATE-DEVICE-CODE");
            assert!(state.client_secret.is_none());
            for expected_interval in [6, 11, 16] {
                let progress = poll_with(&session, &state, &client(), &endpoints)
                    .await
                    .unwrap();
                let AuthPollState::Pending(progress) = progress else {
                    panic!("expected pending")
                };
                assert_eq!(progress.poll_interval_seconds, Some(expected_interval));
                assert!(
                    !serde_json::to_string(&progress)
                        .unwrap()
                        .contains("PRIVATE-DEVICE-CODE")
                );
                session.poll_interval_seconds = progress.poll_interval_seconds;
            }
            let ready = poll_with(&session, &state, &client(), &endpoints)
                .await
                .unwrap();
            let AuthPollState::Ready(ready) = ready else {
                panic!("expected ready")
            };
            assert_eq!(ready.access_token.as_deref(), Some("ACCESS"));
            assert_eq!(ready.refresh_token.as_deref(), Some("REFRESH"));
            assert_eq!(ready.raw["auth_method"], "social");
            assert!(ready.raw.get("client_secret").is_none());
            let requests = seen.requests.lock().unwrap();
            assert_eq!(requests[0].0, "/oauth/device/authorization");
            assert_eq!(requests[0].1["clientId"], "kiro-cli");
            assert!(requests[0].1.get("clientName").is_none());
            assert_eq!(
                requests[0].1["loginProvider"],
                if method == KiroLoginMethod::Google {
                    "Google"
                } else {
                    "Github"
                }
            );
            assert_eq!(requests[1].0, "/oauth/device/poll");
            assert_eq!(
                requests[1].1,
                json!({"deviceCode":"PRIVATE-DEVICE-CODE","clientId":"kiro-cli"})
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn oidc_registration_device_grant_and_metadata_match_aws_wire() {
        let (endpoints, seen, task) = server(vec![
            (StatusCode::OK, json!({"clientId":"CID", "clientSecret":"SECRET", "clientSecretExpiresAt":2000000000})),
            (StatusCode::OK, authorized(false)),
            (StatusCode::BAD_REQUEST, json!({"error":"authorization_pending"})),
            (StatusCode::OK, tokens()),
        ]).await;
        let opts = options(KiroLoginMethod::BuilderId);
        let created = start_with(
            context(opts.clone()),
            login_state(&opts).unwrap(),
            &endpoints,
        )
        .await
        .unwrap();
        assert!(
            parse_expiry(created.expires_at.as_deref().unwrap()).unwrap()
                <= Utc::now() + chrono::Duration::seconds(300)
        );
        let session = session(created);
        let state: DeviceState =
            serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
        assert!(matches!(
            poll_with(&session, &state, &client(), &endpoints)
                .await
                .unwrap(),
            AuthPollState::Pending(_)
        ));
        let AuthPollState::Ready(ready) = poll_with(&session, &state, &client(), &endpoints)
            .await
            .unwrap()
        else {
            panic!("expected ready")
        };
        assert_eq!(ready.raw["auth_method"], "builder-id");
        assert_eq!(ready.raw["client_id"], "CID");
        assert_eq!(ready.raw["client_secret"], "SECRET");
        assert_eq!(ready.raw["client_secret_expires_at"], 2000000000_i64);
        assert_eq!(ready.raw["start_url"], BUILDER_START_URL);
        assert_eq!(ready.scopes, SCOPES);
        let requests = seen.requests.lock().unwrap();
        assert_eq!(requests[0].0, "/client/register");
        assert_eq!(requests[0].1["clientName"], "Kiro IDE");
        assert_eq!(requests[0].1["clientType"], "public");
        assert_eq!(requests[0].1["scopes"], json!(SCOPES));
        assert_eq!(
            requests[0].1["grantTypes"],
            json!([DEVICE_GRANT, "refresh_token"])
        );
        assert!(requests[0].1.get("issuerUrl").is_none());
        assert_eq!(requests[1].0, "/device_authorization");
        assert_eq!(
            requests[1].1,
            json!({"clientId":"CID","clientSecret":"SECRET","startUrl":BUILDER_START_URL})
        );
        assert_eq!(requests[2].0, "/token");
        assert_eq!(
            requests[2].1,
            json!({"clientId":"CID","clientSecret":"SECRET","deviceCode":"PRIVATE-DEVICE-CODE","grantType":DEVICE_GRANT})
        );
        task.abort();
    }

    #[tokio::test]
    async fn idc_preserves_directory_region_separate_from_profile_region() {
        let (endpoints, seen, task) = server(vec![
            (
                StatusCode::OK,
                json!({"clientId":"CID","clientSecret":"SECRET"}),
            ),
            (StatusCode::OK, authorized(false)),
            (StatusCode::OK, tokens()),
        ])
        .await;
        let opts = KiroLoginOptions {
            method: KiroLoginMethod::Idc,
            region: Some("us-east-1".into()),
            idc_region: Some("eu-north-1".into()),
            start_url: Some("https://company.awsapps.com/start".into()),
        };
        let created = start_with(
            context(opts.clone()),
            login_state(&opts).unwrap(),
            &endpoints,
        )
        .await
        .unwrap();
        let session = session(created);
        let state: DeviceState =
            serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
        let AuthPollState::Ready(ready) = poll_with(&session, &state, &client(), &endpoints)
            .await
            .unwrap()
        else {
            panic!("expected ready")
        };
        assert_eq!(ready.raw["auth_method"], "idc");
        assert_eq!(ready.raw["idc_region"], "eu-north-1");
        assert_eq!(ready.raw["region"], "us-east-1");
        let requests = seen.requests.lock().unwrap();
        assert_eq!(requests[0].0, "/client/register");
        assert_eq!(requests[0].1["clientName"], "Kiro IDE");
        assert_eq!(
            requests[1].1["startUrl"],
            "https://company.awsapps.com/start"
        );
        task.abort();
    }

    #[tokio::test]
    async fn denied_expired_malformed_and_unknown_statuses_never_create_credentials() {
        for (status, value, expected) in [
            (
                StatusCode::BAD_REQUEST,
                json!({"error":"access_denied","error_description":"SECRET"}),
                "access_denied",
            ),
            (
                StatusCode::BAD_REQUEST,
                json!({"error":"expired_token"}),
                "expired_token",
            ),
            (
                StatusCode::UNAUTHORIZED,
                json!({"error":"invalid_client"}),
                "invalid_client",
            ),
            (
                StatusCode::OK,
                json!({"status":"denied","accessToken":"SECRET","refreshToken":"SECRET"}),
                "access_denied",
            ),
            (
                StatusCode::OK,
                json!({"status":"unexpected-SECRET"}),
                "invalid_token_response",
            ),
            (
                StatusCode::OK,
                json!({"error":"SECRET"}),
                "authorization_failed",
            ),
            (StatusCode::OK, json!({}), "invalid_token_response"),
            (
                StatusCode::OK,
                json!({"accessToken":"SECRET"}),
                "invalid_token_response",
            ),
            (
                StatusCode::OK,
                json!({"accessToken":12,"refreshToken":"SECRET"}),
                "invalid_token_response",
            ),
        ] {
            let (endpoints, _, task) =
                server(vec![(StatusCode::OK, authorized(true)), (status, value)]).await;
            let opts = options(KiroLoginMethod::Google);
            let created = start_with(
                context(opts.clone()),
                login_state(&opts).unwrap(),
                &endpoints,
            )
            .await
            .unwrap();
            let session = session(created);
            let state: DeviceState =
                serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
            let result = poll_with(&session, &state, &client(), &endpoints)
                .await
                .unwrap();
            let AuthPollState::Error { code, message } = result else {
                panic!("must not authorize")
            };
            assert_eq!(code, expected);
            assert!(!message.contains("SECRET"));
            task.abort();
        }
    }

    #[tokio::test]
    async fn transient_responses_back_off_and_expired_session_never_polls() {
        let (endpoints, seen, task) = server(vec![
            (StatusCode::OK, authorized(true)),
            (StatusCode::TOO_MANY_REQUESTS, json!({"message":"SECRET"})),
            (StatusCode::SERVICE_UNAVAILABLE, json!({})),
        ])
        .await;
        let opts = options(KiroLoginMethod::Google);
        let created = start_with(
            context(opts.clone()),
            login_state(&opts).unwrap(),
            &endpoints,
        )
        .await
        .unwrap();
        let mut session = session(created);
        let state: DeviceState =
            serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
        for expected in [11, 16] {
            let AuthPollState::Pending(progress) =
                poll_with(&session, &state, &client(), &endpoints)
                    .await
                    .unwrap()
            else {
                panic!("expected transient pending")
            };
            assert_eq!(progress.poll_interval_seconds, Some(expected));
            session.poll_interval_seconds = progress.poll_interval_seconds;
        }
        session.expires_at = Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        assert!(
            matches!(poll_with(&session, &state, &client(), &endpoints).await.unwrap(), AuthPollState::Error { code, .. } if code == "expired_token")
        );
        assert_eq!(seen.requests.lock().unwrap().len(), 3);
        task.abort();
    }

    #[tokio::test]
    async fn verification_uri_fallback_works_and_untrusted_urls_are_rejected() {
        let mut fallback = authorized(true);
        fallback
            .as_object_mut()
            .unwrap()
            .remove("verificationUriComplete");
        let (endpoints, _, task) = server(vec![(StatusCode::OK, fallback)]).await;
        let opts = options(KiroLoginMethod::Google);
        let created = start_with(
            context(opts.clone()),
            login_state(&opts).unwrap(),
            &endpoints,
        )
        .await
        .unwrap();
        assert!(created.verification_uri_complete.is_none());
        assert_eq!(
            created.verification_uri.as_deref(),
            Some("https://app.kiro.dev/account/device")
        );
        task.abort();
        for url in [
            "http://app.kiro.dev/account/device",
            "https://app.kiro.dev.evil.test/",
            "https://app.kiro.dev@evil.test/",
            "https://127.0.0.1/",
            "https://app.kiro.dev:8443/",
            "javascript:alert(1)",
        ] {
            let mut value = authorized(true);
            value["verificationUriComplete"] = json!(url);
            let (endpoints, _, task) = server(vec![(StatusCode::OK, value)]).await;
            assert!(
                start_with(
                    context(opts.clone()),
                    login_state(&opts).unwrap(),
                    &endpoints
                )
                .await
                .is_err()
            );
            task.abort();
        }
    }

    #[test]
    fn options_validate_regions_and_start_urls_before_network() {
        for region in [
            "evil.test",
            "us-east-1/evil",
            "us-east-1@evil",
            "127.0.0.1",
            "",
        ] {
            let opts = KiroLoginOptions {
                idc_region: Some(region.into()),
                ..options(KiroLoginMethod::BuilderId)
            };
            assert!(login_state(&opts).is_err());
        }
        for url in [
            "https://evil.test/start",
            "http://company.awsapps.com/start",
            "https://company.awsapps.com.evil.test/start",
            "https://company.awsapps.com:8000/start",
            "https://company.awsapps.com/start?redirect=evil",
            "https://SECRET@company.awsapps.com/start",
        ] {
            let opts = KiroLoginOptions {
                start_url: Some(url.into()),
                ..options(KiroLoginMethod::Idc)
            };
            let error = login_state(&opts).err().unwrap().to_string();
            assert!(!error.contains("SECRET"));
        }
        assert!(login_state(&options(KiroLoginMethod::Idc)).is_err());
        for region in ["us-east-1", "eu-central-1", "us-gov-west-1"] {
            assert!(Endpoints::production(region).is_ok());
        }
        for url in [
            "https://view.awsapps.com/start",
            "https://start.us-gov-home.awsapps.com/directory/d-123",
        ] {
            assert!(validate_start_url(url).is_ok());
        }
        for url in [
            "https://us-east-1.signin.aws/platform/d-123/#/device",
            "https://eu-central-1.signin.aws.amazon.com/platform/d-123/#/device",
        ] {
            assert!(validate_verification_url(url, false).is_ok());
        }
        assert!(
            validate_verification_url(
                "https://eu-central-1.signin.aws.amazon.com.evil.test/device",
                false
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn network_disconnect_is_pending_and_non_json_or_oversized_poll_is_terminal() {
        let (mut endpoints, _, task) = server(vec![(StatusCode::OK, authorized(true))]).await;
        let opts = options(KiroLoginMethod::Google);
        let created = start_with(
            context(opts.clone()),
            login_state(&opts).unwrap(),
            &endpoints,
        )
        .await
        .unwrap();
        let session = session(created);
        let state: DeviceState =
            serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
        task.abort();
        let _ = task.await;
        let AuthPollState::Pending(progress) = poll_with(&session, &state, &client(), &endpoints)
            .await
            .unwrap()
        else {
            panic!("network errors must be retryable")
        };
        assert_eq!(progress.poll_interval_seconds, Some(11));

        for body in ["not json".to_string(), "x".repeat(MAX_BODY + 10)] {
            let app = Router::new().fallback(move || {
                let body = body.clone();
                async move { body }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            endpoints.social = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            assert!(
                matches!(poll_with(&session, &state, &client(), &endpoints).await.unwrap(), AuthPollState::Error { code, .. } if code == "invalid_token_response")
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn social_success_status_is_not_assumed_but_valid_tokens_are_required() {
        for status in [
            "authorized",
            "approved",
            "complete",
            "future-success-status",
        ] {
            let mut success = tokens();
            success["status"] = json!(status);
            let (endpoints, _, task) = server(vec![
                (StatusCode::OK, authorized(true)),
                (StatusCode::OK, success),
            ])
            .await;
            let opts = options(KiroLoginMethod::Google);
            let created = start_with(
                context(opts.clone()),
                login_state(&opts).unwrap(),
                &endpoints,
            )
            .await
            .unwrap();
            let session = session(created);
            let state: DeviceState =
                serde_json::from_str(session.state_json.as_deref().unwrap()).unwrap();
            assert!(matches!(
                poll_with(&session, &state, &client(), &endpoints)
                    .await
                    .unwrap(),
                AuthPollState::Ready(_)
            ));
            task.abort();
        }
    }

    #[tokio::test]
    async fn start_errors_are_sanitized_and_missing_timing_is_not_assumed() {
        for (status, body) in [
            (
                StatusCode::BAD_REQUEST,
                json!({"error":"invalid_request","error_description":"SECRET"}),
            ),
            (
                StatusCode::OK,
                json!({"deviceCode":"SECRET","userCode":"ABCD-EFGH","verificationUri":"https://app.kiro.dev/account/device"}),
            ),
        ] {
            let (endpoints, _, task) = server(vec![(status, body)]).await;
            let opts = options(KiroLoginMethod::Google);
            let error = start_with(
                context(opts.clone()),
                login_state(&opts).unwrap(),
                &endpoints,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(!error.contains("SECRET"));
            task.abort();
        }
    }
}
