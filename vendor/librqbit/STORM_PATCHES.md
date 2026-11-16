# librqbit 9.1.0, patched for Storm

This is librqbit 9.1.0 from crates.io (`webui/`, `examples/` and their manifest
entries removed), wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. Every change is marked `[Storm patch]` in the source.

## Configurable piece picking

Upstream always downloads files one at a time (sorted by name), each from its
first piece, then its last, then the rest in order (`// TODO: make it configurable`
in `torrent_state/live/mod.rs`).

- `src/piece_picking.rs` (new): `PiecePicking` (`RarestFirst`, the default, or
  `Sequential`), `FilePriority` (`Low` / `Normal` / `High`), `PickerSettings`, the
  compiled `FilePriorities` (file order and priority tiers) and `PieceAvailability`
  (how many live peers have each piece).
- `chunk_tracker.rs`: `iter_queued_pieces` became `pick_queued_piece`. In
  sequential mode it walks the files in priority-then-name order, as upstream did.
  In rarest-first mode it takes, within the highest tier that has a piece the peer
  can give us, the piece with the lowest availability (ties broken from a random
  offset).
- `piece_tracker.rs`: `AcquireRequest` gets `piece_availability`. Streaming
  priority pieces still come first, and stealing is unchanged.
- `torrent_state/live/peers/mod.rs`, `torrent_state/live/peer/mod.rs`, and
  `on_have` in `torrent_state/live/mod.rs`: keep `PeerStates::availability` in
  sync. A live peer's bitfield is added when it goes live and removed when it
  leaves. Bitfield updates and `have` messages adjust it too.
- `torrent_state/mod.rs`: `ManagedTorrentShared::picker` holds the settings, so
  they survive pause and resume. `ManagedTorrent::picker_settings()` and
  `set_picker_settings()` read and change them, and a change applies at once to a
  live torrent.
- `session.rs`: `AddTorrentOptions::picker` sets the initial settings.

## Piece map

`ManagedTorrent::have_pieces()` (`torrent_state/mod.rs`) returns the have-bitfield,
for Storm's piece map.

librqbit doesn't persist the piece picking settings. Storm stores them in its own database
(`storm-engine/src/db.rs`, table `picking`) and reapplies them at startup.

## Upgrading librqbit

Copy the new release over this folder and drop `webui/`, `examples/` and the
`[[example]]` entries again. Then re-apply the `[Storm patch]` hunks, using a diff
against the pristine 9.1.0 crate. Run the library tests with:

    cargo test --manifest-path vendor/librqbit/Cargo.toml --no-default-features --features rust-tls --lib

Delete `vendor/librqbit/Cargo.lock` and `target/` afterwards. The workspace lock
file is the one that counts.
