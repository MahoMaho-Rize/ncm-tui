//! Online catalog discovery. Callers receive domain values, never raw API JSON.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, OnceCell};

use crate::download::AudioQuality;
use crate::lyrics::Lyrics;
use crate::ncm_core::{
    NcmClient, NcmError,
    api::{self, search::SearchType, user::ListeningRank},
};
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
    playlist_tracks: KeyedCache<u64, (String, Vec<OnlineTrack>)>,
    listening_ranks: KeyedCache<(u64, bool), Vec<RankedTrack>>,
}

impl Discovery {
    pub fn new(client: NcmClient) -> Self {
        Self {
            client,
            cache: Arc::new(DiscoveryCache::default()),
        }
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
        let primary = self.client.execute(api::track::lyrics_v1(song_id)).await;
        if let Ok(response) = primary
            && let Ok(response) = checked(response)
        {
            let lyrics = parse_lyrics(&response);
            if !lyrics.is_empty() {
                return Ok(lyrics);
            }
        }
        let response = checked(self.client.execute(api::track::lyrics(song_id)).await?)?;
        Ok(parse_lyrics(&response))
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
        let cell = {
            let mut cache = self.cache.playlist_tracks.lock().await;
            cache.entry(playlist_id).or_default().clone()
        };
        cell.get_or_try_init(|| async {
            let response = self.playlist_response(playlist_id).await?;
            let playlist = response
                .get("playlist")
                .ok_or(DiscoveryError::InvalidResponse("playlist"))?;
            let track_ids = ids_at(playlist.get("trackIds"));
            let mut tracks = playlist
                .get("tracks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(parse_track)
                .map(|track| (track.id, track))
                .collect::<HashMap<_, _>>();
            let missing = track_ids
                .iter()
                .copied()
                .filter(|id| !tracks.contains_key(id))
                .collect::<Vec<_>>();
            for track in self.track_details(&missing).await? {
                tracks.insert(track.id, track);
            }
            let ordered = track_ids
                .into_iter()
                .filter_map(|id| tracks.remove(&id))
                .collect();
            Ok((string_at(playlist, "name"), ordered))
        })
        .await
        .cloned()
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
            .filter(|playlist| match scope {
                PlaylistScope::All => true,
                PlaylistScope::Created => playlist.created_by_user,
                PlaylistScope::Subscribed => !playlist.created_by_user,
            })
            .cloned()
            .collect())
    }

    pub async fn subscribed_albums(&self) -> Result<Vec<AlbumSummary>> {
        let mut albums = Vec::new();
        let mut seen = HashSet::new();
        let mut offset = 0;

        for _ in 0..MAX_PAGES {
            let response = checked(
                self.client
                    .execute(api::user::subscribed_albums(offset, COLLECTION_PAGE_SIZE))
                    .await?,
            )?;
            let batch = response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            for value in &batch {
                let Some(summary) = parse_album(value) else {
                    continue;
                };
                if seen.insert(summary.id) {
                    albums.push(summary);
                }
            }
            if !response
                .get("hasMore")
                .and_then(Value::as_bool)
                .unwrap_or(batch.len() == COLLECTION_PAGE_SIZE)
            {
                break;
            }
            offset += COLLECTION_PAGE_SIZE;
        }
        Ok(albums)
    }

    pub async fn subscribed_artists(&self) -> Result<Vec<ArtistSummary>> {
        let mut artists = Vec::new();
        let mut seen = HashSet::new();
        let mut offset = 0;
        for _ in 0..MAX_PAGES {
            let response = checked(
                self.client
                    .execute(api::user::subscribed_artists(offset, COLLECTION_PAGE_SIZE))
                    .await?,
            )?;
            let batch = response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            for value in &batch {
                let Some(id) = id_at(value, "id") else {
                    continue;
                };
                if seen.insert(id) {
                    artists.push(ArtistSummary {
                        id,
                        name: string_at(value, "name"),
                        album_count: number_at(value, "albumSize").unwrap_or_default(),
                        music_count: number_at(value, "musicSize").unwrap_or_default(),
                    });
                }
            }
            if !response
                .get("hasMore")
                .and_then(Value::as_bool)
                .unwrap_or(batch.len() == COLLECTION_PAGE_SIZE)
            {
                break;
            }
            offset += COLLECTION_PAGE_SIZE;
        }
        Ok(artists)
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

fn parse_lyrics(response: &Value) -> Lyrics {
    let lyric = lyric_text(response, "lrc");
    let translated = lyric_text(response, "tlyric");
    let yrc = lyric_text(response, "yrc");
    Lyrics::from_sources(lyric, translated, yrc)
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
    })
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
            "ar": [{ "name": "artist" }], "al": { "name": "record" }
        }))
        .unwrap();
        assert_eq!(track.artists, "artist");
        assert_eq!(track.album, "record");
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
        let lyrics = parse_lyrics(&json!({
            "lrc": { "lyric": "[00:01]fallback" },
            "tlyric": { "lyric": "[00:01]翻译" },
            "yrc": { "lyric": "[1000,1000](1000,500,0)timed" }
        }));
        assert_eq!(lyrics.original[0].text, "timed");
        assert_eq!(
            lyrics.translation_at(std::time::Duration::from_secs(1)),
            Some("翻译")
        );
    }
}
