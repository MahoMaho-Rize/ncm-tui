//! Online catalog discovery. Callers receive domain values, never raw API JSON.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, OnceCell};

use crate::download::AudioQuality;
use crate::lyrics::{LyricDocument, Lyrics, LyricsCache};
use crate::ncm_core::{
    NcmClient, NcmError,
    api::{self, search::SearchType, user::ListeningRank},
};
use crate::pagination::PaginationInfo;
use crate::streaming::PlaybackSource;

const SEARCH_SONG_PAGE_SIZE: usize = 100;
const SEARCH_ENTITY_PAGE_SIZE: usize = 50;
const COLLECTION_PAGE_SIZE: usize = 50;
const ARTIST_ALBUM_PAGE_SIZE: usize = 100;
const ARTIST_SONG_PAGE_SIZE: usize = 1_000;
const MAX_PAGES: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchKind {
    Song,
    Album,
    Artist,
    Playlist,
}

impl From<SearchKind> for SearchType {
    fn from(value: SearchKind) -> Self {
        match value {
            SearchKind::Song => Self::Song,
            SearchKind::Album => Self::Album,
            SearchKind::Artist => Self::Artist,
            SearchKind::Playlist => Self::Playlist,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryResult {
    pub name: String,
    pub track_ids: Vec<u64>,
    /// Result count reported by NCM, before an optional local limit is applied.
    pub total_found: u64,
    /// Number of songs/albums/artists/playlists actually matched and expanded.
    pub matched_items: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistSummary {
    pub id: u64,
    pub name: String,
    pub track_count: u64,
    pub created_by_user: bool,
    pub special_type: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionPage<T> {
    pub items: Vec<T>,
    pub pagination: PaginationInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistScope {
    All,
    Created,
    Subscribed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlbumSummary {
    pub id: u64,
    pub name: String,
    pub artists: String,
    pub track_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtistSummary {
    pub id: u64,
    pub name: String,
    pub album_count: u64,
    pub music_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankedTrack {
    pub track: OnlineTrack,
    pub play_count: u64,
    pub score: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnlineTrack {
    pub id: u64,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u64,
    pub cover_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackPage {
    pub title: String,
    pub items: Vec<OnlineTrack>,
    pub pagination: PaginationInfo,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchPage {
    pub title: String,
    pub kind: SearchKind,
    pub pagination: PaginationInfo,
    pub tracks: Vec<OnlineTrack>,
    pub albums: Vec<AlbumSummary>,
    pub artists: Vec<ArtistSummary>,
    pub playlists: Vec<PlaylistSummary>,
}

struct PlaylistCatalog {
    name: String,
    ids: Vec<u64>,
    tracks: HashMap<u64, OnlineTrack>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error(transparent)]
    Api(#[from] NcmError),
    #[error("NCM API returned code {code}: {message}")]
    ApiCode { code: i64, message: String },
    #[error("NCM response is missing {0}")]
    InvalidResponse(&'static str),
    #[error("song {0} has no playable source")]
    Unavailable(u64),
}

pub type Result<T> = std::result::Result<T, DiscoveryError>;
type SharedCache<T> = Arc<OnceCell<T>>;
type KeyedCache<K, V> = Mutex<HashMap<K, SharedCache<V>>>;

#[derive(Clone)]
pub struct Discovery {
    client: NcmClient,
    cache: Arc<DiscoveryCache>,
}

#[derive(Default)]
struct DiscoveryCache {
    daily_songs: OnceCell<Vec<OnlineTrack>>,
    recommended_playlists: OnceCell<Vec<PlaylistSummary>>,
    user_playlists: KeyedCache<u64, Vec<PlaylistSummary>>,
    playlist_catalogs: Mutex<HashMap<u64, Arc<Mutex<PlaylistCatalog>>>>,
    listening_ranks: KeyedCache<(u64, bool), Vec<RankedTrack>>,
    lyrics: LyricsCache,
    covers: crate::cover::CoverCache,
}

impl Discovery {
    pub fn new(client: NcmClient) -> Self {
        Self {
            client,
            cache: Arc::new(DiscoveryCache::default()),
        }
    }

    pub fn with_lyrics_dir(client: NcmClient, dir: impl Into<std::path::PathBuf>) -> Self {
        let lyrics = dir.into();
        let covers = lyrics
            .parent()
            .map(|parent| parent.join("covers"))
            .unwrap_or_else(|| lyrics.join("covers"));
        Self {
            client,
            cache: Arc::new(DiscoveryCache {
                lyrics: LyricsCache::open(lyrics),
                covers: crate::cover::CoverCache::open(covers),
                ..DiscoveryCache::default()
            }),
        }
    }

    pub fn cached_lyrics(&self, song_id: u64) -> Option<Lyrics> {
        self.cache
            .lyrics
            .memory_get(song_id)
            .map(LyricDocument::into_lyrics)
    }

    pub async fn track_details(&self, track_ids: &[u64]) -> Result<Vec<OnlineTrack>> {
        let mut tracks = std::collections::HashMap::new();
        for chunk in track_ids.chunks(500) {
            let response = checked(self.client.execute(api::track::detail(chunk)?).await?)?;
            for value in response
                .get("songs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(track) = parse_track(value) {
                    tracks.insert(track.id, track);
                }
            }
        }
        Ok(track_ids
            .iter()
            .filter_map(|id| tracks.remove(id))
            .collect())
    }

    pub async fn song_cover_url(&self, song_id: u64) -> Result<String> {
        Ok(self
            .track_details(&[song_id])
            .await?
            .into_iter()
            .next()
            .map(|track| track.cover_url)
            .filter(|url| !url.is_empty())
            .unwrap_or_default())
    }

    pub async fn fetch_cover(&self, url: &str) -> Result<Vec<u8>> {
        if url.is_empty() {
            return Err(DiscoveryError::InvalidResponse("cover url"));
        }
        Ok(self
            .client
            .get_bytes(&crate::cover::ncm_thumb_url(url))
            .await?)
    }

    pub async fn load_cover_image(
        &self,
        song_id: u64,
        title: &str,
        artists: &str,
        album: &str,
        known_url: Option<String>,
    ) -> Option<(Vec<u8>, Option<String>)> {
        if let Some(bytes) = self.cache.covers.get(song_id) {
            return Some((bytes, None));
        }
        if self.cache.covers.is_miss(song_id) && known_url.is_none() {
            return None;
        }
        let mut resolved = known_url.and_then(|url| crate::cover::nonempty_url(&url));
        if resolved.is_none() && crate::library::is_catalog_id(song_id) {
            resolved =
                crate::cover::nonempty_url(&self.song_cover_url(song_id).await.unwrap_or_default());
        }
        if resolved.is_none() {
            resolved = self.search_cover_url(title, artists, album).await;
        }
        let Some(url) = resolved else {
            self.cache.covers.remember_miss(song_id);
            return None;
        };
        match self.fetch_cover(&url).await {
            Ok(bytes) if crate::cover::looks_like_image(&bytes) => {
                self.cache.covers.put(song_id, &bytes);
                Some((bytes, Some(url)))
            }
            Ok(_) => {
                self.cache.covers.remember_miss(song_id);
                None
            }
            Err(_) => None,
        }
    }

    pub async fn search_cover_url(
        &self,
        title: &str,
        artists: &str,
        album: &str,
    ) -> Option<String> {
        let query = cover_search_query(title, artists, album);
        if query.is_empty() {
            return None;
        }
        let page = self
            .search_page(&query, SearchKind::Song, 0, 8)
            .await
            .ok()?;
        page.tracks
            .into_iter()
            .filter(|track| !track.cover_url.is_empty())
            .max_by_key(|track| cover_match_score(title, artists, album, track))
            .filter(|track| cover_match_score(title, artists, album, track) >= 50)
            .map(|track| track.cover_url)
    }

    pub async fn playback_source(
        &self,
        song_id: u64,
        quality: AudioQuality,
    ) -> Result<PlaybackSource> {
        for quality in quality.fallback_chain() {
            let response = checked(
                self.client
                    .execute(api::track::audio_v1(
                        &[song_id],
                        quality.api_level(),
                        quality.encode_type(),
                    ))
                    .await?,
            )?;
            let Some(value) = response
                .get("data")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
            else {
                continue;
            };
            let url = string_at(value, "url");
            if !url.is_empty() {
                return Ok(PlaybackSource {
                    url,
                    format: string_at(value, "type"),
                    bitrate: number_at(value, "br").unwrap_or_default(),
                    size: number_at(value, "size").unwrap_or_default(),
                });
            }
        }
        Err(DiscoveryError::Unavailable(song_id))
    }

    pub async fn lyrics(&self, song_id: u64) -> Result<Lyrics> {
        if let Some(document) = self.cache.lyrics.get(song_id) {
            return Ok(document.into_lyrics());
        }
        let document = self.fetch_lyric_document(song_id).await?;
        if !document.is_empty() {
            self.cache.lyrics.put(song_id, document.clone());
        }
        Ok(document.into_lyrics())
    }

    async fn fetch_lyric_document(&self, song_id: u64) -> Result<LyricDocument> {
        let primary = self.client.execute(api::track::lyrics_v1(song_id)).await;
        if let Ok(response) = primary
            && let Ok(response) = checked(response)
        {
            let document = parse_lyric_document(&response);
            if !document.is_empty() {
                return Ok(document);
            }
        }
        let response = checked(self.client.execute(api::track::lyrics(song_id)).await?)?;
        Ok(parse_lyric_document(&response))
    }

    pub async fn daily_songs(&self) -> Result<Vec<OnlineTrack>> {
        self.cache
            .daily_songs
            .get_or_try_init(|| async {
                let response = checked(self.client.execute(api::recommend::daily_songs()).await?)?;
                let songs = response
                    .get("data")
                    .and_then(|data| data.get("dailySongs"))
                    .or_else(|| response.get("recommend"))
                    .and_then(Value::as_array)
                    .ok_or(DiscoveryError::InvalidResponse("data.dailySongs"))?;
                Ok(songs.iter().filter_map(parse_track).collect())
            })
            .await
            .cloned()
    }

    pub async fn recommended_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.cache
            .recommended_playlists
            .get_or_try_init(|| async {
                let response = checked(self.client.execute(api::recommend::playlists()).await?)?;
                let values = response
                    .get("recommend")
                    .and_then(Value::as_array)
                    .ok_or(DiscoveryError::InvalidResponse("recommend"))?;
                Ok(values
                    .iter()
                    .filter_map(|value| parse_playlist(value, 0))
                    .collect())
            })
            .await
            .cloned()
    }

    pub async fn playlist(&self, playlist_id: u64) -> Result<DiscoveryResult> {
        let response = self.playlist_response(playlist_id).await?;
        let playlist = response
            .get("playlist")
            .ok_or(DiscoveryError::InvalidResponse("playlist"))?;
        let track_ids = ids_at(playlist.get("trackIds"));
        Ok(DiscoveryResult {
            name: string_at(playlist, "name"),
            total_found: track_ids.len() as u64,
            matched_items: 1,
            track_ids,
        })
    }

    /// Opens a playlist without throwing away the track objects returned by
    /// the detail endpoint. Only IDs omitted from that response need another
    /// round trip, which keeps ordinary playlist opens to one API request.
    pub async fn playlist_tracks(&self, playlist_id: u64) -> Result<(String, Vec<OnlineTrack>)> {
        let catalog = self.ensure_playlist_catalog(playlist_id).await?;
        let total = catalog.lock().await.ids.len().max(1);
        let page = self.playlist_page(playlist_id, 0, total).await?;
        Ok((page.title, page.items))
    }

    pub async fn playlist_page(
        &self,
        playlist_id: u64,
        offset: usize,
        limit: usize,
    ) -> Result<TrackPage> {
        let catalog = self.ensure_playlist_catalog(playlist_id).await?;
        let limit = limit.max(1);
        let (title, slice, missing, total) = {
            let catalog = catalog.lock().await;
            let total = catalog.ids.len() as u64;
            if offset >= catalog.ids.len() {
                return Ok(TrackPage {
                    title: catalog.name.clone(),
                    items: Vec::new(),
                    pagination: PaginationInfo::from_fetch(offset, limit, 0, total),
                });
            }
            let end = offset.saturating_add(limit).min(catalog.ids.len());
            let slice = catalog.ids[offset..end].to_vec();
            let missing = slice
                .iter()
                .copied()
                .filter(|id| !catalog.tracks.contains_key(id))
                .collect::<Vec<_>>();
            (catalog.name.clone(), slice, missing, total)
        };
        if !missing.is_empty() {
            let fetched = self.track_details(&missing).await?;
            let mut catalog = catalog.lock().await;
            for track in fetched {
                catalog.tracks.insert(track.id, track);
            }
        }
        let items = {
            let catalog = catalog.lock().await;
            slice
                .iter()
                .filter_map(|id| catalog.tracks.get(id).cloned())
                .collect::<Vec<_>>()
        };
        let received = items.len();
        Ok(TrackPage {
            title,
            items,
            pagination: PaginationInfo::from_fetch(offset, limit, received, total),
        })
    }

    async fn ensure_playlist_catalog(
        &self,
        playlist_id: u64,
    ) -> Result<Arc<Mutex<PlaylistCatalog>>> {
        {
            let catalogs = self.cache.playlist_catalogs.lock().await;
            if let Some(existing) = catalogs.get(&playlist_id) {
                return Ok(existing.clone());
            }
        }
        let response = self.playlist_response(playlist_id).await?;
        let playlist = response
            .get("playlist")
            .ok_or(DiscoveryError::InvalidResponse("playlist"))?;
        let ids = ids_at(playlist.get("trackIds"));
        let tracks = playlist
            .get("tracks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .map(|track| (track.id, track))
            .collect::<HashMap<_, _>>();
        let catalog = Arc::new(Mutex::new(PlaylistCatalog {
            name: string_at(playlist, "name"),
            ids,
            tracks,
        }));
        let mut catalogs = self.cache.playlist_catalogs.lock().await;
        Ok(catalogs
            .entry(playlist_id)
            .or_insert_with(|| catalog.clone())
            .clone())
    }

    async fn playlist_response(&self, playlist_id: u64) -> Result<Value> {
        checked(
            self.client
                .execute(api::playlist::detail(playlist_id))
                .await?,
        )
    }

    pub async fn album(&self, album_id: u64) -> Result<DiscoveryResult> {
        let response = checked(self.client.execute(api::album::detail(album_id)).await?)?;
        let album = response
            .get("album")
            .ok_or(DiscoveryError::InvalidResponse("album"))?;
        let track_ids = ids_at(response.get("songs"));
        Ok(DiscoveryResult {
            name: string_at(album, "name"),
            total_found: track_ids.len() as u64,
            matched_items: 1,
            track_ids,
        })
    }

    pub async fn album_page(
        &self,
        album_id: u64,
        offset: usize,
        limit: usize,
    ) -> Result<TrackPage> {
        let result = self.album(album_id).await?;
        let limit = limit.max(1);
        let total = result.track_ids.len() as u64;
        let slice = result
            .track_ids
            .get(offset..offset.saturating_add(limit).min(result.track_ids.len()))
            .unwrap_or(&[])
            .to_vec();
        let items = self.track_details(&slice).await?;
        let received = items.len();
        Ok(TrackPage {
            title: result.name,
            items,
            pagination: PaginationInfo::from_fetch(offset, limit, received, total),
        })
    }

    pub async fn artist_song_page(
        &self,
        artist_id: u64,
        offset: usize,
        limit: usize,
    ) -> Result<TrackPage> {
        let limit = limit.max(1);
        let response = checked(
            self.client
                .execute(api::artist::songs(artist_id, offset, limit))
                .await?,
        )?;
        let items = response
            .get("songs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .collect::<Vec<_>>();
        let total = number_at(&response, "total").unwrap_or_else(|| {
            offset.saturating_add(items.len()) as u64
                + u64::from(
                    response
                        .get("more")
                        .and_then(Value::as_bool)
                        .unwrap_or(items.len() == limit),
                )
        });
        let title = response
            .get("artist")
            .map(|artist| string_at(artist, "name"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("Artist {artist_id}"));
        let received = items.len();
        Ok(TrackPage {
            title,
            items,
            pagination: PaginationInfo::from_fetch(offset, limit, received, total),
        })
    }

    /// Combines released albums and the artist-song endpoint in stable source order.
    pub async fn artist_tracks(&self, artist_id: u64) -> Result<DiscoveryResult> {
        let mut artist_name = String::new();
        let mut album_ids = Vec::new();
        let mut seen_albums = HashSet::new();
        let mut offset = 0;

        for _ in 0..MAX_PAGES {
            let response = checked(
                self.client
                    .execute(api::artist::albums(
                        artist_id,
                        offset,
                        ARTIST_ALBUM_PAGE_SIZE,
                    ))
                    .await?,
            )?;
            if artist_name.is_empty() {
                artist_name = response
                    .get("artist")
                    .map(|artist| string_at(artist, "name"))
                    .unwrap_or_default();
            }
            let batch = response
                .get("hotAlbums")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            for album in &batch {
                if let Some(id) = id_at(album, "id")
                    && seen_albums.insert(id)
                {
                    album_ids.push(id);
                }
            }
            if !response
                .get("more")
                .and_then(Value::as_bool)
                .unwrap_or(batch.len() == ARTIST_ALBUM_PAGE_SIZE)
            {
                break;
            }
            offset += ARTIST_ALBUM_PAGE_SIZE;
        }

        let mut track_ids = Vec::new();
        let mut seen_tracks = HashSet::new();
        for album_id in &album_ids {
            let album = self.album(*album_id).await?;
            append_unique(&mut track_ids, &mut seen_tracks, album.track_ids);
        }

        offset = 0;
        for _ in 0..MAX_PAGES {
            let response = checked(
                self.client
                    .execute(api::artist::songs(artist_id, offset, ARTIST_SONG_PAGE_SIZE))
                    .await?,
            )?;
            let batch = response
                .get("songs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            append_unique(
                &mut track_ids,
                &mut seen_tracks,
                ids_at(Some(&Value::Array(batch.clone()))),
            );
            let more = response
                .get("more")
                .and_then(Value::as_bool)
                .unwrap_or(batch.len() == ARTIST_SONG_PAGE_SIZE);
            if !more || batch.len() < ARTIST_SONG_PAGE_SIZE {
                break;
            }
            offset += ARTIST_SONG_PAGE_SIZE;
        }

        Ok(DiscoveryResult {
            name: if artist_name.is_empty() {
                format!("Artist {artist_id}")
            } else {
                artist_name
            },
            total_found: track_ids.len() as u64,
            matched_items: album_ids.len().saturating_add(1),
            track_ids,
        })
    }

    /// Searches and expands non-song results into a single stable track list.
    pub async fn search(
        &self,
        keyword: &str,
        kind: SearchKind,
        max_results: usize,
    ) -> Result<DiscoveryResult> {
        let keyword = keyword.trim();
        if keyword.is_empty() || max_results == 0 {
            return Ok(DiscoveryResult {
                name: keyword.to_owned(),
                track_ids: Vec::new(),
                total_found: 0,
                matched_items: 0,
            });
        }

        let page_size = if kind == SearchKind::Song {
            SEARCH_SONG_PAGE_SIZE
        } else {
            SEARCH_ENTITY_PAGE_SIZE
        };
        let (array_key, count_key) = search_keys(kind);
        let mut offset = 0;
        let mut matched_items = 0;
        let mut total_found = 0;
        let mut track_ids = Vec::new();
        let mut seen_tracks = HashSet::new();
        let mut seen_entities = HashSet::new();

        for page in 0..MAX_PAGES {
            if matched_items >= max_results {
                break;
            }
            let response = checked(
                self.client
                    .execute(api::search::cloud(keyword, kind.into(), page_size, offset))
                    .await?,
            )?;
            let result = response
                .get("result")
                .ok_or(DiscoveryError::InvalidResponse("result"))?;
            if page == 0 {
                total_found = number_at(result, count_key).unwrap_or_default();
            }
            let entities = result
                .get(array_key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if entities.is_empty() {
                break;
            }

            for entity in &entities {
                let Some(id) = id_at(entity, "id") else {
                    continue;
                };
                if !seen_entities.insert(id) {
                    continue;
                }
                matched_items += 1;
                let expanded = match kind {
                    SearchKind::Song => vec![id],
                    SearchKind::Album => self.album(id).await?.track_ids,
                    SearchKind::Artist => self.artist_tracks(id).await?.track_ids,
                    SearchKind::Playlist => self.playlist(id).await?.track_ids,
                };
                append_unique(&mut track_ids, &mut seen_tracks, expanded);
                if matched_items >= max_results {
                    break;
                }
            }

            if entities.len() < page_size {
                break;
            }
            offset += page_size;
        }

        Ok(DiscoveryResult {
            name: keyword.to_owned(),
            track_ids,
            total_found,
            matched_items,
        })
    }

    pub async fn search_page(
        &self,
        keyword: &str,
        kind: SearchKind,
        offset: usize,
        limit: usize,
    ) -> Result<SearchPage> {
        let keyword = keyword.trim();
        let limit = limit.max(1);
        if keyword.is_empty() {
            return Ok(SearchPage {
                title: String::new(),
                kind,
                pagination: PaginationInfo::from_fetch(offset, limit, 0, 0),
                tracks: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
                playlists: Vec::new(),
            });
        }
        let response = checked(
            self.client
                .execute(api::search::cloud(keyword, kind.into(), limit, offset))
                .await?,
        )?;
        let result = response
            .get("result")
            .ok_or(DiscoveryError::InvalidResponse("result"))?;
        let (array_key, count_key) = search_keys(kind);
        let total = number_at(result, count_key).unwrap_or_default();
        let entities = result
            .get(array_key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut page = SearchPage {
            title: keyword.to_owned(),
            kind,
            pagination: PaginationInfo::default(),
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        };
        match kind {
            SearchKind::Song => {
                page.tracks = entities.iter().filter_map(parse_track).collect();
            }
            SearchKind::Album => {
                page.albums = entities.iter().filter_map(parse_album).collect();
            }
            SearchKind::Artist => {
                page.artists = entities.iter().filter_map(parse_artist).collect();
            }
            SearchKind::Playlist => {
                page.playlists = entities
                    .iter()
                    .filter_map(|value| parse_playlist(value, 0))
                    .collect();
            }
        }
        let received = match kind {
            SearchKind::Song => page.tracks.len(),
            SearchKind::Album => page.albums.len(),
            SearchKind::Artist => page.artists.len(),
            SearchKind::Playlist => page.playlists.len(),
        };
        page.pagination = PaginationInfo::from_fetch(offset, limit, received, total);
        Ok(page)
    }

    pub async fn user_playlists(
        &self,
        user_id: u64,
        scope: PlaylistScope,
    ) -> Result<Vec<PlaylistSummary>> {
        let cell = {
            let mut cache = self.cache.user_playlists.lock().await;
            cache.entry(user_id).or_default().clone()
        };
        let playlists = cell
            .get_or_try_init(|| async {
                let mut playlists = Vec::new();
                let mut seen = HashSet::new();
                let mut offset = 0;

                for _ in 0..MAX_PAGES {
                    let response = checked(
                        self.client
                            .execute(api::user::playlists(user_id, offset, COLLECTION_PAGE_SIZE))
                            .await?,
                    )?;
                    let batch = response
                        .get("playlist")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if batch.is_empty() {
                        break;
                    }
                    for value in &batch {
                        let Some(summary) = parse_playlist(value, user_id) else {
                            continue;
                        };
                        if seen.insert(summary.id) {
                            playlists.push(summary);
                        }
                    }
                    if !response
                        .get("more")
                        .and_then(Value::as_bool)
                        .unwrap_or(batch.len() == COLLECTION_PAGE_SIZE)
                    {
                        break;
                    }
                    offset += COLLECTION_PAGE_SIZE;
                }
                Ok::<_, DiscoveryError>(playlists)
            })
            .await?;
        Ok(playlists
            .iter()
            .filter(|playlist| matches_playlist_scope(playlist, scope))
            .cloned()
            .collect())
    }

    pub async fn user_playlists_page(
        &self,
        user_id: u64,
        scope: PlaylistScope,
        offset: usize,
        limit: usize,
    ) -> Result<CollectionPage<PlaylistSummary>> {
        let limit = limit.max(1);
        let response = checked(
            self.client
                .execute(api::user::playlists(user_id, offset, limit))
                .await?,
        )?;
        let batch = response
            .get("playlist")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let received = batch.len();
        let more = response
            .get("more")
            .and_then(Value::as_bool)
            .unwrap_or(received == limit);
        let items = batch
            .iter()
            .filter_map(|value| parse_playlist(value, user_id))
            .filter(|playlist| matches_playlist_scope(playlist, scope))
            .collect::<Vec<_>>();
        Ok(CollectionPage {
            items,
            pagination: PaginationInfo::from_fetch(
                offset,
                limit,
                received,
                if more {
                    0
                } else {
                    offset as u64 + received as u64
                },
            ),
        })
    }

    pub async fn subscribed_albums(&self) -> Result<Vec<AlbumSummary>> {
        let mut albums = Vec::new();
        let mut offset = 0;
        for _ in 0..MAX_PAGES {
            let page = self
                .subscribed_albums_page(offset, COLLECTION_PAGE_SIZE)
                .await?;
            if page.items.is_empty() && !page.pagination.has_more {
                break;
            }
            albums.extend(page.items);
            if !page.pagination.has_more {
                break;
            }
            offset = page.pagination.offset;
        }
        Ok(albums)
    }

    pub async fn subscribed_albums_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<CollectionPage<AlbumSummary>> {
        let limit = limit.max(1);
        let response = checked(
            self.client
                .execute(api::user::subscribed_albums(offset, limit))
                .await?,
        )?;
        let batch = response
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let received = batch.len();
        let total = number_at(&response, "count").unwrap_or_default();
        let items = batch.iter().filter_map(parse_album).collect::<Vec<_>>();
        Ok(CollectionPage {
            items,
            pagination: PaginationInfo::from_fetch(offset, limit, received, total),
        })
    }

    pub async fn subscribed_artists(&self) -> Result<Vec<ArtistSummary>> {
        let mut artists = Vec::new();
        let mut offset = 0;
        for _ in 0..MAX_PAGES {
            let page = self
                .subscribed_artists_page(offset, COLLECTION_PAGE_SIZE)
                .await?;
            if page.items.is_empty() && !page.pagination.has_more {
                break;
            }
            artists.extend(page.items);
            if !page.pagination.has_more {
                break;
            }
            offset = page.pagination.offset;
        }
        Ok(artists)
    }

    pub async fn subscribed_artists_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<CollectionPage<ArtistSummary>> {
        let limit = limit.max(1);
        let response = checked(
            self.client
                .execute(api::user::subscribed_artists(offset, limit))
                .await?,
        )?;
        let batch = response
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let received = batch.len();
        let total = number_at(&response, "count").unwrap_or_default();
        let items = batch.iter().filter_map(parse_artist).collect::<Vec<_>>();
        Ok(CollectionPage {
            items,
            pagination: PaginationInfo::from_fetch(offset, limit, received, total),
        })
    }

    pub async fn listening_rank(
        &self,
        user_id: u64,
        kind: ListeningRank,
    ) -> Result<Vec<RankedTrack>> {
        let rank_key = (user_id, matches!(kind, ListeningRank::All));
        let cell = {
            let mut cache = self.cache.listening_ranks.lock().await;
            cache.entry(rank_key).or_default().clone()
        };
        cell.get_or_try_init(|| async {
            let response = checked(
                self.client
                    .execute(api::user::listening_rank(user_id, kind))
                    .await?,
            )?;
            let key = match kind {
                ListeningRank::Week => "weekData",
                ListeningRank::All => "allData",
            };
            Ok(response
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|value| {
                    Some(RankedTrack {
                        track: parse_track(value.get("song")?)?,
                        play_count: number_at(value, "playCount").unwrap_or_default(),
                        score: number_at(value, "score").unwrap_or_default(),
                    })
                })
                .collect())
        })
        .await
        .cloned()
    }
}

fn checked(response: Value) -> Result<Value> {
    match response.get("code").and_then(Value::as_i64) {
        Some(200) => Ok(response),
        Some(code) => Err(DiscoveryError::ApiCode {
            code,
            message: response
                .get("message")
                .or_else(|| response.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("unknown API error")
                .to_owned(),
        }),
        None => Err(DiscoveryError::InvalidResponse("code")),
    }
}

fn parse_lyric_document(response: &Value) -> LyricDocument {
    LyricDocument {
        lrc: lyric_text(response, "lrc").unwrap_or_default().to_owned(),
        tlyric: lyric_text(response, "tlyric")
            .unwrap_or_default()
            .to_owned(),
        yrc: lyric_text(response, "yrc").unwrap_or_default().to_owned(),
    }
}

fn lyric_text<'a>(response: &'a Value, key: &str) -> Option<&'a str> {
    response
        .get(key)
        .and_then(|value| value.get("lyric"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn search_keys(kind: SearchKind) -> (&'static str, &'static str) {
    match kind {
        SearchKind::Song => ("songs", "songCount"),
        SearchKind::Album => ("albums", "albumCount"),
        SearchKind::Artist => ("artists", "artistCount"),
        SearchKind::Playlist => ("playlists", "playlistCount"),
    }
}

fn parse_playlist(value: &Value, user_id: u64) -> Option<PlaylistSummary> {
    Some(PlaylistSummary {
        id: id_at(value, "id")?,
        name: string_at(value, "name"),
        track_count: number_at(value, "trackCount").unwrap_or_default(),
        created_by_user: value
            .get("creator")
            .and_then(|creator| id_at(creator, "userId"))
            == Some(user_id),
        special_type: number_at(value, "specialType").unwrap_or_default(),
    })
}

pub fn liked_playlist(playlists: &[PlaylistSummary]) -> Option<&PlaylistSummary> {
    playlists
        .iter()
        .find(|playlist| playlist.special_type == 5)
        .or_else(|| {
            playlists
                .iter()
                .find(|playlist| playlist.created_by_user && playlist.name.contains("喜欢"))
        })
}

fn matches_playlist_scope(playlist: &PlaylistSummary, scope: PlaylistScope) -> bool {
    match scope {
        PlaylistScope::All => true,
        PlaylistScope::Created => playlist.created_by_user && playlist.special_type != 5,
        PlaylistScope::Subscribed => !playlist.created_by_user,
    }
}

fn parse_artist(value: &Value) -> Option<ArtistSummary> {
    Some(ArtistSummary {
        id: id_at(value, "id")?,
        name: string_at(value, "name"),
        album_count: number_at(value, "albumSize").unwrap_or_default(),
        music_count: number_at(value, "musicSize").unwrap_or_default(),
    })
}

fn parse_album(value: &Value) -> Option<AlbumSummary> {
    let artists = value
        .get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|artist| string_at(artist, "name"))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    Some(AlbumSummary {
        id: id_at(value, "id")?,
        name: string_at(value, "name"),
        artists,
        track_count: number_at(value, "size").unwrap_or_default(),
    })
}

fn parse_track(value: &Value) -> Option<OnlineTrack> {
    let artists = value
        .get("ar")
        .or_else(|| value.get("artists"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|artist| string_at(artist, "name"))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let album = value.get("al").or_else(|| value.get("album"));
    Some(OnlineTrack {
        id: id_at(value, "id")?,
        title: string_at(value, "name"),
        artists,
        album: album
            .map(|value| string_at(value, "name"))
            .unwrap_or_default(),
        duration_ms: number_at(value, "dt")
            .or_else(|| number_at(value, "duration"))
            .unwrap_or_default(),
        cover_url: parse_cover_url(value, album),
    })
}

pub(crate) fn cover_search_query(title: &str, artists: &str, album: &str) -> String {
    [title, artists, album]
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn cover_match_score(
    title: &str,
    artists: &str,
    album: &str,
    track: &OnlineTrack,
) -> i32 {
    let title = normalize_cover_text(title);
    let artists = normalize_cover_text(artists);
    let album = normalize_cover_text(album);
    let got_title = normalize_cover_text(&track.title);
    let got_artists = normalize_cover_text(&track.artists);
    let got_album = normalize_cover_text(&track.album);
    let mut score = 0;
    if !title.is_empty() && got_title == title {
        score += 100;
    } else if !title.is_empty() && (got_title.contains(&title) || title.contains(&got_title)) {
        score += 50;
    }
    if !artists.is_empty() && (got_artists.contains(&artists) || artists.contains(&got_artists)) {
        score += 30;
    }
    if !album.is_empty() && (got_album.contains(&album) || album.contains(&got_album)) {
        score += 10;
    }
    score
}

fn normalize_cover_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_cover_url(song: &Value, album: Option<&Value>) -> String {
    const KEYS: &[&str] = &["picUrl", "coverUrl", "blurPicUrl", "img1v1Url"];
    for source in [album, Some(song)] {
        let Some(value) = source else {
            continue;
        };
        for key in KEYS {
            let url = string_at(value, key);
            if !url.is_empty() && url.starts_with("http") {
                return url;
            }
        }
    }
    String::new()
}

fn append_unique(target: &mut Vec<u64>, seen: &mut HashSet<u64>, ids: Vec<u64>) {
    target.extend(ids.into_iter().filter(|id| seen.insert(*id)));
}

fn ids_at(value: Option<&Value>) -> Vec<u64> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| id_at(value, "id"))
        .collect()
}

fn id_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(number)
}

fn number_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(number)
}

fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn string_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn summaries_tolerate_numeric_strings_and_classify_ownership() {
        let playlist = parse_playlist(
            &json!({
                "id": "12",
                "name": "mine",
                "trackCount": 3,
                "creator": { "userId": "7" }
            }),
            7,
        )
        .unwrap();
        assert_eq!(playlist.id, 12);
        assert!(playlist.created_by_user);
        assert_eq!(playlist.special_type, 0);

        let liked = parse_playlist(
            &json!({
                "id": 99,
                "name": "我喜欢的音乐",
                "trackCount": 8,
                "specialType": 5,
                "creator": { "userId": 7 }
            }),
            7,
        )
        .unwrap();
        assert_eq!(
            liked_playlist(std::slice::from_ref(&liked)).map(|item| item.id),
            Some(99)
        );

        let artist = parse_artist(&json!({
            "id": "8",
            "name": "22/7",
            "albumSize": 4,
            "musicSize": 40
        }))
        .unwrap();
        assert_eq!(artist.id, 8);
        assert_eq!(artist.album_count, 4);

        let album = parse_album(&json!({
            "id": 9,
            "name": "album",
            "size": 2,
            "artists": [{ "name": "A" }, { "name": "B" }]
        }))
        .unwrap();
        assert_eq!(album.artists, "A / B");

        let track = parse_track(&json!({
            "id": 3, "name": "song", "dt": 1000,
            "ar": [{ "name": "artist" }],
            "al": { "name": "record", "picUrl": "https://p1.music.126.net/cover.jpg" }
        }))
        .unwrap();
        assert_eq!(track.artists, "artist");
        assert_eq!(track.album, "record");
        assert_eq!(track.cover_url, "https://p1.music.126.net/cover.jpg");
    }

    #[test]
    fn cover_search_prefers_title_and_artist_matches() {
        let exact = OnlineTrack {
            id: 1,
            title: "晴天".into(),
            artists: "周杰伦".into(),
            album: "叶惠美".into(),
            duration_ms: 1,
            cover_url: "https://p1.music.126.net/a.jpg".into(),
        };
        let weak = OnlineTrack {
            id: 2,
            title: "七里香".into(),
            artists: "周杰伦".into(),
            album: String::new(),
            duration_ms: 1,
            cover_url: "https://p1.music.126.net/b.jpg".into(),
        };
        assert!(cover_match_score("晴天", "周杰伦", "叶惠美", &exact) >= 50);
        assert!(
            cover_match_score("晴天", "周杰伦", "叶惠美", &exact)
                > cover_match_score("晴天", "周杰伦", "叶惠美", &weak)
        );
        assert_eq!(
            cover_search_query("晴天", "周杰伦", "叶惠美"),
            "晴天 周杰伦"
        );
    }

    #[test]
    fn cover_url_falls_back_across_album_and_song_fields() {
        let track = parse_track(&json!({
            "id": 4, "name": "song", "dt": 1,
            "ar": [{ "name": "a" }],
            "al": { "name": "record" },
            "blurPicUrl": "https://p2.music.126.net/blur.jpg"
        }))
        .unwrap();
        assert_eq!(track.cover_url, "https://p2.music.126.net/blur.jpg");
    }

    #[test]
    fn stable_deduplication_keeps_discovery_order() {
        let mut target = vec![3];
        let mut seen = HashSet::from([3]);
        append_unique(&mut target, &mut seen, vec![2, 3, 1, 2]);
        assert_eq!(target, vec![3, 2, 1]);
    }

    #[test]
    fn api_errors_are_not_treated_as_empty_results() {
        let error = checked(json!({ "code": 401, "message": "login required" })).unwrap_err();
        assert!(matches!(error, DiscoveryError::ApiCode { code: 401, .. }));
    }

    #[test]
    fn lyrics_prefer_word_timing_and_keep_translation() {
        let lyrics = parse_lyric_document(&json!({
            "lrc": { "lyric": "[00:01]fallback" },
            "tlyric": { "lyric": "[00:01]翻译" },
            "yrc": { "lyric": "[1000,1000](1000,500,0)timed" }
        }))
        .into_lyrics();
        assert_eq!(lyrics.original[0].text, "timed");
        assert_eq!(
            lyrics.translation_at(std::time::Duration::from_secs(1)),
            Some("翻译")
        );
    }

    #[test]
    fn lyric_document_cache_preserves_yrc_and_translation() {
        let document = parse_lyric_document(&json!({
            "lrc": { "lyric": "[00:01]fallback" },
            "tlyric": { "lyric": "[00:01]翻译" },
            "yrc": { "lyric": "[1000,1000](1000,500,0)timed" }
        }));
        let directory = tempfile::tempdir().unwrap();
        let cache = LyricsCache::open(directory.path());
        cache.put(9, document.clone());
        let restored = LyricsCache::open(directory.path()).get(9).unwrap();
        assert_eq!(restored, document);
        assert_eq!(restored.into_lyrics().original[0].text, "timed");
    }
}
