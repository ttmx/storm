//! Bottom pane: General / Files / Peers for the selected torrent.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use storm_proto::{FilePriority, PeerInfo, TorrentDetails, TorrentFile, TorrentId, TorrentSummary};

use crate::caps::BackendHandle;
use crate::format;
use crate::state::AppState;
use crate::ui::{self, spawn_op};
use crate::views::file_tree::{self, Check, Row, RowKind, check_of};
use crate::views::local_copy::{Copies, CopyRequest, CopyState};
use crate::views::piece_map;
use crate::views::torrent_table::display_name;

const POLL_EVERY: Duration = Duration::from_secs(2);
/// How often the partial files of running copies are measured.
const COPY_TICK: Duration = Duration::from_millis(400);
/// Files copied to this PC at the same time.
const COPY_CONCURRENCY: usize = 3;

const ROW_H: f32 = 28.;
const INDENT: f32 = 16.;
// Files tab column widths.
const W_CHECK: f32 = 16.;
const W_SIZE: f32 = 76.;
const W_PROGRESS: f32 = 150.;
const W_PRIORITY: f32 = 76.;
const W_GET: f32 = 40.;
const W_ACTIONS: f32 = 56.;

// gpui needs a concrete family name; "monospace" doesn't resolve.
#[cfg(target_os = "windows")]
const MONO_FONT: &str = "Consolas";
#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const MONO_FONT: &str = "DejaVu Sans Mono";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailsTab {
    General,
    Files,
    Peers,
}

const TABS: [DetailsTab; 3] = [DetailsTab::General, DetailsTab::Files, DetailsTab::Peers];

pub struct DetailsPane {
    state: Entity<AppState>,
    /// What the table has selected (the pane shows details only for exactly one).
    selection: Vec<TorrentId>,
    tab: DetailsTab,
    details: Option<TorrentDetails>,
    peers: Option<Vec<PeerInfo>>,
    error: Option<String>,
    files_scroll: UniformListScrollHandle,
    peers_scroll: UniformListScrollHandle,
    poll: Task<()>,

    // Files tab.
    /// Visible rows of the file tree, rebuilt when the file list or folding changes.
    rows: Vec<Row>,
    collapsed: HashSet<String>,
    /// The tree has folders (files then reserve a chevron column to line up).
    has_dirs: bool,
    /// Checked files, as positions into `details.files`.
    checked: BTreeSet<usize>,
    /// Row of the last plain click/check, for shift-click ranges.
    anchor: Option<usize>,

    // "Download to this PC" (remote backends).
    copies: Copies,
    copy_tasks: Vec<Task<()>>,
    copy_ticker: Task<()>,
    ticking: bool,
}

