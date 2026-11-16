//! Add-torrent, remove-confirmation and settings dialogs.
//!
//! Each dialog is an entity rendered inside a gpui-component `Dialog` and owns
//! its own buttons (a plain `Dialog` renders no footer buttons by itself).

use std::cell::Cell;
use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use storm_proto::{AddTorrentRequest, Settings, SettingsPatch, TorrentId};

use crate::caps::{BackendHandle, pick_backend_folder};
use crate::format;
use crate::ui::{self, Footer, form_row};

use gpui_kit::assets::IconName as LucideIcon;

/// Read-only display of a backend-side folder + "Browse…". There are deliberately
/// no free-text path fields: the picker (native or remote) is the only way in.
/// The whole field is clickable, not just the button.
fn folder_field(
    id: &'static str,
    value: Option<&str>,
    placeholder: &str,
    on_browse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let on_browse = std::rc::Rc::new(on_browse);
    let on_field = on_browse.clone();
    h_flex()
        .gap_2()
        .items_center()
        .child(
            h_flex()
                .id(SharedString::from(format!("{id}-field")))
                .flex_1()
                .min_w_0()
                .h(ui::control_h())
                .px_3()
                .gap_2()
                .items_center()
                .border_1()
                .border_color(theme.input)
                .rounded(theme.radius)
                .bg(theme.input_background())
                .text_sm()
                .cursor_pointer()
                .hover(|s| s.border_color(theme.ring.opacity(0.6)))
                .on_click(move |ev, window, cx| on_field(ev, window, cx))
                .child(Icon::new(IconName::Folder).small().flex_shrink_0().text_color(theme.muted_foreground))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(value.is_none(), |d| d.text_color(theme.muted_foreground))
                        .child(value.unwrap_or(placeholder).to_owned()),
                ),
        )
        .child(Button::new(id).outline().label("Browse…").on_click(move |ev, window, cx| on_browse(ev, window, cx)))
}

/// Icon, title and description shown at the top of a dialog.
#[derive(Clone)]
struct Header {
    icon: Icon,
    title: SharedString,
    description: SharedString,
    danger: bool,
}

impl Header {
    fn new(icon: impl Into<Icon>, title: impl Into<SharedString>, description: impl Into<SharedString>) -> Self {
        Self { icon: icon.into(), title: title.into(), description: description.into(), danger: false }
    }

    fn render(&self, cx: &App) -> AnyElement {
        let accent = self.danger.then(|| cx.theme().danger);
        let description = Some(self.description.clone()).filter(|d| !d.is_empty());
        ui::dialog_header(self.icon.clone(), self.title.clone(), description, accent, cx)
    }
}

// ---------------------------------------------------------------------------
// Add torrent
// ---------------------------------------------------------------------------

pub enum AddSource {
    Links(Entity<InputState>),
    Files(Vec<(String, Vec<u8>)>),
}

pub struct AddTorrentDialog {
    source: AddSource,
    /// `None` = the backend's default download folder.
    save_to: Option<String>,
    default_dir: Option<String>,
    paused: bool,
    sequential: bool,
    busy: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl AddTorrentDialog {
    pub fn open_links(prefill: Option<String>, default_dir: Option<String>, window: &mut Window, cx: &mut App) {
        let dialog = cx.new(|cx| {
            let input = cx.new(|cx| {
                let mut s =
                    InputState::new(window, cx).placeholder("magnet:?xt=urn:btih:…  or  https://…/file.torrent");
                if let Some(p) = &prefill {
                    s.set_value(p.clone(), window, cx);
                }
                s
            });
            let sub = cx.subscribe_in(&input, window, |this: &mut Self, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            });
            Self::new(AddSource::Links(input), default_dir, vec![sub])
        });
        let header = Header::new(LucideIcon::Magnet, "Add from link", "Magnet links or URLs of .torrent files");
        Self::show(dialog, header, window, cx);
    }

    pub fn open_files(files: Vec<(String, Vec<u8>)>, default_dir: Option<String>, window: &mut Window, cx: &mut App) {
        if files.is_empty() {
            return;
        }
        let title = if files.len() == 1 { "Add torrent".to_owned() } else { format!("Add {} torrents", files.len()) };
        let total: usize = files.iter().map(|(_, b)| b.len()).sum();
        let description = if files.len() == 1 {
            short(&files[0].0)
        } else {
            format!("{} .torrent files · {}", files.len(), format::bytes(total as u64))
        };
        let dialog = cx.new(|_| Self::new(AddSource::Files(files), default_dir, Vec::new()));
        let header = Header::new(LucideIcon::FileUp, title, description);
        Self::show(dialog, header, window, cx);
    }

