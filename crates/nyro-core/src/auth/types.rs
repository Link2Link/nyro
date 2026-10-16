use std::collections::HashMap;

use anyhow::bail;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::models::Provider;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    ApiKey,
    OAuthAuthCodePkce,
    OAuthDeviceCode,
    SetupToken,
    Custom,
}

impl AuthScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuthAuthCodePkce => "oauth_auth_code_pkce",
            Self::OAuthDeviceCode => "oauth_device_code",
            Self::SetupToken => "setup_token",
            Self::Custom => "custom",
        }
    }
}

impl std::fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AuthScheme {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "api_key" | "apikey" => Ok(Self::ApiKey),
            "oauth_auth_code_pkce" | "oauth-pkce" | "oauth_pkce" => Ok(Self::OAuthAuthCodePkce),
            "oauth_device_code" | "device_code" | "oauth-device" => Ok(Self::OAuthDeviceCode),
            "setup_token" | "setup-token" => Ok(Self::SetupToken),
            "custom" => Ok(Self::Custom),
            other => bail!("unsupported auth scheme: {other}"),
        }
    }
}

/// Completion transport is independent of the OAuth grant scheme.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthCompletionMode {
    ManualExchange,
    Poll,
}

impl AuthCompletionMode {
    pub fn legacy_scheme(scheme: &str) -> Self {
        if scheme == AuthScheme::OAuthAuthCodePkce.as_str()
            || scheme == AuthScheme::SetupToken.as_str()
        {
            Self::ManualExchange
        } else {
            Self::Poll
        }
    }
}

/// Process-local controls never enter an admin DTO or persisted credential.
#[derive(Debug)]
pub struct AuthSessionControl {
    pub generation: u64,
    pub in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub cancelled: bool,
    pub cancellation: tokio_util::sync::CancellationToken,
    pub next_poll_at: Option<tokio::time::Instant>,
    pub retain_until: tokio::time::Instant,
}

impl Default for AuthSessionControl {
    fn default() -> Self {
        Self {
            generation: 0,
            in_flight: Default::default(),
            cancelled: false,
            cancellation: tokio_util::sync::CancellationToken::new(),
            next_poll_at: None,
            retain_until: tokio::time::Instant::now() + std::time::Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthSessionStatus {
    Pending,
    Ready,
    Error,
    Cancelled,
}

impl AuthSessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthBindingStatus {
    Pending,
    Connected,
    Error,
    Disconnected,
}

impl AuthBindingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected => "connected",
            Self::Error => "error",
            Self::Disconnected => "disconnected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDriverMetadata {
    pub key: &'static str,
    pub label: &'static str,
    pub scheme: AuthScheme,
    pub supports_new_provider: bool,
    pub supports_existing_provider: bool,
}

/// Kiro's browser login and legacy credential import share one provider identity.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum KiroLoginMethod {
    #[default]
    Import,
    Google,
    Github,
    BuilderId,
    Idc,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KiroLoginOptions {
    #[serde(default)]
    pub method: KiroLoginMethod,
    /// Generation/profile region, independent of the enterprise OIDC region.
    pub region: Option<String>,
    pub idc_region: Option<String>,
    pub start_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StartAuthContext {
    pub provider_id: Option<String>,
    pub provider: Option<Provider>,
    pub use_proxy: bool,
    pub redirect_uri: Option<String>,
    pub requested_scopes: Vec<String>,
    pub kiro: Option<KiroLoginOptions>,
    pub metadata: Value,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshAuthContext {
    pub use_proxy: bool,
    pub metadata: Value,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Default)]
pub struct ExchangeAuthContext {
    pub use_proxy: bool,
    pub metadata: Value,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthExchangeInput {
    pub code: Option<String>,
    pub callback_url: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeBinding {
    pub base_url_override: Option<String>,
    pub extra_headers: HashMap<String, String>,
    pub model_aliases: HashMap<String, String>,
    pub models_source_override: Option<String>,
    pub disable_default_auth: bool,
    /// When `Some`, the admin model-discovery paths return this exact
    /// list and skip every URL/HTTP fallback. OAuth drivers (Claude
    /// Code, future Codex tiers) use this to ship a curated allow-list
    /// since their upstream `/v1/models` either does not accept the
    /// minted Bearer or returns models the OAuth subscription cannot
    /// actually run.
    #[serde(default)]
    pub static_models_override: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialBundle {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredCredential {
    pub driver_key: String,
    pub scheme: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub meta: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProgress {
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSessionInitData {
    pub session_id: String,
    pub vendor: String,
    pub scheme: String,
    pub auth_url: String,
    pub requires_manual_code: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_mode: Option<AuthCompletionMode>,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: i64,
    pub interval: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthSessionStatusData {
    Pending {
        scheme: String,
        auth_url: String,
        requires_manual_code: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_mode: Option<AuthCompletionMode>,
        expires_in: i64,
        interval: i32,
        user_code: String,
        verification_uri_complete: String,
    },
    Ready {
        expires_in: i64,
        resource_url: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthPollState {
    Pending(AuthProgress),
    Ready(CredentialBundle),
    Error { code: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    #[serde(skip)]
    pub generation: u64,
    pub id: String,
    pub provider_id: Option<String>,
    pub driver_key: String,
    pub scheme: String,
    pub status: String,
    pub use_proxy: bool,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub state_json: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAuthSession {
    pub provider_id: Option<String>,
    pub driver_key: String,
    pub scheme: String,
    pub status: String,
    pub use_proxy: bool,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub state_json: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateAuthSession {
    pub status: Option<String>,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub state_json: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
    pub last_error: Option<String>,
}

#[async_trait]
pub trait AuthDriver: Send + Sync {
    fn metadata(&self) -> AuthDriverMetadata;

    fn completion_mode(&self) -> AuthCompletionMode {
        AuthCompletionMode::legacy_scheme(self.metadata().scheme.as_str())
    }

    /// A provider may support multiple login grants while keeping its identity.
    fn supports_session_scheme(&self, scheme: &str) -> bool {
        scheme == self.metadata().scheme.as_str()
    }

    async fn start(&self, _ctx: StartAuthContext) -> anyhow::Result<CreateAuthSession> {
        bail!("{} start flow is not implemented yet", self.metadata().key)
    }

    async fn poll(
        &self,
        _session: &AuthSession,
        _ctx: RefreshAuthContext,
    ) -> anyhow::Result<AuthPollState> {
        bail!("{} poll flow is not implemented yet", self.metadata().key)
    }

    async fn exchange(
        &self,
        _session: &AuthSession,
        _input: AuthExchangeInput,
        _ctx: ExchangeAuthContext,
    ) -> anyhow::Result<CredentialBundle> {
        bail!(
            "{} exchange flow is not implemented yet",
            self.metadata().key
        )
    }

    async fn refresh(
        &self,
        _credential: &StoredCredential,
        _ctx: RefreshAuthContext,
    ) -> anyhow::Result<CredentialBundle> {
        bail!(
            "{} refresh flow is not implemented yet",
            self.metadata().key
        )
    }

    fn bind_runtime(
        &self,
        _provider: &Provider,
        _credential: &StoredCredential,
    ) -> anyhow::Result<RuntimeBinding> {
        Ok(RuntimeBinding::default())
    }
}
