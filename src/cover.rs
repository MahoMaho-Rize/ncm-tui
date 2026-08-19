//! Cover art: terminal graphics protocol (Kitty/Sixel) with half-block fallback.
//!
//! Picker setup matches pigma: query stdio after entering the alternate screen,
//! then force Kitty/Sixel from the environment when the query is conservative.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use image::imageops;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use lofty::picture::PictureType;
use lofty::prelude::TaggedFileExt;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};
use ratatui_image::{
    Resize, StatefulImage,
    picker::{Capability, Picker, ProtocolType},
    protocol::StatefulProtocol,
};

pub fn build_picker() -> Picker {
    #[cfg(test)]
    {
        Picker::from_fontsize((10, 20))
    }
    #[cfg(not(test))]
    {
        query_picker()
    }
}

/// Query font size and graphics capability. Call after entering the alt screen
/// and before reading terminal events, same as ratatui-image / pigma.
pub fn query_picker() -> Picker {
    let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((10, 20)));
    // Only upgrade when the query actually saw a protocol or a cell size.
    // Forcing Kitty with a guessed font size makes unicode placeholders
    // invisible and hides the half-block fallback.
    if picker.protocol_type() == ProtocolType::Halfblocks {
        let seen_kitty = picker
            .capabilities()
            .iter()
            .any(|cap| matches!(cap, Capability::Kitty | Capability::CellSize(Some(_))));
        let seen_sixel = picker
            .capabilities()
            .iter()
            .any(|cap| matches!(cap, Capability::Sixel));
        if kitty_available() && seen_kitty {
            picker.set_protocol_type(ProtocolType::Kitty);
        } else if sixel_available() && seen_sixel {
            picker.set_protocol_type(ProtocolType::Sixel);
        }
    }
    picker
}

pub fn uses_terminal_graphics(picker: &Picker) -> bool {
    !matches!(picker.protocol_type(), ProtocolType::Halfblocks)
}

fn kitty_available() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("KITTY_PID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return true;
    }
    if let Ok("kitty" | "ghostty" | "rio" | "WezTerm") = std::env::var("TERM_PROGRAM").as_deref() {
        return true;
    }
    std::env::var("TERM").ok().is_some_and(|term| {
        let term = term.to_ascii_lowercase();
        term.contains("kitty") || term == "xterm-ghostty"
    })
}

fn sixel_available() -> bool {
    std::env::var_os("FOOT_VERSION").is_some()
        || std::env::var("TERM").ok().is_some_and(|term| {
            let term = term.to_ascii_lowercase();
            term.starts_with("foot") || term.starts_with("mlterm")
        })
}

pub fn ncm_thumb_url(url: &str) -> String {
    let url = url
        .strip_prefix("http://")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| url.to_owned());
    if url.contains('?') {
        format!("{url}&param=400y400")
    } else {
        format!("{url}?param=400y400")
    }
}

pub fn nonempty_url(url: &str) -> Option<String> {
    let url = url.trim();
    (!url.is_empty() && url.starts_with("http")).then(|| url.to_owned())
}

pub fn looks_like_image(bytes: &[u8]) -> bool {
    image::load_from_memory(bytes).is_ok()
}

#[derive(Clone, Debug, Default)]
pub struct CoverCache {
    dir: Option<PathBuf>,
    memory: Arc<Mutex<HashMap<u64, Vec<u8>>>>,
    misses: Arc<Mutex<HashMap<u64, ()>>>,
}

impl CoverCache {
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir: Some(dir),
            memory: Arc::new(Mutex::new(HashMap::new())),
            misses: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get(&self, song_id: u64) -> Option<Vec<u8>> {
        if let Some(bytes) = self.memory.lock().ok()?.get(&song_id).cloned() {
            return Some(bytes);
        }
        let path = self.image_path(song_id)?;
        let bytes = std::fs::read(path)
            .ok()
            .filter(|bytes| looks_like_image(bytes))?;
        if let Ok(mut memory) = self.memory.lock() {
            memory.insert(song_id, bytes.clone());
        }
        Some(bytes)
    }

    pub fn put(&self, song_id: u64, bytes: &[u8]) {
        if !looks_like_image(bytes) {
            return;
        }
        if let Ok(mut memory) = self.memory.lock() {
            memory.insert(song_id, bytes.to_vec());
        }
        if let Ok(mut misses) = self.misses.lock() {
            misses.remove(&song_id);
        }
        let Some(path) = self.image_path(song_id) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
        if let Some(miss) = self.miss_path(song_id) {
            let _ = std::fs::remove_file(miss);
        }
    }

    pub fn is_miss(&self, song_id: u64) -> bool {
        if self
            .misses
            .lock()
            .ok()
            .is_some_and(|misses| misses.contains_key(&song_id))
        {
            return true;
        }
        self.miss_path(song_id).is_some_and(|path| path.is_file())
    }

    pub fn remember_miss(&self, song_id: u64) {
        if let Ok(mut misses) = self.misses.lock() {
            misses.insert(song_id, ());
        }
        if let Some(path) = self.miss_path(song_id) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, []);
        }
    }

    fn image_path(&self, song_id: u64) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{song_id}.img")))
    }

    fn miss_path(&self, song_id: u64) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{song_id}.miss")))
    }
}

