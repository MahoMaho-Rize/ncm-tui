//! Stable NCM-metadata-driven library layout. No duplicate detection is performed.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrganizableTrack {
    pub id: u64,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub album_artist: String,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub total_tracks: Option<u32>,
    pub format: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MoveOutcome {
    Moved {
        from: PathBuf,
        to: PathBuf,
        bytes: u64,
    },
    Skipped(PathBuf),
    Conflict(PathBuf),
}

#[derive(Debug, Error)]
pub enum OrganizerError {
    #[error("file operation failed: {0}")]
    Io(#[from] io::Error),
}

pub fn destination(root: &Path, track: &OrganizableTrack) -> PathBuf {
    let album_artist = nonempty(&track.album_artist, &track.artists, "Unknown Artist");
    let album = nonempty(&track.album, "Unknown Album", "Unknown Album");
    let width = track
        .total_tracks
        .map(|value| value.max(1).ilog10() as usize + 1)
        .unwrap_or(2)
        .max(2);
    let number = track.track_number.unwrap_or(0);
    let filename = format!(
        "{number:0width$} {} [{}].{}",
        sanitize(&track.title),
        track.id,
        safe_extension(&track.format),
        width = width
    );
    let mut directory = root.join(sanitize(album_artist)).join(sanitize(album));
    if track.disc_number.unwrap_or(1) > 1 {
        directory = directory.join(format!("CD{:02}", track.disc_number.unwrap_or(1)));
    }
    directory.join(filename)
}

pub fn move_track(source: &Path, target: &Path) -> Result<MoveOutcome, OrganizerError> {
    if source == target {
        return Ok(MoveOutcome::Skipped(target.to_path_buf()));
    }
    if target.exists() {
        return Ok(MoveOutcome::Conflict(target.to_path_buf()));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    move_one(source, target)?;
    let source_lyrics = source.with_extension("lrc");
    let target_lyrics = target.with_extension("lrc");
    if source_lyrics.is_file()
        && !target_lyrics.exists()
        && let Err(error) = move_one(&source_lyrics, &target_lyrics)
    {
        let _ = move_one(target, source);
        return Err(error.into());
    }
    let bytes = fs::metadata(target)?.len();
    Ok(MoveOutcome::Moved {
        from: source.to_path_buf(),
        to: target.to_path_buf(),
        bytes,
    })
}

pub fn rollback_move(from: &Path, to: &Path) {
    let _ = move_one(to, from);
    let to_lyrics = to.with_extension("lrc");
    if to_lyrics.is_file() {
        let _ = move_one(&to_lyrics, &from.with_extension("lrc"));
    }
}

fn move_one(source: &Path, target: &Path) -> io::Result<()> {
    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) => {
            let temporary = target.with_extension(format!(
                "{}.ncm-moving",
                target
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("tmp")
            ));
            let mut input = fs::File::open(source)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            io::copy(&mut input, &mut output)?;
            output.flush()?;
            output.sync_all()?;
            fs::rename(&temporary, target)?;
            fs::remove_file(source)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn is_cross_device(error: &io::Error) -> bool {
    error.raw_os_error() == Some(18)
}

#[cfg(windows)]
fn is_cross_device(error: &io::Error) -> bool {
    error.raw_os_error() == Some(17)
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device(_error: &io::Error) -> bool {
    false
}

fn nonempty<'a>(first: &'a str, second: &'a str, fallback: &'a str) -> &'a str {
    if !first.trim().is_empty() {
        first
    } else if !second.trim().is_empty() {
        second
    } else {
        fallback
    }
}

pub fn sanitize(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'..='\u{1f}' => '_',
            _ => character,
        })
        .collect::<String>();
    let value = value.trim().trim_end_matches(['.', ' ']);
    let value = if value.is_empty() { "Unknown" } else { value };
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if reserved.iter().any(|name| value.eq_ignore_ascii_case(name)) {
        format!("_{value}")
    } else {
        value.chars().take(120).collect()
    }
}

pub fn safe_extension(value: &str) -> &str {
    match value.to_ascii_lowercase().as_str() {
        "flac" => "flac",
        "ogg" | "vorbis" => "ogg",
        "m4a" | "mp4" | "aac" => "m4a",
        "wav" => "wav",
        _ => "mp3",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: PathBuf) -> OrganizableTrack {
        OrganizableTrack {
            id: 186016,
            title: "晴天".into(),
            artists: "周杰伦".into(),
            album: "叶惠美".into(),
            album_artist: "周杰伦".into(),
            track_number: Some(4),
            disc_number: Some(1),
            total_tracks: Some(11),
            format: "FLAC".into(),
            path,
        }
    }

    #[test]
    fn builds_stable_metadata_path() {
        assert_eq!(
            destination(Path::new("music"), &track(PathBuf::new())),
            Path::new("music/周杰伦/叶惠美/04 晴天 [186016].flac")
        );
        assert_eq!(sanitize("A/B: C?"), "A_B_ C_");
    }

    #[test]
    fn moves_audio_and_lyrics_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("song.flac");
        let target = directory.path().join("artist/album/song.flac");
        fs::write(&source, b"audio").unwrap();
        fs::write(source.with_extension("lrc"), b"lyrics").unwrap();
        assert!(matches!(
            move_track(&source, &target).unwrap(),
            MoveOutcome::Moved { .. }
        ));
        assert_eq!(fs::read(&target).unwrap(), b"audio");
        assert_eq!(fs::read(target.with_extension("lrc")).unwrap(), b"lyrics");
        let other = directory.path().join("other.flac");
        fs::write(&other, b"other").unwrap();
        assert!(matches!(
            move_track(&other, &target).unwrap(),
            MoveOutcome::Conflict(_)
        ));
        assert_eq!(fs::read(target).unwrap(), b"audio");
    }
}
