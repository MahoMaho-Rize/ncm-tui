#![allow(unused_imports)]
//NetEase Cloud Music terminal frontend.

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use qrcode::{QrCode, render::unicode};
use rand::Rng;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, LineGauge, List, ListItem, ListState, Paragraph,
        Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::ncm_core::api::user::ListeningRank;
use crate::{
    auth::{Authentication, Identity, QrChallenge, QrStatus},
    discovery::{
        AlbumSummary, ArtistSummary, Discovery, OnlineTrack, PlaylistScope, PlaylistSummary,
        RankedTrack, SearchKind, SearchPage, TrackPage, liked_playlist,
    },
    download::{
        AudioQuality, DownloadReport, DownloadRequest, DownloadSource, Downloader, TrackSelection,
    },
    library::{Library, LibraryStats, ScanReport, Track, TrackSort, TrackView},
    lyrics::{LyricLine, Lyrics},
    organizer::MoveOutcome,
    pagination::{self, PAGE_SIZE, PaginationInfo},
    palette::{self, PaletteItem},
    playback_cache::{CacheStats, ClearReport, PlaybackCache},
    player::{Player, PlayerState},
    streaming::PreparedStream,
};

use super::*;

pub(super) fn paint_frame(frame: &mut Frame, app: &mut App) {
    let status_width = text_width(&player_status(app.player_state()));
    app.hits = calculate_hits(
        LayoutRequest {
            area: frame.area(),
            column_count: 1 + app.columns.len(),
            focus: app.focus,
            lyrics_hidden: app.lyrics_hidden,
            expanded: app.expanded,
        },
        status_width,
        text_width(account_label(app)),
        app.cover_bytes.is_some() || app.cover_protocol_nav.is_some() || app.cover_nav.is_some(),
    );
    for hit in &mut app.hits.columns {
        let selected = if hit.index == 0 {
            app.selected
        } else {
            app.columns
                .get(hit.index - 1)
                .map_or(0, |column| column.selected)
        };
        hit.offset = content_offset(selected, hit.area.height);
        if hit.index == 0 {
            app.hits.content_offset = hit.offset;
        }
    }
    draw(frame, app);
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.background)),
        area,
    );
    let Some(layout) = layout_for(app, area) else {
        draw_too_small(frame, app, area);
        return;
    };
    draw_header(frame, app, layout.header);
    for pane in &layout.browser {
        if pane.index == 0 {
            draw_content(frame, app, pane.area);
        } else {
            draw_browser_column(frame, app, pane.area, pane.index - 1);
        }
    }
    if let Some(lyrics) = layout.lyrics {
        draw_lyrics(frame, app, lyrics);
    }
    if let Some(navigation) = layout.navigation {
        draw_navigation(frame, app, navigation);
    }
    draw_player(frame, app, layout.player);
    if let Some(navigation) = layout.navigation {
        // Cover last so the player chrome cannot wipe Kitty placeholders.
        draw_cover_slot(frame, app, navigation);
    }
    draw_footer(frame, app, layout.footer);
    if app.show_help {
        draw_help(frame, app, area);
    }
    if app.show_details {
        draw_details_overlay(frame, app, area);
    }
    if app.mode == InputMode::Palette {
        draw_palette(frame, app, area);
    }
    draw_toasts(frame, app, area);
}

pub(super) fn draw_too_small(frame: &mut Frame, app: &App, area: Rect) {
    let message = format!(
        "终端太小  {}×{}\n至少需要 {MIN_WIDTH}×{MIN_HEIGHT}",
        area.width, area.height
    );
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(Style::default().fg(app.theme.text))
            .block(panel(app, " NCM ", false)),
        centered(area, 36, 7),
    );
}

pub(super) fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let user = account_label(app);
    let prefix = "  ♫  ";
    let breadcrumb = breadcrumb_text(app);
    let available = area
        .width
        .saturating_sub(text_width(prefix))
        .saturating_sub(text_width(user))
        .saturating_sub(3);
    let breadcrumb = truncate_to_width(&breadcrumb, available);
    let left = Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            breadcrumb,
            Style::default()
                .fg(app.theme.text)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(
        Paragraph::new(user)
            .alignment(Alignment::Right)
            .style(Style::default().fg(app.theme.muted)),
        Rect::new(area.x, area.y, area.width.saturating_sub(2), 1),
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(app.theme.border)),
        area,
    );
}

pub(super) fn draw_navigation(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner = inner_rect(area);
    let has_cover =
        app.cover_bytes.is_some() || app.cover_protocol_nav.is_some() || app.cover_nav.is_some();
    let cover = navigation_cover_area(area, has_cover);
    let list_height = if cover.height == 0 {
        inner.height
    } else {
        cover.y.saturating_sub(inner.y)
    };
    let grouped = grouped_navigation(list_height);
    let mut previous = "";
    let mut items = Vec::new();
    for (index, route) in Route::ALL.iter().enumerate() {
        if grouped && route.group() != previous {
            items.push(ListItem::new(Line::from(Span::styled(
                format!(" {}", route.group()),
                Style::default()
                    .fg(app.theme.muted)
                    .add_modifier(Modifier::BOLD),
            ))));
            previous = route.group();
        }
        let selected = index == app.nav_selected;
        let active = app.focus == Focus::Navigation;
        let marker = if selected && active {
            "▶"
        } else if selected {
            "○"
        } else {
            " "
        };
        let style = if selected && active {
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default()
                .fg(app.theme.muted)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(app.theme.text)
        };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(" {marker} {}", route.label()),
            style,
        ))));
    }
    frame.render_widget(panel(app, " 音乐 ", app.focus == Focus::Navigation), area);
    let list_area = Rect::new(inner.x, inner.y, inner.width, list_height);
    frame.render_widget(List::new(items), list_area);
}

