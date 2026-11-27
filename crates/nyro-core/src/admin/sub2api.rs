//! Sub2API helpers for the provider form.

use super::*;

pub use crate::provider::sub2api::detect::{
    Confidence as Sub2ApiDetectConfidence, Sub2ApiDetectInput, Sub2ApiPlatformDetection,
};

impl AdminService {
    /// Suggest the group platform of a Sub2API key typed into the provider
    /// form (address + key). Reads only listing/billing endpoints, so no
    /// tokens are spent; the result is advisory and never saved.
    pub async fn detect_sub2api_platform(
        &self,
        input: Sub2ApiDetectInput,
    ) -> anyhow::Result<Sub2ApiPlatformDetection> {
        let client = self.gw.http_client_for_provider(input.use_proxy).await?;
        crate::provider::sub2api::detect::detect(&client, &input.base_url, &input.api_key).await
    }
}
