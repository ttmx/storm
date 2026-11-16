//! The main window's root view: toolbar, torrent table, details pane, status bar.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::{ColumnSort, DataTable, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, IconName, Sizable as _, Size, TitleBar,
    WindowExt as _, h_flex, resizable_panel, v_flex, v_resizable,
};
use storm_proto::{TorrentId, TorrentState};

use crate::caps::{BackendHandle, LocalEngine};
use crate::format;
use crate::prefs::{self, Prefs};
use crate::state::{AppState, Filter};
use crate::ui::{self, spawn_op};
use crate::views::connect::{ConnectDialog, OnConnected};
use crate::views::details::DetailsPane;
use crate::views::dialogs::{AddTorrentDialog, RemoveDialog, SettingsDialog, read_torrent_files};
use crate::views::status_bar::StatusBar;
use crate::views::torrent_table::{Col, Move, TorrentTableDelegate, display_name};

gpui_kit::actions!(
    storm,
    [
        AddLink,
        AddFile,
        PasteLink,
        PauseSelected,
        ResumeSelected,
        RemoveSelected,
        RemoveSelectedWithData,
        OpenSettings,
        SelectAll,
        OpenFolder,
        CopyMagnet,
        CopyInfoHash,
        FocusSearch,
        // Keyboard navigation in the torrent list.
        SelectPrev,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        ExtendPrev,
        ExtendNext,
        ExtendPageUp,
        ExtendPageDown,
        ExtendFirst,
        ExtendLast,
        ClearSelection,
        OpenSelected,
        TogglePause,
        // The torrent list's header menu.
        ResetColumns,
    ]
);

/// Switches the selected torrents to sequential (`true`) or rarest-first picking.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = storm, no_json)]
pub struct SetSequential(pub bool);

/// Shows or hides a column of the torrent list (from its header menu).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = storm, no_json)]
pub struct ToggleColumn(pub Col);

const CONTEXT: &str = "StormApp";

pub fn bind_keys(cx: &mut App) {
    // gpui has no "secondary" modifier in keystroke strings: cmd on macOS, ctrl elsewhere.
    #[cfg(target_os = "macos")]
    let keys = ["cmd-n", "cmd-o", "cmd-v", "cmd-a", "cmd-,", "cmd-f"];
    #[cfg(not(target_os = "macos"))]
    let keys = ["ctrl-n", "ctrl-o", "ctrl-v", "ctrl-a", "ctrl-,", "ctrl-f"];
    cx.bind_keys([
        KeyBinding::new(keys[0], AddLink, Some(CONTEXT)),
        KeyBinding::new(keys[1], AddFile, Some(CONTEXT)),
        KeyBinding::new(keys[2], PasteLink, Some(CONTEXT)),
        KeyBinding::new(keys[3], SelectAll, Some(CONTEXT)),
        KeyBinding::new(keys[4], OpenSettings, Some(CONTEXT)),
        KeyBinding::new(keys[5], FocusSearch, Some(CONTEXT)),
        KeyBinding::new("delete", RemoveSelected, Some(CONTEXT)),
    ]);
    // List navigation applies while the list has focus. Bound on the table's own
    // context so these win over its built-in (single-selection) bindings.
    let list = Some(LIST_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, list),
        KeyBinding::new("down", SelectNext, list),
        KeyBinding::new("pageup", SelectPageUp, list),
        KeyBinding::new("pagedown", SelectPageDown, list),
        KeyBinding::new("home", SelectFirst, list),
        KeyBinding::new("end", SelectLast, list),
        KeyBinding::new("shift-up", ExtendPrev, list),
        KeyBinding::new("shift-down", ExtendNext, list),
        KeyBinding::new("shift-pageup", ExtendPageUp, list),
        KeyBinding::new("shift-pagedown", ExtendPageDown, list),
        KeyBinding::new("shift-home", ExtendFirst, list),
        KeyBinding::new("shift-end", ExtendLast, list),
        KeyBinding::new("escape", ClearSelection, list),
        KeyBinding::new("enter", OpenSelected, list),
        KeyBinding::new("space", TogglePause, list),
    ]);
}

/// The torrent list (gpui-component's `DataTable`) inside the main window.
const LIST_CONTEXT: &str = "StormApp > DataTable";