pub(super) fn draw_content(frame: &mut Frame, app: &App, area: Rect) {
    if app.route == Route::Account {
        draw_account(frame, app, area);
        return;
    }
    if app.route == Route::Downloads {
        draw_downloads(frame, app, area);
        return;
    }
    let border_title = if app.loading {
        format!(" {} 正在加载 ", SPINNER[app.tick % SPINNER.len()])
    } else if app.route == Route::Search {
        format!(
            " {} · F1 歌曲 F2 专辑 F3 歌手 F4 歌单 ",
            search_kind_label(app.search_kind)
        )
    } else if app.route == Route::Local {
        app.local_stats.map_or_else(
            || format!(" {} ", app.title),
            |stats| {
                format!(
                    " {} · {} 首 · {} 张专辑 · {} ",
                    app.title,
                    stats.tracks,
                    stats.albums,
                    format_library_duration(stats.duration_ms)
                )
            },
        )
    } else {
        format!(" {} ", app.title)
    };
    match &app.content {
        Content::Tracks(tracks)
            if app.route == Route::Local
                && tracks.is_empty()
                && !app.loading
                && app.mode != InputMode::LocalSearch =>
        {
            draw_local_empty(frame, app, area, &border_title);
        }
        Content::Tracks(tracks) => draw_tracks(
            frame,
            app,
            area,
            tracks,
            &border_title,
            app.selected,
            app.focus == Focus::Content,
            app.pagination,
        ),
        Content::Playlists(playlists) => draw_playlists(
            frame,
            app,
            area,
            playlists,
            &border_title,
            app.selected,
            app.focus == Focus::Content,
        ),
        Content::Artists(artists) => draw_artists(
            frame,
            app,
            area,
            artists,
            &border_title,
            app.selected,
            app.focus == Focus::Content,
        ),
        Content::Albums(albums) => draw_albums(
            frame,
            app,
            area,
            albums,
            &border_title,
            app.selected,
            app.focus == Focus::Content,
        ),
        Content::LocalMenu(items) => {
            let items = items
                .iter()
                .map(|item| ListItem::new(Line::from(Span::raw(format!(" {}", item.label())))))
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(Some(app.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(panel(app, &border_title, app.focus == Focus::Content))
                    .highlight_style(selection_style(app, app.focus == Focus::Content)),
                area,
                &mut state,
            );
        }
        Content::Empty => {
            if app.route == Route::Local && !app.loading && app.mode != InputMode::LocalSearch {
                draw_local_empty(frame, app, area, &border_title);
            } else {
                let message = if app.loading {
                    SPINNER[app.tick % SPINNER.len()].to_string()
                } else if app.mode == InputMode::LocalSearch {
                    format!("搜索本地音乐：{}", app.input)
                } else if app.mode == InputMode::Search {
                    format!("搜索网易云：{}", app.input)
                } else {
                    empty_message(app)
                };
                frame.render_widget(
                    Paragraph::new(message)
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(app.theme.muted))
                        .block(panel(app, &border_title, app.focus == Focus::Content))
                        .wrap(Wrap { trim: true }),
                    area,
                );
            }
        }
    }
}

pub(super) fn draw_browser_column(frame: &mut Frame, app: &App, area: Rect, column_index: usize) {
    let Some(column) = app.columns.get(column_index) else {
        return;
    };
    let focused = app.focus == Focus::Column(column_index);
    let title = if column.is_loading() {
        format!(" {} {} ", SPINNER[app.tick % SPINNER.len()], column.title)
    } else {
        format!(" {} ", column.title)
    };
    match &column.content {
        Content::Tracks(values) => draw_tracks(
            frame,
            app,
            area,
            values,
            &title,
            column.selected,
            focused,
            column.pagination,
        ),
        Content::Playlists(values) => {
            draw_playlists(frame, app, area, values, &title, column.selected, focused)
        }
        Content::Artists(values) => {
            draw_artists(frame, app, area, values, &title, column.selected, focused)
        }
        Content::Albums(values) => {
            draw_albums(frame, app, area, values, &title, column.selected, focused)
        }
        Content::LocalMenu(_) => {
            frame.render_widget(Paragraph::new("").block(panel(app, &title, focused)), area);
        }
        Content::Empty => {
            let (message, style) = match &column.phase {
                ColumnPhase::Loading => (
                    format!("{} 正在加载", SPINNER[app.tick % SPINNER.len()]),
                    Style::default().fg(app.theme.muted),
                ),
                ColumnPhase::Ready => ("暂无内容".to_owned(), Style::default().fg(app.theme.muted)),
                ColumnPhase::Error(error) => (
                    format!("加载失败\n{error}\n\nEnter 或 r 重试 · Esc 返回"),
                    Style::default().fg(Color::LightRed),
                ),
            };
            frame.render_widget(
                Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .style(style)
                    .block(panel(app, &title, focused))
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
    }
}

pub(super) fn draw_lyrics(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .current_track
        .as_ref()
        .map(|track| format!(" 歌词 · {} — {} ", track.title, track.artists))
        .unwrap_or_else(|| " 歌词 ".to_owned());
    let block = panel(app, &title, app.focus == Focus::Lyrics);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let current_id = app.current;
    match &app.lyrics {
        LyricsState::Idle => draw_lyrics_message(frame, app, inner, "播放歌曲后显示歌词"),
        LyricsState::Loading(song_id) if current_id == Some(*song_id) => draw_lyrics_message(
            frame,
            app,
            inner,
            &format!("{} 正在加载歌词", SPINNER[app.tick % SPINNER.len()]),
        ),
        LyricsState::Error(song_id, error) if current_id == Some(*song_id) => {
            draw_lyrics_message(frame, app, inner, &format!("歌词加载失败 · {error}"))
        }
        LyricsState::Ready(song_id, lyrics) if current_id == Some(*song_id) => {
            if lyrics.is_empty() {
                draw_lyrics_message(frame, app, inner, "纯音乐，请欣赏");
                return;
            }
            draw_lyrics_body(frame, app, inner, lyrics);
        }
        _ => draw_lyrics_message(frame, app, inner, "正在切换歌曲…"),
    }
}

pub(super) fn draw_lyrics_body(frame: &mut Frame, app: &App, area: Rect, lyrics: &Lyrics) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let position = app.player_state().elapsed;
    let active = lyrics.current_index(position);
    let anchor = active.unwrap_or(0);
    let mut rows = Vec::new();
    let mut active_row = 0_usize;
    let radius = usize::from(area.height).max(1);
    let start = anchor.saturating_sub(radius);
    let end = anchor.saturating_add(radius + 1).min(lyrics.original.len());
    for (offset, lyric) in lyrics.original[start..end].iter().enumerate() {
        let index = start + offset;
        let is_active = active == Some(index);
        let distance = index.abs_diff(anchor);
        let wrapped_original = wrap_styled_spans(
            lyric_spans(lyric, position, is_active, distance, app),
            area.width,
        );
        if index == anchor {
            active_row = rows.len();
        }
        for spans in wrapped_original {
            rows.push(LyricDisplayRow::Original(spans));
        }
        if let Some(translation) = lyrics.translation_at(lyric.start) {
            for line in wrap_lyric_text(translation, area.width) {
                rows.push(LyricDisplayRow::Translation(line));
            }
        }
    }
    let scroll = active_row.saturating_sub(usize::from(area.height) / 2);
    let visible = rows
        .into_iter()
        .skip(scroll)
        .take(usize::from(area.height))
        .map(|row| match row {
            LyricDisplayRow::Original(spans) => Line::from(spans),
            LyricDisplayRow::Translation(text) => {
                Line::from(Span::styled(text, Style::default().fg(app.theme.muted)))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible).alignment(Alignment::Center), area);
}

pub(super) enum LyricDisplayRow {
    Original(Vec<Span<'static>>),
    Translation(String),
}

pub(super) fn wrap_lyric_text(text: &str, width: u16) -> Vec<String> {
    wrap_units(
        text.chars().map(|character| {
            let mut encoded = [0_u8; 4];
            (character.encode_utf8(&mut encoded).to_owned(), ())
        }),
        width,
    )
    .into_iter()
    .map(|units| units.into_iter().map(|(piece, _)| piece).collect())
    .filter(|line: &String| !line.is_empty())
    .collect()
}

pub(super) fn wrap_styled_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Vec<Span<'static>>> {
    let units = spans.into_iter().flat_map(|span| {
        let style = span.style;
        span.content
            .chars()
            .map(move |character| {
                let mut encoded = [0_u8; 4];
                (character.encode_utf8(&mut encoded).to_owned(), style)
            })
            .collect::<Vec<_>>()
    });
    wrap_units(units, width)
        .into_iter()
        .map(coalesce_styled_units)
        .filter(|line| !line.is_empty())
        .collect()
}

pub(super) fn wrap_units<T: Copy>(
    units: impl IntoIterator<Item = (String, T)>,
    width: u16,
) -> Vec<Vec<(String, T)>> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = Vec::<(String, T)>::new();
    let mut current_width = 0_u16;
    let mut last_space = None;
    for (piece, extra) in units {
        let piece_width = text_width(&piece).max(1);
        if piece == " " && current.is_empty() {
            continue;
        }
        if current_width + piece_width > width && !current.is_empty() {
            if piece == " " {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                last_space = None;
                continue;
            }
            if let Some(space_at) = last_space {
                let remainder = current.split_off(space_at);
                lines.push(std::mem::take(&mut current));
                current = remainder
                    .into_iter()
                    .filter(|(value, _)| value != " ")
                    .collect();
                current_width = current.iter().map(|(value, _)| text_width(value)).sum();
                last_space = None;
            } else {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                last_space = None;
            }
        }
        if piece == " " {
            last_space = Some(current.len());
        }
        current_width = current_width.saturating_add(piece_width);
        current.push((piece, extra));
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

pub(super) fn coalesce_styled_units(units: Vec<(String, Style)>) -> Vec<Span<'static>> {
    let mut spans = Vec::<Span<'static>>::new();
    for (piece, style) in units {
        if let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(&piece);
        } else {
            spans.push(Span::styled(piece, style));
        }
    }
    spans
}

pub(super) fn draw_lyrics_message(frame: &mut Frame, app: &App, area: Rect, message: &str) {
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(Style::default().fg(app.theme.muted)),
        Rect::new(
            area.x,
            area.y.saturating_add(area.height / 2),
            area.width,
            1.min(area.height),
        ),
    );
}

