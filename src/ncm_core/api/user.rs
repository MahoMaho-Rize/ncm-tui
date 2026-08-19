use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

pub fn playlists(user_id: u64, offset: usize, limit: usize) -> ApiSpec {
    ApiSpec::post(
        "/weapi/user/playlist",
        CryptoMode::Weapi,
        json!({
            "offset": offset.to_string(),
            "limit": limit.to_string(),
            "uid": user_id.to_string(),
        }),
    )
}

pub fn subscribed_albums(offset: usize, limit: usize) -> ApiSpec {
    ApiSpec::post(
        "/weapi/album/sublist",
        CryptoMode::Weapi,
        json!({
            "offset": offset.to_string(),
            "limit": limit.to_string(),
        }),
    )
}

pub fn subscribed_artists(offset: usize, limit: usize) -> ApiSpec {
    ApiSpec::post(
        "/weapi/artist/sublist",
        CryptoMode::Weapi,
        json!({
            "offset": offset.to_string(),
            "limit": limit.to_string(),
        }),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListeningRank {
    Week,
    All,
}

pub fn listening_rank(user_id: u64, kind: ListeningRank) -> ApiSpec {
    ApiSpec::post(
        "/weapi/v1/play/record",
        CryptoMode::Weapi,
        json!({
            "uid": user_id.to_string(),
            "type": match kind {
                ListeningRank::Week => "1",
                ListeningRank::All => "0",
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_requests_are_pageable() {
        let playlist_spec = playlists(7, 50, 50);
        assert_eq!(playlist_spec.payload["uid"], "7");
        assert_eq!(playlist_spec.payload["offset"], "50");

        let album_spec = subscribed_albums(100, 50);
        assert_eq!(album_spec.path, "/weapi/album/sublist");
        assert_eq!(album_spec.payload["offset"], "100");

        let artist_spec = subscribed_artists(25, 50);
        assert_eq!(artist_spec.path, "/weapi/artist/sublist");
        assert_eq!(artist_spec.payload["offset"], "25");
    }

    #[test]
    fn listening_rank_selects_week_or_all_time() {
        let week = listening_rank(7, ListeningRank::Week);
        assert_eq!(week.path, "/weapi/v1/play/record");
        assert_eq!(week.payload["uid"], "7");
        assert_eq!(week.payload["type"], "1");

        let all = listening_rank(7, ListeningRank::All);
        assert_eq!(all.payload["type"], "0");
    }
}
