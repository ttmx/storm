//! In-process torrent engine: librqbit + storm's SQLite database, exposed as a
//! [`storm_proto::Backend`].
//!
//! The engine runs on a tokio runtime. Every `Backend` method hops onto that runtime,
//! so callers (e.g. gpui's executor) don't need tokio themselves.

mod db;
mod fs_browse;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::StreamExt;
use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, DhtSessionConfig, ListenerMode, ListenerOptions, ManagedTorrent,
    PickerSettings, PiecePicking, Session, SessionOptions, SessionPersistenceConfig, TorrentStatsState,
};
use storm_proto::{
    self as proto, AddTorrentRequest, Backend, BackendInfo, BackendKind, DirListing, Error, Event, EventStream,
    FilePriority, PeerInfo, SessionStats, Settings, SettingsPatch, StatsSample, TorrentDetails, TorrentFile, TorrentId,
    TorrentSource, TorrentState, TorrentSummary,
};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::db::{Counter, Db, Picking, TorrentRecord};

pub use crate::db::Db as Database;

const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const HISTORY_RETENTION_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Where storm keeps its database and librqbit's session state.
    pub data_dir: PathBuf,
    /// Download folder used when none is stored in settings yet.
    pub default_download_dir: PathBuf,
}

impl EngineConfig {
    /// Platform-standard locations (e.g. `%APPDATA%\storm` and `~/Downloads`).
    pub fn platform_default() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("dev", "storm", "storm").context("no home directory")?;
        let user = directories::UserDirs::new();
        let downloads = user
            .as_ref()
            .and_then(|u| u.download_dir().map(|d| d.to_owned()))
            .unwrap_or_else(|| dirs.data_dir().join("downloads"));
        Ok(Self { data_dir: dirs.data_dir().to_owned(), default_download_dir: downloads })
    }
}

/// The in-process engine. Cheap to clone.
#[derive(Clone)]
pub struct LocalBackend {
    inner: Arc<Inner>,
    /// Present when the engine owns its runtime (desktop app); `None` when it
    /// borrows the caller's (server).
    _runtime: Option<Arc<Runtime>>,
}

struct Inner {
    handle: Handle,
    session: Arc<Session>,
    api: Api,
    db: Db,
    settings: Mutex<Settings>,
    counters: Mutex<Counters>,
    events: broadcast::Sender<Event>,
    /// Magnets/URLs whose metadata is still being fetched, keyed by synthetic id.
    pending: Mutex<HashMap<TorrentId, Pending>>,
    next_pending: AtomicU64,
    /// Per-peer byte counters from the previous `peers()` call, to derive speeds.
    peer_rates: Mutex<PeerRates>,
}

/// Optional top-level metainfo fields librqbit parses but doesn't surface; read
/// back from the `.torrent` bytes it keeps in `TorrentMetadata::torrent_bytes`.
#[derive(Debug, Default, PartialEq)]
struct MetainfoExtras {
    comment: Option<String>,
    created_by: Option<String>,
    creation_date: Option<i64>,
}

impl MetainfoExtras {
    fn parse(torrent_bytes: &[u8]) -> Self {
        // A failed parse (or empty bytes) just means "no extras".
        let Ok(t) = librqbit::torrent_from_bytes(torrent_bytes) else { return Self::default() };
        let text = |b: Option<librqbit::ByteBuf<'_>>| {
            b.map(|b| String::from_utf8_lossy(b.as_ref()).trim().to_owned()).filter(|s| !s.is_empty())
        };
        Self {
            comment: text(t.comment),
            created_by: text(t.created_by),
            creation_date: t.creation_date.and_then(|d| i64::try_from(d).ok()).filter(|d| *d > 0),
        }
    }
}

/// Turns librqbit's cumulative per-peer byte counters into rates by diffing against
/// the previous sample of the same (torrent, peer address).
#[derive(Default)]
struct PeerRates {
    torrents: HashMap<TorrentId, TorrentPeerSamples>,
}

struct TorrentPeerSamples {
    polled_at: Instant,
    peers: HashMap<String, PeerSample>,
}

#[derive(Clone, Copy)]
struct PeerSample {
    at: Instant,
    downloaded: u64,
    uploaded: u64,
    down_bps: u64,
    up_bps: u64,
}

impl PeerRates {
    /// Samples closer together than this reuse the last rate instead of re-diffing,
    /// so overlapping pollers (several clients) don't produce noisy numbers.
    const MIN_INTERVAL: Duration = Duration::from_millis(500);
    /// Torrents whose peers nobody has looked at for this long are forgotten.
    const STALE_AFTER: Duration = Duration::from_secs(60);