    fn new(source: AddSource, default_dir: Option<String>, subs: Vec<Subscription>) -> Self {
        Self {
            source,
            save_to: None,
            default_dir,
            paused: false,
            sequential: false,
            busy: false,
            error: None,
            _subscriptions: subs,
        }
    }

    fn show(dialog: Entity<Self>, header: Header, window: &mut Window, cx: &mut App) {
        let first = Cell::new(true);
        window.open_dialog(cx, move |d, window, cx| {
            if first.replace(false)
                && let AddSource::Links(input) = &dialog.read(cx).source
            {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            ui::modal(d, header.render(cx), ui::width::MD, cx).child(dialog.clone())
        });
    }

    fn requests(&self, cx: &App) -> Result<Vec<(String, AddTorrentRequest)>, String> {
        let mut reqs: Vec<(String, AddTorrentRequest)> = match &self.source {
            AddSource::Links(input) => {
                let text = input.read(cx).value();
                let links = format::extract_links(&text);
                if links.is_empty() {
                    return Err(if text.trim().is_empty() {
                        "Paste a magnet link or a .torrent URL.".into()
                    } else {
                        "That doesn't look like a magnet link or http(s) URL.".into()
                    });
                }
                links.into_iter().map(|l| (l.clone(), AddTorrentRequest::url(l))).collect()
            }
            AddSource::Files(files) => {
                files.iter().map(|(name, bytes)| (name.clone(), AddTorrentRequest::file(bytes.clone()))).collect()
            }
        };
        for (_, r) in &mut reqs {
            r.output_folder = self.save_to.clone();
            r.paused = self.paused;
            r.sequential = self.sequential;
        }
        Ok(reqs)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let reqs = match self.requests(cx) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let backend = BackendHandle::get(cx).backend;
        cx.spawn_in(window, async move |this, cx| {
            let mut failures = Vec::new();
            let mut existing = 0;
            for (label, req) in reqs {
                match backend.add_torrent(req).await {
                    Ok(r) if r.already_existed => existing += 1,
                    Ok(_) => {}
                    Err(e) => failures.push(format!("{}: {e}", short(&label))),
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if failures.is_empty() {
                    window.close_dialog(cx);
                    if existing > 0 {
                        ui::notify_info(window, cx, format!("{existing} torrent(s) were already in the list."));
                    }
                } else {
                    this.error = Some(failures.join("\n"));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

fn short(s: &str) -> String {
    if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s.to_owned() }
}

impl Render for AddTorrentDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let source: AnyElement = match &self.source {
            AddSource::Links(input) => form_row(
                "Links",
                Input::new(input).prefix(Icon::new(LucideIcon::Link).small().text_color(theme.muted_foreground)),
                Some("Paste one or more, separated by spaces or new lines.".into()),
                cx,
            )
            .into_any_element(),
            AddSource::Files(files) => form_row(
                if files.len() == 1 { "File" } else { "Files" },
                v_flex()
                    .id("files")
                    .max_h(px(5. * 34.))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .text_sm()
                    .children(files.iter().enumerate().map(|(ix, (name, bytes))| {
                        h_flex()
                            .h(px(34.))
                            .flex_shrink_0()
                            .px_3()
                            .gap_2()
                            .items_center()
                            .when(ix > 0, |r| r.border_t_1().border_color(theme.border))
                            .child(Icon::new(IconName::File).small().flex_shrink_0().text_color(theme.muted_foreground))
                            .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format::bytes(bytes.len() as u64)),
                            )
                    })),
                None,
                cx,
            )
            .into_any_element(),
        };
        let current = self.save_to.clone().or_else(|| self.default_dir.clone());
        let this = cx.entity().downgrade();
        let browse = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let this = this.clone();
            let initial = this.upgrade().and_then(|e| {
                let d = e.read(cx);
                d.save_to.clone().or_else(|| d.default_dir.clone())
            });
            pick_backend_folder("Save torrent to", initial, window, cx, move |path, _, cx| {
                let _ = this.update(cx, |d, cx| {
                    d.save_to = Some(path);
                    cx.notify();
                });
            });
        };
        let paused = Checkbox::new("paused").label("Start paused").checked(self.paused).on_click(cx.listener(
            |this, v: &bool, _, cx| {
                this.paused = *v;
                cx.notify();
            },
        ));
        let sequential = Checkbox::new("sequential").label("Download sequentially").checked(self.sequential).on_click(
            cx.listener(|this, v: &bool, _, cx| {
                this.sequential = *v;
                cx.notify();
            }),
        );
        v_flex()
            .pt_2()
            .gap(px(ui::space::ROW))
            .child(source)
            .child(form_row(
                "Save to",
                folder_field("browse-save", current.as_deref(), "Default download folder", browse, cx),
                None,
                cx,
            ))
            .children(ui::error_banner(self.error.as_deref(), cx))
            .child(
                Footer::new("Add")
                    .busy(self.busy)
                    .leading(h_flex().gap_4().child(paused).child(sequential))
                    .render(cx.listener(|this, _, w, cx| this.submit(w, cx))),
            )
    }
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

pub struct RemoveDialog {
    ids: Vec<TorrentId>,
    names: Vec<String>,
    delete_files: bool,
    busy: bool,
}

impl RemoveDialog {
    pub fn open(ids: Vec<TorrentId>, names: Vec<String>, delete_files: bool, window: &mut Window, cx: &mut App) {
        if ids.is_empty() {
            return;
        }
        let dialog = cx.new(|_| Self { ids, names: names.clone(), delete_files, busy: false });
        let (title, description) = match names.as_slice() {
            [one] => ("Remove torrent?".to_owned(), short(one)),
            many => (format!("Remove {} torrents?", many.len()), "They'll be removed from the list.".to_owned()),
        };
        let header = Header { danger: true, ..Header::new(LucideIcon::Trash, title, description) };
        window
            .open_dialog(cx, move |d, _, cx| ui::modal(d, header.render(cx), ui::width::SM, cx).child(dialog.clone()));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        cx.notify();
        let backend = BackendHandle::get(cx).backend;
        let (ids, delete) = (self.ids.clone(), self.delete_files);
        cx.spawn_in(window, async move |_, cx| {
            let mut failures = Vec::new();
            for id in ids {
                if let Err(e) = backend.remove(id, delete).await {
                    failures.push(e.to_string());
                }
            }
            let _ = cx.update(|window, cx| {
                window.close_dialog(cx);
                if !failures.is_empty() {
                    ui::notify_error(window, cx, format!("Removing failed: {}", failures.join("; ")));
                }
            });
        })
        .detach();
    }
}

impl Render for RemoveDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        const SHOWN: usize = 4;
        let list = (self.names.len() > 1).then(|| {
            let more = self.names.len().saturating_sub(SHOWN);
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .rounded(theme.radius)
                .bg(theme.muted)
                .text_sm()
                .children(self.names.iter().take(SHOWN).map(|n| div().truncate().child(n.clone())))
                .when(more > 0, |l| {
                    l.child(div().text_color(theme.muted_foreground).child(format!("and {more} more…")))
                })
        });
        let warning = self.delete_files.then(|| {
            h_flex()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(theme.danger)
                .child(Icon::new(IconName::TriangleAlert).xsmall().flex_shrink_0())
                .child("Downloaded files will be permanently deleted from disk.")
        });
        v_flex()
            .pt_2()
            .gap(px(ui::space::ROW))
            .children(list)
            .child(
                v_flex()
                    .gap(px(ui::space::TIGHT))
                    .child(
                        Checkbox::new("delete-files")
                            .label("Also delete downloaded files")
                            .checked(self.delete_files)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.delete_files = *v;
                                cx.notify();
                            })),
                    )
                    .children(warning),
            )
            .child(
                Footer::new(if self.delete_files { "Remove and delete" } else { "Remove" })
                    .danger(true)
                    .busy(self.busy)
                    .render(cx.listener(|this, _, w, cx| this.submit(w, cx))),
            )
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub struct SettingsDialog {
    original: Settings,
    download_dir: String,
    port: Entity<InputState>,
    upnp: bool,
    down: Entity<InputState>,
    up: Entity<InputState>,
    /// Client-side pref (not part of [`SettingsPatch`]); saved as soon as it's toggled.
    watch_clipboard: bool,
    /// Client-side pref, saved as soon as it's toggled.
    auto_update: bool,
    error: Option<String>,
    busy: bool,
    /// Re-renders when the updater's status changes.
    _updater: Option<Subscription>,
}

