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

pub(super) fn layout_for(app: &App, area: Rect) -> Option<AppLayout> {
    app_layout(LayoutRequest {
        area,
        column_count: 1 + app.columns.len(),
        focus: app.focus,
        lyrics_hidden: app.lyrics_hidden,
        lyrics_expanded: app.show_lyrics,
        content_expanded: app.content_expanded,
    })
}

pub(super) fn nav_column_width(total_width: u16) -> u16 {
    if total_width >= WIDE_WIDTH {
        24
    } else if total_width >= 80 {
        20
    } else {
        16
    }
}

pub(super) fn app_layout(request: LayoutRequest) -> Option<AppLayout> {
    let area = request.area;
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .split(area);
    let column_count = request.column_count.max(1);
    let active = match request.focus {
        Focus::Column(index) => (index + 1).min(column_count - 1),
        Focus::Lyrics => column_count - 1,
        _ => 0,
    };
    let nav_width = nav_column_width(area.width);
    let navigation = Rect::new(rows[1].x, rows[1].y, nav_width, rows[1].height);
    let workspace = Rect::new(
        navigation.right(),
        rows[1].y,
        rows[1].width.saturating_sub(nav_width),
        rows[1].height,
    );
    let lyric_width = if request.lyrics_hidden {
        0
    } else if request.lyrics_expanded && request.focus == Focus::Lyrics {
        let keep = 20.min(workspace.width / 3);
        workspace
            .width
            .saturating_sub(keep)
            .max(24.min(workspace.width))
    } else if area.width >= WIDE_WIDTH {
        32.min(workspace.width.saturating_sub(24))
    } else {
        24.min(workspace.width.saturating_sub(16))
    };
    let browser_area = Rect::new(
        workspace.x,
        workspace.y,
        workspace.width.saturating_sub(lyric_width),
        workspace.height,
    );
    let lyrics = (lyric_width > 0).then_some(Rect::new(
        browser_area.right(),
        workspace.y,
        lyric_width,
        workspace.height,
    ));
    let browser = if request.content_expanded || browser_area.width < 24 {
        vec![BrowserPane {
            index: active,
            area: browser_area,
        }]
    } else {
        let visible_count = (browser_area.width / 24)
            .max(1)
            .min(u16::try_from(column_count).unwrap_or(u16::MAX))
            as usize;
        let start = active
            .saturating_add(1)
            .saturating_sub(visible_count)
            .min(column_count.saturating_sub(visible_count));
        let base_width = browser_area.width / visible_count as u16;
        let remainder = browser_area.width % visible_count as u16;
        let mut x = browser_area.x;
        (0..visible_count)
            .map(|slot| {
                let width = base_width + u16::from(slot < remainder as usize);
                let pane = BrowserPane {
                    index: start + slot,
                    area: Rect::new(x, browser_area.y, width, browser_area.height),
                };
                x = x.saturating_add(width);
                pane
            })
            .collect()
    };
    Some(AppLayout {
        header: rows[0],
        navigation: Some(navigation),
        browser,
        lyrics,
        player: rows[2],
        footer: rows[3],
    })
}

pub(super) fn calculate_hits(
    request: LayoutRequest,
    player_status_width: u16,
    account_width: u16,
    has_cover: bool,
) -> HitRegions {
    let Some(layout) = app_layout(request) else {
        return HitRegions::default();
    };
    let player = player_layout(layout.player, player_status_width, has_cover);
    let columns = layout
        .browser
        .iter()
        .map(|pane| ColumnHit {
            index: pane.index,
            area: inner_rect(pane.area),
            offset: 0,
        })
        .collect::<Vec<_>>();
    let content = columns
        .iter()
        .find(|column| column.index == 0)
        .map_or_else(Rect::default, |column| column.area);
    HitRegions {
        account: Rect::new(
            request
                .area
                .right()
                .saturating_sub(account_width.saturating_add(2)),
            request.area.y,
            account_width,
            u16::from(request.area.height > 0),
        ),
        nav: layout.navigation.map(inner_rect).unwrap_or_default(),
        content,
        columns,
        lyrics: layout.lyrics.map(inner_rect).unwrap_or_default(),
        progress: player.progress,
        previous: player.previous,
        pause: player.pause,
        next: player.next,
        play_mode: player.play_mode,
        volume: player.volume,
        content_offset: 0,
    }
}

