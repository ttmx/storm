//! Storm desktop app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// gpui's derive macros expand to `gpui::...` paths; gpui-kit re-exports gpui and
// documents aliasing itself as `gpui` for exactly this.
extern crate gpui_kit as gpui;

mod app;
mod backdrop;
mod caps;
mod default_app;
mod dev;
mod format;
mod prefs;
mod remotes;
mod single_instance;
mod state;
mod theme;
mod ui;
mod updater;
mod views;

use std::borrow::Cow;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, TitleBar, v_flex};
use storm_engine::{EngineConfig, LocalBackend};

use crate::caps::{BackendHandle, FORCE_REMOTE_UI_ENV, LocalEngine};
use crate::single_instance::Launch;

/// Overrides where the engine keeps its database and session state.
const DATA_DIR_ENV: &str = "STORM_DATA_DIR";

/// gpui-component's icons plus the extra Lucide icons in [`StormIcons`].
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = StormIcons.load(path)? {
            return Ok(Some(svg));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

// Only the Lucide icons we use beyond gpui-component's own set (`Assets`);
// embedding the whole catalogue (`AllAssets`, ~1800 icons) bloats the binary.
gpui_kit::assets::icon_assets!(
    StormIcons,
    [
        CornerLeftUp,
        FileUp,
        FolderOpen,
        FolderPlus,
        Globe,
        KeyRound,
        Link,
        Magnet,
        RefreshCw,
        Server,
        SlidersHorizontal,
        Trash,
        Unplug,
    ]
);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,librqbit=warn,librqbit_dht=warn,tracker_comms=warn".into()),
        )
        .init();

    let config = EngineConfig::platform_default().map(|mut config| {
        // Dev/testing aid: run a second, isolated instance next to the real one.
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
            config.data_dir = dir.into();
        }
        config
    });
    if let Ok(c) = &config {
        log_panics_to(c.data_dir.join("crash.log"));
    }

    // `storm <magnet|url|file.torrent>…`: open them in the Add dialog, here or,
    // if Storm is already running on this data folder, in that copy.
    let open_request = single_instance::OpenRequest::from_args(std::env::args().skip(1));
    let mut instance = match &config {
        Ok(c) => match single_instance::acquire(&c.data_dir, &open_request) {
            Launch::Primary(instance) => Some(instance),
            Launch::Forwarded | Launch::Unreachable => return,
        },
        Err(_) => None,
    };
    let forwarded = instance.as_mut().and_then(|i| i.take_requests());
    default_app::refresh_if_stale();

    // Start the engine before the UI: it blocks briefly while the session comes up.
    let engine = config.and_then(LocalBackend::start_with_own_runtime);

    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        theme::install(cx);
        app::bind_keys(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        match engine {
            Ok(local) => {
                let handle = BackendHandle::new(Arc::new(local.clone()));
                tracing::info!(
                    connection = handle.caps.connection_label(),
                    "engine started{}",
                    if handle.caps.forced_remote() {
                        format!(" ({FORCE_REMOTE_UI_ENV} set: using remote UI)")
                    } else {
                        String::new()
                    }
                );
                cx.set_global(handle);
                cx.set_global(LocalEngine(Arc::new(local.clone())));
                // gpui gives quit hooks ~200ms, too little for the session to stop, so
                // the hook only *starts* the shutdown; `main` waits for it below.
                cx.on_app_quit(move |_| {
                    start_engine_shutdown(local.clone());
                    async {}
                })
                .detach();
                open_main_window(open_request, forwarded, cx);
            }
            Err(e) => {
                tracing::error!("failed to start the engine: {e:#}");
                open_error_window(format!("{e:#}"), cx);
            }
        }
        cx.activate(true);
    });

    // On Windows/Linux `run` returns once the event loop ends (macOS exits inside it).
    if let Some(done) = ENGINE_STOPPED.get() {
        let ok = done.lock().map(|rx| rx.recv_timeout(Duration::from_secs(5)).is_ok()).unwrap_or(false);
        if !ok {
            tracing::warn!("engine didn't stop within 5s; exiting anyway");
        }
    }
    // Only now may another copy start on this data folder.
    drop(instance);
}

static ENGINE_STOPPED: OnceLock<Mutex<mpsc::Receiver<()>>> = OnceLock::new();

/// Stops the engine on a plain thread (its futures run on the engine's own runtime).
fn start_engine_shutdown(local: LocalBackend) {
    let (tx, rx) = mpsc::channel();
    if ENGINE_STOPPED.set(Mutex::new(rx)).is_err() {
        return; // Already stopping.
    }
    std::thread::spawn(move || {
        futures::executor::block_on(local.shutdown());
        tracing::info!("engine stopped");
        let _ = tx.send(());
    });
}

fn window_options(cx: &App, w: f32, h: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(w), px(h)), cx))),
        window_min_size: Some(size(px(640.), px(400.))),
        ..TitleBar::window_options()
    }
}

fn open_main_window(
    request: single_instance::OpenRequest,
    forwarded: Option<futures::channel::mpsc::UnboundedReceiver<single_instance::OpenRequest>>,
    cx: &mut App,
) {
    let opts = window_options(cx, 1280., 800.);
    match gpui_kit::open_window(opts, cx, |window, cx| {
        window.set_window_title("Storm");
        theme::follow_system_appearance(window);
        cx.new(|cx| app::StormApp::new(window, cx))
    }) {
        Ok((window, _)) => {
            dev::open_requested_dialog(window, cx);
            let _ = window.update(cx, |_, window, cx| single_instance::open(request, window, cx));
            if let Some(rx) = forwarded {
                single_instance::serve_requests(rx, window, cx);
            }
        }
        Err(e) => {
            tracing::error!("failed to open main window: {e:#}");
            cx.quit();
        }
    }
}

struct ErrorView {
    message: SharedString,
}

impl Render for ErrorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().child(div().text_sm().child("Storm")))
            .child(
                v_flex()
                    .flex_1()
                    .p_6()
                    .gap_4()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Storm couldn't start its torrent engine"),
                    )
                    .child(div().text_sm().text_color(theme.danger).child(self.message.clone()))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Another copy of Storm may already be running, or the data folder isn't writable."),
                    )
                    .child(Button::new("quit").primary().label("Quit").on_click(|_, _, cx| cx.quit())),
            )
    }
}

fn open_error_window(message: String, cx: &mut App) {
    let opts = window_options(cx, 640., 300.);
    let _ = gpui_kit::open_window(opts, cx, |window, cx| {
        window.set_window_title("Storm: error");
        theme::follow_system_appearance(window);
        cx.new(|_| ErrorView { message: message.into() })
    });
}

/// Release builds abort on panic and have no console, so the default "print to
/// stderr" would lose the message. Append it to `crash.log` as well.
fn log_panics_to(path: std::path::PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write as _;
        let thread = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
        let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[{when}] storm {} panicked on thread '{thread}': {info}", env!("CARGO_PKG_VERSION"));
        }
        default(info);
    }));
}
