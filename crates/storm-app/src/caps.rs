//! The one place that decides how the UI adapts to a local vs remote backend.
//!
//! Views never call `BackendInfo::is_local()` themselves: they ask [`Capabilities`]
//! a question about *behaviour* ("can I open this file?"), or they call
//! [`pick_backend_folder`], which hides the native-vs-remote picker split.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Global, PathPromptOptions, Window};
use storm_proto::{Backend, BackendInfo, BackendKind};

use crate::views::folder_picker::RemoteFolderPicker;

/// Env var that makes a local backend use the remote UI paths (in-app folder
/// browser, "Download to this PC"...). Handy for testing the remote UI without a server.
pub const FORCE_REMOTE_UI_ENV: &str = "STORM_FORCE_REMOTE_UI";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The backend's files live on this machine's filesystem.
    shares_filesystem: bool,
    /// What the status bar shows: "Local" or the server URL.
    connection_label: String,
    /// True when the remote UI is forced over a local backend (debug aid).
    forced_remote: bool,
}

impl Capabilities {
    pub fn from_info(info: &BackendInfo) -> Self {
        let force = std::env::var(FORCE_REMOTE_UI_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
        Self::new(info, force)
    }

    pub fn new(info: &BackendInfo, force_remote_ui: bool) -> Self {
        match &info.kind {
            BackendKind::Local if force_remote_ui => Self {
                shares_filesystem: false,
                connection_label: "Local (remote UI forced)".into(),
                forced_remote: true,
            },
            BackendKind::Local => {
                Self { shares_filesystem: true, connection_label: "Local".into(), forced_remote: false }
            }
            BackendKind::Remote { url } => {
                Self { shares_filesystem: false, connection_label: url.clone(), forced_remote: false }
            }
        }
    }

    /// Native OS folder picker for backend-side folders (vs. the in-app browser).
    pub fn native_folder_picker(&self) -> bool {
        self.shares_filesystem
    }

    /// "Open file" / "Show in folder" on torrent files.
    pub fn open_files_locally(&self) -> bool {
        self.shares_filesystem
    }

    /// "Download to this PC" on torrent files.
    pub fn download_to_this_pc(&self) -> bool {
        !self.shares_filesystem
    }

    pub fn connection_label(&self) -> &str {
        &self.connection_label
    }

    pub fn forced_remote(&self) -> bool {
        self.forced_remote
    }

    /// Connected to an actual server (not the embedded engine, even with forced remote UI).
    pub fn connected_to_server(&self) -> bool {
        !self.shares_filesystem && !self.forced_remote
    }
}

/// The embedded engine. It keeps running while the UI is connected to a server, so
/// "Disconnect" can switch back instantly.
#[derive(Clone)]
pub struct LocalEngine(pub Arc<dyn Backend>);

impl Global for LocalEngine {}

/// The active backend plus its capabilities, available to every view as a global.
/// Swapping backends (e.g. connecting to a server) means replacing this global.
#[derive(Clone)]
pub struct BackendHandle {
    pub backend: Arc<dyn Backend>,
    pub caps: Capabilities,
}

impl Global for BackendHandle {}

impl BackendHandle {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        let caps = Capabilities::from_info(&backend.info());
        Self { backend, caps }
    }

    pub fn get(cx: &App) -> Self {
        cx.global::<Self>().clone()
    }
}

pub type OnPick = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Lets the user choose a folder *on the backend's machine*.
///
/// Local backends get the native OS dialog; remote ones get [`RemoteFolderPicker`],
/// which browses the server through `Backend::list_dirs`. Call sites don't care which.
pub fn pick_backend_folder(
    title: &str,
    initial: Option<String>,
    window: &mut Window,
    cx: &mut App,
    on_pick: impl Fn(String, &mut Window, &mut App) + 'static,
) {
    let handle = BackendHandle::get(cx);
    if handle.caps.native_folder_picker() {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(title.to_owned().into()),
        });
        window
            .spawn(cx, async move |cx| match rx.await {
                Ok(Ok(Some(paths))) => {
                    if let Some(p) = paths.into_iter().next() {
                        let _ = cx.update(|window, cx| on_pick(p.to_string_lossy().into_owned(), window, cx));
                    }
                }
                Ok(Err(e)) => tracing::warn!("folder picker failed: {e:#}"),
                _ => {}
            })
            .detach();
    } else {
        RemoteFolderPicker::open(title, handle.backend, initial, Rc::new(on_pick), window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: BackendKind) -> BackendInfo {
        BackendInfo {
            kind,
            api_version: storm_proto::API_VERSION,
            engine_version: "0".into(),
            librqbit_version: "0".into(),
            self_update: false,
        }
    }

    #[test]
    fn local_uses_native_things() {
        let c = Capabilities::new(&info(BackendKind::Local), false);
        assert!(c.native_folder_picker() && c.open_files_locally());
        assert!(!c.download_to_this_pc());
        assert_eq!(c.connection_label(), "Local");
    }

    #[test]
    fn remote_uses_remote_things() {
        let c = Capabilities::new(&info(BackendKind::Remote { url: "http://nas:9091".into() }), false);
        assert!(!c.native_folder_picker() && !c.open_files_locally());
        assert!(c.download_to_this_pc());
        assert_eq!(c.connection_label(), "http://nas:9091");
    }

    #[test]
    fn forcing_remote_ui_over_local() {
        let c = Capabilities::new(&info(BackendKind::Local), true);
        assert!(c.download_to_this_pc() && !c.native_folder_picker() && c.forced_remote());
        // Forcing has no effect on a real remote.
        let r = Capabilities::new(&info(BackendKind::Remote { url: "u".into() }), true);
        assert!(!r.forced_remote());
    }
}
