//! Persistent, size-bounded cache for streamed playback audio.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use stream_download::storage::StorageProvider;

pub const DEFAULT_PLAYBACK_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const COMPLETION_PENDING: u8 = 0;
const COMPLETION_FINISHING: u8 = 1;
const COMPLETION_COMPLETE: u8 = 2;
const COMPLETION_FAILED: u8 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub entries: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClearReport {
    pub removed_bytes: u64,
    pub retained_active: usize,
    pub stats: CacheStats,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheEntry {
    file_name: String,
    extension: String,
    size: u64,
    last_accessed: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct CacheState {
    max_bytes: u64,
    access_clock: u64,
    entries: HashMap<u64, CacheEntry>,
}

impl Default for CacheState {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_PLAYBACK_CACHE_BYTES,
            access_clock: 0,
            entries: HashMap::new(),
        }
    }
}

#[derive(Debug)]
struct CacheInner {
    state: CacheState,
    active: HashMap<PathBuf, usize>,
}

#[derive(Clone, Debug)]
pub struct PlaybackCache {
    data_dir: Arc<PathBuf>,
    state_path: Arc<PathBuf>,
    inner: Arc<Mutex<CacheInner>>,
}

impl PlaybackCache {
    pub async fn open(path: impl Into<PathBuf>, initial_max_bytes: u64) -> io::Result<Self> {
        let path = path.into();
        tokio::task::spawn_blocking(move || Self::open_sync(path, initial_max_bytes))
            .await
            .map_err(join_error)?
    }

    #[cfg(test)]
    pub(crate) fn open_blocking(
        path: impl Into<PathBuf>,
        initial_max_bytes: u64,
    ) -> io::Result<Self> {
        Self::open_sync(path.into(), initial_max_bytes)
    }

    fn open_sync(root: PathBuf, initial_max_bytes: u64) -> io::Result<Self> {
        let data_dir = root.join("audio");
        let state_path = root.join("state.json");
        fs::create_dir_all(&data_dir)?;
        let mut state = if state_path.is_file() {
            serde_json::from_slice::<CacheState>(&fs::read(&state_path)?).unwrap_or_else(|_| {
                CacheState {
                    max_bytes: initial_max_bytes,
                    ..CacheState::default()
                }
            })
        } else {
            CacheState {
                max_bytes: initial_max_bytes,
                ..CacheState::default()
            }
        };

        state.entries.retain(|_, entry| {
            let path = data_dir.join(&entry.file_name);
            fs::metadata(path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == entry.size)
        });
        state.access_clock = state.access_clock.max(
            state
                .entries
                .values()
                .map(|entry| entry.last_accessed)
                .max()
                .unwrap_or_default(),
        );
        let known = state
            .entries
            .values()
            .map(|entry| entry.file_name.as_str())
            .collect::<HashSet<_>>();
        for entry in fs::read_dir(&data_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && !known.contains(entry.file_name().to_string_lossy().as_ref())
            {
                let _ = fs::remove_file(entry.path());
            }
        }

        let cache = Self {
            data_dir: Arc::new(data_dir),
            state_path: Arc::new(state_path),
            inner: Arc::new(Mutex::new(CacheInner {
                state,
                active: HashMap::new(),
            })),
        };
        {
            let mut inner = cache.lock();
            cache.trim_locked(&mut inner);
            cache.persist_locked(&inner)?;
        }
        Ok(cache)
    }

    pub fn stats(&self) -> CacheStats {
        stats_locked(&self.lock())
    }

    pub async fn lookup(&self, song_id: u64) -> io::Result<Option<CacheHit>> {
        let cache = self.clone();
        tokio::task::spawn_blocking(move || cache.lookup_sync(song_id))
            .await
            .map_err(join_error)?
    }

