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
use ratatui::backend::{Backend, TestBackend};
use tempfile::TempDir;

use crate::ncm_core::{NcmClient, SessionConfig};

fn test_app() -> (TempDir, App) {
    let directory = tempfile::tempdir().unwrap();
    let client = NcmClient::new(SessionConfig::default(), Duration::from_secs(1)).unwrap();
    let services = Services {
        authentication: Authentication::new(client.clone(), directory.path().join("session.json")),
        discovery: Discovery::with_lyrics_dir(
            client.clone(),
            directory.path().join("lyrics-cache"),
        ),
        library: Library::open(directory.path()).unwrap(),
        downloader: Downloader::new(client, directory.path(), 1).unwrap(),
        library_roots: vec![directory.path().to_path_buf()],
        ui_state_path: directory.path().join("ui.toml"),
        playback_cache: PlaybackCache::open_blocking(
            directory.path().join("playback-cache"),
            4 * 1024 * 1024 * 1024,
        )
        .unwrap(),
    };
    (directory, App::new(services))
}

fn layout_req(
    area: Rect,
    column_count: usize,
    focus: Focus,
    lyrics_hidden: bool,
    expanded: bool,
) -> LayoutRequest {
    LayoutRequest {
        area,
        column_count,
        focus,
        lyrics_hidden,
        expanded,
    }
}

fn hits_for(
    area: Rect,
    column_count: usize,
    focus: Focus,
    lyrics_hidden: bool,
    expanded: bool,
) -> HitRegions {
    calculate_hits(
        layout_req(area, column_count, focus, lyrics_hidden, expanded),
        16,
        6,
        false,
    )
}

fn render_app(app: &mut App, width: u16, height: u16) -> (String, (u16, u16)) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    (text, (cursor.x, cursor.y))
}

#[test]
fn parses_granular_download_ranges() {
    let request = parse_download("album 42 3-8").unwrap();
    assert_eq!(request.source, DownloadSource::Album(42));
    assert_eq!(request.selection, TrackSelection::Positions(3..=8));
}

#[test]
fn navigation_hit_map_includes_group_headers() {
    assert_eq!(nav_index_at(0, 20), None);
    assert_eq!(nav_index_at(1, 20), Some(0));
    assert_eq!(nav_index_at(4, 20), None);
    assert_eq!(nav_index_at(5, 20), Some(3));
    assert_eq!(nav_index_at(10, 20), Some(7));
}

#[test]
fn content_scroll_keeps_selection_visible() {
    assert_eq!(content_offset(0, 8), 0);
    assert_eq!(content_offset(7, 8), 0);
    assert_eq!(content_offset(8, 8), 1);
}

#[test]
fn warmed_playlist_catalog_serves_created_and_subscribed_routes() {
    let (_directory, mut app) = test_app();
    app.identity = Some(Identity {
        user_id: 7,
        nickname: "测试用户".into(),
    });
    app.playlist_cache = Some(vec![
        PlaylistSummary {
            id: 1,
            name: "我创建的".into(),
            track_count: 2,
            created_by_user: true,
            special_type: 0,
        },
        PlaylistSummary {
            id: 2,
            name: "我收藏的".into(),
            track_count: 3,
            created_by_user: false,
            special_type: 0,
        },
    ]);

    app.load_playlists(PlaylistScope::Created);
    let Content::Playlists(created) = &app.content else {
        panic!("expected cached playlists");
    };
    assert_eq!(created.iter().map(|item| item.id).collect::<Vec<_>>(), [1]);

    app.load_playlists(PlaylistScope::Subscribed);
    let Content::Playlists(subscribed) = &app.content else {
        panic!("expected cached playlists");
    };
    assert_eq!(
        subscribed.iter().map(|item| item.id).collect::<Vec<_>>(),
        [2]
    );
}

#[test]
fn warmed_playlist_tracks_open_without_an_async_column_task() {
    let (_directory, mut app) = test_app();
    app.focus = Focus::Content;
    app.playlist_track_cache.insert(
        42,
        (
            "缓存歌单".into(),
            vec![OnlineTrack {
                id: 9,
                title: "Cached Song".into(),
                artists: "Artist".into(),
                album: "Album".into(),
                duration_ms: 180_000,
                cover_url: String::new(),
            }],
        ),
    );

    app.open_playlist(42, "旧标题".into());

    assert!(app.column_tasks.is_empty());
    assert_eq!(app.focus, Focus::Column(0));
    assert_eq!(app.columns[0].title, "缓存歌单");
    assert_eq!(app.columns[0].phase, ColumnPhase::Ready);
    assert_eq!(app.columns[0].content.len(), 1);
}

