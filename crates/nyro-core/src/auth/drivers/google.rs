//! Google subscription OAuth drivers (Code Assist v1internal surface).
//!
//! Two channels authenticate a Google account against
//! `cloudcode-pa.googleapis.com/v1internal`:
//!
//! * **Antigravity** (`google` driver key): Google AI Pro / Ultra
//!   subscription via the Antigravity IDE public OAuth client.
//! * **Gemini CLI** (`google-gemini-cli` driver key): Code Assist
//!   subscription (Standard/Enterprise/legacy free) via the public Gemini
//!   CLI OAuth client — same host, slimmer request envelope, clean model
//!   ids (see `provider::google::gemini_cli`).
//!
//! Wire behavior mirrors what the native clients do (and what CLIProxyAPI /
//! sub2api reverse-engineered):
//!
//! * authorize: accounts.google.com/o/oauth2/v2/auth, PKCE S256,
//!   access_type=offline + prompt=consent so a refresh token is issued.
//!   The antigravity client redirects to a loopback URL (state-carrying);
//!   the Gemini CLI client redirects to codeassist.google.com/authcode,
//!   which shows a bare copy/paste code without state.
//! * token: standard oauth2.googleapis.com/token (form encoded, includes
//!   the channel's published client secret).
//! * project bootstrap: loadCodeAssist returns the cloudaicompanionProject;
//!   brand-new accounts need onboardUser before the project exists.
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
use crate::provider::google::antigravity::{
    X_GOOG_API_CLIENT, antigravity_user_agent, antigravity_version,
};
use crate::provider::google::gemini_cli::GEMINI_CLI_USER_AGENT;
use crate::provider::{OAuthConfig, VendorRegistry};

const GOOGLE_PRESET_ID: &str = "google";
const ANTIGRAVITY_CHANNEL_ID: &str = "antigravity";
const GEMINI_CLI_CHANNEL_ID: &str = "gemini-cli";
const GOOGLE_GEMINI_PROTOCOL_ID: &str = "google-gemini";

/// Antigravity IDE public OAuth client secret (identical to the value shipped
/// by CLIProxyAPI and sub2api). This is a published "installed app" client
/// credential, not a private secret of Nyro.
const ANTIGRAVITY_CLIENT_SECRET: &str = "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";

/// Gemini CLI public OAuth client secret (published "installed app"
/// credential, identical to the value shipped by sub2api).
const GEMINI_CLI_CLIENT_SECRET: &str = "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";

/// Google identity endpoint (email → subject_id).
const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";

/// Code Assist control-plane host for antigravity onboarding. Inference
/// defaults to daily; the Antigravity IDE onboardUser also uses that host.
/// The Gemini CLI onboardUser stays on the prod host (sub2api behavior).
const ANTIGRAVITY_DAILY_API_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

/// How long a pending login session stays valid.
const SESSION_TTL_SECONDS: i64 = 10 * 60;
/// onboardUser polling: attempts × delay (mirrors CLIProxyAPI / sub2api).
const ONBOARD_MAX_ATTEMPTS: usize = 5;
const ONBOARD_POLL_DELAY_MS: u64 = 2_000;
// Leave headroom under the parent's 25-second refresh deadline. Only refresh
// uses these budgets; first-login exchange keeps its onboarding behavior.
const REFRESH_TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const REFRESH_ENRICHMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Which Google subscription surface a driver instance talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoogleSubscriptionChannel {
    Antigravity,
    GeminiCli,
}

/// Driver for the Google subscription OAuth channels. A single
/// implementation; the channel selects the OAuth client, client secret,
/// user agent, onboarding host, and runtime headers.
pub struct GoogleSubscriptionDriver {
    channel: GoogleSubscriptionChannel,
}

impl GoogleSubscriptionDriver {
    pub const ANTIGRAVITY: Self = Self {
        channel: GoogleSubscriptionChannel::Antigravity,
    };
    pub const GEMINI_CLI: Self = Self {
        channel: GoogleSubscriptionChannel::GeminiCli,
    };

    fn channel_id(&self) -> &'static str {
        match self.channel {
            GoogleSubscriptionChannel::Antigravity => ANTIGRAVITY_CHANNEL_ID,
            GoogleSubscriptionChannel::GeminiCli => GEMINI_CLI_CHANNEL_ID,
        }
    }

    fn client_secret(&self) -> &'static str {
        match self.channel {
            GoogleSubscriptionChannel::Antigravity => ANTIGRAVITY_CLIENT_SECRET,
            GoogleSubscriptionChannel::GeminiCli => GEMINI_CLI_CLIENT_SECRET,
        }
    }

    /// Control-plane + inference User-Agent for this channel's client.
    fn user_agent(&self) -> String {
        match self.channel {
            GoogleSubscriptionChannel::Antigravity => antigravity_user_agent(),
            GoogleSubscriptionChannel::GeminiCli => GEMINI_CLI_USER_AGENT.to_string(),
        }
    }

    fn metadata_label(&self) -> &'static str {
        match self.channel {
            GoogleSubscriptionChannel::Antigravity => "Google AI Pro (Antigravity)",
            GoogleSubscriptionChannel::GeminiCli => "Gemini CLI (Code Assist)",
        }
    }

    fn metadata_key(&self) -> &'static str {
        match self.channel {
            GoogleSubscriptionChannel::Antigravity => "google",
            GoogleSubscriptionChannel::GeminiCli => "google-gemini-cli",
        }
    }
}

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
struct SubscriptionConfig {
    oauth: &'static OAuthConfig,
    api_base_url: &'static str,
    static_models: &'static [&'static str],
}

