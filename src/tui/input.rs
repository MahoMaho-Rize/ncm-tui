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

pub(super) fn edit_input(input: &mut String, cursor: &mut usize, key: KeyEvent) -> InputEdit {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return InputEdit::None;
    }
    match key.code {
        KeyCode::Left => *cursor = previous_char_boundary(input, *cursor),
        KeyCode::Right => *cursor = next_char_boundary(input, *cursor),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = input.len(),
        KeyCode::Backspace if *cursor > 0 => {
            let previous = previous_char_boundary(input, *cursor);
            input.drain(previous..*cursor);
            *cursor = previous;
        }
        KeyCode::Delete if *cursor < input.len() => {
            input.drain(*cursor..next_char_boundary(input, *cursor));
        }
        KeyCode::Char(character) => {
            input.insert(*cursor, character);
            *cursor += character.len_utf8();
        }
        KeyCode::Enter => return InputEdit::Submit,
        _ => {}
    }
    InputEdit::None
}

pub(super) fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor.min(value.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

pub(super) fn next_char_boundary(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(offset, _)| cursor + offset)
}

pub(super) fn normal_action(
    focus: Focus,
    playback_active: bool,
    key: KeyEvent,
) -> Option<UiAction> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char('q') => Some(UiAction::Quit),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(UiAction::FocusPrevious)
        }
        KeyCode::Tab => Some(UiAction::FocusNext),
        KeyCode::BackTab => Some(UiAction::FocusPrevious),
        KeyCode::Left => Some(UiAction::FocusPrevious),
        KeyCode::Right => Some(UiAction::FocusNext),
        KeyCode::Down | KeyCode::Char('j') => Some(UiAction::SelectNext),
        KeyCode::Up | KeyCode::Char('k') => Some(UiAction::SelectPrevious),
        KeyCode::Home | KeyCode::Char('g') => Some(UiAction::SelectFirst),
        KeyCode::End | KeyCode::Char('G') => Some(UiAction::SelectLast),
        KeyCode::PageDown => Some(UiAction::PageNext),
        KeyCode::PageUp => Some(UiAction::PagePrevious),
        KeyCode::Enter => Some(UiAction::Activate),
        KeyCode::Char('L') => Some(UiAction::Login),
        KeyCode::Char('l') => Some(UiAction::ToggleLyrics),
        KeyCode::Char('h') => Some(UiAction::HideLyrics),
        KeyCode::Char('e') => Some(UiAction::Expand),
        KeyCode::Char('s') if !matches!(focus, Focus::Navigation) => Some(UiAction::Sort),
        KeyCode::Char('c') => Some(UiAction::ClearQueue),
        KeyCode::Char('/') => Some(UiAction::Search),
        KeyCode::Char('D') => Some(UiAction::Download),
        KeyCode::Char(' ') => Some(UiAction::TogglePause),
        KeyCode::Char('n') => Some(UiAction::NextTrack),
        KeyCode::Char('p') => Some(UiAction::PreviousTrack),
        KeyCode::Char('[') if playback_active => Some(UiAction::SeekBackward),
        KeyCode::Char(']') if playback_active => Some(UiAction::SeekForward),
        KeyCode::Char('+') | KeyCode::Char('=') => Some(UiAction::VolumeUp),
        KeyCode::Char('-') => Some(UiAction::VolumeDown),
        KeyCode::Char('m') => Some(UiAction::CycleMode),
        KeyCode::Char('a') => Some(UiAction::Enqueue),
        KeyCode::Char('d') => Some(UiAction::Dequeue),
        KeyCode::Char('f') => Some(UiAction::ToggleFavorite),
        KeyCode::Char('o') => Some(UiAction::Organize),
        KeyCode::Char('i') => Some(UiAction::Details),
        KeyCode::Char('I') => Some(UiAction::Import),
        KeyCode::Char('r') => Some(UiAction::Refresh),
        _ => None,
    }
}