#[test]
fn warmed_primary_routes_open_without_an_async_load() {
    let (_directory, mut app) = test_app();
    app.identity = Some(Identity {
        user_id: 7,
        nickname: "测试用户".into(),
    });
    let track = OnlineTrack {
        id: 9,
        title: "Cached Song".into(),
        artists: "Artist".into(),
        album: "Album".into(),
        duration_ms: 180_000,
        cover_url: "https://p1.music.126.net/cover.jpg".into(),
    };
    app.daily_cache = Some(vec![track.clone()]);
    app.recommended_cache = Some(vec![PlaylistSummary {
        id: 42,
        name: "缓存歌单".into(),
        track_count: 1,
        created_by_user: false,
        special_type: 0,
    }]);
    app.listening_week_cache = Some(vec![RankedTrack {
        track,
        play_count: 12,
        score: 100,
    }]);

    app.load_daily();
    assert!(app.online_task.is_none());
    assert_eq!(app.content.len(), 1);

    app.load_recommended();
    assert!(app.online_task.is_none());
    assert_eq!(app.content.len(), 1);

    app.load_listening_rank();
    assert!(app.online_task.is_none());
    assert_eq!(app.content.len(), 1);
}

#[test]
fn cache_only_events_do_not_request_a_redraw() {
    let (_directory, mut app) = test_app();
    app.event_tx.send(AppEvent::DailyWarmed(vec![])).unwrap();
    app.event_tx
        .send(AppEvent::RecommendedWarmed(vec![]))
        .unwrap();
    app.event_tx
        .send(AppEvent::UserPlaylistsWarmed(vec![]))
        .unwrap();
    app.event_tx
        .send(AppEvent::ListeningRankWarmed(ListeningRank::Week, vec![]))
        .unwrap();
    app.event_tx.send(AppEvent::MetadataWarmFinished).unwrap();

    assert!(!app.poll_events());
    assert!(app.daily_cache.is_some());
    assert!(app.recommended_cache.is_some());
    assert!(app.playlist_cache.is_some());
    assert!(app.listening_week_cache.is_some());
}

#[test]
fn animation_is_disabled_when_idle_and_enabled_during_activity() {
    let (_directory, mut app) = test_app();
    app.loading = false;
    app.columns.clear();
    app.active_job = None;
    app.lyrics = LyricsState::Idle;
    app.player_state = PlayerState::default();
    assert!(!app.needs_animation());

    app.loading = true;
    assert!(app.needs_animation());
}

#[test]
fn breadcrumb_follows_the_cascade_and_omits_adjacent_duplicates() {
    let (_directory, mut app) = test_app();
    app.route = Route::Recommended;
    app.title = "为你精选".into();
    app.columns = vec![
        BrowserColumn::ready(1, "电子音乐".into(), Content::Empty),
        BrowserColumn::ready(2, "夜航".into(), Content::Empty),
        BrowserColumn::ready(3, "夜航".into(), Content::Empty),
    ];

    assert_eq!(
        breadcrumb_text(&app),
        "推荐歌单 › 为你精选 › 电子音乐 › 夜航"
    );
}

#[test]
fn breadcrumb_truncation_preserves_the_active_tail() {
    let truncated = truncate_to_width("推荐歌单 › 为你精选 › 电子音乐 › 夜航", 12);

    assert!(text_width(&truncated) <= 12);
    assert!(truncated.starts_with('…'));
    assert!(truncated.ends_with("夜航"));
    assert_eq!(truncate_to_width("夜航", 12), "夜航");
}

#[test]
fn list_title_reports_the_clamped_selection_position() {
    assert_eq!(title_with_position("歌单", 4, 10), " 歌单 · 5/10 ");
    assert_eq!(title_with_position("歌单", 99, 10), " 歌单 · 10/10 ");
    assert_eq!(title_with_position("歌单", 0, 0), " 歌单 ");
}

#[test]
fn column_errors_are_local_and_stale_responses_are_ignored() {
    let (_directory, mut app) = test_app();
    app.title = "根内容".into();
    app.status = "根状态".into();
    app.content = Content::Tracks(vec![test_track(1)]);
    app.columns
        .push(BrowserColumn::loading(41, "在线歌单".into()));

    app.event_tx
        .send(AppEvent::ColumnLoaded(41, Err("网络超时".into())))
        .unwrap();
    app.event_tx
        .send(AppEvent::ColumnLoaded(99, Err("迟到响应".into())))
        .unwrap();
    app.poll_events();

    assert_eq!(app.title, "根内容");
    assert_eq!(app.status, "根状态");
    assert_eq!(app.content.len(), 1);
    assert_eq!(app.columns.len(), 1);
    assert_eq!(app.columns[0].phase, ColumnPhase::Error("网络超时".into()));
    assert_eq!(app.columns[0].title, "在线歌单");
}

#[test]
fn small_terminal_has_no_mouse_regions() {
    let hits = hits_for(
        Rect::new(0, 0, MIN_WIDTH - 1, MIN_HEIGHT),
        1,
        Focus::Content,
        false,
        false,
    );
    assert_eq!(hits.nav, Rect::default());
    assert_eq!(hits.content, Rect::default());
}

