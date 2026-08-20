use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::organizer::OrganizableTrack;

const INTERNAL_DIR: &str = ".ncm-tui";
const DATABASE_NAME: &str = "library.sqlite3";
const SCHEMA_VERSION: i64 = 5;

const TRACK_COLUMNS: &str = "id, ncm_id, source_kind, title, artists, album, file_path,
        format, file_size, downloaded_at, duration_ms, favorite, play_count,
        last_played_at, cover_url";
const TRACK_COLUMNS_T: &str = "t.id, t.ncm_id, t.source_kind, t.title, t.artists, t.album,
        t.file_path, t.format, t.file_size, t.downloaded_at, t.duration_ms, t.favorite,
        t.play_count, t.last_played_at, t.cover_url";

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("database I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("database mutex is poisoned")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, DatabaseError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayOrigin {
    Local,
    Stream,
}

impl PlayOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Stream => "stream",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Inserted,
    Moved,
    AlreadyNext,
    Missing,
}

#[derive(Clone, Debug)]
pub struct CatalogTrack {
    pub ncm_id: u64,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u64,
    pub cover_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadedTrack {
    pub ncm_id: u64,
    pub title: String,
    pub artists: String,
    pub album_id: Option<u64>,
    pub album: String,
    pub duration_ms: u64,
    pub album_artist: String,
    pub year: Option<i32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub total_tracks: Option<u32>,
    pub cover_url: String,
    pub file_path: PathBuf,
    pub file_size: u64,
    pub format: String,
    pub bitrate: u64,
    pub audio_md5: String,
    pub collection_id: Option<u64>,
    pub collection_kind: String,
    pub collection_name: String,
    pub collection_position: u32,
}

#[derive(Clone, Debug)]
pub struct LibraryTrack {
    pub id: u64,
    pub ncm_id: Option<u64>,
    pub source_kind: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub file_path: Option<PathBuf>,
    pub format: String,
    pub file_size: u64,
    pub downloaded_at: i64,
    pub duration_ms: u64,
    pub favorite: bool,
    pub play_count: u64,
    pub last_played_at: Option<i64>,
    pub cover_url: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackDetail {
    pub album_artist: String,
    pub release_year: Option<i32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub bitrate: u64,
    pub audio_md5: String,
    pub cover_url: String,
}

#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub file_size: u64,
    pub modified_ns: i64,
    pub format: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u64,
    pub album_artist: String,
    pub release_year: Option<i32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub bitrate: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanChanges {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub missing: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DatabaseStats {
    pub tracks: u64,
    pub missing: u64,
    pub albums: u64,
    pub duration_ms: u64,
    pub favorites: u64,
    pub plays: u64,
}

#[derive(Clone)]
pub struct LibraryDb {
    connection: Arc<Mutex<Connection>>,
}

impl LibraryDb {
    /// The database location is derived from the download root and is never a user setting.
    pub fn open(download_root: impl AsRef<Path>) -> Result<Self> {
        let state_dir = download_root.as_ref().join(INTERNAL_DIR);
        fs::create_dir_all(&state_dir)?;
        let path = state_dir.join(DATABASE_NAME);
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-32768;",
        )?;

        let database = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        database.migrate()?;
        database.reconcile_known_files()?;
        Ok(database)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| DatabaseError::Poisoned)
    }

