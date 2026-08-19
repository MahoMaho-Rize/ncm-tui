//! Seekable HTTP playback streams backed by a persistent, size-bounded cache.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

use stream_download::{
    Settings, StreamDownload, StreamPhase,
    http::{HttpStream, reqwest},
};
use symphonia::core::io::MediaSource;

use crate::playback_cache::{CacheHit, CacheLease, PersistentStorageProvider, PlaybackCache};

const PREFETCH_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackSource {
    pub url: String,
    pub format: String,
    pub bitrate: u64,
    pub size: u64,
}

pub struct PreparedStream {
    source: Box<dyn MediaSource>,
    extension: String,
}

impl PreparedStream {
    pub async fn cached(song_id: u64, cache: PlaybackCache) -> Result<Option<Self>, String> {
        cache
            .lookup(song_id)
            .await
            .map_err(|error| error.to_string())
            .map(|hit| hit.map(Self::from_cache_hit))
    }

    pub async fn open(
        song_id: u64,
        source: PlaybackSource,
        cache: PlaybackCache,
    ) -> Result<Self, String> {
        let reservation = cache
            .reserve(song_id, &source.format)
            .map_err(|error| error.to_string())?;
        let completion = reservation.completion.clone();
        let url = source
            .url
            .parse()
            .map_err(|error| format!("无效音源地址：{error}"))?;
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 NCM-TUI/0.1")
            .build()
            .map_err(|error| error.to_string())?;
        let stream = HttpStream::new(client, url)
            .await
            .map_err(|error| error.to_string())?;
        let reader = StreamDownload::from_stream(
            stream,
            reservation.provider,
            Settings::default()
                .prefetch_bytes(PREFETCH_BYTES)
                .on_progress(move |_, state, _| {
                    if state.phase == StreamPhase::Complete {
                        completion.finish();
                    }
                }),
        )
        .await
        .map_err(|error| error.to_string())?;
        let content_length = reader
            .content_length()
            .or((source.size > 0).then_some(source.size));
        Ok(Self {
            source: Box::new(StreamingMediaSource {
                reader,
                content_length,
                _lease: reservation.lease,
            }),
            extension: source.format,
        })
    }

    fn from_cache_hit(hit: CacheHit) -> Self {
        let CacheHit {
            file,
            extension,
            size,
            _lease,
        } = hit;
        Self {
            source: Box::new(CachedMediaSource { file, size, _lease }),
            extension,
        }
    }

    pub(crate) fn into_parts(self) -> (Box<dyn MediaSource>, String) {
        (self.source, self.extension)
    }
}

struct StreamingMediaSource {
    reader: StreamDownload<PersistentStorageProvider>,
    content_length: Option<u64>,
    _lease: CacheLease,
}

impl Read for StreamingMediaSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

struct CachedMediaSource {
    file: File,
    size: u64,
    _lease: CacheLease,
}

impl Read for CachedMediaSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for CachedMediaSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl MediaSource for CachedMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.size)
    }
}

impl Seek for StreamingMediaSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.reader.seek(position)
    }
}

impl MediaSource for StreamingMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.content_length
    }
}
