//! Updating from GitHub Releases, for the desktop app and `stormd`: find a
//! release, download an asset of it, verify it, swap it in for the exe.
//!
//! Networking uses reqwest + rustls on a throwaway single-threaded tokio
//! runtime, since this runs rarely. The functions block; call them off any
//! async executor thread.
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

/// A release with a signed asset we can install.
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
        .user_agent(concat!("storm-update/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(10 * 60))
        .build()?)
}

/// The latest release, if it's newer than `current` and has `asset`. Blocking.
pub fn check(current: &str, asset: &str) -> Result<Option<Release>> {
    // None: no releases yet (or the repo isn't public).
    Ok(fetch("latest".into())?.and_then(|gh| pick_release(gh, current, asset)))
}

/// Release `version` (`x.y.z`), which must carry a signed `asset`. Blocking.
pub fn release(version: &str, asset: &str) -> Result<Release> {
    let gh = fetch(format!("tags/v{version}"))?.ok_or_else(|| err(format!("there's no release {version}")))?;
    complete_release(gh, asset).ok_or_else(|| err(format!("release {version} has no signed {asset}")))
}

fn fetch(which: String) -> Result<Option<GhRelease>> {
    block_on(async move {
        let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/{which}");
        let resp = client()?.get(url).header("Accept", "application/vnd.github+json").send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(resp.error_for_status()?.json().await?))
    })
}

fn pick_release(gh: GhRelease, current: &str, asset: &str) -> Option<Release> {
    if gh.draft || gh.prerelease || !is_newer(gh.tag_name.trim_start_matches('v'), current) {
        return None;
    }
    complete_release(gh, asset)
}

/// `gh` as a [`Release`], if it has `asset`, its signature and the checksums.
fn complete_release(gh: GhRelease, asset: &str) -> Option<Release> {
    let version = gh.tag_name.trim_start_matches('v').to_owned();
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
pub fn is_newer(a: &str, b: &str) -> bool {
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

/// Downloads `release`'s asset and verifies it. Blocking.
pub fn download(release: &Release) -> Result<Vec<u8>> {
    if release.asset_size > MAX_DOWNLOAD {
        return Err(err("update is unexpectedly large"));
    }
    block_on(async {
        let http = client()?;
        let sums = http.get(&release.sums_url).send().await?.error_for_status()?.text().await?;
        let sig = http.get(&release.sig_url).send().await?.error_for_status()?.text().await?;
        let bin = http.get(&release.asset_url).send().await?.error_for_status()?.bytes().await?;
        verify(&bin, &sig, &sums, &release.asset, &release.version)?;
        Ok(bin.to_vec())
    })
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

/// Paths next to `exe` used while swapping: `<exe>.new` and `<exe>.old`.
pub fn sidecars(exe: &Path) -> (PathBuf, PathBuf) {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "storm".into());
    (exe.with_file_name(format!("{name}.new")), exe.with_file_name(format!("{name}.old")))
}

/// Writes `bytes` to `<exe>.new`, executable. [`swap`] then puts it in place;
/// in between, the caller may try running it.
pub fn stage(exe: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let (new, _) = sidecars(exe);
    std::fs::write(&new, bytes).map_err(|e| err(format!("can't write next to {}: {e}", exe.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(new)
}

/// Moves a [`stage`]d `<exe>.new` in place of `exe`, keeping the previous one
/// as `<exe>.old`. Windows won't delete a running exe but will rename it, so
/// this works on the running exe too. Rolls back if the last step fails.
pub fn swap(exe: &Path) -> Result<()> {
    let (new, old) = sidecars(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&new);
        return Err(e.into());
    }
    Ok(())
}

/// Puts `bytes` in place of `exe`: [`stage`] then [`swap`].
pub fn install(exe: &Path, bytes: &[u8]) -> Result<()> {
    stage(exe, bytes)?;
    swap(exe)
}

/// Deletes what an earlier update left beside `exe`.
pub fn remove_leftovers(exe: &Path) {
    let (new, old) = sidecars(exe);
    let _ = std::fs::remove_file(old);
    let _ = std::fs::remove_file(new);
}

#[cfg(test)]
mod tests {
    use super::{GhAsset, GhRelease, checksum_for, complete_release, is_newer, pick_release, verify};

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

    /// Talks to GitHub: `cargo test -p storm-update live_check -- --ignored`.
    #[test]
    #[ignore]
    fn live_check() {
        println!("{:?}", super::check("0.0.0", "storm-windows-x86_64.exe").map(|r| r.map(|r| r.version)));
        println!("{:?}", super::release("0.1.3", "stormd-windows-x86_64.exe").map(|r| r.version));
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

    #[test]
    fn an_exact_release_needs_its_signed_asset_but_not_to_be_newer() {
        let all = ["stormd-linux-x86_64", "stormd-linux-x86_64.minisig", "SHA256SUMS"];
        assert_eq!(complete_release(release("v0.1.0", &all), all[0]).unwrap().version, "0.1.0");
        assert!(complete_release(release("v0.1.0", &all[..2]), all[0]).is_none());
    }

    #[test]
    fn install_swaps_and_keeps_the_previous_exe() {
        let dir = std::env::temp_dir().join(format!("storm-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("stormd");
        std::fs::write(&exe, b"old").unwrap();
        super::install(&exe, b"new").unwrap();
        let (new, old) = super::sidecars(&exe);
        let result = (std::fs::read(&exe).unwrap(), std::fs::read(&old).unwrap(), new.exists());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result, (b"new".to_vec(), b"old".to_vec(), false));
    }
}