pub(super) fn dispatch_action(app: &mut App, action: UiAction) -> bool {
    match action {
        UiAction::Quit => return true,
        UiAction::ToggleHelp => {
            app.show_details = false;
            app.show_help = !app.show_help;
            app.help_scroll = 0;
        }
        UiAction::Back => {
            if app.show_help {
                app.show_help = false;
            } else if app.show_details {
                app.show_details = false;
            } else if app.mode != InputMode::Normal {
                let reload_local = app.mode == InputMode::LocalSearch && app.route == Route::Local;
                app.mode = InputMode::Normal;
                app.input.clear();
                app.input_cursor = 0;
                app.cache_clear_armed_until = None;
                if reload_local {
                    app.load_local();
                }
            } else if !app.cancel_download() {
                app.back();
            }
        }
        UiAction::FocusNext => {
            app.focus_next(false);
        }
        UiAction::FocusPrevious => app.focus_next(true),
        UiAction::SelectPrevious => app.move_selection(-1),
        UiAction::SelectNext => app.move_selection(1),
        UiAction::SelectFirst => match app.focus {
            Focus::Navigation => app.nav_selected = 0,
            Focus::Content => app.selected = 0,
            Focus::Column(index) => {
                if let Some(column) = app.columns.get_mut(index) {
                    column.selected = 0;
                }
            }
            Focus::Lyrics => {}
        },
        UiAction::SelectLast => match app.focus {
            Focus::Navigation => app.nav_selected = Route::ALL.len().saturating_sub(1),
            Focus::Content => app.selected = app.content.len().saturating_sub(1),
            Focus::Column(index) => {
                if let Some(column) = app.columns.get_mut(index) {
                    column.selected = column.content.len().saturating_sub(1);
                }
            }
            Focus::Lyrics => {}
        },
        UiAction::PagePrevious => app.move_selection(-10),
        UiAction::PageNext => app.move_selection(10),
        UiAction::Activate => {
            if !app.retry_focused_error() {
                app.activate();
            }
        }
        UiAction::Palette => app.open_palette(),
        UiAction::HideLyrics => app.set_lyrics_hidden(!app.lyrics_hidden),
        UiAction::Sort => {
            if app.route == Route::Local {
                app.local_sort = app.local_sort.next();
                app.status = format!("排序：{}", app.local_sort.label());
                if !matches!(app.local_layer, LocalLayer::Menu) {
                    app.load_local();
                }
            }
        }
        UiAction::ClearQueue => {
            if app.route == Route::Queue {
                app.clear_play_queue();
            }
        }
        UiAction::Search => {
            if app.route == Route::Local {
                app.mode = InputMode::LocalSearch;
                app.input.clear();
                app.input_cursor = 0;
                app.status = "输入标题、艺术家或专辑，Enter 筛选本地音乐".into();
            } else {
                app.set_route(Route::Search);
                app.focus = Focus::Content;
            }
        }
        UiAction::Login => app.start_login(),
        UiAction::Download => {
            app.set_route(Route::Downloads);
            app.focus = Focus::Content;
            app.mode = InputMode::Download;
            app.input.clear();
            app.input_cursor = 0;
        }
        UiAction::ToggleLyrics => app.toggle_lyrics_focus(),
        UiAction::Expand => app.toggle_expand(),
        UiAction::TogglePause => {
            if let Some(player) = &mut app.player {
                player.toggle_pause();
            }
        }
        UiAction::PreviousTrack => app.next_track(-1),
        UiAction::NextTrack => app.next_track(1),
        UiAction::SeekBackward => {
            if let Some(player) = &mut app.player {
                let _ = player.seek_by(-5);
            }
        }
        UiAction::SeekForward => {
            if let Some(player) = &mut app.player {
                let _ = player.seek_by(5);
            }
        }
        UiAction::SeekTo(target) => {
            if let Some(player) = &mut app.player {
                let _ = player.seek_to(target);
            }
        }
        UiAction::VolumeUp => {
            if let Some(player) = &mut app.player {
                player.change_volume(0.05);
            }
        }
        UiAction::VolumeDown => {
            if let Some(player) = &mut app.player {
                player.change_volume(-0.05);
            }
        }
        UiAction::CycleMode => app.cycle_play_mode(),
        UiAction::Enqueue => app.enqueue(),
        UiAction::Dequeue => app.dequeue(),
        UiAction::ToggleFavorite => app.favorite(),
        UiAction::Organize => app.start_organize(),
        UiAction::Details => app.show_details = app.selected_track().is_some(),
        UiAction::Refresh => {
            if app.retry_focused_error() {
            } else if app.route == Route::Local {
                app.start_scan();
            } else {
                let route = app.route;
                app.set_route(route);
            }
        }
        UiAction::Import => app.begin_import(),
        UiAction::EditCacheSize => {
            app.mode = InputMode::CacheSize;
            app.input = format_cache_size_input(app.services.playback_cache.stats().max_bytes);
            app.input_cursor = app.input.len();
            app.cache_clear_armed_until = None;
            app.status = "输入缓存上限，例如 512M、4G 或 8GiB，Enter 保存".into();
        }
        UiAction::ClearCache => app.clear_playback_cache(),
        UiAction::OpenRoute(route) => {
            app.set_route(route);
            app.focus = Focus::Content;
        }
    }
    false
}

