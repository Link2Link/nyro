//! Google AI Pro (Antigravity) OAuth driver.
//!
//! Authenticates a Google account that carries a Google AI Pro / Ultra
//! subscription through the Antigravity IDE public OAuth client, then talks
//! to the Code Assist internal API (cloudcode-pa.googleapis.com/v1internal).
//!
//! Wire behavior mirrors what the Antigravity IDE does (and what
//! CLIProxyAPI / sub2api reverse-engineered):
//!
//! * authorize: accounts.google.com/o/oauth2/v2/auth, PKCE S256,
//!   access_type=offline + prompt=consent so a refresh token is issued.
//! * token: standard oauth2.googleapis.com/token (form encoded, includes
//!   the Antigravity client secret).
//! * project bootstrap: loadCodeAssist returns the cloudaicompanionProject;
//!   brand-new accounts need onboardUser (on the daily- control-plane host)
//!   before the project exists.
//!
//! The project_id is stashed in the credential metadata (not a secret) and
//! must survive token refreshes — Google's refresh response does not echo it.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

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
use crate::provider::OAuthConfig;
use crate::provider::VendorRegistry;
use crate::provider::google::antigravity::{
    ANTIGRAVITY_VERSION, X_GOOG_API_CLIENT, antigravity_user_agent,
};

const GOOGLE_PRESET_ID: &str = "google";
const ANTIGRAVITY_CHANNEL_ID: &str = "antigravity";
const GOOGLE_GEMINI_PROTOCOL_ID: &str = "google-gemini";

/// Antigravity IDE public OAuth client secret (identical to the value shipped
/// by CLIProxyAPI and sub2api). This is a published "installed app" client
/// credential, not a private secret of Nyro.
const ANTIGRAVITY_CLIENT_SECRET: &str = "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";

/// Google identity endpoint (email → subject_id).
const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";

/// Code Assist control-plane host for onboarding. Inference always hits prod;
/// onboardUser uses the daily- host (matching the Antigravity IDE fingerprint).
const ANTIGRAVITY_DAILY_API_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

/// How long a pending login session stays valid.
const SESSION_TTL_SECONDS: i64 = 10 * 60;
/// onboardUser polling: attempts × delay (mirrors CLIProxyAPI).
const ONBOARD_MAX_ATTEMPTS: usize = 5;
const ONBOARD_POLL_DELAY_MS: u64 = 2_000;

#[derive(Debug, Default)]
pub struct GoogleAntigravityDriver;

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
}

/// Channel-level config resolved from the vendor registry (mirrors the
/// Claude Code driver's claude_code_config).
struct AntigravityConfig {
    oauth: &'static OAuthConfig,
    api_base_url: &'static str,
    static_models: &'static [&'static str],
}

impl GoogleAntigravityDriver {
    fn channel_config() -> Result<AntigravityConfig> {
        let metadata = VendorRegistry::global()
            .metadata(GOOGLE_PRESET_ID)
            .ok_or_else(|| anyhow!("missing provider preset: {GOOGLE_PRESET_ID}"))?;
        let channel = metadata
            .channels
            .iter()
            .find(|c| c.id == ANTIGRAVITY_CHANNEL_ID)
            .ok_or_else(|| {
                anyhow!("missing provider channel: {GOOGLE_PRESET_ID}/{ANTIGRAVITY_CHANNEL_ID}")
            })?;
        let api_base_url = channel
            .base_urls
            .iter()
            .find(|entry| entry.protocol == GOOGLE_GEMINI_PROTOCOL_ID)
            .map(|entry| entry.base_url)
            .ok_or_else(|| {
                anyhow!(
                    "missing base url for protocol {GOOGLE_GEMINI_PROTOCOL_ID} in                      {GOOGLE_PRESET_ID}/{ANTIGRAVITY_CHANNEL_ID}"
                )
            })?;
        Ok(AntigravityConfig {
            oauth: channel.oauth.as_ref().ok_or_else(|| {
                anyhow!("missing oauth config for {GOOGLE_PRESET_ID}/{ANTIGRAVITY_CHANNEL_ID}")
            })?,
            api_base_url,
            static_models: channel.static_models,
        })
    }

