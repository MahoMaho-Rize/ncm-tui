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

pub(super) struct App {
    pub(super) services: Services,
    pub(super) theme: Theme,
    pub(super) route: Route,
    pub(super) focus: Focus,
    pub(super) content: Content,
    pub(super) title: String,
    pub(super) selected: usize,
    pub(super) nav_selected: usize,
    pub(super) input: String,
    pub(super) input_cursor: usize,
    pub(super) mode: InputMode,
    pub(super) search_kind: SearchKind,
    pub(super) status: String,
    pub(super) loading: bool,
    pub(super) columns: Vec<BrowserColumn>,
    pub(super) show_help: bool,
    pub(super) help_scroll: u16,
    pub(super) show_details: bool,
    pub(super) tick: usize,
    pub(super) identity: Option<Identity>,
    pub(super) challenge: Option<QrChallenge>,
    pub(super) qr: String,
    pub(super) login_status: String,
    pub(super) login_polling: bool,
    pub(super) next_login_poll: Instant,
    pub(super) player: Option<Player>,
    pub(super) current: Option<u64>,
    pub(super) current_track: Option<TrackRow>,
    pub(super) play_queue: Vec<TrackRow>,
    pub(super) queue_index: Option<usize>,
    pub(super) play_mode: PlayMode,
    pub(super) completion_latched: bool,
    pub(super) expanded: bool,
    pub(super) lyrics_hidden: bool,
    pub(super) previous_focus: Focus,
    pub(super) lyrics: LyricsState,
    pub(super) lyrics_task: Option<JoinHandle<()>>,
    pub(super) next_job_id: u64,
    pub(super) active_job: Option<ActiveJob>,
    pub(super) online_task: Option<JoinHandle<()>>,
    pub(super) column_tasks: HashMap<u64, JoinHandle<()>>,
    pub(super) auth_task: Option<JoinHandle<()>>,
    pub(super) generation: u64,
    pub(super) stream_task: Option<JoinHandle<()>>,
    pub(super) stream_generation: u64,
    pub(super) event_tx: mpsc::UnboundedSender<AppEvent>,
    pub(super) event_rx: mpsc::UnboundedReceiver<AppEvent>,
    pub(super) hits: HitRegions,
    pub(super) last_click: Option<(usize, usize, Instant)>,
    pub(super) local_stats: Option<LibraryStats>,
    pub(super) player_state: PlayerState,
    pub(super) daily_cache: Option<Vec<OnlineTrack>>,
    pub(super) recommended_cache: Option<Vec<PlaylistSummary>>,
    pub(super) playlist_cache: Option<Vec<PlaylistSummary>>,
    pub(super) playlist_track_cache: HashMap<u64, (String, Vec<OnlineTrack>)>,
    pub(super) listening_week_cache: Option<Vec<RankedTrack>>,
    pub(super) listening_all_cache: Option<Vec<RankedTrack>>,
    pub(super) album_cache: Option<Vec<AlbumSummary>>,
    pub(super) artist_cache: Option<Vec<ArtistSummary>>,
    pub(super) metadata_warm_task: Option<JoinHandle<()>>,
    pub(super) cache_task: Option<JoinHandle<()>>,
    pub(super) cache_clear_armed_until: Option<Instant>,
    pub(super) pagination: PaginationInfo,
    pub(super) paged_source: Option<PagedSource>,
    pub(super) toasts: Vec<Toast>,
    pub(super) palette_selected: usize,
    pub(super) local_layer: LocalLayer,
    pub(super) local_sort: TrackSort,
    pub(super) cover_bytes: Option<Vec<u8>>,
    pub(super) cover_player: Option<crate::cover::CoverArt>,
    pub(super) cover_nav: Option<crate::cover::CoverArt>,
    pub(super) cover_picker: ratatui_image::picker::Picker,
    pub(super) cover_protocol_player: Option<ratatui_image::protocol::StatefulProtocol>,
    pub(super) cover_protocol_nav: Option<ratatui_image::protocol::StatefulProtocol>,
    pub(super) cover_track: Option<u64>,
    pub(super) cover_task: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LocalLayer {
    Menu,
    Tracks(TrackView),
    Album(String),
    Artist(String),
}

impl App {
    pub(super) fn new(services: Services) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (player, status) = match Player::new() {
            Ok(player) => (Some(player), String::new()),
            Err(_) => (None, "音频输出不可用".into()),
        };
        let player_state = player.as_ref().map(Player::state).unwrap_or_default();
        let lyrics_hidden = load_hide_lyrics(&services.ui_state_path);
        let mut app = Self {
            services,
            theme: Theme::from_env(),
            route: Route::Local,
            focus: Focus::Navigation,
            content: Content::Empty,
            title: Route::Local.label().into(),
            selected: 0,
            nav_selected: Route::ALL
                .iter()
                .position(|route| *route == Route::Local)
                .unwrap_or(0),
            input: String::new(),
            input_cursor: 0,
            mode: InputMode::Normal,
            search_kind: SearchKind::Song,
            status,
            loading: false,
            columns: Vec::new(),
            show_help: false,
            help_scroll: 0,
            show_details: false,
            tick: 0,
            identity: None,
            challenge: None,
            qr: String::new(),
            login_status: String::new(),
            login_polling: false,
            next_login_poll: Instant::now(),
            player,
            current: None,
            current_track: None,
            play_queue: Vec::new(),
            queue_index: None,
            play_mode: PlayMode::Sequential,
            completion_latched: false,
            expanded: false,
            lyrics_hidden,
            previous_focus: Focus::Content,
            lyrics: LyricsState::Idle,
            lyrics_task: None,
            next_job_id: 0,
            active_job: None,
            online_task: None,
            column_tasks: HashMap::new(),
            auth_task: None,
            generation: 0,
            stream_task: None,
            stream_generation: 0,
            event_tx,
            event_rx,
            hits: HitRegions::default(),
            last_click: None,
            local_stats: None,
            player_state,
            daily_cache: None,
            recommended_cache: None,
            playlist_cache: None,
            playlist_track_cache: HashMap::new(),
            listening_week_cache: None,
            listening_all_cache: None,
            album_cache: None,
            artist_cache: None,
            metadata_warm_task: None,
            cache_task: None,
            cache_clear_armed_until: None,
            pagination: PaginationInfo::default(),
            paged_source: None,
            toasts: Vec::new(),
            palette_selected: 0,
            local_layer: LocalLayer::Menu,
            local_sort: TrackSort::Title,
            cover_bytes: None,
            cover_player: None,
            cover_nav: None,
            cover_picker: crate::cover::build_picker(),
            cover_protocol_player: None,
            cover_protocol_nav: None,
            cover_track: None,
            cover_task: None,
        };
        app.load_local();
        #[cfg(not(test))]
        app.check_identity();
        app
    }

