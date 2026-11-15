//! The main torrent list: a virtualized, sortable `DataTable` with our own
//! multi-selection (click / Ctrl-click / Shift-click).

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use storm_proto::{TorrentId, TorrentState, TorrentSummary};

use crate::app::{
    CopyInfoHash, CopyMagnet, OpenFolder, PauseSelected, RemoveSelected, RemoveSelectedWithData, ResetColumns,
    ResumeSelected, ToggleColumn,
};
use crate::format;
use crate::state::{Filter, matches_query};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Name,
    Size,
    Progress,
    Status,
    Down,
    Up,
    Eta,
    Peers,
    Ratio,
    Added,
}

const COLUMNS: [Col; 10] = [
    Col::Name,
    Col::Size,
    Col::Progress,
    Col::Status,
    Col::Down,
    Col::Up,
    Col::Eta,
    Col::Peers,
    Col::Ratio,
    Col::Added,
];

/// The Name column never gets narrower than this when filling the table.
const NAME_MIN_WIDTH: Pixels = px(160.);

impl Col {
    fn column(self, width: Pixels) -> Column {
        // Sorting is driven by our own header (see `render_th`), so the table
        // doesn't draw a sort icon on every column.
        let c = Column::new(self.key(), self.title()).width(width);
        match self {
            Col::Name => c.min_width(NAME_MIN_WIDTH),
            _ if self.right_aligned() => c.text_right(),
            _ => c,
        }
    }

    /// Position in [`COLUMNS`], which lists the variants in declaration order.
    fn ix(self) -> usize {
        self as usize
    }

    fn default_width(self) -> Pixels {
        px(match self {
            Col::Name => 360.,
            Col::Size => 88.,
            Col::Progress => 170.,
            Col::Status => 120.,
            Col::Down | Col::Up => 96.,
            Col::Eta | Col::Peers => 76.,
            Col::Ratio => 60.,
            Col::Added => 130.,
        })
    }

    /// Stable identifier, used as the column key and in `prefs.json`.
    pub fn key(self) -> &'static str {
        match self {
            Col::Name => "name",
            Col::Size => "size",
            Col::Progress => "progress",
            Col::Status => "status",
            Col::Down => "down",
            Col::Up => "up",
            Col::Eta => "eta",
            Col::Peers => "peers",
            Col::Ratio => "ratio",
            Col::Added => "added",
        }
    }

    pub fn from_key(key: &str) -> Option<Col> {
        COLUMNS.into_iter().find(|c| c.key() == key)
    }

    fn title(self) -> &'static str {
        match self {
            Col::Name => "Name",
            Col::Size => "Size",
            Col::Progress => "Progress",
            Col::Status => "Status",
            Col::Down => "Down",
            Col::Up => "Up",
            Col::Eta => "ETA",
            Col::Peers => "Peers",
            Col::Ratio => "Ratio",
            Col::Added => "Added",
        }
    }

    fn right_aligned(self) -> bool {
        matches!(self, Col::Size | Col::Down | Col::Up | Col::Eta | Col::Peers | Col::Ratio)
    }

    /// First click on a numeric column sorts biggest first; names/dates ascend.
    fn first_sort(self) -> ColumnSort {
        match self {
            Col::Name | Col::Status | Col::Eta => ColumnSort::Ascending,
            _ => ColumnSort::Descending,
        }
    }
}

/// Next sort state when a header is clicked: first direction, the other one, then off.
fn cycle_sort(current: Option<(Col, ColumnSort)>, col: Col) -> Option<(Col, ColumnSort)> {
    let flip = |s| if s == ColumnSort::Ascending { ColumnSort::Descending } else { ColumnSort::Ascending };
    match current {
        Some((c, s)) if c == col && s == col.first_sort() => Some((col, flip(s))),
        Some((c, _)) if c == col => None,
        _ => Some((col, col.first_sort())),
    }
}

/// Tabular figures so numbers in a column line up digit for digit.
fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// The icon and colour that stand for a torrent's state, shared with the filter strip.
pub fn state_icon(t: &TorrentSummary, cx: &App) -> (IconName, Hsla) {
    let theme = cx.theme();
    match t.state {
        TorrentState::Downloading => (IconName::ArrowDown, theme.primary),
        TorrentState::Seeding => (IconName::ArrowUp, theme.success),
        TorrentState::Paused if t.total_bytes > 0 && t.progress() >= 1.0 => {
            (IconName::CircleCheck, theme.muted_foreground)
        }
        TorrentState::Paused => (IconName::Pause, theme.muted_foreground),
        TorrentState::FetchingMetadata | TorrentState::Initializing => (IconName::LoaderCircle, theme.warning),
        TorrentState::Error => (IconName::CircleAlert, theme.danger),
    }
}

