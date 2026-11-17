//! The mechanics of updating from GitHub Releases: find the latest release,
//! download our asset, verify it, swap it in. No UI here (see `updater`).
//!
//! Networking uses the reqwest + rustls stack Storm already ships (for the
//! remote-server client), on a throwaway single-threaded tokio runtime, since
//! this runs a few times a day at most.
//!
//! A download is only installed if
//! - its SHA-256 matches the release's `SHA256SUMS` entry, and
//! - it carries a minisign signature by [`MINISIGN_PUBLIC_KEY`] whose trusted
//!   comment names this exact file and version (`file:<asset>` and
//!   `version:<v>`), so an older signed build can't be replayed as a newer one.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

const OWNER: &str = "ttmx";
const REPO: &str = "storm";

/// Public half of the release signing key (`minisign`). The secret half is the
/// `MINISIGN_SECRET_KEY` secret of the GitHub repo, used by the release workflow.
pub const MINISIGN_PUBLIC_KEY: &str = "RWTgrKK3kURq20adSQ1+URhVQ95yAr26C+Mx60oZFLDaRcdbca6XgSA8";

/// The release asset this build updates from. Must match the release workflow.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub const ASSET: Option<&str> = Some("storm-windows-x86_64.exe");
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
pub const ASSET: Option<&str> = None;

const SUMS_ASSET: &str = "SHA256SUMS";
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(msg: impl Into<String>) -> Error {
    Error(msg.into())
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        err(format!("network error: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        err(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A newer release we can install.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    asset: String,
    asset_url: String,
    asset_size: u64,
    sums_url: String,
    sig_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Runs `fut` on a fresh current-thread tokio runtime (reqwest needs one).
fn block_on<T>(fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(fut)
}

fn client() -> Result<reqwest::Client> {
    // ring and aws-lc are both linked; rustls needs to be told which to use.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .user_agent(concat!("storm/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(10 * 60))
        .build()?)
}

/// The latest release, if it's newer than `current` and has our asset. Blocking.
pub fn check(current: &str) -> Result<Option<Release>> {
    let asset = ASSET.ok_or_else(|| err("no update asset for this platform"))?;
    block_on(async move {
        let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest");
        let resp = client()?.get(url).header("Accept", "application/vnd.github+json").send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // No releases yet (or the repo isn't public).
            return Ok(None);
        }
        let gh: GhRelease = resp.error_for_status()?.json().await?;
        Ok(pick_release(gh, current, asset))
    })
}

fn pick_release(gh: GhRelease, current: &str, asset: &str) -> Option<Release> {
    if gh.draft || gh.prerelease {
        return None;
    }
    let version = gh.tag_name.trim_start_matches('v').to_owned();
    if !is_newer(&version, current) {
        return None;
    }
    let url_of = |name: &str| gh.assets.iter().find(|a| a.name == name);
    let bin = url_of(asset)?;
    let sums = url_of(SUMS_ASSET)?;
    let sig = url_of(&format!("{asset}.minisig"))?;
    Some(Release {
        version,
        asset: asset.to_owned(),
        asset_url: bin.browser_download_url.clone(),
        asset_size: bin.size,
        sums_url: sums.browser_download_url.clone(),
        sig_url: sig.browser_download_url.clone(),
    })
}

/// `a > b` for plain `major.minor.patch` versions. Anything with a pre-release
/// or unparsable part is never "newer".
fn is_newer(a: &str, b: &str) -> bool {
    fn parse(v: &str) -> Option<(u64, u64, u64)> {
        let v = v.split('+').next()?;
        if v.contains('-') {
            return None;
        }
        let mut it = v.split('.').map(|p| p.parse::<u64>().ok());
        let parsed = (it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(parsed)
    }
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a > b)
}

/// Downloads and verifies `release`, then installs it in place of the running
/// exe. Returns the path to restart into. Blocking.
pub fn download_and_install(release: &Release) -> Result<PathBuf> {
    if release.asset_size > MAX_DOWNLOAD {
        return Err(err("update is unexpectedly large"));
    }
    let bytes = block_on(async {
        let http = client()?;
        let sums = http.get(&release.sums_url).send().await?.error_for_status()?.text().await?;
        let sig = http.get(&release.sig_url).send().await?.error_for_status()?.text().await?;
        let bin = http.get(&release.asset_url).send().await?.error_for_status()?.bytes().await?;
        verify(&bin, &sig, &sums, &release.asset, &release.version)?;
        Ok(bin)
    })?;
    install(&bytes)
}

fn verify(bin: &[u8], sig: &str, sums: &str, asset: &str, version: &str) -> Result<()> {
    let expected = checksum_for(sums, asset).ok_or_else(|| err(format!("{asset} is missing from {SUMS_ASSET}")))?;
    let actual = hex(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bin).as_ref());
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(err("checksum mismatch"));
    }
    let key = minisign_verify::PublicKey::from_base64(MINISIGN_PUBLIC_KEY).map_err(|e| err(format!("bad key: {e}")))?;
    let sig = minisign_verify::Signature::decode(sig).map_err(|e| err(format!("bad signature: {e}")))?;
    key.verify(bin, &sig, false).map_err(|e| err(format!("signature check failed: {e}")))?;
    // The trusted comment is covered by the signature: make sure it was made
    // for this file and this version.
    let tokens: Vec<&str> = sig.trusted_comment().split_whitespace().collect();
    let wants = [format!("file:{asset}"), format!("version:{version}")];
    if !wants.iter().all(|w| tokens.contains(&w.as_str())) {
        return Err(err("signature was made for a different file or version"));
    }
    Ok(())
}

