//! Kiro credential model shared by the auth driver and the chat runtime.
//!
//! A credential is stored as the OAuth record's `access_token`/`refresh_token`
//! plus a flat JSON `meta` object (see [`KiroCredential::to_meta`]). Import
//! formats and refresh rules follow AIClient2API's Kiro provider: Kiro IDE
//! keeps the token in `kiro-auth-token.json` and, for AWS IAM Identity Center
//! and Builder ID accounts, the client registration in a sibling file.
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::auth::types::StoredCredential;

pub const DEFAULT_REGION: &str = "us-east-1";
const MAX_IMPORT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Google/GitHub sign-in through Kiro's own auth service.
    Social,
    /// AWS Builder ID through AWS IAM Identity Center OIDC.
    BuilderId,
    /// Enterprise AWS IAM Identity Center through OIDC.
    Idc,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Social => "social",
            Self::BuilderId => "builder-id",
            Self::Idc => "idc",
        }
    }

    pub fn is_social(self) -> bool {
        self == Self::Social
    }

    /// Accept the spellings Kiro IDE, the CLI and AIClient2API write.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw
            .trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str()
        {
            "social" | "google" | "github" => Some(Self::Social),
            "builder-id" | "builderid" => Some(Self::BuilderId),
            "idc" | "iam-identity-center" | "enterprise" | "identity-center" => Some(Self::Idc),
            _ => None,
        }
    }
}

/// A region is interpolated into hostnames, so it must stay a plain label and
/// can never smuggle a different host into an `https://…amazonaws.com` URL.
pub fn valid_region(region: &str) -> bool {
    let bytes = region.as_bytes();
    (3..=24).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.last().is_some_and(|b| b.is_ascii_alphanumeric())
        && !region.contains("--")
}

/// Region of an AWS ARN (`arn:aws:codewhisperer:us-east-1:123:profile/X`).
pub fn arn_region(arn: &str) -> Option<&str> {
    let region = arn.split(':').nth(3)?;
    valid_region(region).then_some(region)
}

/// `https://q.<region>.amazonaws.com` → `<region>`; anything else is `None`.
pub fn region_from_origin(base_url: &str) -> Option<&str> {
    let host = base_url
        .trim()
        .trim_end_matches('/')
        .strip_prefix("https://")?;
    let region = host.strip_prefix("q.")?.strip_suffix(".amazonaws.com")?;
    valid_region(region).then_some(region)
}

pub fn is_api_key(token: &str) -> bool {
    token.trim_start().starts_with("ksk_")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KiroCredential {
    pub auth_method: AuthMethod,
    /// Region of the generation API/profile, not the social authentication service.
    pub region: String,
    /// Region of the AWS OIDC endpoint (Builder ID / IdC refresh).
    pub idc_region: String,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub profile_arn: Option<String>,
    pub start_url: Option<String>,
}

fn text<'a>(meta: &'a Value, key: &str) -> Option<&'a str> {
    meta.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

impl KiroCredential {
    /// Rebuild from a stored credential. Missing fields get safe defaults so a
    /// hand-edited or older record still resolves to a usable credential.
    pub fn from_stored(credential: &StoredCredential) -> Self {
        let meta = &credential.meta;
        let client_id = text(meta, "client_id").map(str::to_string);
        let client_secret = text(meta, "client_secret").map(str::to_string);
        let auth_method = text(meta, "auth_method")
            .and_then(AuthMethod::parse)
            .unwrap_or(if client_id.is_some() && client_secret.is_some() {
                AuthMethod::Idc
            } else {
                AuthMethod::Social
            });
        let profile_arn = text(meta, "profile_arn").map(str::to_string);
        let region = text(meta, "region")
            .filter(|region| valid_region(region))
            .or_else(|| profile_arn.as_deref().and_then(arn_region))
            .unwrap_or(DEFAULT_REGION)
            .to_string();
        let idc_region = text(meta, "idc_region")
            .filter(|region| valid_region(region))
            .unwrap_or(&region)
            .to_string();
        Self {
            auth_method,
            region,
            idc_region,
            client_id,
            client_secret,
            profile_arn,
            start_url: text(meta, "start_url").map(str::to_string),
        }
    }