#[test]
fn compact_layout_keeps_navigation_and_player() {
    for width in MIN_WIDTH..COMPACT_WIDTH {
        let area = Rect::new(0, 0, width, 24);
        let layout = app_layout(layout_req(area, 1, Focus::Content, false, false)).unwrap();
        let hits = hits_for(area, 1, Focus::Content, false, false);
        let nav = layout.navigation.unwrap();
        assert_eq!(nav.x, 0);
        assert_eq!(nav.width, 16);
        assert_eq!(layout.player.y, 18);
        assert!(hits.nav.x >= 1);
        assert!(hits.content.x >= nav.right());
        assert_eq!(nav.bottom(), layout.player.bottom());
        assert_eq!(layout.player.x, nav.right());
    }
}

#[test]
fn panel_borders_are_not_interactive() {
    let hits = hits_for(Rect::new(0, 0, 96, 30), 1, Focus::Content, false, false);
    assert!(!contains(hits.nav, 0, hits.nav.y));
    assert!(!contains(hits.content, 24, hits.content.y));
    assert_eq!(hits.nav.x, 1);
    assert_eq!(hits.content.x, 25);
}

#[test]
fn wide_layout_keeps_cascade_order_and_a_separate_lyrics_column() {
    let layout = app_layout(layout_req(
        Rect::new(0, 0, 160, 30),
        4,
        Focus::Column(2),
        false,
        false,
    ))
    .unwrap();
    let lyrics = layout.lyrics.unwrap();

    assert!(layout.navigation.is_some());
    assert!(layout.browser.len() >= 2);
    assert!(
        layout
            .browser
            .windows(2)
            .all(|panes| panes[0].index < panes[1].index)
    );
    assert!(
        layout
            .browser
            .windows(2)
            .all(|panes| panes[0].area.right() == panes[1].area.x)
    );
    assert!(layout.browser.last().unwrap().area.right() <= lyrics.x);
}

#[test]
fn compact_lyrics_focus_keeps_navigation_and_player() {
    let layout = app_layout(layout_req(
        Rect::new(0, 0, 60, 24),
        3,
        Focus::Lyrics,
        false,
        true,
    ))
    .unwrap();
    let nav = layout.navigation.unwrap();
    let lyrics = layout.lyrics.unwrap();
    assert_eq!(nav.x, 0);
    assert_eq!(layout.player.y, 18);
    assert!(layout.browser.is_empty());
    assert_eq!(lyrics.x, nav.right());
    assert_eq!(lyrics.right(), 60);
}

#[test]
fn expanded_browser_keeps_navigation_and_player_fixed() {
    let area = Rect::new(0, 0, 160, 30);
    let miller = app_layout(layout_req(area, 3, Focus::Column(0), false, false)).unwrap();
    let expanded = app_layout(layout_req(area, 3, Focus::Column(0), false, true)).unwrap();
    assert_eq!(miller.navigation, expanded.navigation);
    assert_eq!(miller.player, expanded.player);
    assert!(miller.lyrics.is_some());
    assert!(expanded.lyrics.is_none());
    assert!(miller.browser.len() > 1);
    assert_eq!(expanded.browser.len(), 1);
    assert_eq!(
        expanded.browser[0].area.x,
        expanded.navigation.unwrap().right()
    );
    assert_eq!(expanded.browser[0].area.right(), area.right());
}

#[test]
fn lyric_wrap_breaks_cjk_and_latin_at_the_panel_width() {
    assert_eq!(wrap_lyric_text("你好世界", 4), ["你好", "世界"]);
    assert_eq!(wrap_lyric_text("hello world", 8), ["hello", "world"]);
    assert_eq!(wrap_lyric_text("unchanged", 20), ["unchanged"]);
}

#[test]
fn player_controls_have_disjoint_hit_regions() {
    for area in [
        Rect::new(0, 0, 60, 5),
        Rect::new(0, 0, 96, 5),
        Rect::new(0, 0, 160, 5),
    ] {
        let layout = player_layout(area, 18, false, false);
        let regions = [
            layout.progress,
            layout.previous,
            layout.pause,
            layout.next,
            layout.play_mode,
            layout.volume,
        ];
        for (index, left) in regions.iter().enumerate() {
            for right in &regions[index + 1..] {
                assert!(!rects_overlap(*left, *right), "{left:?} overlaps {right:?}");
            }
        }
        assert_eq!(layout.progress.height, 1);
        assert!(layout.progress.width <= inner_rect(area).width / 2);
        assert_eq!(layout.cover.width, 0);
    }
}

#[test]
fn player_cover_sits_left_and_does_not_overlap_controls() {
    let layout = player_layout(Rect::new(0, 0, 96, 5), 18, true, false);
    assert_eq!(layout.cover, Rect::new(1, 1, 8, 3));
    assert_eq!(layout.song_info.x, layout.cover.right() + 1);
    for region in [
        layout.song_info,
        layout.elapsed,
        layout.progress,
        layout.duration,
        layout.previous,
        layout.pause,
        layout.next,
        layout.play_mode,
        layout.volume,
    ] {
        assert!(
            !rects_overlap(layout.cover, region),
            "cover {:?} overlaps {region:?}",
            layout.cover
        );
    }
}

