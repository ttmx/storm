//! Self-update from GitHub Releases (ttmx/storm): the UI side. The mechanics
//! (fetch, verify, swap the exe) live in `storm-update`.
//!
//! Release builds check shortly after startup and then every few hours. A newer
//! release is downloaded, verified and swapped in place of the running exe
//! (Windows allows renaming a running exe; the old one is deleted on the next
//! start). Then a toast offers to restart into it; "Later" just means the next
//! launch runs the new version.
//!
//! Debug builds never update themselves unless `STORM_UPDATE_DEBUG=1`.

use std::path::PathBuf;
use std::time::Duration;

use gpui::*;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};

use crate::prefs;
use crate::views::clipboard_watch::push_bottom_right;

/// The release asset this build updates from. Must match the release workflow.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ASSET: Option<&str> = Some("storm-windows-x86_64.exe");
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
const ASSET: Option<&str> = None;

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// What the Settings dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// This build doesn't update itself (debug build or unsupported platform).
    Unsupported(&'static str),
    Idle,
    Checking,
    UpToDate,
    /// Downloading and installing `version`.
    Updating(String),
    /// `version` is installed and runs after a restart.
    Ready(String),
    Failed(String),
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Status::Unsupported(why) => (*why).to_owned(),
            Status::Idle => String::new(),
            Status::Checking => "Checking for updates…".into(),
            Status::UpToDate => "Storm is up to date".into(),
            Status::Updating(v) => format!("Downloading Storm {v}…"),
            Status::Ready(v) => format!("Storm {v} is installed; restart to use it"),
            Status::Failed(e) => format!("Update failed: {e}"),
        }
    }
}

pub struct Updater {
    enabled: bool,
    status: Status,
    restart_path: Option<PathBuf>,
    busy: bool,
    _loop: Option<Task<()>>,
}

/// Lets the Settings dialog reach the updater.
#[derive(Clone)]
pub struct UpdaterHandle(pub Entity<Updater>);

impl Global for UpdaterHandle {}

impl UpdaterHandle {
    pub fn get(cx: &App) -> Option<Entity<Updater>> {
        cx.try_global::<UpdaterHandle>().map(|h| h.0.clone())
    }
}

fn enabled_for_this_build() -> Result<(), &'static str> {
    if ASSET.is_none() {
        return Err("Automatic updates aren't available on this platform yet");
    }
    if cfg!(debug_assertions) && !std::env::var("STORM_UPDATE_DEBUG").is_ok_and(|v| v == "1") {
        return Err("Development build: updates are disabled");
    }
    Ok(())
}

impl Updater {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self { enabled: false, status: Status::Idle, restart_path: None, busy: false, _loop: None };
        if let Err(why) = enabled_for_this_build() {
            this.status = Status::Unsupported(why);
            return this;
        }
        // Delete the previous exe an earlier update left behind.
        cx.background_executor()
            .spawn(async move {
                if let Ok(exe) = std::env::current_exe() {
                    storm_update::remove_leftovers(&exe);
                }
            })
            .detach();
        this.enabled = true;
        this._loop = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_DELAY).await;
            loop {
                let alive = this.update_in(cx, |this, window, cx| {
                    if prefs::auto_update(cx) {
                        this.check(false, window, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(CHECK_INTERVAL).await;
            }
        }));
        this
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn can_check(&self) -> bool {
        self.enabled && !self.busy && !matches!(self.status, Status::Ready(_))
    }

    /// Checks now; with `user_initiated`, failures are also reported as a toast.
    pub fn check(&mut self, user_initiated: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(asset) = ASSET else { return };
        if !self.enabled || self.busy || matches!(self.status, Status::Ready(_)) {
            return;
        }
        self.busy = true;
        self.set_status(Status::Checking, cx);
        cx.spawn_in(window, async move |this, cx| {
            let found = cx.background_executor().spawn(async move { storm_update::check(current_version(), asset) });
            let release = match found.await {
                Ok(Some(release)) => release,
                Ok(None) => {
                    let _ = this.update(cx, |this, cx| this.finish(Status::UpToDate, cx));
                    return;
                }
                Err(e) => {
                    let _ = this.update_in(cx, |this, window, cx| this.fail(e.to_string(), user_initiated, window, cx));
                    return;
                }
            };
            let version = release.version.clone();
            let _ = this.update(cx, |this, cx| this.set_status(Status::Updating(version.clone()), cx));
            let installed = cx
                .background_executor()
                .spawn(async move {
                    let exe = std::env::current_exe()?;
                    let bytes = storm_update::download(&release)?;
                    storm_update::install(&exe, &bytes)?;
                    Ok::<_, storm_update::Error>(exe)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match installed {
                Ok(exe) => {
                    this.restart_path = Some(exe);
                    this.finish(Status::Ready(version.clone()), cx);
                    show_ready_toast(&version, window, cx);
                }
                Err(e) => this.fail(e.to_string(), user_initiated, window, cx),
            });
        })
        .detach();
    }

    fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn finish(&mut self, status: Status, cx: &mut Context<Self>) {
        self.busy = false;
        self.set_status(status, cx);
    }

    fn fail(&mut self, error: String, user_initiated: bool, window: &mut Window, cx: &mut Context<Self>) {
        tracing::warn!("update failed: {error}");
        if user_initiated {
            push_bottom_right(Notification::error(format!("Couldn't update: {error}")), window, cx);
        }
        self.finish(Status::Failed(error), cx);
    }

    /// Relaunches into the installed update.
    pub fn restart(&mut self, cx: &mut App) {
        if let Some(path) = self.restart_path.clone() {
            cx.set_restart_path(path);
        }
        cx.restart();
    }
}

/// Update toast id.
struct UpdateReady;

fn show_ready_toast(version: &str, window: &mut Window, cx: &mut App) {
    let version = version.to_owned();
    let note =
        Notification::new().id::<UpdateReady>().autohide(false).content(move |_, _, cx| render_ready(&version, cx));
    push_bottom_right(note, window, cx);
}

fn render_ready(version: &str, cx: &mut Context<Notification>) -> AnyElement {
    let theme = cx.theme();
    let restart = cx.listener(|_: &mut Notification, _: &ClickEvent, _, cx| {
        if let Some(updater) = UpdaterHandle::get(cx) {
            updater.update(cx, |u, cx| u.restart(cx));
        }
    });
    let later = cx.listener(|note: &mut Notification, _: &ClickEvent, window, cx| note.dismiss(window, cx));
    h_flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .mt(px(2.))
                .size(px(28.))
                .flex_shrink_0()
                .rounded(theme.radius)
                .bg(theme.success.opacity(0.14))
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(Lucide::RefreshCw).small().text_color(theme.success)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div().pr_5().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!("Storm {version} is ready")),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child("Restart to finish updating."))
                .child(
                    h_flex()
                        .mt_2()
                        .gap_2()
                        .justify_end()
                        .child(Button::new("update-later").small().ghost().label("Later").on_click(later))
                        .child(Button::new("update-restart").small().primary().label("Restart now").on_click(restart)),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::current_version;

    #[test]
    fn version_is_plain_semver() {
        let parts: Vec<_> = current_version().split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.parse::<u64>().is_ok()));
    }
}
