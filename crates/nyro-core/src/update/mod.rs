//! Self-update: discover signed server releases and install them in place.
//!
//! A release publishes `nyro-server-update.json`, a minisign-signed envelope
//! around a manifest of per-platform static binaries. An instance verifies it
//! with the public key compiled into this crate, downloads the matching binary
//! into `<data_dir>/upgrade/versions/<version>/`, checks its sha256, smoke-tests
//! it, records it as the active version and asks the host process to restart
//! into it. Versions only move forward; there is no rollback.
//!
//! Process concerns stay with the host (`nyro-server`): at startup it calls
//! [`decide_boot`] to hand off to an installed newer binary, and it performs the
//! restart once [`UpdateService::restart_requested`] resolves.

pub mod boot;
mod error;
mod install;
mod manifest;
mod service;

pub use boot::{BINARY_NAME, BootDecision, UpgradeLayout, UpgradeState, decide_boot};
pub use error::UpdateError;
pub use manifest::{ReleaseAsset, ReleaseManifest, current_platform};
pub use service::{
    RestartTarget, SelfUpdateSupport, UpdateCheck, UpdateJob, UpdateOptions, UpdatePhase,
    UpdateService, VersionInfo,
};

/// Default discovery URL: the signed manifest attached to the latest release.
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/Link2Link/nyro/releases/latest/download/nyro-server-update.json";

/// Set on a process started by the boot hand-off or an update restart so the
/// child does not repeat the boot decision.
pub const BOOTSTRAPPED_ENV: &str = "NYRO_BOOTSTRAPPED";
