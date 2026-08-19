use serde_json::json;

use crate::ncm_core::{ApiSpec, CryptoMode, Result};

pub fn detail(song_ids: &[u64]) -> Result<ApiSpec> {
    let ids = song_ids
        .iter()
        .map(|id| json!({ "id": id.to_string() }))
        .collect::<Vec<_>>();
    Ok(ApiSpec::post(
        "/weapi/v3/song/detail",
        CryptoMode::Weapi,
        json!({ "c": serde_json::to_string(&ids)? }),
    ))
}

pub fn audio(song_ids: &[u64], bitrate: u32, encode_type: &str) -> ApiSpec {
    ApiSpec::post(
        "/eapi/song/enhance/player/url",
        CryptoMode::Eapi,
        json!({
            "ids": song_ids,
            "encodeType": encode_type,
            "br": bitrate.to_string(),
        }),
    )
}

pub fn audio_v1(song_ids: &[u64], level: &str, encode_type: &str) -> ApiSpec {
    ApiSpec::post(
        "/eapi/song/enhance/player/url/v1",
        CryptoMode::Eapi,
        json!({
            "ids": song_ids,
            "encodeType": encode_type,
            "level": level,
        }),
    )
}

pub fn lyrics_v1(song_id: u64) -> ApiSpec {
    ApiSpec::post(
        "/eapi/song/lyric/v1",
        CryptoMode::Eapi,
        json!({
            "id": song_id.to_string(),
            "cp": false,
            "lv": 0,
            "tv": 0,
            "rv": 0,
            "kv": 0,
            "yv": 0,
            "ytv": 0,
            "yrv": 0,
        }),
    )
}

pub fn lyrics(song_id: u64) -> ApiSpec {
    ApiSpec::post(
        "/weapi/song/lyric",
        CryptoMode::Weapi,
        json!({
            "id": song_id.to_string(),
            "lv": "-1",
            "tv": "-1",
            "rv": "-1",
        }),
    )
}