    /// Fills in `down_bps` / `up_bps` and replaces the stored samples for `id` with
    /// the peers in this snapshot (peers that went away are dropped).
    fn apply(&mut self, id: TorrentId, now: Instant, peers: &mut [PeerInfo]) {
        self.torrents.retain(|_, t| now.saturating_duration_since(t.polled_at) < Self::STALE_AFTER);
        let prev = self.torrents.remove(&id).map(|t| t.peers).unwrap_or_default();
        let mut next = HashMap::with_capacity(peers.len());
        for p in peers.iter_mut() {
            let fresh = PeerSample {
                at: now,
                downloaded: p.downloaded_bytes,
                uploaded: p.uploaded_bytes,
                down_bps: 0,
                up_bps: 0,
            };
            let sample = match prev.get(&p.addr) {
                // Counters went backwards: librqbit forgot and re-added the peer.
                Some(old) if p.downloaded_bytes < old.downloaded || p.uploaded_bytes < old.uploaded => fresh,
                // Too soon to diff: keep the old baseline and rate.
                Some(old) if now.saturating_duration_since(old.at) < Self::MIN_INTERVAL => *old,
                Some(old) => {
                    let secs = now.saturating_duration_since(old.at).as_secs_f64();
                    let rate = |new: u64, old: u64| ((new - old) as f64 / secs).round() as u64;
                    PeerSample {
                        down_bps: rate(p.downloaded_bytes, old.downloaded),
                        up_bps: rate(p.uploaded_bytes, old.uploaded),
                        ..fresh
                    }
                }
                None => fresh,
            };
            p.down_bps = sample.down_bps;
            p.up_bps = sample.up_bps;
            next.insert(p.addr.clone(), sample);
        }
        self.torrents.insert(id, TorrentPeerSamples { polled_at: now, peers: next });
    }

    fn forget(&mut self, id: TorrentId) {
        self.torrents.remove(&id);
    }
}

/// Ids at or above this belong to pending (metadata-fetching) adds, never to librqbit.
const PENDING_ID_BASE: TorrentId = 1 << 40;

struct Pending {
    info_hash: Option<String>,
    name: Option<String>,
    /// Set when resolving failed; the row stays (in `Error`) until removed.
    error: Option<String>,
    added_at: i64,
    output_folder: String,
    /// Picking mode to apply once the torrent is added (can be toggled meanwhile).
    sequential: bool,
    abort: Option<tokio::task::AbortHandle>,
}

#[derive(Default)]
struct Counters {
    torrents: HashMap<String, TorrentRecord>,
    downloaded: Counter,
    uploaded: Counter,
}

impl LocalBackend {
    /// Starts the engine on a dedicated multi-threaded tokio runtime.
    /// Blocks until the session is up. Must not be called from inside a tokio runtime.
    pub fn start_with_own_runtime(config: EngineConfig) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("storm-engine").build()?;
        let mut backend = rt.block_on(Self::start(config))?;
        backend._runtime = Some(Arc::new(rt));
        Ok(backend)
    }

    /// Starts the engine on the current tokio runtime.
    pub async fn start(config: EngineConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&config.data_dir).with_context(|| format!("creating {}", config.data_dir.display()))?;
        let db = Db::open(&config.data_dir.join("storm.db"))?;

        let defaults = Settings {
            download_dir: config.default_download_dir.to_string_lossy().into_owned(),
            listen_port: pick_listen_port(),
            enable_upnp: true,
            down_limit_bps: None,
            up_limit_bps: None,
        };
        let settings = db.load_settings(defaults)?;
        // Persist so a randomly picked port stays stable across runs.
        db.save_settings(&settings)?;

        let counters = Counters {
            torrents: db
                .load_torrents()?
                .into_iter()
                .map(|(k, mut v)| {
                    v.uploaded = v.uploaded.carried_over();
                    v.downloaded = v.downloaded.carried_over();
                    (k, v)
                })
                .collect(),
            downloaded: db.load_counter("downloaded")?.carried_over(),
            uploaded: db.load_counter("uploaded")?.carried_over(),
        };

        let opts = SessionOptions {
            // Keep DHT state per data dir (librqbit's default is one global file), so the
            // app and a server on the same machine don't fight over it.
            dht: Some(DhtSessionConfig {
                persistence: Some(librqbit::dht::DhtPersistenceConfig {
                    config_filename: Some(config.data_dir.join("dht.json")),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json { folder: Some(config.data_dir.join("session")) }),
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpAndUtp,
                listen_addr: SocketAddr::from((Ipv6Addr::UNSPECIFIED, settings.listen_port)),
                enable_upnp_port_forwarding: settings.enable_upnp,
                ..Default::default()
            }),
            ratelimits: librqbit::limits::LimitsConfig {
                download_bps: settings.down_limit_bps.and_then(NonZeroU32::new),
                upload_bps: settings.up_limit_bps.and_then(NonZeroU32::new),
            },
            client_name_and_version: Some(format!("storm {}", env!("CARGO_PKG_VERSION"))),
            ..Default::default()
        };
        let session = Session::new_with_opts(PathBuf::from(&settings.download_dir), opts)
            .await
            .context("starting librqbit session")?;

        let (events, _) = broadcast::channel(16);
        let inner = Arc::new(Inner {
            handle: Handle::current(),
            api: Api::new(session.clone(), None),
            session,
            db,
            settings: Mutex::new(settings),
            counters: Mutex::new(counters),
            events,
            pending: Mutex::new(HashMap::new()),
            next_pending: AtomicU64::new(0),
            peer_rates: Mutex::new(PeerRates::default()),
        });
        inner.restore_picking();
        tokio::spawn(ticker(Arc::downgrade(&inner)));
        Ok(Self { inner, _runtime: None })
    }

    /// Stops the session, flushing state to disk.
    pub async fn shutdown(&self) {
        let inner = self.inner.clone();
        let _ = self
            .inner
            .handle
            .spawn(async move {
                inner.flush_counters();
                inner.session.stop().await;
            })
            .await;
    }

    /// Opens a torrent file for reading (used by the server's download route).
    /// Returns the file's name, its length and a reader. Pieces that aren't downloaded
    /// yet are fetched with priority, so this also works for incomplete torrents.
    pub async fn open_file(
        &self,
        id: TorrentId,
        file_index: usize,
    ) -> proto::Result<(String, u64, impl tokio::io::AsyncRead + Send + Unpin + 'static)> {
        self.run(move |inner| async move {
            let h = inner.handle(id)?;
            let name = h
                .with_metadata(|m| {
                    m.file_infos
                        .get(file_index)
                        .map(|f| f.relative_filename.file_name().map(|n| n.to_string_lossy().into_owned()))
                })
                .map_err(|_| Error::bad_request("torrent metadata isn't available yet"))?
                .ok_or_else(|| Error::not_found(format!("no file {file_index} in torrent {id}")))?
                .unwrap_or_else(|| format!("file-{file_index}"));
            let stream = inner
                .api
                .api_stream(TorrentIdOrHash::Id(id as usize), file_index)
                .await
                .map_err(|e| Error::bad_request(format!("{e:#}")))?;
            let len = stream.len();
            Ok((name, len, stream))
        })
        .await
    }

    /// Runs `f` on the engine's runtime and awaits it from any executor.
    async fn run<T, F, Fut>(&self, f: F) -> proto::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Inner>) -> Fut,
        Fut: Future<Output = proto::Result<T>> + Send + 'static,
    {
        let fut = f(self.inner.clone());
        self.inner.handle.spawn(fut).await.map_err(|e| Error::internal(format!("engine task failed: {e}")))?
    }
}

