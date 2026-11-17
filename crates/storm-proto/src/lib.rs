//! Wire types and the [`Backend`] trait shared by the engine, server, client and app.
//!
//! Everything here is plain serde data so it can travel over HTTP/WebSocket unchanged.
//! The app only ever talks to a `dyn Backend`, so it doesn't care whether the engine
//! runs in-process ([`BackendKind::Local`]) or on another machine ([`BackendKind::Remote`]).

use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

/// Bumped on breaking changes to the HTTP/WebSocket API.
pub const API_VERSION: u32 = 1;

/// Default TCP port of `stormd`.
pub const DEFAULT_PORT: u16 = 9870;

/// Session-local torrent id. Stable across restarts (librqbit persists it).
pub type TorrentId = u64;

// ---------------------------------------------------------------------------
// Backend identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendKind {
    /// The engine runs in this process and shares the UI's filesystem:
    /// native file pickers and "open folder" make sense; "download to this PC" doesn't.
    Local,
    /// The engine runs elsewhere: paths are server-side, files must be fetched over the API.
    Remote { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendInfo {
    pub kind: BackendKind,
    pub api_version: u32,
    /// Version of the storm engine on the other side.
    pub engine_version: String,
    /// Version of librqbit the engine is built against.
    pub librqbit_version: String,
    /// The server can replace itself with another release
    /// ([`Backend::update_server`]). Always false for a local engine.
    #[serde(default)]
    pub self_update: bool,
}

/// Body of `POST /server/update`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateServerRequest {
    /// The release to install, `x.y.z`. Must be newer than the running one.
    pub version: String,
}

impl BackendInfo {
    /// True when the backend's filesystem is the UI's filesystem.
    pub fn is_local(&self) -> bool {
        matches!(self.kind, BackendKind::Local)
    }
}

// ---------------------------------------------------------------------------
// Torrents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TorrentState {
    /// A magnet/URL whose metadata is still being fetched. The torrent has a
    /// temporary id; once resolved, this row is replaced by the real torrent.
    FetchingMetadata,
    /// Checking existing files.
    Initializing,
    Downloading,
    Seeding,
    Paused,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentSummary {
    pub id: TorrentId,
    pub info_hash: String,
    /// `None` until metadata is resolved (e.g. a fresh magnet link).
    pub name: Option<String>,
    pub state: TorrentState,
    pub error: Option<String>,
    /// Bytes selected for download.
    pub total_bytes: u64,
    /// Bytes downloaded and verified.
    pub progress_bytes: u64,
    /// Uploaded bytes, lifetime (persisted across restarts by the engine).
    pub uploaded_bytes: u64,
    /// Downloaded bytes, lifetime (persisted across restarts by the engine).
    pub downloaded_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub eta_secs: Option<u64>,
    pub peers_live: u32,
    pub peers_seen: u32,
    pub seeders_live: u32,
    pub output_folder: String,
    /// Unix seconds.
    pub added_at: Option<i64>,
    /// Unix seconds.
    pub completed_at: Option<i64>,
    /// Pieces are fetched in order (file by file) instead of rarest-first.
    #[serde(default)]
    pub sequential: bool,
}

impl TorrentSummary {
    pub fn progress(&self) -> f64 {
        if self.total_bytes == 0 { 0.0 } else { self.progress_bytes as f64 / self.total_bytes as f64 }
    }

