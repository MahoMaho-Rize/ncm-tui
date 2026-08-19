use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use lofty::prelude::{Accessor, AudioFile, TaggedFileExt};
use lofty::tag::ItemKey;

use crate::database::{DatabaseError, ScannedFile};

const AUDIO_EXTENSIONS: &[&str] = &["flac", "mp3", "wav", "ogg", "m4a", "aac", "wma", "ape"];

pub(crate) fn collect(
    roots: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<ScannedFile>), DatabaseError> {
    let mut normalized_roots = Vec::with_capacity(roots.len());
    let mut files = Vec::new();
    for root in roots {
        let root = fs::canonicalize(root)?;
        if root.is_dir() {
            walk(&root, &mut files)?;
            normalized_roots.push(root);
        } else if root.is_file() {
            if let Some(file) = describe(&root)? {
                files.push(file);
            }
            normalized_roots.push(root);
        }
    }
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    files.dedup_by(|left, right| left.path == right.path);
    Ok((normalized_roots, files))
}

fn walk(directory: &Path, files: &mut Vec<ScannedFile>) -> Result<(), DatabaseError> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        if is_hidden(&entry.file_name()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, files)?;
        } else if file_type.is_file()
            && let Some(file) = describe(&path)?
        {
            files.push(file);
        }
    }
    Ok(())
}

fn describe(path: &Path) -> Result<Option<ScannedFile>, DatabaseError> {
    let Some(format) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Ok(None);
    };
    if !AUDIO_EXTENSIONS.contains(&format.as_str()) {
        return Ok(None);
    }
    let metadata = fs::metadata(path)?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let (fallback_artists, fallback_title) = parse_stem(stem);
    let tags = read_audio_metadata(path);
    Ok(Some(ScannedFile {
        path: path.to_path_buf(),
        file_size: metadata.len(),
        modified_ns,
        format,
        title: non_empty(tags.title, fallback_title),
        artists: non_empty(tags.artists, fallback_artists),
        album: tags.album,
        duration_ms: tags.duration_ms,
        album_artist: tags.album_artist,
        release_year: tags.release_year,
        track_number: tags.track_number,
        disc_number: tags.disc_number,
        bitrate: tags.bitrate,
    }))
}

#[derive(Default)]
struct AudioMetadata {
    title: String,
    artists: String,
    album: String,
    duration_ms: u64,
    album_artist: String,
    release_year: Option<i32>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    bitrate: u64,
}

fn read_audio_metadata(path: &Path) -> AudioMetadata {
    let Ok(file) = lofty::read_from_path(path) else {
        return AudioMetadata::default();
    };
    let tag = file.primary_tag().or_else(|| file.first_tag());
    AudioMetadata {
        title: tag
            .and_then(Accessor::title)
            .unwrap_or_default()
            .trim()
            .to_owned(),
        artists: tag
            .and_then(Accessor::artist)
            .unwrap_or_default()
            .trim()
            .to_owned(),
        album: tag
            .and_then(Accessor::album)
            .unwrap_or_default()
            .trim()
            .to_owned(),
        duration_ms: file
            .properties()
            .duration()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        album_artist: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumArtist))
            .unwrap_or_default()
            .trim()
            .to_owned(),
        release_year: tag
            .and_then(Accessor::date)
            .map(|date| i32::from(date.year)),
        track_number: tag.and_then(Accessor::track),
        disc_number: tag.and_then(Accessor::disk),
        bitrate: file.properties().audio_bitrate().unwrap_or_default() as u64,
    }
}

fn non_empty(preferred: String, fallback: String) -> String {
    if preferred.is_empty() {
        fallback
    } else {
        preferred
    }
}

fn parse_stem(stem: &str) -> (String, String) {
    match stem.split_once(" - ") {
        Some((artists, title)) if !artists.trim().is_empty() && !title.trim().is_empty() => {
            (artists.trim().to_owned(), title.trim().to_owned())
        }
        _ => (String::new(), stem.trim().to_owned()),
    }
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_supported_audio_and_ignores_hidden_items() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("Artist - Song.mp3"), b"audio").unwrap();
        fs::write(directory.path().join("notes.txt"), b"text").unwrap();
        fs::create_dir(directory.path().join(".cache")).unwrap();
        fs::write(directory.path().join(".cache/hidden.flac"), b"audio").unwrap();

        let (_, files) = collect(&[directory.path().to_path_buf()]).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].artists, "Artist");
        assert_eq!(files[0].title, "Song");
        assert_eq!(files[0].album, "");
        assert_eq!(files[0].duration_ms, 0);
        assert_eq!(files[0].bitrate, 0);
    }

    #[test]
    fn metadata_values_override_filename_fallbacks() {
        assert_eq!(non_empty("22/7".to_owned(), "227".to_owned()), "22/7");
        assert_eq!(non_empty(String::new(), "Fallback".to_owned()), "Fallback");
    }
}