#[test]
fn navigation_cover_reserves_a_square_slot() {
    assert_eq!(navigation_cover_rows(14, 18, true), 8);
    assert_eq!(navigation_cover_rows(20, 22, true), 11);
    assert_eq!(navigation_cover_rows(40, 24, true), 12);
    assert_eq!(navigation_cover_rows(20, 22, false), 0);
    assert_eq!(navigation_cover_rows(9, 18, true), 0);
}

#[test]
fn cover_slot_sits_in_the_l_corner_next_to_the_player() {
    let layout = app_layout(layout_req(
        Rect::new(0, 0, 96, 24),
        1,
        Focus::Content,
        false,
        false,
    ))
    .unwrap();
    let nav = layout.navigation.unwrap();
    let inner = inner_rect(nav);
    let cover = navigation_cover_area(nav, true);
    assert_eq!(cover.x, inner.x);
    assert_eq!(cover.width, inner.width);
    assert_eq!(cover.bottom(), inner.bottom());
    assert!(cover.right() <= layout.player.x);
    assert!(cover.height >= NAV_COVER_MIN_ROWS, "{cover:?}");
    assert!(
        cover.y < layout.player.bottom() && cover.bottom() > layout.player.y,
        "cover {cover:?} should sit in the left column beside the player {:?}",
        layout.player
    );
}

#[test]
fn navigation_draws_half_block_cover_at_the_bottom() {
    use image::ImageEncoder;
    let (_directory, mut app) = test_app();
    let png = {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                &[200, 40, 80, 10, 180, 90],
                1,
                2,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        bytes
    };
    app.cover_bytes = Some(png.clone());
    app.cover_nav = crate::cover::from_bytes(&png, 22, 8);
    let (screen, _) = render_app(&mut app, 96, 24);
    assert!(
        screen.contains('▀'),
        "left-column cover should paint half-blocks:\n{screen}"
    );
    let player_line = screen.lines().nth(19).unwrap_or("");
    assert!(
        player_line.contains('▀'),
        "cover should sit to the left of the player:\n{player_line}"
    );
}

#[tokio::test]
async fn load_covers_always_starts_a_fetch_when_no_embedded_art() {
    let (_directory, mut app) = test_app();
    let track = test_track(99);
    app.load_covers(&track, None);
    assert!(
        app.cover_task.is_some(),
        "missing library cover_url must still ask song/detail"
    );
    assert_eq!(app.cover_track, Some(99));
}

#[tokio::test]
async fn load_covers_uses_the_track_pic_url_without_waiting_for_the_library() {
    let (_directory, mut app) = test_app();
    let mut track = test_track(7);
    track.cover_url = "https://p1.music.126.net/cover.jpg".into();
    app.load_covers(&track, None);
    assert!(app.cover_task.is_some());
}

#[test]
fn nav_and_player_share_an_l_frame() {
    let area = Rect::new(0, 0, 96, 24);
    let layout = app_layout(layout_req(area, 1, Focus::Content, false, false)).unwrap();
    let nav = layout.navigation.unwrap();
    assert_eq!(nav.x, 0);
    assert_eq!(nav.bottom(), layout.player.bottom());
    assert_eq!(layout.player.x, nav.right());
    assert!(layout.browser[0].area.x >= nav.right());
    assert!(layout.browser[0].area.bottom() <= layout.player.y);

    let (_directory, mut app) = test_app();
    let (screen, _) = render_app(&mut app, 96, 24);
    assert!(
        !screen.contains('├'),
        "player should not grow a left splitter:\n{screen}"
    );
}

#[test]
fn keyboard_and_player_buttons_share_actions() {
    let layout = player_layout(Rect::new(0, 0, 96, 5), 18, false, false);
    let hits = HitRegions {
        previous: layout.previous,
        pause: layout.pause,
        next: layout.next,
        play_mode: layout.play_mode,
        ..HitRegions::default()
    };
    let pairs = [
        (KeyCode::Char('p'), layout.previous, UiAction::PreviousTrack),
        (KeyCode::Char(' '), layout.pause, UiAction::TogglePause),
        (KeyCode::Char('n'), layout.next, UiAction::NextTrack),
        (KeyCode::Char('m'), layout.play_mode, UiAction::CycleMode),
    ];
    for (key, area, expected) in pairs {
        assert_eq!(
            normal_action(Focus::Content, true, KeyEvent::new(key, KeyModifiers::NONE)),
            Some(expected)
        );
        assert_eq!(player_control_action(&hits, area.x, area.y), Some(expected));
    }
}

#[test]
fn player_hit_widths_match_visible_controls() {
    let layout = player_layout(Rect::new(0, 0, 96, 5), 18, false, false);
    assert_eq!(layout.previous.width, text_width("p ‹"));
    assert_eq!(layout.pause.width, text_width("Space ▶"));
    assert_eq!(layout.next.width, text_width("› n"));
    assert_eq!(layout.play_mode.width, text_width("m 循环"));
}