pub(super) fn lyric_spans(
    line: &LyricLine,
    position: Duration,
    active: bool,
    distance: usize,
    app: &App,
) -> Vec<Span<'static>> {
    if !active {
        let color = if distance <= 1 {
            app.theme.text
        } else {
            app.theme.muted
        };
        return vec![Span::styled(
            line.text.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
    }

    let played = Style::default()
        .fg(app.theme.overlay)
        .bg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let upcoming = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    if !line.words.is_empty() {
        let mut spans = Vec::new();
        for word in &line.words {
            let progress = timed_progress(position, word.start, word.end);
            push_progress_spans(&mut spans, &word.text, progress, played, upcoming);
        }
        return spans;
    }

    let end = line
        .end
        .filter(|end| *end > line.start)
        .unwrap_or_else(|| line.start.saturating_add(Duration::from_secs(4)));
    let progress = timed_progress(position, line.start, end);
    let mut spans = Vec::new();
    push_progress_spans(&mut spans, &line.text, progress, played, upcoming);
    spans
}

pub(super) fn timed_progress(position: Duration, start: Duration, end: Duration) -> f64 {
    if position <= start {
        return 0.0;
    }
    if position >= end || end <= start {
        return 1.0;
    }
    position.saturating_sub(start).as_secs_f64() / end.saturating_sub(start).as_secs_f64()
}

pub(super) fn push_progress_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    progress: f64,
    played: Style,
    upcoming: Style,
) {
    let count = text.chars().count();
    let split = ((count as f64 * progress).floor() as usize).min(count);
    let before = text.chars().take(split).collect::<String>();
    let after = text.chars().skip(split).collect::<String>();
    if !before.is_empty() {
        spans.push(Span::styled(before, played));
    }
    if !after.is_empty() {
        spans.push(Span::styled(after, upcoming));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_tracks(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    tracks: &[TrackRow],
    title: &str,
    selected: usize,
    focused: bool,
    page: PaginationInfo,
) {
    if app.expanded {
        draw_expanded_tracks(frame, app, area, tracks, title, selected, focused, page);
        return;
    }
    let visible = inner_rect(area).height as usize;
    let offset = content_offset(selected, visible as u16);
    let items = tracks
        .iter()
        .skip(offset)
        .take(visible)
        .map(|track| {
            let lead = track_lead(app, track);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {lead}  "), Style::default().fg(app.theme.accent)),
                Span::raw(shorten(&track.title, 28)),
                Span::styled(
                    format!("  {}", shorten(&track.artists, 22)),
                    Style::default().fg(app.theme.muted),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default()
        .with_selected((!tracks.is_empty()).then_some(selected.saturating_sub(offset)));
    let title = title_with_page(title, selected, tracks.len(), page);
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(app, &title, focused))
            .highlight_style(selection_style(app, focused)),
        area,
        &mut state,
    );
    render_scrollbar(frame, app, area, tracks.len(), selected);
}

fn track_lead(app: &App, track: &TrackRow) -> &'static str {
    if app.is_playing(track) {
        "▶"
    } else if track.favorite {
        "♥"
    } else {
        " "
    }
}

pub(super) fn clip_cell(value: &str, width: u16) -> String {
    if width == 0 {
        return String::new();
    }
    if text_width(value) <= width {
        let pad = width.saturating_sub(text_width(value));
        return format!("{value}{}", " ".repeat(pad as usize));
    }
    if width == 1 {
        return "…".into();
    }
    let mut kept = String::new();
    for character in value.chars() {
        let candidate = format!("{kept}{character}");
        if text_width(&candidate) + 1 > width {
            break;
        }
        kept = candidate;
    }
    let clipped = format!("{kept}…");
    let pad = width.saturating_sub(text_width(&clipped));
    format!("{clipped}{}", " ".repeat(pad as usize))
}

fn track_column_widths(inner_width: u16) -> (u16, u16, u16, u16) {
    let duration = 5;
    let gaps = 3;
    let rest = inner_width.saturating_sub(3 + duration + gaps).max(12);
    let title = (rest * 4 / 10).max(6);
    let artist = (rest * 3 / 10).max(4);
    let album = rest.saturating_sub(title + artist).max(4);
    (title, artist, album, duration)
}

#[allow(clippy::too_many_arguments)]
fn draw_expanded_tracks(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    tracks: &[TrackRow],
    title: &str,
    selected: usize,
    focused: bool,
    page: PaginationInfo,
) {
    frame.render_widget(
        panel(
            app,
            &title_with_page(title, selected, tracks.len(), page),
            focused,
        ),
        area,
    );
    let inner = inner_rect(area);
    if inner.width < 20 || inner.height < 2 {
        return;
    }
    let (title_w, artist_w, album_w, duration_w) = track_column_widths(inner.width);
    let header_style = Style::default()
        .fg(app.theme.muted)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::raw("   "),
        Span::styled(clip_cell("歌名", title_w), header_style),
        Span::raw(" "),
        Span::styled(clip_cell("歌手", artist_w), header_style),
        Span::raw(" "),
        Span::styled(clip_cell("专辑", album_w), header_style),
        Span::raw(" "),
        Span::styled(clip_cell("时长", duration_w), header_style),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    let visible = list_area.height as usize;
    let offset = content_offset(selected, list_area.height);
    let items = tracks
        .iter()
        .skip(offset)
        .take(visible)
        .map(|track| {
            let lead = track_lead(app, track);
            let duration = format_time(Duration::from_millis(track.duration_ms));
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {lead} "), Style::default().fg(app.theme.accent)),
                Span::raw(clip_cell(&track.title, title_w)),
                Span::raw(" "),
                Span::styled(
                    clip_cell(&track.artists, artist_w),
                    Style::default().fg(app.theme.muted),
                ),
                Span::raw(" "),
                Span::styled(
                    clip_cell(&track.album, album_w),
                    Style::default().fg(app.theme.muted),
                ),
                Span::raw(" "),
                Span::styled(
                    clip_cell(&duration, duration_w),
                    Style::default().fg(app.theme.muted),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default()
        .with_selected((!tracks.is_empty()).then_some(selected.saturating_sub(offset)));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(app, focused)),
        list_area,
        &mut state,
    );
    render_scrollbar(frame, app, area, tracks.len(), selected);
}

pub(super) fn draw_playlists(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    values: &[PlaylistSummary],
    title: &str,
    selected: usize,
    focused: bool,
) {
    let visible = inner_rect(area).height as usize;
    let offset = content_offset(selected, visible as u16);
    let items = values
        .iter()
        .skip(offset)
        .take(visible)
        .map(|value| {
            ListItem::new(Line::from(vec![
                Span::styled(" ♫  ", Style::default().fg(app.theme.accent)),
                Span::raw(shorten(&value.name, 46)),
                Span::styled(
                    format!("  {} 首", value.track_count),
                    Style::default().fg(app.theme.muted),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    draw_selectable_list(
        frame,
        app,
        area,
        items,
        title,
        ListSelection {
            selected,
            focused,
            offset,
            len: values.len(),
        },
    );
}

pub(super) fn draw_albums(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    values: &[AlbumSummary],
    title: &str,
    selected: usize,
    focused: bool,
) {
    let visible = inner_rect(area).height as usize;
    let offset = content_offset(selected, visible as u16);
    let items = values
        .iter()
        .skip(offset)
        .take(visible)
        .map(|value| {
            ListItem::new(Line::from(vec![
                Span::styled(" ◉  ", Style::default().fg(app.theme.accent)),
                Span::raw(shorten(&value.name, 34)),
                Span::styled(
                    format!(
                        "  {}  ·  {} 首",
                        shorten(&value.artists, 20),
                        value.track_count
                    ),
                    Style::default().fg(app.theme.muted),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    draw_selectable_list(
        frame,
        app,
        area,
        items,
        title,
        ListSelection {
            selected,
            focused,
            offset,
            len: values.len(),
        },
    );
}

pub(super) fn draw_artists(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    values: &[ArtistSummary],
    title: &str,
    selected: usize,
    focused: bool,
) {
    let visible = inner_rect(area).height as usize;
    let offset = content_offset(selected, visible as u16);
    let items = values
        .iter()
        .skip(offset)
        .take(visible)
        .map(|value| {
            ListItem::new(Line::from(vec![
                Span::styled(" ◇  ", Style::default().fg(app.theme.accent)),
                Span::raw(shorten(&value.name, 40)),
                Span::styled(
                    format!("  {} 首 · {} 张专辑", value.music_count, value.album_count),
                    Style::default().fg(app.theme.muted),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    draw_selectable_list(
        frame,
        app,
        area,
        items,
        title,
        ListSelection {
            selected,
            focused,
            offset,
            len: values.len(),
        },
    );
}

pub(super) struct ListSelection {
    pub(super) selected: usize,
    pub(super) focused: bool,
    pub(super) offset: usize,
    pub(super) len: usize,
}

pub(super) fn draw_selectable_list(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    items: Vec<ListItem>,
    title: &str,
    selection: ListSelection,
) {
    let mut state = ListState::default().with_selected(
        (selection.len > 0).then_some(selection.selected.saturating_sub(selection.offset)),
    );
    let title = title_with_position(title, selection.selected, selection.len);
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(app, &title, selection.focused))
            .highlight_style(selection_style(app, selection.focused)),
        area,
        &mut state,
    );
    render_scrollbar(frame, app, area, selection.len, selection.selected);
}

pub(super) fn render_scrollbar(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    total: usize,
    selected: usize,
) {
    if total <= inner_rect(area).height as usize {
        return;
    }
    let mut state = ScrollbarState::new(total).position(selected);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(None)
        .thumb_symbol("│")
        .thumb_style(Style::default().fg(app.theme.accent));
    frame.render_stateful_widget(scrollbar, inner_rect(area), &mut state);
}

pub(super) fn title_with_position(title: &str, selected: usize, total: usize) -> String {
    title_with_page(title, selected, total, PaginationInfo::default())
}

pub(super) fn title_with_page(
    title: &str,
    selected: usize,
    loaded: usize,
    page: PaginationInfo,
) -> String {
    let title = title.trim();
    if loaded == 0 {
        return format!(" {title} ");
    }
    let (total, plus) = page.display_total(loaded);
    let mark = if page.has_more || plus { "+" } else { "" };
    format!(
        " {title} · {}/{}{mark} ",
        selected.min(loaded - 1) + 1,
        total
    )
}

pub(super) fn draw_account(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel(app, " 账号与登录 ", app.focus == Focus::Content);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::vertical([Constraint::Min(5), Constraint::Length(5)]).split(inner);
    let inner = sections[0];
    draw_playback_cache(frame, app, sections[1]);
    if let Some(identity) = &app.identity {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "已登录",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(identity.nickname.as_str()),
                Line::from(Span::styled(
                    format!("UID  {}", identity.user_id),
                    Style::default().fg(app.theme.muted),
                )),
            ])
            .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    if app.qr.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.login_status.is_empty() {
                "按 L 或点击这里扫码登录"
            } else {
                &app.login_status
            })
            .alignment(Alignment::Center)
            .style(Style::default().fg(app.theme.muted)),
            inner,
        );
        return;
    }
    let (width, height) = qr_dimensions(&app.qr);
    if let Some(layout) = qr_layout(inner, width, height) {
        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.qr_light)),
            layout.card,
        );
        frame.render_widget(
            Paragraph::new(app.qr.as_str()).style(
                Style::default()
                    .fg(app.theme.qr_dark)
                    .bg(app.theme.qr_light),
            ),
            layout.code,
        );
        frame.render_widget(
            Paragraph::new(app.login_status.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(app.theme.muted)),
            layout.status,
        );
    } else {
        let url = app
            .challenge
            .as_ref()
            .map(|challenge| challenge.url.as_str())
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "放大窗口以显示二维码",
                    Style::default().fg(app.theme.accent),
                )),
                Line::from(""),
                Line::from(app.login_status.as_str()),
                Line::from(""),
                Line::from(url),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

pub(super) fn draw_playback_cache(frame: &mut Frame, app: &App, area: Rect) {
    let stats = app.services.playback_cache.stats();
    let detail = format!(
        "{} / {} · {} 首",
        format_bytes(stats.used_bytes),
        format_bytes(stats.max_bytes),
        stats.entries
    );
    let hint = if app.cache_task.is_some() {
        format!("{} 缓存操作中…", SPINNER[app.tick % SPINNER.len()])
    } else if app
        .cache_clear_armed_until
        .is_some_and(|deadline| deadline >= Instant::now())
    {
        "再次按 X 确认清除 · 当前播放不会中断".into()
    } else {
        "s 设置上限 · X 清除缓存".into()
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "播放缓存",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(detail),
            Line::from(Span::styled(hint, Style::default().fg(app.theme.muted))),
        ])
        .alignment(Alignment::Center),
        area,
    );
}

pub(super) fn draw_downloads(frame: &mut Frame, app: &App, area: Rect) {
    let (headline, detail) = if let Some(job) = &app.active_job {
        let name = match job.kind {
            JobKind::Download => "正在下载",
            JobKind::Organize => "正在整理",
            JobKind::Scan => "正在扫描音乐目录",
            JobKind::Import => "正在导入本地音乐",
        };
        (
            format!("{} {name}", SPINNER[app.tick % SPINNER.len()]),
            "Esc 取消下载".to_owned(),
        )
    } else if app.mode == InputMode::Download {
        (
            format!("新建下载  {}", app.input),
            "track 347230 · album 32311 3-8 · playlist 3778678 1-20".to_owned(),
        )
    } else {
        ("新建下载".to_owned(), "按 D 或点击这里".to_owned())
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                headline,
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(detail, Style::default().fg(app.theme.muted))),
        ])
        .alignment(Alignment::Center)
        .block(panel(app, " 下载管理 ", app.focus == Focus::Content)),
        area,
    );
}

fn draw_player_clock(
    frame: &mut Frame,
    app: &App,
    layout: &PlayerLayout,
    state: &crate::player::PlayerState,
) {
    frame.render_widget(
        LineGauge::default()
            .filled_style(Style::default().fg(app.theme.accent))
            .unfilled_style(Style::default().fg(app.theme.panel_highlight))
            .line_set(ratatui::symbols::line::NORMAL)
            .ratio(state.progress)
            .label(""),
        layout.progress,
    );
    frame.render_widget(
        Paragraph::new(format_time(state.elapsed))
            .alignment(Alignment::Right)
            .style(Style::default().fg(app.theme.muted)),
        layout.elapsed,
    );
    frame.render_widget(
        Paragraph::new(format_time(state.duration)).style(Style::default().fg(app.theme.muted)),
        layout.duration,
    );
}

pub(super) fn draw_player(frame: &mut Frame, app: &mut App, area: Rect) {
    let state = app.player_state.clone();
    let status = player_status(&state);
    let layout = player_layout(area, text_width(&status), false, true);
    let icon = if state.title.is_empty() {
        "■"
    } else if state.paused {
        "▶"
    } else {
        "Ⅱ"
    };
    let title = app
        .current_track
        .as_ref()
        .map(|track| track.title.clone())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "尚未播放".into());
    let artists = app
        .current_track
        .as_ref()
        .map(|track| track.artists.clone())
        .filter(|artists| !artists.is_empty())
        .unwrap_or_else(|| "选择歌曲开始播放".into());
    let album = app
        .current_track
        .as_ref()
        .map(|track| track.album.clone())
        .unwrap_or_default();
    frame.render_widget(panel_open_left(app, " 播放器 ", false), area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{icon}  "),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                shorten(&title, 30),
                Style::default()
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {}{}",
                    shorten(&artists, 20),
                    if album.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", shorten(&album, 16))
                    }
                ),
                Style::default().fg(app.theme.muted),
            ),
        ])),
        layout.song_info,
    );
    frame.render_widget(
        Paragraph::new(status)
            .alignment(Alignment::Right)
            .style(Style::default().fg(app.theme.muted)),
        layout.volume,
    );
    draw_player_clock(frame, app, &layout, &state);
    frame.render_widget(
        Paragraph::new(format!("p {PREVIOUS_ICON}")).style(Style::default().fg(app.theme.text)),
        layout.previous,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Space {}",
            if state.paused { PLAY_ICON } else { PAUSE_ICON }
        ))
        .style(
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        layout.pause,
    );
    frame.render_widget(
        Paragraph::new(format!("{NEXT_ICON} n")).style(Style::default().fg(app.theme.text)),
        layout.next,
    );
    frame.render_widget(
        Paragraph::new(format!("m {}", app.play_mode.label()))
            .style(Style::default().fg(app.theme.accent)),
        layout.play_mode,
    );
}