pub fn display_name(t: &TorrentSummary) -> String {
    t.name.clone().unwrap_or_else(|| format!("Fetching metadata… ({})", &t.info_hash[..t.info_hash.len().min(12)]))
}

/// Ascending comparison of two torrents on a column. Missing values sort last.
pub fn compare(a: &TorrentSummary, b: &TorrentSummary, col: Col) -> Ordering {
    fn opt<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
        match (a, b) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
    let ord = match col {
        Col::Name => display_name(a).to_lowercase().cmp(&display_name(b).to_lowercase()),
        Col::Size => a.total_bytes.cmp(&b.total_bytes),
        Col::Progress => a.progress().total_cmp(&b.progress()),
        Col::Status => state_rank(a.state).cmp(&state_rank(b.state)),
        Col::Down => a.down_bps.cmp(&b.down_bps),
        Col::Up => a.up_bps.cmp(&b.up_bps),
        Col::Eta => opt(a.eta_secs, b.eta_secs),
        Col::Peers => (a.peers_live, a.peers_seen).cmp(&(b.peers_live, b.peers_seen)),
        Col::Ratio => a.ratio().total_cmp(&b.ratio()),
        Col::Added => opt(a.added_at, b.added_at),
    };
    // Stable, deterministic tie-break so rows don't jitter between snapshots.
    ord.then(a.id.cmp(&b.id))
}

fn state_rank(s: TorrentState) -> u8 {
    match s {
        TorrentState::Downloading => 0,
        TorrentState::FetchingMetadata | TorrentState::Initializing => 1,
        TorrentState::Seeding => 2,
        TorrentState::Paused => 3,
        TorrentState::Error => 4,
    }
}

/// Pure multi-selection model, keyed by torrent id so it survives re-sorting.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub ids: BTreeSet<TorrentId>,
    /// Where Shift-click ranges start from.
    anchor: Option<TorrentId>,
    /// The row the keyboard moves from (the last one clicked or moved to).
    cursor: Option<TorrentId>,
}

impl Selection {
    /// Applies a click on `row` of `order` (the ids as displayed).
    pub fn click(&mut self, order: &[TorrentId], row: usize, toggle: bool, range: bool) {
        let Some(&id) = order.get(row) else {
            // Click on empty space below the rows.
            if !toggle && !range {
                self.clear();
            }
            return;
        };
        self.cursor = Some(id);
        let anchor_ix = self.anchor.and_then(|a| order.iter().position(|&x| x == a));
        match (range, anchor_ix) {
            (true, Some(a)) => {
                let (lo, hi) = if a <= row { (a, row) } else { (row, a) };
                if !toggle {
                    self.ids.clear();
                }
                self.ids.extend(&order[lo..=hi]);
            }
            _ if toggle => {
                if !self.ids.remove(&id) {
                    self.ids.insert(id);
                }
                self.anchor = Some(id);
            }
            _ => {
                self.ids.clear();
                self.ids.insert(id);
                self.anchor = Some(id);
            }
        }
    }

    pub fn select_all(&mut self, order: &[TorrentId]) {
        self.ids = order.iter().copied().collect();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Where the keyboard cursor is in `order`, if it's still shown.
    pub fn cursor_row(&self, order: &[TorrentId]) -> Option<usize> {
        let find = |id: Option<TorrentId>| id.and_then(|id| order.iter().position(|&x| x == id));
        find(self.cursor).or_else(|| find(self.anchor))
    }

    /// Keyboard movement to `row`: selects just that row, or with `extend`
    /// (Shift) everything from the anchor to it.
    pub fn move_to(&mut self, order: &[TorrentId], row: usize, extend: bool) {
        let Some(&id) = order.get(row) else { return };
        let anchor_ix = self.anchor.and_then(|a| order.iter().position(|&x| x == a));
        match (extend, anchor_ix) {
            (true, Some(a)) => {
                let (lo, hi) = if a <= row { (a, row) } else { (row, a) };
                self.ids = order[lo..=hi].iter().copied().collect();
            }
            _ => {
                self.ids = BTreeSet::from([id]);
                self.anchor = Some(id);
            }
        }
        self.cursor = Some(id);
    }

    /// Drops ids that no longer exist.
    pub fn retain(&mut self, order: &[TorrentId]) {
        let present: BTreeSet<_> = order.iter().copied().collect();
        self.ids.retain(|id| present.contains(id));
        if self.anchor.is_some_and(|a| !present.contains(&a)) {
            self.anchor = None;
        }
        if self.cursor.is_some_and(|c| !present.contains(&c)) {
            self.cursor = None;
        }
    }
}

pub struct TorrentTableDelegate {
    /// Everything the backend reported.
    all: Vec<TorrentSummary>,
    /// What's displayed: `all` filtered and sorted.
    rows: Vec<TorrentSummary>,
    filter: Filter,
    query: String,
    sort: Option<(Col, ColumnSort)>,
    pub selection: Selection,
    /// Where context-menu actions are dispatched (the app's focus handle).
    pub action_context: Option<FocusHandle>,
    /// Offer "Open folder" (the backend shares our filesystem).
    pub can_open_folder: bool,
    /// The columns shown, in [`COLUMNS`] order; Name always is, and comes first.
    visible: Vec<Col>,
    /// Current column widths (the user may drag them), in [`COLUMNS`] order.
    /// Hidden columns keep theirs for when they're shown again.
    widths: [Pixels; COLUMNS.len()],
    /// The table's width at the last fit; Name takes what the others leave.
    fit_width: Option<Pixels>,
}

/// A keyboard move in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
    PageUp(usize),
    PageDown(usize),
    First,
    Last,
}

