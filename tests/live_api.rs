use std::time::Duration;

use ncm_tui::ncm_core::{
    NcmClient, SessionConfig,
    api::{login, track},
};

/// Explicitly ignored because normal unit tests must not depend on NCM availability.
#[tokio::test]
#[ignore = "performs a real request to music.163.com"]
async fn public_track_detail_smoke_test() {
    let client = NcmClient::new(SessionConfig::default(), Duration::from_secs(30)).unwrap();
    let response = client
        .execute(track::detail(&[29732235]).unwrap())
        .await
        .unwrap();

    assert_eq!(response["code"], 200);
    assert_eq!(response["songs"][0]["id"], 29732235);
}

/// Exercises EAPI request signing/encryption and encrypted response decoding.
#[tokio::test]
#[ignore = "performs a real request to music.163.com"]
async fn public_audio_url_eapi_smoke_test() {
    let client = NcmClient::new(SessionConfig::default(), Duration::from_secs(30)).unwrap();
    let response = client
        .execute(track::audio_v1(&[29732235], "standard", "mp3"))
        .await
        .unwrap();

    assert_eq!(response["code"], 200);
    assert_eq!(response["data"][0]["id"], 29732235);
}

/// Reads the credential only from process memory. It is never stored in a fixture.
#[tokio::test]
#[ignore = "requires NCM_MUSIC_U and performs authenticated real requests"]
async fn authenticated_session_and_eapi_smoke_test() {
    let music_u = std::env::var("NCM_MUSIC_U").expect("NCM_MUSIC_U is required");
    let client = NcmClient::new(
        SessionConfig::with_cookie(music_u, ""),
        Duration::from_secs(30),
    )
    .unwrap();

    let login_status = client.execute(login::current_login_status()).await.unwrap();
    assert_eq!(login_status["code"], 200);
    let uid = login_status["account"]["id"]
        .as_u64()
        .or_else(|| login_status["profile"]["userId"].as_u64())
        .expect("authenticated response must contain a user id");
    assert!(uid > 0);

    let audio = client
        .execute(track::audio_v1(&[29732235], "standard", "mp3"))
        .await
        .unwrap();
    assert_eq!(audio["code"], 200);
    assert_eq!(audio["data"][0]["id"], 29732235);
}