/// Once a second: broadcast a snapshot. Every [`SAMPLE_EVERY`]: write history and counters.
async fn ticker(inner: std::sync::Weak<Inner>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks: u64 = 0;
    loop {
        interval.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        let (torrents, session) = inner.snapshot();
        ticks += 1;
        if ticks % SAMPLE_EVERY.as_secs() == 0 {
            let now = unix_now();
            let sample = StatsSample { ts: now, down_bps: session.down_bps, up_bps: session.up_bps };
            if let Err(e) = inner.db.insert_sample(sample) {
                tracing::warn!("writing stats sample: {e:#}");
            }
            inner.flush_counters();
            if ticks % 3600 == 0 {
                let _ = inner.db.prune_samples_before(now - HISTORY_RETENTION_SECS);
            }
        }
        // No receivers is fine.
        let _ = inner.events.send(Event::Snapshot { torrents, session });
    }
}

impl Inner {
    fn flush_counters(&self) {
        let c = self.counters.lock().unwrap();
        let res = self
            .db
            .save_torrents(c.torrents.iter())
            .and_then(|_| self.db.save_counter("downloaded", c.downloaded))
            .and_then(|_| self.db.save_counter("uploaded", c.uploaded));
        if let Err(e) = res {
            tracing::warn!("saving counters: {e:#}");
        }
    }

    fn handle(&self, id: TorrentId) -> proto::Result<Arc<ManagedTorrent>> {
        if id >= PENDING_ID_BASE {
            return Err(if self.pending.lock().unwrap().contains_key(&id) {
                Error::bad_request("torrent is still fetching metadata")
            } else {
                Error::not_found(format!("no torrent with id {id}"))
            });
        }
        self.session
            .get(TorrentIdOrHash::Id(id as usize))
            .ok_or_else(|| Error::not_found(format!("no torrent with id {id}")))
    }

    fn snapshot(&self) -> (Vec<TorrentSummary>, SessionStats) {
        let handles: Vec<Arc<ManagedTorrent>> = self.session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
        let mut torrents: Vec<TorrentSummary> = handles.iter().map(|h| self.summarize(h)).collect();
        torrents.sort_by_key(|t| t.id);
        torrents.extend(self.pending_summaries());

        let s = self.session.stats_snapshot();
        let mut c = self.counters.lock().unwrap();
        let total_downloaded = c.downloaded.observe(s.counters.fetched_bytes);
        let total_uploaded = c.uploaded.observe(s.counters.uploaded_bytes);
        let session = SessionStats {
            down_bps: mib_to_bps(s.download_speed.mbps),
            up_bps: mib_to_bps(s.upload_speed.mbps),
            session_downloaded: s.counters.fetched_bytes,
            session_uploaded: s.counters.uploaded_bytes,
            total_downloaded,
            total_uploaded,
            peers_live: s.peers.live,
            uptime_secs: s.uptime_seconds,
        };
        (torrents, session)
    }