/// The hash listed for `asset` in a `sha256sum`-style file.
fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (name == asset && hash.len() == 64).then(|| hash.to_owned())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Paths next to the running exe used while swapping.
fn sidecars(exe: &Path) -> (PathBuf, PathBuf) {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "storm.exe".into());
    (exe.with_file_name(format!("{name}.new")), exe.with_file_name(format!("{name}.old")))
}

/// Puts `bytes` in place of the running exe. Windows won't delete a running
/// exe but will rename it, so: write `.new`, move the current one to `.old`,
/// move `.new` into place (and roll back if that fails).
fn install(bytes: &[u8]) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let (new, old) = sidecars(&exe);
    std::fs::write(&new, bytes).map_err(|e| err(format!("can't write next to {}: {e}", exe.display())))?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old)?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        let _ = std::fs::remove_file(&new);
        return Err(e.into());
    }
    Ok(exe)
}

/// Deletes what an earlier update left beside the exe. Call at startup.
pub fn remove_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        let (new, old) = sidecars(&exe);
        let _ = std::fs::remove_file(old);
        let _ = std::fs::remove_file(new);
    }
}

#[cfg(test)]
mod tests {
    use super::{GhAsset, GhRelease, checksum_for, is_newer, pick_release, verify};

    // Signed with the real release key as file `test-fixture.bin`, version
    // 0.0.1 (so it can never pass as an actual update).
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/update/test-fixture.bin");
    const FIXTURE_SIG: &str = include_str!("../tests/fixtures/update/test-fixture.bin.minisig");
    const FIXTURE_SUMS: &str = include_str!("../tests/fixtures/update/SHA256SUMS");

    #[test]
    fn accepts_a_correctly_signed_file() {
        verify(FIXTURE, FIXTURE_SIG, FIXTURE_SUMS, "test-fixture.bin", "0.0.1").unwrap();
    }

    #[test]
    fn rejects_tampering_and_replays() {
        let mut tampered = FIXTURE.to_vec();
        tampered[0] ^= 1;
        assert!(verify(&tampered, FIXTURE_SIG, FIXTURE_SUMS, "test-fixture.bin", "0.0.1").is_err());
        // Right bytes and checksum, but the signature names another version...
        assert!(verify(FIXTURE, FIXTURE_SIG, FIXTURE_SUMS, "test-fixture.bin", "9.9.9").is_err());
        // ...or the sums list it under another name.
        let renamed = FIXTURE_SUMS.replace("test-fixture.bin", "storm-windows-x86_64.exe");
        assert!(verify(FIXTURE, FIXTURE_SIG, &renamed, "storm-windows-x86_64.exe", "0.0.1").is_err());
        // A checksum that doesn't match is refused before the signature is looked at.
        let wrong = format!(
            "{}  test-fixture.bin
",
            "0".repeat(64)
        );
        assert!(verify(FIXTURE, FIXTURE_SIG, &wrong, "test-fixture.bin", "0.0.1").is_err());
    }

    /// Talks to GitHub: `cargo test -p storm-app live_check -- --ignored`.
    #[test]
    #[ignore]
    fn live_check() {
        println!("{:?}", super::check("0.0.0").map(|r| r.map(|r| r.version)));
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.3.0-beta.1", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("1.2", "0.1.0"));
    }

    #[test]
    fn checksums_parse_both_sha256sum_styles() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let sums = format!("{a}  storm-windows-x86_64.exe\n{b} *storm-windows-x86_64.exe.minisig\n");
        assert_eq!(checksum_for(&sums, "storm-windows-x86_64.exe"), Some(a));
        assert_eq!(checksum_for(&sums, "storm-windows-x86_64.exe.minisig"), Some(b));
        assert_eq!(checksum_for(&sums, "other.exe"), None);
    }

    fn release(tag: &str, names: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: names
                .iter()
                .map(|n| GhAsset { name: (*n).into(), browser_download_url: format!("https://x/{n}"), size: 1 })
                .collect(),
        }
    }

    #[test]
    fn picks_only_complete_newer_releases() {
        let all = ["storm-windows-x86_64.exe", "storm-windows-x86_64.exe.minisig", "SHA256SUMS"];
        let r = pick_release(release("v0.2.0", &all), "0.1.0", all[0]).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.sig_url, "https://x/storm-windows-x86_64.exe.minisig");
        // Same version, missing signature, or a different asset: nothing to do.
        assert!(pick_release(release("v0.1.0", &all), "0.1.0", all[0]).is_none());
        assert!(pick_release(release("v0.2.0", &all[..1]), "0.1.0", all[0]).is_none());
        assert!(pick_release(release("v0.2.0", &all), "0.1.0", "stormd-windows-x86_64.exe").is_none());
    }
}
