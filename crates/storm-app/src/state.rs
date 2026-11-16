//! App-wide live state fed by the backend's event stream.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{App, Context, Entity, Task, Window};
use storm_proto::{Backend, Event, SessionStats, Settings, StatsSample, TorrentId, TorrentState, TorrentSummary};

use crate::ui::spawn_op;

/// How much speed history the status-bar chart keeps.
pub const HISTORY_SECS: i64 = 10 * 60;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    /// Stream ended or subscribing failed; retrying after `retry_in`.
    Disconnected {
        error: Option<String>,
        retry_in: Duration,
    },
}

pub struct AppState {
    backend: Arc<dyn Backend>,
    pub torrents: Vec<TorrentSummary>,
    pub session: SessionStats,
    pub settings: Option<Settings>,
    pub connection: Connection,
    /// Oldest first. Seeded from `stats_history`, then one sample per snapshot.
    pub history: VecDeque<StatsSample>,
    _subscription: Task<()>,
}

impl AppState {
    pub fn new(backend: Arc<dyn Backend>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            torrents: Vec::new(),
            session: SessionStats::default(),
            settings: None,
            connection: Connection::Connecting,
            history: VecDeque::new(),
            _subscription: Task::ready(()),
        };
        this._subscription = this.run_subscription(cx);
        this
    }

    pub fn torrent(&self, id: TorrentId) -> Option<&TorrentSummary> {
        self.torrents.iter().find(|t| t.id == id)
    }

    /// Subscribes, applies events until the stream ends, then retries with
    /// exponential backoff. On every (re)connect it also refreshes settings and
    /// re-seeds the speed history, since a remote server may have restarted.
    fn run_subscription(&self, cx: &mut Context<Self>) -> Task<()> {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let mut backoff = BACKOFF_MIN;
            loop {
                let _ = this.update(cx, |s, cx| {
                    if !matches!(s.connection, Connection::Connected) {
                        s.connection = Connection::Connecting;
                        cx.notify();
                    }
                });
                let error = match backend.subscribe().await {
                    Ok(mut stream) => {
                        backoff = BACKOFF_MIN;
                        let since = unix_now() - HISTORY_SECS;
                        let history = backend.stats_history(since).await.unwrap_or_default();
                        let settings = backend.settings().await.ok();
                        let alive = this.update(cx, |s, cx| {
                            s.connection = Connection::Connected;
                            s.history = history.into_iter().collect();
                            if settings.is_some() {
                                s.settings = settings;
                            }
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                        while let Some(event) = stream.next().await {
                            if this.update(cx, |s, cx| s.apply(event, cx)).is_err() {
                                return;
                            }
                        }
                        tracing::warn!("backend event stream ended");
                        None
                    }
                    Err(e) => {
                        tracing::warn!("subscribing to backend failed: {e}");
                        Some(e.to_string())
                    }
                };
                let alive = this.update(cx, |s, cx| {
                    s.connection = Connection::Disconnected { error, retry_in: backoff };
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor().timer(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        })
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Snapshot { torrents, session } => {
                push_sample(
                    &mut self.history,
                    StatsSample { ts: unix_now(), down_bps: session.down_bps, up_bps: session.up_bps },
                );
                self.torrents = torrents;
                self.session = session;
            }
            Event::SettingsChanged { settings } => self.settings = Some(settings),
        }
        cx.notify();
    }
}

/// Switches torrents between sequential and rarest-first piece picking. The
/// list reflects it right away; the next snapshot confirms it.
pub fn set_sequential(state: &Entity<AppState>, ids: Vec<TorrentId>, on: bool, window: &Window, cx: &mut App) {
    let backend = state.update(cx, |s, cx| {
        for t in s.torrents.iter_mut().filter(|t| ids.contains(&t.id)) {
            t.sequential = on;
        }
        cx.notify();
        s.backend.clone()
    });
    for id in ids {
        let b = backend.clone();
        spawn_op(window, cx, "Changing download order", async move { b.set_sequential(id, on).await }, |_, _, _| {});
    }
}

/// The quick filters above the torrent list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Downloading,
    Seeding,
    Paused,
    Completed,
    Error,
}

impl Filter {
    pub const ALL: [Filter; 6] =
        [Filter::All, Filter::Downloading, Filter::Seeding, Filter::Paused, Filter::Completed, Filter::Error];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Downloading => "Downloading",
            Filter::Seeding => "Seeding",
            Filter::Paused => "Paused",
            Filter::Completed => "Completed",
            Filter::Error => "Errors",
        }
    }

    /// Stable identifier for `prefs.json`.
    pub fn key(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Downloading => "downloading",
            Filter::Seeding => "seeding",
            Filter::Paused => "paused",
            Filter::Completed => "completed",
            Filter::Error => "error",
        }
    }

    pub fn from_key(key: &str) -> Option<Filter> {
        Self::ALL.into_iter().find(|f| f.key() == key)
    }

    pub fn matches(self, t: &TorrentSummary) -> bool {
        match self {
            Filter::All => true,
            Filter::Downloading => matches!(
                t.state,
                TorrentState::Downloading | TorrentState::FetchingMetadata | TorrentState::Initializing
            ),
            Filter::Seeding => t.state == TorrentState::Seeding,
            Filter::Paused => t.state == TorrentState::Paused,
            Filter::Completed => t.total_bytes > 0 && t.progress() >= 1.0,
            Filter::Error => t.state == TorrentState::Error,
        }
    }

    /// How many of `torrents` each filter (in [`Filter::ALL`] order) would show.
    pub fn counts(torrents: &[TorrentSummary]) -> [usize; 6] {
        Self::ALL.map(|f| torrents.iter().filter(|t| f.matches(t)).count())
    }
}