impl GoogleSubscriptionDriver {
    fn channel_config(&self) -> Result<SubscriptionConfig> {
        let metadata = VendorRegistry::global()
            .metadata(GOOGLE_PRESET_ID)
            .ok_or_else(|| anyhow!("missing provider preset: {GOOGLE_PRESET_ID}"))?;
        let channel_id = self.channel_id();
        let channel = metadata
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .ok_or_else(|| anyhow!("missing provider channel: {GOOGLE_PRESET_ID}/{channel_id}"))?;
        let api_base_url = channel
            .base_urls
            .iter()
            .find(|entry| entry.protocol == GOOGLE_GEMINI_PROTOCOL_ID)
            .map(|entry| entry.base_url)
            .ok_or_else(|| {
                anyhow!(
                    "missing base url for protocol {GOOGLE_GEMINI_PROTOCOL_ID} in                      {GOOGLE_PRESET_ID}/{channel_id}"
                )
            })?;
        Ok(SubscriptionConfig {
            oauth: channel.oauth.as_ref().ok_or_else(|| {
                anyhow!("missing oauth config for {GOOGLE_PRESET_ID}/{channel_id}")
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
        let driver_label = self.metadata_key();
        if self.channel == GoogleSubscriptionChannel::Antigravity {
            // Prepare the Hub identity before token exchange, project bootstrap,
            // and catalog discovery construct any User-Agent headers.
            crate::provider::google::antigravity::refresh_antigravity_version(client).await;
        }
        let response = client
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", self.user_agent())
            .form(params)
            .send()
            .await
            .with_context(|| format!("{driver_label} oauth token request"))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("{driver_label} token endpoint failed: HTTP {status} {detail}");
        }
        serde_json::from_str(&body).with_context(|| format!("parse {driver_label} token response"))
    }

    /// Build the credential bundle from a token response, carrying identity
    /// metadata (project_id / email / tier_id) from prior_meta when the
    /// response itself cannot supply it (refresh flow).
    fn build_bundle(
        token: GoogleTokenResponse,
        prior_meta: Option<&Value>,
        config: &SubscriptionConfig,
    ) -> Result<CredentialBundle> {
        let access_token = token
            .access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("token response missing access_token"))?;
        let expires_in = token.expires_in.unwrap_or(3600).max(1);

        let mut meta = serde_json::Map::new();
        if let Some(prior) = prior_meta.and_then(Value::as_object) {
            // Survive refreshes: Google does not echo these back.
            for key in [
                "project_id",
                "email",
                "tier_id",
                "tier_ineligible",
                "subscription_models",
                "subscription_models_source",
                "subscription_endpoint",
            ] {
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
        let driver_label = self.metadata_key();
        let response = client
            .get(GOOGLE_USERINFO_URL)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", self.user_agent())
            .send()
            .await
            .with_context(|| format!("{driver_label} userinfo request"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("{driver_label} userinfo failed: HTTP {status} {detail}");
        }
        let parsed: Value = serde_json::from_str(&body)
            .with_context(|| format!("parse {driver_label} userinfo response"))?;
        parsed
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("{driver_label} userinfo response missing email"))
    }

    /// loadCodeAssist → cloudaicompanionProject + tier summary. Returns
    /// (project_id_opt, tier_summary_json). Both channels send the same
    /// ANTIGRAVITY ideType metadata (sub2api does the same for its gemini
    /// accounts).
    async fn load_code_assist(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        access_token: &str,
    ) -> Result<(Option<String>, Value)> {
        let driver_label = self.metadata_key();
        let endpoint = format!(
            "{}/v1internal:loadCodeAssist",
            base_url.trim_end_matches('/')
        );
        let response = client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "*/*")
            .header("User-Agent", self.user_agent())
            .json(&json!({ "metadata": { "ideType": "ANTIGRAVITY" } }))
            .send()
            .await
            .with_context(|| format!("{driver_label} loadCodeAssist request"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("{driver_label} loadCodeAssist failed: HTTP {status} {detail}");
        }
        let parsed: Value = serde_json::from_str(&body)
            .with_context(|| format!("parse {driver_label} loadCodeAssist response"))?;
        let tier_summary = json!({
            "currentTier": parsed.get("currentTier").cloned().unwrap_or(Value::Null),
            "allowedTiers": parsed.get("allowedTiers").cloned().unwrap_or(Value::Null),
            "paidTier": parsed.get("paidTier").cloned().unwrap_or(Value::Null),
            "ineligibleTiers": parsed.get("ineligibleTiers").cloned().unwrap_or(Value::Null),
        });
        Ok((extract_project_id(&parsed), tier_summary))
    }

    /// onboardUser polling until done (bounded attempts). Only needed for
    /// accounts whose loadCodeAssist has no companion project yet. The
    /// request body/host differs per channel:
    /// * antigravity — daily- host, IDE fingerprint metadata
    ///   (validated by CLIProxyAPI).
    /// * gemini-cli — prod host, `tierId` + {ideType, platform,
    ///   pluginType: GEMINI} metadata (validated by sub2api).
    async fn onboard_user(
        &self,
        client: &reqwest::Client,
        access_token: &str,
        tier_id: &str,
    ) -> Result<String> {
        let driver_label = self.metadata_key();
        let (endpoint, body) = match self.channel {
            GoogleSubscriptionChannel::Antigravity => (
                format!("{ANTIGRAVITY_DAILY_API_BASE_URL}/v1internal:onboardUser"),
                json!({
                    "tier_id": tier_id,
                    "metadata": {
                        "ide_type": "ANTIGRAVITY",
                        "ide_version": antigravity_version(),
                        "ide_name": "antigravity",
                    },
                }),
            ),
            GoogleSubscriptionChannel::GeminiCli => (
                "https://cloudcode-pa.googleapis.com/v1internal:onboardUser".to_string(),
                json!({
                    "tierId": tier_id,
                    "metadata": {
                        "ideType": "ANTIGRAVITY",
                        "platform": "PLATFORM_UNSPECIFIED",
                        "pluginType": "GEMINI",
                    },
                }),
            ),
        };
        let user_agent = self.user_agent();
        for attempt in 1..=ONBOARD_MAX_ATTEMPTS {
            let mut request = client
                .post(&endpoint)
                .header("Authorization", format!("Bearer {access_token}"))
                .header("Content-Type", "application/json")
                .header("Accept", "*/*")
                .header("User-Agent", &user_agent)
                .json(&body);
            if self.channel == GoogleSubscriptionChannel::Antigravity {
                request = request.header("X-Goog-Api-Client", X_GOOG_API_CLIENT);
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("{driver_label} onboardUser attempt {attempt}"))?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                let detail = Self::parse_error(&body).unwrap_or(body);
                bail!("{driver_label} onboardUser failed: HTTP {status} {detail}");
            }
            let parsed: Value = serde_json::from_str(&body)
                .with_context(|| format!("parse {driver_label} onboardUser response"))?;
            if parsed.get("done").and_then(Value::as_bool).unwrap_or(false) {
                if let Some(project) = parsed
                    .get("response")
                    .and_then(extract_project_id_from_value)
                {
                    return Ok(project);
                }
                bail!("{driver_label} onboardUser completed without a project id");
            }
            tokio::time::sleep(std::time::Duration::from_millis(ONBOARD_POLL_DELAY_MS)).await;
        }
        bail!("{driver_label} onboardUser did not complete after {ONBOARD_MAX_ATTEMPTS} attempts")
    }

    /// Resolve (and persist on first login) the companion project id.
    async fn resolve_project_id(
        &self,
        client: &reqwest::Client,
        config: &SubscriptionConfig,
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
                    tracing::warn!(%error, "{} loadCodeAssist failed during refresh", self.metadata_key());
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

    /// Once token refresh succeeds, optional project/catalog requests cannot
    /// invalidate that success. One shared budget bounds both phases; a missing
    /// project remains an explicit request-time diagnostic, not a lost token.
    async fn enrich_refresh_bundle(
        &self,
        client: &reqwest::Client,
        config: &SubscriptionConfig,
        bundle: &mut CredentialBundle,
        runtime_endpoint: Option<&str>,
    ) {
        let enrichment = async {
            let has_project_id = bundle
                .raw
                .get("project_id")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            let access_token = bundle.access_token.clone().unwrap_or_default();
            if !has_project_id {
                let (project_id, tier_summary) = self
                    .resolve_project_id(client, config, &access_token, Some(&bundle.raw))
                    .await?;
                if let Some(meta) = bundle.raw.as_object_mut() {
                    meta.insert("project_id".into(), Value::String(project_id));
                    if let Some(tier) = effective_tier_id(&tier_summary) {
                        meta.insert("tier_id".into(), Value::String(tier));
                    }
                    if let Some(note) = ineligible_tier_note(&tier_summary) {
                        meta.insert("tier_ineligible".into(), Value::String(note));
                    }
                }
            }
            if let Some(project_id) = bundle
                .raw
                .get("project_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                && let Some(meta) = bundle.raw.as_object_mut()
            {
                self.attach_catalog_snapshot(
                    client,
                    &access_token,
                    &project_id,
                    runtime_endpoint,
                    meta,
                )
                .await;
            }
            Ok::<(), anyhow::Error>(())
        };
        match tokio::time::timeout(REFRESH_ENRICHMENT_TIMEOUT, enrichment).await {
            Ok(Ok(())) => {}
            // Do not log raw upstream payloads or credentials for best-effort work.
            Ok(Err(_)) => tracing::debug!(
                driver = self.metadata_key(),
                "Google refresh enrichment failed; preserving successful token bundle"
            ),
            Err(_) => tracing::debug!(
                driver = self.metadata_key(),
                "Google refresh enrichment timed out; preserving successful token bundle"
            ),
        }
    }

    /// Best-effort account catalog snapshot into the credential meta; powers
    /// the antigravity effort→tier model rewrite (`subscription_models`).
    /// Never fails the calling flow; keeps the prior snapshot on error.
    async fn attach_catalog_snapshot(
        &self,
        client: &reqwest::Client,
        access_token: &str,
        project_id: &str,
        runtime_endpoint: Option<&str>,
        meta: &mut serde_json::Map<String, Value>,
    ) {
        use crate::provider::google::antigravity::{
            ANTIGRAVITY_INFERENCE_BASE_URL, CatalogSource, GOOGLE_SUBSCRIPTION_PROD_BASE_URL,
            fetch_available_models, write_catalog_snapshot,
        };
        // Auth contexts have no provider row yet. Use the same channel default
        // or explicit credential opt-in; admin discovery uses the bound runtime.
        let endpoint = runtime_endpoint
            .or_else(|| meta.get("subscription_endpoint").and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(match self.channel {
                GoogleSubscriptionChannel::Antigravity => ANTIGRAVITY_INFERENCE_BASE_URL,
                GoogleSubscriptionChannel::GeminiCli => GOOGLE_SUBSCRIPTION_PROD_BASE_URL,
            });
        let source = CatalogSource::new(endpoint, project_id, &self.user_agent(), access_token);
        let Ok(models) = fetch_available_models(
            client,
            access_token,
            project_id,
            &source.user_agent,
            &source.endpoint,
        )
        .await
        else {
            return;
        };
        let ids = models.into_iter().map(|model| model.id).collect::<Vec<_>>();
        write_catalog_snapshot(meta, &ids, &source);
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

/// Tier id a tier field may carry either as a bare string (legacy) or as an
/// object with an `id` member - sub2api's TierInfo/PaidTierInfo decoding
/// accepts both shapes, so this driver does too.
fn tier_id_from_value(value: &Value) -> Option<String> {
    let id = match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(_) => value
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        _ => None,
    }?;
    (!id.trim().is_empty()).then_some(id)
}

/// Effective subscription tier for the credential: prefer `paidTier` (the
/// subscription entitlement) over `currentTier` (the operating tier).
/// Google reports both for consumer accounts - an active Google One AI
/// Pro/Ultra subscription carries `paidTier: g1-pro-tier`/`g1-ultra-tier`
/// while `currentTier` can remain `free-tier`, so displaying `currentTier`
/// alone mislabels paid accounts as free. sub2api's `GetTier()` applies the
/// same precedence.
fn effective_tier_id(tier_summary: &Value) -> Option<String> {
    let paid = tier_summary
        .get("paidTier")
        .filter(|value| !value.is_null())
        .and_then(tier_id_from_value);
    if paid.is_some() {
        return paid;
    }
    tier_summary
        .get("currentTier")
        .filter(|value| !value.is_null())
        .and_then(tier_id_from_value)
}

/// Compact record of tiers the account is barred from, with Google's reason
/// codes - the diagnostic for the "paid subscription not recognized" case
/// (`paidTier` absent while `ineligibleTiers` explains why, e.g.
/// `INELIGIBLE_ACCOUNT`). Format: `id:reasonCode` entries joined by `, `.
fn ineligible_tier_note(tier_summary: &Value) -> Option<String> {
    let tiers = tier_summary
        .get("ineligibleTiers")
        .and_then(Value::as_array)?;
    let entries: Vec<String> = tiers
        .iter()
        .filter_map(|tier| {
            let id = tier.get("id").and_then(Value::as_str)?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let reason = tier
                .get("reasonCode")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map_or_else(|| "UNKNOWN".to_string(), ToString::to_string);
            Some(format!("{id}:{reason}"))
        })
        .collect();
    (!entries.is_empty()).then(|| entries.join(", "))
}

#[async_trait]
impl AuthDriver for GoogleSubscriptionDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: self.metadata_key(),
            label: self.metadata_label(),
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = self.channel_config()?;
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
        let config = self.channel_config()?;
        let state: PkceAuthState = parse_session_state(session)?;
        let callback = parse_oauth_callback(&input)?;
        // The antigravity loopback redirect always carries state. The Gemini
        // CLI client redirects to codeassist.google.com/authcode, where
        // Google renders a bare copy/paste code with no state; PKCE binds
        // that code to this session's verifier, so a missing state is
        // acceptable there (validated whenever the paste includes one).
        match self.channel {
            GoogleSubscriptionChannel::Antigravity => {
                validate_callback_state(&state.state, callback.state.as_deref(), "google")?
            }
            GoogleSubscriptionChannel::GeminiCli => {
                if callback
                    .state
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|value| !value.is_empty())
                {
                    validate_callback_state(&state.state, callback.state.as_deref(), "google")?;
                }
            }
        }
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
                    ("client_secret", self.client_secret()),
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
        // Resolve the stable subscriber identity at first login as well as
        // refresh; otherwise quota ownership would switch from a random legacy
        // scope to email on the first routine token renewal.
        bundle.subject_id = Some(email.clone());
        if let Some(meta) = bundle.raw.as_object_mut() {
            meta.insert("email".to_string(), Value::String(email));
            meta.insert("project_id".to_string(), Value::String(project_id.clone()));
            if let Some(tier) = effective_tier_id(&tier_summary) {
                meta.insert("tier_id".to_string(), Value::String(tier));
            }
            if let Some(note) = ineligible_tier_note(&tier_summary) {
                meta.insert("tier_ineligible".to_string(), Value::String(note));
            }
            self.attach_catalog_snapshot(&client, &access_token, &project_id, None, meta)
                .await;
        }

        Ok(bundle)
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let config = self.channel_config()?;
        let driver_label = self.metadata_key();
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{driver_label} refresh token is missing"))?;
        let client = required_http_client(ctx.http_client)?;

        let token = tokio::time::timeout(
            REFRESH_TOKEN_TIMEOUT,
            self.token_request(
                &client,
                config.oauth.token_url,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", config.oauth.client_id),
                    ("client_secret", self.client_secret()),
                ],
            ),
        )
        .await
        .map_err(|_| anyhow!("{driver_label} token refresh timed out"))??;

        let mut bundle = Self::build_bundle(token, Some(&credential.meta), &config)?;
        if bundle.refresh_token.is_none() {
            bundle.refresh_token = Some(refresh_token.to_string());
        }

        self.enrich_refresh_bundle(
            &client,
            &config,
            &mut bundle,
            ctx.metadata
                .get("google_subscription_endpoint")
                .and_then(Value::as_str),
        )
        .await;

        Ok(bundle)
    }

    fn bind_runtime(
        &self,
        provider: &Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let config = self.channel_config()?;
        let access_token = credential
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "{} access token is empty in bind_runtime",
                    self.metadata_key()
                )
            })?;

        let mut extra_headers = HashMap::new();
        extra_headers.insert(
            "authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        extra_headers.insert("user-agent".to_string(), self.user_agent());
        if self.channel == GoogleSubscriptionChannel::Antigravity {
            // Antigravity control-plane fingerprint header; the Gemini CLI
            // client does not send it.
            extra_headers.insert(
                "x-goog-api-client".to_string(),
                X_GOOG_API_CLIENT.to_string(),
            );
        }

        // The channel's curated model list is the fallback catalog for when
        // dynamic `fetchAvailableModels` discovery is unavailable; the admin
        // discovery paths query the per-account catalog first.
        let static_models_override: Option<Vec<String>> = if config.static_models.is_empty() {
            None
        } else {
            Some(config.static_models.iter().map(|s| s.to_string()).collect())
        };
        // Both channels pin models.dev as the labeled discovery source so the
        // WebUI test flow proceeds to the model-list step; the per-account
        // dynamic catalog and the static list take priority over it.
        let models_source_override = Some("ai://models.dev/google".to_string());

        // Catalog and inference share the same endpoint policy. Onboarding
        // control-plane calls keep their separate host selection.
        let inference_base_url =
            crate::provider::google::antigravity::subscription_endpoint(provider, Some(credential))
                .base_url;

        Ok(RuntimeBinding {
            base_url_override: Some(inference_base_url.to_string()),
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

    fn test_provider(channel: &str) -> Provider {
        Provider {
            keys: Vec::new(),
            id: "test".into(),
            name: "test".into(),
            vendor: Some("google".into()),
            protocol: "google-gemini".into(),
            base_url: String::new(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("google".into()),
            channel: Some(channel.into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: Default::default(),
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn stored_credential() -> StoredCredential {
        StoredCredential {
            access_token: Some("ya29.token".into()),
            meta: json!({"project_id": "cloudaicompanion-123"}),
            ..Default::default()
        }
    }

    #[test]
    fn google_catalog_runtime_honors_explicit_endpoint_without_changing_legacy_default() {
        for (channel, driver, expected) in [
            (
                "antigravity",
                GoogleSubscriptionDriver::ANTIGRAVITY,
                crate::provider::google::antigravity::ANTIGRAVITY_INFERENCE_BASE_URL,
            ),
            (
                "gemini-cli",
                GoogleSubscriptionDriver::GEMINI_CLI,
                "https://cloudcode-pa.googleapis.com",
            ),
        ] {
            let mut provider = test_provider(channel);
            provider.base_url = "https://cloudcode-pa.googleapis.com".into();
            let mut credential = stored_credential();
            assert_eq!(
                driver
                    .bind_runtime(&provider, &credential)
                    .unwrap()
                    .base_url_override
                    .as_deref(),
                Some(expected)
            );
            provider.base_url = "http://127.0.0.1:12345/custom/".into();
            assert_eq!(
                driver
                    .bind_runtime(&provider, &credential)
                    .unwrap()
                    .base_url_override
                    .as_deref(),
                Some("http://127.0.0.1:12345/custom")
            );
            credential.meta["subscription_endpoint"] = json!("https://cloudcode-pa.googleapis.com");
            assert_eq!(
                driver
                    .bind_runtime(&provider, &credential)
                    .unwrap()
                    .base_url_override
                    .as_deref(),
                Some("https://cloudcode-pa.googleapis.com")
            );
        }
    }

    #[tokio::test]
    async fn google_catalog_auth_snapshot_matches_admin_source_and_empty_failure_semantics() {
        use crate::provider::google::antigravity::{
            catalog_source_for_runtime, write_catalog_snapshot,
        };
        for (channel, driver) in [
            ("antigravity", GoogleSubscriptionDriver::ANTIGRAVITY),
            ("gemini-cli", GoogleSubscriptionDriver::GEMINI_CLI),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let app = axum::Router::new().route(
                "/v1internal:fetchAvailableModels",
                axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                    if body["project"] == "failure" {
                        (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({})))
                    } else {
                        (axum::http::StatusCode::OK, axum::Json(json!({"models":{}})))
                    }
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let mut provider = test_provider(channel);
            provider.base_url = endpoint.clone();
            let mut credential = stored_credential();
            let binding = driver.bind_runtime(&provider, &credential).unwrap();
            let source = catalog_source_for_runtime(&provider, &credential, &binding).unwrap();
            write_catalog_snapshot(
                credential.meta.as_object_mut().unwrap(),
                &["prior-model".into()],
                &source,
            );
            let mut meta = credential.meta.as_object().unwrap().clone();
            driver
                .attach_catalog_snapshot(
                    &client,
                    "ya29.token",
                    "failure",
                    Some(&endpoint),
                    &mut meta,
                )
                .await;
            assert_eq!(
                meta,
                credential.meta.as_object().unwrap().clone(),
                "failure must retain prior source without relabeling"
            );
            driver
                .attach_catalog_snapshot(
                    &client,
                    "ya29.token",
                    "cloudaicompanion-123",
                    Some(&endpoint),
                    &mut meta,
                )
                .await;
            server.abort();
            write_catalog_snapshot(credential.meta.as_object_mut().unwrap(), &[], &source);
            assert_eq!(
                meta,
                credential.meta.as_object().unwrap().clone(),
                "auth and admin use identical snapshot format"
            );
        }
    }

    #[tokio::test]
    async fn google_catalog_refresh_enrichment_timeout_preserves_successful_tokens() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let app = axum::Router::new().route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let calls = captured.clone();
                    async move {
                        assert_eq!(headers["authorization"], "Bearer newly-rotated-access");
                        assert_eq!(body["project"], "fixture-project");
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Never answer: only the production enrichment budget can
                        // finish this flow, before the parent's 25-second deadline.
                        std::future::pending::<axum::Json<Value>>().await
                    }
                },
            ),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = GoogleSubscriptionDriver::GEMINI_CLI;
        let config = driver.channel_config().unwrap();
        let prior = json!({"project_id":"fixture-project", "subscription_models":["prior-model"],
            "subscription_models_source":{"credential_fingerprint":"prior-token"}});
        let mut bundle = GoogleSubscriptionDriver::build_bundle(
            GoogleTokenResponse {
                access_token: Some("newly-rotated-access".into()),
                refresh_token: Some("newly-rotated-refresh".into()),
                expires_in: Some(3600),
                scope: None,
            },
            Some(&prior),
            &config,
        )
        .unwrap();
        let expected = serde_json::to_value(&bundle).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let completed = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            driver.enrich_refresh_bundle(&client, &config, &mut bundle, Some(&endpoint)),
        )
        .await;
        server.abort();
        assert!(
            completed.is_ok(),
            "optional catalog must not consume the parent refresh deadline"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_value(&bundle).unwrap(),
            expected,
            "a slow catalog cannot discard the successful token bundle or relabel the prior source"
        );
        assert_eq!(REFRESH_TOKEN_TIMEOUT, std::time::Duration::from_secs(20));
        assert_eq!(
            REFRESH_ENRICHMENT_TIMEOUT,
            std::time::Duration::from_secs(2)
        );
        assert!(
            REFRESH_TOKEN_TIMEOUT + REFRESH_ENRICHMENT_TIMEOUT < std::time::Duration::from_secs(25)
        );
    }

    #[tokio::test]
    async fn google_catalog_refresh_project_recovery_failure_preserves_successful_tokens() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let app = axum::Router::new().fallback(move || {
            captured.fetch_add(1, Ordering::SeqCst);
            async { axum::http::StatusCode::SERVICE_UNAVAILABLE }
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // All requests, even a mistaken endpoint, terminate on the fixture.
        // The helper skips token/Hub preparation entirely.
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(&endpoint).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let driver = GoogleSubscriptionDriver::GEMINI_CLI;
        let mut config = driver.channel_config().unwrap();
        config.api_base_url = "http://project-fixture.invalid";
        let mut bundle = GoogleSubscriptionDriver::build_bundle(
            GoogleTokenResponse {
                access_token: Some("newly-rotated-access".into()),
                refresh_token: Some("newly-rotated-refresh".into()),
                expires_in: Some(3600),
                scope: None,
            },
            None,
            &config,
        )
        .unwrap();
        let expected = serde_json::to_value(&bundle).unwrap();
        driver
            .enrich_refresh_bundle(&client, &config, &mut bundle, Some(&endpoint))
            .await;
        server.abort();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(serde_json::to_value(&bundle).unwrap(), expected);
        assert!(
            bundle.raw.get("project_id").is_none(),
            "missing project remains diagnostic, not a failed token refresh"
        );
    }

    #[test]
    fn google_catalog_refresh_does_not_relabel_prior_snapshot_for_new_token() {
        let config = GoogleSubscriptionDriver::ANTIGRAVITY
            .channel_config()
            .unwrap();
        let prior = json!({"project_id":"fixture-project", "subscription_endpoint":"https://override.invalid", "subscription_models":["old"], "subscription_models_source":{"credential_fingerprint":"old"}});
        let bundle = GoogleSubscriptionDriver::build_bundle(
            GoogleTokenResponse {
                access_token: Some("new-token".into()),
                refresh_token: None,
                expires_in: None,
                scope: None,
            },
            Some(&prior),
            &config,
        )
        .unwrap();
        assert_eq!(
            bundle.raw["subscription_endpoint"],
            prior["subscription_endpoint"]
        );
        assert_eq!(
            bundle.raw["subscription_models_source"],
            prior["subscription_models_source"]
        );
    }

    #[test]
    fn antigravity_config_loads_from_vendor_registry() {
        let config = GoogleSubscriptionDriver::ANTIGRAVITY
            .channel_config()
            .unwrap();
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
    fn gemini_cli_config_loads_from_vendor_registry() {
        let config = GoogleSubscriptionDriver::GEMINI_CLI
            .channel_config()
            .unwrap();
        assert_eq!(
            config.oauth.client_id,
            "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
        );
        // Copy/paste code page, not a loopback callback.
        assert_eq!(
            config.oauth.redirect_uri,
            "https://codeassist.google.com/authcode"
        );
        // Built-in client rejects generative-language / drive scopes.
        assert!(!config.oauth.scope.contains("generative-language"));
        assert!(!config.oauth.scope.contains("drive"));
        assert_eq!(config.api_base_url, "https://cloudcode-pa.googleapis.com");
        assert!(
            config
                .static_models
                .iter()
                .any(|m| *m == "gemini-3.6-flash")
        );
    }

    #[test]
    fn metadata_keys_distinguish_channels() {
        assert_eq!(
            GoogleSubscriptionDriver::ANTIGRAVITY.metadata().key,
            "google"
        );
        assert_eq!(
            GoogleSubscriptionDriver::GEMINI_CLI.metadata().key,
            "google-gemini-cli"
        );
        assert_eq!(
            GoogleSubscriptionDriver::GEMINI_CLI.metadata().label,
            "Gemini CLI (Code Assist)"
        );
    }

    #[test]
    fn build_bundle_carries_prior_meta_into_refresh() {
        let config = GoogleSubscriptionDriver::ANTIGRAVITY
            .channel_config()
            .unwrap();
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
        let bundle = GoogleSubscriptionDriver::build_bundle(token, Some(&prior), &config).unwrap();
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
    fn effective_tier_id_prefers_paid_tier_over_current() {
        let summary = json!({
            "currentTier": {"id": "free-tier"},
            "paidTier": {"id": "g1-pro-tier"},
        });
        assert_eq!(effective_tier_id(&summary).as_deref(), Some("g1-pro-tier"));
    }

    #[test]
    fn effective_tier_id_accepts_string_and_object_forms() {
        let string_paid = json!({
            "paidTier": "g1-ultra-tier",
            "currentTier": {"id": "free-tier"},
        });
        assert_eq!(
            effective_tier_id(&string_paid).as_deref(),
            Some("g1-ultra-tier")
        );
        let legacy_current = json!({"currentTier": "individual"});
        assert_eq!(
            effective_tier_id(&legacy_current).as_deref(),
            Some("individual")
        );
    }

    #[test]
    fn effective_tier_id_falls_back_to_current_then_none() {
        let current_only = json!({"currentTier": {"id": "free-tier"}});
        assert_eq!(
            effective_tier_id(&current_only).as_deref(),
            Some("free-tier")
        );
        assert_eq!(effective_tier_id(&json!({})), None);
        // Null tiers (loadCodeAssist unavailable during refresh) resolve to none
        // so the prior meta tier_id survives instead of being overwritten.
        let null_summary = json!({"currentTier": null, "paidTier": null});
        assert_eq!(effective_tier_id(&null_summary), None);
    }

    #[test]
    fn ineligible_tier_note_collects_reason_codes() {
        let summary = json!({
            "ineligibleTiers": [
                {"id": "g1-pro-tier", "reasonCode": "INELIGIBLE_ACCOUNT"},
                {"id": "g1-ultra-tier"},
            ]
        });
        assert_eq!(
            ineligible_tier_note(&summary).as_deref(),
            Some("g1-pro-tier:INELIGIBLE_ACCOUNT, g1-ultra-tier:UNKNOWN")
        );
        assert_eq!(ineligible_tier_note(&json!({"ineligibleTiers": []})), None);
        assert_eq!(ineligible_tier_note(&json!({})), None);
    }

    #[test]
    fn bind_runtime_sets_antigravity_headers_and_models() {
        let provider = test_provider("antigravity");
        let credential = stored_credential();
        let binding = GoogleSubscriptionDriver::ANTIGRAVITY
            .bind_runtime(&provider, &credential)
            .unwrap();
        assert_eq!(
            binding.extra_headers.get("authorization").unwrap(),
            "Bearer ya29.token"
        );
        // Hub version, platform and changelist must describe the same release;
        // the old IDE-style `antigravity/<version> windows/amd64` is not Hub.
        assert_eq!(
            binding.extra_headers.get("user-agent").unwrap(),
            "antigravity/hub/2.29.0 (aidev_client; os_type=darwin; arch=arm64; cl=1013060670)"
        );
        assert_eq!(
            binding.extra_headers.get("x-goog-api-client").unwrap(),
            "gl-node/22.21.1"
        );
        assert!(binding.disable_default_auth);
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com"),
            "antigravity inference must ride the daily host (consumer tier gating)"
        );
        assert_eq!(
            binding.models_source_override.as_deref(),
            Some("ai://models.dev/google")
        );
        let models = binding.static_models_override.unwrap();
        // Active catalog generations only: 3.8 flash tiers and Gemini 4.
        assert!(models.iter().any(|m| m == "gemini-3.8-flash-high"));
        assert!(models.iter().any(|m| m == "gemini-3.8-flash-tiered"));
        // Gemini 4 Argon / Hydrogen serve tier ids only (bare ids 404).
        assert!(models.iter().any(|m| m == "gemini-4-argon-high"));
        assert!(models.iter().any(|m| m == "gemini-4-hydrogen-xhigh"));
        assert!(!models.iter().any(|m| m == "gemini-4-argon"));
        assert!(!models.iter().any(|m| m == "gemini-4-hydrogen"));
        // Retired lines stay out of the fallback list — dynamic discovery
        // resurfaces them per-account when still served.
        assert!(!models.iter().any(|m| *m == "gemini-2.5-pro"));
        assert!(!models.iter().any(|m| m == "gemini-3.6-flash-high"));
        assert!(!models.iter().any(|m| m == "gemini-3.7-flash-high"));
    }

    #[test]
    fn bind_runtime_sets_gemini_cli_headers_and_models() {
        let provider = test_provider("gemini-cli");
        let credential = stored_credential();
        let binding = GoogleSubscriptionDriver::GEMINI_CLI
            .bind_runtime(&provider, &credential)
            .unwrap();
        assert_eq!(
            binding.extra_headers.get("authorization").unwrap(),
            "Bearer ya29.token"
        );
        assert_eq!(
            binding.extra_headers.get("user-agent").unwrap(),
            "GeminiCLI/0.1.5 (Windows; AMD64)"
        );
        // The Gemini CLI client does not send the antigravity fingerprint.
        assert!(binding.extra_headers.get("x-goog-api-client").is_none());
        assert!(binding.disable_default_auth);
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://cloudcode-pa.googleapis.com")
        );
        assert_eq!(
            binding.models_source_override.as_deref(),
            Some("ai://models.dev/google")
        );
        let models = binding.static_models_override.unwrap();
        assert!(models.iter().any(|m| m == "gemini-3.6-flash"));
        assert!(models.iter().any(|m| m == "gemini-3.7-flash"));
        // Antigravity-only tier-suffixed ids must not leak into this channel.
        assert!(!models.iter().any(|m| m == "gemini-3.6-flash-high"));
    }
}