    /// Flat metadata persisted next to the tokens. Tokens themselves live in
    /// the credential's dedicated columns.
    pub fn to_meta(&self) -> Value {
        let mut meta = Map::new();
        meta.insert("auth_method".into(), json!(self.auth_method.as_str()));
        meta.insert("region".into(), json!(self.region));
        meta.insert("idc_region".into(), json!(self.idc_region));
        for (key, value) in [
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("profile_arn", &self.profile_arn),
            ("start_url", &self.start_url),
        ] {
            if let Some(value) = value {
                meta.insert(key.into(), json!(value));
            }
        }
        Value::Object(meta)
    }

    /// Whether this credential refreshes through AWS OIDC.
    pub fn refreshes_through_oidc(&self) -> bool {
        !self.auth_method.is_social()
    }

    /// Runtime services are deployed in US/EU; OIDC keeps its original region.
    pub fn service_region(&self) -> &'static str {
        if self.region.starts_with("eu-") {
            "eu-central-1"
        } else {
            DEFAULT_REGION
        }
    }

    /// Compatibility profiles used by the IDE, not account identity evidence.
    /// Never persist these defaults as subject_id, and never guess an enterprise
    /// organization's ARN. Explicit/upstream-discovered profiles always win.
    pub fn request_profile(&self, token: &str) -> Option<&str> {
        if is_api_key(token) {
            return None;
        }
        self.profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|arn| !arn.is_empty())
            .or(match self.auth_method {
                AuthMethod::BuilderId => {
                    Some("arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX")
                }
                AuthMethod::Social => {
                    Some("arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK")
                }
                AuthMethod::Idc => None,
            })
    }

    pub fn api_origin(&self) -> String {
        format!("https://q.{}.amazonaws.com", self.service_region())
    }

    pub fn generate_url(&self, token: &str) -> String {
        if is_api_key(token) {
            format!(
                "https://runtime.{}.kiro.dev/generateAssistantResponse",
                self.service_region()
            )
        } else {
            format!("{}/generateAssistantResponse", self.api_origin())
        }
    }

    pub fn usage_limits_url(&self, token: &str) -> String {
        if is_api_key(token) {
            format!(
                "https://management.{}.kiro.dev/Get-Usage-Limits",
                self.service_region()
            )
        } else {
            format!("{}/getUsageLimits", self.api_origin())
        }
    }

    pub fn list_profiles_url(&self) -> String {
        format!(
            "https://codewhisperer.{}.amazonaws.com/ListAvailableProfiles",
            self.service_region()
        )
    }

    pub fn social_refresh_url(&self) -> String {
        // Kiro documents a single social token/refresh host, independent of
        // the US/EU runtime region: https://kiro.dev/docs/privacy-and-security/firewalls/
        "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken".into()
    }

    pub fn oidc_token_url(&self) -> String {
        format!("https://oidc.{}.amazonaws.com/token", self.idc_region)
    }
}

// ── Import ───────────────────────────────────────────────────────────────────

/// Camel- and snake-case spellings of the fields Kiro's files carry.
#[derive(Debug, Default, Deserialize)]
struct ImportFields {
    #[serde(default, alias = "accessToken")]
    access_token: Option<String>,
    #[serde(default, alias = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(
        default,
        alias = "expiresAt",
        alias = "expired",
        alias = "expiration",
        deserialize_with = "deserialize_import_expiry"
    )]
    expires_at: Option<String>,
    #[serde(default, alias = "authMethod")]
    auth_method: Option<String>,
    #[serde(default, alias = "clientId")]
    client_id: Option<String>,
    #[serde(default, alias = "clientSecret")]
    client_secret: Option<String>,
    #[serde(default, alias = "profileArn")]
    profile_arn: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default, alias = "idcRegion")]
    idc_region: Option<String>,
    #[serde(default, alias = "startUrl")]
    start_url: Option<String>,
}

