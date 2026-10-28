use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use minisign_verify::PublicKey;
use semver::Version;
use serde::Serialize;
use tokio::sync::watch;

use super::boot::{UpgradeLayout, UpgradeState};
use super::error::{UpdateError, io_error};
use super::install;
use super::manifest::{
    MAX_ENVELOPE_BYTES, RELEASE_PUBLIC_KEY, ReleaseAsset, ReleaseManifest, current_platform,
    decode_public_key, resolve_asset_url, verify_envelope,
};
use crate::storage::DynStorage;

/// Successful checks are reused for this long unless forced.
const CHECK_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// Allow outbound release checks at all.
    pub check_enabled: bool,
    /// Allow installing releases in place.
    pub self_update_enabled: bool,
    pub manifest_url: String,
    pub data_dir: PathBuf,
    /// Version of the running binary.
    pub current_version: String,
}

impl UpdateOptions {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            check_enabled: true,
            self_update_enabled: true,
            manifest_url: super::DEFAULT_MANIFEST_URL.to_string(),
            data_dir,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VersionInfo {
    pub version: String,
    pub platform: String,
    pub update_check: bool,
    pub self_update: SelfUpdateSupport,
}

impl VersionInfo {
    /// Version info for hosts without an update service.
    pub fn unavailable(reason: &str) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: current_platform(),
            update_check: false,
            self_update: SelfUpdateSupport::no(reason),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SelfUpdateSupport {
    pub supported: bool,
    /// `disabled` | `unsupported_os` | `data_dir_not_writable` | `unavailable`.
    pub reason: Option<String>,
}

impl SelfUpdateSupport {
    fn yes() -> Self {
        Self {
            supported: true,
            reason: None,
        }
    }

    fn no(reason: &str) -> Self {
        Self {
            supported: false,
            reason: Some(reason.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub notes: String,
    pub pub_date: Option<String>,
    pub release_url: Option<String>,
    pub platform: String,
    pub asset_available: bool,
    pub download_size: Option<u64>,
    /// True when the release can be installed in place on this instance.
    pub can_apply: bool,
    pub checked_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle,
    Downloading,
    Verifying,
    Installing,
    Restarting,
    Failed,
}

impl UpdatePhase {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::Verifying | Self::Installing | Self::Restarting
        )
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateJob {
    pub phase: UpdatePhase,
    pub target_version: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    /// Failure detail when `phase` is `failed`.
    pub message: Option<String>,
    pub updated_at: i64,
}

impl UpdateJob {
    fn idle() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            target_version: None,
            downloaded_bytes: 0,
            total_bytes: None,
            message: None,
            updated_at: chrono::Utc::now().timestamp(),
        }
    }
}

/// Binary the host should restart into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartTarget {
    pub version: String,
    pub binary: PathBuf,
}

/// Release checks and in-place installs. Cheap to clone.
#[derive(Clone)]
pub struct UpdateService {
    inner: Arc<Inner>,
}

struct Inner {
    storage: DynStorage,
    options: UpdateOptions,
    current: Version,
    public_key: PublicKey,
    layout: UpgradeLayout,
    support: SelfUpdateSupport,
    check_cache: tokio::sync::Mutex<Option<(Instant, UpdateCheck)>>,
    job: Mutex<UpdateJob>,
    restart_tx: watch::Sender<Option<RestartTarget>>,
}

impl UpdateService {
    /// `storage` supplies the gateway proxy settings for outbound requests.
    pub fn new(storage: DynStorage, options: UpdateOptions) -> anyhow::Result<Self> {
        Self::with_public_key(storage, options, RELEASE_PUBLIC_KEY)
    }