impl SettingsDialog {
    pub fn open(settings: Settings, window: &mut Window, cx: &mut App) {
        let dialog = cx.new(|cx| {
            let input = |value: String, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut s = InputState::new(window, cx).placeholder(placeholder);
                    s.set_value(value, window, cx);
                    s
                })
            };
            Self {
                download_dir: settings.download_dir.clone(),
                port: input(settings.listen_port.to_string(), "0 = random", window, cx),
                upnp: settings.enable_upnp,
                down: input(format::limit_to_kib(settings.down_limit_bps), "Unlimited", window, cx),
                up: input(format::limit_to_kib(settings.up_limit_bps), "Unlimited", window, cx),
                watch_clipboard: crate::prefs::watch_clipboard(cx),
                auto_update: crate::prefs::auto_update(cx),
                _updater: crate::updater::UpdaterHandle::get(cx).map(|u| cx.observe(&u, |_, _, cx| cx.notify())),
                original: settings,
                error: None,
                busy: false,
            }
        });
        let header = Header::new(LucideIcon::SlidersHorizontal, "Settings", "");
        window
            .open_dialog(cx, move |d, _, cx| ui::modal(d, header.render(cx), ui::width::MD, cx).child(dialog.clone()));
    }

    fn patch(&self, cx: &App) -> Result<SettingsPatch, String> {
        let o = &self.original;
        let port = format::parse_port(&self.port.read(cx).value()).map_err(|e| format!("Listen port: {e}"))?;
        let down = format::parse_limit_kib(&self.down.read(cx).value()).map_err(|e| format!("Download limit: {e}"))?;
        let up = format::parse_limit_kib(&self.up.read(cx).value()).map_err(|e| format!("Upload limit: {e}"))?;
        Ok(SettingsPatch {
            download_dir: (self.download_dir != o.download_dir).then(|| self.download_dir.clone()),
            listen_port: (port != o.listen_port).then_some(port),
            enable_upnp: (self.upnp != o.enable_upnp).then_some(self.upnp),
            down_limit_bps: (down != o.down_limit_bps).then_some(down),
            up_limit_bps: (up != o.up_limit_bps).then_some(up),
        })
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let patch = match self.patch(cx) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let needs_restart = patch.listen_port.is_some() || patch.enable_upnp.is_some();
        self.busy = true;
        self.error = None;
        cx.notify();
        let backend = BackendHandle::get(cx).backend;
        cx.spawn_in(window, async move |this, cx| {
            let result = backend.update_settings(patch).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        window.close_dialog(cx);
                        if needs_restart {
                            ui::notify_info(window, cx, "Port and UPnP changes take effect after a restart.");
                        }
                    }
                    Err(e) => {
                        this.error = Some(e.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

impl SettingsDialog {
    /// Whether Storm opens magnet links and .torrent files, with a button to make it so.
    fn render_default_app(&self, handles: crate::default_app::Handles, cx: &mut Context<Self>) -> AnyElement {
        let both = handles.magnet && handles.torrent;
        let label =
            if both { "Default for magnet links and .torrent files" } else { "Magnet links and .torrent files" };
        let set_default = Button::new("set-default").small().outline().label("Set as default").on_click(cx.listener(
            |_, _: &ClickEvent, window, cx| {
                if let Err(e) = crate::default_app::register() {
                    ui::notify_error(window, cx, format!("Couldn't register Storm: {e}"));
                    return;
                }
                if crate::default_app::status().is_some_and(|h| h.magnet && h.torrent) {
                    ui::notify_info(window, cx, "Storm is now the default.");
                } else {
                    // Windows only lets the user switch from an app they picked.
                    cx.open_url(crate::default_app::SETTINGS_URL);
                }
                cx.notify();
            },
        ));
        h_flex()
            .mt(px(ui::space::TIGHT))
            .h(ui::control_h())
            .items_center()
            .justify_between()
            .child(div().text_sm().child(label))
            .when(!both, |row| row.child(set_default))
            .into_any_element()
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::updater::{Status, UpdaterHandle, current_version};
        use gpui_kit::component::Disableable as _;
        use gpui_kit::component::button::ButtonVariants as _;
        let muted = cx.theme().muted_foreground;
        let updater = UpdaterHandle::get(cx);
        let (status, can_check) = updater
            .as_ref()
            .map(|u| (u.read(cx).status().clone(), u.read(cx).can_check()))
            .unwrap_or((Status::Idle, false));
        let supported = !matches!(status, Status::Unsupported(_));
        let ready = matches!(status, Status::Ready(_));
        let described = status.describe();
        let line = if described.is_empty() {
            format!("Storm {}", current_version())
        } else {
            format!("Storm {} · {described}", current_version())
        };
        let action = if ready {
            Button::new("update-restart").small().primary().label("Restart now").on_click(
                |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    if let Some(u) = crate::updater::UpdaterHandle::get(cx) {
                        u.update(cx, |u, cx| u.restart(cx));
                    }
                },
            )
        } else {
            Button::new("update-check").small().outline().label("Check now").disabled(!can_check).on_click({
                let updater = updater.clone();
                move |_, window, cx| {
                    if let Some(u) = &updater {
                        u.update(cx, |u, cx| u.check(true, window, cx));
                    }
                }
            })
        };
        v_flex()
            .gap(px(ui::space::TIGHT))
            .child(
                h_flex()
                    .h(ui::control_h())
                    .items_center()
                    .justify_between()
                    .child(
                        Switch::new("auto-update")
                            .label("Install updates automatically")
                            .checked(self.auto_update)
                            .disabled(!supported)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.auto_update = *v;
                                crate::prefs::set_auto_update(cx, *v);
                                cx.notify();
                            })),
                    )
                    .when(supported, |row| row.child(action)),
            )
            .child(div().text_xs().text_color(muted).child(line))
            .into_any_element()
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let browse = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let this = this.clone();
            let initial = this.upgrade().map(|e| e.read(cx).download_dir.clone());
            pick_backend_folder("Default download folder", initial, window, cx, move |path, _, cx| {
                let _ = this.update(cx, |d, cx| {
                    d.download_dir = path;
                    cx.notify();
                });
            });
        };
        let muted = cx.theme().muted_foreground;
        let limit =
            |state: &Entity<InputState>| Input::new(state).suffix(div().text_xs().text_color(muted).child("KiB/s"));
        let section = |title: &'static str, body: AnyElement, cx: &App| {
            v_flex().gap(px(10.)).child(ui::section_label(title, cx)).child(body)
        };
        let upnp = h_flex().h(ui::control_h()).items_center().child(
            Switch::new("upnp").label("UPnP port forwarding").checked(self.upnp).on_click(cx.listener(
                |this, v: &bool, _, cx| {
                    this.upnp = *v;
                    cx.notify();
                },
            )),
        );
        v_flex()
            .pt_2()
            .gap(px(22.))
            .child(section(
                "DOWNLOADS",
                form_row(
                    "Default folder",
                    folder_field("browse-dir", Some(&self.download_dir), "", browse, cx),
                    None,
                    cx,
                )
                .into_any_element(),
                cx,
            ))
            .child(section(
                "NETWORK",
                v_flex()
                    .gap(px(ui::space::TIGHT))
                    .child(
                        h_flex()
                            .gap_4()
                            .items_end()
                            .child(div().flex_1().child(form_row("Listen port", Input::new(&self.port), None, cx)))
                            .child(div().flex_1().child(upnp)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Port and UPnP changes apply after a restart. 0 picks a random port."),
                    )
                    .into_any_element(),
                cx,
            ))
            .child(section(
                "BANDWIDTH",
                v_flex()
                    .gap(px(ui::space::TIGHT))
                    .child(
                        h_flex()
                            .gap_4()
                            .child(div().flex_1().child(form_row("Download limit", limit(&self.down), None, cx)))
                            .child(div().flex_1().child(form_row("Upload limit", limit(&self.up), None, cx))),
                    )
                    .into_any_element(),
                cx,
            ))
            .child(section(
                "APP",
                v_flex()
                    .gap(px(ui::space::TIGHT))
                    .child(
                        Switch::new("watch-clipboard")
                            .label("Watch clipboard for magnet links")
                            .checked(self.watch_clipboard)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.watch_clipboard = *v;
                                crate::prefs::set_watch_clipboard(cx, *v);
                                cx.notify();
                            })),
                    )
                    .children(crate::default_app::status().map(|h| self.render_default_app(h, cx)))
                    .into_any_element(),
                cx,
            ))
            .child(section("UPDATES", self.render_updates(cx), cx))
            .children(ui::error_banner(self.error.as_deref(), cx))
            .child(Footer::new("Save").busy(self.busy).render(cx.listener(|this, _, w, cx| this.submit(w, cx))))
    }
}

// ---------------------------------------------------------------------------
// .torrent files from this machine (picker or drag & drop). Works for remote
// backends too: the bytes are uploaded.
// ---------------------------------------------------------------------------

pub fn read_torrent_files(paths: impl IntoIterator<Item = PathBuf>) -> (Vec<(String, Vec<u8>)>, Vec<String>) {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string());
        if p.is_dir() {
            continue;
        }
        match std::fs::read(&p) {
            Ok(bytes) => ok.push((name, bytes)),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    (ok, errors)
}
