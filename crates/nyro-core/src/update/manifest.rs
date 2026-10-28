use std::collections::BTreeMap;

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::{Deserialize, Serialize};

use super::error::UpdateError;

/// Upper bound for a release binary, guarding the data volume against a
/// malformed size field.
pub(crate) const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound for the signed envelope document.
pub(crate) const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;

/// Minisign public key of the release signer. It is compiled in and cannot be
/// overridden at runtime, so even a leaked admin token can only install
/// releases signed by the maintainers.
pub(crate) const RELEASE_PUBLIC_KEY: &str = include_str!("release.pub");

/// Published document: the manifest JSON text plus its minisign signature.
/// The signature covers the exact manifest text, so verification does not
/// depend on how JSON is re-serialized.
#[derive(Debug, Deserialize)]
struct SignedEnvelope {
    manifest: String,
    signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub version: String,
    #[serde(default)]
    pub pub_date: Option<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub release_url: Option<String>,
    /// Binaries keyed by platform (see [`current_platform`]).
    pub assets: BTreeMap<String, ReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseAsset {
    /// Absolute URL, or a path relative to the manifest URL.
    pub url: String,
    /// Hex sha256 of the binary.
    pub sha256: String,
    pub size: u64,
}

impl ReleaseManifest {
    pub fn parsed_version(&self) -> Result<Version, UpdateError> {
        Version::parse(self.version.trim().trim_start_matches('v')).map_err(|error| {
            UpdateError::InvalidManifest(format!("version `{}`: {error}", self.version))
        })
    }

    pub fn asset_for(&self, platform: &str) -> Option<&ReleaseAsset> {
        self.assets.get(platform)
    }

    fn validate(&self) -> Result<(), UpdateError> {
        self.parsed_version()?;
        for (platform, asset) in &self.assets {
            if asset.url.trim().is_empty() {
                return Err(UpdateError::InvalidManifest(format!(
                    "asset {platform} has an empty url"
                )));
            }
            if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(UpdateError::InvalidManifest(format!(
                    "asset {platform} has an invalid sha256"
                )));
            }
            if asset.size == 0 || asset.size > MAX_ASSET_BYTES {
                return Err(UpdateError::InvalidManifest(format!(
                    "asset {platform} has an invalid size {}",
                    asset.size
                )));
            }
        }
        Ok(())
    }
}

/// Platform key used in release manifests, e.g. `linux-x86_64`.
pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn decode_public_key(text: &str) -> Result<PublicKey, UpdateError> {
    PublicKey::decode(text.trim()).map_err(|error| {
        UpdateError::Internal(anyhow::anyhow!("invalid update public key: {error}"))
    })
}

/// Verify a signed envelope and return the manifest it carries.
pub(crate) fn verify_envelope(
    bytes: &[u8],
    public_key: &PublicKey,
) -> Result<ReleaseManifest, UpdateError> {
    let envelope: SignedEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| UpdateError::InvalidManifest(format!("envelope: {error}")))?;
    let signature = Signature::decode(envelope.signature.trim())
        .map_err(|error| UpdateError::Verification(format!("signature: {error}")))?;
    public_key
        .verify(envelope.manifest.as_bytes(), &signature, false)
        .map_err(|error| UpdateError::Verification(format!("signature: {error}")))?;
    let manifest: ReleaseManifest = serde_json::from_str(&envelope.manifest)
        .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Resolve an asset URL against the manifest URL; only http(s) is accepted.
