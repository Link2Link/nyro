use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::shared::{
    PkceAuthState, build_authorize_url, encode_scopes, expires_at_after, generate_code_challenge,
    generate_code_verifier, generate_state, parse_oauth_callback, parse_session_state,
    required_http_client, validate_callback_state,
};
use crate::auth::types::{
    AuthDriver, AuthDriverMetadata, AuthExchangeInput, AuthScheme, AuthSession, CreateAuthSession,
    CredentialBundle, ExchangeAuthContext, RefreshAuthContext, RuntimeBinding, StartAuthContext,
    StoredCredential,
};
use crate::db::models::Provider;
use crate::provider::VendorRegistry;
use crate::provider::{OAuthConfig, RuntimeConfig};

const OPENAI_PRESET_ID: &str = "openai";
const CODEX_CHANNEL_ID: &str = "codex";
const CODEX_REFRESH_SCOPE: &str = "openid profile email";
const CODEX_CLIENT_VERSION: &str = "0.146.0";
const CODEX_ORIGINATOR: &str = "codex-tui";
const CODEX_USER_AGENT: &str = "codex-tui/0.146.0 (Ubuntu 22.4.0; x86_64) xterm-256color";

/// Resolved OAuth + runtime config for the OpenAI / Codex channel,
/// sourced from the in-process `VendorRegistry`.
#[derive(Debug, Clone, Copy)]
struct OpenAICodexConfig {
    oauth: &'static OAuthConfig,
    runtime: &'static RuntimeConfig,
}

#[derive(Debug, Default)]
pub struct OpenAIOAuthDriver;

#[derive(Debug, Deserialize)]
struct OpenAITokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIAuthState {
    #[serde(flatten)]
    pkce: PkceAuthState,
}

fn normalized_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

impl OpenAIOAuthDriver {
    fn codex_config() -> Result<OpenAICodexConfig> {
        let metadata = VendorRegistry::global()
            .metadata(OPENAI_PRESET_ID)
            .ok_or_else(|| anyhow!("missing provider preset: {OPENAI_PRESET_ID}"))?;
        let channel = metadata
            .channels
            .iter()
            .find(|c| c.id == CODEX_CHANNEL_ID)
            .ok_or_else(|| {
                anyhow!("missing provider channel: {OPENAI_PRESET_ID}/{CODEX_CHANNEL_ID}")
            })?;
        Ok(OpenAICodexConfig {
            oauth: channel.oauth.as_ref().ok_or_else(|| {
                anyhow!("missing oauth config for {OPENAI_PRESET_ID}/{CODEX_CHANNEL_ID}")
            })?,
            runtime: channel.runtime.as_ref().ok_or_else(|| {
                anyhow!("missing runtime config for {OPENAI_PRESET_ID}/{CODEX_CHANNEL_ID}")
            })?,
        })
    }

    fn normalize_token_response(
        body: &str,
        fallback_refresh_token: Option<&str>,
        fallback_scopes: Option<&[String]>,
        fallback_meta: Option<&Value>,
        runtime: &RuntimeConfig,
    ) -> Result<CredentialBundle> {
        let token: OpenAITokenResponse =
            serde_json::from_str(body).context("parse openai oauth token response")?;
        let access_token = token
            .access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("openai oauth token response missing access_token"))?;
        let expires_in = token.expires_in.unwrap_or(3600).max(1);

        // Tokens are persisted in dedicated credential columns. Keep only
        // non-secret identity and token metadata in `meta`, merging the old
        // values because refresh responses may omit ID-token claims.
        let mut meta = fallback_meta
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(token_type) = normalized_string(token.token_type.as_deref()) {
            meta.insert("token_type".to_string(), Value::String(token_type));
        }
        if let Some(id_token) = normalized_string(token.id_token.as_deref())
            && let Some(claims) = Self::decode_jwt_claims(&id_token)
            && Self::id_token_claims_current(&claims)
        {
            Self::merge_identity_claims(&mut meta, &claims);
        }