pub(super) const PLAYER_COVER_COLS: u16 = 6;
pub(super) const NAV_COVER_MIN_ROWS: u16 = 4;
pub(super) const NAV_COVER_MAX_ROWS: u16 = 12;

pub(super) fn player_cover_width(inner_width: u16, has_cover: bool) -> u16 {
    if has_cover && inner_width >= 40 {
        PLAYER_COVER_COLS
    } else {
        0
    }
}

pub(super) fn navigation_cover_rows(inner_height: u16, item_count: usize, has_cover: bool) -> u16 {
    if !has_cover {
        return 0;
    }
    let leftover = inner_height.saturating_sub(item_count as u16);
    if leftover >= NAV_COVER_MIN_ROWS {
        leftover.min(NAV_COVER_MAX_ROWS)
    } else {
        0
    }
}

pub(super) fn player_layout(area: Rect, status_width: u16, has_cover: bool) -> PlayerLayout {
    let inner = inner_rect(area);
    if inner.width < 24 || inner.height < 3 {
        return PlayerLayout::default();
    }

    let cover_width = player_cover_width(inner.width, has_cover);
    let cover_gap = u16::from(cover_width > 0);
    let cover = Rect::new(inner.x, inner.y, cover_width, inner.height.min(3));
    let content = Rect::new(
        inner
            .x
            .saturating_add(cover_width.saturating_add(cover_gap)),
        inner.y,
        inner
            .width
            .saturating_sub(cover_width.saturating_add(cover_gap)),
        inner.height,
    );
    let volume_width = status_width.min(content.width / 2);
    let volume = Rect::new(
        content.right().saturating_sub(volume_width),
        content.y,
        volume_width,
        1,
    );
    let song_info = Rect::new(
        content.x,
        content.y,
        content.width.saturating_sub(volume_width.saturating_add(1)),
        1,
    );

    let track_width = (content.width / 2).min(content.width.saturating_sub(12));
    let timeline_width = track_width.saturating_add(12);
    let timeline_x = content
        .x
        .saturating_add(content.width.saturating_sub(timeline_width) / 2);
    let elapsed = Rect::new(timeline_x, content.y + 1, 5, 1);
    let progress = Rect::new(elapsed.right() + 1, content.y + 1, track_width, 1);
    let duration = Rect::new(progress.right() + 1, content.y + 1, 5, 1);

    let previous_width = text_width("p ‹");
    let pause_width = text_width("Space ▶").max(text_width("Space Ⅱ"));
    let next_width = text_width("› n");
    let mode_width = text_width("m 循环");
    let gap = 3;
    let controls_width = previous_width
        .saturating_add(pause_width)
        .saturating_add(next_width)
        .saturating_add(mode_width)
        .saturating_add(gap * 3);
    let mut control_x = content
        .x
        .saturating_add(content.width.saturating_sub(controls_width) / 2);
    let previous = Rect::new(control_x, content.y + 2, previous_width, 1);
    control_x = previous.right().saturating_add(gap);
    let pause = Rect::new(control_x, content.y + 2, pause_width, 1);
    control_x = pause.right().saturating_add(gap);
    let next = Rect::new(control_x, content.y + 2, next_width, 1);
    control_x = next.right().saturating_add(gap);
    let play_mode = Rect::new(control_x, content.y + 2, mode_width, 1);

    PlayerLayout {
        song_info,
        elapsed,
        progress,
        duration,
        previous,
        pause,
        next,
        play_mode,
        volume,
        cover,
    }
}

pub(super) fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

pub(super) fn content_offset(selected: usize, visible_height: u16) -> usize {
    let visible = visible_height as usize;
    selected.saturating_sub(visible.saturating_sub(1))
}

pub(super) fn grouped_navigation(height: u16) -> bool {
    let groups = Route::ALL
        .iter()
        .map(|route| route.group())
        .collect::<std::collections::HashSet<_>>()
        .len();
    Route::ALL.len().saturating_add(groups) <= height as usize
}

pub(super) fn nav_index_at(row: usize, height: u16) -> Option<usize> {
    if !grouped_navigation(height) {
        return (row < Route::ALL.len()).then_some(row);
    }
    let mut line = 0;
    let mut previous = "";
    for (index, route) in Route::ALL.iter().enumerate() {
        if route.group() != previous {
            if row == line {
                return None;
            }
            line += 1;
            previous = route.group();
        }
        if row == line {
            return Some(index);
        }
        line += 1;
    }
    None
}