    fn lookup_sync(&self, song_id: u64) -> io::Result<Option<CacheHit>> {
        let mut inner = self.lock();
        let Some(entry) = inner.state.entries.get(&song_id).cloned() else {
            return Ok(None);
        };
        let path = self.data_dir.join(&entry.file_name);
        let valid = fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == entry.size);
        if !valid {
            inner.state.entries.remove(&song_id);
            self.persist_locked(&inner)?;
            let _ = fs::remove_file(path);
            return Ok(None);
        }
        let file = File::open(&path)?;
        let last_accessed = next_access_stamp(&mut inner);
        if let Some(entry) = inner.state.entries.get_mut(&song_id) {
            entry.last_accessed = last_accessed;
        }
        *inner.active.entry(path.clone()).or_default() += 1;
        self.persist_locked(&inner)?;
        Ok(Some(CacheHit {
            file,
            extension: entry.extension,
            size: entry.size,
            _lease: CacheLease::new(self.clone(), path, true),
        }))
    }

    pub(crate) fn reserve(&self, song_id: u64, extension: &str) -> io::Result<CacheReservation> {
        let extension = sanitize_extension(extension);
        let file_name = format!("{song_id}.{extension}.cache");
        let path = self.data_dir.join(&file_name);
        let mut inner = self.lock();
        if inner.active.contains_key(&path) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cache entry is already active",
            ));
        }
        if let Some(previous) = inner.state.entries.remove(&song_id) {
            let previous_path = self.data_dir.join(previous.file_name);
            if previous_path != path && !inner.active.contains_key(&previous_path) {
                let _ = fs::remove_file(previous_path);
            }
        }
        inner.active.insert(path.clone(), 1);
        self.persist_locked(&inner)?;
        let completion_state = Arc::new(AtomicU8::new(COMPLETION_PENDING));
        Ok(CacheReservation {
            provider: PersistentStorageProvider { path: path.clone() },
            lease: CacheLease {
                cache: self.clone(),
                path: path.clone(),
                completion_state: completion_state.clone(),
            },
            completion: CacheCompletion {
                cache: self.clone(),
                song_id,
                path,
                file_name,
                extension,
                completion_state,
            },
        })
    }

    pub async fn set_max_bytes(&self, max_bytes: u64) -> io::Result<CacheStats> {
        let cache = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut inner = cache.lock();
            inner.state.max_bytes = max_bytes;
            cache.trim_locked(&mut inner);
            cache.persist_locked(&inner)?;
            Ok(stats_locked(&inner))
        })
        .await
        .map_err(join_error)?
    }

    pub async fn clear(&self) -> io::Result<ClearReport> {
        let cache = self.clone();
        tokio::task::spawn_blocking(move || cache.clear_sync())
            .await
            .map_err(join_error)?
    }

    fn clear_sync(&self) -> io::Result<ClearReport> {
        let mut inner = self.lock();
        let mut removed_bytes = 0_u64;
        let retained_active = inner.active.len();
        let ids = inner.state.entries.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let Some(entry) = inner.state.entries.get(&id).cloned() else {
                continue;
            };
            let path = self.data_dir.join(&entry.file_name);
            if inner.active.contains_key(&path) {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed_bytes = removed_bytes.saturating_add(entry.size),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            inner.state.entries.remove(&id);
        }
        for entry in fs::read_dir(self.data_dir.as_ref())? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file() || inner.active.contains_key(&path) {
                continue;
            }
            removed_bytes = removed_bytes.saturating_add(entry.metadata()?.len());
            fs::remove_file(path)?;
        }
        self.persist_locked(&inner)?;
        Ok(ClearReport {
            removed_bytes,
            retained_active,
            stats: stats_locked(&inner),
        })
    }

    fn finish_sync(&self, completion: &CacheCompletion) -> io::Result<()> {
        let mut inner = self.lock();
        let metadata = fs::metadata(&completion.path)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "completed cache file is empty or invalid",
            ));
        }
        let last_accessed = next_access_stamp(&mut inner);
        let previous = inner.state.entries.insert(
            completion.song_id,
            CacheEntry {
                file_name: completion.file_name.clone(),
                extension: completion.extension.clone(),
                size: metadata.len(),
                last_accessed,
            },
        );
        if let Some(previous) = previous {
            let previous_path = self.data_dir.join(previous.file_name);
            if previous_path != completion.path && !inner.active.contains_key(&previous_path) {
                let _ = fs::remove_file(previous_path);
            }
        }
        self.trim_locked(&mut inner);
        self.persist_locked(&inner)
    }

    fn remove_if_inactive_and_unretained(&self, path: &Path) {
        let inner = self.lock();
        let retained = inner
            .state
            .entries
            .values()
            .any(|entry| self.data_dir.join(&entry.file_name) == *path);
        if !inner.active.contains_key(path) && !retained {
            let _ = fs::remove_file(path);
        }
    }

    fn trim_locked(&self, inner: &mut CacheInner) {
        let mut total = inner
            .state
            .entries
            .values()
            .map(|entry| entry.size)
            .sum::<u64>();
        if total <= inner.state.max_bytes {
            return;
        }
        let mut candidates = inner
            .state
            .entries
            .iter()
            .map(|(id, entry)| (*id, entry.clone()))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, entry)| entry.last_accessed);
        for (id, entry) in candidates {
            if total <= inner.state.max_bytes {
                break;
            }
            let path = self.data_dir.join(&entry.file_name);
            if inner.active.contains_key(&path) {
                continue;
            }
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => continue,
            }
            inner.state.entries.remove(&id);
            total = total.saturating_sub(entry.size);
        }
    }

    fn persist_locked(&self, inner: &CacheInner) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&inner.state).map_err(io::Error::other)?;
        let temporary = self.state_path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        if let Err(error) = fs::rename(&temporary, self.state_path.as_ref()) {
            if self.state_path.exists() {
                fs::remove_file(self.state_path.as_ref())?;
                fs::rename(temporary, self.state_path.as_ref())?;
            } else {
                return Err(error);
            }
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheInner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub struct CacheHit {
    pub(crate) file: File,
    pub(crate) extension: String,
    pub(crate) size: u64,
    pub(crate) _lease: CacheLease,
}

pub(crate) struct CacheReservation {
    pub provider: PersistentStorageProvider,
    pub lease: CacheLease,
    pub completion: CacheCompletion,
}

pub(crate) struct CacheCompletion {
    cache: PlaybackCache,
    song_id: u64,
    path: PathBuf,
    file_name: String,
    extension: String,
    completion_state: Arc<AtomicU8>,
}

impl CacheCompletion {
    pub fn finish(&self) {
        if self
            .completion_state
            .compare_exchange(
                COMPLETION_PENDING,
                COMPLETION_FINISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        let succeeded = self.cache.finish_sync(self).is_ok();
        let state = if succeeded {
            COMPLETION_COMPLETE
        } else {
            COMPLETION_FAILED
        };
        self.completion_state.store(state, Ordering::Release);
        if !succeeded {
            self.cache.remove_if_inactive_and_unretained(&self.path);
        }
    }
}

impl Clone for CacheCompletion {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            song_id: self.song_id,
            path: self.path.clone(),
            file_name: self.file_name.clone(),
            extension: self.extension.clone(),
            completion_state: self.completion_state.clone(),
        }
    }
}

