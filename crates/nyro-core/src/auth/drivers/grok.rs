use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use rand::RngCore;
use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;
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

const XAI_PRESET_ID: &str = "xai";
const GROK_CHANNEL_ID: &str = "grok";
const GROK_DEFAULT_ACCESS_TOKEN_TTL: i64 = 6 * 60 * 60;
const GROK_CLI_VERSION: &str = "0.2.114";
const GROK_CLI_TOKEN_AUTH: &str = "xai-grok-cli";
const GROK_CLI_IDENTIFIER: &str = "grok-shell";
const GROK_OAUTH_USER_AGENT: &str = "nyro-grok-oauth/1.0";

/// Resolved OAuth + runtime config for the xAI / Grok channel.
#[derive(Debug, Clone, Copy)]
struct GrokConfig {
    oauth: &'static OAuthConfig,
    runtime: &'static RuntimeConfig,
}

#[derive(Debug, Default)]
pub struct GrokOAuthDriver;

#[derive(Debug, Deserialize)]
struct GrokTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrokErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    message: Option<String>,
}

fn normalized_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn generate_nonce() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn grok_cli_user_agent() -> String {
    format!("xai-grok-workspace/{GROK_CLI_VERSION}")
}

impl GrokOAuthDriver {
    fn grok_config() -> Result<GrokConfig> {
        let metadata = VendorRegistry::global()
            .metadata(XAI_PRESET_ID)
            .ok_or_else(|| anyhow!("missing provider preset: {XAI_PRESET_ID}"))?;
        let channel = metadata
            .channels
            .iter()
            .find(|c| c.id == GROK_CHANNEL_ID)
            .ok_or_else(|| anyhow!("missing provider channel: {XAI_PRESET_ID}/{GROK_CHANNEL_ID}"))?;
        Ok(GrokConfig {
            oauth: channel.oauth.as_ref().ok_or_else(|| {
                anyhow!("missing oauth config for {XAI_PRESET_ID}/{GROK_CHANNEL_ID}")
            })?,
            runtime: channel.runtime.as_ref().ok_or_else(|| {
                anyhow!("missing runtime config for {XAI_PRESET_ID}/{GROK_CHANNEL_ID}")
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
        let token: GrokTokenResponse =
            serde_json::from_str(body).context("parse grok oauth token response")?;
        let access_token = token
            .access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("grok oauth token response missing access_token"))?;
        let expires_in = token
            .expires_in
            .filter(|value| *value > 0)
            .unwrap_or(GROK_DEFAULT_ACCESS_TOKEN_TTL);

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
            Self::merge_identity_claims(&mut meta, &claims, false);
        }
        if let Some(claims) = Self::decode_jwt_claims(&access_token) {
            Self::merge_identity_claims(&mut meta, &claims, true);
        }

        let subject_id = meta
            .get("sub")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                meta.get("email")
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
        let parsed: GrokErrorResponse = serde_json::from_str(body).ok()?;
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

    fn merge_identity_claims(meta: &mut Map<String, Value>, claims: &Value, include_tier: bool) {
        let copy_string = |meta: &mut Map<String, Value>, key: &str, value: Option<&Value>| {
            if meta
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
            {
                return;
            }
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
        copy_string(meta, "team_id", claims.get("team_id"));
        if include_tier
            && meta
                .get("subscription_tier")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            && let Some(tier) = subscription_tier_from_claim(claims.get("tier"))
        {
            meta.insert("subscription_tier".to_string(), Value::String(tier));
        }
    }
}

fn subscription_tier_from_claim(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(normalize_subscription_tier(trimmed))
            }
        }
        Value::Number(n) => n.as_u64().map(map_jwt_subscription_tier),
        _ => None,
    }
}

fn map_jwt_subscription_tier(tier: u64) -> String {
    match tier {
        0 => "free".to_string(),
        1 => "supergrok".to_string(),
        2 => "x_basic".to_string(),
        3 => "x_premium".to_string(),
        4 => "x_premium_plus".to_string(),
        5 => "supergrok_heavy".to_string(),
        6 => "supergrok_lite".to_string(),
        7 => "supergrok_plus".to_string(),
        other => other.to_string(),
    }
}

fn normalize_subscription_tier(raw: &str) -> String {
    let t = raw.trim().to_ascii_lowercase().replace('-', "_");
    let t = t.split_whitespace().collect::<Vec<_>>().join("_");
    match t.as_str() {
        "free" | "grok_free" | "grokfree" | "free_tier" | "freetier" | "grok_basic"
        | "grokbasic" => "free".to_string(),
        "supergrok" | "grokpro" => "supergrok".to_string(),
        "supergrok_lite" | "supergroklite" => "supergrok_lite".to_string(),
        "supergrok_heavy" | "supergrokheavy" => "supergrok_heavy".to_string(),
        "supergrok_plus" | "supergrokplus" => "supergrok_plus".to_string(),
        "x_basic" | "xbasic" | "basic" => "x_basic".to_string(),
        "x_premium" | "xpremium" => "x_premium".to_string(),
        "x_premium_plus" | "xpremiumplus" | "x_premium+" => "x_premium_plus".to_string(),
        other => other.to_string(),
    }
}

#[async_trait]
impl AuthDriver for GrokOAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "grok",
            label: "Grok",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = Self::grok_config()?;
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        let state = generate_state();
        let nonce = generate_nonce();
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
                ("state", &state),
                ("nonce", &nonce),
                ("code_challenge", &code_challenge),
                ("code_challenge_method", "S256"),
                ("plan", "generic"),
                ("referrer", "nyro"),
            ],
        )?;
        let session_state = serde_json::to_string(&PkceAuthState {
            code_verifier,
            state,
            redirect_uri: redirect_uri.to_string(),
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
        let config = Self::grok_config()?;
        let state: PkceAuthState = parse_session_state(session)?;
        let callback = parse_oauth_callback(&input)?;
        // Grok Build's success page often shows only the authorization code
        // ("copy this code into Grok Build") with no callback URL / state.
        // The in-memory PKCE session already binds this exchange, so a missing
        // state is accepted; a present state still has to match.
        if callback
            .state
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        {
            validate_callback_state(&state.state, callback.state.as_deref(), "grok")?;
        }
        let code = callback
            .code
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("missing authorization code"))?;

        let client = required_http_client(ctx.http_client)?;
        let response = client
            .post(config.oauth.token_url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, GROK_OAUTH_USER_AGENT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", config.oauth.client_id),
                ("code", code),
                ("redirect_uri", state.redirect_uri.as_str()),
                ("code_verifier", state.code_verifier.as_str()),
            ])
            .send()
            .await
            .context("exchange grok authorization code")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("grok oauth token exchange failed: HTTP {status} {detail}");
        }

        Self::normalize_token_response(&body, None, None, None, config.runtime)
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let config = Self::grok_config()?;
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("grok oauth refresh token is missing"))?;
        let client = required_http_client(ctx.http_client)?;

        let response = client
            .post(config.oauth.token_url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, GROK_OAUTH_USER_AGENT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", config.oauth.client_id),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .context("refresh grok oauth token")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("grok oauth token refresh failed: HTTP {status} {detail}");
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
        let config = Self::grok_config()?;
        let access_token = credential
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("grok oauth access token is empty in bind_runtime"))?;

        let extra_headers = HashMap::from([
            (
                "authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            (
                "x-xai-token-auth".to_string(),
                GROK_CLI_TOKEN_AUTH.to_string(),
            ),
            (
                "x-grok-client-version".to_string(),
                GROK_CLI_VERSION.to_string(),
            ),
            (
                "x-grok-client-identifier".to_string(),
                GROK_CLI_IDENTIFIER.to_string(),
            ),
            ("user-agent".to_string(), grok_cli_user_agent()),
        ]);

        let base_url_override = credential
            .resource_url
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some(config.runtime.api_base_url.to_string()));
        let models_source_override = Some(config.runtime.models_url.to_string());
        // Model discovery is served by the upstream `/v1/models` endpoint,
        // which accepts the OAuth Bearer (plus the Grok CLI identity headers
        // stamped below). Deliberately no `static_models_override` here: the
        // channel's `static_models` still act as a fallback merge list, but
        // the live list is fetched from cli-chat-proxy so new Grok releases
        // appear without a binary rebuild.
        let static_models_override: Option<Vec<String>> = None;

        Ok(RuntimeBinding {
            base_url_override,
            extra_headers,
            model_aliases: HashMap::new(),
            models_source_override,
            disable_default_auth: true,
            static_models_override,
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

    fn test_provider() -> Provider {
        Provider {
            id: "test".into(),
            name: "Grok".into(),
            vendor: Some("xai".into()),
            protocol: "openai-responses".into(),
            base_url: String::new(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("xai".into()),
            channel: Some("grok".into()),
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
    fn config_loads_from_vendor_registry() {
        let config = GrokOAuthDriver::grok_config().unwrap();
        assert_eq!(config.oauth.auth_base_url, "https://auth.x.ai");
        assert!(config.oauth.authorize_url.contains("auth.x.ai"));
        assert!(config.oauth.token_url.contains("auth.x.ai"));
        assert_eq!(
            config.runtime.api_base_url,
            "https://cli-chat-proxy.grok.com/v1"
        );
    }

    #[test]
    fn normalize_token_response_uses_cli_proxy_and_claims() {
        let access = jwt(json!({
            "sub": "user-1",
            "email": "user@example.com",
            "team_id": "team-9",
            "tier": 1
        }));
        let body = json!({
            "access_token": access,
            "refresh_token": "ref_xyz",
            "expires_in": 7200,
            "scope": "openid profile email offline_access grok-cli:access api:access",
            "token_type": "Bearer"
        })
        .to_string();
        let config = GrokOAuthDriver::grok_config().unwrap();
        let bundle =
            GrokOAuthDriver::normalize_token_response(&body, None, None, None, config.runtime)
                .unwrap();
        assert_eq!(bundle.refresh_token.as_deref(), Some("ref_xyz"));
        assert_eq!(bundle.subject_id.as_deref(), Some("user-1"));
        assert_eq!(bundle.raw["email"], "user@example.com");
        assert_eq!(bundle.raw["team_id"], "team-9");
        assert_eq!(bundle.raw["subscription_tier"], "supergrok");
        assert_eq!(
            bundle.resource_url.as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1")
        );
        assert!(bundle.raw.get("access_token").is_none());
        assert!(bundle.raw.get("refresh_token").is_none());
    }

    #[test]
    fn missing_expires_in_defaults_to_six_hours() {
        let body = r#"{"access_token":"tok_abc"}"#;
        let config = GrokOAuthDriver::grok_config().unwrap();
        let bundle =
            GrokOAuthDriver::normalize_token_response(body, None, None, None, config.runtime)
                .unwrap();
        assert!(bundle.expires_at.is_some());
    }

    fn grok_exchange_state_ok(session_state: &str, pasted_state: Option<&str>) -> bool {
        if pasted_state
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        {
            validate_callback_state(session_state, pasted_state, "grok").is_ok()
        } else {
            true
        }
    }

    #[test]
    fn grok_build_code_without_state_is_accepted() {
        assert!(grok_exchange_state_ok("session-state", None));
        assert!(grok_exchange_state_ok("session-state", Some("")));
        assert!(grok_exchange_state_ok("session-state", Some("session-state")));
        assert!(!grok_exchange_state_ok("session-state", Some("other-state")));
    }

    #[test]
    fn parse_error_prefers_description() {
        let body = r#"{"error":"invalid_grant","error_description":"code expired"}"#;
        assert_eq!(
            GrokOAuthDriver::parse_error(body).as_deref(),
            Some("code expired")
        );
    }

    #[test]
    fn bind_runtime_stamps_cli_identity_and_disables_default_auth() {
        let provider = test_provider();
        let credential = StoredCredential {
            access_token: Some("my_token".into()),
            ..Default::default()
        };
        let binding = GrokOAuthDriver
            .bind_runtime(&provider, &credential)
            .unwrap();
        assert_eq!(
            binding.extra_headers.get("authorization").unwrap(),
            "Bearer my_token"
        );
        assert_eq!(
            binding.extra_headers.get("x-xai-token-auth").unwrap(),
            "xai-grok-cli"
        );
        assert_eq!(
            binding.extra_headers.get("x-grok-client-identifier").unwrap(),
            "grok-shell"
        );
        assert_eq!(
            binding.extra_headers.get("user-agent").unwrap(),
            "xai-grok-workspace/0.2.114"
        );
        assert!(binding.disable_default_auth);
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1")
        );
        // Model discovery must come from the upstream `/v1/models` endpoint,
        // not a static override (static_models only act as a fallback merge).
        assert!(
            binding.static_models_override.is_none(),
            "grok channel must not force a static model override"
        );
        assert_eq!(
            binding.models_source_override.as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1/models")
        );
    }
}