pub(super) fn account_label(app: &App) -> &str {
    app.identity
        .as_ref()
        .map(|identity| identity.nickname.as_str())
        .unwrap_or("L 登录")
}

pub(super) fn context_actions(app: &App) -> Vec<UiAction> {
    if app.focus == Focus::Lyrics {
        return vec![
            UiAction::ToggleLyrics,
            UiAction::TogglePause,
            UiAction::SeekForward,
            UiAction::ToggleHelp,
        ];
    }
    if app.focus == Focus::Navigation {
        return vec![
            UiAction::SelectNext,
            UiAction::Activate,
            UiAction::FocusNext,
            UiAction::Palette,
            UiAction::ToggleHelp,
        ];
    }
    let mut actions = vec![UiAction::SelectNext, UiAction::Activate];
    match app.route {
        Route::Account => {
            if app.identity.is_none() {
                actions.push(UiAction::Login);
            }
            actions.push(UiAction::EditCacheSize);
            actions.push(UiAction::ClearCache);
        }
        Route::Downloads => actions.push(UiAction::Download),
        Route::Local => {
            actions.push(UiAction::Import);
            actions.push(UiAction::Refresh);
            if !matches!(app.local_layer, LocalLayer::Menu) {
                actions.push(UiAction::Sort);
            }
            actions.push(UiAction::Search);
        }
        Route::Queue => {
            actions.push(UiAction::Dequeue);
            actions.push(UiAction::ClearQueue);
            actions.push(UiAction::Details);
        }
        _ if app.selected_track().is_some() => {
            actions.push(UiAction::TogglePause);
            actions.push(UiAction::ToggleFavorite);
            actions.push(UiAction::Details);
        }
        _ => {}
    }
    if !matches!(app.focus, Focus::Navigation) || app.expanded {
        actions.push(UiAction::Expand);
    }
    actions.push(UiAction::ToggleHelp);
    actions
}

