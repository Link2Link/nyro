use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("self-update is not available: {0}")]
    Unsupported(String),
    #[error("an update is already in progress")]
    Busy,
    #[error("update verification failed: {0}")]
    Verification(String),
    #[error("invalid update manifest: {0}")]
    InvalidManifest(String),
    #[error("requested version {requested} does not match the latest release {latest}")]
    VersionMismatch { requested: String, latest: String },
    #[error("version {0} is not newer than the running version")]
    NotNewer(String),
    #[error("release {version} has no binary for platform {platform}")]
    NoAsset { version: String, platform: String },
    #[error("failed to fetch update: {0}")]
    Fetch(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl UpdateError {
    /// Stable machine-readable code for API clients.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "UPDATE_UNSUPPORTED",
            Self::Busy => "UPDATE_BUSY",
            Self::Verification(_) => "UPDATE_VERIFICATION_FAILED",
            Self::InvalidManifest(_) => "UPDATE_INVALID_MANIFEST",
            Self::VersionMismatch { .. } => "UPDATE_VERSION_MISMATCH",
            Self::NotNewer(_) => "UPDATE_NOT_NEWER",
            Self::NoAsset { .. } => "UPDATE_NO_ASSET",
            Self::Fetch(_) => "UPDATE_FETCH_FAILED",
            Self::Internal(_) => "UPDATE_INTERNAL",
        }
    }
}

pub(crate) fn io_error(context: &'static str) -> impl Fn(std::io::Error) -> UpdateError {
    move |error| UpdateError::Internal(anyhow::anyhow!("{context}: {error}"))
}