#[test]
fn seek_requires_explicit_bracket_keys_during_playback() {
    assert_eq!(
        normal_action(
            Focus::Content,
            true,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        ),
        Some(UiAction::FocusPrevious)
    );
    assert_eq!(
        normal_action(
            Focus::Content,
            true,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        ),
        Some(UiAction::FocusNext)
    );
    assert_eq!(
        normal_action(
            Focus::Content,
            true,
            KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
        ),
        Some(UiAction::SeekBackward)
    );
    assert_eq!(
        normal_action(
            Focus::Content,
            true,
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        ),
        Some(UiAction::SeekForward)
    );
    assert_eq!(
        normal_action(
            Focus::Content,
            false,
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        ),
        None
    );
}

#[test]
fn modified_normal_keys_do_not_trigger_plain_bindings() {
    assert_eq!(
        normal_action(
            Focus::Content,
            true,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        ),
        None
    );
    assert_eq!(
        normal_action(
            Focus::Content,
            true,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
        ),
        None
    );
}

#[test]
fn input_editor_handles_utf8_cursor_insert_and_delete() {
    let mut input = "初音".to_owned();
    let mut cursor = input.len();
    edit_input(
        &mut input,
        &mut cursor,
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
    );
    edit_input(
        &mut input,
        &mut cursor,
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
    );
    assert_eq!(input, "初?音");
    edit_input(
        &mut input,
        &mut cursor,
        KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
    );
    assert_eq!(input, "初?");
    edit_input(
        &mut input,
        &mut cursor,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(input, "初");
}

#[tokio::test]
async fn question_mark_is_input_before_it_is_help() {
    let (_directory, mut app) = test_app();
    app.mode = InputMode::Search;
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
    )
    .await;
    assert_eq!(app.input, "?");
    assert!(!app.show_help);

    app.mode = InputMode::Normal;
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
    )
    .await;
    assert!(app.show_help);
}

#[tokio::test]
async fn help_overlay_scrolls_and_mouse_click_closes_it() {
    let (_directory, mut app) = test_app();
    app.show_help = true;
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
    )
    .await;
    assert_eq!(app.help_scroll, 6);
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(app.help_scroll, HELP_MAX_SCROLL);
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(!app.show_help);
}

#[tokio::test]
async fn back_closes_only_the_active_column_and_preserves_left_state() {
    let (_directory, mut app) = test_app();
    app.route = Route::Created;
    app.focus = Focus::Column(1);
    app.title = "创建的歌单".into();
    app.selected = 2;
    app.nav_selected = 7;
    app.content = Content::Tracks(vec![test_track(1), test_track(2), test_track(3)]);
    app.columns = vec![
        BrowserColumn {
            selected: 1,
            ..BrowserColumn::ready(
                1,
                "第一层".into(),
                Content::Tracks(vec![test_track(4), test_track(5)]),
            )
        },
        BrowserColumn::ready(2, "第二层".into(), Content::Tracks(vec![test_track(6)])),
    ];

    app.back();

    assert_eq!(app.route, Route::Created);
    assert_eq!(app.focus, Focus::Column(0));
    assert_eq!(app.title, "创建的歌单");
    assert_eq!(app.selected, 2);
    assert_eq!(app.nav_selected, 7);
    assert_eq!(app.content.len(), 3);
    assert_eq!(app.columns.len(), 1);
    assert_eq!(app.columns[0].selected, 1);

    app.back();
    assert_eq!(app.focus, Focus::Content);
    assert!(app.columns.is_empty());
}

#[tokio::test]
async fn lyrics_focus_expands_without_changing_browser_state() {
    let (_directory, mut app) = test_app();
    app.content = Content::Tracks(vec![test_track(1), test_track(2)]);
    app.selected = 1;
    app.columns = vec![BrowserColumn {
        selected: 1,
        ..BrowserColumn::ready(
            1,
            "歌单".into(),
            Content::Tracks(vec![test_track(3), test_track(4)]),
        )
    }];
    app.focus = Focus::Column(0);

    app.toggle_lyrics_focus();
    assert_eq!(app.focus, Focus::Lyrics);
    assert!(!app.expanded);
    assert_eq!(app.selected, 1);
    assert_eq!(app.columns[0].selected, 1);

    app.toggle_expand();
    assert!(app.expanded);
    assert_eq!(app.focus, Focus::Lyrics);
    assert_eq!(app.selected, 1);
    assert_eq!(app.columns[0].selected, 1);

    app.toggle_expand();
    assert!(!app.expanded);
    assert_eq!(app.focus, Focus::Lyrics);
    assert_eq!(app.selected, 1);
    assert_eq!(app.columns[0].selected, 1);
}