    fn summarize(&self, h: &ManagedTorrent) -> TorrentSummary {
        let stats = h.stats();
        let info_hash = h.info_hash().as_string();
        let live = stats.live.as_ref();

        let state = match stats.state {
            TorrentStatsState::Initializing { .. } => TorrentState::Initializing,
            TorrentStatsState::Paused => TorrentState::Paused,
            TorrentStatsState::Error => TorrentState::Error,
            TorrentStatsState::Live if stats.finished => TorrentState::Seeding,
            TorrentStatsState::Live => TorrentState::Downloading,
        };
        let down_bps = live.map(|l| mib_to_bps(l.download_speed.mbps)).unwrap_or(0);
        let up_bps = live.map(|l| mib_to_bps(l.upload_speed.mbps)).unwrap_or(0);
        let remaining = stats.total_bytes.saturating_sub(stats.progress_bytes);
        let eta_secs = (state == TorrentState::Downloading && down_bps > 0).then(|| remaining / down_bps);
        let peers = live.map(|l| &l.snapshot.peer_stats);

        let now = unix_now();
        let mut c = self.counters.lock().unwrap();
        let rec = c
            .torrents
            .entry(info_hash.clone())
            .or_insert_with(|| TorrentRecord { added_at: now, ..Default::default() });
        if stats.finished && rec.completed_at.is_none() && stats.total_bytes > 0 {
            rec.completed_at = Some(now);
        }
        let uploaded_bytes = rec.uploaded.observe(stats.uploaded_bytes);
        let downloaded_bytes = match live {
            Some(l) => rec.downloaded.observe(l.snapshot.fetched_bytes),
            None => rec.downloaded.total(),
        };

        TorrentSummary {
            id: h.id() as TorrentId,
            info_hash,
            name: h.name(),
            state,
            error: stats.error.clone(),
            total_bytes: stats.total_bytes,
            progress_bytes: stats.progress_bytes,
            uploaded_bytes,
            downloaded_bytes,
            down_bps,
            up_bps,
            eta_secs,
            peers_live: peers.map(|p| p.live).unwrap_or(0),
            peers_seen: peers.map(|p| p.seen).unwrap_or(0),
            seeders_live: peers.map(|p| p.live_seeders).unwrap_or(0),
            output_folder: h.output_folder().to_string_lossy().into_owned(),
            added_at: Some(rec.added_at),
            completed_at: rec.completed_at,
            sequential: h.picker_settings().mode == PiecePicking::Sequential,
        }
    }

    fn details(&self, id: TorrentId) -> proto::Result<TorrentDetails> {
        if let Some(summary) = self.pending_summary(id) {
            return Ok(TorrentDetails {
                summary,
                files: Vec::new(),
                trackers: Vec::new(),
                piece_length: None,
                piece_count: None,
                comment: None,
                created_by: None,
                creation_date: None,
                have_pieces: None,
            });
        }
        let h = self.handle(id)?;
        let summary = self.summarize(&h);
        let stats = h.stats();
        let only: Option<HashSet<usize>> = h.only_files().map(|v| v.into_iter().collect());
        let picker = h.picker_settings();
        let mut trackers: Vec<String> = h.shared().trackers.iter().map(|u| u.to_string()).collect();
        trackers.sort();

        let meta = h.with_metadata(|m| {
            let files = m
                .file_infos
                .iter()
                .enumerate()
                .filter(|(_, f)| !f.attrs.padding)
                .map(|(index, f)| TorrentFile {
                    index,
                    path: f.relative_filename.to_string_lossy().replace('\\', "/"),
                    length: f.len,
                    progress_bytes: stats.file_progress.get(index).copied().unwrap_or(0),
                    included: only.as_ref().is_none_or(|o| o.contains(&index)),
                    priority: from_rq_priority(picker.priority_of(index)),
                    offset: f.offset_in_torrent,
                })
                .collect::<Vec<_>>();
            let lengths = m.info.lengths();
            (files, lengths.default_piece_length(), lengths.total_pieces(), MetainfoExtras::parse(&m.torrent_bytes))
        });
        let (files, piece_length, piece_count, extra) = match meta {
            Ok((f, pl, pc, extra)) => (f, Some(pl), Some(pc), extra),
            // Magnet whose metadata isn't resolved yet.
            Err(_) => (Vec::new(), None, None, MetainfoExtras::default()),
        };
        Ok(TorrentDetails {
            summary,
            files,
            trackers,
            piece_length,
            piece_count,
            comment: extra.comment,
            created_by: extra.created_by,
            creation_date: extra.creation_date,
            have_pieces: h.have_pieces().map(proto::PieceMap),
        })
    }

    fn peers(&self, id: TorrentId) -> proto::Result<Vec<PeerInfo>> {
        if self.pending.lock().unwrap().contains_key(&id) {
            return Ok(Vec::new());
        }
        self.handle(id)?;
        let snap = match self.api.api_peer_stats(TorrentIdOrHash::Id(id as usize), Default::default()) {
            Ok(s) => s,
            // Not live (paused / initializing): no peers.
            Err(_) => return Ok(Vec::new()),
        };
        let mut peers: Vec<PeerInfo> = snap
            .peers
            .into_iter()
            .map(|(addr, p)| PeerInfo {
                addr,
                client: p.client_name,
                transport: p
                    .conn_kind
                    .and_then(|k| serde_json::to_value(k).ok())
                    .and_then(|v| v.as_str().map(|s| s.to_lowercase())),
                state: p.state.to_string(),
                downloaded_bytes: p.counters.fetched_bytes,
                uploaded_bytes: p.counters.uploaded_bytes,
                down_bps: 0,
                up_bps: 0,
                pieces_downloaded: p.counters.downloaded_and_checked_pieces,
                errors: p.counters.errors,
            })
            .collect();
        self.peer_rates.lock().unwrap().apply(id, Instant::now(), &mut peers);
        peers.sort_by(|a, b| {
            b.down_bps.cmp(&a.down_bps).then(b.downloaded_bytes.cmp(&a.downloaded_bytes)).then(a.addr.cmp(&b.addr))
        });
        Ok(peers)
    }