        let subject_id = meta
            .get("chatgpt_account_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                meta.get("sub")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });

        let refresh_token = token
            .refresh_token
            .filter(|value| !value.trim().is_empty())
            .or_else(|| fallback_refresh_token.map(ToString::to_string));
        let scopes = if token
            .scope
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            encode_scopes(token.scope.as_deref())
        } else {
            fallback_scopes.map(ToOwned::to_owned).unwrap_or_default()
        };

        Ok(CredentialBundle {
            access_token: Some(access_token),
            refresh_token,
            expires_at: Some(expires_at_after(expires_in)),
            resource_url: Some(runtime.api_base_url.to_string()),
            subject_id,
            scopes,
            raw: Value::Object(meta),
        })
    }

    fn parse_error(body: &str) -> Option<String> {
        let parsed: OpenAIErrorResponse = serde_json::from_str(body).ok()?;
        parsed
            .error_description
            .filter(|value| !value.trim().is_empty())
            .or_else(|| parsed.message.filter(|value| !value.trim().is_empty()))
            .or_else(|| parsed.error.filter(|value| !value.trim().is_empty()))
    }

    fn decode_jwt_claims(token: &str) -> Option<Value> {
        let mut parts = token.split('.');
        parts.next()?;
        let payload = parts.next()?;
        parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
            .ok()?;
        serde_json::from_slice(&decoded).ok()
    }

    fn id_token_claims_current(claims: &Value) -> bool {
        const CLOCK_SKEW_SECONDS: i64 = 120;
        let Some(exp) = claims.get("exp").and_then(Value::as_i64) else {
            return true;
        };
        chrono::Utc::now().timestamp() <= exp.saturating_add(CLOCK_SKEW_SECONDS)
    }

    fn merge_identity_claims(meta: &mut Map<String, Value>, claims: &Value) {
        let auth = claims
            .get("https://api.openai.com/auth")
            .and_then(Value::as_object);
        let copy_string = |meta: &mut Map<String, Value>, key: &str, value: Option<&Value>| {
            if let Some(value) = value
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                meta.insert(key.to_string(), Value::String(value.to_string()));
            }
        };
        copy_string(meta, "sub", claims.get("sub"));
        copy_string(meta, "email", claims.get("email"));
        copy_string(
            meta,
            "chatgpt_account_id",
            auth.and_then(|value| value.get("chatgpt_account_id"))
                .or_else(|| claims.get("https://api.openai.com/auth.chatgpt_account_id")),
        );
        copy_string(
            meta,
            "chatgpt_user_id",
            auth.and_then(|value| value.get("chatgpt_user_id")),
        );
        copy_string(
            meta,
            "chatgpt_plan_type",
            auth.and_then(|value| value.get("chatgpt_plan_type")),
        );
        copy_string(
            meta,
            "organization_id",
            auth.and_then(|value| value.get("poid")),
        );
    }

    fn extract_account_id(credential: &StoredCredential) -> Option<String> {
        credential
            .meta
            .get("chatgpt_account_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                let access_token = credential.access_token.as_deref()?.trim();
                let claims = Self::decode_jwt_claims(access_token)?;
                claims
                    .get("https://api.openai.com/auth")
                    .and_then(Value::as_object)
                    .and_then(|auth| auth.get("chatgpt_account_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .or_else(|| {
                        claims
                            .get("https://api.openai.com/auth.chatgpt_account_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(ToString::to_string)
                    })
            })
    }

    fn apply_auth_identity(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(USER_AGENT, CODEX_USER_AGENT)
            .header("originator", CODEX_ORIGINATOR)
    }

    fn codex_models_source(runtime: &RuntimeConfig) -> String {
        format!(
            "{}?client_version={}",
            runtime.models_url, runtime.models_client_version
        )
    }
}