pub(crate) struct CacheLease {
    cache: PlaybackCache,
    path: PathBuf,
    completion_state: Arc<AtomicU8>,
}

impl CacheLease {
    fn new(cache: PlaybackCache, path: PathBuf, completed: bool) -> Self {
        Self {
            cache,
            path,
            completion_state: Arc::new(AtomicU8::new(if completed {
                COMPLETION_COMPLETE
            } else {
                COMPLETION_PENDING
            })),
        }
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        let _ = self.completion_state.compare_exchange(
            COMPLETION_PENDING,
            COMPLETION_FAILED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let mut inner = self.cache.lock();
        if let Some(count) = inner.active.get_mut(&self.path) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.active.remove(&self.path);
            }
        }
        let retained = inner
            .state
            .entries
            .values()
            .any(|entry| self.cache.data_dir.join(&entry.file_name) == self.path);
        self.cache.trim_locked(&mut inner);
        let _ = self.cache.persist_locked(&inner);
        drop(inner);
        let state = self.completion_state.load(Ordering::Acquire);
        if state != COMPLETION_FINISHING && !retained {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PersistentStorageProvider {
    path: PathBuf,
}

impl StorageProvider for PersistentStorageProvider {
    type Reader = File;
    type Writer = File;

    fn into_reader_writer(
        self,
        _content_length: Option<u64>,
    ) -> io::Result<(Self::Reader, Self::Writer)> {
        let writer = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&self.path)?;
        let reader = OpenOptions::new().read(true).open(self.path)?;
        Ok((reader, writer))
    }
}

fn stats_locked(inner: &CacheInner) -> CacheStats {
    CacheStats {
        used_bytes: inner.state.entries.values().map(|entry| entry.size).sum(),
        max_bytes: inner.state.max_bytes,
        entries: inner.state.entries.len(),
    }
}

fn sanitize_extension(extension: &str) -> String {
    let value = extension
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    if value.is_empty() {
        "audio".into()
    } else {
        value
    }
}

fn next_access_stamp(inner: &mut CacheInner) -> u64 {
    inner.state.access_clock = inner.state.access_clock.saturating_add(1);
    inner.state.access_clock
}

fn join_error(error: tokio::task::JoinError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn complete_entry(cache: &PlaybackCache, id: u64, bytes: &[u8]) {
        let reservation = cache.reserve(id, "flac").unwrap();
        let (_, mut writer) = reservation.provider.into_reader_writer(None).unwrap();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
        reservation.completion.finish();
        drop(writer);
        drop(reservation.lease);
    }

    #[tokio::test]
    async fn completed_entries_survive_reopening_and_incomplete_entries_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let cache = PlaybackCache::open(directory.path(), 1024).await.unwrap();
        complete_entry(&cache, 7, b"cached audio");
        let incomplete = cache.reserve(8, "mp3").unwrap();
        let (_, mut writer) = incomplete.provider.into_reader_writer(None).unwrap();
        writer.write_all(b"partial").unwrap();
        drop(writer);
        drop(incomplete.lease);
        drop(cache);

        let reopened = PlaybackCache::open(directory.path(), 2048).await.unwrap();
        assert!(reopened.lookup(7).await.unwrap().is_some());
        assert!(reopened.lookup(8).await.unwrap().is_none());
        assert_eq!(reopened.stats().max_bytes, 1024);
    }

