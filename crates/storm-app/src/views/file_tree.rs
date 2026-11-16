//! Pure model behind the Files tab: torrent file paths folded into a collapsible
//! folder tree, plus the tri-state selection helpers. No gpui in here.

use std::collections::{BTreeSet, HashSet};

use storm_proto::TorrentFile;

/// One visible line of the Files list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    pub name: String,
    pub kind: RowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// `path` is the folder's full path (the collapse key); `files` holds the
    /// positions (into `TorrentDetails::files`) of every file below it.
    Dir {
        path: String,
        files: Vec<usize>,
        collapsed: bool,
    },
    File {
        pos: usize,
    },
}

impl Row {
    /// Positions of the files this row stands for.
    pub fn files(&self) -> &[usize] {
        match &self.kind {
            RowKind::Dir { files, .. } => files,
            RowKind::File { pos } => std::slice::from_ref(pos),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Off,
    Partial,
    On,
}

impl Check {
    pub fn of(hits: usize, total: usize) -> Self {
        match hits {
            0 => Check::Off,
            n if n >= total => Check::On,
            _ => Check::Partial,
        }
    }
}

/// How many of `files` are in `set`, as a tri-state.
pub fn check_of(files: &[usize], set: &BTreeSet<usize>) -> Check {
    Check::of(files.iter().filter(|p| set.contains(p)).count(), files.len())
}

#[derive(Default)]
struct Node {
    dirs: Vec<(String, Node)>,
    files: Vec<(String, usize)>,
}

impl Node {
    fn dir(&mut self, name: &str) -> &mut Node {
        let i = match self.dirs.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                self.dirs.push((name.to_owned(), Node::default()));
                self.dirs.len() - 1
            }
        };
        &mut self.dirs[i].1
    }

    fn all_files(&self, out: &mut Vec<usize>) {
        for (_, d) in &self.dirs {
            d.all_files(out);
        }
        out.extend(self.files.iter().map(|(_, p)| *p));
    }
}

fn split(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|s| !s.is_empty()).collect()
}

/// Folds `files` into visible rows. Folders come first, then files, each sorted
/// case-insensitively. A single folder wrapping everything (usually the torrent's
/// name) is skipped: it would only add a level of indentation.
pub fn build_rows(files: &[TorrentFile], collapsed: &HashSet<String>) -> Vec<Row> {
    let mut root = Node::default();
    for (pos, f) in files.iter().enumerate() {
        let parts = split(&f.path);
        let Some((name, dirs)) = parts.split_last() else {
            root.files.push((f.path.clone(), pos));
            continue;
        };
        let mut node = &mut root;
        for d in dirs {
            node = node.dir(d);
        }
        node.files.push(((*name).to_owned(), pos));
    }
    let mut prefix = String::new();
    while root.files.is_empty() && root.dirs.len() == 1 {
        let (name, only) = root.dirs.pop().expect("one dir");
        prefix.push_str(&name);
        prefix.push('/');
        root = only;
    }
    let mut rows = Vec::new();
    emit(&mut root, &prefix, 0, collapsed, &mut rows);
    rows
}

fn emit(node: &mut Node, prefix: &str, depth: usize, collapsed: &HashSet<String>, rows: &mut Vec<Row>) {
    node.dirs.sort_by_key(|(n, _)| n.to_lowercase());
    node.files.sort_by_key(|(n, _)| n.to_lowercase());
    for (name, child) in &mut node.dirs {
        let path = format!("{prefix}{name}");
        let mut files = Vec::new();
        child.all_files(&mut files);
        let is_collapsed = collapsed.contains(&path);
        rows.push(Row {
            depth,
            name: name.clone(),
            kind: RowKind::Dir { path: path.clone(), files, collapsed: is_collapsed },
        });
        if !is_collapsed {
            emit(child, &format!("{path}/"), depth + 1, collapsed, rows);
        }
    }
    for (name, pos) in &node.files {
        rows.push(Row { depth, name: name.clone(), kind: RowKind::File { pos: *pos } });
    }
}

/// Turns a torrent-relative path (from the backend, so untrusted) into a relative
/// path that cannot escape the destination folder.
pub fn safe_relative(path: &str) -> std::path::PathBuf {
    split(path).into_iter().filter(|c| *c != "." && *c != ".." && !c.contains(':')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<TorrentFile> {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| TorrentFile {
                index: i,
                path: (*p).into(),
                length: 10,
                progress_bytes: 0,
                included: true,
                priority: Default::default(),
                offset: 0,
            })
            .collect()
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.name)).collect()
    }

    #[test]
    fn flat_list_stays_flat() {
        let rows = build_rows(&files(&["b.txt", "A.txt"]), &HashSet::new());
        assert_eq!(names(&rows), ["A.txt", "b.txt"]);
    }

    #[test]
    fn common_root_is_skipped_and_dirs_come_first() {
        let rows = build_rows(&files(&["Show/e2.mkv", "Show/Subs/en.srt", "Show/e1.mkv"]), &HashSet::new());
        assert_eq!(names(&rows), ["Subs", "  en.srt", "e1.mkv", "e2.mkv"]);
        assert_eq!(rows[0].files(), &[1]);
        assert!(matches!(&rows[0].kind, RowKind::Dir { path, .. } if path == "Show/Subs"));
    }

    #[test]
    fn collapsed_dirs_hide_children_but_keep_files() {
        let collapsed = HashSet::from(["Show/Subs".to_owned()]);
        let rows = build_rows(&files(&["Show/Subs/en.srt", "Show/Subs/fr.srt", "Show/e1.mkv"]), &collapsed);
        assert_eq!(names(&rows), ["Subs", "e1.mkv"]);
        assert_eq!(rows[0].files(), &[0, 1]);
    }

    #[test]
    fn tri_state() {
        let set = BTreeSet::from([1, 2]);
        assert_eq!(check_of(&[1, 2], &set), Check::On);
        assert_eq!(check_of(&[1, 3], &set), Check::Partial);
        assert_eq!(check_of(&[3], &set), Check::Off);
        assert_eq!(check_of(&[], &set), Check::Off);
    }

    #[test]
    fn relative_paths_cannot_escape() {
        let p = safe_relative("../../etc/C:/x/./ok.txt");
        assert_eq!(p, std::path::PathBuf::from("etc").join("x").join("ok.txt"));
    }
}