impl TorrentTableDelegate {
    pub fn new() -> Self {
        Self {
            all: Vec::new(),
            rows: Vec::new(),
            filter: Filter::All,
            query: String::new(),
            sort: None,
            selection: Selection::default(),
            action_context: None,
            can_open_folder: false,
            visible: COLUMNS.to_vec(),
            widths: COLUMNS.map(Col::default_width),
            fit_width: None,
        }
    }

    pub fn sort(&self) -> Option<(Col, ColumnSort)> {
        self.sort
    }

    pub fn set_sort(&mut self, sort: Option<(Col, ColumnSort)>) {
        self.sort = sort;
        self.apply_sort();
    }

    /// Whether [`Self::fit_name`] would change anything for a table `width` wide.
    pub fn needs_fit(&self, width: Pixels) -> bool {
        self.fit_width != Some(width)
    }

    /// Makes the Name column fill whatever the other columns leave of a table
    /// `width` wide. Returns whether its width changed (the table then needs a
    /// `refresh`).
    pub fn fit_name(&mut self, width: Pixels) -> bool {
        self.fit_width = Some(width);
        self.refit()
    }

    /// Takes over the widths after the user dragged a column edge (given in
    /// displayed order), and refits Name to the rest. Returns whether the
    /// table needs a `refresh`.
    pub fn set_widths(&mut self, widths: &[Pixels]) -> bool {
        // Widths from before a column was shown or hidden would land on the wrong columns.
        if widths.len() != self.visible.len() {
            return false;
        }
        for (col, w) in self.visible.iter().zip(widths) {
            self.widths[col.ix()] = *w;
        }
        self.refit();
        // The table already shows what was dragged; only Name (displayed first) may differ.
        widths.first() != Some(&self.widths[Col::Name.ix()])
    }

    fn refit(&mut self) -> bool {
        let Some(width) = self.fit_width else { return false };
        let others: Pixels = self.visible.iter().filter(|&&c| c != Col::Name).map(|c| self.widths[c.ix()]).sum();
        // A pixel of slack so rounding never brings up a horizontal scrollbar.
        let name = (width - others - px(1.)).max(NAME_MIN_WIDTH).floor();
        let changed = self.widths[Col::Name.ix()] != name;
        self.widths[Col::Name.ix()] = name;
        changed
    }

    /// The column shown at `col_ix`. Falls back to Name rather than panicking
    /// should the table ask about a column it hasn't been refreshed past.
    fn col(&self, col_ix: usize) -> Col {
        self.visible.get(col_ix).copied().unwrap_or(Col::Name)
    }

    pub fn is_shown(&self, col: Col) -> bool {
        self.visible.contains(&col)
    }

    /// The hidden columns, for `prefs.json`.
    pub fn hidden_columns(&self) -> Vec<Col> {
        COLUMNS.into_iter().filter(|&c| !self.is_shown(c)).collect()
    }

    /// Hides exactly `hidden` (Name can't be). A sort on a column that ends up
    /// hidden is cleared: there'd be no header showing it, or to undo it.
    pub fn set_hidden_columns(&mut self, hidden: &[Col]) {
        self.visible = COLUMNS.into_iter().filter(|c| *c == Col::Name || !hidden.contains(c)).collect();
        if self.sort.is_some_and(|(c, _)| !self.is_shown(c)) {
            self.set_sort(None);
        }
        self.refit();
    }

    /// Shows or hides `col` (Name stays). Returns whether anything changed
    /// (the table then needs a `refresh`).
    pub fn toggle_column(&mut self, col: Col) -> bool {
        if col == Col::Name {
            return false;
        }
        let mut hidden = self.hidden_columns();
        match hidden.iter().position(|&c| c == col) {
            Some(i) => _ = hidden.remove(i),
            None => hidden.push(col),
        }
        self.set_hidden_columns(&hidden);
        true
    }

    /// Shows every column at its default width. Returns whether anything changed.
    pub fn reset_columns(&mut self) -> bool {
        let changed = !self.columns_are_default();
        self.visible = COLUMNS.to_vec();
        self.widths = COLUMNS.map(Col::default_width);
        self.refit();
        changed
    }