pub(super) fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    if app.show_help {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.help_scroll = app.help_scroll.saturating_sub(2);
            }
            MouseEventKind::ScrollDown => {
                app.help_scroll = app.help_scroll.saturating_add(2).min(HELP_MAX_SCROLL);
            }
            MouseEventKind::Down(MouseButton::Left) => app.show_help = false,
            _ => {}
        }
        return;
    }
    if app.show_details {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            app.show_details = false;
        }
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            let direction = if mouse.kind == MouseEventKind::ScrollDown {
                -1.0
            } else {
                1.0
            };
            if contains(app.hits.volume, mouse.column, mouse.row) {
                let action = if direction > 0.0 {
                    UiAction::VolumeUp
                } else {
                    UiAction::VolumeDown
                };
                dispatch_action(app, action);
                return;
            }
            if contains(app.hits.lyrics, mouse.column, mouse.row) {
                if app.focus != Focus::Lyrics {
                    app.previous_focus = app.valid_focus(app.focus);
                    app.focus = Focus::Lyrics;
                }
                return;
            }
            if contains(app.hits.nav, mouse.column, mouse.row) {
                app.focus = Focus::Navigation;
            } else {
                let Some(hit) = app
                    .hits
                    .columns
                    .iter()
                    .copied()
                    .find(|hit| contains(hit.area, mouse.column, mouse.row))
                else {
                    return;
                };
                app.focus = if hit.index == 0 {
                    Focus::Content
                } else {
                    Focus::Column(hit.index - 1)
                };
            }
            let delta = if mouse.kind == MouseEventKind::ScrollDown {
                3
            } else {
                -3
            };
            app.move_selection(delta);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if contains(app.hits.account, mouse.column, mouse.row) {
                dispatch_action(app, UiAction::OpenRoute(Route::Account));
            } else if let Some(action) = player_control_action(&app.hits, mouse.column, mouse.row) {
                dispatch_action(app, action);
            } else if contains(app.hits.progress, mouse.column, mouse.row) {
                let state = app.player_state();
                if !state.duration.is_zero()
                    && let Some(ratio) = progress_ratio_at(app.hits.progress, mouse.column)
                {
                    let target = Duration::from_secs_f64(state.duration.as_secs_f64() * ratio);
                    dispatch_action(app, UiAction::SeekTo(target));
                }
            } else if contains(app.hits.nav, mouse.column, mouse.row) {
                app.focus = Focus::Navigation;
                let row = mouse.row.saturating_sub(app.hits.nav.y) as usize;
                if let Some(index) = nav_index_at(row, app.hits.nav.height) {
                    app.nav_selected = index;
                    dispatch_action(app, UiAction::OpenRoute(Route::ALL[index]));
                }
            } else if contains(app.hits.lyrics, mouse.column, mouse.row) {
                if app.focus != Focus::Lyrics {
                    app.previous_focus = app.valid_focus(app.focus);
                }
                app.focus = Focus::Lyrics;
            } else if let Some(hit) = app
                .hits
                .columns
                .iter()
                .copied()
                .find(|hit| contains(hit.area, mouse.column, mouse.row))
            {
                app.focus = if hit.index == 0 {
                    Focus::Content
                } else {
                    Focus::Column(hit.index - 1)
                };
                if hit.index == 0 && contains(content_action_region(app), mouse.column, mouse.row) {
                    if app.route == Route::Account && app.identity.is_none() {
                        dispatch_action(app, UiAction::Login);
                    } else if app.route == Route::Downloads && app.active_job.is_none() {
                        dispatch_action(app, UiAction::Download);
                    } else if app.route == Route::Local
                        && app.mode == InputMode::Normal
                        && app.active_job.is_none()
                        && app.content.is_empty()
                    {
                        dispatch_action(app, UiAction::Import);
                    }
                    return;
                }
                let index = hit.offset + mouse.row.saturating_sub(hit.area.y) as usize;
                let len = if hit.index == 0 {
                    app.content.len()
                } else {
                    app.columns
                        .get(hit.index - 1)
                        .map_or(0, |column| column.content.len())
                };
                if index < len {
                    if hit.index == 0 {
                        app.selected = index;
                    } else if let Some(column) = app.columns.get_mut(hit.index - 1) {
                        column.selected = index;
                    }
                    let now = Instant::now();
                    let double = app.last_click.is_some_and(|(pane, last, at)| {
                        pane == hit.index
                            && last == index
                            && now.duration_since(at) < Duration::from_millis(450)
                    });
                    app.last_click = Some((hit.index, index, now));
                    if double {
                        dispatch_action(app, UiAction::Activate);
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn player_control_action(hits: &HitRegions, x: u16, y: u16) -> Option<UiAction> {
    if contains(hits.previous, x, y) {
        Some(UiAction::PreviousTrack)
    } else if contains(hits.pause, x, y) {
        Some(UiAction::TogglePause)
    } else if contains(hits.next, x, y) {
        Some(UiAction::NextTrack)
    } else if contains(hits.play_mode, x, y) {
        Some(UiAction::CycleMode)
    } else {
        None
    }
}

pub(super) fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

pub(super) fn progress_ratio_at(area: Rect, x: u16) -> Option<f64> {
    if area.width == 0 || x < area.x || x >= area.right() {
        return None;
    }
    if area.width == 1 {
        return Some(0.0);
    }
    Some(f64::from(x - area.x) / f64::from(area.width - 1))
}

pub(super) fn content_action_region(app: &App) -> Rect {
    let (label, row_offset) = match app.route {
        Route::Account if app.identity.is_none() && app.qr.is_empty() => {
            if app.login_status.is_empty() {
                ("按 L 或点击这里扫码登录", 0)
            } else {
                (app.login_status.as_str(), 0)
            }
        }
        Route::Downloads if app.active_job.is_none() && app.mode != InputMode::Download => {
            ("按 D 或点击这里", 2)
        }
        Route::Local
            if app.content.is_empty()
                && app.mode == InputMode::Normal
                && !app.loading
                && app.active_job.is_none() =>
        {
            (LOCAL_IMPORT_HINT, 0)
        }
        _ => return Rect::default(),
    };
    centered_line(
        app.hits.content,
        app.hits.content.y.saturating_add(row_offset),
        text_width(label),
    )
}

pub(super) fn parse_download(command: &str) -> Result<DownloadRequest, &'static str> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 2 {
        return Err("用法：track|album|playlist ID [起始-结束]");
    }
    let id = parts[1].parse::<u64>().map_err(|_| "网易云 ID 无效")?;
    let source = match parts[0] {
        "track" | "t" => DownloadSource::Track(id),
        "album" | "al" => DownloadSource::Album(id),
        "playlist" | "pl" => DownloadSource::Playlist(id),
        _ => return Err("用法：track|album|playlist ID [起始-结束]"),
    };
    let selection = if let Some(range) = parts.get(2) {
        let (start, end) = range.split_once('-').ok_or("范围格式应为 起始-结束")?;
        let start = start.parse::<usize>().map_err(|_| "范围无效")?;
        let end = end.parse::<usize>().map_err(|_| "范围无效")?;
        if start == 0 || end < start {
            return Err("范围无效");
        }
        TrackSelection::Positions(start..=end)
    } else {
        TrackSelection::All
    };
    Ok(DownloadRequest {
        source,
        selection,
        quality: AudioQuality::Lossless,
        overwrite: false,
    })
}

pub(super) fn query_local(
    library: &Library,
    route: Route,
    query: Option<&str>,
    layer: &LocalLayer,
    sort: TrackSort,
) -> Result<(Vec<Track>, Option<LibraryStats>), String> {
    let is_search = query.is_some();
    let tracks = if let Some(query) = query {
        library.search(query, 10_000)
    } else {
        match (route, layer) {
            (Route::Favorites, _) => library.list(true, 10_000),
            (Route::Local, LocalLayer::Album(name)) => library.list_by_album(name, 10_000),
            (Route::Local, LocalLayer::Artist(name)) => library.list_by_artist(name, 10_000),
            (Route::Local, LocalLayer::Tracks(view)) => library.list_view(*view, sort, 10_000),
            (Route::Local, LocalLayer::Menu) => library.list_view(TrackView::All, sort, 10_000),
            (Route::Recent, _) => library.history(1_000),
            (Route::Queue, _) => library.queue(),
            _ => return Ok((Vec::new(), None)),
        }
    }
    .map_err(|error| error.to_string())?;
    let stats = (route == Route::Local && !is_search)
        .then(|| library.stats().map_err(|error| error.to_string()))
        .transpose()?;
    Ok((tracks, stats))
}

pub(super) fn merge_content(existing: &mut Content, extra: Content) {
    match (existing, extra) {
        (Content::Tracks(current), Content::Tracks(more)) => {
            pagination::merge_unique_by_id(current, more, |track| track.id);
        }
        (Content::Playlists(current), Content::Playlists(more)) => {
            pagination::merge_unique_by_id(current, more, |playlist| playlist.id);
        }
        (Content::Albums(current), Content::Albums(more)) => {
            pagination::merge_unique_by_id(current, more, |album| album.id);
        }
        (Content::Artists(current), Content::Artists(more)) => {
            pagination::merge_unique_by_id(current, more, |artist| artist.id);
        }
        (current, extra) => *current = extra,
    }
}

pub(super) async fn load_source_page(
    discovery: Discovery,
    source: PagedSource,
    offset: usize,
) -> Result<Loaded, String> {
    match source.clone() {
        PagedSource::Playlist { id, name } => {
            let mut page = discovery
                .playlist_page(id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            if page.title.is_empty() {
                page.title = name;
            }
            Ok(Loaded::TrackPage(page, source))
        }
        PagedSource::Album { id, name } => {
            let mut page = discovery
                .album_page(id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            if page.title.is_empty() {
                page.title = name;
            }
            Ok(Loaded::TrackPage(page, source))
        }
        PagedSource::Artist { id, name } => {
            let mut page = discovery
                .artist_song_page(id, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            if page.title.is_empty() {
                page.title = name;
            }
            Ok(Loaded::TrackPage(page, source))
        }
        PagedSource::Search { query, kind } => {
            let page = discovery
                .search_page(&query, kind, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::SearchPage(page, source))
        }
        PagedSource::UserPlaylists { user_id, scope } => {
            let page = discovery
                .user_playlists_page(user_id, scope, offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::PlaylistPage(page.items, page.pagination, source))
        }
        PagedSource::SubscribedAlbums => {
            let page = discovery
                .subscribed_albums_page(offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::AlbumPage(page.items, page.pagination, source))
        }
        PagedSource::SubscribedArtists => {
            let page = discovery
                .subscribed_artists_page(offset, PAGE_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::ArtistPage(page.items, page.pagination, source))
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LayoutRequest {
    pub(super) area: Rect,
    pub(super) column_count: usize,
    pub(super) focus: Focus,
    pub(super) lyrics_hidden: bool,
    pub(super) expanded: bool,
}
