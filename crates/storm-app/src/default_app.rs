//! Opening magnet links and `.torrent` files with Storm.
//!
//! Storm is a portable exe without an installer, so it registers itself, per
//! user (no admin): a ProgID each for magnet links and `.torrent` files,
//! `Capabilities` so it's listed under Settings > Default apps, and the
//! `magnet:` / `.torrent` keys themselves so it works right away when no other
//! client is installed. Windows 10+ never lets an app silently take over an
//! association the user already chose, so after registering, Storm opens its
//! Default apps page for the user to confirm.
//!
//! The command line is `storm.exe "%1"`; `single_instance` hands it to a
//! running Storm if there is one.

/// Windows Settings page listing what Storm can open.
#[cfg_attr(not(windows), allow(dead_code))]
pub const SETTINGS_URL: &str = "ms-settings:defaultapps?registeredAppUser=Storm";

/// Whether Windows currently opens each kind with this exe. `None` where the
/// platform has no such integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handles {
    pub magnet: bool,
    pub torrent: bool,
}

#[cfg(windows)]
pub use imp::{refresh_if_stale, register, status};

#[cfg(not(windows))]
pub fn status() -> Option<Handles> {
    None
}

#[cfg(not(windows))]
pub fn register() -> std::io::Result<()> {
    Err(std::io::Error::other("not supported on this platform"))
}

