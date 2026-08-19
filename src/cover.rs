//! Cover art: Kitty graphics when available, half-blocks otherwise.

use std::path::Path;

use image::imageops;
use image::imageops::FilterType;
use lofty::prelude::TaggedFileExt;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};
use ratatui_image::{
    StatefulImage,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

pub fn kitty_enabled() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM")
            .ok()
            .is_some_and(|term| term.contains("kitty"))
}

pub fn kitty_picker() -> Option<Picker> {
    if !kitty_enabled() {
        return None;
    }
    let mut picker = Picker::from_fontsize((8, 16));
    picker.set_protocol_type(ProtocolType::Kitty);
    Some(picker)
}

pub fn kitty_protocols(
    picker: &Picker,
    bytes: &[u8],
) -> Option<(StatefulProtocol, StatefulProtocol)> {
    let image = image::load_from_memory(bytes).ok()?;
    Some((
        picker.new_resize_protocol(image.clone()),
        picker.new_resize_protocol(image),
    ))
}

pub fn render_kitty(frame: &mut Frame, area: Rect, protocol: &mut StatefulProtocol) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_stateful_widget(StatefulImage::default(), area, protocol);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverArt {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Line<'static>>,
}

pub fn picture_bytes(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    Some(tag.pictures().first()?.data().to_vec())
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
