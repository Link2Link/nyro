//! Cursor CLI PKCE login. Wire contract independently implemented from the
//! MIT-licensed oh-my-pi Cursor OAuth reference; no browser credentials are read.
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::shared::{
    build_authorize_url, expires_at_after, generate_code_challenge, generate_code_verifier,
};
use crate::auth::types::*;
use crate::db::models::Provider;

const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
const REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";
pub const CURSOR_API_URL: &str = "https://api2.cursor.sh";
pub const REFRESH_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct CursorAuthDriver;

/// Deliberately never include a response body, bearer, verifier, or request URL.
#[derive(Debug, Clone)]
pub enum CursorAuthError {
    Transient { retry_after: Option<u64> },
    Reauthenticate,
}
impl std::fmt::Display for CursorAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient { .. } => f.write_str("Cursor authentication temporarily unavailable; retry later"),
            Self::Reauthenticate => f.write_str("Cursor credential is invalid or refresh outcome is unknown; reauthentication required"),
        }
    }
}
impl std::error::Error for CursorAuthError {}

#[derive(Serialize, Deserialize)]
struct CursorLoginState {
    uuid: String,
    verifier: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

fn invalid() -> anyhow::Error {
    CursorAuthError::Reauthenticate.into()
}

/// Claims are shape/expiry checks on a token received over trusted TLS, not a
/// substitute for upstream signature validation. Never accept caller JWTs as login.
pub fn validate_cursor_access_token(token: &str) -> Result<(String, String)> {
    if token.is_empty()
        || token.len() > 32768
        || token
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
    {
        return Err(invalid());
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(invalid());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .map_err(|_| invalid())?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .filter(|exp| *exp > Utc::now().timestamp())
        .ok_or_else(invalid)?;
    let expiry = DateTime::<Utc>::from_timestamp(exp, 0)
        .ok_or_else(invalid)?
        .to_rfc3339();
    let sub = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)?;
    let subject = sub.split_once('|').map_or(sub, |(_, user)| user).trim();
    if subject.is_empty() || subject.len() > 512 || subject.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok((subject.to_string(), expiry))
}

pub fn validate_cursor_bundle(
    bundle: &CredentialBundle,
    expected_subject: Option<&str>,
) -> Result<()> {
    let (subject, expiry) =
        validate_cursor_access_token(bundle.access_token.as_deref().ok_or_else(invalid)?)?;
    if bundle.subject_id.as_deref() != Some(subject.as_str())
        || bundle.expires_at.as_deref() != Some(expiry.as_str())
        || expected_subject.is_some_and(|expected| expected != subject)
    {
        return Err(invalid());
    }
    Ok(())
}

fn normalize_tokens(body: &[u8], previous: Option<&StoredCredential>) -> Result<CredentialBundle> {
    let response: CursorTokenResponse = serde_json::from_slice(body).map_err(|_| invalid())?;
    let (subject, expiry) = validate_cursor_access_token(&response.access_token)?;
    if previous
        .and_then(|c| c.subject_id.as_deref())
        .is_some_and(|old| old != subject)
    {
        return Err(invalid());
    }
    let refresh_token = response
        .refresh_token
        .filter(|s| !s.trim().is_empty())
        .or_else(|| previous.and_then(|c| c.refresh_token.clone()));
    if refresh_token.as_deref().is_some_and(|s| {
        s.bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
    }) {
        return Err(invalid());
    }
    let mut meta = previous
        .and_then(|c| c.meta.as_object())
        .cloned()
        .unwrap_or_default();
    meta.remove("cursor_refresh_failures");
    meta.remove("cursor_refresh_retry_at");
    // Only explicitly selected non-secret response fields are ever persisted.
    meta.insert("sub".to_string(), json!(subject));
    Ok(CredentialBundle {
        access_token: Some(response.access_token),
        refresh_token,
        expires_at: Some(expiry),
        subject_id: Some(subject),
        resource_url: Some(CURSOR_API_URL.to_string()),
        scopes: previous.map(|c| c.scopes.clone()).unwrap_or_default(),
        raw: Value::Object(meta),
    })
}

async fn read_token_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    const LIMIT: usize = 64 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > LIMIT as u64)
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
        if bytes.len().saturating_add(chunk.len()) > LIMIT {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn retry_after(response: &reqwest::Response) -> Option<u64> {
    let raw = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    raw.parse().ok().or_else(|| {
        DateTime::parse_from_rfc2822(raw)
            .ok()
            .map(|time| (time.with_timezone(&Utc) - Utc::now()).num_seconds().max(0) as u64)
    })
}

fn pending(session: &AuthSession, retry: Option<u64>) -> AuthPollState {
    let remaining = session
        .expires_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|expiry| {
            (expiry.with_timezone(&Utc) - Utc::now())
                .num_seconds()
                .max(0) as u64
        })
        .unwrap_or(0);
    // Ordinary polling backs off 2 -> 3 -> 4 -> 5 seconds. Retry-After can
    // exceed five seconds, but never extends the five-minute login deadline.
    let delay = retry
        .unwrap_or((session.poll_interval_seconds.unwrap_or(1) + 1).clamp(2, 5) as u64)
        .max(2)
        .min(remaining);
    AuthPollState::Pending(AuthProgress {
        user_code: None,
        verification_uri: None,
        verification_uri_complete: None,
        expires_at: None,
        poll_interval_seconds: Some(delay as i32),
    })
}

