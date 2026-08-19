//! Fine-grained download orchestration. Persistence stays internal.

use std::{
    collections::{HashMap, HashSet},
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::{StreamExt, stream};
use lofty::{
    config::WriteOptions,
    picture::{Picture, PictureType},
    prelude::{Accessor, TagExt},
    tag::{ItemKey, Tag, TagType},
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    database::{DatabaseError, DownloadedTrack, LibraryDb},
    library::LibraryError,
    ncm_core::{NcmClient, NcmError, api},
    organizer::{OrganizableTrack, destination, safe_extension},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DownloadSource {
    Track(u64),
    Tracks(Vec<u64>),
    Playlist(u64),
    Album(u64),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TrackSelection {
    #[default]
    All,
    /// One-based positions within a playlist or album.
    Positions(RangeInclusive<usize>),
    /// Intersects these IDs with the source while preserving source order.
    TrackIds(HashSet<u64>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioQuality {
    Standard,
    Higher,
    ExHigh,
    #[default]
    Lossless,
    HiRes,
    Jyeffect,
    Sky,
    Dolby,
    Jymaster,
}

impl AudioQuality {
    pub(crate) fn api_level(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Higher => "higher",
            Self::ExHigh => "exhigh",
            Self::Lossless => "lossless",
            Self::HiRes => "hires",
            Self::Jyeffect => "jyeffect",
            Self::Sky => "sky",
            Self::Dolby => "dolby",
            Self::Jymaster => "jymaster",
        }
    }

    pub(crate) fn encode_type(self) -> &'static str {
        match self {
            Self::Standard | Self::Higher | Self::ExHigh => "mp3",
            _ => "flac",
        }
    }

    pub(crate) fn fallback_chain(self) -> &'static [Self] {
        match self {
            Self::Standard => &[Self::Standard],
            Self::Higher => &[Self::Higher, Self::Standard],
            Self::ExHigh => &[Self::ExHigh, Self::Higher, Self::Standard],
            Self::Lossless => &[Self::Lossless, Self::ExHigh, Self::Higher, Self::Standard],
            Self::HiRes => &[
                Self::HiRes,
                Self::Lossless,
                Self::ExHigh,
                Self::Higher,
                Self::Standard,
            ],
            Self::Jyeffect => &[
                Self::Jyeffect,
                Self::HiRes,
                Self::Lossless,
                Self::ExHigh,
                Self::Higher,
                Self::Standard,
            ],
            Self::Sky => &[
                Self::Sky,
                Self::HiRes,
                Self::Lossless,
                Self::ExHigh,
                Self::Higher,
                Self::Standard,
            ],
            Self::Dolby => &[
                Self::Dolby,
                Self::HiRes,
                Self::Lossless,
                Self::ExHigh,
                Self::Higher,
                Self::Standard,
            ],
            Self::Jymaster => &[
                Self::Jymaster,
                Self::HiRes,
                Self::Lossless,
                Self::ExHigh,
                Self::Higher,
                Self::Standard,
            ],
        }
    }

    fn from_api_level(value: &str) -> Option<Self> {
        Some(match value {
            "standard" => Self::Standard,
            "higher" => Self::Higher,
            "exhigh" => Self::ExHigh,
            "lossless" => Self::Lossless,
            "hires" => Self::HiRes,
            "jyeffect" => Self::Jyeffect,
            "sky" => Self::Sky,
            "dolby" => Self::Dolby,
            "jymaster" => Self::Jymaster,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub source: DownloadSource,
    pub selection: TrackSelection,
    pub quality: AudioQuality,
    pub overwrite: bool,
}

impl DownloadRequest {
    pub fn track(id: u64) -> Self {
        Self {
            source: DownloadSource::Track(id),
            selection: TrackSelection::All,
            quality: AudioQuality::default(),
            overwrite: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DownloadReport {
    pub requested: usize,
    pub downloaded: Vec<DownloadedFile>,
    pub skipped_existing: Vec<u64>,
    pub unavailable: Vec<u64>,
    pub warnings: Vec<DownloadWarning>,
}

#[derive(Clone, Debug)]
pub struct DownloadedFile {
    pub track_id: u64,
    pub title: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub requested_quality: AudioQuality,
    pub actual_quality: AudioQuality,
    pub format: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadWarningStage {
    Lyrics,
    Cover,
    Tagging,
    Sidecar,
}

#[derive(Clone, Debug)]
pub struct DownloadWarning {
    pub track_id: u64,
    pub stage: DownloadWarningStage,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Api(#[from] NcmError),
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error("NCM response is missing {0}")]
    InvalidResponse(&'static str),
}

pub type Result<T> = std::result::Result<T, DownloadError>;

impl From<DatabaseError> for DownloadError {
    fn from(error: DatabaseError) -> Self {
        Self::Library(error.into())
    }
}

#[derive(Clone)]
pub struct Downloader {
    client: NcmClient,
    library: LibraryDb,
    root: PathBuf,
    concurrency: usize,
    cover_cache: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
}

impl Downloader {
    /// Opens all internal state below `download_root`; no DB path enters the UI.
    pub fn new(
        client: NcmClient,
        download_root: impl AsRef<Path>,
        concurrency: usize,
    ) -> Result<Self> {
        let root = download_root.as_ref().to_path_buf();
        let library = LibraryDb::open(&root)?;
        Ok(Self {
            client,
            library,
            root,
            concurrency: concurrency.max(1),
            cover_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn download(&self, request: DownloadRequest) -> Result<DownloadReport> {
        let discovered = self.discover(&request.source).await?;
        let positions = discovered
            .track_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, (index + 1) as u32))
            .collect::<HashMap<_, _>>();
        let selected = apply_selection(discovered.track_ids, &request.selection);
        let requested = selected.len();
        let existing = if request.overwrite {
            HashSet::new()
        } else {
            self.library.available_ids(&selected)?
        };
        let pending = selected
            .iter()
            .copied()
            .filter(|id| !existing.contains(id))
            .collect::<Vec<_>>();
        if let Some(collection) = discovered.collection.as_ref() {
            let memberships = selected
                .iter()
                .filter(|id| existing.contains(id))
                .filter_map(|id| positions.get(id).map(|position| (*id, *position)))
                .collect::<Vec<_>>();
            self.library.record_collection_membership(
                &collection.kind,
                collection.id,
                &collection.name,
                &memberships,
            )?;
        }
        let metadata = self.track_metadata(&pending).await?;
        let audio = self.audio_urls(&pending, request.quality).await?;
        let collection = discovered.collection;

        let download_ids = pending.clone();
        let results = stream::iter(download_ids.into_iter().enumerate().map(|(index, id)| {
            let metadata = metadata.get(&id).cloned();
            let audio = audio.get(&id).cloned();
            let root = self.root.clone();
            let library = self.library.clone();
            let client = self.client.clone();
            let collection = collection.clone();
            let cover_cache = self.cover_cache.clone();
            let requested_quality = request.quality;
            let position = positions.get(&id).copied().unwrap_or((index + 1) as u32);
            async move {
                let (metadata, audio) = match (metadata, audio) {
                    (Some(metadata), Some(audio)) if !audio.url.is_empty() => (metadata, audio),
                    _ => return Ok(None),
                };
                let extension = safe_extension(&audio.format).to_owned();
                let path = destination(
                    &root,
                    &OrganizableTrack {
                        id: metadata.id,
                        title: metadata.title.clone(),
                        artists: metadata.artists.clone(),
                        album: metadata.album.clone(),
                        album_artist: metadata.album_artist.clone(),
                        track_number: metadata.track_number,
                        disc_number: metadata.disc_number,
                        total_tracks: metadata.total_tracks,
                        format: extension.clone(),
                        path: PathBuf::new(),
                    },
                );
                client.download_to(&audio.url, &path).await?;
                let mut warnings = Vec::new();
                let (lyrics, lyric_warning) = fetch_lyrics(&client, metadata.id).await;
                if let Some(message) = lyric_warning {
                    warnings.push(DownloadWarning {
                        track_id: metadata.id,
                        stage: DownloadWarningStage::Lyrics,
                        message,
                    });
                }
                let (cover, cover_warning) =
                    fetch_cover(&client, &cover_cache, &metadata.cover_url).await;
                if let Some(message) = cover_warning {
                    warnings.push(DownloadWarning {
                        track_id: metadata.id,
                        stage: DownloadWarningStage::Cover,
                        message,
                    });
                }
                let tag_path = path.clone();
                let tag_metadata = metadata.clone();
                let tag_lyrics = lyrics.clone();
                let tag_cover = cover.clone();
                let tag_extension = extension.clone();
                if let Err(message) = tokio::task::spawn_blocking(move || {
                    write_tags(
                        &tag_path,
                        &tag_extension,
                        &tag_metadata,
                        tag_lyrics.as_deref(),
                        tag_cover.as_deref(),
                    )
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result)
                {
                    warnings.push(DownloadWarning {
                        track_id: metadata.id,
                        stage: DownloadWarningStage::Tagging,
                        message,
                    });
                }
                if let Some(lyrics) = lyrics.as_deref()
                    && let Err(error) = write_lyrics_sidecar(&path, lyrics).await
                {
                    warnings.push(DownloadWarning {
                        track_id: metadata.id,
                        stage: DownloadWarningStage::Sidecar,
                        message: error.to_string(),
                    });
                }
                let bytes = tokio::fs::metadata(&path).await?.len();
                library.record_download(&DownloadedTrack {
                    ncm_id: metadata.id,
                    title: metadata.title.clone(),
                    artists: metadata.artists.clone(),
                    album_id: metadata.album_id,
                    album: metadata.album,
                    duration_ms: metadata.duration_ms,
                    album_artist: metadata.album_artist,
                    year: metadata.year,
                    track_number: metadata.track_number,
                    disc_number: metadata.disc_number,
                    total_tracks: metadata.total_tracks,
                    cover_url: metadata.cover_url,
                    file_path: path.clone(),
                    file_size: bytes,
                    format: extension.to_owned(),
                    bitrate: audio.bitrate,
                    audio_md5: audio.md5,
                    collection_id: collection.as_ref().map(|value| value.id),
                    collection_kind: collection
                        .as_ref()
                        .map_or_else(String::new, |value| value.kind.clone()),
                    collection_name: collection
                        .as_ref()
                        .map_or_else(String::new, |value| value.name.clone()),
                    collection_position: position,
                })?;
                Ok::<_, DownloadError>(Some((
                    DownloadedFile {
                        track_id: metadata.id,
                        title: metadata.title,
                        path,
                        bytes,
                        requested_quality,
                        actual_quality: audio.quality,
                        format: extension.to_owned(),
                    },
                    warnings,
                )))
            }
        }))
        .buffer_unordered(self.concurrency)
        .collect::<Vec<_>>()
        .await;

        let mut report = DownloadReport {
            requested,
            skipped_existing: selected
                .iter()
                .copied()
                .filter(|id| existing.contains(id))
                .collect(),
            ..DownloadReport::default()
        };
        for result in results {
            if let Some((file, warnings)) = result? {
                report.downloaded.push(file);
                report.warnings.extend(warnings);
            }
        }
        let downloaded = report
            .downloaded
            .iter()
            .map(|file| file.track_id)
            .collect::<HashSet<_>>();
        report.unavailable = pending
            .into_iter()
            .filter(|id| !downloaded.contains(id))
            .collect();
        report.downloaded.sort_by_key(|file| file.track_id);
        report.warnings.sort_by_key(|warning| warning.track_id);
        Ok(report)
    }

    async fn discover(&self, source: &DownloadSource) -> Result<Discovered> {
        match source {
            DownloadSource::Track(id) => Ok(Discovered::tracks(vec![*id])),
            DownloadSource::Tracks(ids) => Ok(Discovered::tracks(deduplicate(ids))),
            DownloadSource::Playlist(id) => {
                let value = self.client.execute(api::playlist::detail(*id)).await?;
                let playlist = value
                    .get("playlist")
                    .ok_or(DownloadError::InvalidResponse("playlist"))?;
                Ok(Discovered {
                    track_ids: ids_at(playlist.get("trackIds"), "id"),
                    collection: Some(Collection {
                        kind: "playlist".to_owned(),
                        id: *id,
                        name: string_at(playlist, "name"),
                    }),
                })
            }
            DownloadSource::Album(id) => {
                let value = self.client.execute(api::album::detail(*id)).await?;
                let album = value
                    .get("album")
                    .ok_or(DownloadError::InvalidResponse("album"))?;
                Ok(Discovered {
                    track_ids: ids_at(value.get("songs"), "id"),
                    collection: Some(Collection {
                        kind: "album".to_owned(),
                        id: *id,
                        name: string_at(album, "name"),
                    }),
                })
            }
        }
    }

    async fn track_metadata(&self, ids: &[u64]) -> Result<HashMap<u64, TrackMetadata>> {
        let mut result = HashMap::new();
        for chunk in ids.chunks(500) {
            let value = self.client.execute(api::track::detail(chunk)?).await?;
            for song in value
                .get("songs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(track) = TrackMetadata::parse(song) {
                    result.insert(track.id, track);
                }
            }
        }
        Ok(result)
    }

    async fn audio_urls(
        &self,
        ids: &[u64],
        quality: AudioQuality,
    ) -> Result<HashMap<u64, AudioFile>> {
        let mut result = HashMap::new();
        let mut unresolved = ids.iter().copied().collect::<HashSet<_>>();
        for &fallback in quality.fallback_chain() {
            let pending = ids
                .iter()
                .copied()
                .filter(|id| unresolved.contains(id))
                .collect::<Vec<_>>();
            for chunk in pending.chunks(500) {
                let value = self
                    .client
                    .execute(api::track::audio_v1(
                        chunk,
                        fallback.api_level(),
                        fallback.encode_type(),
                    ))
                    .await?;
                for item in value
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(audio) = AudioFile::parse(item, fallback)
                        && !audio.url.is_empty()
                        && unresolved.remove(&audio.id)
                    {
                        result.insert(audio.id, audio);
                    }
                }
            }
            if unresolved.is_empty() {
                break;
            }
        }
        Ok(result)
    }
}

#[derive(Clone, Debug)]
struct Discovered {
    track_ids: Vec<u64>,
    collection: Option<Collection>,
}

impl Discovered {
    fn tracks(ids: Vec<u64>) -> Self {
        Self {
            track_ids: ids,
            collection: None,
        }
    }
}

#[derive(Clone, Debug)]
struct Collection {
    kind: String,
    id: u64,
    name: String,
}

#[derive(Clone, Debug)]
struct TrackMetadata {
    id: u64,
    title: String,
    artists: String,
    album_id: Option<u64>,
    album: String,
    duration_ms: u64,
    cover_url: String,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    total_tracks: Option<u32>,
    album_artist: String,
    year: Option<i32>,
}

impl TrackMetadata {
    fn parse(value: &Value) -> Option<Self> {
        let album = value.get("al").or_else(|| value.get("album"));
        let artists = value
            .get("ar")
            .or_else(|| value.get("artists"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|artist| artist.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" / ");
        let album_artist = album
            .map(album_artists)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| artists.clone());
        Some(Self {
            id: value.get("id")?.as_u64()?,
            title: string_at(value, "name"),
            artists,
            album_id: album
                .and_then(|item| item.get("id"))
                .and_then(Value::as_u64),
            album: album.map_or_else(String::new, |item| string_at(item, "name")),
            duration_ms: value
                .get("dt")
                .or_else(|| value.get("duration"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cover_url: album.map_or_else(String::new, |item| string_at(item, "picUrl")),
            track_number: u32_at(value, "no"),
            disc_number: value.get("cd").and_then(parse_disc_number),
            total_tracks: album.and_then(|item| u32_at(item, "size")),
            album_artist,
            year: value
                .get("publishTime")
                .and_then(Value::as_i64)
                .and_then(year_from_unix_millis),
        })
    }
}

#[derive(Clone, Debug)]
struct AudioFile {
    id: u64,
    url: String,
    format: String,
    bitrate: u64,
    md5: String,
    quality: AudioQuality,
}

impl AudioFile {
    fn parse(value: &Value, requested: AudioQuality) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()?,
            url: string_at(value, "url"),
            format: string_at(value, "type"),
            bitrate: value.get("br").and_then(Value::as_u64).unwrap_or_default(),
            md5: string_at(value, "md5"),
            quality: value
                .get("level")
                .and_then(Value::as_str)
                .and_then(AudioQuality::from_api_level)
                .unwrap_or(requested),
        })
    }
}

async fn fetch_lyrics(client: &NcmClient, track_id: u64) -> (Option<String>, Option<String>) {
    let first = client.execute(api::track::lyrics_v1(track_id)).await;
    match first {
        Ok(value) if lyrics_at(&value).is_some() => (lyrics_at(&value), None),
        first => match client.execute(api::track::lyrics(track_id)).await {
            Ok(value) => (lyrics_at(&value), None),
            Err(second_error) => {
                let first_error = match first {
                    Ok(_) => "new lyrics endpoint returned no lyrics".to_owned(),
                    Err(error) => format!("new lyrics endpoint failed: {error}"),
                };
                (
                    None,
                    Some(format!("{first_error}; fallback failed: {second_error}")),
                )
            }
        },
    }
}

fn lyrics_at(value: &Value) -> Option<String> {
    let lyrics = value.get("lrc")?.get("lyric")?.as_str()?;
    let cleaned = clean_lyrics(lyrics);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn clean_lyrics(value: &str) -> String {
    value
        .lines()
        .filter(|line| {
            let line = line.trim();
            !(line.starts_with('{') && line.ends_with('}'))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

async fn fetch_cover(
    client: &NcmClient,
    cache: &Mutex<HashMap<String, Arc<Vec<u8>>>>,
    url: &str,
) -> (Option<Arc<Vec<u8>>>, Option<String>) {
    if url.is_empty() {
        return (None, None);
    }
    if let Some(cover) = cache.lock().await.get(url).cloned() {
        return (Some(cover), None);
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    match client
        .get_bytes(&format!("{url}{separator}param=1000y1000"))
        .await
    {
        Ok(bytes) => {
            let bytes = Arc::new(bytes);
            cache.lock().await.insert(url.to_owned(), bytes.clone());
            (Some(bytes), None)
        }
        Err(error) => (None, Some(error.to_string())),
    }
}

fn write_tags(
    path: &Path,
    extension: &str,
    metadata: &TrackMetadata,
    lyrics: Option<&str>,
    cover: Option<&Vec<u8>>,
) -> std::result::Result<(), String> {
    let tag_type = match extension {
        "mp3" => TagType::Id3v2,
        "flac" | "ogg" => TagType::VorbisComments,
        "m4a" => TagType::Mp4Ilst,
        _ => return Err(format!("unsupported tag format: {extension}")),
    };
    let mut tag = Tag::new(tag_type);
    tag.set_title(metadata.title.clone());
    tag.set_artist(metadata.artists.clone());
    tag.set_album(metadata.album.clone());
    if let Some(track) = metadata.track_number {
        tag.set_track(track);
    }
    if let Some(total) = metadata.total_tracks {
        tag.set_track_total(total);
    }
    if let Some(disc) = metadata.disc_number {
        tag.set_disk(disc);
    }
    if !metadata.album_artist.is_empty() {
        tag.insert_text(ItemKey::AlbumArtist, metadata.album_artist.clone());
    }
    if let Some(year) = metadata.year {
        tag.insert_text(ItemKey::RecordingDate, year.to_string());
    }
    if let Some(lyrics) = lyrics {
        tag.insert_text(ItemKey::UnsyncLyrics, lyrics.to_owned());
    }
    if let Some(cover) = cover {
        let mut reader = cover.as_slice();
        let mut picture = Picture::from_reader(&mut reader).map_err(|error| error.to_string())?;
        picture.set_pic_type(PictureType::CoverFront);
        tag.push_picture(picture);
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|error| error.to_string())
}

async fn write_lyrics_sidecar(path: &Path, lyrics: &str) -> std::io::Result<()> {
    let mut body = lyrics.to_owned();
    body.push('\n');
    tokio::fs::write(path.with_extension("lrc"), body).await
}

fn apply_selection(ids: Vec<u64>, selection: &TrackSelection) -> Vec<u64> {
    let selected = match selection {
        TrackSelection::All => ids,
        TrackSelection::Positions(range) => ids
            .into_iter()
            .enumerate()
            .filter(|(index, _)| range.contains(&(index + 1)))
            .map(|(_, id)| id)
            .collect(),
        TrackSelection::TrackIds(wanted) => {
            ids.into_iter().filter(|id| wanted.contains(id)).collect()
        }
    };
    deduplicate(&selected)
}

fn deduplicate(ids: &[u64]) -> Vec<u64> {
    let mut seen = HashSet::new();
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

fn ids_at(value: Option<&Value>, key: &str) -> Vec<u64> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(key).and_then(Value::as_u64))
        .collect()
}

fn string_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn u32_at(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn parse_disc_number(value: &Value) -> Option<u32> {
    if let Some(value) = value.as_u64() {
        return u32::try_from(value).ok();
    }
    value
        .as_str()?
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn album_artists(album: &Value) -> String {
    album
        .get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|artist| artist.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn year_from_unix_millis(millis: i64) -> Option<i32> {
    let days = millis.div_euclid(86_400_000);
    let shifted = days.checked_add(719_468)?;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    i32::try_from(year + i64::from(month <= 2)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_one_based_and_deduplicated() {
        assert_eq!(
            apply_selection(vec![10, 20, 20, 30], &TrackSelection::Positions(2..=4)),
            vec![20, 30]
        );
    }

    #[test]
    fn id_selection_preserves_collection_order() {
        let wanted = HashSet::from([30, 10]);
        assert_eq!(
            apply_selection(vec![10, 20, 30], &TrackSelection::TrackIds(wanted)),
            vec![10, 30]
        );
    }

    #[test]
    fn formats_are_normalized() {
        assert_eq!(safe_extension("FLAC"), "flac");
    }

    #[test]
    fn parses_disc_numbers_and_publish_years() {
        assert_eq!(parse_disc_number(&Value::String("2/3".to_owned())), Some(2));
        assert_eq!(parse_disc_number(&Value::String("CD 2".to_owned())), None);
        assert_eq!(year_from_unix_millis(0), Some(1970));
        assert_eq!(year_from_unix_millis(946_684_800_000), Some(2000));
        assert_eq!(year_from_unix_millis(1_704_067_200_000), Some(2024));
    }

    #[test]
    fn quality_fallback_is_batched_and_ordered() {
        assert_eq!(
            AudioQuality::Lossless.fallback_chain(),
            &[
                AudioQuality::Lossless,
                AudioQuality::ExHigh,
                AudioQuality::Higher,
                AudioQuality::Standard
            ]
        );
    }
}