#[cfg(not(windows))]
pub fn refresh_if_stale() {}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegGetValueW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::Shell::{
        ASSOCF_NONE, ASSOCSTR_EXECUTABLE, AssocQueryStringW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify,
    };

    use super::Handles;

    const MAGNET_PROGID: &str = "Storm.Magnet";
    const TORRENT_PROGID: &str = "Storm.Torrent";

    pub fn status() -> Option<Handles> {
        let exe = std::env::current_exe().ok()?;
        let is_us = |assoc: &str| handler_of(assoc).is_some_and(|h| same_file(&h, &exe));
        Some(Handles { magnet: is_us("magnet"), torrent: is_us(".torrent") })
    }

    /// Registers this exe for magnet links and `.torrent` files.
    pub fn register() -> io::Result<()> {
        let exe = std::env::current_exe()?;
        write_registration(&Reg::USER, &exe)?;
        // Explorer caches associations.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, null(), null()) };
        Ok(())
    }

    /// If Storm was registered from an exe that no longer exists (moved or
    /// renamed), points the registration at this one. Leaves a working
    /// registration alone, so a second copy (say, a dev build) doesn't take it.
    pub fn refresh_if_stale() {
        let Some(registered) = Reg::USER.read(&format!("Classes\\{MAGNET_PROGID}\\shell\\open\\command"), None) else {
            return;
        };
        if command_exe(&registered).is_some_and(|p| p.exists()) {
            return;
        }
        match register() {
            Ok(()) => tracing::info!("updated the magnet/.torrent registration to this exe"),
            Err(e) => tracing::warn!("updating the magnet/.torrent registration: {e}"),
        }
    }

    /// Where registry keys go: `HKCU\Software` normally, elsewhere in tests.
    pub(super) struct Reg {
        pub root: &'static str,
    }

    impl Reg {
        pub const USER: Reg = Reg { root: "Software" };

        fn set(&self, key: &str, name: Option<&str>, value: &str) -> io::Result<()> {
            let path = wide(&format!("{}\\{key}", self.root));
            let mut h: HKEY = null_mut();
            let r = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    path.as_ptr(),
                    0,
                    null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE,
                    null(),
                    &mut h,
                    null_mut(),
                )
            };
            if r != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(r as i32));
            }
            let data = wide(value);
            let name = name.map(wide);
            let r = unsafe {
                RegSetValueExW(
                    h,
                    name.as_ref().map_or(null(), |n| n.as_ptr()),
                    0,
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            unsafe { RegCloseKey(h) };
            if r != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(r as i32));
            }
            Ok(())
        }

        pub(super) fn read(&self, key: &str, name: Option<&str>) -> Option<String> {
            let path = wide(&format!("{}\\{key}", self.root));
            let name = name.map(wide);
            let mut buf = vec![0u16; 1024];
            let mut bytes = (buf.len() * 2) as u32;
            let r = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    path.as_ptr(),
                    name.as_ref().map_or(null(), |n| n.as_ptr()),
                    RRF_RT_REG_SZ,
                    null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            (r == ERROR_SUCCESS).then(|| from_wide(&buf[..bytes as usize / 2]))
        }
    }

    pub(super) fn write_registration(reg: &Reg, exe: &Path) -> io::Result<()> {
        let exe = exe.display();
        let command = format!("\"{exe}\" \"%1\"");
        let icon = format!("\"{exe}\",0");
        let classes = "Classes";

        // ProgIDs.
        for (progid, description, url) in
            [(MAGNET_PROGID, "Magnet link", true), (TORRENT_PROGID, "BitTorrent file", false)]
        {
            let key = format!("{classes}\\{progid}");
            reg.set(&key, None, description)?;
            if url {
                reg.set(&key, Some("URL Protocol"), "")?;
            }
            reg.set(&format!("{key}\\DefaultIcon"), None, &icon)?;
            reg.set(&format!("{key}\\shell\\open\\command"), None, &command)?;
        }

        // The scheme and extension themselves, used when the user hasn't picked
        // another app in Settings (that choice wins and isn't ours to change).
        reg.set(&format!("{classes}\\magnet"), None, "URL:Magnet link")?;
        reg.set(&format!("{classes}\\magnet"), Some("URL Protocol"), "")?;
        reg.set(&format!("{classes}\\magnet\\DefaultIcon"), None, &icon)?;
        reg.set(&format!("{classes}\\magnet\\shell\\open\\command"), None, &command)?;
        reg.set(&format!("{classes}\\.torrent"), None, TORRENT_PROGID)?;
        reg.set(&format!("{classes}\\.torrent"), Some("Content Type"), "application/x-bittorrent")?;
        reg.set(&format!("{classes}\\.torrent\\OpenWithProgids"), Some(TORRENT_PROGID), "")?;

        // Listed under Settings > Default apps.
        let caps = "Storm\\Capabilities";
        reg.set(caps, Some("ApplicationName"), "Storm")?;
        reg.set(caps, Some("ApplicationDescription"), "BitTorrent client")?;
        reg.set(&format!("{caps}\\URLAssociations"), Some("magnet"), MAGNET_PROGID)?;
        reg.set(&format!("{caps}\\FileAssociations"), Some(".torrent"), TORRENT_PROGID)?;
        reg.set("RegisteredApplications", Some("Storm"), &format!("{}\\{caps}", reg.root))?;
        Ok(())
    }

    /// The exe Windows runs to open `assoc` (a scheme like `magnet` or an
    /// extension like `.torrent`), after the user's choice in Settings.
    fn handler_of(assoc: &str) -> Option<PathBuf> {
        let assoc = wide(assoc);
        let open = wide("open");
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let hr = unsafe {
            AssocQueryStringW(
                ASSOCF_NONE,
                ASSOCSTR_EXECUTABLE,
                assoc.as_ptr(),
                open.as_ptr(),
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        (hr >= 0).then(|| PathBuf::from(from_wide(&buf[..len as usize])))
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        }
    }

    /// The exe in a `"C:\path\storm.exe" "%1"` command line.
    pub(super) fn command_exe(command: &str) -> Option<PathBuf> {
        let rest = command.trim().strip_prefix('"')?;
        Some(PathBuf::from(&rest[..rest.find('"')?]))
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(s: &[u16]) -> String {
        let end = s.iter().position(|c| *c == 0).unwrap_or(s.len());
        String::from_utf16_lossy(&s[..end])
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp::{Reg, command_exe, write_registration};

    #[test]
    fn command_exe_reads_the_quoted_path() {
        assert_eq!(command_exe(r#""C:\A B\storm.exe" "%1""#).unwrap(), std::path::Path::new(r"C:\A B\storm.exe"));
        assert!(command_exe("storm.exe %1").is_none());
    }

    /// Writes into a throwaway key under HKCU, never the real associations.
    #[test]
    fn registration_points_both_kinds_at_the_exe() {
        const ROOT: &str = "Software\\StormTest-default-app";
        let reg = Reg { root: ROOT };
        let exe = std::path::Path::new(r"C:\Apps\Storm\storm.exe");
        write_registration(&reg, exe).unwrap();
        let command = r#""C:\Apps\Storm\storm.exe" "%1""#;
        let read = |key: &str, name: Option<&str>| reg.read(key, name);
        let result = (
            read("Classes\\Storm.Magnet\\shell\\open\\command", None),
            read("Classes\\Storm.Magnet", Some("URL Protocol")),
            read("Classes\\magnet\\shell\\open\\command", None),
            read("Classes\\.torrent", None),
            read("Classes\\Storm.Torrent\\shell\\open\\command", None),
            read("Storm\\Capabilities\\URLAssociations", Some("magnet")),
            read("Storm\\Capabilities\\FileAssociations", Some(".torrent")),
            read("RegisteredApplications", Some("Storm")),
        );
        // Clean up before asserting, so a failure doesn't leave the key behind.
        let root = ROOT.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        unsafe {
            windows_sys::Win32::System::Registry::RegDeleteTreeW(
                windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
                root.as_ptr(),
            )
        };
        assert_eq!(result.0.as_deref(), Some(command));
        assert_eq!(result.1.as_deref(), Some(""));
        assert_eq!(result.2.as_deref(), Some(command));
        assert_eq!(result.3.as_deref(), Some("Storm.Torrent"));
        assert_eq!(result.4.as_deref(), Some(command));
        assert_eq!(result.5.as_deref(), Some("Storm.Magnet"));
        assert_eq!(result.6.as_deref(), Some("Storm.Torrent"));
        assert_eq!(result.7, Some(format!("{ROOT}\\Storm\\Capabilities")));
    }
}
