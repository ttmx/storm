//! SQLite storage for settings, lifetime transfer counters and speed history.
//!
//! librqbit keeps its own torrent/resume state in JSON; this database only holds what
//! storm adds on top.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use storm_proto::{FilePriority, Settings, StatsSample};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL            -- JSON
);
CREATE TABLE IF NOT EXISTS torrents (
    info_hash    TEXT PRIMARY KEY,
    added_at     INTEGER NOT NULL,
    completed_at INTEGER,
    up_base      INTEGER NOT NULL DEFAULT 0,
    up_last      INTEGER NOT NULL DEFAULT 0,
    down_base    INTEGER NOT NULL DEFAULT 0,
    down_last    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS counters (
    name TEXT PRIMARY KEY,
    base INTEGER NOT NULL,
    last INTEGER NOT NULL
);
-- Piece picking per torrent; a missing row means rarest-first, all files normal.
CREATE TABLE IF NOT EXISTS picking (
    info_hash       TEXT PRIMARY KEY,
    sequential      INTEGER NOT NULL DEFAULT 0,
    file_priorities TEXT NOT NULL DEFAULT '[]'  -- JSON array of FilePriority, by file index
);
CREATE TABLE IF NOT EXISTS stats_samples (
    ts       INTEGER PRIMARY KEY,  -- unix seconds
    down_bps INTEGER NOT NULL,
    up_bps   INTEGER NOT NULL
);
"#;

/// A monotonically increasing lifetime total built from a counter that resets
/// (on restart, pause/resume...). `last` is the most recent raw reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counter {
    pub base: u64,
    pub last: u64,
}

impl Counter {
    /// Called on load: the previous run's final reading becomes part of the base.
    pub fn carried_over(self) -> Self {
        Self { base: self.base + self.last, last: 0 }
    }

    /// Feed a raw reading; returns the lifetime total.
    pub fn observe(&mut self, current: u64) -> u64 {
        if current < self.last {
            // The underlying counter was reset.
            self.base += self.last;
        }
        self.last = current;
        self.total()
    }

    pub fn total(&self) -> u64 {
        self.base + self.last
    }
}

/// How a torrent's pieces are picked (see librqbit's `piece_picking`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picking {
    pub sequential: bool,
    /// By file index; missing entries are `Normal`.
    pub file_priorities: Vec<FilePriority>,
}