#[test]
fn hiding_lyrics_gives_the_browser_the_full_workspace() {
    let (_directory, mut app) = test_app();
    app.set_lyrics_hidden(true);
    let shown = app_layout(layout_req(
        Rect::new(0, 0, 96, 24),
        1,
        Focus::Content,
        false,
        false,
    ))
    .unwrap();
    let hidden = app_layout(layout_req(
        Rect::new(0, 0, 96, 24),
        1,
        Focus::Content,
        true,
        false,
    ))
    .unwrap();
    assert!(shown.lyrics.is_some());
    assert!(hidden.lyrics.is_none());
    assert!(hidden.browser[0].area.width > shown.browser[0].area.width);
    assert!(app.lyrics_hidden);
    assert!(
        std::fs::read_to_string(&app.services.ui_state_path)
            .unwrap()
            .contains("true")
    );
}

#[tokio::test]
async fn identity_reloads_an_empty_collection_route() {
    let (_directory, mut app) = test_app();
    app.route = Route::Created;
    app.content = Content::Empty;
    app.status = "离线 · 按 L 登录网易云".into();
    app.apply_identity(Identity {
        user_id: 7,
        nickname: "测试".into(),
    });
    assert_eq!(app.identity.as_ref().map(|item| item.user_id), Some(7));
    assert!(app.loading || app.online_task.is_some() || app.paged_source.is_some());
}

#[tokio::test]
async fn footer_is_bounded_and_minimum_sizes_render() {
    let (_directory, mut app) = test_app();
    app.status = "一条很长的状态消息，不应该让底栏换行或污染其他点击区域".repeat(4);
    for width in [60, 80] {
        let footer = footer_text(&app, width);
        assert!(text_width(&footer) <= width);
        assert!(footer.contains("? 帮助"));
        let (screen, _) = render_app(&mut app, width, MIN_HEIGHT);
        assert!(!screen.contains("终端太小"));
        assert!(screen.contains("♫"));
        assert!(
            screen.replace(' ', "").contains("?帮助"),
            "{width}-column screen:\n{screen}"
        );
    }
}

#[tokio::test]
async fn compact_navigation_stays_visible_without_covering_player() {
    let (_directory, mut app) = test_app();
    app.focus = Focus::Content;
    let (screen, _) = render_app(&mut app, 60, 24);
    let compact = screen.replace(' ', "");
    assert!(compact.contains("每日推荐"));
    assert!(compact.contains("播放器"));
    assert!(!compact.contains("终端太小"));
}

#[tokio::test]
async fn expand_survives_route_change_and_fills_workspace() {
    let (_directory, mut app) = test_app();
    app.focus = Focus::Content;
    app.content = Content::Tracks(vec![test_track(1)]);
    app.toggle_expand();
    assert!(app.expanded);
    app.set_route(Route::Daily);
    assert!(app.expanded);
    assert_eq!(app.route, Route::Daily);
    let expanded = app_layout(layout_req(
        Rect::new(0, 0, 96, 24),
        1,
        Focus::Content,
        false,
        true,
    ))
    .unwrap();
    assert!(expanded.lyrics.is_none());
    assert_eq!(expanded.browser.len(), 1);
    assert_eq!(
        expanded.browser[0].area.width,
        96 - expanded.navigation.unwrap().width
    );
    app.toggle_expand();
    assert!(!app.expanded);
}

#[test]
fn clip_cell_pads_and_ellipsizes_by_display_width() {
    assert_eq!(clip_cell("ab", 4), "ab  ");
    assert_eq!(clip_cell("你好世界", 5), "你好…");
    assert_eq!(text_width(&clip_cell("超长标题不会换行", 6)), 6);
}

#[test]
fn expanded_track_table_renders_headers() {
    let (_directory, mut app) = test_app();
    app.focus = Focus::Content;
    app.expanded = true;
    app.content = Content::Tracks(vec![test_track(1)]);
    let (screen, _) = render_app(&mut app, 96, 24);
    let compact = screen.replace(' ', "");
    assert!(compact.contains("歌名"));
    assert!(compact.contains("歌手"));
    assert!(compact.contains("专辑"));
    assert!(compact.contains("时长"));
    assert!(compact.contains("Song1"));
}

#[tokio::test]
async fn local_search_uses_local_copy_and_escape_restores_the_library() {
    let (_directory, mut app) = test_app();
    app.route = Route::Local;
    app.focus = Focus::Content;
    app.mode = InputMode::LocalSearch;
    app.input = "爵士".into();
    app.input_cursor = app.input.len();
    app.loading = false;
    app.content = Content::Empty;

    let (screen, _) = render_app(&mut app, 80, 24);
    let compact = screen.replace(' ', "");
    assert!(compact.contains("搜索本地音乐：爵士"));
    assert!(!compact.contains("搜索网易云"));

    dispatch_action(&mut app, UiAction::Back);
    assert!(app.mode == InputMode::Normal);
    assert!(app.input.is_empty());
    assert!(!app.loading);
    assert!(matches!(app.content, Content::LocalMenu(_)));
}

#[tokio::test]
async fn command_palette_filters_and_jumps_to_a_route() {
    let (_directory, mut app) = test_app();
    dispatch_action(&mut app, UiAction::Palette);
    assert_eq!(app.mode, InputMode::Palette);
    app.input = "导入".into();
    app.input_cursor = app.input.len();
    let filtered = app.filtered_palette();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].1, PaletteCommand::Import);
    app.palette_selected = 0;
    app.run_palette_selection();
    assert_eq!(app.mode, InputMode::Import);
    assert_eq!(app.route, Route::Local);
}

