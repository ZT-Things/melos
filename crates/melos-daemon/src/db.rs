//! SQLite index over the downloaded files. Files are the source of truth.

use std::path::Path;

use anyhow::{Context, Result};
use melos_proto::{DayStat, LyricsView, NameStat, PlaylistRow, Stats, TrackRow, TrackStat};
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
              id          INTEGER PRIMARY KEY AUTOINCREMENT,
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
              missing     INTEGER NOT NULL DEFAULT 0,
              position    INTEGER NOT NULL DEFAULT 0
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

            CREATE TABLE IF NOT EXISTS lyrics (
              track_id  INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
              plain     TEXT NOT NULL DEFAULT '',
              synced    TEXT,
              source    TEXT NOT NULL,
              offset_ms INTEGER NOT NULL DEFAULT 0,
              updated_at INTEGER NOT NULL
            );
            "#,
        )?;
        self.ensure_library_position()?;
        self.ensure_autoincrement()?;
        Ok(())
    }

    /// Older libraries sorted by artist/title. Give each track a stable manual
    /// position, seeded from that order, so visual reordering can persist.
    fn ensure_library_position(&self) -> Result<()> {
        let has_position: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = 'position'",
            [],
            |row| row.get(0),
        )?;
        if has_position == 0 {
            self.conn.execute_batch(
                "ALTER TABLE tracks ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
                 WITH ordered AS (
                   SELECT id,
                          ROW_NUMBER() OVER (
                            ORDER BY artist COLLATE NOCASE, title COLLATE NOCASE, id
                          ) - 1 AS pos
                   FROM tracks
                 )
                 UPDATE tracks SET position = (
                   SELECT pos FROM ordered WHERE ordered.id = tracks.id
                 );",
            )?;
        }
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS tracks_position ON tracks(position);")?;
        Ok(())
    }

    /// Track ids are the long-term key for listening history, so they must
    /// never be reused after a delete. Older databases used a plain rowid.
    fn ensure_autoincrement(&self) -> Result<()> {
        let sql: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='tracks'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if sql
            .as_deref()
            .is_none_or(|sql| sql.contains("AUTOINCREMENT"))
        {
            return Ok(());
        }

        self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let result = self.conn.execute_batch(
            "BEGIN;
             CREATE TABLE tracks_new (
               id          INTEGER PRIMARY KEY AUTOINCREMENT,
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
               missing     INTEGER NOT NULL DEFAULT 0,
               position    INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO tracks_new
               (id, path, yt_id, title, artist, album, duration_ms, added_at,
                last_played, play_count, file_mtime, missing, position)
             SELECT id, path, yt_id, title, artist, album, duration_ms, added_at,
                    last_played, play_count, file_mtime, missing, position
             FROM tracks;
             DROP TABLE tracks;
             ALTER TABLE tracks_new RENAME TO tracks;
             CREATE INDEX IF NOT EXISTS tracks_title ON tracks(title);
             CREATE INDEX IF NOT EXISTS tracks_position ON tracks(position);
             COMMIT;",
        );
        self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        if let Err(error) = result {
            let _ = self.conn.execute_batch("ROLLBACK;");
            return Err(error.into());
        }
        Ok(())
    }

    pub fn rows(&self) -> Result<Vec<TrackRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, artist, album, duration_ms, missing
             FROM tracks ORDER BY position, id",
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
                                added_at, file_mtime, missing, position)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0,
                    (SELECT COALESCE(MAX(position) + 1, 0) FROM tracks))
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

    pub fn move_library(&self, start: usize, len: usize, delta: i8) -> Result<bool> {
        let mut ids: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM tracks ORDER BY position, id")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !move_block(&mut ids, start, len, delta) {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        for (position, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE tracks SET position = ?2 WHERE id = ?1",
                params![id, position as i64],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn move_playlist_tracks(
        &self,
        playlist_id: i64,
        start: usize,
        len: usize,
        delta: i8,
    ) -> Result<bool> {
        let mut ids: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT track_id FROM playlist_tracks
                 WHERE playlist_id = ?1 ORDER BY position",
            )?;
            stmt.query_map(params![playlist_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !move_block(&mut ids, start, len, delta) {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1",
            params![playlist_id],
        )?;
        for (position, track_id) in ids.iter().enumerate() {
            tx.execute(
                "INSERT INTO playlist_tracks (playlist_id, track_id, position)
                 VALUES (?1, ?2, ?3)",
                params![playlist_id, track_id, position as i64],
            )?;
        }
        tx.commit()?;
        Ok(true)
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

    pub fn get_lyrics(&self, track_id: i64) -> Result<Option<LyricsView>> {
        self.conn
            .query_row(
                "SELECT track_id, plain, synced, source, offset_ms, updated_at
                 FROM lyrics WHERE track_id = ?1",
                params![track_id],
                |r| {
                    Ok(LyricsView {
                        track_id: r.get(0)?,
                        plain: r.get(1)?,
                        synced: r.get(2)?,
                        source: r.get(3)?,
                        offset_ms: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn save_lyrics(
        &self,
        track_id: i64,
        plain: &str,
        synced: Option<&str>,
        source: &str,
    ) -> Result<LyricsView> {
        self.conn.execute(
            "INSERT INTO lyrics (track_id, plain, synced, source, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(track_id) DO UPDATE SET
               plain = excluded.plain,
               synced = excluded.synced,
               source = excluded.source,
               updated_at = excluded.updated_at",
            params![track_id, plain, synced, source, now_unix()],
        )?;
        self.get_lyrics(track_id)?
            .context("lyrics row disappeared after save")
    }

    pub fn set_lyrics_offset(&self, track_id: i64, offset_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE lyrics SET offset_ms = ?2 WHERE track_id = ?1",
            params![track_id, offset_ms],
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
            "SELECT listening.artist, SUM(listening.played_ms) AS total
             FROM listening JOIN tracks ON tracks.id = listening.track_id
             GROUP BY listening.artist
             ORDER BY total DESC, listening.artist COLLATE NOCASE LIMIT 5",
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
            "SELECT listening.title, listening.artist, SUM(listening.played_ms) AS total
             FROM listening JOIN tracks ON tracks.id = listening.track_id
             GROUP BY listening.title, listening.artist
             ORDER BY total DESC, listening.title COLLATE NOCASE LIMIT 5",
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

/// Move a contiguous block one slot down (`delta = 1`) or up (`delta = -1`).
/// Returns false when the move is invalid or would leave the list.
fn move_block<T: Copy>(items: &mut [T], start: usize, len: usize, delta: i8) -> bool {
    if len == 0 || start + len > items.len() || !matches!(delta, -1 | 1) {
        return false;
    }
    if delta == 1 {
        if start + len >= items.len() {
            return false;
        }
        items[start..=start + len].rotate_left(len);
    } else {
        if start == 0 {
            return false;
        }
        items[start - 1..=start + len - 1].rotate_right(len);
    }
    true
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
               id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE, yt_id TEXT NOT NULL UNIQUE,
               title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT,
               duration_ms INTEGER NOT NULL DEFAULT 0, added_at INTEGER NOT NULL,
               last_played INTEGER, play_count INTEGER NOT NULL DEFAULT 0,
               file_mtime INTEGER NOT NULL DEFAULT 0, missing INTEGER NOT NULL DEFAULT 0,
               position INTEGER NOT NULL DEFAULT 0);
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
               PRIMARY KEY (day, track_id));
             CREATE TABLE lyrics (
               track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
               plain TEXT NOT NULL DEFAULT '', synced TEXT, source TEXT NOT NULL,
               offset_ms INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);",
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
    fn library_order_is_manual_and_new_tracks_append() {
        let db = mem_db();
        db.insert(&track("a")).unwrap();
        db.insert(&track("b")).unwrap();
        db.insert(&track("c")).unwrap();
        assert_eq!(
            db.rows()
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        assert!(db.move_library(0, 1, 1).unwrap());
        assert_eq!(
            db.rows()
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["b", "a", "c"]
        );
        assert!(db.move_library(1, 2, -1).unwrap());
        assert_eq!(
            db.rows()
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        assert!(!db.move_library(0, 1, -1).unwrap());
        assert!(!db.move_library(2, 1, 1).unwrap());

        db.insert(&track("d")).unwrap();
        assert_eq!(
            db.rows()
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "b", "d"]
        );
    }

    #[test]
    fn moving_playlist_tracks_persists_the_new_order() {
        let db = mem_db();
        let a = db.insert(&track("a")).unwrap();
        let b = db.insert(&track("b")).unwrap();
        let c = db.insert(&track("c")).unwrap();
        let playlist = db.create_playlist("Mix").unwrap();
        for id in [a, b, c] {
            db.add_to_playlist(playlist, id).unwrap();
        }

        assert!(db.move_playlist_tracks(playlist, 0, 2, 1).unwrap());
        assert_eq!(
            db.playlist_tracks(playlist)
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["c", "a", "b"]
        );
        assert!(db.move_playlist_tracks(playlist, 2, 1, -1).unwrap());
        assert_eq!(
            db.playlist_tracks(playlist)
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
        assert!(!db.move_playlist_tracks(playlist, 0, 1, -1).unwrap());
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
    fn deleted_tracks_are_hidden_from_top_lists_but_stay_in_totals() {
        let db = mem_db();
        let id = db.insert(&track("a")).unwrap();
        db.add_listening(id, "a", "Artist", 12_000).unwrap();
        db.delete_track(id).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(stats.total_ms, 12_000);
        assert!(stats.top_artists.is_empty());
        assert!(stats.top_tracks.is_empty());
    }

    #[test]
    fn track_ids_are_not_reused_after_deleting_the_newest_one() {
        let db = mem_db();
        let first = db.insert(&track("a")).unwrap();
        db.add_listening(first, "a", "Artist", 3_000).unwrap();
        db.delete_track(first).unwrap();
        let second = db.insert(&track("b")).unwrap();
        assert_ne!(first, second);
        db.add_listening(second, "b", "Artist B", 2_000).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(stats.total_ms, 5_000);
        assert_eq!(stats.top_tracks.len(), 1);
        assert_eq!(stats.top_tracks[0].title, "b");
    }

    #[test]
    fn migration_makes_track_ids_monotonic() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (
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
        conn.execute(
            "INSERT INTO tracks
               (id, path, yt_id, title, artist, added_at)
             VALUES (42, '/tmp/old.opus', 'old', 'old', 'Artist', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks
               (id, path, yt_id, title, artist, added_at)
             VALUES (43, '/tmp/first.opus', 'first', 'first', 'Aardvark', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO playlists (id, name, created_at) VALUES (1, 'Old', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (1, 42, 0)",
            [],
        )
        .unwrap();
        let db = Db { conn };
        db.migrate().unwrap();
        assert!(db.track(42).unwrap().is_some());
        assert_eq!(db.playlist_tracks(1).unwrap()[0].id, 42);
        assert_eq!(
            db.rows()
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["first", "old"]
        );
        assert!(db.move_library(0, 1, 1).unwrap());
        assert_eq!(
            db.rows()
                .unwrap()
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["old", "first"]
        );
        let first = db.insert(&track("a")).unwrap();
        db.delete_track(first).unwrap();
        let second = db.insert(&track("b")).unwrap();
        assert_ne!(first, second);
        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='tracks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("AUTOINCREMENT"));
    }

    #[test]
    fn lyrics_are_cached_offline_and_deleted_with_the_track() {
        let db = mem_db();
        let id = db.insert(&track("a")).unwrap();
        assert!(db.get_lyrics(id).unwrap().is_none());
        let saved = db
            .save_lyrics(
                id,
                "Hello\nWorld",
                Some("[00:01.00]Hello\n[00:02.00]World"),
                "manual",
            )
            .unwrap();
        assert_eq!(saved.source, "manual");
        assert_eq!(saved.plain, "Hello\nWorld");
        assert!(saved.synced.is_some());
        let loaded = db.get_lyrics(id).unwrap().unwrap();
        assert_eq!(loaded, saved);
        db.set_lyrics_offset(id, -500).unwrap();
        assert_eq!(db.get_lyrics(id).unwrap().unwrap().offset_ms, -500);
        db.delete_track(id).unwrap();
        assert!(db.get_lyrics(id).unwrap().is_none());
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