pub(super) fn format_action_hint(action: UiAction) -> String {
    let hint = action_hint(action);
    format!("{} {}", hint.key, hint.label)
}

pub(super) fn format_context_action_hint(app: &App, action: UiAction) -> String {
    if app.route == Route::Local && action == UiAction::Refresh {
        "r 扫描".into()
    } else if action == UiAction::Expand && app.expanded {
        "e 收起".into()
    } else {
        format_action_hint(action)
    }
}

pub(super) fn hint_line(actions: &[UiAction]) -> String {
    actions
        .iter()
        .copied()
        .map(format_action_hint)
        .collect::<Vec<_>>()
        .join("  ·  ")
}

pub(super) fn footer_text(app: &App, width: u16) -> String {
    if width == 0 {
        return String::new();
    }
    let mut actions = context_actions(app);
    actions.retain(|action| *action != UiAction::ToggleHelp);
    actions.truncate(4);
    actions.insert(0, UiAction::ToggleHelp);

    let mut result = String::new();
    for action in actions.into_iter().take(5) {
        let hint = format_context_action_hint(app, action);
        let candidate = if result.is_empty() {
            format!(" {hint}")
        } else {
            format!("{result}  {hint}")
        };
        if text_width(&candidate) <= width {
            result = candidate;
        }
    }
    if !app.status.is_empty() {
        let separator = if result.is_empty() { " " } else { "  ·  " };
        let remaining = width.saturating_sub(text_width(&result));
        if remaining > text_width(separator) {
            result.push_str(separator);
            result.push_str(&fit_text(
                &app.status,
                remaining.saturating_sub(text_width(separator)),
            ));
        }
    }
    fit_text(&result, width)
}