    pub(crate) fn with_public_key(
        storage: DynStorage,
        options: UpdateOptions,
        public_key: &str,
    ) -> anyhow::Result<Self> {
        let current = Version::parse(&options.current_version)?;
        let public_key = decode_public_key(public_key)?;
        let layout = UpgradeLayout::new(&options.data_dir);
        let support = detect_support(&options, &layout);
        let (restart_tx, _) = watch::channel(None);
        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                options,
                current,
                public_key,
                layout,
                support,
                check_cache: tokio::sync::Mutex::new(None),
                job: Mutex::new(UpdateJob::idle()),
                restart_tx,
            }),
        })
    }

    pub fn version_info(&self) -> VersionInfo {
        VersionInfo {
            version: self.inner.current.to_string(),
            platform: current_platform(),
            update_check: self.inner.options.check_enabled,
            self_update: self.inner.support.clone(),
        }
    }

    /// Check the release channel. Successful results are cached briefly so
    /// several open consoles do not hammer the release host.
    pub async fn check(&self, force: bool) -> Result<UpdateCheck, UpdateError> {
        if !self.inner.options.check_enabled {
            return Err(UpdateError::Unsupported(
                "release checks are disabled".into(),
            ));
        }
        let mut cache = self.inner.check_cache.lock().await;
        if !force
            && let Some((at, cached)) = cache.as_ref()
            && at.elapsed() < CHECK_CACHE_TTL
        {
            return Ok(cached.clone());
        }
        let manifest = self.fetch_manifest().await?;
        let check = self.summarize(&manifest)?;
        *cache = Some((Instant::now(), check.clone()));
        Ok(check)
    }

    /// Start installing `requested_version`, which must be the latest
    /// release and newer than the running version. Returns once the job is
    /// claimed; progress is reported by [`UpdateService::job`].
    pub async fn apply(&self, requested_version: &str) -> Result<UpdateJob, UpdateError> {
        if !self.inner.options.check_enabled {
            return Err(UpdateError::Unsupported(
                "release checks are disabled".into(),
            ));
        }
        if let Some(reason) = &self.inner.support.reason {
            return Err(UpdateError::Unsupported(reason.clone()));
        }
        if self.job().phase.is_active() {
            return Err(UpdateError::Busy);
        }
        let requested =
            Version::parse(requested_version.trim().trim_start_matches('v')).map_err(|_| {
                UpdateError::VersionMismatch {
                    requested: requested_version.to_string(),
                    latest: "?".into(),
                }
            })?;
        // Always re-verify against a fresh manifest instead of a cached one.
        let manifest = self.fetch_manifest().await?;
        let latest = manifest.parsed_version()?;
        if latest != requested {
            return Err(UpdateError::VersionMismatch {
                requested: requested.to_string(),
                latest: latest.to_string(),
            });
        }
        if latest <= self.inner.current {
            return Err(UpdateError::NotNewer(latest.to_string()));
        }
        let platform = current_platform();
        let asset = manifest
            .asset_for(&platform)
            .cloned()
            .ok_or_else(|| UpdateError::NoAsset {
                version: latest.to_string(),
                platform: platform.clone(),
            })?;
        let url = resolve_asset_url(&self.inner.options.manifest_url, &asset.url)?;

        let claimed = {
            let mut job = self.inner.job.lock().expect("update job lock");
            if job.phase.is_active() {
                return Err(UpdateError::Busy);
            }
            *job = UpdateJob {
                phase: UpdatePhase::Downloading,
                target_version: Some(latest.to_string()),
                downloaded_bytes: 0,
                total_bytes: Some(asset.size),
                message: None,
                updated_at: chrono::Utc::now().timestamp(),
            };
            job.clone()
        };

        let service = self.clone();
        tokio::spawn(async move {
            match service.install(&latest, &asset, url).await {
                Ok(target) => {
                    tracing::info!("update {} installed; restarting", target.version);
                    service.set_phase(UpdatePhase::Restarting, None);
                    service.inner.restart_tx.send_replace(Some(target));
                }
                Err(error) => {
                    tracing::warn!("update to {latest} failed: {error}");
                    service.set_phase(UpdatePhase::Failed, Some(error.to_string()));
                }
            }
        });
        Ok(claimed)
    }

    pub fn job(&self) -> UpdateJob {
        self.inner.job.lock().expect("update job lock").clone()
    }

    /// Resolves once an installed update asks the host to restart. Never
    /// resolves if no update is installed.
    pub async fn restart_requested(&self) -> RestartTarget {
        let mut rx = self.inner.restart_tx.subscribe();
        loop {
            let pending = rx.borrow_and_update().clone();
            if let Some(target) = pending {
                return target;
            }
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    pub fn pending_restart(&self) -> Option<RestartTarget> {
        self.inner.restart_tx.borrow().clone()
    }

    fn summarize(&self, manifest: &ReleaseManifest) -> Result<UpdateCheck, UpdateError> {
        let latest = manifest.parsed_version()?;
        let platform = current_platform();
        let asset = manifest.asset_for(&platform);
        let update_available = latest > self.inner.current;
        Ok(UpdateCheck {
            current_version: self.inner.current.to_string(),
            latest_version: latest.to_string(),
            update_available,
            notes: manifest.notes.clone(),
            pub_date: manifest.pub_date.clone(),
            release_url: manifest.release_url.clone(),
            asset_available: asset.is_some(),
            download_size: asset.map(|asset| asset.size),
            can_apply: update_available && asset.is_some() && self.inner.support.supported,
            platform,
            checked_at: chrono::Utc::now().timestamp(),
        })
    }

    async fn fetch_manifest(&self) -> Result<ReleaseManifest, UpdateError> {
        let client = self.http_client().await?;
        let response = client
            .get(&self.inner.options.manifest_url)
            .timeout(MANIFEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| UpdateError::Fetch(error.to_string()))?;
        if !response.status().is_success() {
            return Err(UpdateError::Fetch(format!(
                "release manifest returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let bytes = read_limited(response, MAX_ENVELOPE_BYTES).await?;
        verify_envelope(&bytes, &self.inner.public_key)
    }

    async fn install(
        &self,
        version: &Version,
        asset: &ReleaseAsset,
        url: reqwest::Url,
    ) -> Result<RestartTarget, UpdateError> {
        let layout = &self.inner.layout;
        let staging = layout.staging_dir();
        std::fs::create_dir_all(&staging).map_err(io_error("create staging directory"))?;
        let staged = staging.join(format!("{}-{version}.partial", super::BINARY_NAME));
        let result = self.download_and_verify(version, asset, url, &staged).await;
        if let Err(error) = result {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }

        self.set_phase(UpdatePhase::Installing, None);
        let binary = install::promote(layout, &staged, version)?;
        layout
            .write_state(&UpgradeState::new(version))
            .map_err(io_error("write upgrade state"))?;
        if let Err(error) = layout.prune(Some(version)) {
            tracing::warn!("failed to prune old upgrades: {error}");
        }
        Ok(RestartTarget {
            version: version.to_string(),
            binary,
        })
    }

    async fn download_and_verify(
        &self,
        version: &Version,
        asset: &ReleaseAsset,
        url: reqwest::Url,
        staged: &std::path::Path,
    ) -> Result<(), UpdateError> {
        let client = self.http_client().await?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| UpdateError::Fetch(error.to_string()))?;
        if !response.status().is_success() {
            return Err(UpdateError::Fetch(format!(
                "release binary returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let digest = install::download_to(response, staged, asset.size, |received| {
            self.update_job(|job| job.downloaded_bytes = received);
        })
        .await?;

        self.set_phase(UpdatePhase::Verifying, None);
        if !digest.eq_ignore_ascii_case(&asset.sha256) {
            return Err(UpdateError::Verification(
                "binary sha256 does not match the signed manifest".into(),
            ));
        }
        install::make_executable(staged).await?;
        install::smoke_test(staged, version).await
    }

    /// Client for release traffic, honoring the gateway proxy setting.
    async fn http_client(&self) -> Result<reqwest::Client, UpdateError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("nyro-server/", env!("CARGO_PKG_VERSION")));
        let settings = self.inner.storage.settings();
        let proxy_enabled = settings
            .get("proxy_enabled")
            .await
            .ok()
            .flatten()
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            });
        if proxy_enabled {
            let proxy_url = settings
                .get("proxy_url")
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            let proxy_url = proxy_url.trim();
            if !proxy_url.is_empty() {
                let proxy = reqwest::Proxy::all(proxy_url)
                    .map_err(|error| UpdateError::Fetch(format!("proxy_url: {error}")))?;
                builder = builder.proxy(proxy);
            }
        }
        builder
            .build()
            .map_err(|error| UpdateError::Internal(error.into()))
    }

    fn update_job(&self, mutate: impl FnOnce(&mut UpdateJob)) {
        let mut job = self.inner.job.lock().expect("update job lock");
        mutate(&mut job);
        job.updated_at = chrono::Utc::now().timestamp();
    }

    fn set_phase(&self, phase: UpdatePhase, message: Option<String>) {
        self.update_job(|job| {
            job.phase = phase;
            job.message = message;
        });
    }
}

fn detect_support(options: &UpdateOptions, layout: &UpgradeLayout) -> SelfUpdateSupport {
    if !options.self_update_enabled || !options.check_enabled {
        return SelfUpdateSupport::no("disabled");
    }
    if !cfg!(unix) {
        return SelfUpdateSupport::no("unsupported_os");
    }
    let probe = layout.root().join(".write-test");
    let writable = std::fs::create_dir_all(layout.root())
        .and_then(|_| std::fs::write(&probe, b"ok"))
        .and_then(|_| std::fs::remove_file(&probe))
        .is_ok();
    if !writable {
        return SelfUpdateSupport::no("data_dir_not_writable");
    }
    SelfUpdateSupport::yes()
}

async fn read_limited(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, UpdateError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(UpdateError::InvalidManifest("manifest is too large".into()));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| UpdateError::Fetch(error.to_string()))?;
        if body.len() + chunk.len() > limit {
            return Err(UpdateError::InvalidManifest("manifest is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use crate::update::manifest::tests::{TEST_ENVELOPE, TEST_PUBLIC_KEY};
    use axum::Router;
    use axum::routing::get;

    const TEST_BINARY: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/update/nyro-server-test"
    ));

    /// Serve the signed fixture envelope and `binary` as its asset.
    async fn serve(binary: Vec<u8>) -> String {
        let app = Router::new()
            .route("/nyro-server-update.json", get(|| async { TEST_ENVELOPE }))
            .route(
                "/nyro-server-test",
                get(move || {
                    let binary = binary.clone();
                    async move { binary }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/nyro-server-update.json")
    }

    fn make_service(
        data_dir: &std::path::Path,
        manifest_url: String,
        current: &str,
    ) -> UpdateService {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let mut options = UpdateOptions::new(data_dir.to_path_buf());
        options.manifest_url = manifest_url;
        options.current_version = current.to_string();
        UpdateService::with_public_key(storage, options, TEST_PUBLIC_KEY).unwrap()
    }

    async fn wait_for_terminal(service: &UpdateService) -> UpdateJob {
        for _ in 0..1000 {
            let job = service.job();
            if matches!(job.phase, UpdatePhase::Restarting | UpdatePhase::Failed) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("update job did not finish: {:?}", service.job());
    }

    #[tokio::test]
    async fn check_reports_newer_signed_release() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(TEST_BINARY.to_vec()).await;
        let service = make_service(dir.path(), url, "2.1.2");
        assert!(service.version_info().self_update.supported);

        let check = service.check(true).await.unwrap();
        assert!(check.update_available);
        assert_eq!(check.current_version, "2.1.2");
        assert_eq!(check.latest_version, "9.9.9");
        assert!(check.asset_available);
        assert!(check.can_apply);
        assert_eq!(check.download_size, Some(TEST_BINARY.len() as u64));

        let url = serve(TEST_BINARY.to_vec()).await;
        let up_to_date = make_service(dir.path(), url, "9.9.9")
            .check(true)
            .await
            .unwrap();
        assert!(!up_to_date.update_available);
        assert!(!up_to_date.can_apply);
    }

    #[tokio::test]
    async fn check_rejects_manifest_signed_by_another_key() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(TEST_BINARY.to_vec()).await;
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let mut options = UpdateOptions::new(dir.path().to_path_buf());
        options.manifest_url = url;
        options.current_version = "2.1.2".into();
        // Production key: the fixture is signed by the throwaway test key.
        let service = UpdateService::new(storage, options).unwrap();
        let error = service.check(true).await.unwrap_err();
        assert_eq!(error.code(), "UPDATE_VERIFICATION_FAILED", "{error}");
    }

    #[tokio::test]
    async fn apply_installs_verified_binary_and_requests_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(TEST_BINARY.to_vec()).await;
        let service = make_service(dir.path(), url, "2.1.2");
        let restart = tokio::spawn({
            let service = service.clone();
            async move { service.restart_requested().await }
        });

        let job = service.apply("9.9.9").await.unwrap();
        assert_eq!(job.phase, UpdatePhase::Downloading);
        let job = wait_for_terminal(&service).await;
        assert_eq!(job.phase, UpdatePhase::Restarting, "{:?}", job.message);
        assert_eq!(job.downloaded_bytes, TEST_BINARY.len() as u64);

        let target = tokio::time::timeout(Duration::from_secs(5), restart)
            .await
            .unwrap()
            .unwrap();
        let layout = UpgradeLayout::new(dir.path());
        assert_eq!(target.version, "9.9.9");
        assert_eq!(target.binary, layout.binary_path(&Version::new(9, 9, 9)));
        assert_eq!(std::fs::read(&target.binary).unwrap(), TEST_BINARY);
        assert_eq!(layout.read_state().unwrap().unwrap().active, "9.9.9");
        assert_eq!(service.pending_restart(), Some(target.clone()));
        assert_eq!(
            crate::update::decide_boot(&layout, "2.1.2"),
            crate::update::BootDecision::Exec {
                version: "9.9.9".into(),
                binary: target.binary,
            }
        );
        assert!(matches!(
            service.apply("9.9.9").await,
            Err(UpdateError::Busy)
        ));
    }

    #[tokio::test]
    async fn apply_rejects_tampered_binary_without_installing() {
        let dir = tempfile::tempdir().unwrap();
        let mut tampered = TEST_BINARY.to_vec();
        let last = tampered.len() - 2;
        tampered[last] ^= 0x01;
        let url = serve(tampered).await;
        let service = make_service(dir.path(), url, "2.1.2");

        service.apply("9.9.9").await.unwrap();
        let job = wait_for_terminal(&service).await;
        assert_eq!(job.phase, UpdatePhase::Failed);
        assert!(job.message.unwrap().contains("sha256"));
        let layout = UpgradeLayout::new(dir.path());
        assert!(layout.read_state().unwrap().is_none());
        assert!(!layout.versions_dir().exists());
        assert_eq!(std::fs::read_dir(layout.staging_dir()).unwrap().count(), 0);
        assert!(service.pending_restart().is_none());
    }

    #[tokio::test]
    async fn apply_rejects_truncated_binary() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(TEST_BINARY[..TEST_BINARY.len() / 2].to_vec()).await;
        let service = make_service(dir.path(), url, "2.1.2");
        service.apply("9.9.9").await.unwrap();
        let job = wait_for_terminal(&service).await;
        assert_eq!(job.phase, UpdatePhase::Failed);
        assert!(job.message.unwrap().contains("size"));
    }

    #[tokio::test]
    async fn apply_only_moves_forward_to_the_latest_release() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(TEST_BINARY.to_vec()).await;
        let behind = make_service(dir.path(), url.clone(), "2.1.2");
        assert!(matches!(
            behind.apply("9.9.8").await,
            Err(UpdateError::VersionMismatch { .. })
        ));
        assert!(matches!(
            behind.apply("not-a-version").await,
            Err(UpdateError::VersionMismatch { .. })
        ));
        let current = make_service(dir.path(), url, "9.9.9");
        assert!(matches!(
            current.apply("9.9.9").await,
            Err(UpdateError::NotNewer(_))
        ));
        assert_eq!(behind.job().phase, UpdatePhase::Idle);
    }

    #[tokio::test]
    async fn disabled_self_update_still_checks_but_refuses_apply() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(TEST_BINARY.to_vec()).await;
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let mut options = UpdateOptions::new(dir.path().to_path_buf());
        options.manifest_url = url;
        options.current_version = "2.1.2".into();
        options.self_update_enabled = false;
        let service =
            UpdateService::with_public_key(storage.clone(), options.clone(), TEST_PUBLIC_KEY)
                .unwrap();
        assert_eq!(
            service.version_info().self_update.reason.as_deref(),
            Some("disabled")
        );
        let check = service.check(true).await.unwrap();
        assert!(check.update_available);
        assert!(!check.can_apply);
        assert!(matches!(
            service.apply("9.9.9").await,
            Err(UpdateError::Unsupported(_))
        ));

        options.check_enabled = false;
        let offline = UpdateService::with_public_key(storage, options, TEST_PUBLIC_KEY).unwrap();
        assert!(!offline.version_info().update_check);
        assert!(matches!(
            offline.check(false).await,
            Err(UpdateError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn unreachable_release_host_is_a_fetch_error() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let service = make_service(
            dir.path(),
            format!("http://{addr}/nyro-server-update.json"),
            "2.1.2",
        );
        let error = service.check(true).await.unwrap_err();
        assert_eq!(error.code(), "UPDATE_FETCH_FAILED", "{error}");
    }
}
