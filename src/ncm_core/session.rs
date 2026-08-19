use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use super::Result;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionConfig {
    pub music_u: String,
    pub csrf_token: String,
    pub os: String,
    pub appver: String,
    pub osver: String,
    pub channel: String,
    pub device_id: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            music_u: String::new(),
            csrf_token: String::new(),
            os: "iPhone OS".to_owned(),
            appver: "10.0.0".to_owned(),
            osver: "16.2".to_owned(),
            channel: "distribution".to_owned(),
            device_id: "pyncm!".to_owned(),
        }
    }
}

impl SessionConfig {
    pub fn with_cookie(music_u: impl Into<String>, csrf_token: impl Into<String>) -> Self {
        Self {
            music_u: music_u.into(),
            csrf_token: csrf_token.into(),
            ..Self::default()
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read(path)?;
        Ok(serde_json::from_slice(&content)?)
    }

    /// Persists credentials through a private temporary file and atomic rename.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, serde_json::to_vec(self)?)?;
        set_private_permissions(&temporary)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn session_round_trip_preserves_credentials() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.json");
        let session = SessionConfig::with_cookie("secret", "csrf");
        session.save(&path).unwrap();

        let restored = SessionConfig::load(path).unwrap();
        assert_eq!(restored.music_u, "secret");
        assert_eq!(restored.csrf_token, "csrf");
    }
}