#[tokio::test]
async fn offline_daily_route_uses_warm_cache() {
    let (_directory, mut app) = test_app();
    app.identity = None;
    app.daily_cache = Some(vec![OnlineTrack {
        id: 9,
        title: "Cached Song".into(),
        artists: "Artist".into(),
        album: "Album".into(),
        duration_ms: 180_000,
        cover_url: String::new(),
    }]);
    app.load_daily();
    assert!(matches!(app.content, Content::Tracks(ref tracks) if tracks.len() == 1));
    assert!(app.status.contains("离线缓存"));
}

#[test]
fn paged_title_marks_incomplete_windows() {
    let page = PaginationInfo {
        offset: 50,
        limit: 50,
        has_more: true,
        total: 80,
        loading: false,
    };
    assert_eq!(title_with_page("歌单", 0, 50, page), " 歌单 · 1/80+ ");
}

#[tokio::test]
async fn retry_reloads_an_errored_column() {
    let (_directory, mut app) = test_app();
    app.columns.push(BrowserColumn {
        phase: ColumnPhase::Error("timeout".into()),
        source: Some(PagedSource::Playlist {
            id: 1,
            name: "夜航".into(),
        }),
        ..BrowserColumn::ready(9, "夜航".into(), Content::Empty)
    });
    app.focus = Focus::Column(0);
    assert!(app.retry_focused_error());
    assert!(matches!(app.columns[0].phase, ColumnPhase::Loading));
    assert!(app.columns[0].pagination.loading);
}

#[tokio::test]
async fn empty_local_library_shows_import_entry() {
    let (_directory, mut app) = test_app();
    let started = Instant::now();
    loop {
        let _ = app.poll_events();
        if !app.loading {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for local library, status={}",
            app.status
        );
        tokio::task::yield_now().await;
    }
    app.focus = Focus::Content;
    let (screen, _) = render_app(&mut app, 80, 24);
    let compact = screen.replace(' ', "");
    assert!(
        compact.contains("全部歌曲") || compact.contains("按I或点击这里导入"),
        "empty local screen:\n{screen}"
    );
    let footer = footer_text(&app, 80);
    assert!(footer.contains("I 导入"), "{footer}");
}

#[tokio::test]
async fn local_footer_calls_refresh_a_scan() {
    let (_directory, mut app) = test_app();
    app.route = Route::Local;
    app.focus = Focus::Content;
    app.status.clear();
    let footer = footer_text(&app, 100);
    assert!(footer.contains("I 导入"));
    assert!(footer.contains("r 扫描"));
    assert!(!footer.contains("r 刷新"));
}

#[tokio::test]
async fn import_prompt_opens_from_shortcut_and_rejects_missing_path() {
    let (_directory, mut app) = test_app();
    dispatch_action(&mut app, UiAction::Import);
    assert_eq!(app.route, Route::Local);
    assert_eq!(app.mode, InputMode::Import);
    assert!(app.status.contains("导入"));

    app.input = "/definitely-not-a-real-ncm-import-path".into();
    app.input_cursor = app.input.len();
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
    assert_eq!(app.mode, InputMode::Import);
    assert!(app.status.contains("路径不存在"));
    assert!(app.active_job.is_none());
}

#[tokio::test]
async fn import_indexes_local_audio_into_the_library() {
    let (directory, mut app) = test_app();
    let music = directory.path().join("incoming");
    std::fs::create_dir(&music).unwrap();
    std::fs::write(music.join("Artist - Song.mp3"), b"audio").unwrap();

    dispatch_action(&mut app, UiAction::Import);
    app.input = music.to_string_lossy().into_owned();
    app.input_cursor = app.input.len();
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
    assert_eq!(app.mode, InputMode::Normal);
    assert!(app.active_job.is_some());

    let started = Instant::now();
    loop {
        let _ = app.poll_events();
        if app.status.starts_with("导入完成") {
            break;
        }
        if app.status.contains("失败") || app.status.contains("error") {
            panic!("import failed: {}", app.status);
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for import, status={}",
            app.status
        );
        tokio::task::yield_now().await;
    }

    let tracks = app.services.library.search("Song", 10).unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].artists, "Artist");
    assert!(
        app.services
            .library_roots
            .iter()
            .any(|root| root == &music.canonicalize().unwrap())
    );
}

#[test]
fn parse_import_path_accepts_quoted_and_existing_directories() {
    let directory = tempfile::tempdir().unwrap();
    let quoted = format!("\"{}\"", directory.path().display());
    let parsed = parse_import_path(&quoted).unwrap();
    assert_eq!(parsed, directory.path().canonicalize().unwrap());
    assert!(parse_import_path("   ").is_err());
    assert!(parse_import_path("/definitely-not-a-real-ncm-import-path").is_err());
}