struct Parts {
    state: Entity<AppState>,
    table: Entity<TableState<TorrentTableDelegate>>,
    details: Entity<DetailsPane>,
    status: Entity<StatusBar>,
    subs: Vec<Subscription>,
}

pub struct StormApp {
    state: Entity<AppState>,
    table: Entity<TableState<TorrentTableDelegate>>,
    details: Entity<DetailsPane>,
    status: Entity<StatusBar>,
    focus_handle: FocusHandle,
    /// Blurred snapshot of the window drawn under open dialogs (see `backdrop`),
    /// plus a counter so each opening gets a fresh fade-in.
    backdrop: Option<std::sync::Arc<RenderImage>>,
    backdrop_seq: usize,
    frames: usize,
    /// The search field; its text filters the table (kept across backend switches).
    search: Entity<InputState>,
    filter: Filter,
    _subscriptions: Vec<Subscription>,
    _search_sub: Subscription,
    _clipboard_watch: Entity<crate::views::clipboard_watch::ClipboardWatcher>,
    _updater: Entity<crate::updater::Updater>,
}

impl StormApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search torrents"));
        let (filter, sort) = {
            let p = Prefs::get(cx);
            (p.filter(), p.table_sort())
        };
        let parts = Self::build(&focus_handle, filter, "", sort, window, cx);
        parts.table.read(cx).focus_handle(cx).focus(window, cx);
        let _search_sub =
            cx.subscribe_in(&search, window, |this: &mut Self, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let query = input.read(cx).value();
                    this.table.update(cx, |t, cx| {
                        t.delegate_mut().set_query(&query);
                        cx.notify();
                    });
                    this.sync_selection(cx);
                    cx.notify();
                }
                // Enter hands the keyboard back to the list (and its shortcuts).
                InputEvent::PressEnter { .. } => this.focus_list(window, cx),
                _ => {}
            });
        Self {
            state: parts.state,
            table: parts.table,
            details: parts.details,
            status: parts.status,
            focus_handle,
            backdrop: None,
            backdrop_seq: 0,
            frames: 0,
            search,
            filter,
            _subscriptions: parts.subs,
            _search_sub,
            _clipboard_watch: cx.new(|cx| crate::views::clipboard_watch::ClipboardWatcher::new(window, cx)),
            _updater: {
                let updater = cx.new(|cx| crate::updater::Updater::new(window, cx));
                cx.set_global(crate::updater::UpdaterHandle(updater.clone()));
                updater
            },
        }
    }

    /// Creates the state and views for whatever backend `BackendHandle` currently holds.
    fn build(
        focus: &FocusHandle,
        filter: Filter,
        query: &str,
        sort: Option<(Col, ColumnSort)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Parts {
        let handle = BackendHandle::get(cx);
        let backend = handle.backend;
        let hidden = Prefs::get(cx).hidden_columns();
        let state = cx.new(|cx| AppState::new(backend, cx));
        let table = cx.new(|cx| {
            let mut delegate = TorrentTableDelegate::new();
            delegate.action_context = Some(focus.clone());
            delegate.can_open_folder = handle.caps.open_files_locally();
            delegate.set_filter(filter);
            delegate.set_query(query);
            delegate.set_sort(sort);
            // After the sort, so a sort on a hidden column is dropped.
            delegate.set_hidden_columns(&hidden);
            TableState::new(delegate, window, cx)
                // We keep our own multi-selection in the delegate.
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
        });
        let details = cx.new(|cx| DetailsPane::new(state.clone(), cx));
        let status = cx.new(|cx| StatusBar::new(state.clone(), cx));

        let subs = vec![
            cx.observe(&state, |this, state, cx| {
                let torrents = state.read(cx).torrents.clone();
                this.table.update(cx, |t, cx| {
                    t.delegate_mut().set_rows(torrents);
                    cx.notify();
                });
                this.sync_selection(cx);
                cx.notify();
            }),
            cx.subscribe_in(&table, window, |this, table, ev: &TableEvent, window, cx| match ev {
                TableEvent::SelectRow(_) | TableEvent::ClearSelection => {
                    this.sync_selection(cx);
                    cx.notify();
                }
                // The click already selected just that row.
                TableEvent::DoubleClickedRow(_) => this.open_folder(&OpenFolder, window, cx),
                // A column edge was dragged: Name gives or takes the difference.
                TableEvent::ColumnWidthsChanged(widths) => table.update(cx, |t, cx| {
                    if t.delegate_mut().set_widths(widths) {
                        t.refresh(cx);
                        cx.notify();
                    }
                }),
                _ => {}
            }),
        ];
        Parts { state, table, details, status, subs }
    }

    /// Points the whole UI at `backend`. The old state entity (and its event
    /// subscription) is dropped with the old parts.
    fn switch_backend(
        &mut self,
        backend: std::sync::Arc<dyn storm_proto::Backend>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.set_global(BackendHandle::new(backend));
        let query = self.search.read(cx).value();
        let sort = self.table.read(cx).delegate().sort();
        let parts = Self::build(&self.focus_handle, self.filter, &query, sort, window, cx);
        self.state = parts.state;
        self.table = parts.table;
        self.details = parts.details;
        self.status = parts.status;
        self._subscriptions = parts.subs;
        self.focus_list(window, cx);
        cx.notify();
    }

    /// Gives the keyboard to the torrent list (arrows, Space, Enter, shortcuts).
    fn focus_list(&self, window: &mut Window, cx: &mut App) {
        self.table.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn selected(&self, cx: &App) -> Vec<TorrentId> {
        self.table.read(cx).delegate().selected_ids()
    }

    fn sync_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected(cx);
        self.details.update(cx, |d, cx| d.set_selection(ids, cx));
    }

    fn default_dir(&self, cx: &App) -> Option<String> {
        self.state.read(cx).settings.as_ref().map(|s| s.download_dir.clone())
    }

    // -- actions -----------------------------------------------------------

    fn add_link(&mut self, _: &AddLink, window: &mut Window, cx: &mut Context<Self>) {
        // Pre-fill from the clipboard when it holds a link.
        let prefill = cx
            .read_from_clipboard()
            .and_then(|c| c.text())
            .map(|t| format::extract_links(&t))
            .filter(|l| !l.is_empty())
            .map(|l| l.join(" "));
        AddTorrentDialog::open_links(prefill, self.default_dir(cx), window, cx);
    }

    fn paste_link(&mut self, _: &PasteLink, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text());
        // Don't also offer the pasted link as a clipboard toast.
        if let Some(t) = &text {
            self._clipboard_watch.update(cx, |w, cx| w.mark_seen(t, window, cx));
        }
        let links = text.map(|t| format::extract_links(&t)).unwrap_or_default();
        if links.is_empty() {
            ui::notify_info(window, cx, "The clipboard doesn't contain a magnet link or URL.");
        } else {
            AddTorrentDialog::open_links(Some(links.join(" ")), self.default_dir(cx), window, cx);
        }
    }

    fn add_file(&mut self, _: &AddFile, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add .torrent files".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match rx.await {
                Ok(Ok(Some(p))) => p,
                Ok(Err(e)) => {
                    tracing::warn!("file picker failed: {e:#}");
                    return;
                }
                _ => return,
            };
            let _ = this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx));
        })
        .detach();
    }

    fn add_paths(&mut self, paths: Vec<std::path::PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let (files, errors) = read_torrent_files(paths);
        if !errors.is_empty() {
            ui::notify_error(window, cx, format!("Couldn't read: {}", errors.join("; ")));
        }
        AddTorrentDialog::open_files(files, self.default_dir(cx), window, cx);
    }

    fn for_each_selected(
        &mut self,
        what: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        filter: impl Fn(TorrentState) -> bool,
        op: fn(
            std::sync::Arc<dyn storm_proto::Backend>,
            TorrentId,
        ) -> futures::future::BoxFuture<'static, storm_proto::Result<()>>,
    ) {
        let backend = BackendHandle::get(cx).backend;
        let state = self.state.read(cx);
        let ids: Vec<TorrentId> =
            self.selected(cx).into_iter().filter(|id| state.torrent(*id).is_some_and(|t| filter(t.state))).collect();
        for id in ids {
            spawn_op(window, cx, what, op(backend.clone(), id), |_, _, _| {});
        }
    }

    fn pause(&mut self, _: &PauseSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.for_each_selected(
            "Pausing",
            window,
            cx,
            |s| !matches!(s, TorrentState::Paused | TorrentState::FetchingMetadata),
            |b, id| Box::pin(async move { b.pause(id).await }),
        );
    }

    fn resume(&mut self, _: &ResumeSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.for_each_selected(
            "Resuming",
            window,
            cx,
            |s| matches!(s, TorrentState::Paused | TorrentState::Error),
            |b, id| Box::pin(async move { b.resume(id).await }),
        );
    }

    fn remove(&mut self, _: &RemoveSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.open_remove(false, window, cx);
    }

    fn remove_with_data(&mut self, _: &RemoveSelectedWithData, window: &mut Window, cx: &mut Context<Self>) {
        self.open_remove(true, window, cx);
    }

    fn open_remove(&mut self, delete_files: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected(cx);
        let state = self.state.read(cx);
        let names = ids.iter().filter_map(|id| state.torrent(*id)).map(display_name).collect();
        RemoveDialog::open(ids, names, delete_files, window, cx);
    }

    fn set_sequential(&mut self, action: &SetSequential, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let ids: Vec<_> = self.selected_torrents(state, cx).iter().map(|t| t.id).collect();
        if !ids.is_empty() {
            crate::state::set_sequential(&self.state, ids, action.0, window, cx);
        }
    }

    fn settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        // Fetch fresh: another client may have changed them.
        let backend = BackendHandle::get(cx).backend;
        spawn_op(window, cx, "Loading settings", async move { backend.settings().await }, |s, window, cx| {
            SettingsDialog::open(s, window, cx)
        });
    }

    fn toggle_pause(&mut self, _: &TogglePause, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let any_active = self
            .selected_torrents(state, cx)
            .iter()
            .any(|t| !matches!(t.state, TorrentState::Paused | TorrentState::FetchingMetadata | TorrentState::Error));
        if any_active { self.pause(&PauseSelected, window, cx) } else { self.resume(&ResumeSelected, window, cx) }
    }

    /// Enter: reveal the download in the file manager (local backends only;
    /// the details pane already follows the selection).
    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected(cx).len() == 1 {
            self.open_folder(&OpenFolder, window, cx);
        }
    }

    fn move_cursor(&mut self, mv: Move, extend: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            // A page is what's on screen, less one row of overlap.
            let page = t.visible_range().rows().len().saturating_sub(1);
            let mv = match mv {
                Move::PageUp(_) => Move::PageUp(page),
                Move::PageDown(_) => Move::PageDown(page),
                mv => mv,
            };
            if let Some(row) = t.delegate_mut().move_cursor(mv, extend) {
                t.vertical_scroll_handle.scroll_to_item(row, ScrollStrategy::Nearest);
            }
            cx.notify();
        });
        self.sync_selection(cx);
        cx.notify();
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            t.delegate_mut().clear_selection();
            cx.notify();
        });
        self.sync_selection(cx);
        cx.notify();
    }

    fn toggle_column(&mut self, action: &ToggleColumn, window: &mut Window, cx: &mut Context<Self>) {
        self.change_columns(window, cx, |d| d.toggle_column(action.0));
    }

    fn reset_columns(&mut self, _: &ResetColumns, window: &mut Window, cx: &mut Context<Self>) {
        self.change_columns(window, cx, TorrentTableDelegate::reset_columns);
    }

    /// Applies a change to the shown columns and remembers the result, including
    /// a sort that hiding its column cleared. Runs from the header menu, outside
    /// the table's update.
    fn change_columns(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut TorrentTableDelegate) -> bool,
    ) {
        let changed = self.table.update(cx, |t, cx| {
            let changed = change(t.delegate_mut());
            if changed {
                t.refresh(cx);
                cx.notify();
            }
            changed
        });
        if changed {
            let d = self.table.read(cx).delegate();
            let (hidden, sort) = (d.hidden_columns(), d.sort());
            prefs::set_hidden_columns(cx, &hidden);
            prefs::set_table_sort(cx, sort);
        }
        // The menu took the keyboard; give it back to the list.
        self.focus_list(window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            t.delegate_mut().select_all();
            cx.notify();
        });
        self.sync_selection(cx);
    }

    fn connect_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let on_connected: OnConnected = std::rc::Rc::new(move |backend, window, cx| {
            let url = backend.info();
            let _ = this.update(cx, |this, cx| this.switch_backend(backend, window, cx));
            if let storm_proto::BackendKind::Remote { url } = url.kind {
                ui::notify_info(window, cx, format!("Connected to {url}"));
            }
        });
        ConnectDialog::open(on_connected, window, cx);
    }

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let local = cx.global::<LocalEngine>().0.clone();
        self.switch_backend(local, window, cx);
        ui::notify_info(window, cx, "Disconnected: back to the local engine.");
    }

    fn selected_torrents<'a>(&self, state: &'a AppState, cx: &App) -> Vec<&'a storm_proto::TorrentSummary> {
        self.selected(cx).into_iter().filter_map(|id| state.torrent(id)).collect()
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        if !BackendHandle::get(cx).caps.open_files_locally() {
            return;
        }
        let state = self.state.read(cx);
        let Some(t) = self.selected_torrents(state, cx).first().copied() else { return };
        let folder = std::path::PathBuf::from(&t.output_folder);
        let item = t.name.as_ref().map(|n| folder.join(n)).filter(|p| p.exists());
        match item {
            // Opens the containing folder with the torrent's file/folder selected.
            Some(path) => cx.reveal_path(&path),
            None if folder.exists() => cx.open_with_system(&folder),
            None => ui::notify_info(window, cx, format!("{} doesn't exist yet.", folder.display())),
        }
    }

    fn copy_magnet(&mut self, _: &CopyMagnet, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let links: Vec<String> = self
            .selected_torrents(state, cx)
            .iter()
            .map(|t| format::magnet_link(&t.info_hash, t.name.as_deref()))
            .collect();
        self.copy_lines(links, ("magnet link", "magnet links"), window, cx);
    }

    fn copy_info_hash(&mut self, _: &CopyInfoHash, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let hashes: Vec<String> = self.selected_torrents(state, cx).iter().map(|t| t.info_hash.clone()).collect();
        self.copy_lines(hashes, ("info hash", "info hashes"), window, cx);
    }

    fn copy_lines(
        &mut self,
        lines: Vec<String>,
        (one, many): (&str, &str),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if lines.is_empty() {
            return;
        }
        let msg =
            if lines.len() == 1 { format!("Copied the {one}.") } else { format!("Copied {} {many}.", lines.len()) };
        cx.write_to_clipboard(ClipboardItem::new_string(lines.join("\n")));
        ui::notify_info(window, cx, msg);
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        prefs::set_filter(cx, filter);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_filter(filter);
            cx.notify();
        });
        self.sync_selection(cx);
        cx.notify();
    }

    // -- rendering -----------------------------------------------------------

    /// Brand, add/transfer actions and window-level buttons, all in the title bar.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.state.read(cx);
        let selected = self.selected(cx);
        let states: Vec<TorrentState> = selected.iter().filter_map(|id| state.torrent(*id)).map(|t| t.state).collect();
        let can_pause = states.iter().any(|s| !matches!(s, TorrentState::Paused | TorrentState::FetchingMetadata));
        let can_resume = states.iter().any(|s| matches!(s, TorrentState::Paused | TorrentState::Error));
        let focus = self.focus_handle.clone();
        let dispatch = move |action: Box<dyn Action>| {
            let focus = focus.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                // Dispatch from our focus so the action reaches StormApp's handlers.
                focus.dispatch_action(&*action, window, cx);
            }
        };
        let icon_button = |id: &'static str, icon: IconName, tip: &'static str, action: Box<dyn Action>| {
            let tip_action = action.boxed_clone();
            tb_icon_button(id, icon).tooltip_with_action(tip, &*tip_action, Some(CONTEXT)).on_click(dispatch(action))
        };
        let divider = || div().w(px(1.)).h(px(18.)).mx_1().bg(theme.border);
        let caps = BackendHandle::get(cx).caps;
        let connected = caps.connected_to_server();

        TitleBar::new().h(px(44.)).bg(theme.background).border_color(theme.border.opacity(0.6)).child(
            h_flex()
                .size_full()
                .pr_1()
                .gap_1()
                // Interactive groups occlude the title bar's drag area so they get clicks.
                .child(
                    h_flex()
                        .occlude()
                        .gap_1()
                        .child(
                            Button::new("add")
                                .primary()
                                .with_size(Size::Size(px(30.)))
                                .h(px(30.))
                                .pl_2()
                                .pr_3()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .child(Icon::new(IconName::Plus).size(px(15.)))
                                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Add")),
                                )
                                .tooltip_with_action("Add a magnet link or URL", &AddLink, Some(CONTEXT))
                                .on_click(dispatch(Box::new(AddLink))),
                        )
                        .child(icon_button("add-file", IconName::FolderOpen, "Open .torrent files", Box::new(AddFile)))
                        .child(divider())
                        .child(
                            icon_button("resume", IconName::Play, "Resume", Box::new(ResumeSelected))
                                .disabled(!can_resume),
                        )
                        .child(
                            icon_button("pause", IconName::Pause, "Pause", Box::new(PauseSelected))
                                .disabled(!can_pause),
                        )
                        .child(
                            icon_button("remove", IconName::Delete, "Remove…", Box::new(RemoveSelected))
                                .disabled(selected.is_empty()),
                        ),
                )
                .child(div().flex_1())
                .when(connected, |bar| bar.child(self.render_server_pill(&caps, cx)))
                .child(
                    h_flex()
                        .occlude()
                        .gap_0p5()
                        .child(if connected {
                            tb_icon_button("disconnect", gpui_kit::assets::IconName::Unplug)
                                .tooltip("Disconnect and go back to this PC")
                                .on_click(cx.listener(|this, _, w, cx| this.disconnect(w, cx)))
                        } else {
                            tb_icon_button("connect", IconName::Globe)
                                .tooltip("Connect to a storm server…")
                                .on_click(cx.listener(|this, _, w, cx| this.connect_remote(w, cx)))
                        })
                        .child(icon_button("settings", IconName::Settings, "Settings", Box::new(OpenSettings))),
                ),
        )
    }

    /// The torrent list. Its Name column stretches to fill the width, which the
    /// table has no notion of: a canvas behind it reports the width, and the
    /// delegate resizes Name to whatever the other columns leave.
    fn render_table(&self) -> impl IntoElement {
        let table = self.table.clone();
        let measure = canvas(
            move |bounds, window, cx| {
                let width = bounds.size.width;
                if table.read(cx).delegate().needs_fit(width) {
                    let table = table.clone();
                    window.defer(cx, move |_, cx| {
                        table.update(cx, |t, cx| {
                            if t.delegate_mut().fit_name(width) {
                                t.refresh(cx);
                                cx.notify();
                            }
                        })
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div().relative().size_full().child(measure).child(
            DataTable::new(&self.table)
                .stripe(false)
                // On a borderless table the ring reads as a stray line; the selection tint shows focus.
                .focus_ring(false)
                .bordered(false)
                .with_size(Size::Size(px(38.)))
                .header_row_height(px(30.)),
        )
    }

    /// Quick filters with counts on the left, search on the right.
    /// Shown in the title bar only when connected to a server: which one, and
    /// whether the event stream is up.
    fn render_server_pill(&self, caps: &crate::caps::Capabilities, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::state::Connection;
        let theme = cx.theme().clone();
        let url = caps.connection_label().to_owned();
        let host = format::host_of(&url).to_owned();
        let (dot, status) = match &self.state.read(cx).connection {
            Connection::Connected => (theme.success, "Connected".to_owned()),
            Connection::Connecting => (theme.warning, "Connecting…".to_owned()),
            Connection::Disconnected { error, retry_in } => (
                theme.danger,
                format!(
                    "Disconnected{} · retrying in {}s",
                    error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default(),
                    retry_in.as_secs()
                ),
            ),
        };
        let tip: SharedString = format!("{url} · {status}").into();
        h_flex()
            .id("server-pill")
            .occlude()
            .h(px(28.))
            .px_2p5()
            .mr_1()
            .gap_2()
            .items_center()
            .rounded(px(8.))
            .bg(theme.secondary.opacity(0.6))
            .text_sm()
            .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(dot))
            .child(div().max_w(px(260.)).truncate().child(host))
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
    }

    fn render_filter_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let counts = Filter::counts(&self.state.read(cx).torrents);
        let pills = Filter::ALL.into_iter().zip(counts).filter_map(|(filter, count)| {
            // Errors only earn a pill when there are some (or it's the active filter).
            if filter == Filter::Error && count == 0 && self.filter != Filter::Error {
                return None;
            }
            let active = filter == self.filter;
            let danger = filter == Filter::Error;
            Some(
                h_flex()
                    .id(filter.label())
                    .h(px(28.))
                    .px_2p5()
                    .gap_1p5()
                    .rounded(px(7.))
                    .text_sm()
                    // Same weight active or not, so selecting a pill doesn't reflow the others.
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .map(|d| {
                        if active {
                            d.bg(theme.secondary).text_color(theme.foreground)
                        } else {
                            d.text_color(theme.muted_foreground)
                                .hover(|d| d.bg(theme.secondary.opacity(0.5)).text_color(theme.foreground))
                        }
                    })
                    .child(filter.label())
                    .child(
                        div()
                            .min_w(px(20.))
                            .h(px(18.))
                            .px_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .map(|d| match (danger, active) {
                                (true, _) => d.bg(theme.danger.opacity(0.15)).text_color(theme.danger),
                                (false, true) => d.bg(theme.background.opacity(0.6)).text_color(theme.foreground),
                                (false, false) => d.bg(theme.secondary.opacity(0.7)).text_color(theme.muted_foreground),
                            })
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx))),
            )
        });
        h_flex().h(px(44.)).flex_shrink_0().px_2().gap_0p5().children(pills).child(div().flex_1()).child(
            div().w(px(240.)).child(
                Input::new(&self.search)
                    .small()
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).size(px(14.)).text_color(theme.muted_foreground)),
            ),
        )
    }
}

