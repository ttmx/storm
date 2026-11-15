//! Client-side preferences (as opposed to engine settings, which live in the backend).
//!
//! Stored as `prefs.json` next to `remote.json` in the app's config dir.

use std::path::PathBuf;

use gpui::{App, Global};
use gpui_kit::component::table::ColumnSort;
use serde::{Deserialize, Serialize};

use crate::state::Filter;
use crate::views::torrent_table::Col;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Offer to add magnet links / `.torrent` URLs found on the clipboard.
    pub watch_clipboard: bool,
    /// Check GitHub for new releases and install them in the background.
    pub auto_update: bool,
    /// The active quick filter, by key (see [`Filter::key`]).
    pub filter: String,
    /// The torrent list's sort column (see [`Col::key`]); `None` = order added.
    pub sort_column: Option<String>,
    pub sort_descending: bool,
    /// Columns hidden from the torrent list (see [`Col::key`]). Hidden rather
    /// than shown, so columns added later appear by default.
    pub hidden_columns: Vec<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            watch_clipboard: true,
            auto_update: true,
            filter: Filter::All.key().into(),
            sort_column: None,
            sort_descending: false,
            hidden_columns: Vec::new(),
        }
    }
}

impl Global for Prefs {}

impl Prefs {
    /// The current prefs, loaded from disk on first use.
    pub fn get(cx: &mut App) -> &Prefs {
        if !cx.has_global::<Prefs>() {
            cx.set_global(load().unwrap_or_default());
        }
        cx.global::<Prefs>()
    }

    /// Applies `f`, then persists the result (only if something changed).
    pub fn update(cx: &mut App, f: impl FnOnce(&mut Prefs)) {
        let mut prefs = Self::get(cx).clone();
        f(&mut prefs);
        if &prefs != Self::get(cx) {
            save(&prefs);
            cx.set_global(prefs);
        }
    }

    /// The saved quick filter; unknown keys fall back to "All".
    pub fn filter(&self) -> Filter {
        Filter::from_key(&self.filter).unwrap_or_default()
    }

    /// The saved sort of the torrent list; unknown columns mean unsorted.
    pub fn table_sort(&self) -> Option<(Col, ColumnSort)> {
        let col = Col::from_key(self.sort_column.as_deref()?)?;
        Some((col, if self.sort_descending { ColumnSort::Descending } else { ColumnSort::Ascending }))
    }

    /// The saved hidden columns; unknown keys are skipped.
    pub fn hidden_columns(&self) -> Vec<Col> {
        self.hidden_columns.iter().filter_map(|k| Col::from_key(k)).collect()
    }
}

/// Remembers the active quick filter.
pub fn set_filter(cx: &mut App, filter: Filter) {
    Prefs::update(cx, |p| p.filter = filter.key().into());
}

/// Remembers how the torrent list is sorted.
pub fn set_table_sort(cx: &mut App, sort: Option<(Col, ColumnSort)>) {
    Prefs::update(cx, |p| match sort {
        Some((col, dir)) if dir != ColumnSort::Default => {
            p.sort_column = Some(col.key().into());
            p.sort_descending = dir == ColumnSort::Descending;
        }
        _ => {
            p.sort_column = None;
            p.sort_descending = false;
        }
    });
}

/// Remembers which torrent-list columns are hidden.
pub fn set_hidden_columns(cx: &mut App, hidden: &[Col]) {
    Prefs::update(cx, |p| p.hidden_columns = hidden.iter().map(|c| c.key().into()).collect());
}

/// Whether the clipboard watcher is enabled ("Watch clipboard for magnet links").
pub fn watch_clipboard(cx: &mut App) -> bool {
    Prefs::get(cx).watch_clipboard
}

/// Turns the clipboard watcher on or off and saves the choice.
// Called by the Settings dialog's toggle.
pub fn set_watch_clipboard(cx: &mut App, on: bool) {
    Prefs::update(cx, |p| p.watch_clipboard = on);
}

/// Whether to check for and install updates automatically.
pub fn auto_update(cx: &mut App) -> bool {
    Prefs::get(cx).auto_update
}

pub fn set_auto_update(cx: &mut App, on: bool) {
    Prefs::update(cx, |p| p.auto_update = on);
}

fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "storm", "storm").map(|d| d.config_dir().join("prefs.json"))
}

fn load() -> Option<Prefs> {
    let text = std::fs::read_to_string(path()?).ok()?;
    serde_json::from_str(&text).map_err(|e| tracing::warn!("ignoring malformed prefs.json: {e}")).ok()
}

fn save(prefs: &Prefs) {
    let Some(p) = path() else { return };
    let res = p
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::write(&p, serde_json::to_string_pretty(prefs).unwrap_or_default()));
    if let Err(e) = res {
        tracing::warn!("saving {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_default_on() {
        let p: Prefs = serde_json::from_str("{}").unwrap();
        assert!(p.watch_clipboard);
        let p: Prefs = serde_json::from_str(r#"{"watch_clipboard":false}"#).unwrap();
        assert!(!p.watch_clipboard);
        assert_eq!(p.filter(), Filter::All);
        assert_eq!(p.table_sort(), None);
    }

    #[test]
    fn filter_and_sort_round_trip_and_tolerate_unknown_keys() {
        let mut p = Prefs { filter: Filter::Seeding.key().into(), ..Prefs::default() };
        p.sort_column = Some(Col::Size.key().into());
        p.sort_descending = true;
        let p: Prefs = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p.filter(), Filter::Seeding);
        assert_eq!(p.table_sort(), Some((Col::Size, ColumnSort::Descending)));

        let p: Prefs =
            serde_json::from_str(r#"{"watch_clipboard":false,"filter":"bogus","sort_column":"nope"}"#).unwrap();
        assert!(!p.watch_clipboard);
        assert_eq!(p.filter(), Filter::All);
        assert_eq!(p.table_sort(), None);
    }

    #[test]
    fn hidden_columns_round_trip_and_skip_unknown_keys() {
        let p = Prefs { hidden_columns: vec![Col::Eta.key().into(), Col::Ratio.key().into()], ..Prefs::default() };
        let p: Prefs = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p.hidden_columns(), vec![Col::Eta, Col::Ratio]);

        let p: Prefs = serde_json::from_str(r#"{"hidden_columns":["nope","peers"]}"#).unwrap();
        assert_eq!(p.hidden_columns(), vec![Col::Peers]);
        assert!(Prefs::default().hidden_columns().is_empty());
    }
}