    async fn add(self: Arc<Self>, req: AddTorrentRequest) -> proto::Result<proto::AddTorrentResponse> {
        let base =
            PathBuf::from(req.output_folder.unwrap_or_else(|| self.settings.lock().unwrap().download_dir.clone()));
        match req.source {
            // Parsing a .torrent is local and fast: add synchronously.
            TorrentSource::File { bytes } => {
                self.resolve_and_add(
                    AddTorrent::from_bytes(bytes),
                    base,
                    req.paused,
                    req.only_files,
                    Vec::new(),
                    req.sequential,
                )
                .await
            }
            // Magnets (and remote .torrent URLs) may take a long time to resolve: show a
            // pending row right away and finish in the background.
            TorrentSource::Url { url } => {
                let magnet = librqbit::Magnet::parse(&url).ok();
                let info_hash = magnet.as_ref().and_then(|m| m.as_id20());
                if let Some(h) = info_hash
                    && let Some(existing) = self.session.get(TorrentIdOrHash::Hash(h))
                {
                    return Ok(proto::AddTorrentResponse {
                        id: existing.id() as TorrentId,
                        info_hash: h.as_string(),
                        already_existed: true,
                    });
                }
                let info_hash = info_hash.map(|h| h.as_string());
                if let Some(h) = &info_hash {
                    let pending = self.pending.lock().unwrap();
                    if let Some((&id, _)) = pending.iter().find(|(_, p)| p.info_hash.as_ref() == Some(h)) {
                        return Ok(proto::AddTorrentResponse { id, info_hash: h.clone(), already_existed: true });
                    }
                }

                let id = PENDING_ID_BASE + self.next_pending.fetch_add(1, Ordering::Relaxed);
                let trackers = magnet.as_ref().map(|m| m.trackers.clone()).unwrap_or_default();
                self.pending.lock().unwrap().insert(
                    id,
                    Pending {
                        info_hash: info_hash.clone(),
                        name: magnet.and_then(|m| m.name),
                        error: None,
                        added_at: unix_now(),
                        output_folder: base.to_string_lossy().into_owned(),
                        sequential: req.sequential,
                        abort: None,
                    },
                );
                let inner = self.clone();
                let task = tokio::spawn(async move {
                    let res = inner
                        .resolve_and_add(
                            AddTorrent::from_url(url),
                            base,
                            req.paused,
                            req.only_files,
                            trackers,
                            req.sequential,
                        )
                        .await;
                    match res {
                        Ok(added) => {
                            let toggled = inner
                                .pending
                                .lock()
                                .unwrap()
                                .remove(&id)
                                .map(|p| p.sequential)
                                .filter(|&seq| seq != req.sequential);
                            // Sequential was toggled while the metadata was being fetched.
                            if let Some(seq) = toggled
                                && !added.already_existed
                                && let Err(e) = inner.update_picking(added.id, |p, _| {
                                    p.sequential = seq;
                                    Ok(())
                                })
                            {
                                tracing::warn!("applying picking mode to torrent {}: {e}", added.id);
                            }
                        }
                        Err(e) => {
                            if let Some(p) = inner.pending.lock().unwrap().get_mut(&id) {
                                p.error = Some(e.message);
                                p.abort = None;
                            }
                        }
                    }
                });
                if let Some(p) = self.pending.lock().unwrap().get_mut(&id) {
                    p.abort = Some(task.abort_handle());
                }
                Ok(proto::AddTorrentResponse { id, info_hash: info_hash.unwrap_or_default(), already_existed: false })
            }
        }
    }

    /// Fetches metadata ("list only"), picks the output folder the same way librqbit
    /// would for its default folder (multi-file torrents get a subfolder), then adds.
    async fn resolve_and_add(
        &self,
        add: AddTorrent<'static>,
        base: PathBuf,
        paused: bool,
        only_files: Option<Vec<usize>>,
        trackers: Vec<String>,
        sequential: bool,
    ) -> proto::Result<proto::AddTorrentResponse> {
        let bad = |e: anyhow::Error| Error::bad_request(format!("{e:#}"));
        let listed = self
            .session
            .add_torrent(add, Some(AddTorrentOptions { list_only: true, ..Default::default() }))
            .await
            .map_err(bad)?;
        let listed = match listed {
            librqbit::AddTorrentResponse::ListOnly(l) => l,
            librqbit::AddTorrentResponse::AlreadyManaged(id, h) | librqbit::AddTorrentResponse::Added(id, h) => {
                return Ok(proto::AddTorrentResponse {
                    id: id as TorrentId,
                    info_hash: h.info_hash().as_string(),
                    already_existed: true,
                });
            }
        };
        let output_folder = output_folder_for(&listed.info, &base).map_err(bad)?;
        let opts = AddTorrentOptions {
            paused,
            only_files: only_files.or(listed.only_files),
            output_folder: Some(output_folder.to_string_lossy().into_owned()),
            // Lets a torrent pick up files already on disk (re-adding, moving between clients).
            overwrite: true,
            initial_peers: (!listed.seen_peers.is_empty()).then_some(listed.seen_peers),
            trackers: (!trackers.is_empty()).then_some(trackers),
            picker: Some(to_rq(&Picking { sequential, file_priorities: Vec::new() })),
            ..Default::default()
        };
        let resp =
            self.session.add_torrent(AddTorrent::from_bytes(listed.torrent_bytes), Some(opts)).await.map_err(bad)?;
        let (id, handle, already_existed) = match resp {
            librqbit::AddTorrentResponse::Added(id, h) => (id, h, false),
            librqbit::AddTorrentResponse::AlreadyManaged(id, h) => (id, h, true),
            librqbit::AddTorrentResponse::ListOnly(_) => return Err(Error::internal("unexpected list-only response")),
        };
        if sequential && !already_existed {
            self.save_picking(&handle);
        }
        Ok(proto::AddTorrentResponse {
            id: id as TorrentId,
            info_hash: handle.info_hash().as_string(),
            already_existed,
        })
    }

