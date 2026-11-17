//! `stormd` replacing itself with another release, on request from the app.
//!
//! The app asks for its own version (`POST /server/update`) when the server is
//! older. The server downloads that exact release from GitHub, verifies it
//! like the app's own updates do (SHA256SUMS + minisign, see `storm-update`),
//! checks that the new binary runs and reports the expected version, then
//! moves it in place of its own exe (keeping the previous one as `stormd.old`)
//! and asks `stormd` to restart: shut down cleanly, then run the new exe with
//! the same arguments (`exec` on Unix, so a supervisor keeps tracking the PID).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use storm_proto::Error;
use tokio::sync::Notify;

/// Set to `1` to let a debug build update itself (for testing).
pub const DEBUG_ENV: &str = "STORMD_UPDATE_DEBUG";

/// The release asset `stormd` updates from. Must match the release workflow.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ASSET: Option<&str> = Some("stormd-windows-x86_64.exe");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Option<&str> = Some("stormd-linux-x86_64");
#[cfg(not(any(all(target_os = "windows", target_arch = "x86_64"), all(target_os = "linux", target_arch = "x86_64"))))]
const ASSET: Option<&str> = None;

const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub struct SelfUpdate {
    /// Our exe, resolved at startup (once it's been renamed to `.old`, the OS
    /// may report that name instead).
    exe: PathBuf,
    asset: &'static str,
    /// An update is downloading, or installed and restarting.
    busy: AtomicBool,
    restart: Notify,
}

impl SelfUpdate {
    /// Updates for this build, unless it can't update itself: an unsupported
    /// platform, or a debug build without [`DEBUG_ENV`].
    pub fn for_this_build() -> Result<Arc<Self>, String> {
        let asset = ASSET.ok_or("no stormd release for this platform")?;
        if cfg!(debug_assertions) && !std::env::var(DEBUG_ENV).is_ok_and(|v| v == "1") {
            return Err(format!("debug build (set {DEBUG_ENV}=1 to allow)"));
        }
        let exe = std::env::current_exe().map_err(|e| format!("can't find our own exe: {e}"))?;
        // A download an earlier update didn't install. `.old` stays as a manual rollback.
        let _ = std::fs::remove_file(storm_update::sidecars(&exe).0);
        Ok(Arc::new(Self { exe, asset, busy: AtomicBool::new(false), restart: Notify::new() }))
    }

    /// The exe to restart into.
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Resolves once an installed update wants `stormd` to restart.
    pub async fn restart_requested(&self) {
        self.restart.notified().await
    }

    /// Downloads, verifies and installs release `version`, then (shortly
    /// after, so this response gets out) asks for a restart.
    pub async fn install(self: Arc<Self>, version: String) -> Result<(), Error> {
        let version = version.trim().trim_start_matches('v').to_owned();
        if !storm_update::is_newer(&version, CURRENT) {
            return Err(Error::bad_request(format!("the server is on {CURRENT}; {version} isn't newer")));
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(Error::bad_request("the server is already updating"));
        }
        tracing::info!("updating to {version}");
        let this = self.clone();
        let result = tokio::task::spawn_blocking(move || this.install_blocking(&version))
            .await
            .unwrap_or_else(|e| Err(format!("update task failed: {e}")));
        match result {
            Ok(()) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    self.restart.notify_one();
                });
                Ok(())
            }
            Err(e) => {
                tracing::warn!("update failed: {e}");
                let _ = std::fs::remove_file(storm_update::sidecars(&self.exe).0);
                self.busy.store(false, Ordering::SeqCst);
                Err(Error::internal(e))
            }
        }
    }

    fn install_blocking(&self, version: &str) -> Result<(), String> {
        let release = storm_update::release(version, self.asset).map_err(|e| e.to_string())?;
        let bytes = storm_update::download(&release).map_err(|e| e.to_string())?;
        let new = storm_update::stage(&self.exe, &bytes).map_err(|e| e.to_string())?;
        preflight(&new, version)?;
        storm_update::swap(&self.exe).map_err(|e| format!("replacing {}: {e}", self.exe.display()))?;
        tracing::info!("installed {version}; restarting");
        Ok(())
    }
}

/// The staged exe must run here and say it's `version`.
fn preflight(exe: &Path, version: &str) -> Result<(), String> {
    let out = Command::new(exe).arg("--version").output().map_err(|e| format!("the new build won't run: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || said != format!("stormd {version}") {
        return Err(format!("the new build reported {said:?} instead of stormd {version}"));
    }
    Ok(())
}
