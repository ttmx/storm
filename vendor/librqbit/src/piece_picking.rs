//! [Storm patch] Configurable piece picking.
//!
//! Upstream librqbit 9.1.0 always downloads files one after another (sorted by
//! name), each from its first piece, then its last, then the rest in order. This
//! module makes that a choice:
//!
//! - [`PiecePicking::RarestFirst`] (the default): among the pieces a peer can give
//!   us, take the one the fewest connected peers have. This is what keeps a swarm
//!   healthy, and usually what's fastest.
//! - [`PiecePicking::Sequential`]: the upstream order. Good for previewing media
//!   while it downloads.
//!
//! Either way, files are grouped by [`FilePriority`]: no piece of a lower tier is
//! requested from a peer while it can still give us one from a higher tier.
//! Streaming (`api_stream`) priority pieces still come before everything.

use std::sync::atomic::{AtomicU32, Ordering};

use librqbit_core::lengths::{Lengths, ValidPieceIndex};

use crate::type_aliases::{BS, FileInfos};

/// How the next piece to request from a peer is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PiecePicking {
    #[default]
    RarestFirst,
    Sequential,
}

/// Download priority of one file. Files that aren't wanted at all are deselected
/// with `only_files` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum FilePriority {
    Low,
    #[default]
    Normal,
    High,
}

/// The user-facing picker configuration of a torrent. Lives in
/// `ManagedTorrentShared`, so it survives pause/resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerSettings {
    pub mode: PiecePicking,
    /// Per file index. Empty (or too short) means [`FilePriority::Normal`].
    pub file_priorities: Vec<FilePriority>,
}

impl PickerSettings {
    pub fn priority_of(&self, file: usize) -> FilePriority {
        self.file_priorities.get(file).copied().unwrap_or_default()
    }
}

/// The picker settings compiled for a live torrent: file ids ordered by priority
/// (high first), then by name, split into tiers of equal priority.
#[derive(Debug, Clone)]
pub(crate) struct FilePriorities {
    mode: PiecePicking,
    order: Vec<usize>,
    /// Exclusive end index (into `order`) of each tier.
    tier_ends: Vec<usize>,
}

impl FilePriorities {
    pub fn new(settings: &PickerSettings, file_infos: &FileInfos) -> Self {
        let mut order: Vec<usize> = (0..file_infos.len()).collect();
        // By name within a tier, as upstream does: many torrents list files in a
        // random order.
        order.sort_by(|&a, &b| {
            settings
                .priority_of(b)
                .cmp(&settings.priority_of(a))
                .then_with(|| file_infos[a].relative_filename.cmp(&file_infos[b].relative_filename))
        });
        let mut tier_ends = Vec::new();
        for i in 1..=order.len() {
            if i == order.len() || settings.priority_of(order[i]) != settings.priority_of(order[i - 1]) {
                tier_ends.push(i);
            }
        }
        Self {
            mode: settings.mode,
            order,
            tier_ends,
        }
    }

    pub fn mode(&self) -> PiecePicking {
        self.mode
    }

    /// File ids, one slice per priority tier, highest tier first.
    pub fn tiers(&self) -> impl Iterator<Item = &[usize]> + '_ {
        let mut start = 0;
        self.tier_ends.iter().map(move |&end| {
            let tier = &self.order[start..end];
            start = end;
            tier
        })
    }

    /// All file ids, highest priority first.
    pub fn iter(&self) -> impl Iterator<Item = &usize> + '_ {
        self.order.iter()
    }
}

/// Upstream's default: one tier, files in the given order, sequential.
/// (Kept so tests can build one with `(0..n).collect()`.)
impl FromIterator<usize> for FilePriorities {
    fn from_iter<T: IntoIterator<Item = usize>>(iter: T) -> Self {
        let order: Vec<usize> = iter.into_iter().collect();
        let tier_ends = if order.is_empty() { vec![] } else { vec![order.len()] };
        Self {
            mode: PiecePicking::Sequential,
            order,
            tier_ends,
        }
    }
}