pub fn protocol_from_bytes(picker: &Picker, bytes: &[u8]) -> Option<StatefulProtocol> {
    Some(picker.new_resize_protocol(square_cover(bytes)?))
}

fn square_cover(bytes: &[u8]) -> Option<DynamicImage> {
    let image = image::load_from_memory(bytes).ok()?;
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let size = width.min(height);
    let x = (width - size) / 2;
    let y = (height - size) / 2;
    Some(image.crop_imm(x, y, size, size))
}

pub fn render_protocol(frame: &mut Frame, area: Rect, protocol: &mut StatefulProtocol) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Scale fills the reserved slot. Fit never upscales a 200–400px thumb, so
    // the image sat at the top of the leftover and the L-corner looked empty.
    frame.render_stateful_widget(
        StatefulImage::new().resize(Resize::Scale(Some(FilterType::Triangle))),
        area,
        protocol,
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverArt {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Line<'static>>,
}

pub fn picture_bytes(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    let pictures = file
        .tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .collect::<Vec<_>>();
    let picture = pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())?;
    let data = picture.data();
    (!data.is_empty()).then(|| data.to_vec())
}

pub fn from_audio(path: &Path, cols: u16, rows: u16) -> Option<CoverArt> {
    from_bytes(&picture_bytes(path)?, cols, rows)
}

/// Fit a roughly square cover into `max_cols` × `max_rows` cells.
/// Half-blocks are twice as tall as they are wide, so columns ≈ 2 × rows.
pub fn fit(bytes: &[u8], max_cols: u16, max_rows: u16) -> Option<CoverArt> {
    if max_cols == 0 || max_rows == 0 {
        return None;
    }
    let cols = max_cols.min(max_rows.saturating_mul(2)).max(1);
    from_bytes(bytes, cols, max_rows)
}

pub fn from_bytes(bytes: &[u8], cols: u16, rows: u16) -> Option<CoverArt> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let image = image::load_from_memory(bytes).ok()?.to_rgb8();
    let width = u32::from(cols.max(1));
    let height = u32::from(rows.saturating_mul(2).max(2));
    let resized = imageops::resize(&image, width, height, FilterType::Triangle);
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(cols as usize);
        let y = u32::from(row) * 2;
        for col in 0..cols {
            let x = u32::from(col);
            let top = resized.get_pixel(x.min(resized.width().saturating_sub(1)), y);
            let bottom = resized.get_pixel(
                x.min(resized.width().saturating_sub(1)),
                (y + 1).min(resized.height().saturating_sub(1)),
            );
            spans.push(Span::styled(
                "▀",
                Style::default()
                    .fg(Color::Rgb(top[0], top[1], top[2]))
                    .bg(Color::Rgb(bottom[0], bottom[1], bottom[2])),
            ));
        }
        lines.push(Line::from(spans));
    }
    Some(CoverArt { cols, rows, lines })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, ImageEncoder};

    fn rgb_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(pixels, width, height, ExtendedColorType::Rgb8)
            .unwrap();
        png
    }

    #[test]
    fn cover_cache_round_trips_bytes_and_remembers_misses() {
        let directory = tempfile::tempdir().unwrap();
        let cache = CoverCache::open(directory.path());
        let png = rgb_png(2, 2, &[200, 40, 80, 200, 40, 80, 10, 180, 90, 10, 180, 90]);
        cache.put(42, &png);
        assert_eq!(cache.get(42).as_deref(), Some(png.as_slice()));
        assert!(!cache.is_miss(42));
        cache.remember_miss(7);
        assert!(cache.is_miss(7));
        assert!(cache.get(7).is_none());
    }

    #[test]
    fn thumb_url_upgrades_http_and_appends_size() {
        assert_eq!(
            ncm_thumb_url("http://p1.music.126.net/a.jpg"),
            "https://p1.music.126.net/a.jpg?param=400y400"
        );
        assert_eq!(
            ncm_thumb_url("https://p1.music.126.net/a.jpg?foo=1"),
            "https://p1.music.126.net/a.jpg?foo=1&param=400y400"
        );
        assert_eq!(nonempty_url("  "), None);
        assert_eq!(
            nonempty_url("https://p1.music.126.net/a.jpg"),
            Some("https://p1.music.126.net/a.jpg".into())
        );
    }

    #[test]
    fn renders_half_block_grid_for_solid_bytes() {
        let png = rgb_png(2, 2, &[200, 40, 80, 200, 40, 80, 10, 180, 90, 10, 180, 90]);
        let art = from_bytes(&png, 2, 1).unwrap();
        assert_eq!(art.cols, 2);
        assert_eq!(art.rows, 1);
        assert_eq!(art.lines.len(), 1);
        assert_eq!(art.lines[0].spans.len(), 2);
        assert_eq!(art.lines[0].spans[0].content, "▀");
    }

    #[test]
    fn fit_keeps_square_cover_from_overflowing_the_slot() {
        let png = rgb_png(4, 4, &[180; 48]);
        let art = fit(&png, 18, 6).unwrap();
        assert_eq!(art.cols, 12);
        assert_eq!(art.rows, 6);
    }
}