pub(super) fn input_footer(prefix: &str, input: &str, cursor: usize, width: u16) -> (String, u16) {
    let prefix = fit_text(prefix, width);
    let prefix_width = text_width(&prefix);
    let available = width.saturating_sub(prefix_width).saturating_sub(1);
    let cursor = cursor.min(input.len());
    let mut start = cursor;
    while start > 0 {
        let previous = previous_char_boundary(input, start);
        if text_width(&input[previous..cursor]) > available {
            break;
        }
        start = previous;
    }
    let mut end = cursor;
    while end < input.len() {
        let next = next_char_boundary(input, end);
        if text_width(&input[start..next]) > available {
            break;
        }
        end = next;
    }
    let visible = &input[start..end];
    (
        format!("{prefix}{visible}"),
        prefix_width + text_width(&input[start..cursor]),
    )
}

pub(super) fn fit_text(value: &str, width: u16) -> String {
    if text_width(value) <= width {
        return value.to_owned();
    }
    let mut result = String::new();
    let mut used = 0_u16;
    for character in value.chars() {
        let mut encoded = [0_u8; 4];
        let character_width = text_width(character.encode_utf8(&mut encoded));
        if used.saturating_add(character_width) > width {
            break;
        }
        result.push(character);
        used = used.saturating_add(character_width);
    }
    result
}

pub(super) fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let (text, cursor) = if app.mode == InputMode::Search {
        let (text, cursor) = input_footer(" / 搜索  ", &app.input, app.input_cursor, area.width);
        (text, Some(cursor))
    } else if app.mode == InputMode::LocalSearch {
        let (text, cursor) =
            input_footer(" / 本地筛选  ", &app.input, app.input_cursor, area.width);
        (text, Some(cursor))
    } else if app.mode == InputMode::Download {
        let (text, cursor) = input_footer(" D 下载  ", &app.input, app.input_cursor, area.width);
        (text, Some(cursor))
    } else if app.mode == InputMode::CacheSize {
        let (text, cursor) =
            input_footer(" s 缓存上限  ", &app.input, app.input_cursor, area.width);
        (text, Some(cursor))
    } else if app.mode == InputMode::Import {
        let (text, cursor) = input_footer(" I 导入  ", &app.input, app.input_cursor, area.width);
        (text, Some(cursor))
    } else if app.mode == InputMode::Palette {
        let (text, cursor) = input_footer(" Ctrl+P  ", &app.input, app.input_cursor, area.width);
        (text, Some(cursor))
    } else {
        (footer_text(app, area.width), None)
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(app.theme.muted)),
        area,
    );
    if let Some(cursor) = cursor {
        frame.set_cursor_position((area.x + cursor.min(area.width.saturating_sub(1)), area.y));
    }
}

pub(super) fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let popup = help_popup(area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "浏览",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(hint_line(&[
                UiAction::SelectNext,
                UiAction::SelectFirst,
                UiAction::Activate,
                UiAction::Back,
            ])),
            Line::from("←/→ 或 Tab/Shift+Tab 切换相邻栏 · Enter 向右展开/播放"),
            Line::from("Esc 收起当前栏 · 单击选择 · 双击打开 · 滚轮浏览"),
            Line::from(""),
            Line::from(Span::styled(
                "音乐",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(hint_line(&[
                UiAction::TogglePause,
                UiAction::PreviousTrack,
                UiAction::NextTrack,
                UiAction::SeekForward,
                UiAction::VolumeUp,
            ])),
            Line::from(hint_line(&[
                UiAction::ToggleLyrics,
                UiAction::CycleMode,
                UiAction::Enqueue,
                UiAction::ToggleFavorite,
                UiAction::Details,
            ])),
            Line::from("l 聚焦歌词栏 · e 展开当前栏（歌单/曲目/歌词）占满中间区域 · 再按收起"),
            Line::from("h 彻底关闭/打开歌词栏 · [/] 快退/快进"),
            Line::from("音量处滚轮调节；进度轨道可点击"),
            Line::from(""),
            Line::from(Span::styled(
                "网易云",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{}  ·  F1/F2/F3/F4 搜索歌曲/专辑/歌手/歌单  ·  {}",
                format_action_hint(UiAction::Search),
                format_action_hint(UiAction::Login),
            )),
            Line::from(hint_line(&[
                UiAction::Download,
                UiAction::Organize,
                UiAction::Dequeue,
                UiAction::Refresh,
                UiAction::ToggleHelp,
                UiAction::Quit,
            ])),
            Line::from(hint_line(&[UiAction::EditCacheSize, UiAction::ClearCache])),
            Line::from(format!(
                "{}  导入本地文件或目录  ·  {}  扫描已配置目录  ·  {}",
                format_action_hint(UiAction::Import),
                format_action_hint(UiAction::Refresh),
                format_action_hint(UiAction::Palette),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "输入框",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("←/→ 移动光标 · Home/End 首尾 · Backspace/Delete 删除"),
            Line::from("Enter 确认 · Esc 取消"),
            Line::from(""),
            Line::from(Span::styled(
                "帮助滚动",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("↑↓/jk 逐行 · PgUp/PgDn 翻页 · Home/End 首尾"),
            Line::from("滚轮滚动 · 单击或 Esc 关闭"),
        ])
        .block(overlay_panel(app, " 快捷键 "))
        .wrap(Wrap { trim: true })
        .scroll((app.help_scroll, 0)),
        popup,
    );
}

pub(super) fn draw_details_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let Some(track) = app.selected_track() else {
        return;
    };
    let popup = centered(area, 68, 22);
    frame.render_widget(Clear, popup);
    let extra = app
        .services
        .library
        .track_detail(track.id)
        .ok()
        .flatten()
        .unwrap_or_default();
    let local = if track.path.is_some() {
        "已下载"
    } else {
        "在线"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                track.title.as_str(),
                Style::default()
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("艺术家  ", Style::default().fg(app.theme.muted)),
                Span::raw(track.artists.as_str()),
            ]),
            Line::from(vec![
                Span::styled("专辑    ", Style::default().fg(app.theme.muted)),
                Span::raw(track.album.as_str()),
            ]),
            Line::from(vec![
                Span::styled("时长    ", Style::default().fg(app.theme.muted)),
                Span::raw(format_time(Duration::from_millis(track.duration_ms))),
            ]),
            Line::from(vec![
                Span::styled("状态    ", Style::default().fg(app.theme.muted)),
                Span::raw(local),
            ]),
            Line::from(vec![
                Span::styled("播放    ", Style::default().fg(app.theme.muted)),
                Span::raw(track.play_count.to_string()),
            ]),
            Line::from(vec![
                Span::styled("格式    ", Style::default().fg(app.theme.muted)),
                Span::raw(if track.format.is_empty() {
                    "—".into()
                } else {
                    track.format.to_uppercase()
                }),
            ]),
            Line::from(vec![
                Span::styled("大小    ", Style::default().fg(app.theme.muted)),
                Span::raw(format_bytes(track.bytes)),
            ]),
            Line::from(vec![
                Span::styled("路径    ", Style::default().fg(app.theme.muted)),
                Span::raw(
                    track
                        .path
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "—".into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("NCM ID  ", Style::default().fg(app.theme.muted)),
                Span::raw(
                    track
                        .ncm_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "—".into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("年份    ", Style::default().fg(app.theme.muted)),
                Span::raw(
                    extra
                        .release_year
                        .map(|year| year.to_string())
                        .unwrap_or_else(|| "—".into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("碟/轨   ", Style::default().fg(app.theme.muted)),
                Span::raw(format!(
                    "{} / {}",
                    extra
                        .disc_number
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "—".into()),
                    extra
                        .track_number
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "—".into()),
                )),
            ]),
            Line::from(vec![
                Span::styled("码率    ", Style::default().fg(app.theme.muted)),
                Span::raw(if extra.bitrate == 0 {
                    "—".into()
                } else {
                    format!("{} kbps", extra.bitrate)
                }),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Esc 关闭",
                Style::default().fg(app.theme.muted),
            )),
        ])
        .block(overlay_panel(app, " 歌曲详情 ")),
        popup,
    );
}

