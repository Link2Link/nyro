//! On-disk layout of installed upgrades and the startup hand-off decision.
//!
//! ```text
//! <data_dir>/upgrade/
//!   state.json                    {"schema":1,"active":"2.2.0",...}
//!   versions/<version>/nyro-server
//!   staging/                      partial downloads
//! ```
//!
//! This layout is a compatibility contract: the binary baked into an image
//! performs the boot decision for every later version it hands off to.

use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

pub const BINARY_NAME: &str = "nyro-server";
const STATE_SCHEMA: u32 = 1;

#[derive(Debug, Clone)]
pub struct UpgradeLayout {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeState {
    pub schema: u32,
    /// Canonical semver of the installed binary that should run.
    pub active: String,
    #[serde(default)]
    pub installed_at: Option<i64>,
}

impl UpgradeState {
    pub fn new(active: &Version) -> Self {
        Self {
            schema: STATE_SCHEMA,
            active: active.to_string(),
            installed_at: Some(chrono::Utc::now().timestamp()),
        }
    }
}

impl UpgradeLayout {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("upgrade"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    pub fn version_dir(&self, version: &Version) -> PathBuf {
        self.versions_dir().join(version.to_string())
    }

    pub fn binary_path(&self, version: &Version) -> PathBuf {
        self.version_dir(version).join(BINARY_NAME)
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// `Ok(None)` when nothing has been installed yet.
    pub fn read_state(&self) -> anyhow::Result<Option<UpgradeState>> {
        match std::fs::read(self.state_path()) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Atomically replace `state.json` (write temp, fsync, rename).
    pub fn write_state(&self, state: &UpgradeState) -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.root)?;
        let tmp = self.root.join("state.json.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(state)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, self.state_path())?;
        sync_dir(&self.root);
        Ok(())
    }

    /// Remove every installed version except `keep`, plus staging leftovers.
    /// Unlinking the binary of a running process is safe on Unix.
    pub fn prune(&self, keep: Option<&Version>) -> std::io::Result<()> {
        let keep = keep.map(Version::to_string);
        match std::fs::read_dir(self.versions_dir()) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if keep.as_deref() != entry.file_name().to_str() {
                        remove_path(&entry.path())?;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match std::fs::remove_dir_all(self.staging_dir()) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Forget installed upgrades: the running binary supersedes them. The
    /// state file goes first so an interrupted clear still boots correctly.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.state_path()) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        self.prune(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    /// Nothing usable is installed; run the current binary.
    RunCurrent,
    /// The current binary is at least as new as the installed upgrade (for
    /// example the image was rebuilt); installed upgrades can be discarded.
    Superseded,
    /// Hand off to the newer installed binary.
    Exec { version: String, binary: PathBuf },
}

/// Decide which binary should serve this start. Never fails: unreadable state
/// or a missing binary falls back to running the current binary.
pub fn decide_boot(layout: &UpgradeLayout, current_version: &str) -> BootDecision {
    let Ok(current) = Version::parse(current_version) else {
        return BootDecision::RunCurrent;
    };
    let state = match layout.read_state() {
        Ok(Some(state)) => state,
        Ok(None) => return BootDecision::RunCurrent,
        Err(error) => {
            tracing::warn!("ignoring unreadable upgrade state: {error}");
            return BootDecision::RunCurrent;
        }
    };
    let Ok(active) = Version::parse(&state.active) else {
        tracing::warn!(
            "ignoring upgrade state with invalid version `{}`",
            state.active
        );
        return BootDecision::RunCurrent;
    };
    if active <= current {
        return BootDecision::Superseded;
    }
    let binary = layout.binary_path(&active);
    if !is_executable(&binary) {
        tracing::warn!(
            "installed version {active} is missing its binary at {}; running {current}",
            binary.display()
        );
        return BootDecision::RunCurrent;
    }
    BootDecision::Exec {
        version: active.to_string(),
        binary,
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub(crate) fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(layout: &UpgradeLayout, version: &str) -> PathBuf {
        let version = Version::parse(version).unwrap();
        let path = layout.binary_path(&version);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn activate(layout: &UpgradeLayout, version: &str) {
        layout
            .write_state(&UpgradeState::new(&Version::parse(version).unwrap()))
            .unwrap();
    }

    #[test]
    fn fresh_data_dir_runs_current() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        assert_eq!(decide_boot(&layout, "2.1.2"), BootDecision::RunCurrent);
    }

    #[test]
    fn newer_installed_version_is_exec_target() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        let binary = install(&layout, "2.2.0");
        activate(&layout, "2.2.0");
        assert_eq!(
            decide_boot(&layout, "2.1.2"),
            BootDecision::Exec {
                version: "2.2.0".into(),
                binary,
            }
        );
    }

    #[test]
    fn prerelease_ordering_follows_semver() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        install(&layout, "2.1.3-rc.1");
        activate(&layout, "2.1.3-rc.1");
        assert!(matches!(
            decide_boot(&layout, "2.1.2"),
            BootDecision::Exec { .. }
        ));
        assert_eq!(decide_boot(&layout, "2.1.3"), BootDecision::Superseded);
    }

    #[test]
    fn equal_or_older_installed_version_is_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        install(&layout, "2.2.0");
        activate(&layout, "2.2.0");
        assert_eq!(decide_boot(&layout, "2.2.0"), BootDecision::Superseded);
        assert_eq!(decide_boot(&layout, "3.0.0"), BootDecision::Superseded);
    }

    #[test]
    fn missing_binary_or_corrupt_state_runs_current() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        activate(&layout, "2.2.0");
        assert_eq!(decide_boot(&layout, "2.1.2"), BootDecision::RunCurrent);

        std::fs::write(layout.state_path(), b"{not json").unwrap();
        assert_eq!(decide_boot(&layout, "2.1.2"), BootDecision::RunCurrent);

        std::fs::write(layout.state_path(), br#"{"schema":1,"active":"../../x"}"#).unwrap();
        assert_eq!(decide_boot(&layout, "2.1.2"), BootDecision::RunCurrent);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_binary_runs_current() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        let binary = install(&layout, "2.2.0");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
        activate(&layout, "2.2.0");
        assert_eq!(decide_boot(&layout, "2.1.2"), BootDecision::RunCurrent);
    }

    #[test]
    fn prune_keeps_only_requested_version_and_clear_forgets_state() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        install(&layout, "2.1.3");
        install(&layout, "2.2.0");
        std::fs::create_dir_all(layout.staging_dir()).unwrap();
        std::fs::write(layout.staging_dir().join("partial"), b"x").unwrap();
        activate(&layout, "2.2.0");

        layout.prune(Some(&Version::new(2, 2, 0))).unwrap();
        assert!(!layout.version_dir(&Version::new(2, 1, 3)).exists());
        assert!(layout.binary_path(&Version::new(2, 2, 0)).exists());
        assert!(!layout.staging_dir().exists());

        layout.clear().unwrap();
        assert!(layout.read_state().unwrap().is_none());
        assert!(!layout.version_dir(&Version::new(2, 2, 0)).exists());
        assert_eq!(decide_boot(&layout, "2.1.2"), BootDecision::RunCurrent);
    }

    #[test]
    fn state_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let layout = UpgradeLayout::new(dir.path());
        let state = UpgradeState::new(&Version::new(2, 2, 0));
        layout.write_state(&state).unwrap();
        assert_eq!(layout.read_state().unwrap(), Some(state));
        assert!(!layout.root().join("state.json.tmp").exists());
    }
}