    #[tokio::test]
    async fn a_late_completion_cannot_restore_an_abandoned_download() {
        let directory = tempfile::tempdir().unwrap();
        let cache = PlaybackCache::open(directory.path(), 1024).await.unwrap();
        let reservation = cache.reserve(9, "mp3").unwrap();
        let path = reservation.provider.path.clone();
        let (_, mut writer) = reservation.provider.into_reader_writer(None).unwrap();
        writer.write_all(b"partial").unwrap();
        writer.flush().unwrap();
        drop(writer);

        drop(reservation.lease);
        reservation.completion.finish();

        assert!(!path.exists());
        assert!(cache.lookup(9).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn capacity_is_persistent_and_evicts_oldest_entries() {
        let directory = tempfile::tempdir().unwrap();
        let cache = PlaybackCache::open(directory.path(), 20).await.unwrap();
        complete_entry(&cache, 1, b"1234567890");
        complete_entry(&cache, 2, b"abcdefghij");
        let stats = cache.set_max_bytes(10).await.unwrap();
        assert_eq!(stats.used_bytes, 10);
        assert_eq!(stats.entries, 1);
        assert!(cache.lookup(1).await.unwrap().is_none());
        assert!(cache.lookup(2).await.unwrap().is_some());

        drop(cache);
        let reopened = PlaybackCache::open(directory.path(), 99).await.unwrap();
        assert_eq!(reopened.stats().max_bytes, 10);
    }

    #[tokio::test]
    async fn accessing_an_entry_refreshes_its_lru_position() {
        let directory = tempfile::tempdir().unwrap();
        let cache = PlaybackCache::open(directory.path(), 30).await.unwrap();
        complete_entry(&cache, 1, b"1234567890");
        complete_entry(&cache, 2, b"abcdefghij");
        drop(cache.lookup(1).await.unwrap().unwrap());
        complete_entry(&cache, 3, b"0987654321");

        cache.set_max_bytes(20).await.unwrap();

        assert!(cache.lookup(1).await.unwrap().is_some());
        assert!(cache.lookup(2).await.unwrap().is_none());
        assert!(cache.lookup(3).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn clear_skips_an_active_entry() {
        let directory = tempfile::tempdir().unwrap();
        let cache = PlaybackCache::open(directory.path(), 1024).await.unwrap();
        complete_entry(&cache, 1, b"first");
        complete_entry(&cache, 2, b"second");
        let hit = cache.lookup(1).await.unwrap().unwrap();

        let report = cache.clear().await.unwrap();
        assert_eq!(report.retained_active, 1);
        assert_eq!(report.stats.entries, 1);
        drop(hit);
        assert_eq!(cache.clear().await.unwrap().stats.entries, 0);
    }
}
