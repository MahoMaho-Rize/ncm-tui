use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

pub fn daily_songs() -> ApiSpec {
    ApiSpec::post(
        "/api/v3/discovery/recommend/songs",
        CryptoMode::Weapi,
        json!({ "offset": "0", "total": "true", "limit": "1000" }),
    )
}

pub fn playlists() -> ApiSpec {
    ApiSpec::post(
        "/weapi/v1/discovery/recommend/resource",
        CryptoMode::Weapi,
        json!({}),
    )
}