impl CursorAuthDriver {
    async fn poll_at(
        &self,
        session: &AuthSession,
        client: reqwest::Client,
        endpoint: &str,
    ) -> Result<AuthPollState> {
        let state: CursorLoginState = session
            .state_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or_else(invalid)?;
        let remaining = session
            .expires_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|time| {
                (time.with_timezone(&Utc) - Utc::now())
                    .to_std()
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        if remaining.is_zero() {
            return Ok(AuthPollState::Error {
                code: "AUTH_TIMEOUT".into(),
                message: "Cursor authentication timed out".into(),
            });
        }
        let request = async {
            let response = match client
                .get(endpoint)
                .query(&[("uuid", &state.uuid), ("verifier", &state.verifier)])
                .send()
                .await
            {
                Ok(response) => response,
                Err(_) => return Ok(pending(session, None)),
            };
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::ACCEPTED {
                return Ok(pending(session, None));
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Ok(pending(session, retry_after(&response)));
            }
            if status.is_server_error() {
                return Ok(pending(session, None));
            }
            if !status.is_success() {
                return Err(invalid());
            }
            let bytes = read_token_body(response).await?;
            Ok(AuthPollState::Ready(normalize_tokens(&bytes, None)?))
        };
        tokio::time::timeout(remaining.min(Duration::from_secs(15)), request)
            .await
            .unwrap_or_else(|_| Ok(pending(session, None)))
    }

    async fn refresh_at(
        &self,
        credential: &StoredCredential,
        client: reqwest::Client,
        endpoint: &str,
    ) -> Result<CredentialBundle> {
        let refresh = credential
            .refresh_token
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(invalid)?;
        if refresh.len() > 32768
            || refresh
                .bytes()
                .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
        {
            return Err(invalid());
        }
        let operation = async {
            let response = client
                .post(endpoint)
                .bearer_auth(refresh)
                .json(&json!({}))
                .send()
                .await
                .map_err(|e| {
                    if e.is_connect() {
                        anyhow!(CursorAuthError::Transient { retry_after: None })
                    } else {
                        invalid()
                    }
                })?;
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                return Err(CursorAuthError::Transient {
                    retry_after: retry_after(&response),
                }
                .into());
            }
            if !status.is_success() {
                return Err(invalid());
            }
            let bytes = read_token_body(response).await?;
            normalize_tokens(&bytes, Some(credential))
        };
        // This includes body decoding, and is shorter than the 60-second CAS lease.
        tokio::time::timeout(REFRESH_DEADLINE, operation)
            .await
            .map_err(|_| invalid())?
    }
}