    #[cfg(not(test))]
    pub(super) fn check_identity(&mut self) {
        let authentication = self.services.authentication.clone();
        let tx = self.event_tx.clone();
        self.auth_task = Some(tokio::spawn(async move {
            let result = authentication
                .current_identity()
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::Identity(result));
        }));
    }

    pub(super) fn load_local_menu(&mut self) {
        self.local_layer = LocalLayer::Menu;
        self.content = Content::LocalMenu(local_menu_items());
        self.loading = false;
        self.title = "本地音乐".into();
        self.selected = 0;
        if let Ok(stats) = self.services.library.stats() {
            self.local_stats = Some(stats);
        }
    }

    pub(super) fn load_local(&mut self) {
        let route = self.route;
        if route == Route::Local && matches!(self.local_layer, LocalLayer::Menu) {
            self.load_local_menu();
            return;
        }
        if !matches!(
            route,
            Route::Favorites | Route::Local | Route::Recent | Route::Queue
        ) {
            return;
        }
        let library = self.services.library.clone();
        let title = match (&route, &self.local_layer) {
            (Route::Local, LocalLayer::Tracks(view)) => LocalMenuItem::Tracks(*view).label().into(),
            (Route::Local, LocalLayer::Album(name)) => name.clone(),
            (Route::Local, LocalLayer::Artist(name)) => name.clone(),
            _ => route.label().to_owned(),
        };
        let layer = self.local_layer.clone();
        let sort = self.local_sort;
        if tokio::runtime::Handle::try_current().is_err() {
            match query_local(&library, route, None, &layer, sort) {
                Ok((values, stats)) => {
                    self.content = Content::Tracks(values.into_iter().map(Into::into).collect());
                    self.local_stats = stats;
                }
                Err(error) => self.status = error,
            }
            self.clamp_selection();
            return;
        }
        self.spawn_load(async move {
            tokio::task::spawn_blocking(move || query_local(&library, route, None, &layer, sort))
                .await
                .map_err(|error| error.to_string())?
                .map(|(tracks, stats)| Loaded::LocalTracks(title, tracks, stats))
        });
    }

    pub(super) fn search_local(&mut self) {
        let query = self.input.trim().to_owned();
        if query.is_empty() {
            self.mode = InputMode::Normal;
            self.load_local();
            return;
        }
        self.mode = InputMode::Normal;
        let library = self.services.library.clone();
        let title = format!("本地搜索：{query}");
        self.spawn_load(async move {
            tokio::task::spawn_blocking(move || {
                query_local(
                    &library,
                    Route::Local,
                    Some(&query),
                    &LocalLayer::Tracks(TrackView::All),
                    TrackSort::Title,
                )
            })
            .await
            .map_err(|error| error.to_string())?
            .map(|(tracks, stats)| Loaded::LocalTracks(title, tracks, stats))
        });
    }

    pub(super) fn begin_import(&mut self) {
        if self.route != Route::Local {
            self.set_route(Route::Local);
            self.focus = Focus::Content;
        }
        self.mode = InputMode::Import;
        self.input.clear();
        self.input_cursor = 0;
        self.status = "输入本地文件或目录路径，Enter 导入到音乐库".into();
    }

    pub(super) fn start_import(&mut self) {
        let path = match parse_import_path(&self.input) {
            Ok(path) => path,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        self.mode = InputMode::Normal;
        self.input.clear();
        self.input_cursor = 0;
        if !self.services.library_roots.iter().any(|root| root == &path) {
            self.services.library_roots.push(path.clone());
        }
        self.start_library_scan(vec![path], JobKind::Import);
    }

    pub(super) fn start_scan(&mut self) {
        if self.services.library_roots.is_empty() {
            self.status =
                "请按 I 导入本地音乐，或在 config.toml 的 [library] dirs 中配置目录".into();
            return;
        }
        let roots = self.services.library_roots.clone();
        self.start_library_scan(roots, JobKind::Scan);
    }

    pub(super) fn start_library_scan(&mut self, roots: Vec<PathBuf>, kind: JobKind) {
        if self.active_job.is_some() {
            self.status = "已有任务正在执行".into();
            return;
        }
        let job_id = self.allocate_job_id();
        let library = self.services.library.clone();
        let tx = self.event_tx.clone();
        self.status = match kind {
            JobKind::Import => roots.first().map_or_else(
                || "正在导入本地音乐…".into(),
                |root| format!("正在导入 {} …", root.display()),
            ),
            _ => format!("正在后台扫描 {} 个音乐目录…", roots.len()),
        };
        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || library.scan(&roots))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = tx.send(AppEvent::ScanFinished(job_id, result));
        });
        self.active_job = Some(ActiveJob {
            id: job_id,
            kind,
            handle,
        });
    }

    pub(super) fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.content.len().saturating_sub(1));
    }

    pub(super) fn set_route(&mut self, route: Route) {
        if let Some(task) = self.online_task.take() {
            task.abort();
        }
        self.truncate_columns(0);
        self.route = route;
        self.nav_selected = Route::ALL
            .iter()
            .position(|item| *item == route)
            .unwrap_or(0);
        self.title = route.label().into();
        self.selected = 0;
        self.content = Content::Empty;
        self.loading = false;
        self.show_details = false;
        self.last_click = None;
        self.mode = InputMode::Normal;
        self.input.clear();
        self.input_cursor = 0;
        self.pagination = PaginationInfo::default();
        self.paged_source = None;
        match route {
            Route::Favorites => self.load_favorites(),
            Route::Local => self.load_local_menu(),
            Route::Recent | Route::Queue => self.load_local(),
            Route::Daily => self.load_daily(),
            Route::Recommended => self.load_recommended(),
            Route::ListeningRank => self.load_listening_rank(),
            Route::Created => self.load_playlists(PlaylistScope::Created),
            Route::Subscribed => self.load_playlists(PlaylistScope::Subscribed),
            Route::Artists => self.load_artists(),
            Route::Albums => self.load_albums(),
            Route::Search => {
                self.mode = InputMode::Search;
                self.input.clear();
                self.input_cursor = 0;
                self.status = "输入关键词后按 Enter 搜索网易云".into();
            }
            Route::Downloads => {
                self.status = if self.active_job.is_some() {
                    "下载任务进行中，按 Esc 可取消".into()
                } else {
                    "按 D 输入 track/album/playlist ID，可附加曲目范围".into()
                };
            }
            Route::Account => self.status.clear(),
        }
    }

    pub(super) fn requires_login(&mut self) -> bool {
        if self.identity.is_some() {
            return false;
        }
        if self.auth_task.is_some() {
            self.loading = true;
            self.status = "正在恢复登录…".into();
            return true;
        }
        self.push_toast(ToastKind::Warn, "离线 · 按 L 登录后可浏览网易云");
        self.status = "离线 · 本地音乐仍可播放，按 L 登录网易云".into();
        self.content = Content::Empty;
        true
    }

    pub(super) fn apply_identity(&mut self, identity: crate::auth::Identity) {
        self.identity = Some(identity);
        self.start_metadata_warmup();
        if matches!(
            self.route,
            Route::Daily
                | Route::Recommended
                | Route::Favorites
                | Route::ListeningRank
                | Route::Created
                | Route::Subscribed
                | Route::Artists
                | Route::Albums
        ) && (self.content.is_empty()
            || self.status.contains("离线")
            || self.status.contains("登录"))
        {
            let route = self.route;
            self.set_route(route);
        }
    }

    pub(super) fn push_toast(&mut self, kind: ToastKind, message: impl Into<String>) {
        let message = message.into();
        if message.is_empty() {
            return;
        }
        self.toasts.retain(|toast| toast.message != message);
        self.toasts.push(Toast {
            kind,
            message,
            until: Instant::now() + TOAST_TTL,
        });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    pub(super) fn prune_toasts(&mut self) -> bool {
        let before = self.toasts.len();
        let now = Instant::now();
        self.toasts.retain(|toast| toast.until > now);
        before != self.toasts.len()
    }

    pub(super) fn spawn_load<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = Result<Loaded, String>> + Send + 'static,
    {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let tx = self.event_tx.clone();
        self.loading = true;
        self.content = Content::Empty;
        self.online_task = Some(tokio::spawn(async move {
            let _ = tx.send(AppEvent::Loaded(generation, future.await));
        }));
    }

    pub(super) fn truncate_columns(&mut self, keep: usize) {
        for column in self.columns.drain(keep..) {
            if let Some(task) = self.column_tasks.remove(&column.id) {
                task.abort();
            }
        }
    }

    #[allow(dead_code)]
    pub(super) fn spawn_column<F>(&mut self, title: String, future: F)
    where
        F: std::future::Future<Output = Result<Loaded, String>> + Send + 'static,
    {
        let keep = match self.focus {
            Focus::Content => 0,
            Focus::Column(index) => index.saturating_add(1),
            _ => self.columns.len(),
        };
        self.truncate_columns(keep);
        let id = self.allocate_job_id();
        self.columns.push(BrowserColumn::loading(id, title));
        self.focus = Focus::Column(self.columns.len() - 1);
        let tx = self.event_tx.clone();
        let task = tokio::spawn(async move {
            let _ = tx.send(AppEvent::ColumnLoaded(id, future.await));
        });
        self.column_tasks.insert(id, task);
    }

    pub(super) fn spawn_column_page(&mut self, title: String, source: PagedSource) {
        let keep = match self.focus {
            Focus::Content => 0,
            Focus::Column(index) => index.saturating_add(1),
            _ => self.columns.len(),
        };
        self.truncate_columns(keep);
        let id = self.allocate_job_id();
        let mut column = BrowserColumn::loading(id, title);
        column.source = Some(source.clone());
        column.pagination.loading = true;
        column.pagination.limit = PAGE_SIZE;
        self.columns.push(column);
        self.focus = Focus::Column(self.columns.len() - 1);
        self.fetch_page(Some(id), source, 0);
    }

    pub(super) fn fetch_page(
        &mut self,
        column_id: Option<u64>,
        source: PagedSource,
        offset: usize,
    ) {
        let discovery = self.services.discovery.clone();
        let generation = self.generation;
        let tx = self.event_tx.clone();
        let task = tokio::spawn(async move {
            let result = load_source_page(discovery, source.clone(), offset).await;
            let _ = tx.send(AppEvent::PageLoaded {
                column_id,
                generation,
                source,
                offset,
                result,
            });
        });
        if let Some(id) = column_id {
            self.column_tasks.insert(id, task);
        } else {
            if let Some(existing) = self.online_task.take() {
                existing.abort();
            }
            self.online_task = Some(task);
        }
    }

    pub(super) fn maybe_prefetch(&mut self) {
        match self.focus {
            Focus::Content => {
                let Some(source) = self.paged_source.clone() else {
                    return;
                };
                if !self
                    .pagination
                    .should_prefetch(self.selected, self.content.len())
                {
                    return;
                }
                self.pagination.loading = true;
                self.fetch_page(None, source, self.pagination.offset);
            }
            Focus::Column(index) => {
                let (id, source, offset) = {
                    let Some(column) = self.columns.get(index) else {
                        return;
                    };
                    if !column
                        .pagination
                        .should_prefetch(column.selected, column.content.len())
                    {
                        return;
                    }
                    let Some(source) = column.source.clone() else {
                        return;
                    };
                    (column.id, source, column.pagination.offset)
                };
                if let Some(column) = self.columns.get_mut(index) {
                    column.pagination.loading = true;
                }
                self.fetch_page(Some(id), source, offset);
            }
            _ => {}
        }
    }

    pub(super) fn apply_page(
        &mut self,
        loaded: Loaded,
        source: PagedSource,
        append: bool,
        column_id: Option<u64>,
    ) {
        let (title, content, pagination) = match loaded {
            Loaded::TrackPage(page, _) => (
                page.title,
                Content::Tracks(page.items.into_iter().map(Into::into).collect()),
                page.pagination,
            ),
            Loaded::SearchPage(page, _) => {
                let content = match page.kind {
                    SearchKind::Song => {
                        Content::Tracks(page.tracks.into_iter().map(Into::into).collect())
                    }
                    SearchKind::Album => Content::Albums(page.albums),
                    SearchKind::Artist => Content::Artists(page.artists),
                    SearchKind::Playlist => Content::Playlists(page.playlists),
                };
                (page.title, content, page.pagination)
            }
            Loaded::Tracks(title, tracks) => (
                title,
                Content::Tracks(tracks.into_iter().map(Into::into).collect()),
                PaginationInfo::default(),
            ),
            Loaded::Playlists(values) => (
                String::new(),
                Content::Playlists(values),
                PaginationInfo::default(),
            ),
            Loaded::Albums(values) => (
                String::new(),
                Content::Albums(values),
                PaginationInfo::default(),
            ),
            Loaded::Artists(values) => (
                String::new(),
                Content::Artists(values),
                PaginationInfo::default(),
            ),
            Loaded::RankedTracks(title, values) => (
                title,
                Content::Tracks(
                    values
                        .into_iter()
                        .map(|ranked| {
                            let mut row = TrackRow::from(ranked.track);
                            row.play_count = ranked.play_count;
                            row
                        })
                        .collect(),
                ),
                PaginationInfo::default(),
            ),
            Loaded::LocalTracks(title, values, _) => (
                title,
                Content::Tracks(values.into_iter().map(Into::into).collect()),
                PaginationInfo::default(),
            ),
            Loaded::PlaylistPage(items, pagination, _) => {
                (String::new(), Content::Playlists(items), pagination)
            }
            Loaded::AlbumPage(items, pagination, _) => {
                (String::new(), Content::Albums(items), pagination)
            }
            Loaded::ArtistPage(items, pagination, _) => {
                (String::new(), Content::Artists(items), pagination)
            }
        };
        if let Some(id) = column_id {
            let Some(column) = self.columns.iter_mut().find(|column| column.id == id) else {
                return;
            };
            if append {
                merge_content(&mut column.content, content);
            } else {
                column.content = content;
                column.selected = 0;
                column.phase = ColumnPhase::Ready;
            }
            if !title.is_empty() {
                column.title = title;
            }
            column.pagination = pagination;
            column.source = Some(source);
            return;
        }
        if append {
            merge_content(&mut self.content, content);
        } else {
            self.content = content;
            self.selected = 0;
            self.loading = false;
            if !title.is_empty() {
                self.title = title;
            }
        }
        match &self.content {
            Content::Playlists(items) => {
                let cache = self.playlist_cache.get_or_insert_with(Vec::new);
                pagination::merge_unique_by_id(cache, items.clone(), |playlist| playlist.id);
            }
            Content::Albums(items) => {
                let cache = self.album_cache.get_or_insert_with(Vec::new);
                pagination::merge_unique_by_id(cache, items.clone(), |album| album.id);
            }
            Content::Artists(items) => {
                let cache = self.artist_cache.get_or_insert_with(Vec::new);
                pagination::merge_unique_by_id(cache, items.clone(), |artist| artist.id);
            }
            _ => {}
        }
        self.pagination = pagination;
        self.paged_source = Some(source.clone());
        if self.route == Route::Favorites
            && matches!(source, PagedSource::UserPlaylists { .. })
            && let Some(liked) = self
                .playlist_cache
                .as_ref()
                .and_then(|playlists| liked_playlist(playlists))
                .cloned()
        {
            self.open_liked_playlist(liked);
            return;
        }
        if self.content.is_empty() && self.pagination.has_more && !self.pagination.loading {
            self.pagination.loading = true;
            self.loading = true;
            let offset = self.pagination.offset;
            self.fetch_page(None, source, offset);
        }
    }

    pub(super) fn retry_focused_error(&mut self) -> bool {
        let Focus::Column(index) = self.focus else {
            return false;
        };
        let Some(column) = self.columns.get(index) else {
            return false;
        };
        if !matches!(column.phase, ColumnPhase::Error(_)) {
            return false;
        }
        let Some(source) = column.source.clone() else {
            return false;
        };
        let id = column.id;
        if let Some(column) = self.columns.get_mut(index) {
            column.phase = ColumnPhase::Loading;
            column.pagination.loading = true;
            column.content = Content::Empty;
        }
        self.fetch_page(Some(id), source, 0);
        true
    }

    pub(super) fn open_palette(&mut self) {
        self.show_help = false;
        self.show_details = false;
        self.mode = InputMode::Palette;
        self.input.clear();
        self.input_cursor = 0;
        self.palette_selected = 0;
        self.status = "Ctrl+P 命令面板 · 输入筛选，Enter 执行".into();
    }

    pub(super) fn palette_entries(&self) -> Vec<(PaletteItem, PaletteCommand)> {
        let mut entries = Vec::new();
        for route in Route::ALL {
            entries.push((
                PaletteItem::new(route.label(), "跳转", route.group()),
                PaletteCommand::Route(route),
            ));
        }
        entries.extend([
            (
                PaletteItem::new("导入本地音乐", "I", "import scan folder"),
                PaletteCommand::Import,
            ),
            (
                PaletteItem::new("扫描音乐目录", "r", "scan library refresh"),
                PaletteCommand::Scan,
            ),
            (
                PaletteItem::new("整理当前文件", "o", "organize"),
                PaletteCommand::Organize,
            ),
            (
                PaletteItem::new("扫码登录", "L", "login qr"),
                PaletteCommand::Login,
            ),
            (
                PaletteItem::new("新建下载", "D", "download"),
                PaletteCommand::Download,
            ),
            (
                PaletteItem::new("聚焦歌词", "l", "lyrics"),
                PaletteCommand::ToggleLyrics,
            ),
            (
                PaletteItem::new("播放/暂停", "Space", "pause play"),
                PaletteCommand::TogglePause,
            ),
            (
                PaletteItem::new("下一首", "n", "next"),
                PaletteCommand::NextTrack,
            ),
            (
                PaletteItem::new("上一首", "p", "previous"),
                PaletteCommand::PreviousTrack,
            ),
            (
                PaletteItem::new("歌曲详情", "i", "details"),
                PaletteCommand::Details,
            ),
            (
                PaletteItem::new("快捷键帮助", "?", "help"),
                PaletteCommand::Help,
            ),
            (
                PaletteItem::new("关闭或打开歌词栏", "h", "lyrics hide panel"),
                PaletteCommand::HideLyrics,
            ),
            (
                PaletteItem::new("展开或收起浏览栏", "e", "expand miller zoom"),
                PaletteCommand::Expand,
            ),
        ]);
        entries
    }

    pub(super) fn filtered_palette(&self) -> Vec<(PaletteItem, PaletteCommand)> {
        let entries = self.palette_entries();
        let items = entries
            .iter()
            .map(|(item, _)| item.clone())
            .collect::<Vec<_>>();
        palette::filter_items(&items, &self.input)
            .into_iter()
            .filter_map(|index| entries.get(index).cloned())
            .collect()
    }

    pub(super) fn run_palette_selection(&mut self) {
        let filtered = self.filtered_palette();
        if filtered.is_empty() {
            return;
        }
        let index = self.palette_selected.min(filtered.len() - 1);
        let command = filtered[index].1;
        self.mode = InputMode::Normal;
        self.input.clear();
        self.input_cursor = 0;
        match command {
            PaletteCommand::Route(route) => {
                self.set_route(route);
                self.focus = Focus::Content;
            }
            PaletteCommand::Import => self.begin_import(),
            PaletteCommand::Scan => {
                self.set_route(Route::Local);
                self.focus = Focus::Content;
                self.start_scan();
            }
            PaletteCommand::Organize => self.start_organize(),
            PaletteCommand::Login => self.start_login(),
            PaletteCommand::Download => {
                self.set_route(Route::Downloads);
                self.focus = Focus::Content;
                self.mode = InputMode::Download;
                self.input.clear();
                self.input_cursor = 0;
            }
            PaletteCommand::ToggleLyrics => self.toggle_lyrics_focus(),
            PaletteCommand::TogglePause => {
                if let Some(player) = &mut self.player {
                    player.toggle_pause();
                }
            }
            PaletteCommand::NextTrack => self.next_track(1),
            PaletteCommand::PreviousTrack => self.next_track(-1),
            PaletteCommand::Details => self.show_details = self.selected_track().is_some(),
            PaletteCommand::Help => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            PaletteCommand::HideLyrics => self.set_lyrics_hidden(!self.lyrics_hidden),
            PaletteCommand::Expand => self.toggle_expand(),
        }
    }

    pub(super) fn load_daily(&mut self) {
        if let Some(tracks) = self.daily_cache.clone() {
            self.use_cached_tracks("每日推荐", tracks);
            if self.identity.is_none() {
                self.status = "离线缓存 · 按 L 登录后刷新".into();
            }
            return;
        }
        if self.requires_login() {
            return;
        }
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .daily_songs()
                .await
                .map(|tracks| Loaded::Tracks("每日推荐".into(), tracks))
                .map_err(|error| error.to_string())
        });
    }

    pub(super) fn load_recommended(&mut self) {
        if let Some(playlists) = self.recommended_cache.clone() {
            self.use_cached_content(Content::Playlists(playlists));
            if self.identity.is_none() {
                self.status = "离线缓存 · 按 L 登录后刷新".into();
            }
            return;
        }
        if self.requires_login() {
            return;
        }
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .recommended_playlists()
                .await
                .map(Loaded::Playlists)
                .map_err(|error| error.to_string())
        });
    }

    pub(super) fn load_playlists(&mut self, scope: PlaylistScope) {
        if let Some(playlists) = self.playlist_cache.as_ref() {
            let filtered = playlists
                .iter()
                .filter(|playlist| match scope {
                    PlaylistScope::All => true,
                    PlaylistScope::Created => {
                        playlist.created_by_user && playlist.special_type != 5
                    }
                    PlaylistScope::Subscribed => !playlist.created_by_user,
                })
                .cloned()
                .collect::<Vec<_>>();
            if !filtered.is_empty() {
                self.content = Content::Playlists(filtered);
                self.loading = false;
                self.selected = 0;
                self.status.clear();
                if self.identity.is_none() {
                    self.status = "离线缓存 · 按 L 登录后刷新".into();
                }
                return;
            }
        }
        let Some(user_id) = self.identity.as_ref().map(|identity| identity.user_id) else {
            self.requires_login();
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.content = Content::Empty;
        let source = PagedSource::UserPlaylists { user_id, scope };
        self.paged_source = Some(source.clone());
        self.pagination.loading = true;
        self.fetch_page(None, source, 0);
    }

    pub(super) fn load_albums(&mut self) {
        if let Some(albums) = self.album_cache.clone()
            && !albums.is_empty()
        {
            self.use_cached_content(Content::Albums(albums));
            return;
        }
        if self.requires_login() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.content = Content::Empty;
        self.paged_source = Some(PagedSource::SubscribedAlbums);
        self.pagination.loading = true;
        self.fetch_page(None, PagedSource::SubscribedAlbums, 0);
    }

    pub(super) fn load_artists(&mut self) {
        if let Some(artists) = self.artist_cache.clone()
            && !artists.is_empty()
        {
            self.use_cached_content(Content::Artists(artists));
            return;
        }
        if self.requires_login() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.content = Content::Empty;
        self.paged_source = Some(PagedSource::SubscribedArtists);
        self.pagination.loading = true;
        self.fetch_page(None, PagedSource::SubscribedArtists, 0);
    }

    pub(super) fn load_favorites(&mut self) {
        if let Some(liked) = self
            .playlist_cache
            .as_ref()
            .and_then(|playlists| liked_playlist(playlists))
            .cloned()
        {
            self.open_liked_playlist(liked);
            return;
        }
        if self.identity.is_some() {
            self.load_playlists(PlaylistScope::All);
            return;
        }
        self.load_local();
    }

    pub(super) fn open_liked_playlist(&mut self, liked: PlaylistSummary) {
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.content = Content::Empty;
        self.title = liked.name.clone();
        let source = PagedSource::Playlist {
            id: liked.id,
            name: liked.name,
        };
        self.paged_source = Some(source.clone());
        self.pagination.loading = true;
        self.fetch_page(None, source, 0);
    }

    pub(super) fn load_listening_rank(&mut self) {
        if let Some(tracks) = self.listening_week_cache.clone() {
            self.use_cached_ranked_tracks("本周听歌排行", tracks);
            if self.identity.is_none() {
                self.status = "离线缓存 · 按 L 登录后刷新".into();
            }
            return;
        }
        let Some(user_id) = self.identity.as_ref().map(|identity| identity.user_id) else {
            self.requires_login();
            return;
        };
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .listening_rank(user_id, ListeningRank::Week)
                .await
                .map(|tracks| Loaded::RankedTracks("本周听歌排行".into(), tracks))
                .map_err(|error| error.to_string())
        });
    }

    pub(super) fn search_online(&mut self) {
        let keyword = self.input.trim().to_owned();
        if keyword.is_empty() {
            self.status = "请输入搜索关键词".into();
            return;
        }
        self.mode = InputMode::Normal;
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.content = Content::Empty;
        self.pagination = PaginationInfo {
            loading: true,
            limit: PAGE_SIZE,
            ..PaginationInfo::default()
        };
        let source = PagedSource::Search {
            query: keyword,
            kind: self.search_kind,
        };
        self.paged_source = Some(source.clone());
        self.fetch_page(None, source, 0);
    }

    pub(super) fn use_cached_content(&mut self, content: Content) {
        self.content = content;
        self.loading = false;
        self.selected = 0;
        self.status.clear();
    }

    pub(super) fn use_cached_tracks(&mut self, title: &str, tracks: Vec<OnlineTrack>) {
        self.title = title.into();
        self.use_cached_content(Content::Tracks(
            tracks.into_iter().map(Into::into).collect(),
        ));
    }

    pub(super) fn use_cached_ranked_tracks(&mut self, title: &str, tracks: Vec<RankedTrack>) {
        self.title = title.into();
        self.use_cached_content(Content::Tracks(
            tracks
                .into_iter()
                .map(|ranked| {
                    let mut row = TrackRow::from(ranked.track);
                    row.play_count = ranked.play_count;
                    row
                })
                .collect(),
        ));
    }

    pub(super) fn open_playlist(&mut self, id: u64, name: String) {
        if let Some((cached_name, tracks)) = self.playlist_track_cache.get(&id).cloned() {
            let title = if cached_name.is_empty() {
                name.clone()
            } else {
                cached_name
            };
            self.push_cached_column(
                title,
                Content::Tracks(tracks.into_iter().map(Into::into).collect()),
            );
            if let Some(column) = self.columns.last_mut() {
                column.source = Some(PagedSource::Playlist {
                    id,
                    name: name.clone(),
                });
                let loaded = column.content.len();
                column.pagination = PaginationInfo {
                    offset: loaded,
                    limit: PAGE_SIZE,
                    has_more: loaded >= PAGE_SIZE,
                    total: loaded as u64,
                    loading: false,
                };
            }
            return;
        }
        self.spawn_column_page(name.clone(), PagedSource::Playlist { id, name });
    }

    pub(super) fn push_cached_column(&mut self, title: String, content: Content) {
        let keep = match self.focus {
            Focus::Content => 0,
            Focus::Column(index) => index.saturating_add(1),
            _ => self.columns.len(),
        };
        self.truncate_columns(keep);
        let id = self.allocate_job_id();
        self.columns.push(BrowserColumn::ready(id, title, content));
        self.focus = Focus::Column(self.columns.len() - 1);
    }

    pub(super) fn start_metadata_warmup(&mut self) {
        let Some(user_id) = self.identity.as_ref().map(|identity| identity.user_id) else {
            return;
        };
        if self.metadata_warm_task.is_some() {
            return;
        }
        let discovery = self.services.discovery.clone();
        let tx = self.event_tx.clone();
        self.metadata_warm_task = Some(tokio::spawn(async move {
            let (daily, recommended, user_playlists, albums, artists, week_rank, all_rank) = tokio::join!(
                discovery.daily_songs(),
                discovery.recommended_playlists(),
                discovery.user_playlists_page(user_id, PlaylistScope::All, 0, 200),
                discovery.subscribed_albums_page(0, PAGE_SIZE),
                discovery.subscribed_artists_page(0, PAGE_SIZE),
                discovery.listening_rank(user_id, ListeningRank::Week),
                discovery.listening_rank(user_id, ListeningRank::All),
            );

            if let Ok(tracks) = daily {
                let _ = tx.send(AppEvent::DailyWarmed(tracks));
            }
            if let Ok(tracks) = week_rank {
                let _ = tx.send(AppEvent::ListeningRankWarmed(ListeningRank::Week, tracks));
            }
            if let Ok(tracks) = all_rank {
                let _ = tx.send(AppEvent::ListeningRankWarmed(ListeningRank::All, tracks));
            }

            if let Ok(playlists) = recommended {
                let _ = tx.send(AppEvent::RecommendedWarmed(playlists));
            }
            if let Ok(page) = user_playlists {
                let _ = tx.send(AppEvent::UserPlaylistsWarmed(page.items));
            }
            if let Ok(page) = albums {
                let _ = tx.send(AppEvent::AlbumsWarmed(page.items));
            }
            if let Ok(page) = artists {
                let _ = tx.send(AppEvent::ArtistsWarmed(page.items));
            }

            let _ = tx.send(AppEvent::MetadataWarmFinished);
        }));
    }

    pub(super) fn open_album(&mut self, id: u64, name: String) {
        self.spawn_column_page(name.clone(), PagedSource::Album { id, name });
    }

    pub(super) fn open_artist(&mut self, id: u64, name: String) {
        self.spawn_column_page(name.clone(), PagedSource::Artist { id, name });
    }

    pub(super) fn back(&mut self) {
        match self.focus {
            Focus::Lyrics => {
                if self.expanded {
                    self.expanded = false;
                    self.status = "已回到分栏浏览".into();
                } else {
                    self.focus = self.valid_focus(self.previous_focus);
                }
            }
            Focus::Column(index) => {
                self.truncate_columns(index);
                self.focus = if index == 0 {
                    Focus::Content
                } else {
                    Focus::Column(index - 1)
                };
            }
            Focus::Content => {
                if self.route == Route::Local && !matches!(self.local_layer, LocalLayer::Menu) {
                    self.load_local_menu();
                } else {
                    self.focus = Focus::Navigation;
                }
            }
            Focus::Navigation => {}
        }
        self.last_click = None;
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let (len, selected) = match self.focus {
            Focus::Navigation => (Route::ALL.len(), &mut self.nav_selected),
            Focus::Content => (self.content.len(), &mut self.selected),
            Focus::Column(index) => {
                let Some(column) = self.columns.get_mut(index) else {
                    return;
                };
                (column.content.len(), &mut column.selected)
            }
            Focus::Lyrics => return,
        };
        *selected = if len == 0 {
            0
        } else {
            selected.saturating_add_signed(delta).min(len - 1)
        };
        self.maybe_prefetch();
    }

    pub(super) fn activate(&mut self) {
        if self.focus == Focus::Navigation {
            self.set_route(Route::ALL[self.nav_selected]);
            self.focus = Focus::Content;
            return;
        }
        enum Target {
            Track(TrackRow, Vec<TrackRow>, usize),
            Playlist(u64, String),
            Album(u64, String),
            Artist(u64, String),
            LocalMenu(LocalMenuItem),
            LocalAlbum(String),
            LocalArtist(String),
            Empty,
        }
        let (content, selected) = match self.focus {
            Focus::Content => (&self.content, self.selected),
            Focus::Column(index) => {
                let Some(column) = self.columns.get(index) else {
                    return;
                };
                (&column.content, column.selected)
            }
            Focus::Lyrics | Focus::Navigation => return,
        };
        let target = match content {
            Content::Tracks(tracks) => tracks
                .get(selected)
                .cloned()
                .map_or(Target::Empty, |track| {
                    Target::Track(track, tracks.clone(), selected)
                }),
            Content::Playlists(values) => values.get(selected).map_or(Target::Empty, |value| {
                Target::Playlist(value.id, value.name.clone())
            }),
            Content::Albums(values) => values.get(selected).map_or(Target::Empty, |value| {
                if self.route == Route::Local {
                    Target::LocalAlbum(value.name.clone())
                } else {
                    Target::Album(value.id, value.name.clone())
                }
            }),
            Content::Artists(values) => values.get(selected).map_or(Target::Empty, |value| {
                if self.route == Route::Local {
                    Target::LocalArtist(value.name.clone())
                } else {
                    Target::Artist(value.id, value.name.clone())
                }
            }),
            Content::LocalMenu(values) => values
                .get(selected)
                .copied()
                .map_or(Target::Empty, Target::LocalMenu),
            Content::Empty => Target::Empty,
        };
        match target {
            Target::Track(track, queue, index) => {
                self.play_queue = queue;
                self.queue_index = Some(index);
                self.play(track);
            }
            Target::Playlist(id, name) => {
                if self.route == Route::Local {
                    self.open_local_collection(id, name);
                } else {
                    self.open_playlist(id, name);
                }
            }
            Target::Album(id, name) => self.open_album(id, name),
            Target::Artist(id, name) => self.open_artist(id, name),
            Target::LocalMenu(item) => self.open_local_menu_item(item),
            Target::LocalAlbum(name) => self.open_local_named_album(name),
            Target::LocalArtist(name) => self.open_local_named_artist(name),
            Target::Empty => {
                if self.route == Route::Account && self.identity.is_none() {
                    self.start_login();
                } else if self.route == Route::Downloads {
                    self.mode = InputMode::Download;
                    self.input.clear();
                } else if self.route == Route::Local
                    && !matches!(self.local_layer, LocalLayer::Menu)
                {
                    self.begin_import();
                }
            }
        }
    }

    pub(super) fn open_local_menu_item(&mut self, item: LocalMenuItem) {
        match item {
            LocalMenuItem::Tracks(view) => {
                self.local_layer = LocalLayer::Tracks(view);
                self.selected = 0;
                self.load_local();
            }
            LocalMenuItem::Albums => match self.services.library.albums() {
                Ok(albums) => {
                    self.content = Content::Albums(
                        albums
                            .into_iter()
                            .map(|album| AlbumSummary {
                                id: 0,
                                name: album.name,
                                artists: album.artists,
                                track_count: album.tracks,
                            })
                            .collect(),
                    );
                    self.title = "本地专辑".into();
                    self.selected = 0;
                }
                Err(error) => self.status = error.to_string(),
            },
            LocalMenuItem::Artists => match self.services.library.artists() {
                Ok(artists) => {
                    self.content = Content::Artists(
                        artists
                            .into_iter()
                            .map(|artist| ArtistSummary {
                                id: 0,
                                name: artist.name,
                                album_count: 0,
                                music_count: artist.tracks,
                            })
                            .collect(),
                    );
                    self.title = "本地歌手".into();
                    self.selected = 0;
                }
                Err(error) => self.status = error.to_string(),
            },
            LocalMenuItem::Collections => match self.services.library.collections() {
                Ok(collections) => {
                    self.content = Content::Playlists(
                        collections
                            .into_iter()
                            .map(|collection| PlaylistSummary {
                                id: collection.id,
                                name: format!("{} · {}", collection.kind, collection.name),
                                track_count: collection.tracks,
                                created_by_user: true,
                                special_type: 0,
                            })
                            .collect(),
                    );
                    self.title = "本地歌单".into();
                    self.selected = 0;
                }
                Err(error) => self.status = error.to_string(),
            },
        }
    }

    pub(super) fn open_local_named_album(&mut self, name: String) {
        self.local_layer = LocalLayer::Album(name);
        self.selected = 0;
        self.load_local();
    }

    pub(super) fn open_local_named_artist(&mut self, name: String) {
        self.local_layer = LocalLayer::Artist(name);
        self.selected = 0;
        self.load_local();
    }

    pub(super) fn open_local_collection(&mut self, id: u64, name: String) {
        let library = self.services.library.clone();
        let title = name.clone();
        self.spawn_column(title, async move {
            let tracks = tokio::task::spawn_blocking(move || {
                let collections = library.collections().map_err(|error| error.to_string())?;
                let kind = collections
                    .iter()
                    .find(|collection| collection.id == id)
                    .map(|collection| collection.kind.clone())
                    .unwrap_or_else(|| "playlist".into());
                library
                    .collection_tracks(&kind, id, 10_000)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())??;
            Ok(Loaded::LocalTracks(name, tracks, None))
        });
    }

    pub(super) fn play(&mut self, track: TrackRow) {
        if let Some(path) = track
            .path
            .clone()
            .or_else(|| self.services.library.track_path(track.id).ok().flatten())
        {
            if let Some(task) = self.stream_task.take() {
                task.abort();
            }
            self.stream_generation = self.stream_generation.wrapping_add(1);
            self.start_playback(track, path);
        } else {
            self.start_stream(track);
        }
    }

    pub(super) fn start_stream(&mut self, track: TrackRow) {
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
        self.stream_generation = self.stream_generation.wrapping_add(1);
        let generation = self.stream_generation;
        let discovery = self.services.discovery.clone();
        let playback_cache = self.services.playback_cache.clone();
        let tx = self.event_tx.clone();
        let song_id = track.id;
        self.status = format!("正在连接 {}…", track.title);
        self.stream_task = Some(tokio::spawn(async move {
            let result = match PreparedStream::cached(song_id, playback_cache.clone()).await {
                Ok(Some(stream)) => Ok(stream),
                Ok(None) => match discovery
                    .playback_source(song_id, AudioQuality::Lossless)
                    .await
                {
                    Ok(source) => PreparedStream::open(song_id, source, playback_cache).await,
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error),
            };
            let _ = tx.send(AppEvent::StreamReady(generation, track, result));
        }));
    }

    pub(super) fn start_playback(&mut self, mut track: TrackRow, path: PathBuf) {
        let Some(player) = self.player.as_mut() else {
            self.status = "音频输出不可用".into();
            return;
        };
        match player.play(&path, &track.title, &track.artists) {
            Ok(()) => {
                track.path = Some(path.clone());
                self.current = Some(track.id);
                self.current_track = Some(track.clone());
                self.load_covers(&track, Some(&path));
                self.completion_latched = false;
                let _ = self.services.library.record_play(track.id);
                self.load_lyrics(track);
                self.status.clear();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    pub(super) fn load_covers(&mut self, track: &TrackRow, path: Option<&std::path::Path>) {
        if let Some(task) = self.cover_task.take() {
            task.abort();
        }
        let song_id = track.id;
        self.cover_track = Some(song_id);
        self.cover_bytes = None;
        self.cover_player = None;
        self.cover_nav = None;
        self.cover_protocol_player = None;
        self.cover_protocol_nav = None;
        if let Some(path) = path
            && let Some(bytes) = crate::cover::picture_bytes(path)
            && self.apply_cover_bytes(bytes)
        {
            return;
        }
        let known_url = crate::cover::nonempty_url(&track.cover_url).or_else(|| {
            self.services
                .library
                .track_detail(song_id)
                .ok()
                .flatten()
                .and_then(|detail| crate::cover::nonempty_url(&detail.cover_url))
        });
        let discovery = self.services.discovery.clone();
        let library = self.services.library.clone();
        let title = track.title.clone();
        let artists = track.artists.clone();
        let album = track.album.clone();
        let tx = self.event_tx.clone();
        self.cover_task = Some(tokio::spawn(async move {
            let Some((bytes, resolved_url)) = discovery
                .load_cover_image(song_id, &title, &artists, &album, known_url)
                .await
            else {
                return;
            };
            if let Some(url) = resolved_url {
                let _ = library.set_cover_url(song_id, &url);
            }
            let _ = tx.send(AppEvent::CoverLoaded(song_id, bytes));
        }));
    }

    fn apply_cover_bytes(&mut self, bytes: Vec<u8>) -> bool {
        if !crate::cover::looks_like_image(&bytes) {
            return false;
        }
        self.cover_player = None;
        self.cover_nav = None;
        self.cover_protocol_player = None;
        self.cover_protocol_nav = crate::cover::protocol_from_bytes(&self.cover_picker, &bytes);
        self.cover_bytes = Some(bytes);
        true
    }

    pub(super) fn toggle_expand(&mut self) {
        if self.expanded {
            self.expanded = false;
            self.status = "已回到分栏浏览".into();
            return;
        }
        if self.focus == Focus::Lyrics && self.lyrics_hidden {
            self.set_lyrics_hidden(false);
            self.focus = Focus::Lyrics;
        }
        if matches!(
            self.focus,
            Focus::Content | Focus::Column(_) | Focus::Lyrics | Focus::Navigation
        ) {
            if self.focus == Focus::Navigation {
                self.focus = Focus::Content;
            }
            self.expanded = true;
            self.status = "已展开当前栏 · 切页保持展开 · 按 e 回到分栏".into();
        }
    }

    pub(super) fn start_stream_playback(&mut self, track: TrackRow, stream: PreparedStream) {
        let Some(player) = self.player.as_mut() else {
            self.status = "音频输出不可用".into();
            return;
        };
        let (source, extension) = stream.into_parts();
        match player.play_source(source, Some(&extension), &track.title, &track.artists) {
            Ok(()) => {
                let path = track
                    .path
                    .clone()
                    .or_else(|| self.services.library.track_path(track.id).ok().flatten());
                self.current = Some(track.id);
                self.current_track = Some(track.clone());
                self.load_covers(&track, path.as_deref());
                self.completion_latched = false;
                let _ = self.services.library.record_play(track.id);
                self.load_lyrics(track);
                self.status.clear();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    pub(super) fn set_cache_size(&mut self) {
        let max_bytes = match parse_cache_size(&self.input) {
            Ok(value) => value,
            Err(error) => {
                self.status = error.into();
                return;
            }
        };
        if self.cache_task.is_some() {
            self.status = "缓存操作正在进行".into();
            return;
        }
        self.mode = InputMode::Normal;
        self.input.clear();
        self.input_cursor = 0;
        self.cache_clear_armed_until = None;
        self.status = format!("正在将播放缓存上限设置为 {}…", format_bytes(max_bytes));
        let cache = self.services.playback_cache.clone();
        let tx = self.event_tx.clone();
        self.cache_task = Some(tokio::spawn(async move {
            let result = cache
                .set_max_bytes(max_bytes)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::CacheLimitUpdated(result));
        }));
    }

    pub(super) fn clear_playback_cache(&mut self) {
        if self.cache_task.is_some() {
            self.status = "缓存操作正在进行".into();
            return;
        }
        let now = Instant::now();
        if !self
            .cache_clear_armed_until
            .is_some_and(|deadline| deadline >= now)
        {
            self.cache_clear_armed_until = Some(now + Duration::from_secs(5));
            self.status = "再次按 X 确认清除播放缓存（当前播放不会中断）".into();
            return;
        }
        self.cache_clear_armed_until = None;
        self.status = "正在清除播放缓存…".into();
        let cache = self.services.playback_cache.clone();
        let tx = self.event_tx.clone();
        self.cache_task = Some(tokio::spawn(async move {
            let result = cache.clear().await.map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::CacheCleared(result));
        }));
    }

    pub(super) fn load_lyrics(&mut self, track: TrackRow) {
        if let Some(task) = self.lyrics_task.take() {
            task.abort();
        }
        let song_id = track.id;
        if track.path.is_none()
            && let Some(lyrics) = self.services.discovery.cached_lyrics(song_id)
        {
            self.lyrics = LyricsState::Ready(song_id, lyrics);
            return;
        }
        self.lyrics = LyricsState::Loading(song_id);
        let discovery = self.services.discovery.clone();
        let tx = self.event_tx.clone();
        self.lyrics_task = Some(tokio::spawn(async move {
            let local = match track.path.as_ref() {
                Some(path) => Lyrics::load_local(path).await,
                None => None,
            };
            let result = if let Some(lyrics) = local.clone().filter(|lyrics| !lyrics.is_empty()) {
                Ok(lyrics)
            } else {
                match discovery.lyrics(song_id).await {
                    Ok(lyrics) if !lyrics.is_empty() => Ok(lyrics),
                    Ok(_) => Ok(local.unwrap_or_default()),
                    Err(error) => {
                        if local.as_ref().is_some_and(|lyrics| !lyrics.is_empty()) {
                            Ok(local.unwrap())
                        } else if track.path.is_some() {
                            Ok(Lyrics::default())
                        } else {
                            Err(error.to_string())
                        }
                    }
                }
            };
            let _ = tx.send(AppEvent::LyricsLoaded(song_id, result));
        }));
    }

    pub(super) fn next_track(&mut self, delta: isize) {
        if self.play_queue.is_empty() {
            let tracks = match self.focus {
                Focus::Content => match &self.content {
                    Content::Tracks(tracks) => Some((tracks, self.selected)),
                    _ => None,
                },
                Focus::Column(index) => self.columns.get(index).and_then(|column| {
                    if let Content::Tracks(tracks) = &column.content {
                        Some((tracks, column.selected))
                    } else {
                        None
                    }
                }),
                _ => None,
            };
            if let Some((tracks, selected)) = tracks {
                self.play_queue = tracks.clone();
                self.queue_index = (!tracks.is_empty()).then_some(selected.min(tracks.len() - 1));
            }
        }
        let Some(index) = self.queue_index else {
            return;
        };
        let next = index.saturating_add_signed(delta);
        if next < self.play_queue.len() {
            self.queue_index = Some(next);
            self.play(self.play_queue[next].clone());
        }
    }

    pub(super) fn handle_completion(&mut self) {
        if self.completion_latched {
            return;
        }
        self.completion_latched = true;
        let Some(index) = self.queue_index else {
            return;
        };
        let next = match self.play_mode {
            PlayMode::RepeatOne => Some(index),
            PlayMode::Sequential => (index + 1 < self.play_queue.len()).then_some(index + 1),
            PlayMode::RepeatAll => {
                (!self.play_queue.is_empty()).then_some((index + 1) % self.play_queue.len())
            }
            PlayMode::Shuffle if self.play_queue.len() > 1 => {
                let mut next = rand::thread_rng().gen_range(0..self.play_queue.len() - 1);
                if next >= index {
                    next += 1;
                }
                Some(next)
            }
            PlayMode::Shuffle => (!self.play_queue.is_empty()).then_some(0),
        };
        if let Some(next) = next {
            self.queue_index = Some(next);
            self.play(self.play_queue[next].clone());
        }
    }

    pub(super) fn cycle_play_mode(&mut self) {
        self.play_mode = self.play_mode.next();
        self.status = format!("播放模式：{}", self.play_mode.label());
    }

    pub(super) fn player_state(&self) -> &PlayerState {
        &self.player_state
    }

    pub(super) fn refresh_player_state(&mut self) {
        self.player_state = self.player.as_ref().map(Player::state).unwrap_or_default();
    }

    pub(super) fn playback_finished(&self) -> bool {
        self.player_state.finished
    }

    pub(super) fn start_login(&mut self) {
        if self.login_polling {
            return;
        }
        if let Some(task) = self.auth_task.take() {
            task.abort();
        }
        self.route = Route::Account;
        self.nav_selected = Route::ALL.len() - 1;
        self.focus = Focus::Content;
        self.challenge = None;
        self.qr.clear();
        self.login_status = "正在创建二维码…".into();
        let authentication = self.services.authentication.clone();
        let tx = self.event_tx.clone();
        self.auth_task = Some(tokio::spawn(async move {
            let result = authentication
                .begin_qr()
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::LoginStarted(result));
        }));
    }

    pub(super) fn poll_login(&mut self) {
        if !self.login_polling || self.auth_task.is_some() || Instant::now() < self.next_login_poll
        {
            return;
        }
        let Some(challenge) = self.challenge.clone() else {
            return;
        };
        self.next_login_poll = Instant::now() + Duration::from_secs(2);
        let authentication = self.services.authentication.clone();
        let tx = self.event_tx.clone();
        self.auth_task = Some(tokio::spawn(async move {
            let result = authentication
                .poll_qr(&challenge)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::LoginPolled(result));
        }));
    }

    pub(super) fn start_download(&mut self) {
        match parse_download(self.input.trim()) {
            Ok(request) => {
                self.input.clear();
                self.mode = InputMode::Normal;
                self.start_download_request(request);
            }
            Err(message) => self.status = message.into(),
        }
    }

    pub(super) fn start_download_request(&mut self, request: DownloadRequest) {
        if self.active_job.is_some() {
            self.status = "已有任务正在执行".into();
            return;
        }
        let job_id = self.allocate_job_id();
        let downloader = self.services.downloader.clone();
        let tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            let result = downloader
                .download(request)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::DownloadFinished(job_id, result));
        });
        self.active_job = Some(ActiveJob {
            id: job_id,
            kind: JobKind::Download,
            handle,
        });
    }

    pub(super) fn start_organize(&mut self) {
        let Some(id) = self.selected_track().map(|track| track.id) else {
            return;
        };
        if self.active_job.is_some() {
            self.status = "已有任务正在执行".into();
            return;
        }
        let job_id = self.allocate_job_id();
        let library = self.services.library.clone();
        let tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || library.organize_track(id))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = tx.send(AppEvent::OrganizeFinished(job_id, result));
        });
        self.active_job = Some(ActiveJob {
            id: job_id,
            kind: JobKind::Organize,
            handle,
        });
    }

    pub(super) fn allocate_job_id(&mut self) -> u64 {
        self.next_job_id = self.next_job_id.wrapping_add(1);
        self.next_job_id
    }

    pub(super) fn cancel_download(&mut self) -> bool {
        let Some(job) = self.active_job.take() else {
            return false;
        };
        if job.kind != JobKind::Download {
            self.active_job = Some(job);
            return false;
        }
        job.handle.abort();
        self.status = "下载已取消".into();
        true
    }

    pub(super) fn selected_track(&self) -> Option<&TrackRow> {
        match self.focus {
            Focus::Content => match &self.content {
                Content::Tracks(values) => values.get(self.selected),
                _ => None,
            },
            Focus::Column(index) => self.columns.get(index).and_then(|column| {
                if let Content::Tracks(values) = &column.content {
                    values.get(column.selected)
                } else {
                    None
                }
            }),
            _ => None,
        }
    }

    pub(super) fn valid_focus(&self, focus: Focus) -> Focus {
        match focus {
            Focus::Column(index) if index >= self.columns.len() => self
                .columns
                .len()
                .checked_sub(1)
                .map_or(Focus::Content, Focus::Column),
            other => other,
        }
    }

    pub(super) fn focus_next(&mut self, backwards: bool) {
        let mut order = Vec::with_capacity(self.columns.len() + 3);
        order.push(Focus::Navigation);
        order.push(Focus::Content);
        order.extend((0..self.columns.len()).map(Focus::Column));
        if !self.lyrics_hidden {
            order.push(Focus::Lyrics);
        }
        let current = order
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            current.checked_sub(1).unwrap_or(order.len() - 1)
        } else {
            (current + 1) % order.len()
        };
        self.focus = order[next];
    }

    pub(super) fn set_lyrics_hidden(&mut self, hidden: bool) {
        self.lyrics_hidden = hidden;
        if hidden && self.focus == Focus::Lyrics {
            self.focus = self.valid_focus(self.previous_focus);
        }
        save_hide_lyrics(&self.services.ui_state_path, hidden);
        self.status = if hidden {
            "已关闭歌词栏 · 按 h 再打开".into()
        } else {
            "已打开歌词栏".into()
        };
        self.push_toast(ToastKind::Success, self.status.clone());
    }

    pub(super) fn toggle_lyrics_focus(&mut self) {
        if self.lyrics_hidden {
            self.set_lyrics_hidden(false);
        }
        if self.focus != Focus::Lyrics {
            self.previous_focus = self.valid_focus(self.focus);
            self.focus = Focus::Lyrics;
        }
    }

    pub(super) fn favorite(&mut self) {
        let Some(id) = self.selected_track().map(|track| track.id) else {
            return;
        };
        self.status = match self.services.library.toggle_favorite(id) {
            Ok(Some(true)) => "已喜欢".into(),
            Ok(Some(false)) => "已取消喜欢".into(),
            Ok(None) => "请先下载该歌曲".into(),
            Err(error) => error.to_string(),
        };
        if matches!(self.route, Route::Favorites | Route::Local) {
            self.load_local();
        }
    }

    pub(super) fn enqueue(&mut self) {
        let Some(track) = self.selected_track().cloned() else {
            return;
        };
        if let Some(index) = self.queue_index
            && !self.play_queue.iter().any(|queued| queued.id == track.id)
        {
            self.play_queue.insert(index + 1, track.clone());
        }
        self.status = match self.services.library.enqueue(track.id) {
            Ok(true) => format!("下一首播放：{}", track.title),
            Ok(false) => format!("已在队列：{}", track.title),
            Err(error) => error.to_string(),
        };
    }

    pub(super) fn dequeue(&mut self) {
        if self.route != Route::Queue {
            return;
        }
        let Some(id) = self.selected_track().map(|track| track.id) else {
            return;
        };
        self.status = match self.services.library.dequeue(id) {
            Ok(true) => "已移出播放队列".into(),
            Ok(false) => String::new(),
            Err(error) => error.to_string(),
        };
        self.load_local();
    }

    pub(super) fn poll_events(&mut self) -> bool {
        let mut changed = self.prune_toasts();
        while let Ok(event) = self.event_rx.try_recv() {
            changed |= !matches!(
                &event,
                AppEvent::DailyWarmed(_)
                    | AppEvent::RecommendedWarmed(_)
                    | AppEvent::UserPlaylistsWarmed(_)
                    | AppEvent::AlbumsWarmed(_)
                    | AppEvent::ArtistsWarmed(_)
                    | AppEvent::ListeningRankWarmed(_, _)
                    | AppEvent::MetadataWarmFinished
            );
            match event {
                #[cfg(not(test))]
                AppEvent::Identity(result) => {
                    self.auth_task.take();
                    match result {
                        Ok(identity) => self.apply_identity(identity),
                        Err(_) => {
                            if self.loading && self.content.is_empty() {
                                self.loading = false;
                                self.status = "离线 · 按 L 登录网易云".into();
                            }
                        }
                    }
                }
                AppEvent::Loaded(generation, result) => {
                    if generation != self.generation {
                        continue;
                    }
                    self.online_task.take();
                    self.loading = false;
                    match result {
                        Ok(Loaded::Tracks(title, values)) => {
                            if self.route == Route::Daily {
                                self.daily_cache = Some(values.clone());
                            }
                            self.title = title;
                            self.content =
                                Content::Tracks(values.into_iter().map(Into::into).collect());
                            self.status.clear();
                        }
                        Ok(Loaded::RankedTracks(title, values)) => {
                            if self.route == Route::ListeningRank {
                                self.listening_week_cache = Some(values.clone());
                            }
                            self.title = title;
                            self.content = Content::Tracks(
                                values
                                    .into_iter()
                                    .map(|ranked| {
                                        let mut row = TrackRow::from(ranked.track);
                                        row.play_count = ranked.play_count;
                                        row
                                    })
                                    .collect(),
                            );
                            self.status.clear();
                        }
                        Ok(Loaded::Playlists(values)) => {
                            if self.route == Route::Recommended {
                                self.recommended_cache = Some(values.clone());
                            }
                            self.content = Content::Playlists(values);
                            self.status.clear();
                        }
                        Ok(Loaded::Artists(values)) => {
                            self.content = Content::Artists(values);
                            self.status.clear();
                        }
                        Ok(Loaded::Albums(values)) => {
                            self.content = Content::Albums(values);
                            self.status.clear();
                        }
                        Ok(Loaded::LocalTracks(title, values, stats)) => {
                            self.title = title;
                            self.content =
                                Content::Tracks(values.into_iter().map(Into::into).collect());
                            self.local_stats = stats;
                            if !self.status.starts_with("扫描完成")
                                && !self.status.starts_with("导入完成")
                            {
                                self.status.clear();
                            }
                        }
                        Ok(Loaded::TrackPage(page, source)) => {
                            self.apply_page(
                                Loaded::TrackPage(page, source.clone()),
                                source,
                                false,
                                None,
                            );
                        }
                        Ok(Loaded::SearchPage(page, source)) => {
                            self.apply_page(
                                Loaded::SearchPage(page, source.clone()),
                                source,
                                false,
                                None,
                            );
                        }
                        Ok(Loaded::PlaylistPage(items, pagination, source)) => {
                            self.apply_page(
                                Loaded::PlaylistPage(items, pagination, source.clone()),
                                source,
                                false,
                                None,
                            );
                        }
                        Ok(Loaded::AlbumPage(items, pagination, source)) => {
                            self.apply_page(
                                Loaded::AlbumPage(items, pagination, source.clone()),
                                source,
                                false,
                                None,
                            );
                        }
                        Ok(Loaded::ArtistPage(items, pagination, source)) => {
                            self.apply_page(
                                Loaded::ArtistPage(items, pagination, source.clone()),
                                source,
                                false,
                                None,
                            );
                        }
                        Err(error) => {
                            self.content = Content::Empty;
                            self.push_toast(ToastKind::Error, error.clone());
                            self.status = error;
                        }
                    }
                    self.selected = 0;
                }
                AppEvent::ColumnLoaded(id, result) => {
                    self.column_tasks.remove(&id);
                    match result {
                        Ok(Loaded::TrackPage(page, source)) => {
                            self.apply_page(
                                Loaded::TrackPage(page, source.clone()),
                                source,
                                false,
                                Some(id),
                            );
                        }
                        Ok(Loaded::SearchPage(page, source)) => {
                            self.apply_page(
                                Loaded::SearchPage(page, source.clone()),
                                source,
                                false,
                                Some(id),
                            );
                        }
                        other => {
                            let toast = {
                                let Some(column) =
                                    self.columns.iter_mut().find(|column| column.id == id)
                                else {
                                    continue;
                                };
                                let toast = match other {
                                    Ok(Loaded::Tracks(title, values)) => {
                                        column.phase = ColumnPhase::Ready;
                                        column.title = title;
                                        column.content = Content::Tracks(
                                            values.into_iter().map(Into::into).collect(),
                                        );
                                        None
                                    }
                                    Ok(Loaded::RankedTracks(title, values)) => {
                                        column.phase = ColumnPhase::Ready;
                                        column.title = title;
                                        column.content = Content::Tracks(
                                            values
                                                .into_iter()
                                                .map(|ranked| {
                                                    let mut row = TrackRow::from(ranked.track);
                                                    row.play_count = ranked.play_count;
                                                    row
                                                })
                                                .collect(),
                                        );
                                        None
                                    }
                                    Ok(Loaded::Playlists(values)) => {
                                        column.phase = ColumnPhase::Ready;
                                        column.content = Content::Playlists(values);
                                        None
                                    }
                                    Ok(Loaded::Artists(values)) => {
                                        column.phase = ColumnPhase::Ready;
                                        column.content = Content::Artists(values);
                                        None
                                    }
                                    Ok(Loaded::Albums(values)) => {
                                        column.phase = ColumnPhase::Ready;
                                        column.content = Content::Albums(values);
                                        None
                                    }
                                    Ok(Loaded::LocalTracks(title, values, _)) => {
                                        column.phase = ColumnPhase::Ready;
                                        column.title = title;
                                        column.content = Content::Tracks(
                                            values.into_iter().map(Into::into).collect(),
                                        );
                                        None
                                    }
                                    Err(error) => {
                                        column.content = Content::Empty;
                                        column.phase = ColumnPhase::Error(error.clone());
                                        Some(error)
                                    }
                                    Ok(
                                        Loaded::TrackPage(_, _)
                                        | Loaded::SearchPage(_, _)
                                        | Loaded::PlaylistPage(_, _, _)
                                        | Loaded::AlbumPage(_, _, _)
                                        | Loaded::ArtistPage(_, _, _),
                                    ) => None,
                                };
                                column.selected = 0;
                                toast
                            };
                            if let Some(error) = toast {
                                self.push_toast(ToastKind::Error, error);
                            }
                        }
                    }
                }
                AppEvent::PageLoaded {
                    column_id,
                    generation,
                    source,
                    offset,
                    result,
                } => {
                    if column_id.is_none() && generation != self.generation {
                        continue;
                    }
                    if let Some(id) = column_id
                        && !self.columns.iter().any(|column| column.id == id)
                    {
                        continue;
                    }
                    match result {
                        Ok(loaded) => {
                            let append = offset > 0;
                            if column_id.is_none() {
                                self.online_task.take();
                                self.loading = false;
                            } else if let Some(id) = column_id {
                                self.column_tasks.remove(&id);
                            }
                            if let Loaded::TrackPage(page, _) = &loaded
                                && offset == 0
                                && let PagedSource::Playlist { id, .. } = source
                            {
                                self.playlist_track_cache
                                    .insert(id, (page.title.clone(), page.items.clone()));
                            }
                            self.apply_page(loaded, source, append, column_id);
                        }
                        Err(error) => {
                            if offset == 0 {
                                if let Some(id) = column_id {
                                    if let Some(column) =
                                        self.columns.iter_mut().find(|column| column.id == id)
                                    {
                                        column.phase = ColumnPhase::Error(error.clone());
                                        column.content = Content::Empty;
                                        column.pagination.loading = false;
                                    }
                                } else {
                                    self.loading = false;
                                    self.content = Content::Empty;
                                    self.status = error.clone();
                                    self.pagination.loading = false;
                                }
                            } else if let Some(id) = column_id {
                                if let Some(column) =
                                    self.columns.iter_mut().find(|column| column.id == id)
                                {
                                    column.pagination.loading = false;
                                }
                            } else {
                                self.pagination.loading = false;
                            }
                            self.push_toast(ToastKind::Error, error);
                        }
                    }
                }
                AppEvent::StreamReady(generation, track, result) => {
                    if generation != self.stream_generation {
                        continue;
                    }
                    self.stream_task.take();
                    match result {
                        Ok(stream) => self.start_stream_playback(track, stream),
                        Err(error) => {
                            self.push_toast(ToastKind::Error, error.clone());
                            self.status = error;
                        }
                    }
                }
                AppEvent::LoginStarted(result) => {
                    self.auth_task.take();
                    match result {
                        Ok(challenge) => {
                            self.qr = QrCode::new(challenge.url.as_bytes())
                                .map(|code| {
                                    code.render::<unicode::Dense1x2>()
                                        .quiet_zone(true)
                                        .module_dimensions(1, 1)
                                        .build()
                                })
                                .unwrap_or_default();
                            self.challenge = Some(challenge);
                            self.login_status = "使用网易云音乐 App 扫码并确认".into();
                            self.login_polling = true;
                            self.next_login_poll = Instant::now();
                        }
                        Err(error) => self.login_status = error,
                    }
                }
                AppEvent::LoginPolled(result) => {
                    self.auth_task.take();
                    match result {
                        Ok(QrStatus::Waiting) => self.login_status = "等待扫码…".into(),
                        Ok(QrStatus::Scanned) => {
                            self.login_status = "已扫码，请在手机上确认".into()
                        }
                        Ok(QrStatus::Authenticated(identity)) => {
                            self.login_status = format!("登录成功：{}", identity.nickname);
                            self.identity = Some(identity);
                            self.start_metadata_warmup();
                            self.login_polling = false;
                            self.challenge = None;
                            let route = self.route;
                            if route != Route::Account {
                                self.set_route(route);
                            }
                        }
                        Ok(QrStatus::Expired) => {
                            self.login_status = "二维码已过期，按 L 刷新".into();
                            self.login_polling = false;
                        }
                        Ok(QrStatus::RiskControlled) => {
                            self.login_status = "登录触发风控，请稍后重试".into();
                            self.login_polling = false;
                        }
                        Err(error) => {
                            self.login_status = error;
                            self.login_polling = false;
                        }
                    }
                }
                AppEvent::DownloadFinished(id, result) => {
                    if self.active_job.as_ref().map(|job| job.id) != Some(id) {
                        continue;
                    }
                    self.active_job.take();
                    match result {
                        Ok(report) => {
                            self.status = format!(
                                "已下载 {} · 已存在 {} · 不可用 {}",
                                report.downloaded.len(),
                                report.skipped_existing.len(),
                                report.unavailable.len()
                            );
                            if matches!(
                                self.route,
                                Route::Local | Route::Favorites | Route::Recent | Route::Queue
                            ) {
                                self.load_local();
                            }
                            self.push_toast(ToastKind::Success, self.status.clone());
                        }
                        Err(error) => {
                            self.push_toast(ToastKind::Error, error.clone());
                            self.status = error;
                        }
                    }
                }
                AppEvent::OrganizeFinished(id, result) => {
                    if self.active_job.as_ref().map(|job| job.id) != Some(id) {
                        continue;
                    }
                    self.active_job.take();
                    self.status = match result {
                        Ok(Some(MoveOutcome::Moved { .. })) => "整理完成".into(),
                        Ok(Some(MoveOutcome::Skipped(_))) => "无需整理".into(),
                        Ok(Some(MoveOutcome::Conflict(_))) => "目标文件已存在".into(),
                        Ok(None) => "缺少 NCM 元数据".into(),
                        Err(error) => error,
                    };
                    let kind = if self.status.contains("完成") {
                        ToastKind::Success
                    } else if self.status.contains("缺少") || self.status.contains("存在") {
                        ToastKind::Warn
                    } else {
                        ToastKind::Error
                    };
                    self.push_toast(kind, self.status.clone());
                    self.load_local();
                }
                AppEvent::ScanFinished(id, result) => {
                    if self.active_job.as_ref().map(|job| job.id) != Some(id) {
                        continue;
                    }
                    let kind = self.active_job.take().map(|job| job.kind);
                    match result {
                        Ok(report) => {
                            let verb = if kind == Some(JobKind::Import) {
                                "导入完成"
                            } else {
                                "扫描完成"
                            };
                            self.status = format!(
                                "{verb} · 发现 {} · 新增 {} · 更新 {} · 缺失 {}",
                                report.discovered, report.added, report.updated, report.missing
                            );
                            self.push_toast(ToastKind::Success, self.status.clone());
                            self.load_local();
                        }
                        Err(error) => {
                            self.push_toast(ToastKind::Error, error.clone());
                            self.status = error;
                        }
                    }
                }
                AppEvent::CoverLoaded(song_id, bytes) => {
                    if self.cover_track == Some(song_id) && self.current == Some(song_id) {
                        let _ = self.apply_cover_bytes(bytes);
                    }
                }
                AppEvent::LyricsLoaded(song_id, result) => {
                    if self.current != Some(song_id) {
                        continue;
                    }
                    self.lyrics_task.take();
                    self.lyrics = match result {
                        Ok(lyrics) => LyricsState::Ready(song_id, lyrics),
                        Err(error) => LyricsState::Error(song_id, error),
                    };
                }
                AppEvent::DailyWarmed(tracks) => {
                    self.daily_cache = Some(tracks);
                }
                AppEvent::RecommendedWarmed(playlists) => {
                    self.recommended_cache = Some(playlists);
                }
                AppEvent::UserPlaylistsWarmed(playlists) => {
                    let cache = self.playlist_cache.get_or_insert_with(Vec::new);
                    pagination::merge_unique_by_id(cache, playlists, |playlist| playlist.id);
                    if matches!(
                        self.route,
                        Route::Created | Route::Subscribed | Route::Favorites
                    ) && self.content.is_empty()
                    {
                        let route = self.route;
                        self.set_route(route);
                        changed = true;
                    }
                }
                AppEvent::AlbumsWarmed(albums) => {
                    let cache = self.album_cache.get_or_insert_with(Vec::new);
                    pagination::merge_unique_by_id(cache, albums, |album| album.id);
                    if self.route == Route::Albums && self.content.is_empty() {
                        self.load_albums();
                        changed = true;
                    }
                }
                AppEvent::ArtistsWarmed(artists) => {
                    let cache = self.artist_cache.get_or_insert_with(Vec::new);
                    pagination::merge_unique_by_id(cache, artists, |artist| artist.id);
                    if self.route == Route::Artists && self.content.is_empty() {
                        self.load_artists();
                        changed = true;
                    }
                }
                AppEvent::ListeningRankWarmed(kind, tracks) => match kind {
                    ListeningRank::Week => self.listening_week_cache = Some(tracks),
                    ListeningRank::All => self.listening_all_cache = Some(tracks),
                },
                AppEvent::MetadataWarmFinished => {
                    self.metadata_warm_task.take();
                }
                AppEvent::CacheLimitUpdated(result) => {
                    self.cache_task.take();
                    self.status = match result {
                        Ok(stats) => format!(
                            "播放缓存上限已设为 {}，当前使用 {}",
                            format_bytes(stats.max_bytes),
                            format_bytes(stats.used_bytes)
                        ),
                        Err(error) => format!("设置播放缓存失败：{error}"),
                    };
                }
                AppEvent::CacheCleared(result) => {
                    self.cache_task.take();
                    self.status = match result {
                        Ok(report) if report.retained_active > 0 => format!(
                            "已清除 {}，保留 {} 个正在播放/下载的缓存文件",
                            format_bytes(report.removed_bytes),
                            report.retained_active
                        ),
                        Ok(report) => {
                            format!("已清除 {} 播放缓存", format_bytes(report.removed_bytes))
                        }
                        Err(error) => format!("清除播放缓存失败：{error}"),
                    };
                }
            }
        }
        changed
    }

    pub(super) fn needs_ui_tick(&self) -> bool {
        self.loading
            || self.columns.iter().any(BrowserColumn::is_loading)
            || self.pagination.loading
            || self.active_job.is_some()
            || matches!(self.lyrics, LyricsState::Loading(_))
            || !self.toasts.is_empty()
            || self.mode == InputMode::Palette
    }

    pub(super) fn needs_progress_tick(&self) -> bool {
        !self.player_state.title.is_empty()
            && !self.player_state.paused
            && !self.player_state.finished
    }

    #[cfg(test)]
    pub(super) fn needs_animation(&self) -> bool {
        self.needs_ui_tick() || self.needs_progress_tick()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(job) = self.active_job.take() {
            job.handle.abort();
        }
        if let Some(task) = self.online_task.take() {
            task.abort();
        }
        for (_, task) in self.column_tasks.drain() {
            task.abort();
        }
        if let Some(task) = self.auth_task.take() {
            task.abort();
        }
        if let Some(task) = self.lyrics_task.take() {
            task.abort();
        }
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
        if let Some(task) = self.metadata_warm_task.take() {
            task.abort();
        }
        if let Some(task) = self.cache_task.take() {
            task.abort();
        }
        if let Some(task) = self.cover_task.take() {
            task.abort();
        }
    }
}

