use std::time::Duration;

use ncm_tui::{
    auth::Authentication,
    config::Config,
    discovery::Discovery,
    download::Downloader,
    library::Library,
    ncm_core::{NcmClient, SessionConfig},
    playback_cache::PlaybackCache,
    tui::{self, Services},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load("config.toml")?;
    let mut session = if config.auth.session_file.is_file() {
        SessionConfig::load(&config.auth.session_file)?
    } else {
        SessionConfig::default()
    };

    if !config.auth.music_u.is_empty() {
        session.music_u = config.auth.music_u.clone();
    }
    if !config.auth.csrf.is_empty() {
        session.csrf_token = config.auth.csrf.clone();
    }

    let client = NcmClient::with_rate_limit(
        session,
        Duration::from_secs(config.download.timeout),
        config.download.api_qps,
    )?;
    let mut library_roots = config.library.dirs.clone();
    if library_roots.is_empty() {
        library_roots.push(config.download.dir.clone());
    }
    let playback_cache =
        PlaybackCache::open(&config.playback_cache.dir, config.playback_cache.max_bytes).await?;
    let ui_state_path = config.download.dir.join(".ncm-tui").join("ui.toml");
    if config.ui.hide_lyrics && !ui_state_path.exists() {
        let _ = std::fs::create_dir_all(config.download.dir.join(".ncm-tui"));
        let _ = std::fs::write(&ui_state_path, "hide_lyrics = true\n");
    }
    let services = Services {
        authentication: Authentication::new(client.clone(), config.auth.session_file),
        discovery: Discovery::with_lyrics_dir(
            client.clone(),
            config.download.dir.join(".ncm-tui").join("lyrics"),
        ),
        library: Library::open(&config.download.dir)?,
        downloader: Downloader::new(client, &config.download.dir, config.download.max_workers)?,
        library_roots,
        playback_cache,
        ui_state_path,
    };

    tui::run(services).await?;
    Ok(())
}