#[cfg(not(windows))]
#[test]
fn unescape_shell_path_keeps_spaces_from_escaped_drop_paths() {
    assert_eq!(unescape_shell_path("/tmp/My\\ Music"), "/tmp/My Music");
    assert_eq!(unescape_shell_path("/tmp/plain"), "/tmp/plain");
}

#[tokio::test]
async fn input_uses_terminal_cursor_and_inactive_selection_is_dimmed() {
    let (_directory, mut app) = test_app();
    app.mode = InputMode::Search;
    app.input = "初音".into();
    app.input_cursor = "初".len();
    let (_, cursor) = render_app(&mut app, 80, 24);
    assert_eq!(cursor.1, 23);
    assert_eq!(cursor.0, text_width(" / 搜索  初"));

    let inactive = selection_style(&app, false);
    let active = selection_style(&app, true);
    assert!(inactive.add_modifier.contains(Modifier::DIM));
    assert!(!inactive.add_modifier.contains(Modifier::BOLD));
    assert_eq!(active.bg, Some(app.theme.panel_highlight));
    assert!(active.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn parses_playback_cache_sizes_as_binary_units() {
    assert_eq!(parse_cache_size("512M"), Ok(512 * 1024 * 1024));
    assert_eq!(parse_cache_size("4G"), Ok(4 * 1024 * 1024 * 1024));
    assert_eq!(parse_cache_size("8GiB"), Ok(8 * 1024 * 1024 * 1024));
    assert_eq!(parse_cache_size("1.5G"), Ok(1536 * 1024 * 1024));
    assert!(parse_cache_size("0").is_err());
    assert!(parse_cache_size("huge").is_err());
}

#[tokio::test]
async fn account_shows_cache_controls_at_minimum_sizes() {
    let (_directory, mut app) = test_app();
    app.route = Route::Account;
    app.focus = Focus::Content;
    for width in [60, 80] {
        let (screen, _) = render_app(&mut app, width, 24);
        let compact = screen.replace(' ', "");
        assert!(compact.contains("播放缓存"), "{width} columns:\n{screen}");
        assert!(compact.contains("4.0GiB"), "{width} columns:\n{screen}");
        assert!(compact.contains("s设置上限"), "{width} columns:\n{screen}");
    }
}

#[tokio::test]
async fn clearing_cache_requires_a_second_confirmation() {
    let (_directory, mut app) = test_app();
    app.route = Route::Account;
    dispatch_action(&mut app, UiAction::ClearCache);
    assert!(app.cache_clear_armed_until.is_some());
    assert!(app.cache_task.is_none());

    dispatch_action(&mut app, UiAction::ClearCache);
    assert!(app.cache_clear_armed_until.is_none());
    assert!(app.cache_task.is_some());
    let started = Instant::now();
    loop {
        let _ = app.poll_events();
        if app.status.contains("已清除") {
            break;
        }
        if app.status.contains("失败") {
            panic!("cache clear failed: {}", app.status);
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for cache clear, status={}",
            app.status
        );
        tokio::task::yield_now().await;
    }
}

fn test_track(id: u64) -> TrackRow {
    TrackRow {
        id,
        title: format!("Song {id}"),
        artists: "Artist".into(),
        album: "Album".into(),
        duration_ms: 180_000,
        path: None,
        favorite: false,
        play_count: 0,
        format: String::new(),
        bytes: 0,
        cover_url: String::new(),
    }
}

#[test]
fn progress_seek_maps_the_complete_track() {
    let track = Rect::new(10, 4, 21, 1);
    assert_eq!(progress_ratio_at(track, 10), Some(0.0));
    assert_eq!(progress_ratio_at(track, 20), Some(0.5));
    assert_eq!(progress_ratio_at(track, 30), Some(1.0));
    assert_eq!(progress_ratio_at(track, 31), None);
}

fn rects_overlap(left: Rect, right: Rect) -> bool {
    left.x < right.right()
        && right.x < left.right()
        && left.y < right.bottom()
        && right.y < left.bottom()
}

#[test]
fn qr_layout_preserves_outer_quiet_zone() {
    let layout = qr_layout(Rect::new(10, 5, 60, 32), 37, 19).unwrap();
    assert_eq!(layout.code.x - layout.card.x, QR_PADDING_X);
    assert_eq!(layout.code.y - layout.card.y, QR_PADDING_Y);
    assert_eq!(layout.card.right() - layout.code.right(), QR_PADDING_X);
    assert_eq!(layout.card.bottom() - layout.code.bottom(), QR_PADDING_Y);
    assert_eq!(layout.status.y - layout.card.bottom(), QR_STATUS_GAP);
}

#[test]
fn qr_layout_rejects_clipped_rendering() {
    assert!(qr_layout(Rect::new(0, 0, 40, 20), 41, 19).is_none());
}

#[test]
fn miku_theme_uses_transparent_base_and_turquoise_accent() {
    let theme = Theme::miku();
    assert_eq!(theme.background, Color::Reset);
    assert_eq!(theme.accent, Color::Rgb(57, 197, 187));
}