fn draw_cover_slot(frame: &mut Frame, app: &mut App, nav: Rect) {
    let has_cover =
        app.cover_bytes.is_some() || app.cover_protocol_nav.is_some() || app.cover_nav.is_some();
    let cover_area = navigation_cover_area(nav, has_cover);
    if cover_area.width == 0 || cover_area.height == 0 {
        return;
    }
    draw_navigation_cover(frame, app, cover_area);
}

fn draw_navigation_cover(frame: &mut Frame, app: &mut App, cover_area: Rect) {
    // Graphics protocol only when the picker actually selected Kitty/Sixel.
    // Never paint another widget under it — that wipes unicode placeholders.
    if crate::cover::uses_terminal_graphics(&app.cover_picker)
        && let Some(protocol) = app.cover_protocol_nav.as_mut()
    {
        crate::cover::render_protocol(frame, cover_area, protocol);
        return;
    }
    if app
        .cover_nav
        .as_ref()
        .is_none_or(|cover| cover.cols != cover_area.width || cover.rows != cover_area.height)
        && let Some(bytes) = app.cover_bytes.as_deref()
    {
        app.cover_nav = crate::cover::from_bytes(bytes, cover_area.width, cover_area.height);
    }
    let Some(cover) = app.cover_nav.as_ref() else {
        return;
    };
    let lines = cover
        .lines
        .iter()
        .take(cover_area.height as usize)
        .cloned()
        .map(|line| {
            let spans = line
                .spans
                .into_iter()
                .take(cover_area.width as usize)
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), cover_area);
}

pub(super) fn panel<'a>(app: &App, title: &'a str, focused: bool) -> Block<'a> {
    let border = if focused {
        app.theme.accent
    } else {
        app.theme.border
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(app.theme.panel).fg(app.theme.text))
}

fn panel_open_left<'a>(app: &App, title: &'a str, focused: bool) -> Block<'a> {
    let border = if focused {
        app.theme.accent
    } else {
        app.theme.border
    };
    Block::default()
        .title(title)
        .borders(Borders::TOP.union(Borders::RIGHT).union(Borders::BOTTOM))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(app.theme.panel).fg(app.theme.text))
}

pub(super) fn breadcrumb_text(app: &App) -> String {
    let mut segments = vec![app.route.label()];
    if app.title != app.route.label() {
        segments.push(app.title.as_str());
    }
    for column in &app.columns {
        if segments.last().copied() != Some(column.title.as_str()) {
            segments.push(column.title.as_str());
        }
    }
    segments.join(" › ")
}

pub(super) fn truncate_to_width(value: &str, width: u16) -> String {
    if text_width(value) <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width as usize).collect();
    }
    let mut suffix = String::new();
    for character in value.chars().rev() {
        let candidate = format!("{character}{suffix}");
        if text_width(&candidate) > width.saturating_sub(1) {
            break;
        }
        suffix = candidate;
    }
    format!("…{suffix}")
}

pub(super) fn overlay_panel<'a>(app: &App, title: &'a str) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.overlay).fg(app.theme.text))
}

