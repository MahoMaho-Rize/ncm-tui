use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

pub fn albums(artist_id: u64, offset: usize, limit: usize) -> ApiSpec {
    ApiSpec::post(
        format!("/weapi/artist/albums/{artist_id}"),
        CryptoMode::Weapi,
        json!({
            "offset": offset.to_string(),
            "total": "true",
            "limit": limit.to_string(),
        }),
    )
}

pub fn songs(artist_id: u64, offset: usize, limit: usize) -> ApiSpec {
    ApiSpec::post(
        "/weapi/v1/artist/songs",
        CryptoMode::Weapi,
        json!({
            "id": artist_id.to_string(),
            "offset": offset.to_string(),
            "total": "true",
            "limit": limit.to_string(),
            "order": "time",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_requests_preserve_legacy_wire_types() {
        let album_spec = albums(42, 100, 50);
        assert_eq!(album_spec.path, "/weapi/artist/albums/42");
        assert_eq!(album_spec.payload["offset"], "100");

        let song_spec = songs(42, 0, 1000);
        assert_eq!(song_spec.payload["id"], "42");
        assert_eq!(song_spec.payload["order"], "time");
    }
}