/// Account-manager exports use Unix milliseconds; IDE files use RFC3339 strings.
/// Keep strings unchanged so the auth driver retains its existing refresh fallback.
fn deserialize_import_expiry<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(Value::Number(value)) => value
            .as_i64()
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
            .map(|at| Some(at.to_rfc3339()))
            .ok_or_else(|| serde::de::Error::custom("Invalid Kiro expiry timestamp")),
        _ => Err(serde::de::Error::custom("Invalid Kiro expiry type")),
    }
}

/// Extract only credentials. Exported subscription/usage/status are untrusted
/// snapshots, not authoritative account state, and must never be persisted here.
fn account_lite_fields(value: &Value) -> Result<ImportFields, ImportError> {
    if value.get("app").and_then(Value::as_str) != Some("kiro-account-lite") {
        return Err(ImportError::Invalid);
    }
    let accounts = value
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or(ImportError::Invalid)?;
    match accounts.len() {
        0 => return Err(ImportError::MissingToken),
        1 => {}
        _ => return Err(ImportError::MultipleAccounts),
    }
    let account = &accounts[0];
    let credentials = account
        .get("credentials")
        .filter(|value| value.is_object())
        .ok_or(ImportError::Invalid)?;
    let mut fields: ImportFields =
        serde_json::from_value(credentials.clone()).map_err(|_| ImportError::Invalid)?;
    // Some exports keep the profile only on the account. The credential's own
    // profile always wins; never merge tokens or expiry from other objects.
    if fields
        .profile_arn
        .as_deref()
        .is_none_or(|arn| arn.trim().is_empty())
    {
        fields.profile_arn = text(account, "profileArn").map(str::to_string);
    }
    Ok(fields)
}

fn fill(target: &mut Option<String>, value: Option<String>) {
    if let Some(value) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        *target = Some(value);
    }
}

impl ImportFields {
    fn absorb(&mut self, next: ImportFields) {
        fill(&mut self.access_token, next.access_token);
        fill(&mut self.refresh_token, next.refresh_token);
        // The token file owns the expiry; a registration file's own expiry
        // (client secret lifetime) must not replace it.
        if self.expires_at.is_none() {
            fill(&mut self.expires_at, next.expires_at);
        }
        fill(&mut self.auth_method, next.auth_method);
        fill(&mut self.client_id, next.client_id);
        fill(&mut self.client_secret, next.client_secret);
        fill(&mut self.profile_arn, next.profile_arn);
        fill(&mut self.region, next.region);
        fill(&mut self.idc_region, next.idc_region);
        fill(&mut self.start_url, next.start_url);
    }
}

/// Result of parsing what the operator pasted.
#[derive(Debug, PartialEq, Eq)]
pub struct Imported {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub credential: KiroCredential,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    Empty,
    TooLarge,
    Invalid,
    /// Neither a refresh token nor a usable access token was supplied.
    MissingToken,
    /// IdC/Builder ID credentials refresh with the registered client.
    MissingClient,
    /// The existing authorization session binds exactly one account.
    MultipleAccounts,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "Paste the Kiro credential JSON or a refresh token",
            Self::TooLarge => "Kiro credential input is too large",
            Self::Invalid => {
                "Kiro credential input is not valid JSON, a refresh token or an API key"
            }
            Self::MultipleAccounts => {
                "Import one Kiro account at a time; export a single account from kiro-account-lite"
            }
            Self::MissingToken => "Kiro credential needs a refreshToken (or an accessToken)",
            Self::MissingClient => {
                "Builder ID and IAM Identity Center credentials need clientId and clientSecret"
            }
        })
    }
}

impl std::error::Error for ImportError {}

