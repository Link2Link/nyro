//! Process side of self-update: the startup hand-off to an installed newer
//! binary and the exec that replaces this process after an update.

use std::path::Path;

use nyro_core::update::{
    BOOTSTRAPPED_ENV, BootDecision, RestartTarget, UpgradeLayout, decide_boot,
};

/// Run before anything binds a port. When a newer binary is installed under
/// `data_dir`, replace this process with it (same argv and environment).
/// Returns normally when the current binary should keep running.
pub fn bootstrap(data_dir: &Path) {
    if std::env::var_os(BOOTSTRAPPED_ENV).is_some() {
        return;
    }
    let layout = UpgradeLayout::new(data_dir);
    match decide_boot(&layout, env!("CARGO_PKG_VERSION")) {
        BootDecision::RunCurrent => {}
        BootDecision::Superseded => {
            tracing::info!(
                "running {} supersedes installed upgrades; removing them",
                env!("CARGO_PKG_VERSION")
            );
            if let Err(error) = layout.clear() {
                tracing::warn!("failed to remove superseded upgrades: {error}");
            }
        }
        BootDecision::Exec { version, binary } => {
            tracing::info!("handing off to installed nyro-server {version}");
            let error = exec(&binary);
            tracing::error!(
                "failed to start installed version {version} ({}): {error}; continuing with {}",
                binary.display(),
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}

/// Replace this process with the freshly installed binary. Only returns on
/// failure; the caller should exit non-zero so the supervisor restarts the
/// instance, which then boots the installed version.
pub fn restart_into(target: &RestartTarget) -> std::io::Error {
    tracing::info!("restarting into nyro-server {}", target.version);
    exec(&target.binary)
}

#[cfg(unix)]
fn exec(binary: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(binary)
        .args(std::env::args_os().skip(1))
        .env(BOOTSTRAPPED_ENV, "1")
        .exec()
}

#[cfg(not(unix))]
fn exec(_binary: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "self-update restart requires a Unix host",
    )
}