    pub fn ratio(&self) -> f64 {
        let base = self.downloaded_bytes.max(self.progress_bytes);
        if base == 0 { 0.0 } else { self.uploaded_bytes as f64 / base as f64 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFile {
    pub index: usize,
    /// Path relative to the torrent's output folder, `/`-separated.
    pub path: String,
    pub length: u64,
    pub progress_bytes: u64,
    pub included: bool,
    #[serde(default)]
    pub priority: FilePriority,
    /// Where the file starts within the torrent's data, in bytes.
    #[serde(default)]
    pub offset: u64,
}

/// Download priority of a file. Files of a higher priority are fetched first;
/// files that aren't wanted at all are excluded (`TorrentFile::included`) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePriority {
    Low,
    #[default]
    Normal,
    High,
}

impl FilePriority {
    pub const ALL: [FilePriority; 3] = [FilePriority::High, FilePriority::Normal, FilePriority::Low];

    pub fn label(self) -> &'static str {
        match self {
            FilePriority::Low => "Low",
            FilePriority::Normal => "Normal",
            FilePriority::High => "High",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentDetails {
    pub summary: TorrentSummary,
    pub files: Vec<TorrentFile>,
    pub trackers: Vec<String>,
    pub piece_length: Option<u32>,
    pub piece_count: Option<u32>,
    pub comment: Option<String>,
    /// The metainfo's `created by` field (the tool that made the .torrent).
    #[serde(default)]
    pub created_by: Option<String>,
    /// The metainfo's `creation date`, unix seconds.
    #[serde(default)]
    pub creation_date: Option<i64>,
    /// Which pieces are downloaded and verified. `None` until known.
    #[serde(default)]
    pub have_pieces: Option<PieceMap>,
}

/// One bit per piece, most significant bit first (BitTorrent's bitfield layout).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PieceMap(#[serde(with = "base64_bytes")] pub Vec<u8>);

impl PieceMap {
    pub fn from_bools(bits: impl IntoIterator<Item = bool>) -> Self {
        let mut bytes = Vec::new();
        for (i, b) in bits.into_iter().enumerate() {
            if i % 8 == 0 {
                bytes.push(0);
            }
            if b {
                *bytes.last_mut().unwrap() |= 0x80 >> (i % 8);
            }
        }
        PieceMap(bytes)
    }

    /// Whether piece `i` is downloaded (`false` when out of range).
    pub fn has(&self, i: u32) -> bool {
        let i = i as usize;
        self.0.get(i / 8).is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub addr: String,
    pub client: Option<String>,
    /// "tcp", "utp", "socks"...
    pub transport: Option<String>,
    pub state: String,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    /// Current download rate from this peer, bytes/s. Derived by the backend from
    /// successive samples of `downloaded_bytes`; 0 until there are two samples.
    #[serde(default)]
    pub down_bps: u64,
    /// Current upload rate to this peer, bytes/s (see `down_bps`).
    #[serde(default)]
    pub up_bps: u64,
    /// Pieces received from this peer that passed the hash check.
    #[serde(default)]
    pub pieces_downloaded: u32,
    /// Errors seen on connections to this peer.
    #[serde(default)]
    pub errors: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TorrentSource {
    /// `magnet:` link or `http(s)://` URL pointing at a `.torrent` file.
    Url { url: String },
    /// Raw `.torrent` file contents (base64 on the wire).
    File {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTorrentRequest {
    pub source: TorrentSource,
    /// Server-side folder. Defaults to the backend's download directory.
    #[serde(default)]
    pub output_folder: Option<String>,
    #[serde(default)]
    pub paused: bool,
    /// Only download these file indices.
    #[serde(default)]
    pub only_files: Option<Vec<usize>>,
    /// Download pieces in order instead of rarest-first.
    #[serde(default)]
    pub sequential: bool,
}

impl AddTorrentRequest {
    pub fn url(url: impl Into<String>) -> Self {
        Self::new(TorrentSource::Url { url: url.into() })
    }

    pub fn file(bytes: Vec<u8>) -> Self {
        Self::new(TorrentSource::File { bytes })
    }

    fn new(source: TorrentSource) -> Self {
        Self { source, output_folder: None, paused: false, only_files: None, sequential: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTorrentResponse {
    pub id: TorrentId,
    pub info_hash: String,
    /// True if the torrent was already in the session.
    pub already_existed: bool,
}

// ---------------------------------------------------------------------------
// Session, settings, history
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStats {
    pub down_bps: u64,
    pub up_bps: u64,
    /// Bytes fetched since the engine started.
    pub session_downloaded: u64,
    /// Bytes uploaded since the engine started.
    pub session_uploaded: u64,
    /// Lifetime totals across all runs (from the stats database).
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    pub peers_live: u32,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Default download folder (server-side path).
    pub download_dir: String,
    /// TCP/uTP listen port. 0 = random. Takes effect after a restart.
    pub listen_port: u16,
    pub enable_upnp: bool,
    /// `None` = unlimited.
    pub down_limit_bps: Option<u32>,
    pub up_limit_bps: Option<u32>,
}

/// Fields that are `Some` get changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsPatch {
    pub download_dir: Option<String>,
    pub listen_port: Option<u16>,
    pub enable_upnp: Option<bool>,
    /// `Some(None)` clears the limit.
    #[serde(default, with = "double_option")]
    pub down_limit_bps: Option<Option<u32>>,
    #[serde(default, with = "double_option")]
    pub up_limit_bps: Option<Option<u32>>,
}

/// A directory on the backend's machine, for the remote folder browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// Absolute path on the backend's machine.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirListing {
    /// Absolute, normalized path that was listed.
    pub path: String,
    /// `None` at a filesystem root.
    pub parent: Option<String>,
    /// Subdirectories only, sorted case-insensitively.
    pub dirs: Vec<DirEntry>,
    /// Filesystem roots (drive letters on Windows, `/` elsewhere) for a sidebar.
    pub roots: Vec<String>,
    /// Path separator used by the backend's OS (`\` or `/`).
    pub separator: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StatsSample {
    /// Unix seconds.
    pub ts: i64,
    pub down_bps: u64,
    pub up_bps: u64,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Full state, sent on subscribe and then about once a second.
    Snapshot {
        torrents: Vec<TorrentSummary>,
        session: SessionStats,
    },
    SettingsChanged {
        settings: Settings,
    },
}

pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotFound,
    BadRequest,
    Unauthorized,
    /// Couldn't reach the backend at all.
    Connection,
    Internal,
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, m)
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, m)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Everything the UI can ask of a torrent engine.
///
/// Implementations must not require the caller to run inside any particular async
/// runtime: the gpui app polls these futures on its own executor.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    fn info(&self) -> BackendInfo;

    /// Starts with a [`Event::Snapshot`], then streams updates until dropped.
    async fn subscribe(&self) -> Result<EventStream>;

    async fn list_torrents(&self) -> Result<Vec<TorrentSummary>>;
    async fn torrent_details(&self, id: TorrentId) -> Result<TorrentDetails>;
    async fn torrent_peers(&self, id: TorrentId) -> Result<Vec<PeerInfo>>;

    async fn add_torrent(&self, req: AddTorrentRequest) -> Result<AddTorrentResponse>;
    async fn pause(&self, id: TorrentId) -> Result<()>;
    async fn resume(&self, id: TorrentId) -> Result<()>;
    async fn remove(&self, id: TorrentId, delete_files: bool) -> Result<()>;
    async fn set_included_files(&self, id: TorrentId, files: Vec<usize>) -> Result<()>;
    /// Sequential (in order, file by file) vs rarest-first piece picking.
    async fn set_sequential(&self, id: TorrentId, sequential: bool) -> Result<()>;
    /// Sets the download priority of the given files.
    async fn set_file_priority(&self, id: TorrentId, files: Vec<usize>, priority: FilePriority) -> Result<()>;

    async fn session_stats(&self) -> Result<SessionStats>;
    /// Samples newer than `since` (unix seconds), oldest first.
    async fn stats_history(&self, since: i64) -> Result<Vec<StatsSample>>;

    async fn settings(&self) -> Result<Settings>;
    async fn update_settings(&self, patch: SettingsPatch) -> Result<Settings>;

    /// Lists subdirectories of `path` on the backend's machine. `None` starts at the
    /// download directory. Drives the folder browser when the backend is remote.
    async fn list_dirs(&self, path: Option<String>) -> Result<DirListing>;
    /// Creates a directory (and its parents) on the backend's machine.
    async fn create_dir(&self, path: String) -> Result<()>;

    /// Copies a torrent file from the backend's machine to `dest` on this machine.
    /// Only meaningful for remote backends; a local backend's files are already here.
    async fn download_file(&self, id: TorrentId, file_index: usize, dest: std::path::PathBuf) -> Result<()> {
        let _ = (id, file_index, dest);
        Err(Error::bad_request("files of a local backend are already on this machine"))
    }

    /// Asks a server to install release `version` and restart into it. Returns
    /// once the new build is in place; the connection then drops and comes
    /// back on the new version (see [`BackendInfo::self_update`]).
    async fn update_server(&self, version: String) -> Result<()> {
        let _ = version;
        Err(Error::bad_request("this backend can't update itself"))
    }
}

// ---------------------------------------------------------------------------
// serde helpers
// ---------------------------------------------------------------------------

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(inner) => inner.serialize(s),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_patch_distinguishes_unset_and_clear() {
        let p: SettingsPatch = serde_json::from_str(r#"{"down_limit_bps": null}"#).unwrap();
        assert_eq!(p.down_limit_bps, Some(None));
        assert_eq!(p.up_limit_bps, None);
        let p: SettingsPatch = serde_json::from_str(r#"{"up_limit_bps": 1000}"#).unwrap();
        assert_eq!(p.up_limit_bps, Some(Some(1000)));
    }

    #[test]
    fn piece_map_is_msb_first_and_base64_on_the_wire() {
        let m = PieceMap::from_bools([true, false, false, false, false, false, false, true, true]);
        assert_eq!(m.0, [0b1000_0001, 0b1000_0000]);
        assert!(m.has(0) && !m.has(1) && m.has(7) && m.has(8) && !m.has(9) && !m.has(100));
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "\"gYA=\"");
        assert_eq!(serde_json::from_str::<PieceMap>(&json).unwrap(), m);
    }

    #[test]
    fn torrent_file_roundtrips_as_base64() {
        let req = AddTorrentRequest::file(vec![1, 2, 3]);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("AQID"));
        let back: AddTorrentRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.source, TorrentSource::File { bytes } if bytes == vec![1, 2, 3]));
    }

    #[test]
    fn older_payloads_default_to_rarest_first_and_normal_priority() {
        let f: TorrentFile =
            serde_json::from_str(r#"{"index":0,"path":"a","length":1,"progress_bytes":0,"included":true}"#).unwrap();
        assert_eq!(f.priority, FilePriority::Normal);
        let r: AddTorrentRequest = serde_json::from_str(r#"{"source":{"type":"url","url":"magnet:?"}}"#).unwrap();
        assert!(!r.sequential);
        assert_eq!(serde_json::to_string(&FilePriority::High).unwrap(), r#""high""#);
    }

    #[test]
    fn peer_info_accepts_older_payloads() {
        let p: PeerInfo = serde_json::from_str(
            r#"{"addr":"1.2.3.4:5","client":null,"transport":"tcp","state":"live","downloaded_bytes":10,"uploaded_bytes":0}"#,
        )
        .unwrap();
        assert_eq!((p.down_bps, p.up_bps, p.pieces_downloaded, p.errors), (0, 0, 0, 0));
    }
}