pub(super) struct TerminalGuard {
    pub(super) active: bool,
}

impl TerminalGuard {
    pub(super) fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
        Ok(Self { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.active {
            let _ = execute!(
                io::stdout(),
                Show,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
    }
}

pub async fn run(services: Services) -> Result<(), TuiError> {
    let guard = TerminalGuard::enter()?;
    let picker = crate::cover::query_picker();
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(services);
    app.cover_picker = picker;
    let result = run_loop(&mut terminal, app).await;
    drop(terminal);
    drop(guard);
    result
}

pub(super) async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
) -> Result<(), TuiError> {
    let mut dirty = true;
    let mut next_ui_tick = Instant::now() + UI_TICK_INTERVAL;
    let mut next_progress = Instant::now() + PROGRESS_INTERVAL;
    loop {
        dirty |= app.poll_events();
        app.poll_login();

        let now = Instant::now();
        if app.needs_ui_tick() && now >= next_ui_tick {
            app.tick = app.tick.wrapping_add(1);
            dirty = true;
            next_ui_tick = now + UI_TICK_INTERVAL;
        }

        let playing = app.needs_progress_tick();
        // pigma: wait for events, then paint a full frame. Progress arrives
        // about every 200ms; we never clone the back buffer or overlay patches.
        if dirty || (playing && now >= next_progress) {
            app.refresh_player_state();
            if app.playback_finished() {
                app.handle_completion();
                app.refresh_player_state();
            }
            terminal.draw(|frame| paint_frame(frame, &mut app))?;
            dirty = false;
            next_progress = Instant::now() + PROGRESS_INTERVAL;
        }

        let mut timeout = EVENT_POLL_INTERVAL;
        if app.needs_ui_tick() {
            timeout = timeout.min(next_ui_tick.saturating_duration_since(Instant::now()));
        }
        if playing {
            timeout = timeout.min(next_progress.saturating_duration_since(Instant::now()));
        }
        if !event::poll(timeout)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(&mut app, key).await {
                    break;
                }
                dirty = true;
            }
            Event::Mouse(mouse) => {
                handle_mouse(&mut app, mouse);
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
    Ok(())
}

pub(super) async fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    if key.code == KeyCode::Esc {
        return dispatch_action(app, UiAction::Back);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
    {
        if app.mode == InputMode::Palette {
            app.mode = InputMode::Normal;
            app.input.clear();
            app.input_cursor = 0;
            app.status.clear();
            return false;
        }
        return dispatch_action(app, UiAction::Palette);
    }
    if app.mode == InputMode::Palette {
        let filtered = app.filtered_palette().len();
        match key.code {
            KeyCode::Enter => app.run_palette_selection(),
            KeyCode::Up => {
                app.palette_selected = app.palette_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab => {
                if filtered > 0 {
                    app.palette_selected = app
                        .palette_selected
                        .saturating_add(1)
                        .min(filtered.saturating_sub(1));
                }
            }
            KeyCode::Home => app.palette_selected = 0,
            KeyCode::End if filtered > 0 => app.palette_selected = filtered - 1,
            _ => {
                let _ = edit_input(&mut app.input, &mut app.input_cursor, key);
                let filtered = app.filtered_palette().len();
                if filtered == 0 {
                    app.palette_selected = 0;
                } else {
                    app.palette_selected = app.palette_selected.min(filtered - 1);
                }
            }
        }
        return false;
    }
    if app.show_help {
        match key.code {
            KeyCode::Char('?') => return dispatch_action(app, UiAction::ToggleHelp),
            KeyCode::Up | KeyCode::Char('k') => app.help_scroll = app.help_scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                app.help_scroll = app.help_scroll.saturating_add(1).min(HELP_MAX_SCROLL)
            }
            KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(6),
            KeyCode::PageDown => {
                app.help_scroll = app.help_scroll.saturating_add(6).min(HELP_MAX_SCROLL)
            }
            KeyCode::Home => app.help_scroll = 0,
            KeyCode::End => app.help_scroll = HELP_MAX_SCROLL,
            _ => {}
        }
        return false;
    }
    if app.show_details {
        return false;
    }
    if app.mode != InputMode::Normal {
        if edit_input(&mut app.input, &mut app.input_cursor, key) == InputEdit::Submit {
            match app.mode {
                InputMode::Search => app.search_online(),
                InputMode::LocalSearch => app.search_local(),
                InputMode::Download => app.start_download(),
                InputMode::CacheSize => app.set_cache_size(),
                InputMode::Import => app.start_import(),
                InputMode::Palette | InputMode::Normal => {}
            }
        }
        return false;
    }
    if key.code == KeyCode::Char('?') {
        return dispatch_action(app, UiAction::ToggleHelp);
    }
    if app.route == Route::Search {
        let kind = match key.code {
            KeyCode::F(1) => Some(SearchKind::Song),
            KeyCode::F(2) => Some(SearchKind::Album),
            KeyCode::F(3) => Some(SearchKind::Artist),
            KeyCode::F(4) => Some(SearchKind::Playlist),
            _ => None,
        };
        if let Some(kind) = kind {
            app.search_kind = kind;
            app.mode = InputMode::Search;
            app.input.clear();
            app.input_cursor = 0;
            app.status = format!("搜索范围：{}", search_kind_label(kind));
            return false;
        }
    }
    if app.route == Route::Account {
        match key.code {
            KeyCode::Char('s') => return dispatch_action(app, UiAction::EditCacheSize),
            KeyCode::Char('X') => return dispatch_action(app, UiAction::ClearCache),
            _ => {}
        }
    }
    normal_action(app.focus, app.current.is_some(), key)
        .is_some_and(|action| dispatch_action(app, action))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InputEdit {
    None,
    Submit,
}