impl Picking {
    pub fn is_default(&self) -> bool {
        !self.sequential && self.file_priorities.iter().all(|p| *p == FilePriority::Normal)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TorrentRecord {
    pub added_at: i64,
    pub completed_at: Option<i64>,
    pub uploaded: Counter,
    pub downloaded: Counter,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- settings -----------------------------------------------------------

    /// Loads settings, filling anything missing or unparsable from `defaults`.
    pub fn load_settings(&self, defaults: Settings) -> anyhow::Result<Settings> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
        let rows: HashMap<String, String> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;

        fn get<T: serde::de::DeserializeOwned>(rows: &HashMap<String, String>, key: &str, default: T) -> T {
            rows.get(key).and_then(|v| serde_json::from_str(v).ok()).unwrap_or(default)
        }

        Ok(Settings {
            download_dir: get(&rows, "download_dir", defaults.download_dir),
            listen_port: get(&rows, "listen_port", defaults.listen_port),
            enable_upnp: get(&rows, "enable_upnp", defaults.enable_upnp),
            down_limit_bps: get(&rows, "down_limit_bps", defaults.down_limit_bps),
            up_limit_bps: get(&rows, "up_limit_bps", defaults.up_limit_bps),
        })
    }

    pub fn save_settings(&self, s: &Settings) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?;
            let pairs: [(&str, serde_json::Value); 5] = [
                ("download_dir", serde_json::json!(s.download_dir)),
                ("listen_port", serde_json::json!(s.listen_port)),
                ("enable_upnp", serde_json::json!(s.enable_upnp)),
                ("down_limit_bps", serde_json::json!(s.down_limit_bps)),
                ("up_limit_bps", serde_json::json!(s.up_limit_bps)),
            ];
            for (k, v) in pairs {
                stmt.execute(params![k, v.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // --- counters -----------------------------------------------------------

    pub fn load_torrents(&self) -> anyhow::Result<HashMap<String, TorrentRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT info_hash, added_at, completed_at, up_base, up_last, down_base, down_last FROM torrents",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                TorrentRecord {
                    added_at: r.get(1)?,
                    completed_at: r.get(2)?,
                    uploaded: Counter { base: r.get::<_, i64>(3)? as u64, last: r.get::<_, i64>(4)? as u64 },
                    downloaded: Counter { base: r.get::<_, i64>(5)? as u64, last: r.get::<_, i64>(6)? as u64 },
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn save_torrents<'a>(
        &self,
        records: impl IntoIterator<Item = (&'a String, &'a TorrentRecord)>,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO torrents (info_hash, added_at, completed_at, up_base, up_last, down_base, down_last)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(info_hash) DO UPDATE SET
                    completed_at = excluded.completed_at,
                    up_base = excluded.up_base, up_last = excluded.up_last,
                    down_base = excluded.down_base, down_last = excluded.down_last",
            )?;
            for (hash, r) in records {
                stmt.execute(params![
                    hash,
                    r.added_at,
                    r.completed_at,
                    r.uploaded.base as i64,
                    r.uploaded.last as i64,
                    r.downloaded.base as i64,
                    r.downloaded.last as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_torrent(&self, info_hash: &str) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM torrents WHERE info_hash = ?1", [info_hash])?;
        conn.execute("DELETE FROM picking WHERE info_hash = ?1", [info_hash])?;
        Ok(())
    }

    // --- piece picking ------------------------------------------------------

    pub fn load_picking(&self) -> anyhow::Result<HashMap<String, Picking>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT info_hash, sequential, file_priorities FROM picking")?;
        let rows = stmt.query_map([], |r| {
            let prios: String = r.get(2)?;
            Ok((
                r.get::<_, String>(0)?,
                Picking { sequential: r.get(1)?, file_priorities: serde_json::from_str(&prios).unwrap_or_default() },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Stores `p`, or forgets the row when `p` is the default.
    pub fn save_picking(&self, info_hash: &str, p: &Picking) -> anyhow::Result<()> {
        let conn = self.conn();
        if p.is_default() {
            conn.execute("DELETE FROM picking WHERE info_hash = ?1", [info_hash])?;
        } else {
            conn.execute(
                "INSERT INTO picking (info_hash, sequential, file_priorities) VALUES (?1, ?2, ?3)
                 ON CONFLICT(info_hash) DO UPDATE SET
                    sequential = excluded.sequential, file_priorities = excluded.file_priorities",
                params![info_hash, p.sequential, serde_json::to_string(&p.file_priorities)?],
            )?;
        }
        Ok(())
    }

    pub fn load_counter(&self, name: &str) -> anyhow::Result<Counter> {
        Ok(self
            .conn()
            .query_row("SELECT base, last FROM counters WHERE name = ?1", [name], |r| {
                Ok(Counter { base: r.get::<_, i64>(0)? as u64, last: r.get::<_, i64>(1)? as u64 })
            })
            .optional()?
            .unwrap_or_default())
    }

    pub fn save_counter(&self, name: &str, c: Counter) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO counters (name, base, last) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET base = excluded.base, last = excluded.last",
            params![name, c.base as i64, c.last as i64],
        )?;
        Ok(())
    }

    // --- history ------------------------------------------------------------

    pub fn insert_sample(&self, s: StatsSample) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO stats_samples (ts, down_bps, up_bps) VALUES (?1, ?2, ?3)",
            params![s.ts, s.down_bps as i64, s.up_bps as i64],
        )?;
        Ok(())
    }

    pub fn samples_since(&self, since: i64) -> anyhow::Result<Vec<StatsSample>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT ts, down_bps, up_bps FROM stats_samples WHERE ts > ?1 ORDER BY ts")?;
        let rows = stmt.query_map([since], |r| {
            Ok(StatsSample { ts: r.get(0)?, down_bps: r.get::<_, i64>(1)? as u64, up_bps: r.get::<_, i64>(2)? as u64 })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn prune_samples_before(&self, ts: i64) -> anyhow::Result<()> {
        self.conn().execute("DELETE FROM stats_samples WHERE ts < ?1", [ts])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_survives_resets_and_restarts() {
        let mut c = Counter::default();
        assert_eq!(c.observe(100), 100);
        assert_eq!(c.observe(150), 150);
        // pause/resume resets the raw counter
        assert_eq!(c.observe(10), 160);
        // restart
        let mut c = c.carried_over();
        assert_eq!(c.total(), 160);
        assert_eq!(c.observe(5), 165);
    }

    #[test]
    fn settings_roundtrip_with_defaults() {
        let db = Db::open_in_memory().unwrap();
        let defaults = Settings {
            download_dir: "/dl".into(),
            listen_port: 1234,
            enable_upnp: true,
            down_limit_bps: None,
            up_limit_bps: None,
        };
        assert_eq!(db.load_settings(defaults.clone()).unwrap(), defaults);
        let changed = Settings { up_limit_bps: Some(5000), ..defaults.clone() };
        db.save_settings(&changed).unwrap();
        assert_eq!(db.load_settings(defaults).unwrap(), changed);
    }

    #[test]
    fn picking_roundtrips_and_default_is_not_stored() {
        let db = Db::open_in_memory().unwrap();
        let p = Picking { sequential: true, file_priorities: vec![FilePriority::High, FilePriority::Low] };
        db.save_picking("aa", &p).unwrap();
        db.save_picking("bb", &Picking { sequential: true, ..Default::default() }).unwrap();
        db.save_picking("bb", &Picking::default()).unwrap();
        let loaded = db.load_picking().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["aa"], p);
        db.delete_torrent("aa").unwrap();
        assert!(db.load_picking().unwrap().is_empty());
    }

    #[test]
    fn torrents_and_samples() {
        let db = Db::open_in_memory().unwrap();
        let rec = TorrentRecord {
            added_at: 1,
            completed_at: Some(2),
            uploaded: Counter { base: 3, last: 4 },
            downloaded: Counter { base: 5, last: 6 },
        };
        let hash = "abc".to_string();
        db.save_torrents([(&hash, &rec)]).unwrap();
        let loaded = db.load_torrents().unwrap();
        assert_eq!(loaded["abc"].uploaded, rec.uploaded);
        assert_eq!(loaded["abc"].completed_at, Some(2));

        for ts in 1..=5 {
            db.insert_sample(StatsSample { ts, down_bps: ts as u64, up_bps: 0 }).unwrap();
        }
        db.prune_samples_before(3).unwrap();
        let s = db.samples_since(3).unwrap();
        assert_eq!(s.iter().map(|s| s.ts).collect::<Vec<_>>(), vec![4, 5]);
    }
}
