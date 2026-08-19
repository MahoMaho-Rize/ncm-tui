use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::playback_cache::DEFAULT_PLAYBACK_CACHE_BYTES;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub auth: AuthConfig,
    pub download: DownloadConfig,
    pub library: LibraryConfig,
    pub tag: TagConfig,
    pub acoustid: AcoustidConfig,
    pub organize: OrganizeConfig,
    pub playback_cache: PlaybackCacheConfig,
    pub ui: UiConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub music_u: String,
    pub csrf: String,
    pub method: String,
    pub session_file: PathBuf,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            music_u: String::new(),
            csrf: String::new(),
            method: "auto".to_string(),
            session_file: ".ncm_session".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    pub dir: PathBuf,
    pub playlist_id: u64,
    pub max_workers: usize,
    pub timeout: u64,
    pub api_qps: f64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            dir: "./downloads".into(),
            playlist_id: 0,
            max_workers: 4,
            timeout: 30,
            api_qps: 0.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PlaybackCacheConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

impl Default for PlaybackCacheConfig {
    fn default() -> Self {
        Self {
            dir: "./.ncm-cache/playback".into(),
            max_bytes: DEFAULT_PLAYBACK_CACHE_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
    pub dirs: Vec<PathBuf>,
    pub scan_before_download: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct TagConfig {
    pub dirs: Vec<PathBuf>,
    pub use_fingerprint: bool,
    pub overwrite: bool,
    pub dry_run: bool,
}

impl Default for TagConfig {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            use_fingerprint: true,
            overwrite: false,
            dry_run: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AcoustidConfig {
    pub api_key: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct OrganizeConfig {
    pub dir: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub hide_lyrics: bool,
}

impl Default for OrganizeConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
        }
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref();

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)?;
        let config = toml::from_str(&content)?;

        Ok(config)
    }
}
