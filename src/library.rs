//! User-facing local library service. Storage details deliberately stay private.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    database::{DatabaseError, LibraryDb, LibraryTrack},
    organizer::{self, MoveOutcome, OrganizerError},
    scanner,
};

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("local library storage failed: {0}")]
    Database(#[from] DatabaseError),
    #[error("library organization failed: {0}")]
    Organizer(#[from] OrganizerError),
}

pub type Result<T> = std::result::Result<T, LibraryError>;

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub path: PathBuf,
    pub format: String,
    pub bytes: u64,
    pub downloaded_at: i64,
    pub duration_ms: u64,
    pub favorite: bool,
    pub play_count: u64,
    pub last_played_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LibraryStats {
    pub tracks: u64,
    pub missing: u64,
    pub albums: u64,
    pub duration_ms: u64,
    pub favorites: u64,
    pub plays: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanReport {
    pub discovered: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub missing: usize,
}

#[derive(Clone)]
pub struct Library {
    inner: LibraryDb,
    root: PathBuf,
}

impl Library {
    /// Uses the same download-root-relative state as the downloader.
    pub fn open(download_root: impl AsRef<Path>) -> Result<Self> {
        let root = download_root.as_ref().to_path_buf();
        Ok(Self {
            inner: LibraryDb::open(&root)?,
            root,
        })
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<Track>> {
        Ok(map_tracks(self.inner.recent(limit)?))
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>> {
        Ok(map_tracks(self.inner.search(query, limit)?))
    }

    pub fn list(&self, favorites_only: bool, limit: usize) -> Result<Vec<Track>> {
        Ok(map_tracks(self.inner.list(favorites_only, limit)?))
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let stats = self.inner.stats()?;
        Ok(LibraryStats {
            tracks: stats.tracks,
            missing: stats.missing,
            albums: stats.albums,
            duration_ms: stats.duration_ms,
            favorites: stats.favorites,
            plays: stats.plays,
        })
    }

    pub fn track_path(&self, track_id: u64) -> Result<Option<PathBuf>> {
        Ok(self.inner.available_path(track_id)?)
    }

    pub fn scan(&self, roots: &[PathBuf]) -> Result<ScanReport> {
        let (roots, files) = scanner::collect(roots)?;
        let changes = self.inner.apply_scan(&files, &roots)?;
        Ok(ScanReport {
            discovered: files.len(),
            added: changes.added,
            updated: changes.updated,
            unchanged: changes.unchanged,
            missing: changes.missing,
        })
    }

    pub fn toggle_favorite(&self, track_id: u64) -> Result<Option<bool>> {
        Ok(self.inner.toggle_favorite(track_id)?)
    }

    pub fn enqueue(&self, track_id: u64) -> Result<bool> {
        Ok(self.inner.enqueue(track_id)?)
    }

    pub fn dequeue(&self, track_id: u64) -> Result<bool> {
        Ok(self.inner.dequeue(track_id)?)
    }

    pub fn queue(&self) -> Result<Vec<Track>> {
        Ok(map_tracks(self.inner.queue()?))
    }

    pub fn record_play(&self, track_id: u64) -> Result<bool> {
        Ok(self.inner.record_play(track_id)?)
    }

    pub fn organize_track(&self, track_id: u64) -> Result<Option<MoveOutcome>> {
        let Some(track) = self.inner.organizable_track(track_id)? else {
            return Ok(None);
        };
        self.organize_one(track).map(Some)
    }

    pub fn organize_album(&self, album_id: u64) -> Result<Vec<MoveOutcome>> {
        self.inner
            .organizable_album(album_id)?
            .into_iter()
            .map(|track| self.organize_one(track))
            .collect()
    }

    pub fn organize_all(&self) -> Result<Vec<MoveOutcome>> {
        self.inner
            .organizable_all()?
            .into_iter()
            .map(|track| self.organize_one(track))
            .collect()
    }

    fn organize_one(&self, track: organizer::OrganizableTrack) -> Result<MoveOutcome> {
        let target = organizer::destination(&self.root, &track);
        let outcome = organizer::move_track(&track.path, &target)?;
        if let MoveOutcome::Moved { from, to, bytes } = &outcome
            && let Err(error) = self.inner.record_move(track.id, from, to, *bytes)
        {
            organizer::rollback_move(from, to);
            return Err(error.into());
        }
        Ok(outcome)
    }
}

fn map_tracks(tracks: Vec<LibraryTrack>) -> Vec<Track> {
    tracks
        .into_iter()
        .map(|track| Track {
            id: track.ncm_id,
            title: track.title,
            artists: track.artists,
            album: track.album,
            path: track.file_path,
            format: track.format,
            bytes: track.file_size,
            downloaded_at: track.downloaded_at,
            duration_ms: track.duration_ms,
            favorite: track.favorite,
            play_count: track.play_count,
            last_played_at: track.last_played_at,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn scan_is_incremental_and_marks_removed_files_missing() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        let song = directory.path().join("Artist - Song.flac");
        fs::write(&song, b"audio data").unwrap();

        let first = library.scan(&[directory.path().to_path_buf()]).unwrap();
        assert_eq!(first.discovered, 1);
        assert_eq!(first.added, 1);
        assert_eq!(library.stats().unwrap().tracks, 1);
        let tracks = library.search("Song", 10).unwrap();
        assert_eq!(tracks[0].artists, "Artist");

        let second = library.scan(&[directory.path().to_path_buf()]).unwrap();
        assert_eq!(second.unchanged, 1);

        fs::remove_file(song).unwrap();
        let third = library.scan(&[directory.path().to_path_buf()]).unwrap();
        assert_eq!(third.missing, 1);
        let stats = library.stats().unwrap();
        assert_eq!(stats.tracks, 0);
        assert_eq!(stats.missing, 1);
    }

    #[test]
    fn importing_another_root_does_not_mark_existing_tracks_missing() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("A - One.mp3"), b"audio").unwrap();
        fs::write(second.join("B - Two.flac"), b"audio").unwrap();

        library.scan(&[first]).unwrap();
        let imported = library.scan(&[second]).unwrap();

        assert_eq!(imported.discovered, 1);
        assert_eq!(imported.added, 1);
        assert_eq!(imported.missing, 0);
        assert_eq!(library.stats().unwrap().tracks, 2);
        assert_eq!(library.search("Two", 10).unwrap()[0].artists, "B");
    }
}