/// Accepts, in order of preference: one or more concatenated JSON objects
/// (the token file, optionally followed by the client-registration file), a
/// single-account kiro-account-lite export, a `ksk_` API key, or a bare refresh token.
pub fn parse_import(input: &str) -> Result<Imported, ImportError> {
    let input = input.trim().trim_matches(['\'', '"']).trim();
    if input.is_empty() {
        return Err(ImportError::Empty);
    }
    if input.len() > MAX_IMPORT_BYTES {
        return Err(ImportError::TooLarge);
    }
    let mut fields = ImportFields::default();
    if input.starts_with('{') {
        let mut count = 0;
        let mut account_export = false;
        for value in serde_json::Deserializer::from_str(input).into_iter::<Value>() {
            let value = value.map_err(|_| ImportError::Invalid)?;
            let is_export = value.get("accounts").is_some()
                || value.get("app").and_then(Value::as_str) == Some("kiro-account-lite");
            // Unlike IDE token + registration files, a complete account export
            // cannot be concatenated with other objects: that could mix identities.
            if account_export || (is_export && count != 0) {
                return Err(ImportError::Invalid);
            }
            let next = if is_export {
                account_export = true;
                account_lite_fields(&value)?
            } else {
                serde_json::from_value(value).map_err(|_| ImportError::Invalid)?
            };
            fields.absorb(next);
            count += 1;
        }
        if count == 0 {
            return Err(ImportError::Invalid);
        }
    } else if is_api_key(input) {
        fields.access_token = Some(input.to_string());
    } else if input.bytes().all(|b| b.is_ascii_graphic()) {
        fields.refresh_token = Some(input.to_string());
    } else {
        return Err(ImportError::Invalid);
    }
    finish_import(fields)
}