    fn parse_error(body: &str) -> Option<String> {
        let parsed: GoogleErrorResponse = serde_json::from_str(body).ok()?;
        parsed
            .error_description
            .filter(|value| !value.trim().is_empty())
            .or_else(|| parsed.error.filter(|value| !value.trim().is_empty()))
    }

    /// Exchange / refresh against Google's token endpoint. Google uses
    /// classic form encoding (unlike Anthropic's JSON endpoint).
    async fn token_request(
        &self,
        client: &reqwest::Client,
        token_url: &str,
        params: &[(&str, &str)],
    ) -> Result<GoogleTokenResponse> {
        let response = client
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", antigravity_user_agent())
            .form(params)
            .send()
            .await
            .context("antigravity oauth token request")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("antigravity token endpoint failed: HTTP {status} {detail}");
        }
        serde_json::from_str(&body).context("parse antigravity token response")
    }

    /// Build the credential bundle from a token response, carrying identity
    /// metadata (project_id / email / tier_id) from prior_meta when the
    /// response itself cannot supply it (refresh flow).
    fn build_bundle(
        token: GoogleTokenResponse,
        prior_meta: Option<&Value>,
        config: &AntigravityConfig,
    ) -> Result<CredentialBundle> {
        let access_token = token
            .access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("antigravity token response missing access_token"))?;
        let expires_in = token.expires_in.unwrap_or(3600).max(1);

        let mut meta = serde_json::Map::new();
        if let Some(prior) = prior_meta.and_then(Value::as_object) {
            // Survive refreshes: Google does not echo these back.
            for key in ["project_id", "email", "tier_id"] {
                if let Some(value) = prior.get(key).filter(|v| !v.is_null()) {
                    meta.insert(key.to_string(), value.clone());
                }
            }
        }

        Ok(CredentialBundle {
            resource_url: Some(config.api_base_url.to_string()),
            subject_id: meta
                .get("email")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            scopes: encode_scopes(token.scope.as_deref()),
            expires_at: Some(expires_at_after(expires_in)),
            access_token: Some(access_token),
            refresh_token: token.refresh_token.filter(|value| !value.trim().is_empty()),
            raw: Value::Object(meta),
        })
    }

    async fn fetch_user_email(
        &self,
        client: &reqwest::Client,
        access_token: &str,
    ) -> Result<String> {
        let response = client
            .get(GOOGLE_USERINFO_URL)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", antigravity_user_agent())
            .send()
            .await
            .context("antigravity userinfo request")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("antigravity userinfo failed: HTTP {status} {detail}");
        }
        let parsed: Value =
            serde_json::from_str(&body).context("parse antigravity userinfo response")?;
        parsed
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("antigravity userinfo response missing email"))
    }

    /// loadCodeAssist → cloudaicompanionProject + tier summary. Returns
    /// (project_id_opt, tier_summary_json).
    async fn load_code_assist(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        access_token: &str,
    ) -> Result<(Option<String>, Value)> {
        let endpoint = format!(
            "{}/v1internal:loadCodeAssist",
            base_url.trim_end_matches('/')
        );
        let response = client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "*/*")
            .header("User-Agent", antigravity_user_agent())
            .json(&json!({ "metadata": { "ideType": "ANTIGRAVITY" } }))
            .send()
            .await
            .context("antigravity loadCodeAssist request")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("antigravity loadCodeAssist failed: HTTP {status} {detail}");
        }
        let parsed: Value =
            serde_json::from_str(&body).context("parse antigravity loadCodeAssist response")?;
        let tier_summary = json!({
            "currentTier": parsed.get("currentTier").cloned().unwrap_or(Value::Null),
            "allowedTiers": parsed.get("allowedTiers").cloned().unwrap_or(Value::Null),
        });
        Ok((extract_project_id(&parsed), tier_summary))
    }

    /// onboardUser polling until done (bounded attempts). Only needed for
    /// accounts whose loadCodeAssist has no companion project yet.
    async fn onboard_user(
        &self,
        client: &reqwest::Client,
        access_token: &str,
        tier_id: &str,
    ) -> Result<String> {
        let endpoint = format!("{ANTIGRAVITY_DAILY_API_BASE_URL}/v1internal:onboardUser");
        let user_agent = antigravity_user_agent();
        for attempt in 1..=ONBOARD_MAX_ATTEMPTS {
            let response = client
                .post(&endpoint)
                .header("Authorization", format!("Bearer {access_token}"))
                .header("Content-Type", "application/json")
                .header("Accept", "*/*")
                .header("User-Agent", &user_agent)
                .header("X-Goog-Api-Client", X_GOOG_API_CLIENT)
                .json(&json!({
                    "tier_id": tier_id,
                    "metadata": {
                        "ide_type": "ANTIGRAVITY",
                        "ide_version": ANTIGRAVITY_VERSION,
                        "ide_name": "antigravity",
                    },
                }))
                .send()
                .await
                .with_context(|| format!("antigravity onboardUser attempt {attempt}"))?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                let detail = Self::parse_error(&body).unwrap_or(body);
                bail!("antigravity onboardUser failed: HTTP {status} {detail}");
            }
            let parsed: Value =
                serde_json::from_str(&body).context("parse antigravity onboardUser response")?;
            if parsed.get("done").and_then(Value::as_bool).unwrap_or(false) {
                if let Some(project) = parsed
                    .get("response")
                    .and_then(extract_project_id_from_value)
                {
                    return Ok(project);
                }
                bail!("antigravity onboardUser completed without a project id");
            }
            tokio::time::sleep(std::time::Duration::from_millis(ONBOARD_POLL_DELAY_MS)).await;
        }
        bail!("antigravity onboardUser did not complete after {ONBOARD_MAX_ATTEMPTS} attempts")
    }

    /// Resolve (and persist on first login) the companion project id.
    async fn resolve_project_id(
        &self,
        client: &reqwest::Client,
        config: &AntigravityConfig,
        access_token: &str,
        prior_meta: Option<&Value>,
    ) -> Result<(String, Value)> {
        if let Some(project) = prior_meta
            .and_then(|meta| meta.get("project_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            // Project already onboarded previously; reuse it. Tier info is
            // best-effort — never fail a refresh over it.
            let (_, tier_summary) = self
                .load_code_assist(client, config.api_base_url, access_token)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "antigravity loadCodeAssist failed during refresh");
                    (None, Value::Null)
                });
            return Ok((project.to_string(), tier_summary));
        }

        let (project, tier_summary) = self
            .load_code_assist(client, config.api_base_url, access_token)
            .await?;
        if let Some(project) = project {
            return Ok((project, tier_summary));
        }

        // No companion project yet → finish onboarding with the default tier.
        let tier_id = default_tier_id(&tier_summary);
        let project = self.onboard_user(client, access_token, &tier_id).await?;
        Ok((project, tier_summary))
    }
}