#[async_trait]
impl AuthDriver for OpenAIOAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "codex",
            label: "Codex",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = Self::codex_config()?;
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        let state = generate_state();
        let redirect_uri = ctx
            .redirect_uri
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(config.oauth.redirect_uri);
        let auth_url = build_authorize_url(
            config.oauth.authorize_url,
            &[
                ("response_type", "code"),
                ("client_id", config.oauth.client_id),
                ("redirect_uri", redirect_uri),
                ("scope", config.oauth.scope),
                ("code_challenge", &code_challenge),
                ("code_challenge_method", "S256"),
                ("state", &state),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
            ],
        )?;
        let session_state = serde_json::to_string(&OpenAIAuthState {
            pkce: PkceAuthState {
                code_verifier,
                state,
                redirect_uri: redirect_uri.to_string(),
            },
        })?;

        Ok(CreateAuthSession {
            provider_id: ctx.provider_id,
            driver_key: self.metadata().key.to_string(),
            scheme: self.metadata().scheme.as_str().to_string(),
            status: "pending".to_string(),
            use_proxy: ctx.use_proxy,
            user_code: None,
            verification_uri: Some(config.oauth.auth_base_url.to_string()),
            verification_uri_complete: Some(auth_url),
            state_json: Some(session_state),
            context_json: None,
            result_json: None,
            expires_at: Some(expires_at_after(10 * 60)),
            poll_interval_seconds: Some(2),
            last_error: None,
        })
    }

    async fn exchange(
        &self,
        session: &AuthSession,
        input: AuthExchangeInput,
        ctx: ExchangeAuthContext,
    ) -> Result<CredentialBundle> {
        let config = Self::codex_config()?;
        let state: OpenAIAuthState = parse_session_state(session)?;
        let callback = parse_oauth_callback(&input)?;
        validate_callback_state(&state.pkce.state, callback.state.as_deref(), "openai")?;
        let code = callback
            .code
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("missing authorization code"))?;

        let client = required_http_client(ctx.http_client)?;
        let response = Self::apply_auth_identity(client.post(config.oauth.token_url))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", state.pkce.redirect_uri.as_str()),
                ("client_id", config.oauth.client_id),
                ("code_verifier", state.pkce.code_verifier.as_str()),
            ])
            .send()
            .await
            .context("exchange openai authorization code")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("openai oauth token exchange failed: HTTP {status} {detail}");
        }

        Self::normalize_token_response(&body, None, None, None, config.runtime)
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let config = Self::codex_config()?;
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("openai oauth refresh token is missing"))?;
        let client = required_http_client(ctx.http_client)?;

        let response = Self::apply_auth_identity(client.post(config.oauth.token_url))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", config.oauth.client_id),
                ("refresh_token", refresh_token),
                ("scope", CODEX_REFRESH_SCOPE),
            ])
            .send()
            .await
            .context("refresh openai oauth token")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("openai oauth token refresh failed: HTTP {status} {detail}");
        }

        Self::normalize_token_response(
            &body,
            Some(refresh_token),
            Some(&credential.scopes),
            Some(&credential.meta),
            config.runtime,
        )
    }

    fn bind_runtime(
        &self,
        _provider: &Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let config = Self::codex_config()?;
        let account_id = Self::extract_account_id(credential);
        let access_token = credential
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("openai oauth credential is missing access token"))?;
        let mut extra_headers = HashMap::from([
            (
                "authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("user-agent".to_string(), CODEX_USER_AGENT.to_string()),
            ("originator".to_string(), CODEX_ORIGINATOR.to_string()),
            ("version".to_string(), CODEX_CLIENT_VERSION.to_string()),
        ]);
        if let Some(account_id) = account_id {
            extra_headers.insert("chatgpt-account-id".to_string(), account_id);
        }
        let base_url_override = credential
            .resource_url
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some(config.runtime.api_base_url.to_string()));

        let models_source_override = Some(Self::codex_models_source(config.runtime));

        Ok(RuntimeBinding {
            base_url_override,
            extra_headers,
            model_aliases: HashMap::new(),
            models_source_override,
            // Runtime binding owns both Bearer auth and the Codex identity;
            // suppress the generic OpenAI API-key header path.
            disable_default_auth: true,
            static_models_override: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    fn jwt(payload: Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn token_response_sanitizes_secrets_and_extracts_identity() {
        let config = OpenAIOAuthDriver::codex_config().unwrap();
        let id_token = jwt(json!({
            "exp": chrono::Utc::now().timestamp() + 3600,
            "sub": "user-1",
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-1",
                "chatgpt_user_id": "chatgpt-user-1",
                "chatgpt_plan_type": "plus",
                "poid": "org-1"
            }
        }));
        let body = json!({
            "access_token": "secret-access",
            "refresh_token": "secret-refresh",
            "id_token": id_token,
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "openid profile email offline_access"
        })
        .to_string();

        let bundle =
            OpenAIOAuthDriver::normalize_token_response(&body, None, None, None, config.runtime)
                .unwrap();

        assert_eq!(bundle.subject_id.as_deref(), Some("account-1"));
        assert_eq!(bundle.raw["chatgpt_account_id"], "account-1");
        assert_eq!(bundle.raw["email"], "user@example.com");
        assert!(bundle.raw.get("access_token").is_none());
        assert!(bundle.raw.get("refresh_token").is_none());
        assert!(bundle.raw.get("id_token").is_none());
    }

    #[test]
    fn expired_id_token_claims_are_ignored() {
        let config = OpenAIOAuthDriver::codex_config().unwrap();
        let id_token = jwt(json!({
            "exp": 1,
            "https://api.openai.com/auth": { "chatgpt_account_id": "expired-account" }
        }));
        let body = json!({
            "access_token": "secret-access",
            "refresh_token": "secret-refresh",
            "id_token": id_token,
            "expires_in": 3600
        })
        .to_string();
        let bundle =
            OpenAIOAuthDriver::normalize_token_response(&body, None, None, None, config.runtime)
                .unwrap();

        assert!(bundle.subject_id.is_none());
        assert!(bundle.raw.get("chatgpt_account_id").is_none());
    }

    #[test]
    fn refresh_response_preserves_old_identity_metadata() {
        let config = OpenAIOAuthDriver::codex_config().unwrap();
        let old = json!({
            "chatgpt_account_id": "account-old",
            "email": "old@example.com",
            "chatgpt_plan_type": "pro"
        });
        let old_scopes = vec!["openid".to_string(), "profile".to_string()];
        let bundle = OpenAIOAuthDriver::normalize_token_response(
            r#"{"access_token":"new-access","expires_in":1800}"#,
            Some("old-refresh"),
            Some(&old_scopes),
            Some(&old),
            config.runtime,
        )
        .unwrap();

        assert_eq!(bundle.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(bundle.subject_id.as_deref(), Some("account-old"));
        assert_eq!(bundle.scopes, old_scopes);
        assert_eq!(bundle.raw["chatgpt_plan_type"], "pro");
    }

    #[test]
    fn runtime_uses_metadata_before_legacy_access_token_claims() {
        let driver = OpenAIOAuthDriver;
        let credential = StoredCredential {
            driver_key: "codex".to_string(),
            scheme: AuthScheme::OAuthAuthCodePkce.as_str().to_string(),
            access_token: Some("not-a-jwt".to_string()),
            refresh_token: None,
            expires_at: None,
            resource_url: None,
            subject_id: None,
            scopes: vec![],
            meta: json!({ "chatgpt_account_id": "account-meta" }),
        };
        let provider = Provider {
            id: "provider".to_string(),
            name: "Codex".to_string(),
            vendor: Some("openai".to_string()),
            protocol: "openai-responses".to_string(),
            base_url: "https://placeholder.invalid".to_string(),
            protocol_mode: "fixed".to_string(),
            protocol_endpoints: vec![],
            preset_key: Some("openai".to_string()),
            channel: Some("codex".to_string()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".to_string(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };

        let binding = driver.bind_runtime(&provider, &credential).unwrap();
        assert_eq!(
            binding
                .extra_headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("account-meta")
        );
        assert_eq!(binding.extra_headers["authorization"], "Bearer not-a-jwt");
        assert_eq!(binding.extra_headers["originator"], CODEX_ORIGINATOR);
        assert_eq!(binding.extra_headers["version"], CODEX_CLIENT_VERSION);
        assert!(binding.disable_default_auth);
    }
}
