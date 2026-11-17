//! SQLite index over the downloaded files. Files are the source of truth.

use std::path::Path;

use anyhow::{Context, Result};
use melos_proto::{DayStat, NameStat, PlaylistRow, Stats, TrackRow, TrackStat};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct NewTrack {
    pub path: String,
    pub yt_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub mtime: i64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tracks (
              id          INTEGER PRIMARY KEY,
              path        TEXT NOT NULL UNIQUE,
              yt_id       TEXT NOT NULL UNIQUE,
              title       TEXT NOT NULL,
              artist      TEXT NOT NULL,
              album       TEXT,
              duration_ms INTEGER NOT NULL DEFAULT 0,
              added_at    INTEGER NOT NULL,
              last_played INTEGER,
              play_count  INTEGER NOT NULL DEFAULT 0,
              file_mtime  INTEGER NOT NULL DEFAULT 0,
              missing     INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS tracks_title ON tracks(title);

            CREATE TABLE IF NOT EXISTS playlists (
              id         INTEGER PRIMARY KEY,
              name       TEXT NOT NULL UNIQUE,
              created_at INTEGER NOT NULL,
              sort_order INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS playlist_tracks (
              playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
              track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
              position    INTEGER NOT NULL,
              PRIMARY KEY (playlist_id, position),
              UNIQUE (playlist_id, track_id)
            );

            CREATE TABLE IF NOT EXISTS kv (
              key   TEXT PRIMARY KEY,
              value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS listening (
              day       TEXT NOT NULL,
              track_id  INTEGER NOT NULL,
              title     TEXT NOT NULL,
              artist    TEXT NOT NULL,
              played_ms INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (day, track_id)
            );
            CREATE INDEX IF NOT EXISTS listening_day ON listening(day);
            "#,
        )?;
        Ok(())
    }

    pub fn rows(&self) -> Result<Vec<TrackRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, artist, album, duration_ms, missing
             FROM tracks ORDER BY artist COLLATE NOCASE, title COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], row_to_track)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn track(&self, id: i64) -> Result<Option<(TrackRow, String)>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, title, artist, album, duration_ms, missing, path
                 FROM tracks WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        TrackRow {
                            id: r.get(0)?,
                            title: r.get(1)?,
                            artist: r.get(2)?,
                            album: r.get(3)?,
                            duration_ms: r.get::<_, i64>(4)?.max(0) as u64,
                            missing: r.get::<_, i64>(5)? != 0,
                        },
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn tracks_by_ids(&self, ids: &[i64]) -> Result<Vec<TrackRow>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some((row, _)) = self.track(*id)? {
                out.push(row);
            }
        }
        Ok(out)
    }

    pub fn insert(&self, t: &NewTrack) -> Result<i64> {
        let now = now_unix();
        let id = self.conn.query_row(
            r#"
            INSERT INTO tracks (path, yt_id, title, artist, album, duration_ms,
                                added_at, file_mtime, missing)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
            ON CONFLICT(yt_id) DO UPDATE SET
                path = excluded.path,
                title = excluded.title,
                artist = excluded.artist,
                album = excluded.album,
                duration_ms = excluded.duration_ms,
                file_mtime = excluded.file_mtime,
                missing = 0
            RETURNING id
            "#,
            params![
                t.path,
                t.yt_id,
                t.title,
                t.artist,
                t.album,
                t.duration_ms as i64,
                now,
                t.mtime
            ],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn bump_play_count(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET play_count = play_count + 1, last_played = ?2 WHERE id = ?1",
            params![id, now_unix()],
        )?;
        Ok(())
    }

    pub fn set_missing(&self, id: i64, missing: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET missing = ?2 WHERE id = ?1",
            params![id, missing as i64],
        )?;
        Ok(())
    }

    pub fn playlists(&self) -> Result<Vec<PlaylistRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name,
                    (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id)
             FROM playlists p ORDER BY p.sort_order, p.name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PlaylistRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    count: r.get::<_, i64>(2)?.max(0) as usize,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        let next: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sort_order) + 1, 0) FROM playlists",
            [],
            |r| r.get(0),
        )?;
        let id = self.conn.query_row(
            "INSERT INTO playlists (name, created_at, sort_order) VALUES (?1, ?2, ?3)
             RETURNING id",
            params![name, now_unix(), next],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn playlist_tracks(&self, id: i64) -> Result<Vec<TrackRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, t.artist, t.album, t.duration_ms, t.missing
             FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id
             WHERE pt.playlist_id = ?1 ORDER BY pt.position",
        )?;
        let rows = stmt
            .query_map(params![id], row_to_track)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn add_to_playlist(&self, playlist_id: i64, track_id: i64) -> Result<()> {
        let next: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?1",
            params![playlist_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO playlist_tracks (playlist_id, track_id, position)
             VALUES (?1, ?2, ?3)",
            params![playlist_id, track_id, next],
        )?;
        Ok(())
    }

    pub fn remove_from_playlist(&self, playlist_id: i64, track_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
            params![playlist_id, track_id],
        )?;
        self.conn.execute(
            "WITH ordered AS (
                SELECT track_id, ROW_NUMBER() OVER (ORDER BY position) - 1 AS new_pos
                FROM playlist_tracks WHERE playlist_id = ?1
             )
             UPDATE playlist_tracks SET position = (
                SELECT new_pos FROM ordered WHERE ordered.track_id = playlist_tracks.track_id
             ) WHERE playlist_id = ?1",
            params![playlist_id],
        )?;
        Ok(())
    }

    pub fn delete_track(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn rename_track(&self, id: i64, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        self.conn.execute(
            "UPDATE listening SET title = ?2 WHERE track_id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE playlists SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Add listening time to the current local day.
    pub fn add_listening(&self, track_id: i64, title: &str, artist: &str, ms: u64) -> Result<()> {
        if ms == 0 {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO listening (day, track_id, title, artist, played_ms)
             VALUES (date('now','localtime'), ?1, ?2, ?3, ?4)
             ON CONFLICT(day, track_id) DO UPDATE SET
               played_ms = listening.played_ms + excluded.played_ms,
               title = excluded.title,
               artist = excluded.artist",
            params![track_id, title, artist, ms as i64],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        let total_ms = sum_ms(
            &self.conn,
            "SELECT COALESCE(SUM(played_ms), 0) FROM listening",
        )?;
        let today_ms = sum_ms(
            &self.conn,
            "SELECT COALESCE(SUM(played_ms), 0) FROM listening
             WHERE day = date('now','localtime')",
        )?;
        let week_ms = sum_ms(
            &self.conn,
            "SELECT COALESCE(SUM(played_ms), 0) FROM listening
             WHERE day >= date('now','localtime','-6 days')",
        )?;

        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE days(day) AS (
               SELECT date('now','localtime','-6 days')
               UNION ALL
               SELECT date(day, '+1 day') FROM days
               WHERE day < date('now','localtime')
             )
             SELECT days.day, COALESCE(SUM(listening.played_ms), 0)
             FROM days LEFT JOIN listening ON listening.day = days.day
             GROUP BY days.day ORDER BY days.day",
        )?;
        let days = stmt
            .query_map([], |r| {
                Ok(DayStat {
                    day: r.get(0)?,
                    ms: r.get::<_, i64>(1)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT artist, SUM(played_ms) AS total FROM listening
             GROUP BY artist ORDER BY total DESC, artist COLLATE NOCASE LIMIT 5",
        )?;
        let top_artists = stmt
            .query_map([], |r| {
                Ok(NameStat {
                    name: r.get(0)?,
                    ms: r.get::<_, i64>(1)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT title, artist, SUM(played_ms) AS total FROM listening
             GROUP BY title, artist ORDER BY total DESC, title COLLATE NOCASE LIMIT 5",
        )?;
        let top_tracks = stmt
            .query_map([], |r| {
                Ok(TrackStat {
                    title: r.get(0)?,
                    artist: r.get(1)?,
                    ms: r.get::<_, i64>(2)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Stats {
            total_ms,
            today_ms,
            week_ms,
            days,
            top_artists,
            top_tracks,
        })
    }
}

fn sum_ms(conn: &Connection, sql: &str) -> Result<u64> {
    let value: i64 = conn.query_row(sql, [], |r| r.get(0))?;
    Ok(value.max(0) as u64)
}

fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrackRow> {
    Ok(TrackRow {
        id: r.get(0)?,
        title: r.get(1)?,
        artist: r.get(2)?,
        album: r.get(3)?,
        duration_ms: r.get::<_, i64>(4)?.max(0) as u64,
        missing: r.get::<_, i64>(5)? != 0,
    })
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE tracks (
               id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, yt_id TEXT NOT NULL UNIQUE,
               title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT,
               duration_ms INTEGER NOT NULL DEFAULT 0, added_at INTEGER NOT NULL,
               last_played INTEGER, play_count INTEGER NOT NULL DEFAULT 0,
               file_mtime INTEGER NOT NULL DEFAULT 0, missing INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE playlists (
               id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
               created_at INTEGER NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE playlist_tracks (
               playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
               track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
               position INTEGER NOT NULL, PRIMARY KEY (playlist_id, position),
               UNIQUE (playlist_id, track_id));
             CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE listening (
               day TEXT NOT NULL, track_id INTEGER NOT NULL,
               title TEXT NOT NULL, artist TEXT NOT NULL,
               played_ms INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (day, track_id));",
        )
        .unwrap();
        Db { conn }
    }

    fn track(n: &str) -> NewTrack {
        NewTrack {
            path: format!("/tmp/{n}.opus"),
            yt_id: n.into(),
            title: n.into(),
            artist: "Artist".into(),
            album: None,
            duration_ms: 1000,
            mtime: 1,
        }
    }

    #[test]
    fn upsert_and_rows() {
        let db = mem_db();
        let id = db.insert(&track("a")).unwrap();
        let again = db.insert(&track("a")).unwrap();
        assert_eq!(id, again);
        assert_eq!(db.rows().unwrap().len(), 1);
    }

    #[test]
    fn playlist_flow() {
        let db = mem_db();
        let t1 = db.insert(&track("a")).unwrap();
        let t2 = db.insert(&track("b")).unwrap();
        let pl = db.create_playlist("Favorites").unwrap();
        db.add_to_playlist(pl, t1).unwrap();
        db.add_to_playlist(pl, t2).unwrap();
        db.add_to_playlist(pl, t1).unwrap(); // ignored duplicate
        let rows = db.playlist_tracks(pl).unwrap();
        assert_eq!(rows.len(), 2);
        db.remove_from_playlist(pl, t1).unwrap();
        let rows = db.playlist_tracks(pl).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, t2);
        assert_eq!(db.playlists().unwrap()[0].count, 1);
        db.delete_playlist(pl).unwrap();
        assert!(db.playlists().unwrap().is_empty());
    }

    #[test]
    fn delete_track_removes_membership_but_keeps_listening_history() {
        let db = mem_db();
        let id = db.insert(&track("a")).unwrap();
        let pl = db.create_playlist("Favorites").unwrap();
        db.add_to_playlist(pl, id).unwrap();
        db.add_listening(id, "a", "Artist", 12_000).unwrap();
        db.delete_track(id).unwrap();
        assert!(db.track(id).unwrap().is_none());
        assert!(db.playlist_tracks(pl).unwrap().is_empty());
        assert_eq!(db.stats().unwrap().total_ms, 12_000);
    }

    #[test]
    fn rename_track_updates_library_and_history() {
        let db = mem_db();
        let id = db.insert(&track("a")).unwrap();
        db.add_listening(id, "a", "Artist", 5_000).unwrap();
        db.rename_track(id, "Renamed").unwrap();
        assert_eq!(db.track(id).unwrap().unwrap().0.title, "Renamed");
        assert_eq!(db.stats().unwrap().top_tracks[0].title, "Renamed");
    }

    #[test]
    fn rename_playlist_changes_the_visible_name() {
        let db = mem_db();
        let pl = db.create_playlist("Old").unwrap();
        db.rename_playlist(pl, "New").unwrap();
        assert_eq!(db.playlists().unwrap()[0].name, "New");
    }

    #[test]
    fn listening_accumulates_per_day_and_builds_stats() {
        let db = mem_db();
        let id = db.insert(&track("a")).unwrap();
        db.add_listening(id, "a", "Artist", 30_000).unwrap();
        db.add_listening(id, "a", "Artist", 15_000).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(stats.total_ms, 45_000);
        assert_eq!(stats.today_ms, 45_000);
        assert_eq!(stats.week_ms, 45_000);
        assert_eq!(stats.days.len(), 7);
        assert_eq!(stats.days.last().unwrap().ms, 45_000);
        assert_eq!(stats.top_artists[0].name, "Artist");
        assert_eq!(stats.top_artists[0].ms, 45_000);
        assert_eq!(stats.top_tracks[0].title, "a");
    }
}