/// Integrity comes from the signed sha256, not from the transport.
pub(crate) fn resolve_asset_url(
    manifest_url: &str,
    asset_url: &str,
) -> Result<reqwest::Url, UpdateError> {
    let base = reqwest::Url::parse(manifest_url)
        .map_err(|error| UpdateError::Internal(anyhow::anyhow!("manifest url: {error}")))?;
    let url = base
        .join(asset_url.trim())
        .map_err(|error| UpdateError::InvalidManifest(format!("asset url: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UpdateError::InvalidManifest(format!(
            "asset url scheme `{}` is not allowed",
            url.scheme()
        )));
    }
    Ok(url)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const TEST_PUBLIC_KEY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/update/test.pub"
    ));
    pub(crate) const TEST_ENVELOPE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/update/nyro-server-update.json"
    ));

    fn test_key() -> PublicKey {
        decode_public_key(TEST_PUBLIC_KEY).unwrap()
    }

    #[test]
    fn release_public_key_is_valid() {
        decode_public_key(RELEASE_PUBLIC_KEY).unwrap();
    }

    #[test]
    fn verifies_generator_output_with_matching_key() {
        let manifest = verify_envelope(TEST_ENVELOPE, &test_key()).unwrap();
        assert_eq!(manifest.parsed_version().unwrap(), Version::new(9, 9, 9));
        assert!(!manifest.notes.is_empty());
        let asset = manifest.asset_for("linux-x86_64").unwrap();
        assert_eq!(asset.url, "nyro-server-test");
        assert_eq!(asset.sha256.len(), 64);
    }

    #[test]
    fn rejects_fixture_under_the_release_key() {
        let release_key = decode_public_key(RELEASE_PUBLIC_KEY).unwrap();
        let error = verify_envelope(TEST_ENVELOPE, &release_key).unwrap_err();
        assert!(matches!(error, UpdateError::Verification(_)), "{error}");
    }

    #[test]
    fn rejects_tampered_manifest_text() {
        let mut envelope: serde_json::Value = serde_json::from_slice(TEST_ENVELOPE).unwrap();
        let tampered = envelope["manifest"]
            .as_str()
            .unwrap()
            .replace("\"9.9.9\"", "\"9.9.10\"");
        envelope["manifest"] = serde_json::Value::String(tampered);
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let error = verify_envelope(&bytes, &test_key()).unwrap_err();
        assert!(matches!(error, UpdateError::Verification(_)), "{error}");
    }

    #[test]
    fn rejects_unsigned_or_malformed_documents() {
        for bytes in [
            &b"not json"[..],
            br#"{"version":"9.9.9","assets":{}}"#,
            br#"{"manifest":"{}","signature":"garbage"}"#,
        ] {
            let error = verify_envelope(bytes, &test_key()).unwrap_err();
            assert!(
                matches!(
                    error,
                    UpdateError::InvalidManifest(_) | UpdateError::Verification(_)
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn validates_asset_fields() {
        let mut manifest = verify_envelope(TEST_ENVELOPE, &test_key()).unwrap();
        let platform = "linux-x86_64".to_string();
        manifest.assets.get_mut(&platform).unwrap().sha256 = "zz".into();
        assert!(manifest.validate().is_err());

        let mut manifest = verify_envelope(TEST_ENVELOPE, &test_key()).unwrap();
        manifest.assets.get_mut(&platform).unwrap().size = MAX_ASSET_BYTES + 1;
        assert!(manifest.validate().is_err());

        let mut manifest = verify_envelope(TEST_ENVELOPE, &test_key()).unwrap();
        manifest.version = "latest".into();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn resolves_relative_and_absolute_asset_urls() {
        let base = "https://example.com/releases/latest/download/nyro-server-update.json";
        assert_eq!(
            resolve_asset_url(base, "nyro-server-linux-x86_64-musl")
                .unwrap()
                .as_str(),
            "https://example.com/releases/latest/download/nyro-server-linux-x86_64-musl"
        );
        assert_eq!(
            resolve_asset_url(base, "https://cdn.example.com/a/bin")
                .unwrap()
                .as_str(),
            "https://cdn.example.com/a/bin"
        );
        assert!(resolve_asset_url(base, "file:///etc/passwd").is_err());
    }

    #[test]
    fn platform_key_matches_manifest_naming() {
        let platform = current_platform();
        assert!(platform.contains('-'));
        assert_eq!(
            platform,
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        );
    }
}
