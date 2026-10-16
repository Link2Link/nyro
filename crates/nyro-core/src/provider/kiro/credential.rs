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

    /// Builder ID accounts without a profile talk to the CodeWhisperer host.
    pub fn uses_codewhisperer_host(&self) -> bool {
        self.auth_method == AuthMethod::BuilderId && self.profile_arn.is_none()
    }

    /// `https://q.<region>.amazonaws.com` (or the CodeWhisperer variant).
    pub fn api_origin(&self) -> String {
        let prefix = if self.uses_codewhisperer_host() {
            "codewhisperer"
        } else {
            "q"
        };
        format!("https://{prefix}.{}.amazonaws.com", self.region)
    }

    pub fn generate_url(&self) -> String {
        format!("{}/generateAssistantResponse", self.api_origin())
    }

    pub fn usage_limits_url(&self) -> String {
        // Usage limits always live on the Q host, never on CodeWhisperer.
        format!("https://q.{}.amazonaws.com/getUsageLimits", self.region)
    }

    pub fn list_profiles_url(&self) -> String {
        format!(
            "https://codewhisperer.{}.amazonaws.com/ListAvailableProfiles",
            self.region
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
    #[serde(default, alias = "expiresAt", alias = "expired", alias = "expiration")]
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
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "Paste the Kiro credential JSON or a refresh token",
            Self::TooLarge => "Kiro credential input is too large",
            Self::Invalid => {
                "Kiro credential input is not valid JSON, a refresh token or an API key"
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
/// `ksk_` API key, or a bare refresh token.
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
        for value in serde_json::Deserializer::from_str(input).into_iter::<ImportFields>() {
            fields.absorb(value.map_err(|_| ImportError::Invalid)?);
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
            credential.generate_url(),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            credential.social_refresh_url(),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
        );

        credential.auth_method = AuthMethod::BuilderId;
        assert!(credential.uses_codewhisperer_host());
        assert_eq!(
            credential.generate_url(),
            "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            credential.usage_limits_url(),
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
}
