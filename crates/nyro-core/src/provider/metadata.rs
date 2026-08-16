//! Vendor metadata types.
//!
//! Moved from `protocol/vendor/types.rs` (PR-15). These structures are the
//! runtime source of truth for provider presets. Each vendor module owns a
//! `const METADATA: VendorMetadata` and registers itself via `inventory::submit!`.

use serde::Serialize;

/// Where to fetch model capability data for a provider channel.
///
/// This is a compile-time preset concern — not user-configurable. The admin
/// layer reads this from the vendor preset to decide how to resolve capabilities
/// without consulting a DB column.
#[derive(Debug, Clone, Copy)]
pub enum CapabilitiesSource {
    /// Query models.dev for the given vendor key (empty string = all vendors).
    ModelsDev(&'static str),
    /// Query an HTTP endpoint (e.g. Ollama `/api/show`, OpenRouter `/v1/models`).
    Http(&'static str),
    /// Fuzzy-match the model name against all of models.dev.
    Auto,
}

/// Bilingual label.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Label {
    pub zh: &'static str,
    pub en: &'static str,
}

/// Authentication mode advertised to the WebUI.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    ApiKey,
    OAuth,
    SetupToken,
}

/// (protocol_alias, base_url) pair.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProtocolBaseUrl {
    pub protocol: &'static str,
    pub base_url: &'static str,
}

/// Per-protocol authentication-scheme override for a channel.
///
/// Some vendors expose an Anthropic-compatible endpoint that authenticates
/// with `Authorization: Bearer` instead of the Anthropic-standard `x-api-key`
/// (e.g. Volcengine Ark). The WebUI seeds provider protocol endpoints with
/// this scheme so both the connectivity probe and proxy egress send the
/// header the vendor actually expects.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProtocolAuthScheme {
    pub protocol: &'static str,
    pub auth_scheme: &'static str,
}

/// OAuth configuration for a channel.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    pub auth_base_url: &'static str,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub client_id: &'static str,
    pub redirect_uri: &'static str,
    pub scope: &'static str,
}

/// Runtime hints used by OAuth drivers (currently only Codex).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub api_base_url: &'static str,
    pub models_url: &'static str,
    pub models_client_version: &'static str,
}

/// One channel under a vendor (e.g. `openai/default`, `openai/codex`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDef {
    pub id: &'static str,
    pub label: Label,
    #[serde(
        serialize_with = "serialize_base_urls",
        skip_serializing_if = "<[ProtocolBaseUrl]>::is_empty"
    )]
    pub base_urls: &'static [ProtocolBaseUrl],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_source: Option<&'static str>,
    /// Internal preset capability strategy — never serialized to the wire API.
    #[serde(skip)]
    pub capabilities_source: CapabilitiesSource,
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    pub static_models: &'static [&'static str],
    pub auth_mode: AuthMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeConfig>,
    /// Shared-key multi-protocol channel: one API key is valid for every
    /// protocol in `base_urls`. When true, the WebUI renders a single API
    /// key field plus per-protocol checkboxes instead of the single-protocol
    /// form, and creates the provider in adaptive mode with one endpoint per
    /// checked protocol (all carrying the same key).
    #[serde(skip_serializing_if = "is_false")]
    pub shared_key_protocols: bool,
    /// Per-protocol auth-scheme overrides (serialized as
    /// `authSchemes: {protocol: scheme}`); `None` = protocol defaults.
    #[serde(
        serialize_with = "serialize_auth_schemes",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_schemes: Option<&'static [ProtocolAuthScheme]>,
}

/// Top-level vendor entry. One `VendorMetadata` per vendor.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorMetadata {
    pub id: &'static str,
    pub label: Label,
    pub icon: &'static str,
    pub default_protocol: &'static str,
    pub channels: &'static [ChannelDef],
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn serialize_base_urls<S>(base_urls: &&[ProtocolBaseUrl], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(base_urls.len()))?;
    for entry in base_urls.iter() {
        map.serialize_entry(entry.protocol, entry.base_url)?;
    }
    map.end()
}

fn serialize_auth_schemes<S>(
    auth_schemes: &Option<&'static [ProtocolAuthScheme]>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let entries = auth_schemes.unwrap_or_default();
    let mut map = serializer.serialize_map(Some(entries.len()))?;
    for entry in entries.iter() {
        map.serialize_entry(entry.protocol, entry.auth_scheme)?;
    }
    map.end()
}
