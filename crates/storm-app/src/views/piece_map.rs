//! A strip showing which pieces of a torrent (or file) are downloaded.

use gpui::{Bounds, Hsla, IntoElement, Styled as _, canvas, fill, point, px, size};
use storm_proto::{PieceMap, TorrentFile};

/// Coverage is drawn in this many steps, so neighbouring columns merge into one quad.
const STEPS: f32 = 8.;

/// The pieces a file spans, given the torrent's piece length.
pub fn file_pieces(f: &TorrentFile, piece_length: u32) -> std::ops::Range<u32> {
    if f.length == 0 || piece_length == 0 {
        return 0..0;
    }
    let pl = piece_length as u64;
    (f.offset / pl) as u32..((f.offset + f.length - 1) / pl + 1) as u32
}

/// Which of `ranges` (in order) are downloaded, one flag per piece.
pub fn pieces_of(map: &PieceMap, ranges: impl IntoIterator<Item = std::ops::Range<u32>>) -> Vec<bool> {
    ranges.into_iter().flatten().map(|i| map.has(i)).collect()
}

/// Splits `have` into at most `cols` columns and returns runs of
/// `(first column, column count, coverage)`, coverage in `(0, 1]` and rounded to
/// `1 / STEPS`. Columns without any piece are left out.
pub fn runs(have: &[bool], cols: usize) -> Vec<(usize, usize, f32)> {
    let n = have.len();
    let cols = cols.min(n);
    let mut out: Vec<(usize, usize, f32)> = Vec::new();
    for c in 0..cols {
        let (a, b) = (c * n / cols, ((c + 1) * n / cols).max(c * n / cols + 1));
        let got = have[a..b].iter().filter(|h| **h).count();
        if got == 0 {
            continue;
        }
        // Any piece shows; only a full column is solid.
        let f = if got == b - a {
            1.
        } else {
            ((got as f32 / (b - a) as f32 * STEPS).round() / STEPS).clamp(1. / STEPS, 1. - 1. / STEPS)
        };
        match out.last_mut() {
            Some((s, len, lf)) if *lf == f && *s + *len == c => *len += 1,
            _ => out.push((c, 1, f)),
        }
    }
    out
}

/// A strip of `have` (one flag per piece) on a `track` background. Partly
/// downloaded columns (more pieces than pixels) are drawn fainter.
pub fn strip(have: Vec<bool>, done: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(fill(bounds, track));
            let w = f32::from(bounds.size.width);
            let cols = (w.floor() as usize).clamp(1, have.len().max(1));
            let col_w = w / cols as f32;
            for (start, len, f) in runs(&have, cols) {
                let rect = Bounds::new(
                    point(bounds.origin.x + px(start as f32 * col_w), bounds.origin.y),
                    size(px(len as f32 * col_w), bounds.size.height),
                );
                window.paint_quad(fill(rect, done.opacity(0.25 + 0.75 * f)));
            }
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(offset: u64, length: u64) -> TorrentFile {
        TorrentFile {
            index: 0,
            path: "f".into(),
            length,
            progress_bytes: 0,
            included: true,
            priority: Default::default(),
            offset,
        }
    }

    #[test]
    fn files_span_the_pieces_they_touch() {
        assert_eq!(file_pieces(&file(0, 10), 10), 0..1);
        assert_eq!(file_pieces(&file(5, 10), 10), 0..2);
        assert_eq!(file_pieces(&file(20, 1), 10), 2..3);
        assert_eq!(file_pieces(&file(20, 0), 10), 0..0);
    }

    #[test]
    fn few_pieces_get_a_column_each_and_runs_merge() {
        let have = [true, true, false, true];
        assert_eq!(runs(&have, 100), [(0, 2, 1.), (3, 1, 1.)]);
    }

    #[test]
    fn many_pieces_are_binned_by_coverage() {
        // 16 pieces in 4 columns: full, half, empty, one of four.
        let have: Vec<bool> = [[true; 4], [true, false, true, false], [false; 4], [false, false, false, true]]
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(runs(&have, 4), [(0, 1, 1.), (1, 1, 0.5), (3, 1, 0.25)]);
        assert!(runs(&[], 10).is_empty());
    }
}
