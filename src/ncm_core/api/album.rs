use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode};

/// Album metadata and its complete song list.
pub fn detail(album_id: u64) -> ApiSpec {
    ApiSpec::post(
        format!("/weapi/v1/album/{album_id}"),
        CryptoMode::Weapi,
        json!({}),
    )
}