impl Focusable for StormApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StormApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        self.frames = self.frames.saturating_add(1);
        // Snapshot the window the first frame a dialog is up (the screen still
        // shows the previous, dialog-free frame), drop it once all are closed.
        if window.has_active_dialog(cx) {
            // The capture reads the screen, so only trust it once we've been drawn
            // and nothing else is likely on top of us.
            if self.backdrop.is_none() && self.frames > 2 && window.is_window_active() {
                self.backdrop = crate::backdrop::capture(window);
                self.backdrop_seq += 1;
            }
        } else if let Some(image) = self.backdrop.take() {
            let _ = window.drop_image(image);
        }
        let backdrop = self.backdrop.clone().map(|image| {
            img(ImageSource::Render(image))
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .object_fit(ObjectFit::Fill)
                .with_animation(
                    ElementId::NamedInteger("backdrop".into(), self.backdrop_seq as u64),
                    Animation::new(std::time::Duration::from_millis(160)).with_easing(ease_out_quint()),
                    |el, delta| el.opacity(delta),
                )
        });
        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::add_link))
            .on_action(cx.listener(Self::add_file))
            .on_action(cx.listener(Self::paste_link))
            .on_action(cx.listener(Self::pause))
            .on_action(cx.listener(Self::resume))
            .on_action(cx.listener(Self::remove))
            .on_action(cx.listener(Self::remove_with_data))
            .on_action(cx.listener(Self::settings))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::copy_magnet))
            .on_action(cx.listener(Self::set_sequential))
            .on_action(cx.listener(Self::copy_info_hash))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::toggle_pause))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::toggle_column))
            .on_action(cx.listener(Self::reset_columns))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.move_cursor(Move::Up, false, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_cursor(Move::Down, false, cx)))
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.move_cursor(Move::PageUp(0), false, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.move_cursor(Move::PageDown(0), false, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.move_cursor(Move::First, false, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.move_cursor(Move::Last, false, cx)))
            .on_action(cx.listener(|this, _: &ExtendPrev, _, cx| this.move_cursor(Move::Up, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.move_cursor(Move::Down, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendPageUp, _, cx| this.move_cursor(Move::PageUp(0), true, cx)))
            .on_action(cx.listener(|this, _: &ExtendPageDown, _, cx| this.move_cursor(Move::PageDown(0), true, cx)))
            .on_action(cx.listener(|this, _: &ExtendFirst, _, cx| this.move_cursor(Move::First, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendLast, _, cx| this.move_cursor(Move::Last, true, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_paths(paths.paths().to_vec(), window, cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_title_bar(cx))
            .child(self.render_filter_bar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("main-split").child(resizable_panel().child(self.render_table())).child(
                        resizable_panel().size(px(260.)).size_range(px(120.)..px(800.)).child(self.details.clone()),
                    ),
                ),
            )
            .child(self.status.clone())
            .relative()
            .children(backdrop)
    }
}

/// Title-bar icon button: a 32px square hit area whose hover background is the
/// whole square.
fn tb_icon_button(id: &'static str, icon: impl Into<Icon>) -> Button {
    Button::new(id)
        .ghost()
        .with_size(Size::Size(px(32.)))
        .w(px(32.))
        .h(px(32.))
        .p_0()
        .child(Icon::from(icon.into()).size(px(17.)))
}