/// cloudaicompanionProject may be a bare string or {id: …}; the same shape
/// appears inside onboardUser's response wrapper.
fn extract_project_id_from_value(value: &Value) -> Option<String> {
    match value.get("cloudaicompanionProject") {
        Some(Value::String(project)) => Some(project.trim().to_string()).filter(|p| !p.is_empty()),
        Some(project @ Value::Object(_)) => project
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(ToString::to_string),
        _ => None,
    }
}

fn extract_project_id(value: &Value) -> Option<String> {
    extract_project_id_from_value(value)
}

fn default_tier_id(tier_summary: &Value) -> String {
    if let Some(tiers) = tier_summary.get("allowedTiers").and_then(Value::as_array) {
        for tier in tiers {
            let is_default = tier.get("isDefault").and_then(Value::as_bool) == Some(true);
            let id = tier.get("id").and_then(Value::as_str).map(str::trim);
            if is_default && id.is_some_and(|value| !value.is_empty()) {
                return id.unwrap_or_default().to_string();
            }
        }
    }
    if let Some(id) = tier_summary
        .get("currentTier")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
    {
        return id.to_string();
    }
    "free-tier".to_string()
}

#[async_trait]
impl AuthDriver for GoogleAntigravityDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "google",
            label: "Google AI Pro (Antigravity)",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = Self::channel_config()?;
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
                ("client_id", config.oauth.client_id),
                ("redirect_uri", redirect_uri),
                ("response_type", "code"),
                ("scope", config.oauth.scope),
                ("state", &state),
                ("code_challenge", &code_challenge),
                ("code_challenge_method", "S256"),
                ("access_type", "offline"),
                ("prompt", "consent"),
                ("include_granted_scopes", "true"),
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
            expires_at: Some(expires_at_after(SESSION_TTL_SECONDS)),
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
        let config = Self::channel_config()?;
        let state: PkceAuthState = parse_session_state(session)?;
        let callback = parse_oauth_callback(&input)?;
        validate_callback_state(&state.state, callback.state.as_deref(), "google")?;
        let code = callback
            .code
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("missing authorization code"))?;

        let client = required_http_client(ctx.http_client)?;
        let token = self
            .token_request(
                &client,
                config.oauth.token_url,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("client_id", config.oauth.client_id),
                    ("client_secret", ANTIGRAVITY_CLIENT_SECRET),
                    ("redirect_uri", &state.redirect_uri),
                    ("code_verifier", &state.code_verifier),
                ],
            )
            .await?;

        let mut bundle = Self::build_bundle(token, None, &config)?;

        // Identity + project bootstrap: the access token is fresh, so resolve
        // email and the companion project right away.
        let access_token = bundle.access_token.clone().unwrap_or_default();
        let email = self.fetch_user_email(&client, &access_token).await?;
        let (project_id, tier_summary) = self
            .resolve_project_id(&client, &config, &access_token, None)
            .await?;
        if let Some(meta) = bundle.raw.as_object_mut() {
            meta.insert("email".to_string(), Value::String(email));
            meta.insert("project_id".to_string(), Value::String(project_id));
            if let Some(tier) = tier_summary
                .get("currentTier")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
            {
                meta.insert("tier_id".to_string(), Value::String(tier.to_string()));
            }
        }
        bundle.subject_id = bundle
            .raw
            .get("email")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        Ok(bundle)
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let config = Self::channel_config()?;
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("antigravity refresh token is missing"))?;
        let client = required_http_client(ctx.http_client)?;

        let token = self
            .token_request(
                &client,
                config.oauth.token_url,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", config.oauth.client_id),
                    ("client_secret", ANTIGRAVITY_CLIENT_SECRET),
                ],
            )
            .await?;

        let mut bundle = Self::build_bundle(token, Some(&credential.meta), &config)?;
        if bundle.refresh_token.is_none() {
            bundle.refresh_token = Some(refresh_token.to_string());
        }

        // The project id is required at request time; if prior metadata lost
        // it (e.g. credential written by an older build), recover it now.
        let has_project_id = bundle
            .raw
            .get("project_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        if !has_project_id {
            let access_token = bundle.access_token.clone().unwrap_or_default();
            let (project_id, tier_summary) = self
                .resolve_project_id(&client, &config, &access_token, Some(&credential.meta))
                .await?;
            if let Some(meta) = bundle.raw.as_object_mut() {
                meta.insert("project_id".to_string(), Value::String(project_id));
                if let Some(tier) = tier_summary
                    .get("currentTier")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                {
                    meta.insert("tier_id".to_string(), Value::String(tier.to_string()));
                }
            }
        }

        Ok(bundle)
    }

    fn bind_runtime(
        &self,
        _provider: &Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let config = Self::channel_config()?;
        let access_token = credential
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("antigravity access token is empty in bind_runtime"))?;

        let mut extra_headers = HashMap::new();
        extra_headers.insert(
            "authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        extra_headers.insert("user-agent".to_string(), antigravity_user_agent());
        extra_headers.insert(
            "x-goog-api-client".to_string(),
            X_GOOG_API_CLIENT.to_string(),
        );

        // The channel's curated model list is the catalog the subscription can
        // actually run (dynamic fetchAvailableModels discovery is a
        // follow-up).
        let static_models_override: Option<Vec<String>> = if config.static_models.is_empty() {
            None
        } else {
            Some(config.static_models.iter().map(|s| s.to_string()).collect())
        };

        Ok(RuntimeBinding {
            base_url_override: Some(config.api_base_url.to_string()),
            extra_headers,
            model_aliases: HashMap::new(),
            models_source_override: Some("ai://models.dev/google".to_string()),
            disable_default_auth: true,
            static_models_override,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider() -> Provider {
        Provider {
            id: "test".into(),
            name: "test".into(),
            vendor: Some("google".into()),
            protocol: "google-gemini".into(),
            base_url: String::new(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("google".into()),
            channel: Some("antigravity".into()),
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
        let config = GoogleAntigravityDriver::channel_config().unwrap();
        assert!(config.oauth.authorize_url.contains("accounts.google.com"));
        assert!(config.oauth.token_url.contains("oauth2.googleapis.com"));
        assert_eq!(config.api_base_url, "https://cloudcode-pa.googleapis.com");
        assert!(
            config
                .oauth
                .scope
                .contains("https://www.googleapis.com/auth/cloud-platform")
        );
        assert!(!config.static_models.is_empty());
    }

    #[test]
    fn build_bundle_carries_prior_meta_into_refresh() {
        let config = GoogleAntigravityDriver::channel_config().unwrap();
        let token = GoogleTokenResponse {
            access_token: Some("ya29.new".into()),
            refresh_token: None,
            expires_in: Some(3600),
            scope: Some("https://www.googleapis.com/auth/cloud-platform".into()),
        };
        let prior = json!({
            "project_id": "cloudaicompanion-123",
            "email": "user@example.com",
            "tier_id": "tiered",
            "unrelated": true,
        });
        let bundle = GoogleAntigravityDriver::build_bundle(token, Some(&prior), &config).unwrap();
        assert_eq!(bundle.access_token.as_deref(), Some("ya29.new"));
        assert_eq!(bundle.subject_id.as_deref(), Some("user@example.com"));
        assert_eq!(
            bundle.raw.get("project_id").and_then(Value::as_str),
            Some("cloudaicompanion-123")
        );
        // Only the identity keys are carried; unrelated blobs are dropped.
        assert!(bundle.raw.get("unrelated").is_none());
    }

    #[test]
    fn extract_project_id_handles_string_and_object_forms() {
        let string_form = json!({"cloudaicompanionProject": "proj-1"});
        assert_eq!(extract_project_id(&string_form).as_deref(), Some("proj-1"));
        let object_form = json!({"cloudaicompanionProject": {"id": "proj-2"}});
        assert_eq!(extract_project_id(&object_form).as_deref(), Some("proj-2"));
        let onboard = json!({"done": true, "response": {"cloudaicompanionProject": "proj-3"}});
        assert_eq!(
            extract_project_id_from_value(onboard.get("response").unwrap()).as_deref(),
            Some("proj-3")
        );
        assert_eq!(extract_project_id(&json!({})), None);
    }

    #[test]
    fn default_tier_id_prefers_default_flag_then_current() {
        let summary = json!({
            "allowedTiers": [
                {"id": "individual", "isDefault": false},
                {"id": "tiered", "isDefault": true},
            ],
            "currentTier": {"id": "legacy"},
        });
        assert_eq!(default_tier_id(&summary), "tiered");
        assert_eq!(default_tier_id(&json!({})), "free-tier");
    }

    #[test]
    fn bind_runtime_sets_antigravity_headers_and_models() {
        let provider = test_provider();
        let credential = StoredCredential {
            access_token: Some("ya29.token".into()),
            meta: json!({"project_id": "cloudaicompanion-123"}),
            ..Default::default()
        };
        let binding = GoogleAntigravityDriver
            .bind_runtime(&provider, &credential)
            .unwrap();
        assert_eq!(
            binding.extra_headers.get("authorization").unwrap(),
            "Bearer ya29.token"
        );
        assert!(
            binding
                .extra_headers
                .get("user-agent")
                .unwrap()
                .starts_with("antigravity/2.9.1")
        );
        assert_eq!(
            binding.extra_headers.get("x-goog-api-client").unwrap(),
            "gl-node/22.21.1"
        );
        assert!(binding.disable_default_auth);
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://cloudcode-pa.googleapis.com")
        );
        let models = binding.static_models_override.unwrap();
        assert!(models.iter().any(|m| m == "gemini-2.5-pro"));
    }
}