    /// Reapplies the stored picking settings to the torrents librqbit restored.
    fn restore_picking(&self) {
        let stored = match self.db.load_picking() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("loading piece picking settings: {e:#}");
                return;
            }
        };
        let handles: Vec<Arc<ManagedTorrent>> = self.session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
        for h in handles {
            if let Some(p) = stored.get(&h.info_hash().as_string())
                && let Err(e) = h.set_picker_settings(to_rq(p))
            {
                tracing::warn!("restoring piece picking of {}: {e:#}", h.info_hash().as_string());
            }
        }
    }

    fn save_picking(&self, h: &ManagedTorrent) {
        let p = from_rq(&h.picker_settings());
        if let Err(e) = self.db.save_picking(&h.info_hash().as_string(), &p) {
            tracing::warn!("saving piece picking settings: {e:#}");
        }
    }

    /// Edits a torrent's picking settings with `f`, which also gets the torrent's
    /// file count (`None` while its metadata isn't known), then applies and persists them.
    fn update_picking(
        &self,
        id: TorrentId,
        f: impl FnOnce(&mut Picking, Option<usize>) -> proto::Result<()>,
    ) -> proto::Result<()> {
        if let Some(p) = self.pending.lock().unwrap().get_mut(&id) {
            // Still fetching metadata: only the mode can be chosen, applied once added.
            let mut picking = Picking { sequential: p.sequential, file_priorities: Vec::new() };
            f(&mut picking, None)?;
            p.sequential = picking.sequential;
            return Ok(());
        }
        let h = self.handle(id)?;
        let file_count = h.with_metadata(|m| m.file_infos.len()).ok();
        let mut picking = from_rq(&h.picker_settings());
        f(&mut picking, file_count)?;
        h.set_picker_settings(to_rq(&picking)).map_err(|e| Error::bad_request(format!("{e:#}")))?;
        self.save_picking(&h);
        Ok(())
    }

    fn pending_summaries(&self) -> Vec<TorrentSummary> {
        let pending = self.pending.lock().unwrap();
        let mut out: Vec<TorrentSummary> = pending
            .iter()
            .map(|(&id, p)| TorrentSummary {
                id,
                info_hash: p.info_hash.clone().unwrap_or_default(),
                name: p.name.clone(),
                state: if p.error.is_some() { TorrentState::Error } else { TorrentState::FetchingMetadata },
                error: p.error.clone(),
                total_bytes: 0,
                progress_bytes: 0,
                uploaded_bytes: 0,
                downloaded_bytes: 0,
                down_bps: 0,
                up_bps: 0,
                eta_secs: None,
                peers_live: 0,
                peers_seen: 0,
                seeders_live: 0,
                output_folder: p.output_folder.clone(),
                added_at: Some(p.added_at),
                completed_at: None,
                sequential: p.sequential,
            })
            .collect();
        out.sort_by_key(|t| t.id);
        out
    }

    fn pending_summary(&self, id: TorrentId) -> Option<TorrentSummary> {
        self.pending_summaries().into_iter().find(|t| t.id == id)
    }

    fn apply_patch(&self, patch: SettingsPatch) -> proto::Result<Settings> {
        let mut s = self.settings.lock().unwrap();
        let mut next = s.clone();
        if let Some(v) = patch.download_dir {
            if v.trim().is_empty() {
                return Err(Error::bad_request("download_dir can't be empty"));
            }
            next.download_dir = v;
        }
        if let Some(v) = patch.listen_port {
            next.listen_port = v;
        }
        if let Some(v) = patch.enable_upnp {
            next.enable_upnp = v;
        }
        if let Some(v) = patch.down_limit_bps {
            next.down_limit_bps = v.filter(|&x| x > 0);
        }
        if let Some(v) = patch.up_limit_bps {
            next.up_limit_bps = v.filter(|&x| x > 0);
        }
        self.db.save_settings(&next).map_err(|e| Error::internal(format!("{e:#}")))?;
        // Live-applicable settings. Port/UPnP take effect on next start.
        self.session.ratelimits.set_download_bps(next.down_limit_bps.and_then(NonZeroU32::new));
        self.session.ratelimits.set_upload_bps(next.up_limit_bps.and_then(NonZeroU32::new));
        *s = next.clone();
        drop(s);
        let _ = self.events.send(Event::SettingsChanged { settings: next.clone() });
        Ok(next)
    }
}