    /// All columns shown at their default widths (Name's is fitted, so it doesn't count).
    fn columns_are_default(&self) -> bool {
        self.visible.len() == COLUMNS.len()
            && COLUMNS.iter().all(|&c| c == Col::Name || self.widths[c.ix()] == c.default_width())
    }

    /// The header's right-click menu: a checkable entry per column, then a reset.
    fn columns_menu(&self, menu: PopupMenu) -> PopupMenu {
        let menu = match &self.action_context {
            Some(h) => menu.action_context(h.clone()),
            None => menu,
        };
        COLUMNS
            .into_iter()
            .fold(menu.min_w(px(200.)), |menu, col| {
                let (shown, fixed) = (self.is_shown(col), col == Col::Name);
                menu.menu_with_check_and_disabled(col.title(), shown, Box::new(ToggleColumn(col)), fixed)
            })
            .separator()
            .menu_with_disabled("Reset columns", Box::new(ResetColumns), self.columns_are_default())
    }

    /// Moves the keyboard cursor (and the selection with it). Returns the row
    /// moved to, to scroll it into view.
    pub fn move_cursor(&mut self, mv: Move, extend: bool) -> Option<usize> {
        let order = self.order();
        let last = order.len().checked_sub(1)?;
        let row = match (mv, self.selection.cursor_row(&order)) {
            (Move::First, _) => 0,
            (Move::Last, _) => last,
            (Move::Up, Some(r)) => r.saturating_sub(1),
            (Move::Down, Some(r)) => (r + 1).min(last),
            (Move::PageUp(n), Some(r)) => r.saturating_sub(n.max(1)),
            (Move::PageDown(n), Some(r)) => (r + n.max(1)).min(last),
            // Nothing selected yet: going down starts at the top, going up at the bottom.
            (Move::Down | Move::PageDown(_), None) => 0,
            (Move::Up | Move::PageUp(_), None) => last,
        };
        self.selection.move_to(&order, row, extend);
        Some(row)
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    pub fn set_rows(&mut self, rows: Vec<TorrentSummary>) {
        self.all = rows;
        self.refresh();
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.refresh();
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_owned();
        self.refresh();
    }

    /// Re-applies filter, search and sort, and drops selected ids that went out of view.
    fn refresh(&mut self) {
        let (filter, query) = (self.filter, &self.query);
        self.rows = self.all.iter().filter(|t| filter.matches(t) && matches_query(t, query)).cloned().collect();
        self.apply_sort();
        let order = self.order();
        self.selection.retain(&order);
    }

    fn order(&self) -> Vec<TorrentId> {
        self.rows.iter().map(|t| t.id).collect()
    }

    pub fn selected_ids(&self) -> Vec<TorrentId> {
        self.selection.ids.iter().copied().collect()
    }

    pub fn select_all(&mut self) {
        let order = self.order();
        self.selection.select_all(&order);
    }

    fn apply_sort(&mut self) {
        match self.sort {
            Some((col, ColumnSort::Ascending)) => self.rows.sort_by(|a, b| compare(a, b, col)),
            Some((col, ColumnSort::Descending)) => self.rows.sort_by(|a, b| compare(b, a, col)),
            _ => self.rows.sort_by_key(|t| t.id),
        }
    }

    fn selected_states(&self) -> Vec<TorrentState> {
        self.rows.iter().filter(|t| self.selection.ids.contains(&t.id)).map(|t| t.state).collect()
    }
}

impl TableDelegate for TorrentTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let col = self.col(col_ix);
        col.column(self.widths[col.ix()])
    }

    fn render_header(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let theme = cx.theme();
        // Right-clicking anywhere on the header (gaps and empty end included)
        // opens the column menu. The table lays its header cells out after this
        // child, over it; their hitboxes don't block it, and it stops the press
        // before it reaches the table's own (row) context menu. The menu is built
        // later, outside the table's update, so reading the table there is fine.
        let table = cx.entity().downgrade();
        let menu_area = div().id("header-menu").absolute().top_0().left_0().size_full().context_menu(
            move |menu, _, cx| match table.upgrade() {
                Some(table) => table.read(cx).delegate().columns_menu(menu),
                None => menu,
            },
        );
        div()
            .id("header")
            .bg(theme.background)
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.muted_foreground)
            .border_b_1()
            .border_color(theme.border.opacity(0.6))
            .child(menu_area)
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let col = self.col(col_ix);
        let active = self.sort.and_then(|(c, s)| (c == col && s != ColumnSort::Default).then_some(s));
        let theme = cx.theme();
        let fg = theme.foreground;
        let arrow = active.map(|s| {
            let icon = if s == ColumnSort::Ascending { IconName::ChevronUp } else { IconName::ChevronDown };
            Icon::new(icon).xsmall().text_color(fg)
        });
        let right = col.right_aligned();
        h_flex()
            .id(("th", col_ix))
            .w_full()
            .h_full()
            .gap_1()
            .cursor_pointer()
            .when(right, |d| d.justify_end())
            .when(active.is_some(), |d| d.text_color(fg))
            .hover(move |d| d.text_color(fg))
            .when(right, |d| d.children(arrow.clone()))
            .child(col.title())
            .when(!right, |d| d.children(arrow))
            .on_click(cx.listener(move |table, _, _, cx| {
                let d = table.delegate_mut();
                d.set_sort(cycle_sort(d.sort, col));
                crate::prefs::set_table_sort(cx, d.sort);
                cx.notify();
            }))
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let selected = self.rows.get(row_ix).is_some_and(|t| self.selection.ids.contains(&t.id));
        let theme = cx.theme();
        div()
            .id(("row", row_ix))
            .text_sm()
            // No grid lines: rows are separated by spacing and hover/selection tints.
            .border_color(transparent_black())
            .when(selected, |d| d.bg(theme.primary.opacity(0.14)))
            .on_click(cx.listener(move |table, ev: &ClickEvent, _, cx| {
                let m = ev.modifiers();
                let toggle = m.secondary(cx);
                let d = table.delegate_mut();
                let order = d.order();
                d.selection.click(&order, row_ix, toggle, m.shift);
                cx.emit(TableEvent::SelectRow(row_ix));
                // The app opens the download's folder on a plain double-click.
                if ev.click_count() == 2 && !toggle && !m.shift {
                    cx.emit(TableEvent::DoubleClickedRow(row_ix));
                }
                cx.notify();
            }))
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(id) = self.rows.get(row_ix).map(|t| t.id) else { return menu };
        // Right-clicking outside the selection selects just that row, like file managers do.
        if !self.selection.ids.contains(&id) {
            let order = self.order();
            self.selection.click(&order, row_ix, false, false);
            cx.emit(TableEvent::SelectRow(row_ix));
            cx.notify();
        }
        let states = self.selected_states();
        let can_pause = states.iter().any(|s| !matches!(s, TorrentState::Paused | TorrentState::FetchingMetadata));
        let can_resume = states.iter().any(|s| matches!(s, TorrentState::Paused | TorrentState::Error));
        let single = states.len() == 1;
        let menu = match &self.action_context {
            Some(h) => menu.action_context(h.clone()),
            None => menu,
        };
        menu.min_w(px(200.))
            .menu_with_icon_and_disabled("Resume", IconName::Play, Box::new(ResumeSelected), !can_resume)
            .menu_with_icon_and_disabled("Pause", IconName::Pause, Box::new(PauseSelected), !can_pause)
            .separator()
            .when(self.can_open_folder, |m| {
                m.menu_with_icon_and_disabled("Open folder", IconName::FolderOpen, Box::new(OpenFolder), !single)
            })
            .menu_with_icon("Copy magnet link", IconName::Copy, Box::new(CopyMagnet))
            .menu_with_icon("Copy info hash", IconName::Copy, Box::new(CopyInfoHash))
            .separator()
            .menu_with_icon("Remove…", IconName::Delete, Box::new(RemoveSelected))
            .menu_with_icon("Remove and delete files…", IconName::Delete, Box::new(RemoveSelectedWithData))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // The table's cell is a plain block with top/bottom padding, so content
        // would sit at the top of the row; center it vertically instead.
        h_flex().size_full().items_center().child(self.render_cell_content(row_ix, col_ix, window, cx))
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let Some(t) = self.rows.get(row_ix) else { return String::new() };
        match self.col(col_ix) {
            Col::Name => display_name(t),
            Col::Size => format::bytes(t.total_bytes),
            Col::Progress => format::percent(t.progress()),
            Col::Status => format::state(t.state).into(),
            Col::Down => format::speed(t.down_bps),
            Col::Up => format::speed(t.up_bps),
            Col::Eta => format::eta(t.eta_secs),
            Col::Peers => format!("{} / {}", t.peers_live, t.peers_seen),
            Col::Ratio => format::ratio(t.ratio()),
            Col::Added => format::date(t.added_at),
        }
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let theme = cx.theme();
        let (icon, title, hint) = if self.all.is_empty() {
            (
                IconName::Inbox,
                "Drop a .torrent or paste a magnet link",
                "Ctrl+V adds a link from the clipboard · Ctrl+O opens .torrent files",
            )
        } else {
            (IconName::Search, "Nothing matches", "No torrents match the current filter or search.")
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(52.))
                    .mb_1()
                    .rounded(px(14.))
                    .bg(theme.muted.opacity(0.6))
                    .child(Icon::new(icon).size(px(24.)).text_color(theme.muted_foreground)),
            )
            .child(div().text_base().font_weight(FontWeight::MEDIUM).text_color(theme.foreground).child(title))
            .child(div().text_sm().text_color(theme.muted_foreground).child(hint))
    }
}