fn finish_import(fields: ImportFields) -> Result<Imported, ImportError> {
    let has_refresh = fields.refresh_token.is_some();
    if !has_refresh && fields.access_token.is_none() {
        return Err(ImportError::MissingToken);
    }
    let has_client = fields.client_id.is_some() && fields.client_secret.is_some();
    let auth_method = match fields.auth_method.as_deref() {
        Some(raw) => AuthMethod::parse(raw).unwrap_or(if has_client {
            AuthMethod::Idc
        } else {
            AuthMethod::Social
        }),
        None if has_client => AuthMethod::BuilderId,
        None => AuthMethod::Social,
    };
    let api_key = fields.access_token.as_deref().is_some_and(is_api_key);
    if has_refresh && !auth_method.is_social() && !has_client {
        return Err(ImportError::MissingClient);
    }
    let region = fields
        .region
        .as_deref()
        .filter(|region| valid_region(region))
        .or_else(|| fields.profile_arn.as_deref().and_then(arn_region))
        .unwrap_or(DEFAULT_REGION)
        .to_string();
    let idc_region = fields
        .idc_region
        .as_deref()
        .filter(|region| valid_region(region))
        .unwrap_or(&region)
        .to_string();
    Ok(Imported {
        // A static API key never expires and never refreshes.
        expires_at: if api_key { None } else { fields.expires_at },
        access_token: fields.access_token,
        refresh_token: fields.refresh_token,
        credential: KiroCredential {
            auth_method,
            region,
            idc_region,
            client_id: fields.client_id,
            client_secret: fields.client_secret,
            profile_arn: fields.profile_arn,
            start_url: fields.start_url,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(meta: Value) -> StoredCredential {
        StoredCredential {
            meta,
            ..Default::default()
        }
    }

    #[test]
    fn regions_are_plain_labels_that_cannot_change_the_host() {
        for ok in ["us-east-1", "eu-central-1", "ap-southeast-2"] {
            assert!(valid_region(ok), "{ok}");
        }
        for bad in [
            "",
            "us",
            "US-EAST-1",
            "us-east-1.evil.com",
            "evil.com/x",
            "us_east_1",
            "-us-east-1",
            "us-east-1-",
            "us--east",
            "a@b.c",
            "us-east-1\n",
            "us-east-1:443",
        ] {
            assert!(!valid_region(bad), "{bad:?}");
        }
        assert_eq!(
            arn_region("arn:aws:codewhisperer:eu-central-1:1:profile/X"),
            Some("eu-central-1")
        );
        assert_eq!(arn_region("arn:aws:codewhisperer:../..:1:profile/X"), None);
        assert_eq!(arn_region("garbage"), None);
        assert_eq!(
            region_from_origin("https://q.us-east-1.amazonaws.com/"),
            Some("us-east-1")
        );
        for bad in [
            "http://q.us-east-1.amazonaws.com",
            "https://q.us-east-1.amazonaws.com.evil.com",
            "https://q.us-east-1.amazonaws.com/path",
            "https://evil.com",
            "https://q.evil.com/.amazonaws.com",
        ] {
            assert_eq!(region_from_origin(bad), None, "{bad}");
        }
    }

    #[test]
    fn auth_method_spellings_normalize() {
        for (raw, expected) in [
            ("social", AuthMethod::Social),
            ("Google", AuthMethod::Social),
            ("builder-id", AuthMethod::BuilderId),
            ("BuilderId", AuthMethod::BuilderId),
            ("builder_id", AuthMethod::BuilderId),
            ("IdC", AuthMethod::Idc),
            ("iam identity center", AuthMethod::Idc),
        ] {
            assert_eq!(AuthMethod::parse(raw), Some(expected), "{raw}");
        }
        assert_eq!(AuthMethod::parse("oauth"), None);
    }

    #[test]
    fn urls_follow_region_auth_method_and_profile() {
        let mut credential = KiroCredential::from_stored(&stored(json!({})));
        assert_eq!(credential.auth_method, AuthMethod::Social);
        assert_eq!(
            credential.generate_url("oauth-token"),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            credential.social_refresh_url(),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
        );

        credential.auth_method = AuthMethod::BuilderId;
        assert_eq!(
            credential.generate_url("oauth-token"),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            credential.usage_limits_url("oauth-token"),
            "https://q.us-east-1.amazonaws.com/getUsageLimits"
        );
        credential.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/X".into());
        assert_eq!(credential.api_origin(), "https://q.us-east-1.amazonaws.com");

        credential.idc_region = "eu-west-1".into();
        assert_eq!(
            credential.oidc_token_url(),
            "https://oidc.eu-west-1.amazonaws.com/token"
        );
    }

    #[test]
    fn social_refresh_stays_on_auth_service_when_runtime_profile_is_european() {
        let credential = KiroCredential::from_stored(&stored(json!({
            "auth_method":"social", "region":"eu-central-1", "idc_region":"eu-west-1"
        })));
        assert_eq!(
            credential.api_origin(),
            "https://q.eu-central-1.amazonaws.com"
        );
        assert_eq!(
            credential.social_refresh_url(),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
        );
        assert_eq!(
            credential.oidc_token_url(),
            "https://oidc.eu-west-1.amazonaws.com/token"
        );
    }

    #[test]
    fn stored_meta_round_trips_and_defaults_are_safe() {
        let credential = KiroCredential {
            auth_method: AuthMethod::Idc,
            region: "us-east-1".into(),
            idc_region: "eu-west-1".into(),
            client_id: Some("cid".into()),
            client_secret: Some("secret".into()),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:1:profile/X".into()),
            start_url: Some("https://acme.awsapps.com/start".into()),
        };
        assert_eq!(
            KiroCredential::from_stored(&stored(credential.to_meta())),
            credential
        );

        // A hostile or garbled region falls back to the profile ARN, then default.
        let rebuilt = KiroCredential::from_stored(&stored(json!({
            "region": "evil.com/x",
            "profile_arn": "arn:aws:codewhisperer:eu-central-1:1:profile/X",
        })));
        assert_eq!(rebuilt.region, "eu-central-1");
        let rebuilt = KiroCredential::from_stored(&stored(json!({"region": "../x"})));
        assert_eq!(rebuilt.region, DEFAULT_REGION);
        // Client credentials with no auth_method imply IdC.
        let rebuilt =
            KiroCredential::from_stored(&stored(json!({"client_id":"a","client_secret":"b"})));
        assert_eq!(rebuilt.auth_method, AuthMethod::Idc);
    }

    #[test]
    fn imports_the_ide_token_file_alone_as_a_social_credential() {
        let imported = parse_import(
            r#"{"accessToken":"AT","refreshToken":"RT","expiresAt":"2030-01-01T00:00:00.000Z",
                "authMethod":"social","provider":"Google","profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/X"}"#,
        )
        .unwrap();
        assert_eq!(imported.access_token.as_deref(), Some("AT"));
        assert_eq!(imported.refresh_token.as_deref(), Some("RT"));
        assert_eq!(
            imported.expires_at.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );
        assert_eq!(imported.credential.auth_method, AuthMethod::Social);
        assert_eq!(imported.credential.region, "us-east-1");
        assert!(imported.credential.profile_arn.is_some());
    }

    fn account_lite_export() -> Value {
        json!({
            "app": "kiro-account-lite", "version": "1.0.37", "exportedAt": 1793011196617_i64,
            "accounts": [{
                "id": "fixture-account", "email": "fixture@example.com", "idp": "Github",
                "credentials": {
                    "accessToken": "FIXTURE-AT", "refreshToken": "FIXTURE-RT", "csrfToken": "",
                    "expiresAt": 1793013476000_i64, "authMethod": "social", "provider": "Github",
                    "region": "us-east-1", "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/X"
                },
                "subscription": {"title": "KIRO PRO MAX", "expiresAt": 1793491200000_i64},
                "usage": {"current": 0, "limit": 5000}, "status": "active",
                "profileArn": "arn:aws:codewhisperer:eu-west-1:1:profile/OTHER"
            }]
        })
    }

    #[test]
    fn imports_account_lite_export_without_trusting_subscription_snapshot() {
        let input = account_lite_export();
        let imported = parse_import(&input.to_string()).unwrap();
        assert_eq!(
            imported,
            parse_import(&input["accounts"][0]["credentials"].to_string()).unwrap()
        );
        assert_eq!(imported.access_token.as_deref(), Some("FIXTURE-AT"));
        assert_eq!(imported.refresh_token.as_deref(), Some("FIXTURE-RT"));
        assert_eq!(imported.credential.auth_method, AuthMethod::Social);
        assert_eq!(imported.credential.region, "us-east-1");
        let expiry =
            chrono::DateTime::parse_from_rfc3339(imported.expires_at.as_deref().unwrap()).unwrap();
        assert_eq!(expiry.timestamp_millis(), 1793013476000_i64);
        let meta = imported.credential.to_meta();
        assert_eq!(meta.as_object().unwrap().len(), 4);
        for key in [
            "usage",
            "subscription",
            "email",
            "status",
            "accessToken",
            "refreshToken",
        ] {
            assert!(meta.get(key).is_none());
        }
    }

    #[test]
    fn account_lite_profile_fallback_and_oidc_client_requirements() {
        let mut input = account_lite_export();
        input["accounts"][0]["credentials"]
            .as_object_mut()
            .unwrap()
            .remove("profileArn");
        let imported = parse_import(&input.to_string()).unwrap();
        assert_eq!(
            imported.credential.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:eu-west-1:1:profile/OTHER")
        );
        input["accounts"][0]["credentials"]["authMethod"] = json!("builder-id");
        assert_eq!(
            parse_import(&input.to_string()).unwrap_err(),
            ImportError::MissingClient
        );
        input["accounts"][0]["credentials"]["clientId"] = json!("CID");
        input["accounts"][0]["credentials"]["clientSecret"] = json!("SECRET");
        let imported = parse_import(&input.to_string()).unwrap();
        assert_eq!(imported.credential.auth_method, AuthMethod::BuilderId);
        assert_eq!(imported.credential.client_id.as_deref(), Some("CID"));
    }

    #[test]
    fn account_lite_rejects_ambiguous_or_malformed_exports_without_echoing_secrets() {
        let valid = account_lite_export();
        for (accounts, expected) in [
            (json!([]), ImportError::MissingToken),
            (
                json!([valid["accounts"][0], valid["accounts"][0]]),
                ImportError::MultipleAccounts,
            ),
            (json!({}), ImportError::Invalid),
            (json!([{}]), ImportError::Invalid),
            (
                json!([{"credentials": "FIXTURE-SECRET"}]),
                ImportError::Invalid,
            ),
            (json!([{"credentials": {}}]), ImportError::MissingToken),
        ] {
            let mut input = valid.clone();
            input["accounts"] = accounts;
            let error = parse_import(&input.to_string()).unwrap_err();
            assert!(!error.to_string().contains("FIXTURE"));
            assert_eq!(error, expected);
        }
        let mut unknown = valid.clone();
        unknown["app"] = json!("unknown");
        assert_eq!(
            parse_import(&unknown.to_string()).unwrap_err(),
            ImportError::Invalid
        );
        for input in [
            format!("{valid} {{\"refreshToken\":\"OTHER\"}}"),
            format!("{{}} {valid}"),
        ] {
            assert_eq!(parse_import(&input).unwrap_err(), ImportError::Invalid);
        }
    }

    #[test]
    fn import_expiry_accepts_milliseconds_null_and_existing_strings_but_rejects_invalid_types() {
        for expiry in [
            json!(true),
            json!({}),
            json!([]),
            json!(1.5),
            json!(i64::MAX),
            json!(u64::MAX),
        ] {
            let input = json!({"refreshToken": "RT", "expiresAt": expiry});
            assert_eq!(
                parse_import(&input.to_string()).unwrap_err(),
                ImportError::Invalid
            );
        }
        let input = json!({"refreshToken": "RT", "expiresAt": null});
        assert!(
            parse_import(&input.to_string())
                .unwrap()
                .expires_at
                .is_none()
        );
        for key in ["expiresAt", "expires_at", "expired", "expiration"] {
            let mut input = json!({"refreshToken": "RT"});
            input[key] = json!(0);
            assert_eq!(
                parse_import(&input.to_string())
                    .unwrap()
                    .expires_at
                    .as_deref(),
                Some("1970-01-01T00:00:00+00:00")
            );
        }
    }

    #[test]
    fn merges_the_token_file_with_the_client_registration_file() {
        let imported = parse_import(
            r#"{"accessToken":"AT","refreshToken":"RT","expiresAt":"2030-01-01T00:00:00Z","region":"us-east-1"}
               {"clientId":"cid","clientSecret":"sec","expiresAt":"2020-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(imported.credential.auth_method, AuthMethod::BuilderId);
        assert_eq!(imported.credential.client_id.as_deref(), Some("cid"));
        assert_eq!(imported.credential.client_secret.as_deref(), Some("sec"));
        // The token file's expiry wins over the registration's.
        assert_eq!(imported.expires_at.as_deref(), Some("2030-01-01T00:00:00Z"));
        assert!(imported.credential.refreshes_through_oidc());
    }

    #[test]
    fn idc_import_keeps_its_own_oidc_region() {
        let imported = parse_import(
            r#"{"refreshToken":"RT","clientId":"c","clientSecret":"s","authMethod":"IdC",
                "idcRegion":"eu-west-1","region":"us-east-1","startUrl":"https://x.awsapps.com/start"}"#,
        )
        .unwrap();
        assert_eq!(imported.credential.auth_method, AuthMethod::Idc);
        assert_eq!(imported.credential.idc_region, "eu-west-1");
        assert_eq!(imported.credential.region, "us-east-1");
    }

    #[test]
    fn bare_refresh_tokens_and_api_keys_are_recognised() {
        let social = parse_import("  aorAAAAAGexample-refresh_token  ").unwrap();
        assert_eq!(
            social.refresh_token.as_deref(),
            Some("aorAAAAAGexample-refresh_token")
        );
        assert_eq!(social.credential.auth_method, AuthMethod::Social);
        assert_eq!(social.access_token, None);

        let key = parse_import("ksk_abc123").unwrap();
        assert_eq!(key.access_token.as_deref(), Some("ksk_abc123"));
        assert_eq!(key.refresh_token, None);
        assert_eq!(key.expires_at, None);
    }

    #[test]
    fn invalid_inputs_are_rejected_with_a_specific_reason() {
        assert_eq!(parse_import("   ").unwrap_err(), ImportError::Empty);
        assert_eq!(parse_import("{not json").unwrap_err(), ImportError::Invalid);
        assert_eq!(parse_import("two words").unwrap_err(), ImportError::Invalid);
        assert_eq!(parse_import("{}").unwrap_err(), ImportError::MissingToken);
        assert_eq!(
            parse_import(&"x".repeat(MAX_IMPORT_BYTES + 1)).unwrap_err(),
            ImportError::TooLarge
        );
        // Builder ID without a registered client cannot refresh.
        assert_eq!(
            parse_import(r#"{"refreshToken":"RT","authMethod":"builder-id"}"#).unwrap_err(),
            ImportError::MissingClient
        );
        // Errors never echo the pasted secret.
        assert!(!ImportError::Invalid.to_string().contains("RT"));
    }

    #[test]
    fn hostile_regions_in_an_import_fall_back_to_the_default() {
        let imported = parse_import(r#"{"refreshToken":"RT","region":"evil.com/x"}"#).unwrap();
        assert_eq!(imported.credential.region, DEFAULT_REGION);
    }
    #[test]
    fn request_profiles_do_not_fabricate_verified_identity_or_enterprise_profiles() {
        let mut c = KiroCredential::from_stored(&stored(json!({"auth_method":"builder-id"})));
        assert!(
            c.request_profile("oauth")
                .unwrap()
                .ends_with("AAAACCCCXXXX")
        );
        assert!(c.profile_arn.is_none());
        assert!(c.to_meta().get("profile_arn").is_none());
        c.auth_method = AuthMethod::Social;
        assert!(
            c.request_profile("oauth")
                .unwrap()
                .ends_with("EHGA3GRVQMUK")
        );
        c.auth_method = AuthMethod::Idc;
        assert_eq!(c.request_profile("oauth"), None);
        c.profile_arn = Some("arn:aws:codewhisperer:eu-central-1:1:profile/REAL".into());
        assert_eq!(c.request_profile("oauth"), c.profile_arn.as_deref());
        assert_eq!(c.request_profile("ksk_fixture"), None);
    }

    #[test]
    fn service_mapping_preserves_oidc_region_and_separates_api_keys() {
        let c = KiroCredential::from_stored(&stored(
            json!({"region":"eu-west-1", "idc_region":"ap-southeast-2"}),
        ));
        assert_eq!(
            c.generate_url("oauth"),
            "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            c.generate_url("ksk_fixture"),
            "https://runtime.eu-central-1.kiro.dev/generateAssistantResponse"
        );
        assert_eq!(
            c.usage_limits_url("ksk_fixture"),
            "https://management.eu-central-1.kiro.dev/Get-Usage-Limits"
        );
        assert_eq!(
            c.oidc_token_url(),
            "https://oidc.ap-southeast-2.amazonaws.com/token"
        );
        assert_eq!(
            c.list_profiles_url(),
            "https://codewhisperer.eu-central-1.amazonaws.com/ListAvailableProfiles"
        );
        let c = KiroCredential::from_stored(&stored(json!({"region":"ap-southeast-2"})));
        assert_eq!(c.service_region(), "us-east-1");
    }
}