#[async_trait::async_trait]
impl Backend for LocalBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Local,
            api_version: proto::API_VERSION,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            librqbit_version: librqbit::version().to_string(),
        }
    }

    async fn subscribe(&self) -> proto::Result<EventStream> {
        self.run(|inner| async move {
            // Subscribe before snapshotting so nothing is missed in between.
            let rx = inner.events.subscribe();
            let (torrents, session) = inner.snapshot();
            let first = futures::stream::once(async move { Event::Snapshot { torrents, session } });
            // A lagging subscriber just skips to the next snapshot.
            let rest = BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });
            Ok(Box::pin(first.chain(rest)) as EventStream)
        })
        .await
    }

    async fn list_torrents(&self) -> proto::Result<Vec<TorrentSummary>> {
        self.run(|inner| async move { Ok(inner.snapshot().0) }).await
    }

    async fn torrent_details(&self, id: TorrentId) -> proto::Result<TorrentDetails> {
        self.run(move |inner| async move { inner.details(id) }).await
    }

    async fn torrent_peers(&self, id: TorrentId) -> proto::Result<Vec<PeerInfo>> {
        self.run(move |inner| async move { inner.peers(id) }).await
    }

    async fn add_torrent(&self, req: AddTorrentRequest) -> proto::Result<proto::AddTorrentResponse> {
        self.run(move |inner| async move { inner.add(req).await }).await
    }

    async fn pause(&self, id: TorrentId) -> proto::Result<()> {
        self.run(move |inner| async move {
            let h = inner.handle(id)?;
            inner.session.pause(&h).await.map_err(|e| Error::bad_request(format!("{e:#}")))
        })
        .await
    }

    async fn resume(&self, id: TorrentId) -> proto::Result<()> {
        self.run(move |inner| async move {
            let h = inner.handle(id)?;
            inner.session.unpause(&h).await.map_err(|e| Error::bad_request(format!("{e:#}")))
        })
        .await
    }

    async fn remove(&self, id: TorrentId, delete_files: bool) -> proto::Result<()> {
        self.run(move |inner| async move {
            // Cancels an in-flight metadata fetch.
            if let Some(p) = inner.pending.lock().unwrap().remove(&id) {
                if let Some(a) = p.abort {
                    a.abort();
                }
                return Ok(());
            }
            let h = inner.handle(id)?;
            let hash = h.info_hash().as_string();
            drop(h);
            inner
                .session
                .delete(TorrentIdOrHash::Id(id as usize), delete_files)
                .await
                .map_err(|e| Error::internal(format!("{e:#}")))?;
            inner.counters.lock().unwrap().torrents.remove(&hash);
            inner.peer_rates.lock().unwrap().forget(id);
            let _ = inner.db.delete_torrent(&hash);
            Ok(())
        })
        .await
    }

    async fn set_included_files(&self, id: TorrentId, files: Vec<usize>) -> proto::Result<()> {
        self.run(move |inner| async move {
            if files.is_empty() {
                return Err(Error::bad_request("select at least one file"));
            }
            let h = inner.handle(id)?;
            let set: HashSet<usize> = files.into_iter().collect();
            inner.session.update_only_files(&h, &set).await.map_err(|e| Error::bad_request(format!("{e:#}")))
        })
        .await
    }

    async fn set_sequential(&self, id: TorrentId, sequential: bool) -> proto::Result<()> {
        self.run(move |inner| async move {
            inner.update_picking(id, |p, _| {
                p.sequential = sequential;
                Ok(())
            })
        })
        .await
    }

    async fn set_file_priority(&self, id: TorrentId, files: Vec<usize>, priority: FilePriority) -> proto::Result<()> {
        self.run(move |inner| async move {
            inner.update_picking(id, |p, file_count| {
                let n = file_count.ok_or_else(|| Error::bad_request("torrent metadata isn't available yet"))?;
                if let Some(bad) = files.iter().find(|&&i| i >= n) {
                    return Err(Error::bad_request(format!("no file {bad} in torrent {id}")));
                }
                p.file_priorities.resize(n, FilePriority::Normal);
                for i in files {
                    p.file_priorities[i] = priority;
                }
                Ok(())
            })
        })
        .await
    }

    async fn session_stats(&self) -> proto::Result<SessionStats> {
        self.run(|inner| async move { Ok(inner.snapshot().1) }).await
    }

    async fn stats_history(&self, since: i64) -> proto::Result<Vec<StatsSample>> {
        self.run(
            move |inner| async move { inner.db.samples_since(since).map_err(|e| Error::internal(format!("{e:#}"))) },
        )
        .await
    }

    async fn settings(&self) -> proto::Result<Settings> {
        Ok(self.inner.settings.lock().unwrap().clone())
    }

    async fn update_settings(&self, patch: SettingsPatch) -> proto::Result<Settings> {
        self.run(move |inner| async move { inner.apply_patch(patch) }).await
    }

    async fn list_dirs(&self, path: Option<String>) -> proto::Result<DirListing> {
        let start = path.unwrap_or_else(|| self.inner.settings.lock().unwrap().download_dir.clone());
        self.run(move |_| async move {
            tokio::task::spawn_blocking(move || fs_browse::list_dirs(&start))
                .await
                .map_err(|e| Error::internal(e.to_string()))?
        })
        .await
    }

    async fn create_dir(&self, path: String) -> proto::Result<()> {
        self.run(move |_| async move {
            tokio::fs::create_dir_all(&path).await.map_err(|e| Error::bad_request(format!("creating {path}: {e}")))
        })
        .await
    }
}