/// Case-insensitive search over the name and info hash. Every whitespace
/// separated word must match somewhere.
pub fn matches_query(t: &TorrentSummary, query: &str) -> bool {
    let name = t.name.as_deref().unwrap_or_default().to_lowercase();
    let hash = t.info_hash.to_lowercase();
    query.split_whitespace().map(str::to_lowercase).all(|w| name.contains(&w) || hash.starts_with(&w))
}

/// Appends a sample and drops everything older than [`HISTORY_SECS`] before it.
pub fn push_sample(history: &mut VecDeque<StatsSample>, sample: StatsSample) {
    if history.back().is_some_and(|last| last.ts > sample.ts) {
        // Clock went backwards (or server/client skew): start over rather than draw garbage.
        history.clear();
    }
    history.push_back(sample);
    let cutoff = sample.ts - HISTORY_SECS;
    while history.front().is_some_and(|s| s.ts < cutoff) {
        history.pop_front();
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ts: i64) -> StatsSample {
        StatsSample { ts, down_bps: 1, up_bps: 2 }
    }

    fn torrent(state: TorrentState, name: &str, done: u64) -> TorrentSummary {
        TorrentSummary {
            id: 1,
            info_hash: "08ada5a7a6183aae1e09d831df6748d566095a10".into(),
            name: Some(name.into()),
            state,
            error: None,
            total_bytes: 100,
            progress_bytes: done,
            uploaded_bytes: 0,
            downloaded_bytes: done,
            down_bps: 0,
            up_bps: 0,
            eta_secs: None,
            peers_live: 0,
            peers_seen: 0,
            seeders_live: 0,
            output_folder: String::new(),
            added_at: None,
            completed_at: None,
            sequential: false,
        }
    }

    #[test]
    fn filters_and_counts() {
        let ts = [
            torrent(TorrentState::Downloading, "a", 10),
            torrent(TorrentState::FetchingMetadata, "b", 0),
            torrent(TorrentState::Seeding, "c", 100),
            torrent(TorrentState::Paused, "d", 100),
            torrent(TorrentState::Error, "e", 5),
        ];
        // All, Downloading, Seeding, Paused, Completed, Error
        assert_eq!(Filter::counts(&ts), [5, 2, 1, 1, 2, 1]);
    }

    #[test]
    fn query_matches_name_words_and_hash_prefix() {
        let t = torrent(TorrentState::Downloading, "Sintel 4K Remaster", 0);
        assert!(matches_query(&t, ""));
        assert!(matches_query(&t, "sintel"));
        assert!(matches_query(&t, "4k  SINTEL"));
        assert!(matches_query(&t, "08ADA5"));
        assert!(!matches_query(&t, "ada5"));
        assert!(!matches_query(&t, "sintel 1080p"));
    }

    #[test]
    fn history_is_windowed() {
        let mut h = VecDeque::new();
        push_sample(&mut h, s(0));
        push_sample(&mut h, s(10));
        push_sample(&mut h, s(HISTORY_SECS + 5));
        assert_eq!(h.iter().map(|x| x.ts).collect::<Vec<_>>(), vec![10, HISTORY_SECS + 5]);
    }

    #[test]
    fn history_resets_when_time_goes_backwards() {
        let mut h = VecDeque::new();
        push_sample(&mut h, s(100));
        push_sample(&mut h, s(50));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].ts, 50);
    }
}