pub(super) fn selection_style(app: &App, active: bool) -> Style {
    if active {
        Style::default()
            .bg(app.theme.panel_highlight)
            .fg(app.theme.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.muted)
            .add_modifier(Modifier::DIM)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct QrLayout {
    pub(super) card: Rect,
    pub(super) code: Rect,
    pub(super) status: Rect,
}

pub(super) fn qr_dimensions(qr: &str) -> (u16, u16) {
    let width = qr
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let height = qr.lines().count();
    (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    )
}

pub(super) fn qr_layout(area: Rect, code_width: u16, code_height: u16) -> Option<QrLayout> {
    if code_width == 0 || code_height == 0 {
        return None;
    }
    let card_width = code_width.checked_add(QR_PADDING_X.checked_mul(2)?)?;
    let card_height = code_height.checked_add(QR_PADDING_Y.checked_mul(2)?)?;
    let total_height = card_height.checked_add(QR_STATUS_GAP)?.checked_add(1)?;
    if card_width > area.width || total_height > area.height {
        return None;
    }

    let card = Rect::new(
        area.x + area.width.saturating_sub(card_width) / 2,
        area.y + area.height.saturating_sub(total_height) / 2,
        card_width,
        card_height,
    );
    Some(QrLayout {
        code: Rect::new(
            card.x + QR_PADDING_X,
            card.y + QR_PADDING_Y,
            code_width,
            code_height,
        ),
        status: Rect::new(area.x, card.y + card.height + QR_STATUS_GAP, area.width, 1),
        card,
    })
}

pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub(super) fn help_popup(area: Rect) -> Rect {
    centered(area, 76, 18)
}

pub(super) fn format_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub(super) fn format_library_duration(duration_ms: u64) -> String {
    let minutes = duration_ms / 60_000;
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

pub(super) fn format_bytes(bytes: u64) -> String {
    pub(super) const KIB: f64 = 1024.0;
    pub(super) const MIB: f64 = KIB * 1024.0;
    pub(super) const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

pub(super) fn format_cache_size_input(bytes: u64) -> String {
    pub(super) const GIB: u64 = 1024 * 1024 * 1024;
    pub(super) const MIB: u64 = 1024 * 1024;
    if bytes.is_multiple_of(GIB) {
        format!("{}GiB", bytes / GIB)
    } else if bytes.is_multiple_of(MIB) {
        format!("{}MiB", bytes / MIB)
    } else {
        bytes.to_string()
    }
}

pub(super) fn parse_cache_size(value: &str) -> Result<u64, &'static str> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("请输入缓存上限，例如 512M、4G 或 8GiB");
    }
    let units = [
        ("gib", 1024_u64.pow(3)),
        ("gb", 1024_u64.pow(3)),
        ("g", 1024_u64.pow(3)),
        ("mib", 1024_u64.pow(2)),
        ("mb", 1024_u64.pow(2)),
        ("m", 1024_u64.pow(2)),
        ("kib", 1024_u64),
        ("kb", 1024_u64),
        ("k", 1024_u64),
        ("b", 1_u64),
    ];
    let (number, multiplier) = units
        .iter()
        .find_map(|(suffix, multiplier)| {
            normalized
                .strip_suffix(suffix)
                .map(|number| (number.trim(), *multiplier))
        })
        .unwrap_or((normalized.as_str(), 1));
    let number = number
        .parse::<f64>()
        .map_err(|_| "缓存大小格式无效，例如 512M、4G 或 8GiB")?;
    let bytes = number * multiplier as f64;
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        return Err("缓存大小必须大于 0 且不能溢出");
    }
    Ok(bytes.round() as u64)
}

pub(super) fn player_status(state: &PlayerState) -> String {
    if state.audio.compact().is_empty() {
        format!("音量 {:>3}%", (state.volume * 100.0).round() as u8)
    } else {
        format!(
            "{} · {:>3}%",
            state.audio.compact(),
            (state.volume * 100.0).round() as u8
        )
    }
}

pub(super) fn text_width(value: &str) -> u16 {
    u16::try_from(Line::from(value).width()).unwrap_or(u16::MAX)
}

pub(super) fn centered_line(area: Rect, y: u16, width: u16) -> Rect {
    let width = width.min(area.width);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        y,
        width,
        u16::from(area.height > 0),
    )
}

pub(super) fn shorten(value: &str, max: usize) -> String {
    let length = value.chars().count();
    if length <= max {
        return value.to_owned();
    }
    let visible = max.saturating_sub(1);
    format!("{}…", value.chars().take(visible).collect::<String>())
}

pub(super) fn draw_palette(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered(
        area,
        56.min(area.width.saturating_sub(4)),
        16.min(area.height.saturating_sub(4)),
    );
    frame.render_widget(Clear, popup);
    let filtered = app.filtered_palette();
    let selected = if filtered.is_empty() {
        0
    } else {
        app.palette_selected.min(filtered.len() - 1)
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "命令  {}/{}",
            selected.saturating_add(1).min(filtered.len()),
            filtered.len()
        ),
        Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "没有匹配的命令",
            Style::default().fg(app.theme.muted),
        )));
    } else {
        let visible = popup.height.saturating_sub(3) as usize;
        let start = selected.saturating_sub(visible.saturating_sub(1));
        for (index, (item, _)) in filtered.iter().enumerate().skip(start).take(visible) {
            let style = if index == selected {
                selection_style(app, true)
            } else {
                Style::default().fg(app.theme.text)
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {}", item.title), style),
                Span::styled(
                    format!("  {}", item.hint),
                    Style::default().fg(app.theme.muted),
                ),
            ]));
        }
    }
    lines.push(Line::from(Span::styled(
        "Enter 执行 · Esc 关闭 · ↑↓ 选择",
        Style::default().fg(app.theme.muted),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(overlay_panel(app, " Ctrl+P "))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

pub(super) fn draw_toasts(frame: &mut Frame, app: &App, area: Rect) {
    let Some(toast) = app.toasts.last() else {
        return;
    };
    let color = match toast.kind {
        ToastKind::Success => app.theme.accent,
        ToastKind::Warn => Color::Yellow,
        ToastKind::Error => Color::LightRed,
    };
    let width = text_width(&toast.message)
        .saturating_add(4)
        .min(area.width.saturating_sub(4))
        .max(12);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.bottom().saturating_sub(4).max(area.y),
        width,
        3,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", toast.message),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center)
        .block(overlay_panel(app, " ")),
        popup,
    );
}

pub(super) fn load_hide_lyrics(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| {
        text.lines().any(|line| {
            let line = line.trim().replace(' ', "");
            line == "hide_lyrics=true"
        })
    })
}

pub(super) fn save_hide_lyrics(path: &std::path::Path, hidden: bool) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("hide_lyrics = {hidden}\n"));
}

pub(super) fn search_kind_label(kind: SearchKind) -> &'static str {
    match kind {
        SearchKind::Song => "歌曲",
        SearchKind::Album => "专辑",
        SearchKind::Artist => "歌手",
        SearchKind::Playlist => "歌单",
    }
}

pub(super) fn draw_local_empty(frame: &mut Frame, app: &App, area: Rect, title: &str) {
    let message = if app.mode == InputMode::Import {
        if app.input.is_empty() {
            "输入本地文件或目录路径，Enter 导入".into()
        } else {
            format!("导入本地音乐：{}", app.input)
        }
    } else {
        LOCAL_IMPORT_HINT.to_owned()
    };
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(Style::default().fg(app.theme.muted))
            .block(panel(app, title, app.focus == Focus::Content))
            .wrap(Wrap { trim: true }),
        area,
    );
}

pub(super) fn parse_import_path(input: &str) -> Result<PathBuf, String> {
    let trimmed = input
        .trim()
        .trim_matches(|character| character == '"' || character == '\'');
    if trimmed.is_empty() {
        return Err("请输入要导入的文件或目录路径".into());
    }
    let path = expand_user_path(trimmed);
    if !path.exists() {
        return Err(format!("路径不存在：{}", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("无法解析路径：{error}"))
}

pub(super) fn expand_user_path(value: &str) -> PathBuf {
    let value = unescape_shell_path(value);
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(mut home) = home_dir()
    {
        home.push(rest);
        return home;
    }
    PathBuf::from(value)
}

pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(super) fn unescape_shell_path(value: &str) -> String {
    if cfg!(windows) {
        return value.to_owned();
    }
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\\'
            && let Some(next) = chars.next()
        {
            result.push(next);
        } else {
            result.push(character);
        }
    }
    result
}

pub(super) fn empty_message(app: &App) -> String {
    match app.route {
        Route::Daily | Route::Recommended | Route::Created | Route::Subscribed | Route::Albums
            if app.identity.is_none() =>
        {
            "按 L 扫码登录".into()
        }
        Route::Search => "按 / 输入关键词".into(),
        Route::Queue => "播放队列为空".into(),
        Route::Favorites => "还没有喜欢的音乐 · 登录后显示网易云红心歌单".into(),
        Route::Local => LOCAL_IMPORT_HINT.into(),
        Route::Recent => "暂无最近播放".into(),
        _ => "暂无内容".into(),
    }
}
