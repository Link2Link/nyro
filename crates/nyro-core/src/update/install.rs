//! Download, verify and stage a release binary.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt;
use semver::Version;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::boot::{UpgradeLayout, sync_dir};
use super::error::{UpdateError, io_error};

/// Abort a download that receives no bytes for this long.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Stream `response` into `dest`, enforcing `expected_size`, and return the
/// hex sha256. `on_progress` receives the running byte count.
pub(crate) async fn download_to(
    response: reqwest::Response,
    dest: &Path,
    expected_size: u64,
    mut on_progress: impl FnMut(u64),
) -> Result<String, UpdateError> {
    if let Some(length) = response.content_length()
        && length != expected_size
    {
        return Err(UpdateError::Verification(format!(
            "download size {length} does not match the signed size {expected_size}"
        )));
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(io_error("create staging file"))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut stream = response.bytes_stream();
    loop {
        let chunk = match tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next()).await {
            Err(_) => return Err(UpdateError::Fetch("download stalled".into())),
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(UpdateError::Fetch(error.to_string())),
            Ok(Some(Ok(chunk))) => chunk,
        };
        received += chunk.len() as u64;
        if received > expected_size {
            return Err(UpdateError::Verification(format!(
                "download exceeds the signed size {expected_size}"
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(io_error("write staging file"))?;
        on_progress(received);
    }
    file.flush().await.map_err(io_error("flush staging file"))?;
    file.sync_all()
        .await
        .map_err(io_error("sync staging file"))?;
    if received != expected_size {
        return Err(UpdateError::Verification(format!(
            "download is {received} bytes, expected {expected_size}"
        )));
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) async fn make_executable(path: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(io_error("chmod staged binary"))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Run `<binary> --version` and require it to report `expected`. This catches
/// wrong-architecture or libc-incompatible builds and noexec mounts before the
/// running instance is touched.
pub(crate) async fn smoke_test(binary: &Path, expected: &Version) -> Result<(), UpdateError> {
    let mut last_error = None;
    // A just-closed write handle can briefly race exec (ETXTBSY); retry.
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let run = tokio::process::Command::new(binary)
            .arg("--version")
            .env_clear()
            .env(super::BOOTSTRAPPED_ENV, "1")
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(SMOKE_TEST_TIMEOUT, run).await {
            Err(_) => {
                return Err(UpdateError::Verification(
                    "new binary did not answer --version in time".into(),
                ));
            }
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                last_error = Some(error);
            }
            Ok(Err(error)) => {
                return Err(UpdateError::Verification(format!(
                    "new binary cannot be executed on this host: {error}"
                )));
            }
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let expected = expected.to_string();
                if output.status.success()
                    && stdout.split_whitespace().any(|token| token == expected)
                {
                    return Ok(());
                }
                return Err(UpdateError::Verification(format!(
                    "new binary reported `{}` instead of version {expected}",
                    stdout.trim()
                )));
            }
        }
    }
    Err(UpdateError::Verification(format!(
        "new binary cannot be executed on this host: {}",
        last_error.map(|e| e.to_string()).unwrap_or_default()
    )))
}

/// Move a verified staged binary to `versions/<version>/nyro-server`.
pub(crate) fn promote(
    layout: &UpgradeLayout,
    staged: &Path,
    version: &Version,
) -> Result<std::path::PathBuf, UpdateError> {
    let dir = layout.version_dir(version);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io_error("replace version directory"))?;
    }
    std::fs::create_dir_all(&dir).map_err(io_error("create version directory"))?;
    let binary = layout.binary_path(version);
    std::fs::rename(staged, &binary).map_err(io_error("install binary"))?;
    sync_dir(&dir);
    Ok(binary)
}