impl TorrentTableDelegate {
    fn render_cell_content(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> AnyElement {
        let Some(t) = self.rows.get(row_ix) else { return div().into_any_element() };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let num = |s: String, color: Hsla| {
            div()
                .w_full()
                .text_right()
                .truncate()
                .font_features(tabular())
                .text_color(color)
                .child(s)
                .into_any_element()
        };
        match self.col(col_ix) {
            Col::Name => {
                let (icon, color) = state_icon(t, cx);
                h_flex()
                    .w_full()
                    .gap_2p5()
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .size(px(22.))
                            .rounded(px(6.))
                            .bg(color.opacity(0.14))
                            .child(Icon::new(icon).size(px(13.)).text_color(color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(t.name.is_none(), |d| d.text_color(muted).italic())
                            .child(display_name(t)),
                    )
                    .into_any_element()
            }
            Col::Size => num(format::bytes(t.total_bytes), muted),
            Col::Progress => {
                let p = t.progress().clamp(0.0, 1.0);
                let color = match t.state {
                    _ if p >= 1.0 => theme.success,
                    TorrentState::Error => theme.danger,
                    TorrentState::Paused => muted,
                    _ => theme.primary,
                };
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .h(px(4.))
                            .rounded_full()
                            .bg(theme.muted)
                            .child(div().h_full().rounded_full().bg(color).w(relative(p as f32))),
                    )
                    .child(
                        div()
                            .w(px(44.))
                            .flex_shrink_0()
                            .text_right()
                            .font_features(tabular())
                            .text_color(muted)
                            .child(format::percent(p)),
                    )
                    .into_any_element()
            }
            Col::Status => {
                let (text, color) = match (&t.error, t.state) {
                    (Some(e), TorrentState::Error) => (e.clone(), theme.danger),
                    (_, TorrentState::Error) => ("Error".into(), theme.danger),
                    (_, s) => (format::state(s).to_owned(), muted),
                };
                div().w_full().truncate().text_color(color).child(text).into_any_element()
            }
            Col::Down => num(format::speed(t.down_bps), theme.foreground),
            Col::Up => num(format::speed(t.up_bps), theme.foreground),
            Col::Eta => num(format::eta(t.eta_secs), muted),
            Col::Peers => {
                let text = if t.peers_live == 0 && t.peers_seen == 0 {
                    String::new()
                } else {
                    format!("{} / {}", t.peers_live, t.peers_seen)
                };
                num(text, muted)
            }
            Col::Ratio => num(format::ratio(t.ratio()), muted),
            Col::Added => {
                div().w_full().truncate().text_color(muted).child(format::short_date(t.added_at)).into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COLUMNS, Col, ColumnSort, Filter, Move, NAME_MIN_WIDTH, Ordering, Pixels, Selection, TorrentId, TorrentState,
        TorrentSummary, TorrentTableDelegate, compare, cycle_sort, px,
    };

    fn t(id: TorrentId, name: Option<&str>, total: u64, done: u64, eta: Option<u64>) -> TorrentSummary {
        TorrentSummary {
            id,
            info_hash: format!("{id:040}"),
            name: name.map(Into::into),
            state: TorrentState::Downloading,
            error: None,
            total_bytes: total,
            progress_bytes: done,
            uploaded_bytes: 0,
            downloaded_bytes: done,
            down_bps: 0,
            up_bps: 0,
            eta_secs: eta,
            peers_live: 0,
            peers_seen: 0,
            seeders_live: 0,
            output_folder: String::new(),
            added_at: Some(id as i64),
            completed_at: None,
        }
    }

    #[test]
    fn sorts_by_column() {
        let a = t(1, Some("beta"), 10, 5, Some(30));
        let b = t(2, Some("Alpha"), 20, 20, None);
        assert_eq!(compare(&a, &b, Col::Name), Ordering::Greater);
        assert_eq!(compare(&a, &b, Col::Size), Ordering::Less);
        assert_eq!(compare(&a, &b, Col::Progress), Ordering::Less);
        // Unknown ETA sorts after known ones.
        assert_eq!(compare(&a, &b, Col::Eta), Ordering::Less);
        // Ties break on id.
        assert_eq!(compare(&a, &a.clone(), Col::Size), Ordering::Equal);
        let c = t(3, Some("beta"), 10, 5, Some(30));
        assert_eq!(compare(&a, &c, Col::Name), Ordering::Less);
    }

    #[test]
    fn delegate_sorts_rows() {
        let mut d = TorrentTableDelegate::new();
        d.sort = Some((Col::Size, ColumnSort::Descending));
        d.set_rows(vec![t(1, None, 1, 0, None), t(2, None, 3, 0, None), t(3, None, 2, 0, None)]);
        assert_eq!(d.order(), vec![2, 3, 1]);
        d.sort = Some((Col::Size, ColumnSort::Default));
        d.apply_sort();
        assert_eq!(d.order(), vec![1, 2, 3]);
    }

    #[test]
    fn header_clicks_cycle_sort() {
        let s = cycle_sort(None, Col::Size);
        assert_eq!(s, Some((Col::Size, ColumnSort::Descending)));
        let s = cycle_sort(s, Col::Size);
        assert_eq!(s, Some((Col::Size, ColumnSort::Ascending)));
        assert_eq!(cycle_sort(s, Col::Size), None);
        assert_eq!(cycle_sort(s, Col::Name), Some((Col::Name, ColumnSort::Ascending)));
    }

    #[test]
    fn filter_and_search_narrow_rows_and_selection() {
        let mut d = TorrentTableDelegate::new();
        let mut paused = t(2, Some("Sintel"), 10, 10, None);
        paused.state = TorrentState::Paused;
        d.set_rows(vec![t(1, Some("Big Buck Bunny"), 10, 5, None), paused, t(3, Some("Sintel trailer"), 10, 1, None)]);
        d.select_all();
        d.set_filter(Filter::Paused);
        assert_eq!(d.order(), vec![2]);
        assert_eq!(d.selected_ids(), vec![2]);
        d.set_filter(Filter::All);
        d.set_query("sintel");
        assert_eq!(d.order(), vec![2, 3]);
        d.set_query("");
        assert_eq!(d.order(), vec![1, 2, 3]);
    }

    #[test]
    fn selection_click_toggle_range() {
        let order = [10, 20, 30, 40];
        let mut s = Selection::default();
        s.click(&order, 1, false, false);
        assert_eq!(s.ids.iter().copied().collect::<Vec<_>>(), vec![20]);
        s.click(&order, 3, false, true);
        assert_eq!(s.ids.iter().copied().collect::<Vec<_>>(), vec![20, 30, 40]);
        s.click(&order, 2, true, false);
        assert_eq!(s.ids.iter().copied().collect::<Vec<_>>(), vec![20, 40]);
        // Ctrl+Shift extends without clearing; anchor is now 30.
        s.click(&order, 0, true, true);
        assert_eq!(s.ids.iter().copied().collect::<Vec<_>>(), vec![10, 20, 30, 40]);
        // Plain click on empty space clears.
        s.click(&order, 99, false, false);
        assert!(s.ids.is_empty());
    }

    #[test]
    fn keyboard_moves_and_extends_selection() {
        let mut d = TorrentTableDelegate::new();
        d.set_rows((1..=5).map(|id| t(id, None, 1, 0, None)).collect());
        // Nothing selected: Down starts at the top, Up at the bottom.
        assert_eq!(d.move_cursor(Move::Down, false), Some(0));
        assert_eq!(d.selected_ids(), vec![1]);
        d.clear_selection();
        assert_eq!(d.move_cursor(Move::Up, false), Some(4));
        // Moves clamp at the ends.
        assert_eq!(d.move_cursor(Move::Down, false), Some(4));
        assert_eq!(d.move_cursor(Move::First, false), Some(0));
        assert_eq!(d.move_cursor(Move::Up, false), Some(0));
        // Shift extends from the anchor, and shrinks back when reversing.
        d.move_cursor(Move::Down, true);
        d.move_cursor(Move::Down, true);
        assert_eq!(d.selected_ids(), vec![1, 2, 3]);
        d.move_cursor(Move::Up, true);
        assert_eq!(d.selected_ids(), vec![1, 2]);
        d.move_cursor(Move::Last, true);
        assert_eq!(d.selected_ids(), vec![1, 2, 3, 4, 5]);
        // A plain move collapses to the cursor row.
        assert_eq!(d.move_cursor(Move::PageUp(2), false), Some(2));
        assert_eq!(d.selected_ids(), vec![3]);
        // Moving continues from a clicked row.
        let order = d.order();
        d.selection.click(&order, 3, false, false);
        assert_eq!(d.move_cursor(Move::Down, false), Some(4));
    }

    #[test]
    fn name_column_fills_the_rest() {
        let mut d = TorrentTableDelegate::new();
        let others: Pixels = COLUMNS[1..].iter().map(|c| c.default_width()).sum();
        assert!(d.fit_name(others + px(500.)));
        assert_eq!(d.widths[0], px(499.));
        assert!(!d.needs_fit(others + px(500.)));
        assert!(!d.fit_name(others + px(500.)));
        // Never narrower than the minimum.
        assert!(d.fit_name(px(200.)));
        assert_eq!(d.widths[0], NAME_MIN_WIDTH);
        // Dragging another column wider takes the room from Name.
        d.fit_name(others + px(500.));
        let mut dragged = d.widths.to_vec();
        dragged[1] += px(50.);
        assert!(d.set_widths(&dragged));
        assert_eq!(d.widths[0], px(449.));
        assert_eq!(d.widths[1], Col::Size.default_width() + px(50.));
    }

    #[test]
    fn column_keys_round_trip() {
        for c in COLUMNS {
            assert_eq!(Col::from_key(c.key()), Some(c));
            assert_eq!(COLUMNS[c.ix()], c);
        }
        assert_eq!(Col::from_key("nope"), None);
    }

    #[test]
    fn hiding_columns_maps_displayed_indices() {
        let mut d = TorrentTableDelegate::new();
        assert!(d.toggle_column(Col::Size));
        assert!(d.toggle_column(Col::Eta));
        assert_eq!(d.hidden_columns(), vec![Col::Size, Col::Eta]);
        assert_eq!(d.visible.len(), COLUMNS.len() - 2);
        assert_eq!((d.col(0), d.col(1), d.col(5)), (Col::Name, Col::Progress, Col::Peers));
        // Showing again puts it back in its place, not at the end.
        assert!(d.toggle_column(Col::Size));
        assert_eq!(d.col(1), Col::Size);
        // Name can't be hidden; it's always first.
        assert!(!d.toggle_column(Col::Name));
        d.set_hidden_columns(&[Col::Name, Col::Up]);
        assert_eq!(d.hidden_columns(), vec![Col::Up]);
        assert_eq!(d.col(0), Col::Name);
        assert!(d.reset_columns());
        assert!(d.hidden_columns().is_empty());
        assert!(!d.reset_columns());
    }

    #[test]
    fn hiding_the_sorted_column_clears_the_sort() {
        let mut d = TorrentTableDelegate::new();
        d.set_sort(Some((Col::Size, ColumnSort::Descending)));
        d.toggle_column(Col::Eta);
        assert_eq!(d.sort(), Some((Col::Size, ColumnSort::Descending)));
        d.toggle_column(Col::Size);
        assert_eq!(d.sort(), None);
        // Also when restoring a saved sort on a column that's saved as hidden.
        d.set_sort(Some((Col::Size, ColumnSort::Ascending)));
        d.set_hidden_columns(&d.hidden_columns());
        assert_eq!(d.sort(), None);
    }

    #[test]
    fn name_fills_the_rest_with_hidden_columns_and_widths_survive() {
        let mut d = TorrentTableDelegate::new();
        let others: Pixels = COLUMNS[1..].iter().map(|c| c.default_width()).sum();
        d.fit_name(others + px(500.));
        assert_eq!(d.widths[0], px(499.));
        // Hiding a column gives its width to Name, showing it takes it back.
        d.toggle_column(Col::Progress);
        assert_eq!(d.widths[0], px(499.) + Col::Progress.default_width());
        d.toggle_column(Col::Progress);
        assert_eq!(d.widths[0], px(499.));

        // Drag Status (displayed at 2 with Progress hidden) wider; widths come in displayed order.
        d.toggle_column(Col::Progress);
        let mut dragged: Vec<Pixels> = d.visible.iter().map(|c| d.widths[c.ix()]).collect();
        assert_eq!(d.col(2), Col::Status);
        dragged[2] += px(40.);
        assert!(d.set_widths(&dragged));
        assert_eq!(d.widths[Col::Status.ix()], Col::Status.default_width() + px(40.));
        assert_eq!(d.widths[Col::Progress.ix()], Col::Progress.default_width());
        assert_eq!(d.widths[0], px(499.) + Col::Progress.default_width() - px(40.));
        // Widths from before the columns changed are ignored.
        assert!(!d.set_widths(&COLUMNS.map(Col::default_width)));
        assert_eq!(d.widths[Col::Status.ix()], Col::Status.default_width() + px(40.));

        // Shown again, Progress has its old width and Status keeps its dragged one.
        d.toggle_column(Col::Progress);
        assert_eq!(d.widths[Col::Progress.ix()], Col::Progress.default_width());
        assert_eq!(d.widths[0], px(459.));
        // Reset puts the default widths back.
        assert!(d.reset_columns());
        assert_eq!(d.widths[Col::Status.ix()], Col::Status.default_width());
        assert_eq!(d.widths[0], px(499.));
    }

    #[test]
    fn selection_survives_resort_and_prunes_removed() {
        let mut s = Selection::default();
        s.click(&[1, 2, 3], 1, false, false);
        s.retain(&[3, 2]);
        assert!(s.ids.contains(&2));
        s.retain(&[3]);
        assert!(s.ids.is_empty());
        s.click(&[3], 0, false, true); // shift with no anchor = plain click
        assert!(s.ids.contains(&3));
    }
}
