use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

const PLAYLIST_TRACK_PREVIEW: usize = 1_000;

/// Playlist metadata with all track IDs (not only the first page of songs).
pub fn detail(playlist_id: u64) -> ApiSpec {
    ApiSpec::post(
        "/weapi/v6/playlist/detail",
        CryptoMode::Weapi,
        json!({
            "id": playlist_id,
            // The response still contains every track ID. Asking the server to
            // expand 100,000 full track objects made even small playlists pay
            // the cost of an unnecessarily huge response.
            "n": PLAYLIST_TRACK_PREVIEW,
            "s": 0,
        }),
    )
}