    fn migrate(&self) -> Result<()> {
        let mut connection = self.lock()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (version INTEGER NOT NULL);
             INSERT INTO schema_meta(version)
             SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM schema_meta);",
        )?;
        let has_tracks = table_exists(&connection, "tracks")?;
        let has_surrogate = has_tracks && table_has_column(&connection, "tracks", "id")?;
        if !has_tracks {
            let transaction = connection.transaction()?;
            create_v5_schema(&transaction)?;
            transaction.execute("UPDATE schema_meta SET version=?1", [SCHEMA_VERSION])?;
            transaction.commit()?;
            return Ok(());
        }
        if !has_surrogate {
            connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let transaction = connection.transaction()?;
            ensure_legacy_columns(&transaction)?;
            migrate_legacy_to_v5(&transaction)?;
            transaction.execute("UPDATE schema_meta SET version=?1", [SCHEMA_VERSION])?;
            transaction.commit()?;
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
            return Ok(());
        }
        let version: i64 =
            connection.query_row("SELECT MAX(version) FROM schema_meta", [], |row| row.get(0))?;
        if version < SCHEMA_VERSION {
            connection.execute("UPDATE schema_meta SET version=?1", [SCHEMA_VERSION])?;
        }
        Ok(())
    }

    pub fn record_download(&self, track: &DownloadedTrack) -> Result<()> {
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO tracks (
                 ncm_id, source_kind, source_id, title, artists, album_id, album,
                 duration_ms, album_artist, release_year, track_number, disc_number,
                 total_tracks, cover_url, file_path, file_size, format, bitrate,
                 audio_md5, status, downloaded_at, created_at, updated_at
             ) VALUES (
                 ?1, 'ncm', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18, 'available', ?19, ?19, ?19
             )
             ON CONFLICT(ncm_id) DO UPDATE SET
                 title=excluded.title,
                 artists=excluded.artists,
                 album_id=excluded.album_id,
                 album=excluded.album,
                 duration_ms=excluded.duration_ms,
                 album_artist=excluded.album_artist,
                 release_year=excluded.release_year,
                 track_number=excluded.track_number,
                 disc_number=excluded.disc_number,
                 total_tracks=excluded.total_tracks,
                 cover_url=excluded.cover_url,
                 file_path=excluded.file_path,
                 file_size=excluded.file_size,
                 format=excluded.format,
                 bitrate=excluded.bitrate,
                 audio_md5=excluded.audio_md5,
                 status='available',
                 downloaded_at=COALESCE(tracks.downloaded_at, excluded.downloaded_at),
                 updated_at=excluded.updated_at",
            params![
                track.ncm_id as i64,
                track.ncm_id.to_string(),
                track.title,
                track.artists,
                track.album_id.map(|id| id as i64),
                track.album,
                track.duration_ms as i64,
                track.album_artist,
                track.year,
                track.track_number,
                track.disc_number,
                track.total_tracks,
                track.cover_url,
                track.file_path.to_string_lossy(),
                track.file_size as i64,
                track.format,
                track.bitrate as i64,
                track.audio_md5,
                now,
            ],
        )?;
        let track_id: i64 = transaction.query_row(
            "SELECT id FROM tracks WHERE ncm_id=?1",
            [track.ncm_id as i64],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO media_locations(
                 track_id, filepath, format, file_size, modified_ns,
                 is_present, first_seen_at, last_seen_at
             ) VALUES (?1, ?2, ?3, ?4, 0, 1, ?5, ?5)
             ON CONFLICT(filepath) DO UPDATE SET
                 track_id=excluded.track_id, format=excluded.format,
                 file_size=excluded.file_size, is_present=1,
                 last_seen_at=excluded.last_seen_at",
            params![
                track_id,
                track.file_path.to_string_lossy(),
                track.format,
                track.file_size as i64,
                now
            ],
        )?;

        if let Some(collection_id) = track.collection_id {
            transaction.execute(
                "INSERT INTO collections(kind, ncm_id, name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(kind, ncm_id) DO UPDATE SET
                     name=excluded.name, updated_at=excluded.updated_at",
                params![
                    track.collection_kind,
                    collection_id as i64,
                    track.collection_name,
                    now
                ],
            )?;
            transaction.execute(
                "INSERT INTO collection_tracks(collection_kind, collection_id, track_id, position)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO UPDATE SET position=excluded.position",
                params![
                    track.collection_kind,
                    collection_id as i64,
                    track_id,
                    track.collection_position as i64
                ],
            )?;
        }
        transaction.execute(
            "UPDATE maintenance_state SET downloads_since=downloads_since+1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        drop(connection);
        self.run_incremental_maintenance()?;
        Ok(())
    }

    pub fn ensure_catalog(&self, track: &CatalogTrack) -> Result<u64> {
        Ok(*self
            .ensure_catalog_many(std::slice::from_ref(track))?
            .first()
            .unwrap_or(&0))
    }

    pub fn ensure_catalog_many(&self, tracks: &[CatalogTrack]) -> Result<Vec<u64>> {
        if tracks.is_empty() {
            return Ok(Vec::new());
        }
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let mut ids = Vec::with_capacity(tracks.len());
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO tracks (
                     ncm_id, source_kind, source_id, title, artists, album,
                     duration_ms, cover_url, status, created_at, updated_at
                 ) VALUES (?1, 'ncm', ?2, ?3, ?4, ?5, ?6, ?7, 'remote', ?8, ?8)
                 ON CONFLICT(ncm_id) DO UPDATE SET
                     title=excluded.title,
                     artists=excluded.artists,
                     album=CASE WHEN excluded.album<>'' THEN excluded.album ELSE tracks.album END,
                     duration_ms=CASE WHEN excluded.duration_ms>0 THEN excluded.duration_ms ELSE tracks.duration_ms END,
                     cover_url=CASE WHEN excluded.cover_url<>'' THEN excluded.cover_url ELSE tracks.cover_url END,
                     updated_at=excluded.updated_at
                 RETURNING id",
            )?;
            for track in tracks {
                if track.ncm_id == 0 {
                    ids.push(0);
                    continue;
                }
                let id: i64 = statement.query_row(
                    params![
                        track.ncm_id as i64,
                        track.ncm_id.to_string(),
                        track.title,
                        track.artists,
                        track.album,
                        track.duration_ms as i64,
                        track.cover_url,
                        now,
                    ],
                    |row| row.get(0),
                )?;
                ids.push(id as u64);
            }
        }
        transaction.commit()?;
        Ok(ids)
    }

    #[cfg(test)]
    pub fn id_by_ncm(&self, ncm_id: u64) -> Result<Option<u64>> {
        let connection = self.lock()?;
        let id: Option<i64> = connection
            .query_row(
                "SELECT id FROM tracks WHERE ncm_id=?1",
                [ncm_id as i64],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.map(|id| id as u64))
    }

    pub fn record_collection_membership(
        &self,
        kind: &str,
        collection_id: u64,
        name: &str,
        tracks: &[(u64, u32)],
    ) -> Result<()> {
        if tracks.is_empty() {
            return Ok(());
        }
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO collections(kind, ncm_id, name, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(kind, ncm_id) DO UPDATE SET
                 name=excluded.name, updated_at=excluded.updated_at",
            params![kind, collection_id as i64, name, now],
        )?;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO collection_tracks(
                     collection_kind, collection_id, track_id, position
                 ) SELECT ?1, ?2, id, ?4
                   FROM tracks WHERE ncm_id=?3
                 ON CONFLICT DO UPDATE SET position=excluded.position",
            )?;
            for (ncm_id, position) in tracks {
                statement.execute(params![
                    kind,
                    collection_id as i64,
                    *ncm_id as i64,
                    *position as i64
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn available_path(&self, track_id: u64) -> Result<Option<PathBuf>> {
        let connection = self.lock()?;
        let path: Option<String> = connection
            .prepare_cached(
                "SELECT file_path FROM tracks
                 WHERE id=?1 AND status='available'",
            )?
            .query_row([track_id as i64], |row| row.get(0))
            .optional()?
            .flatten();
        drop(connection);
        match path.map(PathBuf::from) {
            Some(path) if path.is_file() => Ok(Some(path)),
            Some(_) => {
                let connection = self.lock()?;
                connection.execute(
                    "UPDATE tracks SET status='missing', updated_at=?1 WHERE id=?2",
                    params![unix_timestamp(), track_id as i64],
                )?;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Resolves a whole plan in one indexed query instead of one lookup per track.
    pub fn available_ids(&self, ncm_ids: &[u64]) -> Result<HashSet<u64>> {
        use rusqlite::params_from_iter;
        let mut found = HashSet::new();
        let mut connection = self.lock()?;
        let mut missing = Vec::new();
        for chunk in ncm_ids.chunks(500) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT ncm_id, file_path FROM tracks
                 WHERE status='available' AND ncm_id IN ({placeholders})"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(
                params_from_iter(chunk.iter().map(|id| *id as i64)),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )?;
            for row in rows {
                let (id, path) = row?;
                if path
                    .as_deref()
                    .is_some_and(|path| Path::new(path).is_file())
                {
                    found.insert(id);
                } else {
                    missing.push(id);
                }
            }
        }
        if !missing.is_empty() {
            let transaction = connection.transaction()?;
            {
                let mut statement = transaction.prepare_cached(
                    "UPDATE tracks SET status='missing', updated_at=?1 WHERE ncm_id=?2",
                )?;
                let now = unix_timestamp();
                for id in missing {
                    statement.execute(params![now, id as i64])?;
                }
            }
            transaction.commit()?;
        }
        Ok(found)
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<LibraryTrack>> {
        let connection = self.lock()?;
        let sql = format!(
            "SELECT {TRACK_COLUMNS}
             FROM tracks
             WHERE status='available'
             ORDER BY COALESCE(downloaded_at, created_at) DESC, id DESC
             LIMIT ?1"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([limit as i64], map_library_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<LibraryTrack>> {
        if query.trim().is_empty() {
            return self.recent(limit);
        }
        let fts_query = query
            .split_whitespace()
            .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        let connection = self.lock()?;
        let sql = format!(
            "SELECT {TRACK_COLUMNS_T}
             FROM tracks_fts f
             JOIN tracks t ON t.id=f.rowid
             WHERE tracks_fts MATCH ?1 AND t.status='available'
             ORDER BY bm25(tracks_fts, 5.0, 3.0, 1.0), t.downloaded_at DESC
             LIMIT ?2"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params![fts_query, limit as i64], map_library_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn list(&self, favorites_only: bool, limit: usize) -> Result<Vec<LibraryTrack>> {
        let connection = self.lock()?;
        let sql = format!(
            "SELECT {TRACK_COLUMNS}
             FROM tracks
             WHERE status='available' AND (?1=0 OR favorite=1)
             ORDER BY title COLLATE NOCASE, artists COLLATE NOCASE, id
             LIMIT ?2"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params![favorites_only, limit as i64], map_library_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn stats(&self) -> Result<DatabaseStats> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT
                     SUM(status='available'),
                     SUM(status='missing'),
                     COUNT(DISTINCT CASE WHEN status='available' THEN NULLIF(album, '') END),
                     COALESCE(SUM(CASE WHEN status='available' THEN duration_ms ELSE 0 END), 0),
                     SUM(status='available' AND favorite=1),
                     COALESCE(SUM(play_count), 0)
                 FROM tracks",
                [],
                |row| {
                    Ok(DatabaseStats {
                        tracks: row.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64,
                        missing: row.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                        albums: row.get::<_, i64>(2)? as u64,
                        duration_ms: row.get::<_, i64>(3)? as u64,
                        favorites: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                        plays: row.get::<_, i64>(5)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn toggle_favorite(&self, track_id: u64) -> Result<Option<bool>> {
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE tracks SET favorite=NOT favorite, updated_at=?1 WHERE id=?2",
            params![now, track_id as i64],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let favorite: bool = transaction.query_row(
            "SELECT favorite FROM tracks WHERE id=?1",
            [track_id as i64],
            |row| row.get(0),
        )?;
        record_event(
            &transaction,
            "track",
            track_id,
            "favorite",
            if favorite { "on" } else { "off" },
            now,
        )?;
        transaction.commit()?;
        Ok(Some(favorite))
    }

    pub fn replace_queue(&self, track_ids: &[u64], current_index: Option<usize>) -> Result<()> {
        let now = unix_timestamp();
        let index = current_index
            .filter(|index| *index < track_ids.len())
            .map(|index| index as i64);
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM playback_queue", [])?;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO playback_queue(position, track_id, enqueued_at)
                 SELECT ?1, id, ?3 FROM tracks WHERE id=?2",
            )?;
            for (position, track_id) in track_ids.iter().enumerate() {
                statement.execute(params![position as i64, *track_id as i64, now])?;
            }
        }
        transaction.execute(
            "UPDATE playback_state SET current_index=?1, updated_at=?2 WHERE singleton=1",
            params![index, now],
        )?;
        record_event(
            &transaction,
            "queue",
            0,
            "replace",
            &track_ids.len().to_string(),
            now,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn enqueue_next(&self, track_id: u64) -> Result<EnqueueOutcome> {
        if !self.track_exists(track_id)? {
            return Ok(EnqueueOutcome::Missing);
        }
        let mut ids = self.queue_ids()?;
        let mut current = self.queue_index()?;
        if let Some(position) = ids.iter().position(|id| *id == track_id) {
            match current {
                Some(index) if index == position || position == index + 1 => {
                    return Ok(EnqueueOutcome::AlreadyNext);
                }
                None => return Ok(EnqueueOutcome::AlreadyNext),
                Some(_) => {}
            }
            ids.remove(position);
            if let Some(index) = current.as_mut()
                && position < *index
            {
                *index -= 1;
            }
            let insert_at = current.map(|index| index + 1).unwrap_or(ids.len());
            ids.insert(insert_at.min(ids.len()), track_id);
            self.replace_queue(&ids, current)?;
            return Ok(EnqueueOutcome::Moved);
        }
        let insert_at = current.map(|index| index + 1).unwrap_or(ids.len());
        ids.insert(insert_at.min(ids.len()), track_id);
        self.replace_queue(&ids, current)?;
        Ok(EnqueueOutcome::Inserted)
    }

    pub fn dequeue(&self, track_id: u64) -> Result<bool> {
        let mut ids = self.queue_ids()?;
        let Some(position) = ids.iter().position(|id| *id == track_id) else {
            return Ok(false);
        };
        let mut current = self.queue_index()?;
        ids.remove(position);
        if let Some(index) = current.as_mut() {
            if position < *index {
                *index -= 1;
            } else if position == *index {
                if ids.is_empty() {
                    current = None;
                } else if *index >= ids.len() {
                    current = Some(ids.len() - 1);
                }
            }
        }
        self.replace_queue(&ids, current)?;
        Ok(true)
    }

    pub fn clear_queue(&self) -> Result<usize> {
        let count = self.queue_ids()?.len();
        self.replace_queue(&[], None)?;
        Ok(count)
    }

    pub fn queue_index(&self) -> Result<Option<usize>> {
        let connection = self.lock()?;
        let index: Option<i64> = connection.query_row(
            "SELECT current_index FROM playback_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        Ok(index
            .filter(|index| *index >= 0)
            .map(|index| index as usize))
    }

    pub fn set_queue_index(&self, index: Option<usize>) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE playback_state SET current_index=?1, updated_at=?2 WHERE singleton=1",
            params![index.map(|index| index as i64), unix_timestamp()],
        )?;
        Ok(())
    }

    fn queue_ids(&self) -> Result<Vec<u64>> {
        let connection = self.lock()?;
        let mut statement =
            connection.prepare_cached("SELECT track_id FROM playback_queue ORDER BY position")?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        Ok(rows
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|id| id as u64)
            .collect())
    }

    fn track_exists(&self, track_id: u64) -> Result<bool> {
        let connection = self.lock()?;
        let found: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM tracks WHERE id=?1",
                [track_id as i64],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn albums(&self) -> Result<Vec<(String, String, u64, u64)>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT album, MIN(artists), COUNT(*), COALESCE(SUM(duration_ms), 0)
             FROM tracks
             WHERE status='available' AND TRIM(album)<>''
             GROUP BY album COLLATE NOCASE
             ORDER BY album COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn artists(&self) -> Result<Vec<(String, u64, u64)>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT artists, COUNT(*), COALESCE(SUM(duration_ms), 0)
             FROM tracks
             WHERE status='available' AND TRIM(artists)<>''
             GROUP BY artists COLLATE NOCASE
             ORDER BY artists COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn list_by_album(&self, album: &str, limit: usize) -> Result<Vec<LibraryTrack>> {
        self.query_tracks(
            &format!(
                "SELECT {TRACK_COLUMNS}
                 FROM tracks
                 WHERE status='available' AND album=?1 COLLATE NOCASE
                 ORDER BY disc_number, track_number, title COLLATE NOCASE, id
                 LIMIT ?2"
            ),
            params![album, limit as i64],
        )
    }

    pub fn list_by_artist(&self, artists: &str, limit: usize) -> Result<Vec<LibraryTrack>> {
        self.query_tracks(
            &format!(
                "SELECT {TRACK_COLUMNS}
                 FROM tracks
                 WHERE status='available' AND artists=?1 COLLATE NOCASE
                 ORDER BY album COLLATE NOCASE, disc_number, track_number, id
                 LIMIT ?2"
            ),
            params![artists, limit as i64],
        )
    }

    pub fn list_view(
        &self,
        view: crate::library::TrackView,
        sort: crate::library::TrackSort,
        limit: usize,
    ) -> Result<Vec<LibraryTrack>> {
        let filter = match view {
            crate::library::TrackView::All => "status='available'",
            crate::library::TrackView::Favorites => "status='available' AND favorite=1",
            crate::library::TrackView::Unplayed => "status='available' AND play_count=0",
            crate::library::TrackView::Frequent => "status='available' AND play_count>=3",
            crate::library::TrackView::RecentAdded => "status='available'",
            crate::library::TrackView::Incomplete => {
                "status='available' AND (TRIM(album)='' OR TRIM(artists)='')"
            }
            crate::library::TrackView::Missing => "status='missing'",
            crate::library::TrackView::Large => "status='available' AND file_size>=31457280",
        };
        let order = match (view, sort) {
            (crate::library::TrackView::RecentAdded, _) => {
                "COALESCE(downloaded_at, created_at) DESC, id DESC"
            }
            (_, crate::library::TrackSort::Title) => {
                "title COLLATE NOCASE, artists COLLATE NOCASE, id"
            }
            (_, crate::library::TrackSort::RecentAdded) => {
                "COALESCE(downloaded_at, created_at) DESC, id DESC"
            }
            (_, crate::library::TrackSort::RecentPlayed) => {
                "last_played_at DESC, play_count DESC, id DESC"
            }
            (_, crate::library::TrackSort::MostPlayed) => "play_count DESC, id DESC",
            (_, crate::library::TrackSort::Duration) => "duration_ms DESC, id",
            (_, crate::library::TrackSort::Size) => "file_size DESC, id",
        };
        let sql = format!(
            "SELECT {TRACK_COLUMNS}
             FROM tracks
             WHERE {filter}
             ORDER BY {order}
             LIMIT ?1"
        );
        let connection = self.lock()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([limit as i64], map_library_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn collections(&self) -> Result<Vec<(String, u64, String, u64)>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT c.kind, c.ncm_id, c.name, COUNT(t.track_id)
             FROM collections c
             LEFT JOIN collection_tracks t
               ON t.collection_kind=c.kind AND t.collection_id=c.ncm_id
             GROUP BY c.kind, c.ncm_id, c.name
             ORDER BY c.kind, c.name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? as u64,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn collection_tracks(
        &self,
        kind: &str,
        collection_id: u64,
        limit: usize,
    ) -> Result<Vec<LibraryTrack>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS_T}
             FROM collection_tracks c
             JOIN tracks t ON t.id=c.track_id
             WHERE c.collection_kind=?1 AND c.collection_id=?2 AND t.status='available'
             ORDER BY c.position, t.id
             LIMIT ?3"
        );
        let connection = self.lock()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![kind, collection_id as i64, limit as i64],
            map_library_track,
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn history(&self, limit: usize) -> Result<Vec<(LibraryTrack, i64)>> {
        let connection = self.lock()?;
        let sql = format!(
            "SELECT {TRACK_COLUMNS_T}, h.played_at
             FROM playback_history h
             JOIN tracks t ON t.id=h.track_id
             ORDER BY h.played_at DESC, h.id DESC
             LIMIT ?1"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok((map_library_track(row)?, row.get::<_, i64>(15)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn set_cover_url(&self, track_id: u64, cover_url: &str) -> Result<bool> {
        let changed = self.lock()?.execute(
            "UPDATE tracks SET cover_url=?1, updated_at=?2 WHERE id=?3",
            params![cover_url, unix_timestamp(), track_id as i64],
        )?;
        Ok(changed > 0)
    }

    pub fn track_detail(&self, track_id: u64) -> Result<Option<TrackDetail>> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT album_artist, release_year, track_number, disc_number,
                        bitrate, audio_md5, cover_url
                 FROM tracks WHERE id=?1",
                [track_id as i64],
                |row| {
                    Ok(TrackDetail {
                        album_artist: row.get(0)?,
                        release_year: row.get(1)?,
                        track_number: row.get::<_, Option<i64>>(2)?.map(|value| value as u32),
                        disc_number: row.get::<_, Option<i64>>(3)?.map(|value| value as u32),
                        bitrate: row.get::<_, i64>(4)? as u64,
                        audio_md5: row.get(5)?,
                        cover_url: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn queue(&self) -> Result<Vec<LibraryTrack>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS_T}
             FROM playback_queue q JOIN tracks t ON t.id=q.track_id
             ORDER BY q.position"
        );
        let connection = self.lock()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([], map_library_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn record_play(&self, track_id: u64, origin: PlayOrigin) -> Result<bool> {
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE tracks SET play_count=play_count+1, last_played_at=?1, updated_at=?1
             WHERE id=?2",
            params![now, track_id as i64],
        )?;
        if changed != 0 {
            transaction.execute(
                "INSERT INTO playback_history(track_id, played_at, origin) VALUES(?1, ?2, ?3)",
                params![track_id as i64, now, origin.as_str()],
            )?;
            record_event(
                &transaction,
                "track",
                track_id,
                "play",
                origin.as_str(),
                now,
            )?;
        }
        transaction.commit()?;
        Ok(changed != 0)
    }

    pub fn organizable_track(&self, track_id: u64) -> Result<Option<OrganizableTrack>> {
        let connection = self.lock()?;
        connection
            .prepare_cached(
                "SELECT id, ncm_id, title, artists, album, album_artist, track_number,
                        disc_number, total_tracks, format, file_path
                 FROM tracks
                 WHERE id=?1 AND status='available' AND source_kind='ncm'",
            )?
            .query_row([track_id as i64], map_organizable_track)
            .optional()
            .map_err(Into::into)
    }

    pub fn organizable_album(&self, album_id: u64) -> Result<Vec<OrganizableTrack>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT id, ncm_id, title, artists, album, album_artist, track_number,
                    disc_number, total_tracks, format, file_path
             FROM tracks
             WHERE album_id=?1 AND status='available' AND source_kind='ncm'
             ORDER BY COALESCE(disc_number, 1), COALESCE(track_number, 0), id",
        )?;
        let rows = statement.query_map([album_id as i64], map_organizable_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn organizable_all(&self) -> Result<Vec<OrganizableTrack>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT id, ncm_id, title, artists, album, album_artist, track_number,
                    disc_number, total_tracks, format, file_path
             FROM tracks
             WHERE status='available' AND source_kind='ncm'
             ORDER BY album_artist COLLATE NOCASE, album COLLATE NOCASE,
                      COALESCE(disc_number, 1), COALESCE(track_number, 0), id",
        )?;
        let rows = statement.query_map([], map_organizable_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn record_move(
        &self,
        track_id: u64,
        old_path: &Path,
        new_path: &Path,
        file_size: u64,
    ) -> Result<bool> {
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let old = old_path.to_string_lossy();
        let new = new_path.to_string_lossy();
        let changed = transaction.execute(
            "UPDATE tracks
             SET file_path=?1, file_size=?2, status='available', updated_at=?3
             WHERE id=?4 AND source_kind='ncm' AND file_path=?5",
            params![new, file_size as i64, now, track_id as i64, old],
        )?;
        if changed != 0 {
            transaction.execute(
                "UPDATE media_locations
                 SET filepath=?1, file_size=?2, is_present=1, last_seen_at=?3
                 WHERE track_id=?4 AND filepath=?5",
                params![new, file_size as i64, now, track_id as i64, old],
            )?;
            record_event(
                &transaction,
                "track",
                track_id,
                "move",
                &format!("{} -> {}", old_path.display(), new_path.display()),
                now,
            )?;
        }
        transaction.commit()?;
        Ok(changed != 0)
    }

    pub fn apply_scan(&self, files: &[ScannedFile], roots: &[PathBuf]) -> Result<ScanChanges> {
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let mut changes = ScanChanges::default();
        let mut seen = HashSet::with_capacity(files.len());

        for file in files {
            let path = file.path.to_string_lossy().into_owned();
            seen.insert(file.path.clone());
            let existing = transaction
                .query_row(
                    "SELECT track_id, file_size, modified_ns, is_present
                     FROM media_locations WHERE filepath=?1",
                    [&path],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, bool>(3)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((track_id, size, modified_ns, present)) = existing {
                transaction.execute(
                    "UPDATE tracks SET title=?1, artists=?2, album=?3, duration_ms=?4,
                         file_path=?5, file_size=?6, format=?7, album_artist=?8,
                         release_year=?9, track_number=?10, disc_number=?11, bitrate=?12,
                         status='available', updated_at=?13
                     WHERE id=?14 AND source_kind='local'",
                    params![
                        file.title,
                        file.artists,
                        file.album,
                        file.duration_ms as i64,
                        path,
                        file.file_size as i64,
                        file.format,
                        file.album_artist,
                        file.release_year,
                        file.track_number.map(|value| value as i64),
                        file.disc_number.map(|value| value as i64),
                        file.bitrate as i64,
                        now,
                        track_id
                    ],
                )?;
                if size == file.file_size as i64 && modified_ns == file.modified_ns && present {
                    transaction.execute(
                        "UPDATE media_locations SET last_seen_at=?1 WHERE filepath=?2",
                        params![now, path],
                    )?;
                    changes.unchanged += 1;
                } else {
                    transaction.execute(
                        "UPDATE media_locations SET format=?1, file_size=?2, modified_ns=?3,
                             is_present=1, last_seen_at=?4 WHERE filepath=?5",
                        params![
                            file.format,
                            file.file_size as i64,
                            file.modified_ns,
                            now,
                            path
                        ],
                    )?;
                    transaction.execute(
                        "UPDATE tracks SET file_path=?1, file_size=?2, format=?3,
                             status='available', updated_at=?4 WHERE id=?5",
                        params![path, file.file_size as i64, file.format, now, track_id],
                    )?;
                    changes.updated += 1;
                }
                continue;
            }

            let migrated_track = transaction
                .query_row("SELECT id FROM tracks WHERE file_path=?1", [&path], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?;
            let track_id = match migrated_track {
                Some(track_id) => {
                    transaction.execute(
                        "UPDATE tracks SET status='available', file_size=?1, format=?2,
                             updated_at=?3 WHERE id=?4",
                        params![file.file_size as i64, file.format, now, track_id],
                    )?;
                    track_id
                }
                None => {
                    transaction.execute(
                        "INSERT INTO tracks(
                             ncm_id, title, artists, album, duration_ms, file_path, file_size,
                             format, album_artist, release_year, track_number, disc_number,
                             bitrate, status, downloaded_at, created_at, updated_at,
                             source_kind, source_id
                         ) VALUES(
                             NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                             'available', ?13, ?13, ?13, 'local', ?5
                         )",
                        params![
                            file.title,
                            file.artists,
                            file.album,
                            file.duration_ms as i64,
                            path,
                            file.file_size as i64,
                            file.format,
                            file.album_artist,
                            file.release_year,
                            file.track_number.map(|value| value as i64),
                            file.disc_number.map(|value| value as i64),
                            file.bitrate as i64,
                            now
                        ],
                    )?;
                    transaction.last_insert_rowid()
                }
            };
            transaction.execute(
                "INSERT INTO media_locations(
                     track_id, filepath, format, file_size, modified_ns, is_present,
                     first_seen_at, last_seen_at
                 ) VALUES(?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)",
                params![
                    track_id,
                    path,
                    file.format,
                    file.file_size as i64,
                    file.modified_ns,
                    now
                ],
            )?;
            changes.added += 1;
        }

        let known_locations = {
            let mut statement = transaction
                .prepare("SELECT track_id, filepath FROM media_locations WHERE is_present=1")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (track_id, path) in known_locations {
            if roots.iter().any(|root| path.starts_with(root)) && !seen.contains(&path) {
                transaction.execute(
                    "UPDATE media_locations SET is_present=0, last_seen_at=?1
                     WHERE filepath=?2",
                    params![now, path.to_string_lossy()],
                )?;
                transaction.execute(
                    "UPDATE tracks SET status='missing', updated_at=?1
                     WHERE id=?2 AND NOT EXISTS(
                         SELECT 1 FROM media_locations WHERE track_id=?2 AND is_present=1
                     )",
                    params![now, track_id],
                )?;
                changes.missing += 1;
            }
        }
        record_event(
            &transaction,
            "library",
            0,
            "scan",
            &format!(
                "added={},updated={},unchanged={},missing={}",
                changes.added, changes.updated, changes.unchanged, changes.missing
            ),
            now,
        )?;
        transaction.commit()?;
        Ok(changes)
    }

    /// Startup maintenance is automatic and only touches rows already known to the DB.
    pub fn reconcile_known_files(&self) -> Result<usize> {
        let connection = self.lock()?;
        let paths = {
            let mut statement = connection.prepare_cached(
                "SELECT id, file_path FROM tracks WHERE file_path IS NOT NULL AND file_path<>''",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let transaction = connection.unchecked_transaction()?;
        let mut changed = 0;
        for (track_id, path) in paths {
            let status = if Path::new(&path).is_file() {
                "available"
            } else {
                "missing"
            };
            changed += transaction.execute(
                "UPDATE tracks SET status=?1, updated_at=?2
                 WHERE id=?3 AND status<>?1",
                params![status, unix_timestamp(), track_id],
            )?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    fn query_tracks(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<LibraryTrack>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(params, map_library_track)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn run_incremental_maintenance(&self) -> Result<()> {
        const MAINTENANCE_INTERVAL: i64 = 32;
        let mut connection = self.lock()?;
        let pending: i64 = connection.query_row(
            "SELECT downloads_since FROM maintenance_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if pending < MAINTENANCE_INTERVAL {
            return Ok(());
        }
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE maintenance_state SET downloads_since=0, last_run_at=?1 WHERE singleton=1",
            [unix_timestamp()],
        )?;
        transaction.execute_batch("PRAGMA optimize;")?;
        transaction.commit()?;
        Ok(())
    }
}

fn map_library_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<LibraryTrack> {
    Ok(LibraryTrack {
        id: row.get::<_, i64>(0)? as u64,
        ncm_id: row.get::<_, Option<i64>>(1)?.map(|id| id as u64),
        source_kind: row.get(2)?,
        title: row.get(3)?,
        artists: row.get(4)?,
        album: row.get(5)?,
        file_path: row
            .get::<_, Option<String>>(6)?
            .filter(|path| !path.is_empty())
            .map(PathBuf::from),
        format: row.get(7)?,
        file_size: row.get::<_, i64>(8)? as u64,
        downloaded_at: row.get::<_, Option<i64>>(9)?.unwrap_or_default(),
        duration_ms: row.get::<_, i64>(10)? as u64,
        favorite: row.get(11)?,
        play_count: row.get::<_, i64>(12)? as u64,
        last_played_at: row.get(13)?,
        cover_url: row.get(14)?,
    })
}

fn map_organizable_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<OrganizableTrack> {
    Ok(OrganizableTrack {
        id: row.get::<_, i64>(0)? as u64,
        ncm_id: row.get::<_, Option<i64>>(1)?.map(|id| id as u64),
        title: row.get(2)?,
        artists: row.get(3)?,
        album: row.get(4)?,
        album_artist: row.get(5)?,
        track_number: row.get(6)?,
        disc_number: row.get(7)?,
        total_tracks: row.get(8)?,
        format: row.get(9)?,
        path: PathBuf::from(row.get::<_, Option<String>>(10)?.unwrap_or_default()),
    })
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn create_v5_schema(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             ncm_id          INTEGER UNIQUE,
             source_kind     TEXT NOT NULL DEFAULT 'ncm',
             source_id       TEXT NOT NULL DEFAULT '',
             title           TEXT NOT NULL,
             artists         TEXT NOT NULL,
             album_id        INTEGER,
             album           TEXT NOT NULL DEFAULT '',
             duration_ms     INTEGER NOT NULL DEFAULT 0,
             album_artist    TEXT NOT NULL DEFAULT '',
             release_year    INTEGER,
             track_number    INTEGER,
             disc_number     INTEGER,
             total_tracks    INTEGER,
             cover_url       TEXT NOT NULL DEFAULT '',
             file_path       TEXT UNIQUE,
             file_size       INTEGER NOT NULL DEFAULT 0,
             format          TEXT NOT NULL DEFAULT '',
             bitrate         INTEGER NOT NULL DEFAULT 0,
             audio_md5       TEXT NOT NULL DEFAULT '',
             status          TEXT NOT NULL DEFAULT 'remote',
             downloaded_at   INTEGER,
             created_at      INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL,
             favorite        INTEGER NOT NULL DEFAULT 0,
             play_count      INTEGER NOT NULL DEFAULT 0,
             last_played_at  INTEGER
         );

         CREATE TABLE IF NOT EXISTS collections (
             kind             TEXT NOT NULL,
             ncm_id           INTEGER NOT NULL,
             name             TEXT NOT NULL,
             updated_at       INTEGER NOT NULL,
             PRIMARY KEY(kind, ncm_id)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS collection_tracks (
             collection_kind  TEXT NOT NULL,
             collection_id    INTEGER NOT NULL,
             track_id         INTEGER NOT NULL,
             position         INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY(collection_kind, collection_id, track_id),
             FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS media_locations (
             track_id      INTEGER NOT NULL,
             filepath      TEXT NOT NULL UNIQUE,
             format        TEXT NOT NULL DEFAULT '',
             file_size     INTEGER NOT NULL DEFAULT 0,
             modified_ns   INTEGER NOT NULL DEFAULT 0,
             is_present    INTEGER NOT NULL DEFAULT 1,
             first_seen_at INTEGER NOT NULL,
             last_seen_at  INTEGER NOT NULL,
             PRIMARY KEY(track_id, filepath),
             FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
         );

         CREATE TABLE IF NOT EXISTS playback_queue (
             position    INTEGER PRIMARY KEY,
             track_id    INTEGER NOT NULL,
             enqueued_at INTEGER NOT NULL,
             FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
         );

         CREATE TABLE IF NOT EXISTS playback_history (
             id        INTEGER PRIMARY KEY AUTOINCREMENT,
             track_id  INTEGER NOT NULL,
             played_at INTEGER NOT NULL,
             origin    TEXT NOT NULL DEFAULT 'local',
             FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE
         );

         CREATE TABLE IF NOT EXISTS playback_state (
             singleton      INTEGER PRIMARY KEY CHECK(singleton=1),
             current_index  INTEGER,
             updated_at     INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO playback_state(singleton) VALUES(1);

         CREATE TABLE IF NOT EXISTS lifecycle_events (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             entity_kind TEXT NOT NULL,
             entity_id   TEXT NOT NULL DEFAULT '',
             action      TEXT NOT NULL,
             detail      TEXT NOT NULL DEFAULT '',
             created_at  INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS maintenance_state (
             singleton       INTEGER PRIMARY KEY CHECK(singleton=1),
             downloads_since INTEGER NOT NULL DEFAULT 0,
             last_run_at     INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO maintenance_state(singleton) VALUES(1);",
    )?;
    create_v5_indexes(transaction)
}

fn create_v5_indexes(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tracks_source
             ON tracks(source_kind, source_id) WHERE source_id<>'';
         CREATE INDEX IF NOT EXISTS idx_tracks_downloaded
             ON tracks(status, downloaded_at DESC);
         CREATE INDEX IF NOT EXISTS idx_tracks_available_title
             ON tracks(title COLLATE NOCASE, id) WHERE status='available';
         CREATE INDEX IF NOT EXISTS idx_tracks_album
             ON tracks(album_id, id);
         CREATE INDEX IF NOT EXISTS idx_tracks_artists
             ON tracks(artists COLLATE NOCASE, title COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_collection_tracks_track
             ON collection_tracks(track_id);
         CREATE INDEX IF NOT EXISTS idx_media_locations_present
             ON media_locations(is_present, track_id);
         CREATE INDEX IF NOT EXISTS idx_playback_history_recent
             ON playback_history(played_at DESC, id DESC);
         CREATE INDEX IF NOT EXISTS idx_lifecycle_events_recent
             ON lifecycle_events(created_at DESC, id DESC);

         CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
             title, artists, album,
             content='tracks', content_rowid='id',
             tokenize='unicode61 remove_diacritics 2'
         );

         DROP TRIGGER IF EXISTS tracks_ai;
         DROP TRIGGER IF EXISTS tracks_ad;
         DROP TRIGGER IF EXISTS tracks_au;
         CREATE TRIGGER tracks_ai AFTER INSERT ON tracks BEGIN
             INSERT INTO tracks_fts(rowid, title, artists, album)
             VALUES (new.id, new.title, new.artists, new.album);
         END;
         CREATE TRIGGER tracks_ad AFTER DELETE ON tracks BEGIN
             INSERT INTO tracks_fts(tracks_fts, rowid, title, artists, album)
             VALUES ('delete', old.id, old.title, old.artists, old.album);
         END;
         CREATE TRIGGER tracks_au AFTER UPDATE OF title, artists, album ON tracks BEGIN
             INSERT INTO tracks_fts(tracks_fts, rowid, title, artists, album)
             VALUES ('delete', old.id, old.title, old.artists, old.album);
             INSERT INTO tracks_fts(rowid, title, artists, album)
             VALUES (new.id, new.title, new.artists, new.album);
         END;",
    )
}

fn ensure_legacy_columns(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (
             ncm_id          INTEGER PRIMARY KEY,
             title           TEXT NOT NULL,
             artists         TEXT NOT NULL,
             album_id        INTEGER,
             album           TEXT NOT NULL DEFAULT '',
             duration_ms     INTEGER NOT NULL DEFAULT 0,
             album_artist    TEXT NOT NULL DEFAULT '',
             release_year    INTEGER,
             track_number    INTEGER,
             disc_number     INTEGER,
             total_tracks    INTEGER,
             cover_url       TEXT NOT NULL DEFAULT '',
             file_path       TEXT NOT NULL UNIQUE,
             file_size       INTEGER NOT NULL DEFAULT 0,
             format          TEXT NOT NULL DEFAULT '',
             bitrate         INTEGER NOT NULL DEFAULT 0,
             audio_md5       TEXT NOT NULL DEFAULT '',
             status          TEXT NOT NULL DEFAULT 'available',
             downloaded_at   INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL,
             source_kind     TEXT NOT NULL DEFAULT 'ncm',
             source_id       TEXT NOT NULL DEFAULT '',
             favorite        INTEGER NOT NULL DEFAULT 0,
             play_count      INTEGER NOT NULL DEFAULT 0,
             last_played_at  INTEGER
         );
         CREATE TABLE IF NOT EXISTS collections (
             kind             TEXT NOT NULL,
             ncm_id           INTEGER NOT NULL,
             name             TEXT NOT NULL,
             updated_at       INTEGER NOT NULL,
             PRIMARY KEY(kind, ncm_id)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS collection_tracks (
             collection_kind  TEXT NOT NULL,
             collection_id    INTEGER NOT NULL,
             track_id         INTEGER NOT NULL,
             position         INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY(collection_kind, collection_id, track_id)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS media_locations (
             track_id      INTEGER NOT NULL,
             filepath      TEXT NOT NULL UNIQUE,
             format        TEXT NOT NULL DEFAULT '',
             file_size     INTEGER NOT NULL DEFAULT 0,
             modified_ns   INTEGER NOT NULL DEFAULT 0,
             is_present    INTEGER NOT NULL DEFAULT 1,
             first_seen_at INTEGER NOT NULL,
             last_seen_at  INTEGER NOT NULL,
             PRIMARY KEY(track_id, filepath)
         );
         CREATE TABLE IF NOT EXISTS playback_queue (
             position    INTEGER PRIMARY KEY,
             track_id    INTEGER NOT NULL UNIQUE,
             enqueued_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS playback_history (
             id        INTEGER PRIMARY KEY AUTOINCREMENT,
             track_id  INTEGER NOT NULL,
             played_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS lifecycle_events (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             entity_kind TEXT NOT NULL,
             entity_id   TEXT NOT NULL DEFAULT '',
             action      TEXT NOT NULL,
             detail      TEXT NOT NULL DEFAULT '',
             created_at  INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS maintenance_state (
             singleton       INTEGER PRIMARY KEY CHECK(singleton=1),
             downloads_since INTEGER NOT NULL DEFAULT 0,
             last_run_at     INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO maintenance_state(singleton) VALUES(1);",
    )?;
    if !table_has_column(transaction, "tracks", "source_kind")? {
        transaction.execute_batch(
            "ALTER TABLE tracks ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'ncm';
             ALTER TABLE tracks ADD COLUMN source_id TEXT NOT NULL DEFAULT '';
             ALTER TABLE tracks ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE tracks ADD COLUMN play_count INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE tracks ADD COLUMN last_played_at INTEGER;",
        )?;
    }
    for (column, definition) in [
        ("album_id", "INTEGER"),
        ("album_artist", "TEXT NOT NULL DEFAULT ''"),
        ("release_year", "INTEGER"),
        ("track_number", "INTEGER"),
        ("disc_number", "INTEGER"),
        ("total_tracks", "INTEGER"),
        ("cover_url", "TEXT NOT NULL DEFAULT ''"),
    ] {
        if !table_has_column(transaction, "tracks", column)? {
            transaction.execute(
                &format!("ALTER TABLE tracks ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn migrate_legacy_to_v5(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS tracks_ai;
         DROP TRIGGER IF EXISTS tracks_ad;
         DROP TRIGGER IF EXISTS tracks_au;
         DROP TABLE IF EXISTS tracks_fts;
         DROP TABLE IF EXISTS tracks_new;
         DROP TABLE IF EXISTS track_id_map;

         CREATE TABLE tracks_new (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             old_pk          INTEGER NOT NULL UNIQUE,
             ncm_id          INTEGER UNIQUE,
             source_kind     TEXT NOT NULL DEFAULT 'ncm',
             source_id       TEXT NOT NULL DEFAULT '',
             title           TEXT NOT NULL,
             artists         TEXT NOT NULL,
             album_id        INTEGER,
             album           TEXT NOT NULL DEFAULT '',
             duration_ms     INTEGER NOT NULL DEFAULT 0,
             album_artist    TEXT NOT NULL DEFAULT '',
             release_year    INTEGER,
             track_number    INTEGER,
             disc_number     INTEGER,
             total_tracks    INTEGER,
             cover_url       TEXT NOT NULL DEFAULT '',
             file_path       TEXT UNIQUE,
             file_size       INTEGER NOT NULL DEFAULT 0,
             format          TEXT NOT NULL DEFAULT '',
             bitrate         INTEGER NOT NULL DEFAULT 0,
             audio_md5       TEXT NOT NULL DEFAULT '',
             status          TEXT NOT NULL DEFAULT 'remote',
             downloaded_at   INTEGER,
             created_at      INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL,
             favorite        INTEGER NOT NULL DEFAULT 0,
             play_count      INTEGER NOT NULL DEFAULT 0,
             last_played_at  INTEGER
         );

         INSERT INTO tracks_new (
             old_pk, ncm_id, source_kind, source_id, title, artists, album_id, album,
             duration_ms, album_artist, release_year, track_number, disc_number,
             total_tracks, cover_url, file_path, file_size, format, bitrate, audio_md5,
             status, downloaded_at, created_at, updated_at, favorite, play_count,
             last_played_at
         )
         SELECT
             ncm_id,
             CASE WHEN COALESCE(source_kind, 'ncm')='ncm' THEN ncm_id END,
             COALESCE(source_kind, 'ncm'),
             CASE
                 WHEN COALESCE(source_kind, 'ncm')='local'
                     THEN COALESCE(NULLIF(source_id, ''), file_path)
                 ELSE CAST(ncm_id AS TEXT)
             END,
             title, artists, album_id, album, duration_ms, album_artist, release_year,
             track_number, disc_number, total_tracks, cover_url, NULLIF(file_path, ''),
             file_size, format, bitrate, audio_md5,
             CASE WHEN status IN ('available', 'missing', 'remote') THEN status ELSE 'available' END,
             downloaded_at, downloaded_at, updated_at, favorite, play_count, last_played_at
         FROM tracks;

         CREATE TABLE track_id_map (
             old_id INTEGER PRIMARY KEY,
             new_id INTEGER NOT NULL
         );
         INSERT INTO track_id_map(old_id, new_id)
         SELECT old_pk, id FROM tracks_new;",
    )?;

    transaction.execute_batch(
        "CREATE TABLE collection_tracks_new (
             collection_kind  TEXT NOT NULL,
             collection_id    INTEGER NOT NULL,
             track_id         INTEGER NOT NULL,
             position         INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY(collection_kind, collection_id, track_id)
         ) WITHOUT ROWID;
         INSERT OR IGNORE INTO collection_tracks_new
         SELECT c.collection_kind, c.collection_id, m.new_id, c.position
         FROM collection_tracks c
         JOIN track_id_map m ON m.old_id=c.track_id;
         DROP TABLE collection_tracks;
         ALTER TABLE collection_tracks_new RENAME TO collection_tracks;

         CREATE TABLE media_locations_new (
             track_id      INTEGER NOT NULL,
             filepath      TEXT NOT NULL UNIQUE,
             format        TEXT NOT NULL DEFAULT '',
             file_size     INTEGER NOT NULL DEFAULT 0,
             modified_ns   INTEGER NOT NULL DEFAULT 0,
             is_present    INTEGER NOT NULL DEFAULT 1,
             first_seen_at INTEGER NOT NULL,
             last_seen_at  INTEGER NOT NULL,
             PRIMARY KEY(track_id, filepath)
         );
         INSERT OR IGNORE INTO media_locations_new
         SELECT m.new_id, l.filepath, l.format, l.file_size, l.modified_ns,
                l.is_present, l.first_seen_at, l.last_seen_at
         FROM media_locations l
         JOIN track_id_map m ON m.old_id=l.track_id;
         DROP TABLE media_locations;
         ALTER TABLE media_locations_new RENAME TO media_locations;

         CREATE TABLE playback_queue_new (
             position    INTEGER PRIMARY KEY,
             track_id    INTEGER NOT NULL,
             enqueued_at INTEGER NOT NULL
         );
         INSERT OR IGNORE INTO playback_queue_new
         SELECT q.position, m.new_id, q.enqueued_at
         FROM playback_queue q
         JOIN track_id_map m ON m.old_id=q.track_id;
         DROP TABLE playback_queue;
         ALTER TABLE playback_queue_new RENAME TO playback_queue;

         CREATE TABLE playback_history_new (
             id        INTEGER PRIMARY KEY AUTOINCREMENT,
             track_id  INTEGER NOT NULL,
             played_at INTEGER NOT NULL,
             origin    TEXT NOT NULL DEFAULT 'local'
         );
         INSERT INTO playback_history_new(track_id, played_at, origin)
         SELECT m.new_id, h.played_at, 'local'
         FROM playback_history h
         JOIN track_id_map m ON m.old_id=h.track_id;
         DROP TABLE playback_history;
         ALTER TABLE playback_history_new RENAME TO playback_history;

         CREATE TABLE IF NOT EXISTS playback_state (
             singleton      INTEGER PRIMARY KEY CHECK(singleton=1),
             current_index  INTEGER,
             updated_at     INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO playback_state(singleton) VALUES(1);

         DROP TABLE tracks;
         CREATE TABLE tracks (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             ncm_id          INTEGER UNIQUE,
             source_kind     TEXT NOT NULL DEFAULT 'ncm',
             source_id       TEXT NOT NULL DEFAULT '',
             title           TEXT NOT NULL,
             artists         TEXT NOT NULL,
             album_id        INTEGER,
             album           TEXT NOT NULL DEFAULT '',
             duration_ms     INTEGER NOT NULL DEFAULT 0,
             album_artist    TEXT NOT NULL DEFAULT '',
             release_year    INTEGER,
             track_number    INTEGER,
             disc_number     INTEGER,
             total_tracks    INTEGER,
             cover_url       TEXT NOT NULL DEFAULT '',
             file_path       TEXT UNIQUE,
             file_size       INTEGER NOT NULL DEFAULT 0,
             format          TEXT NOT NULL DEFAULT '',
             bitrate         INTEGER NOT NULL DEFAULT 0,
             audio_md5       TEXT NOT NULL DEFAULT '',
             status          TEXT NOT NULL DEFAULT 'remote',
             downloaded_at   INTEGER,
             created_at      INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL,
             favorite        INTEGER NOT NULL DEFAULT 0,
             play_count      INTEGER NOT NULL DEFAULT 0,
             last_played_at  INTEGER
         );
         INSERT INTO tracks (
             id, ncm_id, source_kind, source_id, title, artists, album_id, album,
             duration_ms, album_artist, release_year, track_number, disc_number,
             total_tracks, cover_url, file_path, file_size, format, bitrate, audio_md5,
             status, downloaded_at, created_at, updated_at, favorite, play_count,
             last_played_at
         )
         SELECT
             id, ncm_id, source_kind, source_id, title, artists, album_id, album,
             duration_ms, album_artist, release_year, track_number, disc_number,
             total_tracks, cover_url, file_path, file_size, format, bitrate, audio_md5,
             status, downloaded_at, created_at, updated_at, favorite, play_count,
             last_played_at
         FROM tracks_new;
         DROP TABLE tracks_new;
         DROP TABLE track_id_map;",
    )?;
    create_v5_indexes(transaction)?;
    transaction.execute("INSERT INTO tracks_fts(tracks_fts) VALUES('rebuild')", [])?;
    Ok(())
}

fn record_event(
    transaction: &rusqlite::Transaction<'_>,
    entity_kind: &str,
    entity_id: u64,
    action: &str,
    detail: &str,
    now: i64,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO lifecycle_events(entity_kind, entity_id, action, detail, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![entity_kind, entity_id.to_string(), action, detail, now],
    )?;
    Ok(())
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downloaded_track(id: u64, path: PathBuf) -> DownloadedTrack {
        DownloadedTrack {
            ncm_id: id,
            title: "透明数据库测试".to_owned(),
            artists: "Rust Artist".to_owned(),
            album_id: Some(7),
            album: "Fast Queries".to_owned(),
            duration_ms: 1_000,
            album_artist: "Rust Artist".to_owned(),
            year: Some(2026),
            track_number: Some(1),
            disc_number: Some(1),
            total_tracks: Some(12),
            cover_url: "https://example.invalid/cover.jpg".to_owned(),
            file_path: path,
            file_size: 10,
            format: "mp3".to_owned(),
            bitrate: 320_000,
            audio_md5: "abc".to_owned(),
            collection_id: None,
            collection_kind: String::new(),
            collection_name: String::new(),
            collection_position: 0,
        }
    }

    #[test]
    fn incrementally_records_and_searches_downloads() {
        let directory = tempfile::tempdir().unwrap();
        let audio_path = directory.path().join("song.mp3");
        fs::write(&audio_path, b"test audio").unwrap();
        let database = LibraryDb::open(directory.path()).unwrap();
        let mut track = downloaded_track(42, audio_path.clone());
        track.collection_id = Some(9);
        track.collection_kind = "playlist".to_owned();
        track.collection_name = "Tests".to_owned();
        track.collection_position = 1;
        database.record_download(&track).unwrap();

        let id = database.id_by_ncm(42).unwrap().unwrap();
        assert_ne!(id, 42);
        assert_eq!(database.available_path(id).unwrap(), Some(audio_path));
        assert_eq!(database.search("透明", 10).unwrap().len(), 1);
        assert_eq!(database.stats().unwrap().tracks, 1);
    }

    #[test]
    fn missing_files_are_reconciled_during_batch_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let audio_path = directory.path().join("removed.mp3");
        fs::write(&audio_path, b"test audio").unwrap();
        let database = LibraryDb::open(directory.path()).unwrap();
        database
            .record_download(&downloaded_track(51, audio_path.clone()))
            .unwrap();

        fs::remove_file(audio_path).unwrap();

        assert!(database.available_ids(&[51]).unwrap().is_empty());
        assert_eq!(database.stats().unwrap().missing, 1);
    }

    #[test]
    fn move_updates_the_transparent_library_path() {
        let directory = tempfile::tempdir().unwrap();
        let old_path = directory.path().join("old.mp3");
        let new_path = directory.path().join("Artist").join("new.mp3");
        fs::write(&old_path, b"test audio").unwrap();
        let database = LibraryDb::open(directory.path()).unwrap();
        database
            .record_download(&downloaded_track(52, old_path.clone()))
            .unwrap();
        let id = database.id_by_ncm(52).unwrap().unwrap();

        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::rename(&old_path, &new_path).unwrap();
        assert!(database.record_move(id, &old_path, &new_path, 10).unwrap());

        assert_eq!(database.available_path(id).unwrap(), Some(new_path));
    }

    #[test]
    fn membership_updates_preserve_collection_positions() {
        let directory = tempfile::tempdir().unwrap();
        let audio_path = directory.path().join("existing.mp3");
        fs::write(&audio_path, b"test audio").unwrap();
        let database = LibraryDb::open(directory.path()).unwrap();
        database
            .record_download(&downloaded_track(61, audio_path))
            .unwrap();

        database
            .record_collection_membership("album", 12, "Ordered", &[(61, 7)])
            .unwrap();

        let connection = database.lock().unwrap();
        let position: i64 = connection
            .query_row(
                "SELECT position FROM collection_tracks
                 WHERE collection_kind='album' AND collection_id=12",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(position, 7);
    }

    #[test]
    fn upgrades_legacy_single_column_schema_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = directory.path().join(INTERNAL_DIR);
        fs::create_dir_all(&state_dir).unwrap();
        let legacy = Connection::open(state_dir.join(DATABASE_NAME)).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE schema_meta (version INTEGER NOT NULL);
                 INSERT INTO schema_meta(version) VALUES(1);",
            )
            .unwrap();
        drop(legacy);

        let database = LibraryDb::open(directory.path()).unwrap();
        let version: i64 = database
            .lock()
            .unwrap()
            .query_row("SELECT MAX(version) FROM schema_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migrates_legacy_ncm_primary_keys_to_surrogate_ids() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = directory.path().join(INTERNAL_DIR);
        fs::create_dir_all(&state_dir).unwrap();
        let path = directory.path().join("legacy.mp3");
        fs::write(&path, b"audio").unwrap();
        let legacy = Connection::open(state_dir.join(DATABASE_NAME)).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE schema_meta (version INTEGER NOT NULL);
                 INSERT INTO schema_meta(version) VALUES(4);
                 CREATE TABLE tracks (
                     ncm_id INTEGER PRIMARY KEY,
                     title TEXT NOT NULL,
                     artists TEXT NOT NULL,
                     album TEXT NOT NULL DEFAULT '',
                     duration_ms INTEGER NOT NULL DEFAULT 0,
                     file_path TEXT NOT NULL UNIQUE,
                     file_size INTEGER NOT NULL DEFAULT 0,
                     format TEXT NOT NULL DEFAULT '',
                     bitrate INTEGER NOT NULL DEFAULT 0,
                     audio_md5 TEXT NOT NULL DEFAULT '',
                     status TEXT NOT NULL DEFAULT 'available',
                     downloaded_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL,
                     source_kind TEXT NOT NULL DEFAULT 'ncm',
                     source_id TEXT NOT NULL DEFAULT '',
                     favorite INTEGER NOT NULL DEFAULT 0,
                     play_count INTEGER NOT NULL DEFAULT 0,
                     last_played_at INTEGER
                 );
                 CREATE TABLE collections (
                     kind TEXT NOT NULL,
                     ncm_id INTEGER NOT NULL,
                     name TEXT NOT NULL,
                     updated_at INTEGER NOT NULL,
                     PRIMARY KEY(kind, ncm_id)
                 );
                 CREATE TABLE collection_tracks (
                     collection_kind TEXT NOT NULL,
                     collection_id INTEGER NOT NULL,
                     track_id INTEGER NOT NULL,
                     position INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY(collection_kind, collection_id, track_id)
                 );
                 CREATE TABLE media_locations (
                     track_id INTEGER NOT NULL,
                     filepath TEXT NOT NULL UNIQUE,
                     format TEXT NOT NULL DEFAULT '',
                     file_size INTEGER NOT NULL DEFAULT 0,
                     modified_ns INTEGER NOT NULL DEFAULT 0,
                     is_present INTEGER NOT NULL DEFAULT 1,
                     first_seen_at INTEGER NOT NULL,
                     last_seen_at INTEGER NOT NULL,
                     PRIMARY KEY(track_id, filepath)
                 );
                 CREATE TABLE playback_queue (
                     position INTEGER PRIMARY KEY,
                     track_id INTEGER NOT NULL UNIQUE,
                     enqueued_at INTEGER NOT NULL
                 );
                 CREATE TABLE playback_history (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     track_id INTEGER NOT NULL,
                     played_at INTEGER NOT NULL
                 );
                 CREATE TABLE lifecycle_events (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     entity_kind TEXT NOT NULL,
                     entity_id TEXT NOT NULL DEFAULT '',
                     action TEXT NOT NULL,
                     detail TEXT NOT NULL DEFAULT '',
                     created_at INTEGER NOT NULL
                 );
                 CREATE TABLE maintenance_state (
                     singleton INTEGER PRIMARY KEY,
                     downloads_since INTEGER NOT NULL DEFAULT 0,
                     last_run_at INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO maintenance_state(singleton) VALUES(1);",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO tracks(
                     ncm_id, title, artists, file_path, downloaded_at, updated_at, source_kind
                 ) VALUES (99, '旧主键', 'A', ?1, 1, 1, 'ncm')",
                [path.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(legacy);

        let database = LibraryDb::open(directory.path()).unwrap();
        let id = database.id_by_ncm(99).unwrap().unwrap();
        assert_ne!(id, 99);
        assert_eq!(database.search("旧主键", 10).unwrap()[0].ncm_id, Some(99));
    }

    #[test]
    fn favorite_queue_and_history_are_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let audio_path = directory.path().join("queue.mp3");
        fs::write(&audio_path, b"test audio").unwrap();
        let database = LibraryDb::open(directory.path()).unwrap();
        database
            .record_download(&downloaded_track(71, audio_path))
            .unwrap();
        let id = database.id_by_ncm(71).unwrap().unwrap();

        assert_eq!(database.toggle_favorite(id).unwrap(), Some(true));
        assert_eq!(database.enqueue_next(id).unwrap(), EnqueueOutcome::Inserted);
        assert_eq!(
            database.enqueue_next(id).unwrap(),
            EnqueueOutcome::AlreadyNext
        );
        assert_eq!(database.enqueue_next(999).unwrap(), EnqueueOutcome::Missing);
        assert_eq!(database.queue().unwrap().len(), 1);
        assert!(database.record_play(id, PlayOrigin::Local).unwrap());
        assert_eq!(database.queue().unwrap().len(), 1);
        assert_eq!(database.history(10).unwrap().len(), 1);
        let stats = database.stats().unwrap();
        assert_eq!(stats.favorites, 1);
        assert_eq!(stats.plays, 1);
    }

    #[test]
    fn streaming_tracks_share_the_library_and_recent_history() {
        let directory = tempfile::tempdir().unwrap();
        let database = LibraryDb::open(directory.path()).unwrap();
        let id = database
            .ensure_catalog(&CatalogTrack {
                ncm_id: 347230,
                title: "海阔天空".into(),
                artists: "Beyond".into(),
                album: "乐与怒".into(),
                duration_ms: 1000,
                cover_url: String::new(),
            })
            .unwrap();
        assert_ne!(id, 347230);
        assert!(database.available_path(id).unwrap().is_none());
        assert!(database.record_play(id, PlayOrigin::Stream).unwrap());
        let history = database.history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].0.ncm_id, Some(347230));
        assert_eq!(history[0].0.title, "海阔天空");
        database.replace_queue(&[id, id], Some(0)).unwrap();
        assert_eq!(database.queue_index().unwrap(), Some(0));
        assert_eq!(database.queue().unwrap().len(), 2);
        assert_eq!(database.queue().unwrap()[0].id, id);
    }
}