/// How many live peers have each piece. Maintained by `PeerStates` on every
/// change of a live peer's bitfield and on every transition into or out of the
/// live state.
#[derive(Debug, Default)]
pub(crate) struct PieceAvailability(Box<[AtomicU32]>);

impl PieceAvailability {
    pub fn new(lengths: &Lengths) -> Self {
        Self(
            (0..lengths.total_pieces())
                .map(|_| AtomicU32::new(0))
                .collect(),
        )
    }

    pub fn get(&self, piece: usize) -> u32 {
        self.0
            .get(piece)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn inc(&self, piece: usize) {
        if let Some(c) = self.0.get(piece) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add(&self, bitfield: &BS) {
        for piece in bitfield.iter_ones() {
            self.inc(piece);
        }
    }

    pub fn sub(&self, bitfield: &BS) {
        for piece in bitfield.iter_ones() {
            if let Some(c) = self.0.get(piece) {
                // Saturating decrement. (Not `fetch_update`: newer toolchains deprecate it.)
                let mut v = c.load(Ordering::Relaxed);
                while v > 0 {
                    match c.compare_exchange_weak(v, v - 1, Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(_) => break,
                        Err(now) => v = now,
                    }
                }
            }
        }
    }
}

/// Random start offset for breaking availability ties, so that peers asking at
/// the same time don't all converge on the lowest-numbered rare piece.
/// In `0..total_pieces`.
pub(crate) fn tiebreak_offset(total_pieces: u32) -> u32 {
    rand::random::<u32>() % total_pieces.max(1)
}

/// Orders candidate pieces for rarest-first: lower availability wins, ties go to
/// the first piece at or after `offset`, wrapping around at `total_pieces`.
pub(crate) fn rarity_key(availability: u32, piece: ValidPieceIndex, offset: u32, total_pieces: u32) -> (u32, u32) {
    let p = piece.get();
    let distance = if p >= offset { p - offset } else { p + total_pieces - offset };
    (availability, distance)
}

#[cfg(test)]
mod tests {
    use librqbit_core::lengths::Lengths;

    use super::*;
    use crate::{bitv::BitV, chunk_tracker::ChunkTracker, file_info::FileInfo, type_aliases::BF};

    const PIECE: u32 = 16384;
    const PER_FILE: u32 = 4;

    /// Files "a", "b", "c" (listed as c, a, b), 4 pieces each: a = 4..8,
    /// b = 8..12, c = 0..4. Nothing downloaded, everything selected.
    fn setup() -> (ChunkTracker, FileInfos) {
        let total = 3 * PER_FILE;
        let lengths = Lengths::new(PIECE as u64 * total as u64, PIECE).unwrap();
        let file_infos: FileInfos = ["c", "a", "b"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let start = i as u32 * PER_FILE;
                FileInfo {
                    relative_filename: name.into(),
                    offset_in_torrent: start as u64 * PIECE as u64,
                    len: PER_FILE as u64 * PIECE as u64,
                    piece_range: start..start + PER_FILE,
                    attrs: Default::default(),
                }
            })
            .collect();
        let bytes = lengths.piece_bitfield_bytes();
        let have = BF::from_boxed_slice(vec![0u8; bytes].into_boxed_slice());
        let mut selected = BF::from_boxed_slice(vec![0u8; bytes].into_boxed_slice());
        selected[..total as usize].fill(true);
        let chunks = ChunkTracker::new(have.into_dyn(), selected, lengths, &file_infos).unwrap();
        (chunks, file_infos)
    }

    fn settings(mode: PiecePicking, prios: &[FilePriority]) -> PickerSettings {
        PickerSettings {
            mode,
            file_priorities: prios.to_vec(),
        }
    }

    /// Picks (and reserves) pieces until none is left.
    fn drain(
        chunks: &mut ChunkTracker,
        fi: &FileInfos,
        s: &PickerSettings,
        avail: &[u32],
        peer_has: impl Fn(ValidPieceIndex) -> bool,
    ) -> Vec<u32> {
        let fp = FilePriorities::new(s, fi);
        let mut out = vec![];
        while let Some(p) =
            chunks.pick_queued_piece(&fp, fi, |i| avail.get(i).copied().unwrap_or(0), &peer_has)
        {
            chunks.reserve_needed_piece(p);
            out.push(p.get());
        }
        out
    }

    #[test]
    fn sequential_goes_by_name_then_first_last_rest() {
        let (mut c, fi) = setup();
        let s = settings(PiecePicking::Sequential, &[]);
        // a (4..8), b (8..12), c (0..4); each first, last, then the middle.
        assert_eq!(drain(&mut c, &fi, &s, &[], |_| true), [4, 7, 5, 6, 8, 11, 9, 10, 0, 3, 1, 2]);
    }

    #[test]
    fn sequential_honours_priorities() {
        let (mut c, fi) = setup();
        use FilePriority::*;
        // c high, a low, b normal.
        let s = settings(PiecePicking::Sequential, &[High, Low, Normal]);
        assert_eq!(drain(&mut c, &fi, &s, &[], |_| true), [0, 3, 1, 2, 8, 11, 9, 10, 4, 7, 5, 6]);
    }

    #[test]
    fn rarest_first_takes_least_available() {
        let (mut c, fi) = setup();
        let s = settings(PiecePicking::RarestFirst, &[]);
        let avail = [5, 9, 2, 7, 8, 1, 6, 4, 3, 12, 11, 10];
        let order = drain(&mut c, &fi, &s, &avail, |_| true);
        let mut by_rarity: Vec<u32> = (0..12).collect();
        by_rarity.sort_by_key(|&p| avail[p as usize]);
        assert_eq!(order, by_rarity);
    }

    #[test]
    fn rarest_first_skips_pieces_the_peer_lacks() {
        let (mut c, fi) = setup();
        let s = settings(PiecePicking::RarestFirst, &[]);
        let avail = [1; 12];
        let order = drain(&mut c, &fi, &s, &avail, |p| p.get() % 2 == 0);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, [0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn rarest_first_finishes_a_tier_before_the_next() {
        let (mut c, fi) = setup();
        use FilePriority::*;
        // b (8..12) is high but its pieces are common; rare pieces elsewhere wait.
        let s = settings(PiecePicking::RarestFirst, &[Normal, Low, High]);
        let avail = [1, 1, 1, 1, 1, 1, 1, 1, 9, 9, 9, 9];
        let order = drain(&mut c, &fi, &s, &avail, |_| true);
        let tier = |slice: &[u32]| {
            let mut v = slice.to_vec();
            v.sort();
            v
        };
        assert_eq!(tier(&order[0..4]), [8, 9, 10, 11]);
        assert_eq!(tier(&order[4..8]), [0, 1, 2, 3]);
        assert_eq!(tier(&order[8..12]), [4, 5, 6, 7]);
    }

    #[test]
    fn rarest_first_spreads_ties() {
        // With all pieces equally rare, different calls shouldn't all pick piece 0.
        let (c, fi) = setup();
        let fp = FilePriorities::new(&settings(PiecePicking::RarestFirst, &[]), &fi);
        let picks: std::collections::HashSet<u32> = (0..64)
            .filter_map(|_| c.pick_queued_piece(&fp, &fi, |_| 1, |_| true))
            .map(|p| p.get())
            .collect();
        assert!(picks.len() > 1, "{picks:?}");
    }

    #[test]
    fn availability_tracks_bitfields() {
        let lengths = Lengths::new(PIECE as u64 * 10, PIECE).unwrap();
        let a = PieceAvailability::new(&lengths);
        let mut bf = BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into_boxed_slice());
        bf.set(1, true);
        bf.set(3, true);
        a.add(&bf);
        a.add(&bf);
        a.inc(3);
        assert_eq!((0..5).map(|i| a.get(i)).collect::<Vec<_>>(), [0, 2, 0, 3, 0]);
        a.sub(&bf);
        a.sub(&bf);
        assert_eq!((0..5).map(|i| a.get(i)).collect::<Vec<_>>(), [0, 0, 0, 1, 0]);
        a.sub(&bf); // piece 1 saturates at 0 instead of wrapping
        assert_eq!((0..5).map(|i| a.get(i)).collect::<Vec<_>>(), [0, 0, 0, 0, 0]);
    }
}