impl DetailsPane {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self {
            state,
            selection: Vec::new(),
            tab: DetailsTab::General,
            details: None,
            peers: None,
            error: None,
            files_scroll: UniformListScrollHandle::new(),
            peers_scroll: UniformListScrollHandle::new(),
            poll: Task::ready(()),
            rows: Vec::new(),
            collapsed: HashSet::new(),
            has_dirs: false,
            checked: BTreeSet::new(),
            anchor: None,
            copies: Copies::default(),
            copy_tasks: Vec::new(),
            copy_ticker: Task::ready(()),
            ticking: false,
        }
    }

    fn current(&self) -> Option<TorrentId> {
        match self.selection.as_slice() {
            [id] => Some(*id),
            _ => None,
        }
    }

    pub fn set_selection(&mut self, ids: Vec<TorrentId>, cx: &mut Context<Self>) {
        if ids == self.selection {
            return;
        }
        let changed = self.current()
            != match ids.as_slice() {
                [id] => Some(*id),
                _ => None,
            };
        self.selection = ids;
        if changed {
            self.details = None;
            self.peers = None;
            self.error = None;
            self.rows.clear();
            self.collapsed.clear();
            self.checked.clear();
            self.anchor = None;
            self.restart_poll(cx);
        }
        cx.notify();
    }

    fn set_tab(&mut self, tab: DetailsTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.restart_poll(cx);
            cx.notify();
        }
    }

    fn set_details(&mut self, d: TorrentDetails) {
        let same_paths = self.details.as_ref().is_some_and(|old| {
            old.files.len() == d.files.len() && old.files.iter().zip(&d.files).all(|(a, b)| a.path == b.path)
        });
        self.details = Some(d);
        if !same_paths {
            self.rebuild_rows();
            let n = self.details.as_ref().map_or(0, |d| d.files.len());
            self.checked.retain(|p| *p < n);
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows = match &self.details {
            Some(d) => file_tree::build_rows(&d.files, &self.collapsed),
            None => Vec::new(),
        };
        self.has_dirs = self.rows.iter().any(|r| matches!(r.kind, RowKind::Dir { .. }));
        if self.anchor.is_some_and(|a| a >= self.rows.len()) {
            self.anchor = None;
        }
    }

    /// One loop per (torrent, tab): Files and General refresh details (per-file
    /// progress, the piece map), Peers refreshes peers and fetches details once.
    /// Replacing the task cancels the previous loop.
    fn restart_poll(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.current() else {
            self.poll = Task::ready(());
            return;
        };
        let backend = BackendHandle::get(cx).backend;
        let tab = self.tab;
        self.poll = cx.spawn(async move |this, cx| {
            loop {
                let need_details = match tab {
                    DetailsTab::Files | DetailsTab::General => true,
                    DetailsTab::Peers => this.read_with(cx, |p, _| p.details.is_none()).unwrap_or(false),
                };
                if need_details {
                    let r = backend.torrent_details(id).await;
                    let ok = this.update(cx, |p, cx| {
                        match r {
                            Ok(d) => {
                                p.set_details(d);
                                p.error = None;
                            }
                            Err(e) => p.error = Some(e.to_string()),
                        }
                        cx.notify();
                    });
                    if ok.is_err() {
                        return;
                    }
                }
                if tab == DetailsTab::Peers {
                    let r = backend.torrent_peers(id).await;
                    let ok = this.update(cx, |p, cx| {
                        match r {
                            Ok(peers) => p.peers = Some(peers),
                            Err(e) => p.error = Some(e.to_string()),
                        }
                        cx.notify();
                    });
                    if ok.is_err() {
                        return;
                    }
                }
                cx.background_executor().timer(POLL_EVERY).await;
            }
        });
    }

    // -- file selection ----------------------------------------------------

    fn toggle_collapsed(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_owned());
        }
        self.anchor = None;
        self.rebuild_rows();
        cx.notify();
    }

    fn set_checked(&mut self, files: impl IntoIterator<Item = usize>, on: bool) {
        for p in files {
            if on {
                self.checked.insert(p);
            } else {
                self.checked.remove(&p);
            }
        }
    }

    fn files_in_rows(&self, from: usize, to: usize) -> Vec<usize> {
        let (a, b) = (from.min(to), from.max(to));
        self.rows
            .get(a..=b.min(self.rows.len().saturating_sub(1)))
            .unwrap_or_default()
            .iter()
            .flat_map(|r| r.files().iter().copied())
            .collect()
    }

    /// Checkbox click: toggles the row (a folder toggles everything below it).
    /// With shift, the whole range from the last clicked row gets the new state.
    fn check_row(&mut self, ix: usize, shift: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        let on = check_of(row.files(), &self.checked) != Check::On;
        let files = match (shift, self.anchor) {
            (true, Some(a)) => self.files_in_rows(a, ix),
            _ => row.files().to_vec(),
        };
        self.set_checked(files, on);
        self.anchor = Some(ix);
        cx.notify();
    }

    /// Row click, file-manager style: plain selects only this row, Ctrl toggles,
    /// Shift selects the range from the last clicked row. Plain clicks on a
    /// folder fold/unfold it.
    fn click_row(&mut self, ix: usize, mods: Modifiers, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        let secondary = mods.secondary(cx);
        if mods.shift {
            let files = self.files_in_rows(self.anchor.unwrap_or(ix), ix);
            if !secondary {
                self.checked.clear();
            }
            self.set_checked(files, true);
        } else if secondary {
            return self.check_row(ix, false, cx);
        } else if let RowKind::Dir { path, .. } = &row.kind {
            let path = path.clone();
            return self.toggle_collapsed(&path, cx);
        } else {
            let files = row.files().to_vec();
            let only_this = self.checked.len() == files.len() && files.iter().all(|f| self.checked.contains(f));
            self.checked.clear();
            if !only_this {
                self.set_checked(files, true);
            }
            self.anchor = Some(ix);
        }
        cx.notify();
    }

    fn check_all(&mut self, cx: &mut Context<Self>) {
        let n = self.details.as_ref().map_or(0, |d| d.files.len());
        if self.checked.len() >= n {
            self.checked.clear();
        } else {
            self.checked = (0..n).collect();
        }
        self.anchor = None;
        cx.notify();
    }

    // -- included files ----------------------------------------------------

    /// Includes or skips `positions` in the torrent's download.
    fn set_included(&mut self, positions: Vec<usize>, include: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(details) = &mut self.details else { return };
        let id = details.summary.id;
        let affected: HashSet<usize> = positions.into_iter().collect();
        let mut wanted: Vec<usize> = details
            .files
            .iter()
            .enumerate()
            .filter(|(pos, f)| if affected.contains(pos) { include } else { f.included })
            .map(|(_, f)| f.index)
            .collect();
        wanted.sort_unstable();
        if wanted.is_empty() {
            ui::notify_error(window, cx, "At least one file must stay included.");
            return;
        }
        // Optimistic: flip now, the next poll confirms.
        for pos in &affected {
            if let Some(f) = details.files.get_mut(*pos) {
                f.included = include;
            }
        }
        cx.notify();
        let backend = BackendHandle::get(cx).backend;
        let this = cx.entity().downgrade();
        spawn_op(
            window,
            cx,
            "Changing file selection",
            async move { backend.set_included_files(id, wanted).await },
            move |_, _, cx| {
                let _ = this.update(cx, |p, cx| p.restart_poll(cx));
            },
        );
    }

    // -- priorities -----------------------------------------------------------

    /// Sets the download priority of the files at `positions`.
    fn set_priority(
        &mut self,
        positions: Vec<usize>,
        priority: FilePriority,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(details) = &mut self.details else { return };
        let id = details.summary.id;
        let mut indices = Vec::with_capacity(positions.len());
        // Optimistic: change now, the next poll confirms.
        for pos in positions {
            if let Some(f) = details.files.get_mut(pos) {
                f.priority = priority;
                indices.push(f.index);
            }
        }
        if indices.is_empty() {
            return;
        }
        cx.notify();
        let backend = BackendHandle::get(cx).backend;
        let this = cx.entity().downgrade();
        spawn_op(
            window,
            cx,
            "Changing file priority",
            async move { backend.set_file_priority(id, indices, priority).await },
            move |_, _, cx| {
                let _ = this.update(cx, |p, cx| p.restart_poll(cx));
            },
        );
    }

    /// A menu of the three priorities, applying the picked one to `positions`.
    /// `current` gets the check mark (`None` for a mix).
    fn priority_menu(
        &self,
        positions: Vec<usize>,
        current: Option<FilePriority>,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let this = cx.entity().downgrade();
        move |mut menu, _, _| {
            for p in FilePriority::ALL {
                let (this, positions) = (this.clone(), positions.clone());
                menu = menu.item(PopupMenuItem::new(p.label()).checked(current == Some(p)).on_click(
                    move |_, window, cx| {
                        let _ = this.update(cx, |d, cx| d.set_priority(positions.clone(), p, window, cx));
                    },
                ));
            }
            menu
        }
    }

    // -- download to this PC -----------------------------------------------

    /// Checked files that are complete on the backend (only those can be copied).
    fn checked_complete(&self) -> Vec<usize> {
        let Some(d) = &self.details else { return Vec::new() };
        self.checked.iter().copied().filter(|p| d.files.get(*p).is_some_and(is_complete)).collect()
    }

    /// Asks once for a folder on this PC, then copies the checked files into it,
    /// keeping their sub-folders.
    fn download_checked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current() else { return };
        let Some(d) = &self.details else { return };
        let files: Vec<TorrentFile> =
            self.checked_complete().into_iter().filter_map(|p| d.files.get(p).cloned()).collect();
        let positions = self.checked_complete();
        if files.is_empty() {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Download here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let dir = match rx.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(p) => p,
                    None => return,
                },
                Ok(Err(e)) => {
                    tracing::warn!("folder picker failed: {e:#}");
                    return;
                }
                _ => return,
            };
            let reqs: Vec<CopyRequest> = positions
                .into_iter()
                .zip(files)
                .map(|(pos, f)| (pos, f.index, f.length, dir.join(file_tree::safe_relative(&f.path))))
                .collect();
            let _ = this.update(cx, |p, cx| p.start_copies(id, reqs, cx));
        })
        .detach();
    }

    /// Single file: a save dialog, then the same copy machinery.
    fn download_one(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current() else { return };
        let Some(f) = self.details.as_ref().and_then(|d| d.files.get(pos)).cloned() else { return };
        let rx = cx.prompt_for_new_path(&local_downloads_dir(), Some(format::file_name(&f.path)));
        cx.spawn_in(window, async move |this, cx| {
            let dest = match rx.await {
                Ok(Ok(Some(p))) => p,
                Ok(Err(e)) => {
                    tracing::warn!("save dialog failed: {e:#}");
                    return;
                }
                _ => return,
            };
            let _ = this.update(cx, |p, cx| p.start_copies(id, vec![(pos, f.index, f.length, dest)], cx));
        })
        .detach();
    }

    fn retry_copy(&mut self, pos: usize, cx: &mut Context<Self>) {
        let Some(id) = self.current() else { return };
        let Some(job) = self.copies.for_file(id, pos).cloned() else { return };
        self.start_copies(id, vec![(job.pos, job.index, job.len, job.dest)], cx);
    }

    fn start_copies(&mut self, torrent: TorrentId, reqs: Vec<CopyRequest>, cx: &mut Context<Self>) {
        if self.copies.summary().active == 0 {
            self.copy_tasks.clear();
        }
        let ids = self.copies.enqueue(torrent, reqs);
        if ids.is_empty() {
            return;
        }
        let backend = BackendHandle::get(cx).backend;
        self.copy_tasks.push(cx.spawn(async move |this, cx| {
            futures::stream::iter(ids)
                .for_each_concurrent(COPY_CONCURRENCY, |job_id| {
                    let (backend, this, mut cx) = (backend.clone(), this.clone(), cx.clone());
                    async move {
                        let job = this.update(&mut cx, |p, cx| {
                            let job = p.copies.get(job_id).filter(|j| j.state == CopyState::Queued).cloned()?;
                            p.copies.set(job_id, CopyState::Running { bytes: 0 });
                            cx.notify();
                            Some(job)
                        });
                        let Ok(Some(job)) = job else { return };
                        let result = match job.dest.parent().map(std::fs::create_dir_all).transpose() {
                            Ok(_) => backend
                                .download_file(job.torrent, job.index, job.dest.clone())
                                .await
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(format!("creating folder: {e}")),
                        };
                        let _ = this.update(&mut cx, |p, cx| {
                            // A cancelled job stays cancelled even if its last write raced us.
                            if p.copies.get(job_id).is_some_and(|j| j.state.is_active()) {
                                p.copies.set(
                                    job_id,
                                    match result {
                                        Ok(()) => CopyState::Done,
                                        Err(e) => CopyState::Failed(e),
                                    },
                                );
                            }
                            cx.notify();
                        });
                    }
                })
                .await;
        }));
        self.ensure_copy_ticker(cx);
        cx.notify();
    }

    /// Measures partial files while copies run, for byte progress.
    fn ensure_copy_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        self.copy_ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(COPY_TICK).await;
                let keep = this.update(cx, |p, cx| {
                    for j in &mut p.copies.jobs {
                        if let CopyState::Running { bytes } = &mut j.state
                            && let Ok(m) = std::fs::metadata(storm_client::partial_path(&j.dest))
                        {
                            *bytes = m.len();
                        }
                    }
                    let keep = p.copies.summary().active > 0;
                    p.ticking = keep;
                    cx.notify();
                    keep
                });
                if !matches!(keep, Ok(true)) {
                    return;
                }
            }
        });
    }

    fn cancel_copies(&mut self, cx: &mut Context<Self>) {
        // Dropping the tasks aborts the transfers; then remove what they left behind.
        self.copy_tasks.clear();
        let partials: Vec<PathBuf> = self.copies.cancel().iter().map(|j| storm_client::partial_path(&j.dest)).collect();
        cx.spawn(async move |_, cx| {
            // Give the aborted writers a moment to close their files (Windows locks them).
            cx.background_executor().timer(Duration::from_millis(500)).await;
            for p in partials {
                let _ = std::fs::remove_file(p);
            }
        })
        .detach();
        cx.notify();
    }

    // -- rendering ---------------------------------------------------------

    fn render_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let id = self.current().unwrap_or_default();
        let state = self.state.read(cx);
        // Prefer the live snapshot over the (possibly stale) details summary.
        let Some(t) = state.torrent(id).cloned().or_else(|| self.details.as_ref().map(|d| d.summary.clone())) else {
            return empty_state(IconName::Info, "Loading…", None, cx);
        };
        let theme = cx.theme().clone();
        let caps = BackendHandle::get(cx).caps;
        let d = self.details.as_ref();
        let dash = || "—".to_owned();
        let or_dash = |s: String| if s.is_empty() { dash() } else { s };
        let pieces = match d.and_then(|d| Some((d.piece_count?, d.piece_length?))) {
            Some((n, len)) => format!("{n} × {}", format::bytes(len as u64)),
            None => dash(),
        };
        let folder = t.output_folder.clone();

        let order_menu =
            {
                let (state, sequential) = (self.state.clone(), t.sequential);
                move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                    [(false, "Rarest first"), (true, "Sequential")].into_iter().fold(menu, |menu, (seq, label)| {
                        let state = state.clone();
                        menu.item(PopupMenuItem::new(label).checked(sequential == seq).on_click(
                            move |_, window, cx| crate::state::set_sequential(&state, vec![id], seq, window, cx),
                        ))
                    })
                }
            };
        let transfer = column()
            .child(kv("Progress", progress_row(&t, d, cx), cx))
            .child(kv("Speed", speeds(&t, cx), cx))
            .child(kv(
                "Transferred",
                format!(
                    "{} down · {} up · ratio {}",
                    format::bytes(t.downloaded_bytes),
                    format::bytes(t.uploaded_bytes),
                    format::ratio(t.ratio())
                ),
                cx,
            ))
            .child(kv(
                "Peers",
                format!("{} connected · {} seeders · {} seen", t.peers_live, t.seeders_live, t.peers_seen),
                cx,
            ))
            .child(kv(
                "Order",
                // Text-sized and pulled left by its padding, so the label lines up with the
                // values above it; the caret marks it as a choice.
                Button::new("order")
                    .small()
                    .compact()
                    .ghost()
                    .ml(px(-6.))
                    .mt(px(-2.))
                    .label(if t.sequential { "Sequential" } else { "Rarest first" })
                    .dropdown_caret(true)
                    .dropdown_menu(order_menu),
                cx,
            ));

        let hash = t.info_hash.clone();
        let torrent = column()
            .child(kv("Name", div().truncate().child(display_name(&t)), cx))
            .child(kv(
                "Info hash",
                h_flex()
                    .gap_1()
                    .min_w_0()
                    .child(div().truncate().font_family(MONO_FONT).text_xs().child(hash.clone()))
                    .child(Clipboard::new("copy-hash").value(hash)),
                cx,
            ))
            .child(kv(
                "Save path",
                h_flex().gap_1().min_w_0().child(div().truncate().child(folder.clone())).when(
                    caps.open_files_locally(),
                    |row| {
                        row.child(icon_button(
                            "open-folder",
                            IconName::FolderOpen,
                            "Open folder",
                            false,
                            cx,
                            move |_, _, cx| cx.open_with_system(Path::new(&folder)),
                        ))
                    },
                ),
                cx,
            ))
            .child(kv("Pieces", pieces, cx))
            .child(kv("Added", or_dash(format::date(t.added_at)), cx))
            .child(kv("Completed", or_dash(format::date(t.completed_at)), cx))
            .when_some(d.and_then(|d| d.comment.clone()).filter(|c| !c.trim().is_empty()), |s, c| {
                s.child(kv("Comment", div().truncate().child(c), cx))
            })
            .when_some(d.and_then(|d| d.created_by.clone()), |s, c| {
                s.child(kv("Created by", div().truncate().child(c), cx))
            })
            .when_some(d.and_then(|d| d.creation_date), |s, ts| s.child(kv("Created", format::date(Some(ts)), cx)))
            .when_some(d.filter(|d| !d.trackers.is_empty()), |s, d| {
                let more = d.trackers.len().saturating_sub(4);
                s.child(kv(
                    "Trackers",
                    v_flex()
                        .min_w_0()
                        .children(d.trackers.iter().take(4).cloned().map(|u| div().truncate().child(u)))
                        .when(more > 0, |v| {
                            v.child(div().text_color(theme.muted_foreground).child(format!("+{more} more")))
                        }),
                    cx,
                ))
            });

        div()
            .id("general")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .children(ui::error_banner(t.error.as_deref(), cx))
                    .child(h_flex().items_start().gap_8().child(transfer).child(torrent)),
            )
            .into_any_element()
    }

    fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(details) = &self.details else {
            return empty_state(IconName::File, "Loading files…", None, cx);
        };
        if details.files.is_empty() {
            return empty_state(
                IconName::LoaderCircle,
                "No file list yet",
                Some("Metadata is still being fetched."),
                cx,
            );
        }
        let theme = cx.theme().clone();
        let all: Vec<usize> = (0..details.files.len()).collect();
        let header = h_flex()
            .h(px(ROW_H))
            .px_3()
            .gap_3()
            .flex_none()
            .text_xs()
            .text_color(theme.muted_foreground)
            .border_b_1()
            .border_color(theme.border)
            .child(
                tri_checkbox("check-all", check_of(&all, &self.checked), cx)
                    .tooltip(|w, cx| Tooltip::new("Select all").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.check_all(cx)
                    })),
            )
            .child(div().flex_1().child("Name"))
            .child(div().w(px(W_SIZE)).text_right().child("Size"))
            .child(div().w(px(W_PROGRESS)).child("Progress"))
            .child(div().w(px(W_PRIORITY)).text_center().child("Priority"))
            .child(div().w(px(W_GET)).child("Get"))
            .child(div().w(px(W_ACTIONS)));

        let list = uniform_list(
            "files",
            self.rows.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range.filter_map(|ix| this.render_file_row(ix, cx)).collect()
            }),
        )
        .track_scroll(&self.files_scroll)
        .flex_1();

        v_flex().size_full().child(self.render_files_toolbar(cx)).child(header).child(list).into_any_element()
    }

    fn render_files_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let caps = BackendHandle::get(cx).caps;
        let files = self.details.as_ref().map(|d| d.files.as_slice()).unwrap_or_default();
        let size_of =
            |ps: &mut dyn Iterator<Item = usize>| ps.filter_map(|p| files.get(p)).map(|f| f.length).sum::<u64>();
        let n_checked = self.checked.len();

        let left = if n_checked == 0 {
            let included = files.iter().filter(|f| f.included).count();
            h_flex().gap_2().text_xs().text_color(theme.muted_foreground).child(format!(
                "{} files · {} · {} included",
                files.len(),
                format::bytes(files.iter().map(|f| f.length).sum()),
                included
            ))
        } else {
            let complete = self.checked_complete().len();
            let checked: Vec<usize> = self.checked.iter().copied().collect();
            let prio_menu = self.priority_menu(checked.clone(), common_priority(files, &checked), cx);
            let (inc, skip) = (checked.clone(), checked);
            h_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .mr_2()
                        .child(format!("{n_checked} selected · {}", format::bytes(size_of(&mut self.checked.iter().copied())))),
                )
                .when(caps.download_to_this_pc(), |row| {
                    let label = if complete == 1 { "Download 1 file to this PC".to_owned() } else { format!("Download {complete} files to this PC") };
                    row.child(
                        Button::new("dl-checked")
                            .xsmall()
                            .primary()
                            .icon(IconName::ArrowDown)
                            .label(label)
                            .disabled(complete == 0)
                            .tooltip(if complete < n_checked {
                                "Copies the selected files into a folder on this PC. Files still downloading on the server are skipped."
                            } else {
                                "Copies the selected files into a folder on this PC, keeping sub-folders."
                            })
                            .on_click(cx.listener(|this, _, w, cx| this.download_checked(w, cx))),
                    )
                })
                .child(
                    Button::new("priority")
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronsUpDown)
                        .label("Priority")
                        .dropdown_menu(prio_menu),
                )
                .child(
                    Button::new("include")
                        .xsmall()
                        .ghost()
                        .icon(IconName::Check)
                        .label("Include")
                        .tooltip("Download these files as part of the torrent")
                        .on_click(cx.listener(move |this, _, w, cx| this.set_included(inc.clone(), true, w, cx))),
                )
                .child(
                    Button::new("skip")
                        .xsmall()
                        .ghost()
                        .icon(IconName::Ban)
                        .label("Skip")
                        .tooltip("Don't download these files")
                        .on_click(cx.listener(move |this, _, w, cx| this.set_included(skip.clone(), false, w, cx))),
                )
                .child(
                    Button::new("clear-checked")
                        .xsmall()
                        .ghost()
                        .icon(IconName::Close)
                        .tooltip("Clear selection")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.checked.clear();
                            cx.notify();
                        })),
                )
        };

        let s = self.copies.summary();
        let right = (s.total > 0).then(|| {
            let fraction = s.fraction();
            let label = if s.active > 0 {
                format!(
                    "Copying {} of {} · {}",
                    (s.done + s.failed + 1).min(s.total),
                    s.total,
                    format::percent(fraction)
                )
            } else if s.failed == 0 {
                format!("Copied {} {} to this PC", s.done, if s.done == 1 { "file" } else { "files" })
            } else {
                format!("Copied {} of {}", s.done, s.total)
            };
            h_flex()
                .gap_2()
                .text_xs()
                .when(s.active > 0, |r| r.child(Spinner::new().xsmall().color(theme.muted_foreground)))
                .when(s.active == 0 && s.failed == 0, |r| {
                    r.child(Icon::new(IconName::CircleCheck).xsmall().text_color(theme.success))
                })
                .child(div().text_color(theme.muted_foreground).child(label))
                .when(s.active > 0, |r| r.child(div().w(px(90.)).child(thin_bar(fraction, theme.progress_bar, cx))))
                .when(s.failed > 0, |r| r.child(div().text_color(theme.danger).child(format!("{} failed", s.failed))))
                .child(if s.active > 0 {
                    Button::new("cancel-copies")
                        .xsmall()
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_copies(cx)))
                } else {
                    Button::new("dismiss-copies").xsmall().ghost().icon(IconName::Close).tooltip("Dismiss").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.copies.clear_finished();
                            cx.notify();
                        }),
                    )
                })
        });

        h_flex()
            .h(px(36.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(left)
            .child(div().flex_1())
            .children(right)
    }

    fn render_file_row(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let details = self.details.as_ref()?;
        let row = self.rows.get(ix)?;
        let theme = cx.theme().clone();
        let caps = BackendHandle::get(cx).caps;
        let positions = row.files().to_vec();
        let files: Vec<&TorrentFile> = positions.iter().filter_map(|p| details.files.get(*p)).collect();
        let len: u64 = files.iter().map(|f| f.length).sum();
        let done: u64 = files.iter().map(|f| f.progress_bytes.min(f.length)).sum();
        let p = if len == 0 { 1.0 } else { done as f64 / len as f64 };
        let included = Check::of(files.iter().filter(|f| f.included).count(), files.len());
        let checked = check_of(&positions, &self.checked);
        let job = match row.kind {
            RowKind::File { pos } => self.copies.for_file(details.summary.id, pos),
            RowKind::Dir { .. } => None,
        };
        let muted = theme.muted_foreground;

        // Name: indent, chevron/spacer, icon, name, inline copy error.
        let (lead, icon) = match &row.kind {
            RowKind::Dir { collapsed, .. } => (
                Some(
                    Icon::new(if *collapsed { IconName::ChevronRight } else { IconName::ChevronDown })
                        .xsmall()
                        .text_color(muted)
                        .into_any_element(),
                ),
                IconName::Folder,
            ),
            // Line files up with folder names; a flat list needs no chevron column.
            RowKind::File { .. } => {
                (self.has_dirs.then(|| div().flex_none().size_3().into_any_element()), IconName::File)
            }
        };
        let name = h_flex()
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .pl(px(row.depth as f32 * INDENT))
            .children(lead)
            .child(Icon::new(icon).small().text_color(muted))
            .child(div().min_w_0().truncate().child(row.name.clone()))
            .when_some(
                job.and_then(|j| match &j.state {
                    CopyState::Failed(e) => Some(e.clone()),
                    _ => None,
                }),
                |n, e| n.child(div().flex_shrink(1.).min_w_0().truncate().text_xs().text_color(theme.danger).child(e)),
            );

        let have = match (&details.have_pieces, details.piece_length) {
            (Some(map), Some(pl)) if p < 1.0 => {
                Some(piece_map::pieces_of(map, files.iter().map(|f| piece_map::file_pieces(f, pl))))
            }
            _ => None,
        };
        let bar = div().flex_1().child(piece_bar(p, have, cx));
        let progress = if included == Check::Off && p < 1.0 {
            div().w(px(W_PROGRESS)).text_xs().text_color(muted).child("Skipped").into_any_element()
        } else {
            h_flex()
                .w(px(W_PROGRESS))
                .gap_2()
                .child(bar)
                .child(div().w(px(40.)).text_xs().text_right().text_color(muted).child(format::percent(p)))
                .into_any_element()
        };

        let priority = common_priority(&details.files, &positions);
        let prio_label = priority.map_or("Mixed", |p| p.label());
        let prio_color = match priority {
            Some(FilePriority::High) => theme.foreground,
            _ => muted,
        };
        let prio = div().id(("prio-wrap", ix)).w(px(W_PRIORITY)).on_click(|_, _, cx| cx.stop_propagation()).child(
            Button::new(("prio", ix))
                .xsmall()
                .ghost()
                .w_full()
                .text_color(prio_color)
                .label(prio_label)
                .dropdown_menu(self.priority_menu(positions.clone(), priority, cx)),
        );

        let get_files = positions.clone();
        let get = div().id(("get-wrap", ix)).w(px(W_GET)).on_click(|_, _, cx| cx.stop_propagation()).child(
            Switch::new(("get", ix))
                .xsmall()
                .checked(included != Check::Off)
                .tooltip(match included {
                    Check::On => "Included in the torrent download",
                    Check::Partial => "Some files inside are skipped",
                    Check::Off => "Skipped: not downloaded",
                })
                .on_click(cx.listener(move |this, on: &bool, w, cx| this.set_included(get_files.clone(), *on, w, cx))),
        );

        let actions = h_flex().w(px(W_ACTIONS)).gap_0p5().justify_end().when_some(
            match row.kind {
                RowKind::File { pos } => Some(pos),
                RowKind::Dir { .. } => None,
            },
            |a, pos| {
                let complete = p >= 1.0;
                if caps.open_files_locally() {
                    let abs: PathBuf = Path::new(&details.summary.output_folder)
                        .join(file_tree::safe_relative(&details.files[pos].path));
                    let abs2 = abs.clone();
                    a.child(icon_button(
                        ("open", ix),
                        IconName::ExternalLink,
                        if complete { "Open" } else { "Open (not complete yet)" },
                        !complete,
                        cx,
                        move |_, _, cx| cx.open_with_system(&abs),
                    ))
                    .child(icon_button(
                        ("show", ix),
                        IconName::FolderOpen,
                        "Show in folder",
                        false,
                        cx,
                        move |_, _, cx| cx.reveal_path(&abs2),
                    ))
                } else if caps.download_to_this_pc() {
                    a.child(match job.map(|j| (&j.state, j)) {
                        Some((CopyState::Queued, _)) => status_icon(
                            ("cq", ix),
                            Icon::new(IconName::Ellipsis).small().text_color(muted).into_any_element(),
                            "Queued".into(),
                        ),
                        Some((CopyState::Running { bytes }, j)) => status_icon(
                            ("cr", ix),
                            Spinner::new().small().color(theme.progress_bar).into_any_element(),
                            format!(
                                "Copying to this PC… {}",
                                format::percent(if j.len == 0 { 0.0 } else { *bytes as f64 / j.len as f64 })
                            )
                            .into(),
                        ),
                        Some((CopyState::Done, j)) => {
                            let dest = j.dest.clone();
                            icon_button_colored(
                                ("cd", ix),
                                IconName::CircleCheck,
                                theme.success,
                                format!("Saved to {}. Click to show it.", j.dest.display()),
                                cx,
                                move |_, _, cx| cx.reveal_path(&dest),
                            )
                        }
                        Some((CopyState::Failed(e), _)) => icon_button_colored(
                            ("cf", ix),
                            IconName::RefreshCw,
                            theme.danger,
                            format!("Failed: {e}. Click to retry."),
                            cx,
                            cx.listener(move |this, _, _, cx| this.retry_copy(pos, cx)),
                        ),
                        None => icon_button(
                            ("dl", ix),
                            IconName::ArrowDown,
                            if complete {
                                "Download to this PC…"
                            } else {
                                "Download to this PC (not complete on the server yet)"
                            },
                            !complete,
                            cx,
                            cx.listener(move |this, _, w, cx| this.download_one(pos, w, cx)),
                        ),
                    })
                } else {
                    a
                }
            },
        );

        let selected = checked != Check::Off && matches!(row.kind, RowKind::File { .. }) || checked == Check::On;
        Some(
            h_flex()
                .id(("file", ix))
                .w_full()
                .h(px(ROW_H))
                .px_3()
                .gap_3()
                .text_sm()
                .cursor_default()
                .when(selected, |d| d.bg(theme.list_active))
                .when(!selected, |d| d.hover(|s| s.bg(theme.list_hover)))
                .when(included == Check::Off, |d| d.text_color(muted))
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| this.click_row(ix, ev.modifiers(), cx)))
                .child(tri_checkbox(("check", ix), checked, cx).on_click(cx.listener(
                    move |this, ev: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.check_row(ix, ev.modifiers().shift, cx)
                    },
                )))
                .child(name)
                .child(div().w(px(W_SIZE)).text_right().text_xs().text_color(muted).child(format::bytes(len)))
                .child(progress)
                .child(prio)
                .child(get)
                .child(actions)
                .into_any_element(),
        )
    }

    fn render_peers(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(peers) = &self.peers else { return empty_state(IconName::Network, "Loading peers…", None, cx) };
        if peers.is_empty() {
            return empty_state(
                IconName::Network,
                "No connected peers",
                Some("Peers show up here once the swarm finds them."),
                cx,
            );
        }
        let theme = cx.theme().clone();
        let header = peer_cols([
            div().child("Address"),
            div().child("Client"),
            div().child("Via"),
            div().child("State"),
            div().child("Down"),
            div().child("Up"),
            div().child("Downloaded"),
            div().child("Uploaded"),
        ])
        .text_xs()
        .text_color(theme.muted_foreground)
        .border_b_1()
        .border_color(theme.border);
        let list = uniform_list(
            "peers",
            peers.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let Some(peers) = &this.peers else { return Vec::new() };
                let theme = cx.theme().clone();
                let mut order: Vec<&PeerInfo> = peers.iter().collect();
                order.sort_by(|a, b| {
                    peer_rank(&a.state)
                        .cmp(&peer_rank(&b.state))
                        .then(b.down_bps.cmp(&a.down_bps))
                        .then(b.downloaded_bytes.cmp(&a.downloaded_bytes))
                        .then(a.addr.cmp(&b.addr))
                });
                range
                    .filter_map(|ix| {
                        let p = order.get(ix)?;
                        let dot = match peer_rank(&p.state) {
                            0 => theme.success,
                            1 => theme.warning,
                            _ => theme.muted_foreground.opacity(0.5),
                        };
                        // Zero reads as a quiet dash so active peers stand out.
                        let num = |text: String, active: bool| {
                            div()
                                .font_features(peer_tabular())
                                .text_color(if active { theme.foreground } else { theme.muted_foreground })
                                .child(if active { text } else { "—".to_owned() })
                        };
                        let detail = peer_detail(p);
                        Some(
                            peer_cols([
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
                                    .child(div().truncate().font_family(MONO_FONT).text_xs().child(p.addr.clone())),
                                div().truncate().child(p.client.clone().unwrap_or_else(|| "—".into())),
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(p.transport.clone().unwrap_or_default().to_uppercase()),
                                div().text_color(theme.muted_foreground).child(capitalize(&p.state)),
                                num(format::speed(p.down_bps), p.down_bps > 0),
                                num(format::speed(p.up_bps), p.up_bps > 0),
                                num(format::bytes(p.downloaded_bytes), p.downloaded_bytes > 0),
                                num(format::bytes(p.uploaded_bytes), p.uploaded_bytes > 0),
                            ])
                            .id(("peer", ix))
                            .text_sm()
                            .hover(|s| s.bg(theme.list_hover))
                            .when_some(detail, |row, text| {
                                row.tooltip(move |w, cx| Tooltip::new(text.clone()).build(w, cx))
                            })
                            .into_any_element(),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.peers_scroll)
        .flex_1();
        v_flex().size_full().child(header).child(list).into_any_element()
    }
}

fn is_complete(f: &TorrentFile) -> bool {
    f.progress_bytes >= f.length
}

/// Live peers first, then ones being connected, then the rest.
fn peer_rank(state: &str) -> u8 {
    match state.to_ascii_lowercase().as_str() {
        "live" => 0,
        "connecting" | "queued" => 1,
        _ => 2,
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Peers table layout: address, client, transport, state, then four right-aligned
/// numeric columns (down/up speed, downloaded/uploaded totals).
fn peer_cols(cells: [Div; 8]) -> Div {
    let [addr, client, via, state, down, up, downloaded, uploaded] = cells;
    let num = |w: f32, cell: Div| div().w(px(w)).flex_none().text_right().truncate().child(cell);
    h_flex()
        .w_full()
        .h(px(ROW_H))
        .flex_none()
        .px_3()
        .gap_3()
        .child(div().w(px(220.)).flex_none().min_w_0().overflow_hidden().child(addr))
        .child(div().flex_1().min_w_0().overflow_hidden().child(client))
        .child(div().w(px(40.)).flex_none().child(via))
        .child(div().w(px(80.)).flex_none().truncate().child(state))
        .child(num(84., down))
        .child(num(84., up))
        .child(num(84., downloaded))
        .child(num(84., uploaded))
}

/// Tabular figures so the numeric peer columns line up digit for digit.
fn peer_tabular() -> FontFeatures {
    FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}

/// Hover text with the per-peer counters that don't get their own column.
fn peer_detail(p: &PeerInfo) -> Option<SharedString> {
    let mut parts = Vec::new();
    if p.pieces_downloaded > 0 {
        let s = if p.pieces_downloaded == 1 { "" } else { "s" };
        parts.push(format!("{} verified piece{s} received", p.pieces_downloaded));
    }
    if p.errors > 0 {
        let s = if p.errors == 1 { "" } else { "s" };
        parts.push(format!("{} connection error{s}", p.errors));
    }
    (!parts.is_empty()).then(|| parts.join(" · ").into())
}

fn speeds(t: &TorrentSummary, cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_3()
        .child(ui::icon_label(IconName::ArrowDown, format::speed_always(t.down_bps), Some(muted)))
        .child(ui::icon_label(IconName::ArrowUp, format::speed_always(t.up_bps), Some(muted)))
        .when_some(Some(format::eta(t.eta_secs)).filter(|e| !e.is_empty()), |row, eta| {
            row.child(div().text_color(muted).child(format!("{eta} left")))
        })
}

/// General tab's Progress value: a bar of the pieces, then the numbers.
fn progress_row(t: &TorrentSummary, d: Option<&TorrentDetails>, cx: &App) -> impl IntoElement {
    let p = t.progress();
    let have = d.and_then(|d| {
        let (map, count) = (d.have_pieces.as_ref()?, d.piece_count?);
        Some((0..count).map(|i| map.has(i)).collect())
    });
    h_flex().gap_2().child(div().w(px(160.)).child(piece_bar(p, have, cx))).child(format!(
        "{} of {} · {} · {}",
        format::bytes(t.progress_bytes),
        format::bytes(t.total_bytes),
        format::percent(p),
        format::state(t.state)
    ))
}

/// A thin progress bar. While incomplete and the pieces are known (`have`, one
/// flag per piece), it shows which parts are downloaded rather than just how much.
fn piece_bar(fraction: f64, have: Option<Vec<bool>>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let color = if fraction >= 1.0 { theme.success } else { theme.progress_bar };
    match have {
        Some(have) if fraction < 1.0 => div()
            .w_full()
            .h(px(4.))
            .rounded_full()
            .overflow_hidden()
            .child(piece_map::strip(have, color, theme.muted_foreground.opacity(0.15)))
            .into_any_element(),
        _ => thin_bar(fraction, color, cx).into_any_element(),
    }
}

/// A column of key/value rows (General tab).
fn column() -> Div {
    v_flex().flex_1().min_w_0().gap_1()
}

/// The priority shared by all files at `positions`, or `None` if they differ.
fn common_priority(files: &[TorrentFile], positions: &[usize]) -> Option<FilePriority> {
    let mut it = positions.iter().filter_map(|p| files.get(*p)).map(|f| f.priority);
    let first = it.next()?;
    it.all(|p| p == first).then_some(first)
}

fn kv(label: &'static str, value: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex().min_h(px(24.)).child(ui::field(label, value, cx))
}

/// 4px rounded progress bar on a faint track.
fn thin_bar(fraction: f64, color: Hsla, cx: &App) -> impl IntoElement {
    let f = fraction.clamp(0.0, 1.0) as f32;
    div()
        .w_full()
        .h(px(4.))
        .rounded_full()
        .bg(cx.theme().muted_foreground.opacity(0.15))
        .child(div().h_full().rounded_full().bg(color).w(relative(f)))
}

/// Checkbox with a third "some inside" state (gpui-component's has only two).
fn tri_checkbox(id: impl Into<ElementId>, state: Check, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let on = state != Check::Off;
    div()
        .id(id)
        .flex_none()
        .size(px(W_CHECK))
        .rounded(px(4.))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .border_color(if on { theme.primary } else { theme.input })
        .when(on, |d| d.bg(theme.primary))
        .when(!on, |d| d.hover(|s| s.border_color(theme.primary)))
        .when(on, |d| {
            d.child(
                Icon::new(if state == Check::On { IconName::Check } else { IconName::Minus })
                    .with_size(px(12.))
                    .text_color(theme.primary_foreground),
            )
        })
}

/// Compact ghost icon button (22px) with a tooltip, vertically centred in a row.
fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tip: impl Into<SharedString>,
    disabled: bool,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let color = if disabled { cx.theme().muted_foreground.opacity(0.4) } else { cx.theme().muted_foreground };
    icon_button_inner(id, icon, color, tip.into(), disabled, cx, on_click)
}

fn icon_button_colored(
    id: impl Into<ElementId>,
    icon: IconName,
    color: Hsla,
    tip: impl Into<SharedString>,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    icon_button_inner(id, icon, color, tip.into(), false, cx, on_click)
}

fn icon_button_inner(
    id: impl Into<ElementId>,
    icon: IconName,
    color: Hsla,
    tip: SharedString,
    disabled: bool,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover = cx.theme().secondary_hover;
    div()
        .id(id)
        .flex_none()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .child(Icon::new(icon).small().text_color(color))
        .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
        .when(!disabled, |d| {
            d.cursor_pointer().hover(|s| s.bg(hover)).on_click(move |ev, w, cx| {
                cx.stop_propagation();
                on_click(ev, w, cx)
            })
        })
        .when(disabled, |d| d.on_click(|_, _, cx| cx.stop_propagation()))
        .into_any_element()
}

/// Non-interactive status glyph with a tooltip, sized like [`icon_button`].
fn status_icon(id: impl Into<ElementId>, glyph: AnyElement, tip: SharedString) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .child(glyph)
        .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
        .into_any_element()
}

fn empty_state(icon: IconName, title: impl Into<SharedString>, hint: Option<&'static str>, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(Icon::new(icon).large().text_color(muted.opacity(0.6)))
        .child(div().text_sm().text_color(muted).child(title.into()))
        .children(hint.map(|h| div().text_xs().text_color(muted.opacity(0.8)).child(h)))
        .into_any_element()
}

/// Local folder suggested by the "Download to this PC" save dialog.
fn local_downloads_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(Path::to_path_buf))
        .or_else(|| directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Render for DetailsPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = match (self.selection.len(), self.current()) {
            (0, _) => {
                empty_state(IconName::Inbox, "No torrent selected", Some("Select a torrent to see its details."), cx)
            }
            (n, None) => empty_state(
                IconName::Inbox,
                format!("{n} torrents selected"),
                Some("Select a single torrent to see its details."),
                cx,
            ),
            (_, Some(_)) => match self.tab {
                DetailsTab::General => self.render_general(cx),
                DetailsTab::Files => self.render_files(cx),
                DetailsTab::Peers => self.render_peers(cx),
            },
        };
        let current = self.current();
        let n_files = current.and(self.details.as_ref()).map(|d| d.files.len()).filter(|n| *n > 0);
        let n_peers = current.and_then(|id| self.state.read(cx).torrent(id).map(|t| t.peers_live)).filter(|n| *n > 0);
        let label = |name: &str, n: Option<String>| match n {
            Some(n) => format!("{name} {n}"),
            None => name.to_owned(),
        };
        let selected_ix = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .px_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        TabBar::new("details-tabs")
                            .underline()
                            .small()
                            .selected_index(selected_ix)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| this.set_tab(TABS[*ix], cx)))
                            .child(Tab::new().label("General"))
                            .child(Tab::new().label(label("Files", n_files.map(|n| n.to_string()))))
                            .child(Tab::new().label(label("Peers", n_peers.map(|n| n.to_string())))),
                    )
                    .child(div().flex_1())
                    .when_some(self.error.clone(), |row, e| {
                        row.child(
                            h_flex()
                                .gap_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.danger)
                                .child(Icon::new(IconName::CircleAlert).xsmall())
                                .child(div().truncate().child(e)),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().overflow_hidden().child(body))
    }
}