#[async_trait]
impl AuthDriver for CursorAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "cursor",
            label: "Cursor",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }
    fn completion_mode(&self) -> AuthCompletionMode {
        AuthCompletionMode::Poll
    }
    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);
        let uuid = uuid::Uuid::new_v4().to_string();
        let url = build_authorize_url(
            LOGIN_URL,
            &[
                ("challenge", &challenge),
                ("uuid", &uuid),
                ("mode", "login"),
                ("redirectTarget", "cli"),
            ],
        )?;
        Ok(CreateAuthSession {
            provider_id: ctx.provider_id,
            driver_key: "cursor".into(),
            scheme: AuthScheme::OAuthAuthCodePkce.as_str().into(),
            status: "pending".into(),
            use_proxy: ctx.use_proxy,
            user_code: None,
            verification_uri: Some(LOGIN_URL.into()),
            verification_uri_complete: Some(url),
            state_json: Some(serde_json::to_string(&CursorLoginState { uuid, verifier })?),
            context_json: None,
            result_json: None,
            expires_at: Some(expires_at_after(300)),
            poll_interval_seconds: Some(2),
            last_error: None,
        })
    }
    async fn poll(&self, session: &AuthSession, ctx: RefreshAuthContext) -> Result<AuthPollState> {
        self.poll_at(session, ctx.http_client.ok_or_else(invalid)?, POLL_URL)
            .await
    }
    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        self.refresh_at(
            credential,
            ctx.http_client.ok_or_else(invalid)?,
            REFRESH_URL,
        )
        .await
    }
    fn bind_runtime(
        &self,
        _provider: &Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let (subject, _) =
            validate_cursor_access_token(credential.access_token.as_deref().ok_or_else(invalid)?)?;
        if credential.subject_id.as_deref() != Some(subject.as_str()) {
            return Err(invalid());
        }
        Ok(RuntimeBinding {
            base_url_override: Some(CURSOR_API_URL.into()),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn token(exp: i64, sub: &str) -> String {
        format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&json!({"exp":exp,"sub":sub})).unwrap())
        )
    }
    #[tokio::test]
    async fn cursor_start_keeps_verifier_server_side() {
        let created = CursorAuthDriver
            .start(StartAuthContext::default())
            .await
            .unwrap();
        let state: CursorLoginState =
            serde_json::from_str(created.state_json.as_deref().unwrap()).unwrap();
        let url =
            reqwest::Url::parse(created.verification_uri_complete.as_deref().unwrap()).unwrap();
        assert_eq!(url.origin().ascii_serialization(), "https://cursor.com");
        assert_eq!(url.path(), "/loginDeepControl");
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs["challenge"], generate_code_challenge(&state.verifier));
        assert_eq!(pairs["mode"], "login");
        assert_eq!(pairs["redirectTarget"], "cli");
        assert!(!url.as_str().contains(&state.verifier));
        assert_eq!(CursorAuthDriver.completion_mode(), AuthCompletionMode::Poll);
    }
    #[test]
    fn cursor_validation_rejects_bad_expiry_subject_and_token() {
        for t in [
            "secret".to_string(),
            token(0, "user"),
            token(Utc::now().timestamp() + 100, ""),
            token(Utc::now().timestamp() + 100, "auth0|"),
        ] {
            let error = validate_cursor_access_token(&t).unwrap_err().to_string();
            assert!(!error.contains(&t));
        }
        assert_eq!(
            validate_cursor_access_token(&token(Utc::now().timestamp() + 100, "auth0|user"))
                .unwrap()
                .0,
            "user"
        );
    }
    #[test]
    fn cursor_refresh_preserves_refresh_and_metadata_but_not_tokens_in_meta() {
        let old = StoredCredential {
            refresh_token: Some("existing-refresh".into()),
            subject_id: Some("user".into()),
            meta: json!({"label":"keep", "cursor_refresh_failures":3}),
            ..Default::default()
        };
        let access = token(Utc::now().timestamp() + 3600, "auth0|user");
        let bundle = normalize_tokens(
            &serde_json::to_vec(
                &json!({"accessToken":access, "refreshToken":"", "secret":"never-copy"}),
            )
            .unwrap(),
            Some(&old),
        )
        .unwrap();
        assert_eq!(bundle.refresh_token.as_deref(), Some("existing-refresh"));
        assert_eq!(bundle.raw["label"], "keep");
        assert!(bundle.raw.get("secret").is_none());
        assert!(bundle.raw.get("cursor_refresh_failures").is_none());
        assert!(
            normalize_tokens(
                &serde_json::to_vec(
                    &json!({"accessToken":token(Utc::now().timestamp()+3600,"other")})
                )
                .unwrap(),
                Some(&old)
            )
            .is_err()
        );
    }

    async fn response_server(
        status: u16,
        body: String,
        retry: Option<&str>,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<(http::Method, http::Uri, http::HeaderMap, String)>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router,
            body::{Body, to_bytes},
            extract::Request,
            response::Response,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sender = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));
        let retry = retry.map(str::to_string);
        let app = Router::new().fallback(move |request: Request| {
            let (sender, body, retry) = (sender.clone(), body.clone(), retry.clone());
            async move {
                let (parts, input) = request.into_parts();
                let input = to_bytes(input, 1024 * 64).await.unwrap();
                if let Some(tx) = sender.lock().await.take() {
                    let _ = tx.send((
                        parts.method,
                        parts.uri,
                        parts.headers,
                        String::from_utf8(input.to_vec()).unwrap(),
                    ));
                }
                let mut response = Response::builder().status(status);
                if let Some(retry) = retry {
                    response = response.header("retry-after", retry);
                }
                response.body(Body::from(body)).unwrap()
            }
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), rx, task)
    }

    async fn test_session() -> AuthSession {
        let created = CursorAuthDriver
            .start(StartAuthContext::default())
            .await
            .unwrap();
        // Create/session share their serialized wire fields; process controls default.
        let mut value = serde_json::to_value(created).unwrap();
        value["id"] = json!("fixture");
        value["created_at"] = json!(Utc::now().to_rfc3339());
        value["updated_at"] = value["created_at"].clone();
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn cursor_poll_statuses_backoff_and_retry_after_respect_deadline() {
        let mut session = test_session().await;
        for (status, retry, expected) in [
            (404, None, 3),
            (202, None, 4),
            (404, None, 5),
            (429, Some("12"), 12),
            (429, Some("99999"), 299),
        ] {
            let (url, request, task) =
                response_server(status, "upstream secret should not escape".into(), retry).await;
            let state = CursorAuthDriver
                .poll_at(&session, reqwest::Client::new(), &url)
                .await
                .unwrap();
            let (method, uri, _, _) = request.await.unwrap();
            assert_eq!(method, http::Method::GET);
            assert!(uri.query().unwrap().contains("verifier="));
            let AuthPollState::Pending(progress) = state else {
                panic!("expected pending");
            };
            let interval = progress.poll_interval_seconds.unwrap();
            if expected == 299 {
                assert!((295..=300).contains(&interval));
            } else {
                assert_eq!(interval, expected);
            }
            assert!(progress.expires_at.is_none());
            session.poll_interval_seconds = Some(interval);
            task.abort();
            let _ = task.await;
        }
    }

    #[tokio::test]
    async fn cursor_refresh_contract_and_redacted_failure_classes() {
        let credential = StoredCredential {
            refresh_token: Some("test-refresh".into()),
            subject_id: Some("user".into()),
            ..Default::default()
        };
        let access = token(Utc::now().timestamp() + 3600, "auth0|user");
        let (url, request, task) =
            response_server(200, json!({"accessToken":access}).to_string(), None).await;
        let bundle = CursorAuthDriver
            .refresh_at(
                &credential,
                reqwest::Client::new(),
                &format!("{url}/auth/exchange_user_api_key"),
            )
            .await
            .unwrap();
        let (method, uri, headers, body) = request.await.unwrap();
        assert_eq!(method, http::Method::POST);
        assert_eq!(uri.path(), "/auth/exchange_user_api_key");
        assert!(uri.query().is_none());
        assert_eq!(headers[http::header::AUTHORIZATION], "Bearer test-refresh");
        assert_eq!(body, "{}");
        assert_eq!(bundle.refresh_token.as_deref(), Some("test-refresh"));
        task.abort();
        let _ = task.await;
        for (status, transient) in [
            (429, true),
            (503, true),
            (401, false),
            (404, false),
            (400, false),
        ] {
            let (url, _, task) =
                response_server(status, "secret-refresh verifier=private".into(), Some("60")).await;
            let error = CursorAuthDriver
                .refresh_at(&credential, reqwest::Client::new(), &url)
                .await
                .unwrap_err();
            assert_eq!(
                matches!(
                    error.downcast_ref::<CursorAuthError>(),
                    Some(CursorAuthError::Transient { .. })
                ),
                transient
            );
            assert!(!format!("{error:#}").contains("secret-refresh"));
            assert!(!format!("{error:#}").contains(&url));
            task.abort();
            let _ = task.await;
        }
    }

    #[tokio::test]
    async fn cursor_missing_refresh_requires_reauth_without_request() {
        let error = CursorAuthDriver
            .refresh_at(
                &StoredCredential::default(),
                reqwest::Client::new(),
                "http://127.0.0.1:1",
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CursorAuthError>(),
            Some(CursorAuthError::Reauthenticate)
        ));
    }

    #[tokio::test]
    async fn cursor_refresh_total_deadline_includes_waiting_for_headers() {
        use axum::{Router, response::Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sender = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));
        let app = Router::new().fallback(move || {
            let sender = sender.clone();
            async move {
                if let Some(tx) = sender.lock().await.take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Response>().await
            }
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let refresh = tokio::spawn(async move {
            CursorAuthDriver
                .refresh_at(
                    &StoredCredential {
                        refresh_token: Some("test-refresh".into()),
                        ..Default::default()
                    },
                    reqwest::Client::new(),
                    &endpoint,
                )
                .await
        });
        rx.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        let error = refresh.await.unwrap().unwrap_err();
        tokio::time::resume();
        assert!(matches!(
            error.downcast_ref::<CursorAuthError>(),
            Some(CursorAuthError::Reauthenticate)
        ));
        server.abort();
        let _ = server.await;
    }
}