fn to_rq(p: &Picking) -> PickerSettings {
    PickerSettings {
        mode: if p.sequential { PiecePicking::Sequential } else { PiecePicking::RarestFirst },
        file_priorities: p
            .file_priorities
            .iter()
            .map(|p| match p {
                FilePriority::Low => librqbit::FilePriority::Low,
                FilePriority::Normal => librqbit::FilePriority::Normal,
                FilePriority::High => librqbit::FilePriority::High,
            })
            .collect(),
    }
}

fn from_rq_priority(p: librqbit::FilePriority) -> FilePriority {
    match p {
        librqbit::FilePriority::Low => FilePriority::Low,
        librqbit::FilePriority::Normal => FilePriority::Normal,
        librqbit::FilePriority::High => FilePriority::High,
    }
}

fn from_rq(s: &PickerSettings) -> Picking {
    Picking {
        sequential: s.mode == PiecePicking::Sequential,
        file_priorities: s.file_priorities.iter().copied().map(from_rq_priority).collect(),
    }
}

/// Same rule librqbit applies for its default folder: single-file torrents go straight
/// into `base`, multi-file torrents into `base/<torrent name>` (falling back to the
/// longest file's stem). Rejects names that would escape `base`.
fn output_folder_for(
    info: &librqbit::ValidatedTorrentMetaV1Info<librqbit::ByteBufOwned>,
    base: &Path,
) -> anyhow::Result<PathBuf> {
    let files: Vec<(PathBuf, u64)> =
        info.iter_file_details().map(|fd| Ok((fd.filename.to_pathbuf(), fd.len))).collect::<anyhow::Result<_>>()?;
    if files.len() < 2 {
        return Ok(base.to_path_buf());
    }
    let sub = match info.name().filter(|n| !n.is_empty()) {
        Some(name) => PathBuf::from(name.as_ref()),
        None => files
            .iter()
            .max_by_key(|(_, len)| *len)
            .and_then(|(p, _)| p.file_stem())
            .map(PathBuf::from)
            .context("can't determine a folder name for the torrent")?,
    };
    if sub.components().any(|c| !matches!(c, Component::Normal(_))) {
        anyhow::bail!("path traversal in torrent name detected");
    }
    Ok(base.join(sub))
}

fn mib_to_bps(mib_per_sec: f64) -> u64 {
    (mib_per_sec * 1024.0 * 1024.0).round().max(0.0) as u64
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A port in the dynamic range, picked once and then persisted.
fn pick_listen_port() -> u16 {
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    49152 + (nanos % (65535 - 49152)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(addr: &str, down: u64, up: u64) -> PeerInfo {
        PeerInfo {
            addr: addr.into(),
            client: None,
            transport: None,
            state: "live".into(),
            downloaded_bytes: down,
            uploaded_bytes: up,
            down_bps: 0,
            up_bps: 0,
            pieces_downloaded: 0,
            errors: 0,
        }
    }

    #[test]
    fn peer_rates_diff_cumulative_counters() {
        let mut r = PeerRates::default();
        let t0 = Instant::now();
        let mut ps = vec![peer("a", 1000, 0), peer("b", 0, 500)];
        r.apply(1, t0, &mut ps);
        assert!(ps.iter().all(|p| p.down_bps == 0 && p.up_bps == 0), "first sample has no rate");

        let mut ps = vec![peer("a", 3000, 0), peer("b", 0, 1500)];
        r.apply(1, t0 + Duration::from_secs(2), &mut ps);
        assert_eq!((ps[0].down_bps, ps[0].up_bps), (1000, 0));
        assert_eq!((ps[1].down_bps, ps[1].up_bps), (0, 500));

        // Too soon: previous rate is reported, baseline kept.
        let mut ps = vec![peer("a", 3100, 0)];
        r.apply(1, t0 + Duration::from_millis(2100), &mut ps);
        assert_eq!(ps[0].down_bps, 1000);
        // "b" vanished from the snapshot and is pruned.
        assert!(!r.torrents[&1].peers.contains_key("b"));

        let mut ps = vec![peer("a", 4000, 0)];
        r.apply(1, t0 + Duration::from_secs(3), &mut ps);
        assert_eq!(ps[0].down_bps, 1000);

        // Counters reset (peer re-added): no bogus spike.
        let mut ps = vec![peer("a", 10, 0)];
        r.apply(1, t0 + Duration::from_secs(4), &mut ps);
        assert_eq!(ps[0].down_bps, 0);

        // Torrents nobody polls are forgotten.
        r.apply(2, t0 + Duration::from_secs(120), &mut []);
        assert!(!r.torrents.contains_key(&1));
    }

    #[test]
    fn metainfo_extras_tolerate_garbage() {
        assert_eq!(MetainfoExtras::parse(b""), MetainfoExtras::default());
        assert_eq!(MetainfoExtras::parse(b"not bencode"), MetainfoExtras::default());
    }
}
