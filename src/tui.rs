//NetEase Cloud Music terminal frontend.

use std::{
    collections::{HashMap, HashSet},
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
        RankedTrack, SearchKind,
    },
    download::{
        AudioQuality, DownloadReport, DownloadRequest, DownloadSource, Downloader, TrackSelection,
    },
    library::{Library, LibraryStats, ScanReport, Track},
    lyrics::{LyricLine, Lyrics},
    organizer::MoveOutcome,
    playback_cache::{CacheStats, ClearReport, PlaybackCache},
    player::{Player, PlayerState},
    streaming::PreparedStream,
};

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 24;
const WIDE_WIDTH: u16 = 96;
const COMPACT_WIDTH: u16 = 72;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const QR_PADDING_X: u16 = 2;
const QR_PADDING_Y: u16 = 1;
const QR_STATUS_GAP: u16 = 1;
const ANIMATION_INTERVAL: Duration = Duration::from_millis(100);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const HELP_MAX_SCROLL: u16 = 8;
const LOCAL_IMPORT_HINT: &str = "暂无本地音乐 · 按 I 或点击这里导入";

#[derive(Clone, Copy)]
struct Theme {
    background: Color,
    panel: Color,
    panel_highlight: Color,
    overlay: Color,
    border: Color,
    text: Color,
    muted: Color,
    accent: Color,
    qr_light: Color,
    qr_dark: Color,
}

impl Theme {
    fn miku() -> Self {
        Self {
            // Preserve the terminal's own opacity/blur as the transparent glass base.
            background: Color::Reset,
            panel: Color::Rgb(8, 34, 40),
            panel_highlight: Color::Rgb(16, 55, 60),
            overlay: Color::Rgb(5, 26, 32),
            border: Color::Rgb(39, 105, 106),
            text: Color::Rgb(225, 249, 247),
            muted: Color::Rgb(128, 178, 176),
            accent: Color::Rgb(57, 197, 187),
            qr_light: Color::Rgb(225, 249, 247),
            qr_dark: Color::Rgb(5, 26, 32),
        }
    }

    fn from_env() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Self {
                background: Color::Reset,
                panel: Color::Reset,
                panel_highlight: Color::Reset,
                overlay: Color::Reset,
                border: Color::Reset,
                text: Color::Reset,
                muted: Color::Reset,
                accent: Color::Reset,
                qr_light: Color::Reset,
                qr_dark: Color::Reset,
            };
        }
        Self::miku()
    }
}

#[derive(Clone)]
pub struct Services {
    pub authentication: Authentication,
    pub discovery: Discovery,
    pub library: Library,
    pub downloader: Downloader,
    pub library_roots: Vec<PathBuf>,
    pub playback_cache: PlaybackCache,
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Route {
    Daily,
    Recommended,
    Search,
    Favorites,
    Local,
    Recent,
    ListeningRank,
    Created,
    Subscribed,
    Artists,
    Albums,
    Queue,
    Downloads,
    Account,
}

impl Route {
    const ALL: [Self; 14] = [
        Self::Daily,
        Self::Recommended,
        Self::Search,
        Self::Favorites,
        Self::Local,
        Self::Recent,
        Self::ListeningRank,
        Self::Created,
        Self::Subscribed,
        Self::Artists,
        Self::Albums,
        Self::Queue,
        Self::Downloads,
        Self::Account,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Daily => "每日推荐",
            Self::Recommended => "推荐歌单",
            Self::Search => "在线搜索",
            Self::Favorites => "我喜欢的音乐",
            Self::Local => "本地音乐",
            Self::Recent => "最近播放",
            Self::ListeningRank => "听歌排行",
            Self::Created => "创建的歌单",
            Self::Subscribed => "收藏的歌单",
            Self::Artists => "收藏的歌手",
            Self::Albums => "收藏的专辑",
            Self::Queue => "播放队列",
            Self::Downloads => "下载管理",
            Self::Account => "账号与登录",
        }
    }

    fn group(self) -> &'static str {
        match self {
            Self::Daily | Self::Recommended | Self::Search => "发现",
            Self::Favorites | Self::Local | Self::Recent => "我的音乐",
            Self::ListeningRank
            | Self::Created
            | Self::Subscribed
            | Self::Artists
            | Self::Albums => "我的收藏",
            Self::Queue | Self::Downloads => "播放",
            Self::Account => "账号",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Navigation,
    Content,
    Column(usize),
    Lyrics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Normal,
    Search,
    LocalSearch,
    Download,
    CacheSize,
    Import,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiAction {
    Quit,
    ToggleHelp,
    Back,
    FocusNext,
    FocusPrevious,
    SelectPrevious,
    SelectNext,
    SelectFirst,
    SelectLast,
    PagePrevious,
    PageNext,
    Activate,
    Search,
    Login,
    Download,
    ToggleLyrics,
    TogglePause,
    PreviousTrack,
    NextTrack,
    SeekBackward,
    SeekForward,
    SeekTo(Duration),
    VolumeUp,
    VolumeDown,
    CycleMode,
    Enqueue,
    Dequeue,
    ToggleFavorite,
    Organize,
    Details,
    Refresh,
    Import,
    EditCacheSize,
    ClearCache,
    OpenRoute(Route),
}

#[derive(Clone, Copy)]
struct ActionHint {
    key: &'static str,
    label: &'static str,
}

const PREVIOUS_ICON: &str = "‹";
const NEXT_ICON: &str = "›";
const PLAY_ICON: &str = "▶";
const PAUSE_ICON: &str = "Ⅱ";

fn action_hint(action: UiAction) -> ActionHint {
    match action {
        UiAction::Quit => ActionHint {
            key: "q",
            label: "退出",
        },
        UiAction::ToggleHelp => ActionHint {
            key: "?",
            label: "帮助",
        },
        UiAction::Back => ActionHint {
            key: "Esc",
            label: "返回",
        },
        UiAction::FocusNext => ActionHint {
            key: "Tab",
            label: "切换焦点",
        },
        UiAction::FocusPrevious => ActionHint {
            key: "Shift+Tab",
            label: "反向切换",
        },
        UiAction::SelectPrevious | UiAction::SelectNext => ActionHint {
            key: "↑↓/jk",
            label: "选择",
        },
        UiAction::SelectFirst | UiAction::SelectLast => ActionHint {
            key: "g/G",
            label: "首尾",
        },
        UiAction::Activate => ActionHint {
            key: "Enter",
            label: "打开/播放",
        },
        UiAction::Search => ActionHint {
            key: "/",
            label: "搜索",
        },
        UiAction::Login => ActionHint {
            key: "L",
            label: "登录",
        },
        UiAction::Download => ActionHint {
            key: "D",
            label: "下载",
        },
        UiAction::ToggleLyrics => ActionHint {
            key: "l",
            label: "歌词",
        },
        UiAction::TogglePause => ActionHint {
            key: "Space",
            label: "播放/暂停",
        },
        UiAction::PreviousTrack => ActionHint {
            key: "p",
            label: "上一首",
        },
        UiAction::NextTrack => ActionHint {
            key: "n",
            label: "下一首",
        },
        UiAction::SeekBackward | UiAction::SeekForward => ActionHint {
            key: "[/]",
            label: "快退/快进",
        },
        UiAction::VolumeUp | UiAction::VolumeDown => ActionHint {
            key: "+/-",
            label: "音量",
        },
        UiAction::CycleMode => ActionHint {
            key: "m",
            label: "播放模式",
        },
        UiAction::Enqueue => ActionHint {
            key: "a",
            label: "下一首播放",
        },
        UiAction::Dequeue => ActionHint {
            key: "d",
            label: "移出队列",
        },
        UiAction::ToggleFavorite => ActionHint {
            key: "f",
            label: "喜欢",
        },
        UiAction::Organize => ActionHint {
            key: "o",
            label: "整理文件",
        },
        UiAction::Details => ActionHint {
            key: "i",
            label: "详情",
        },
        UiAction::Refresh => ActionHint {
            key: "r",
            label: "刷新",
        },
        UiAction::Import => ActionHint {
            key: "I",
            label: "导入",
        },
        UiAction::EditCacheSize => ActionHint {
            key: "s",
            label: "缓存大小",
        },
        UiAction::ClearCache => ActionHint {
            key: "X",
            label: "清除缓存",
        },
        UiAction::PagePrevious | UiAction::PageNext => ActionHint {
            key: "PgUp/PgDn",
            label: "翻页",
        },
        UiAction::SeekTo(_) | UiAction::OpenRoute(_) => ActionHint {
            key: "鼠标",
            label: "点击",
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JobKind {
    Download,
    Organize,
    Scan,
    Import,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlayMode {
    Sequential,
    RepeatOne,
    RepeatAll,
    Shuffle,
}

impl PlayMode {
    fn next(self) -> Self {
        match self {
            Self::Sequential => Self::RepeatOne,
            Self::RepeatOne => Self::RepeatAll,
            Self::RepeatAll => Self::Shuffle,
            Self::Shuffle => Self::Sequential,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Sequential => "顺序",
            Self::RepeatOne => "单曲",
            Self::RepeatAll => "循环",
            Self::Shuffle => "随机",
        }
    }
}

enum LyricsState {
    Idle,
    Loading(u64),
    Ready(u64, Lyrics),
    Error(u64, String),
}

#[derive(Clone)]
struct TrackRow {
    id: u64,
    title: String,
    artists: String,
    album: String,
    duration_ms: u64,
    path: Option<PathBuf>,
    favorite: bool,
    play_count: u64,
    format: String,
    bytes: u64,
}

impl From<Track> for TrackRow {
    fn from(track: Track) -> Self {
        Self {
            id: track.id,
            title: track.title,
            artists: track.artists,
            album: track.album,
            duration_ms: track.duration_ms,
            path: Some(track.path),
            favorite: track.favorite,
            play_count: track.play_count,
            format: track.format,
            bytes: track.bytes,
        }
    }
}

impl From<OnlineTrack> for TrackRow {
    fn from(track: OnlineTrack) -> Self {
        Self {
            id: track.id,
            title: track.title,
            artists: track.artists,
            album: track.album,
            duration_ms: track.duration_ms,
            path: None,
            favorite: false,
            play_count: 0,
            format: String::new(),
            bytes: 0,
        }
    }
}

enum Content {
    Tracks(Vec<TrackRow>),
    Playlists(Vec<PlaylistSummary>),
    Artists(Vec<ArtistSummary>),
    Albums(Vec<AlbumSummary>),
    Empty,
}

impl Content {
    fn len(&self) -> usize {
        match self {
            Self::Tracks(values) => values.len(),
            Self::Playlists(values) => values.len(),
            Self::Artists(values) => values.len(),
            Self::Albums(values) => values.len(),
            Self::Empty => 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

enum Loaded {
    Tracks(String, Vec<OnlineTrack>),
    RankedTracks(String, Vec<RankedTrack>),
    Playlists(Vec<PlaylistSummary>),
    Artists(Vec<ArtistSummary>),
    Albums(Vec<AlbumSummary>),
    LocalTracks(String, Vec<Track>, Option<LibraryStats>),
}

struct ActiveJob {
    id: u64,
    kind: JobKind,
    handle: JoinHandle<()>,
}

struct BrowserColumn {
    id: u64,
    title: String,
    content: Content,
    selected: usize,
    phase: ColumnPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ColumnPhase {
    Loading,
    Ready,
    Error(String),
}

impl BrowserColumn {
    fn loading(id: u64, title: String) -> Self {
        Self {
            id,
            title,
            content: Content::Empty,
            selected: 0,
            phase: ColumnPhase::Loading,
        }
    }

    fn is_loading(&self) -> bool {
        self.phase == ColumnPhase::Loading
    }
}

enum AppEvent {
    #[cfg(not(test))]
    Identity(Result<Identity, String>),
    Loaded(u64, Result<Loaded, String>),
    ColumnLoaded(u64, Result<Loaded, String>),
    StreamReady(u64, TrackRow, Result<PreparedStream, String>),
    LoginStarted(Result<QrChallenge, String>),
    LoginPolled(Result<QrStatus, String>),
    DownloadFinished(u64, Result<DownloadReport, String>),
    OrganizeFinished(u64, Result<Option<MoveOutcome>, String>),
    ScanFinished(u64, Result<ScanReport, String>),
    LyricsLoaded(u64, Result<Lyrics, String>),
    DailyWarmed(Vec<OnlineTrack>),
    RecommendedWarmed(Vec<PlaylistSummary>),
    UserPlaylistsWarmed(Vec<PlaylistSummary>),
    PlaylistWarmed(u64, String, Vec<OnlineTrack>),
    ListeningRankWarmed(ListeningRank, Vec<RankedTrack>),
    MetadataWarmFinished,
    CacheLimitUpdated(Result<CacheStats, String>),
    CacheCleared(Result<ClearReport, String>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ColumnHit {
    index: usize,
    area: Rect,
    offset: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HitRegions {
    account: Rect,
    nav: Rect,
    content: Rect,
    columns: Vec<ColumnHit>,
    lyrics: Rect,
    progress: Rect,
    previous: Rect,
    pause: Rect,
    next: Rect,
    play_mode: Rect,
    volume: Rect,
    content_offset: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PlayerLayout {
    song_info: Rect,
    elapsed: Rect,
    progress: Rect,
    duration: Rect,
    previous: Rect,
    pause: Rect,
    next: Rect,
    play_mode: Rect,
    volume: Rect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BrowserPane {
    index: usize,
    area: Rect,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AppLayout {
    header: Rect,
    navigation: Option<Rect>,
    browser: Vec<BrowserPane>,
    lyrics: Option<Rect>,
    player: Rect,
    footer: Rect,
}

struct App {
    services: Services,
    theme: Theme,
    route: Route,
    focus: Focus,
    content: Content,
    title: String,
    selected: usize,
    nav_selected: usize,
    input: String,
    input_cursor: usize,
    mode: InputMode,
    search_kind: SearchKind,
    status: String,
    loading: bool,
    columns: Vec<BrowserColumn>,
    show_help: bool,
    help_scroll: u16,
    show_details: bool,
    tick: usize,
    identity: Option<Identity>,
    challenge: Option<QrChallenge>,
    qr: String,
    login_status: String,
    login_polling: bool,
    next_login_poll: Instant,
    player: Option<Player>,
    current: Option<u64>,
    current_track: Option<TrackRow>,
    play_queue: Vec<TrackRow>,
    queue_index: Option<usize>,
    play_mode: PlayMode,
    completion_latched: bool,
    show_lyrics: bool,
    previous_focus: Focus,
    lyrics: LyricsState,
    lyrics_task: Option<JoinHandle<()>>,
    next_job_id: u64,
    active_job: Option<ActiveJob>,
    online_task: Option<JoinHandle<()>>,
    column_tasks: HashMap<u64, JoinHandle<()>>,
    auth_task: Option<JoinHandle<()>>,
    generation: u64,
    stream_task: Option<JoinHandle<()>>,
    stream_generation: u64,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    hits: HitRegions,
    last_click: Option<(usize, usize, Instant)>,
    local_stats: Option<LibraryStats>,
    player_state: PlayerState,
    daily_cache: Option<Vec<OnlineTrack>>,
    recommended_cache: Option<Vec<PlaylistSummary>>,
    playlist_cache: Option<Vec<PlaylistSummary>>,
    playlist_track_cache: HashMap<u64, (String, Vec<OnlineTrack>)>,
    listening_week_cache: Option<Vec<RankedTrack>>,
    listening_all_cache: Option<Vec<RankedTrack>>,
    metadata_warm_task: Option<JoinHandle<()>>,
    cache_task: Option<JoinHandle<()>>,
    cache_clear_armed_until: Option<Instant>,
}

impl App {
    fn new(services: Services) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (player, status) = match Player::new() {
            Ok(player) => (Some(player), String::new()),
            Err(_) => (None, "音频输出不可用".into()),
        };
        let player_state = player.as_ref().map(Player::state).unwrap_or_default();
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
            show_lyrics: false,
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
            metadata_warm_task: None,
            cache_task: None,
            cache_clear_armed_until: None,
        };
        app.load_local();
        #[cfg(not(test))]
        app.check_identity();
        app
    }

    #[cfg(not(test))]
    fn check_identity(&mut self) {
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

    fn load_local(&mut self) {
        let route = self.route;
        if !matches!(
            route,
            Route::Favorites | Route::Local | Route::Recent | Route::Queue
        ) {
            return;
        }
        let library = self.services.library.clone();
        let title = route.label().to_owned();
        if tokio::runtime::Handle::try_current().is_err() {
            match query_local(&library, route, None) {
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
            tokio::task::spawn_blocking(move || query_local(&library, route, None))
                .await
                .map_err(|error| error.to_string())?
                .map(|(tracks, stats)| Loaded::LocalTracks(title, tracks, stats))
        });
    }

    fn search_local(&mut self) {
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
            tokio::task::spawn_blocking(move || query_local(&library, Route::Local, Some(&query)))
                .await
                .map_err(|error| error.to_string())?
                .map(|(tracks, stats)| Loaded::LocalTracks(title, tracks, stats))
        });
    }

    fn begin_import(&mut self) {
        if self.route != Route::Local {
            self.set_route(Route::Local);
            self.focus = Focus::Content;
        }
        self.mode = InputMode::Import;
        self.input.clear();
        self.input_cursor = 0;
        self.status = "输入本地文件或目录路径，Enter 导入到音乐库".into();
    }

    fn start_import(&mut self) {
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

    fn start_scan(&mut self) {
        if self.services.library_roots.is_empty() {
            self.status =
                "请按 I 导入本地音乐，或在 config.toml 的 [library] dirs 中配置目录".into();
            return;
        }
        let roots = self.services.library_roots.clone();
        self.start_library_scan(roots, JobKind::Scan);
    }

    fn start_library_scan(&mut self, roots: Vec<PathBuf>, kind: JobKind) {
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

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.content.len().saturating_sub(1));
    }

    fn set_route(&mut self, route: Route) {
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
        match route {
            Route::Favorites | Route::Local | Route::Recent | Route::Queue => self.load_local(),
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

    fn requires_login(&mut self) -> bool {
        if self.identity.is_none() {
            self.status = "此入口需要登录，按 L 扫码登录".into();
            self.content = Content::Empty;
            true
        } else {
            false
        }
    }

    fn spawn_load<F>(&mut self, future: F)
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

    fn truncate_columns(&mut self, keep: usize) {
        for column in self.columns.drain(keep..) {
            if let Some(task) = self.column_tasks.remove(&column.id) {
                task.abort();
            }
        }
    }

    fn spawn_column<F>(&mut self, title: String, future: F)
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

    fn load_daily(&mut self) {
        if self.requires_login() {
            return;
        }
        if let Some(tracks) = self.daily_cache.clone() {
            self.use_cached_tracks("每日推荐", tracks);
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

    fn load_recommended(&mut self) {
        if self.requires_login() {
            return;
        }
        if let Some(playlists) = self.recommended_cache.clone() {
            self.use_cached_content(Content::Playlists(playlists));
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

    fn load_playlists(&mut self, scope: PlaylistScope) {
        let Some(user_id) = self.identity.as_ref().map(|identity| identity.user_id) else {
            self.requires_login();
            return;
        };
        if let Some(playlists) = self.playlist_cache.as_ref() {
            self.content = Content::Playlists(
                playlists
                    .iter()
                    .filter(|playlist| match scope {
                        PlaylistScope::All => true,
                        PlaylistScope::Created => playlist.created_by_user,
                        PlaylistScope::Subscribed => !playlist.created_by_user,
                    })
                    .cloned()
                    .collect(),
            );
            self.loading = false;
            self.selected = 0;
            self.status.clear();
            return;
        }
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .user_playlists(user_id, scope)
                .await
                .map(Loaded::Playlists)
                .map_err(|error| error.to_string())
        });
    }

    fn load_albums(&mut self) {
        if self.requires_login() {
            return;
        }
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .subscribed_albums()
                .await
                .map(Loaded::Albums)
                .map_err(|error| error.to_string())
        });
    }

    fn load_artists(&mut self) {
        if self.requires_login() {
            return;
        }
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .subscribed_artists()
                .await
                .map(Loaded::Artists)
                .map_err(|error| error.to_string())
        });
    }

    fn load_listening_rank(&mut self) {
        let Some(user_id) = self.identity.as_ref().map(|identity| identity.user_id) else {
            self.requires_login();
            return;
        };
        if let Some(tracks) = self.listening_week_cache.clone() {
            self.use_cached_ranked_tracks("本周听歌排行", tracks);
            return;
        }
        let discovery = self.services.discovery.clone();
        self.spawn_load(async move {
            discovery
                .listening_rank(user_id, ListeningRank::Week)
                .await
                .map(|tracks| Loaded::RankedTracks("本周听歌排行".into(), tracks))
                .map_err(|error| error.to_string())
        });
    }

    fn search_online(&mut self) {
        let keyword = self.input.trim().to_owned();
        if keyword.is_empty() {
            self.status = "请输入搜索关键词".into();
            return;
        }
        self.mode = InputMode::Normal;
        let discovery = self.services.discovery.clone();
        let kind = self.search_kind;
        self.spawn_load(async move {
            let result = discovery
                .search(&keyword, kind, 100)
                .await
                .map_err(|error| error.to_string())?;
            let tracks = discovery
                .track_details(&result.track_ids)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::Tracks(format!("搜索：{keyword}"), tracks))
        });
    }

    fn use_cached_content(&mut self, content: Content) {
        self.content = content;
        self.loading = false;
        self.selected = 0;
        self.status.clear();
    }

    fn use_cached_tracks(&mut self, title: &str, tracks: Vec<OnlineTrack>) {
        self.title = title.into();
        self.use_cached_content(Content::Tracks(
            tracks.into_iter().map(Into::into).collect(),
        ));
    }

    fn use_cached_ranked_tracks(&mut self, title: &str, tracks: Vec<RankedTrack>) {
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

    fn open_playlist(&mut self, id: u64, name: String) {
        if let Some((cached_name, tracks)) = self.playlist_track_cache.get(&id).cloned() {
            let title = if cached_name.is_empty() {
                name
            } else {
                cached_name
            };
            self.push_cached_column(
                title,
                Content::Tracks(tracks.into_iter().map(Into::into).collect()),
            );
            return;
        }
        let discovery = self.services.discovery.clone();
        let loading_title = name.clone();
        self.spawn_column(loading_title, async move {
            let (api_name, tracks) = discovery
                .playlist_tracks(id)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::Tracks(
                if api_name.is_empty() { name } else { api_name },
                tracks,
            ))
        });
    }

    fn push_cached_column(&mut self, title: String, content: Content) {
        let keep = match self.focus {
            Focus::Content => 0,
            Focus::Column(index) => index.saturating_add(1),
            _ => self.columns.len(),
        };
        self.truncate_columns(keep);
        let id = self.allocate_job_id();
        self.columns.push(BrowserColumn {
            id,
            title,
            content,
            selected: 0,
            phase: ColumnPhase::Ready,
        });
        self.focus = Focus::Column(self.columns.len() - 1);
    }

    fn start_metadata_warmup(&mut self) {
        let Some(user_id) = self.identity.as_ref().map(|identity| identity.user_id) else {
            return;
        };
        if self.metadata_warm_task.is_some() {
            return;
        }
        let discovery = self.services.discovery.clone();
        let tx = self.event_tx.clone();
        self.metadata_warm_task = Some(tokio::spawn(async move {
            let (daily, recommended, user_playlists, week_rank, all_rank) = tokio::join!(
                discovery.daily_songs(),
                discovery.recommended_playlists(),
                discovery.user_playlists(user_id, PlaylistScope::All),
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

            let recommended = match recommended {
                Ok(playlists) => {
                    let _ = tx.send(AppEvent::RecommendedWarmed(playlists.clone()));
                    playlists
                }
                Err(_) => Vec::new(),
            };
            let user_playlists = match user_playlists {
                Ok(playlists) => {
                    let _ = tx.send(AppEvent::UserPlaylistsWarmed(playlists.clone()));
                    playlists
                }
                Err(_) => Vec::new(),
            };

            let mut seen = HashSet::new();
            let mut details = tokio::task::JoinSet::new();
            for playlist in recommended.into_iter().chain(user_playlists) {
                if !seen.insert(playlist.id) {
                    continue;
                }
                let discovery = discovery.clone();
                details.spawn(async move {
                    (playlist.id, discovery.playlist_tracks(playlist.id).await)
                });
            }
            while let Some(result) = details.join_next().await {
                let Ok((id, Ok((name, tracks)))) = result else {
                    continue;
                };
                if tx.send(AppEvent::PlaylistWarmed(id, name, tracks)).is_err() {
                    break;
                }
            }
            let _ = tx.send(AppEvent::MetadataWarmFinished);
        }));
    }

    fn open_album(&mut self, id: u64, name: String) {
        let discovery = self.services.discovery.clone();
        let loading_title = name.clone();
        self.spawn_column(loading_title, async move {
            let result = discovery
                .album(id)
                .await
                .map_err(|error| error.to_string())?;
            let tracks = discovery
                .track_details(&result.track_ids)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::Tracks(name, tracks))
        });
    }

    fn open_artist(&mut self, id: u64, name: String) {
        let discovery = self.services.discovery.clone();
        let loading_title = name.clone();
        self.spawn_column(loading_title, async move {
            let result = discovery
                .artist_tracks(id)
                .await
                .map_err(|error| error.to_string())?;
            let tracks = discovery
                .track_details(&result.track_ids)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Loaded::Tracks(name, tracks))
        });
    }

    fn back(&mut self) {
        match self.focus {
            Focus::Lyrics => {
                self.show_lyrics = false;
                self.focus = self.valid_focus(self.previous_focus);
            }
            Focus::Column(index) => {
                self.truncate_columns(index);
                self.focus = if index == 0 {
                    Focus::Content
                } else {
                    Focus::Column(index - 1)
                };
            }
            Focus::Content => self.focus = Focus::Navigation,
            Focus::Navigation => {}
        }
        self.last_click = None;
    }

    fn move_selection(&mut self, delta: isize) {
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
    }

    fn activate(&mut self) {
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
                Target::Album(value.id, value.name.clone())
            }),
            Content::Artists(values) => values.get(selected).map_or(Target::Empty, |value| {
                Target::Artist(value.id, value.name.clone())
            }),
            Content::Empty => Target::Empty,
        };
        match target {
            Target::Track(track, queue, index) => {
                self.play_queue = queue;
                self.queue_index = Some(index);
                self.play(track);
            }
            Target::Playlist(id, name) => self.open_playlist(id, name),
            Target::Album(id, name) => self.open_album(id, name),
            Target::Artist(id, name) => self.open_artist(id, name),
            Target::Empty => {
                if self.route == Route::Account && self.identity.is_none() {
                    self.start_login();
                } else if self.route == Route::Downloads {
                    self.mode = InputMode::Download;
                    self.input.clear();
                } else if self.route == Route::Local {
                    self.begin_import();
                }
            }
        }
    }

    fn play(&mut self, track: TrackRow) {
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

    fn start_stream(&mut self, track: TrackRow) {
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

    fn start_playback(&mut self, mut track: TrackRow, path: PathBuf) {
        let Some(player) = self.player.as_mut() else {
            self.status = "音频输出不可用".into();
            return;
        };
        match player.play(&path, &track.title, &track.artists) {
            Ok(()) => {
                track.path = Some(path);
                self.current = Some(track.id);
                self.current_track = Some(track.clone());
                self.completion_latched = false;
                let _ = self.services.library.record_play(track.id);
                self.load_lyrics(track);
                self.status.clear();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn start_stream_playback(&mut self, track: TrackRow, stream: PreparedStream) {
        let Some(player) = self.player.as_mut() else {
            self.status = "音频输出不可用".into();
            return;
        };
        let (source, extension) = stream.into_parts();
        match player.play_source(source, Some(&extension), &track.title, &track.artists) {
            Ok(()) => {
                self.current = Some(track.id);
                self.current_track = Some(track.clone());
                self.completion_latched = false;
                let _ = self.services.library.record_play(track.id);
                self.load_lyrics(track);
                self.status.clear();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn set_cache_size(&mut self) {
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

    fn clear_playback_cache(&mut self) {
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

    fn load_lyrics(&mut self, track: TrackRow) {
        if let Some(task) = self.lyrics_task.take() {
            task.abort();
        }
        let song_id = track.id;
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

    fn next_track(&mut self, delta: isize) {
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

    fn handle_completion(&mut self) {
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

    fn cycle_play_mode(&mut self) {
        self.play_mode = self.play_mode.next();
        self.status = format!("播放模式：{}", self.play_mode.label());
    }

    fn player_state(&self) -> &PlayerState {
        &self.player_state
    }

    fn refresh_player_state(&mut self) {
        self.player_state = self.player.as_ref().map(Player::state).unwrap_or_default();
    }

    fn playback_finished(&self) -> bool {
        self.player_state.finished
    }

    fn start_login(&mut self) {
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

    fn poll_login(&mut self) {
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

    fn start_download(&mut self) {
        match parse_download(self.input.trim()) {
            Ok(request) => {
                self.input.clear();
                self.mode = InputMode::Normal;
                self.start_download_request(request);
            }
            Err(message) => self.status = message.into(),
        }
    }

    fn start_download_request(&mut self, request: DownloadRequest) {
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

    fn start_organize(&mut self) {
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

    fn allocate_job_id(&mut self) -> u64 {
        self.next_job_id = self.next_job_id.wrapping_add(1);
        self.next_job_id
    }

    fn cancel_download(&mut self) -> bool {
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

    fn selected_track(&self) -> Option<&TrackRow> {
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

    fn valid_focus(&self, focus: Focus) -> Focus {
        match focus {
            Focus::Column(index) if index >= self.columns.len() => self
                .columns
                .len()
                .checked_sub(1)
                .map_or(Focus::Content, Focus::Column),
            other => other,
        }
    }

    fn focus_next(&mut self, backwards: bool) {
        let mut order = Vec::with_capacity(self.columns.len() + 3);
        order.push(Focus::Navigation);
        order.push(Focus::Content);
        order.extend((0..self.columns.len()).map(Focus::Column));
        order.push(Focus::Lyrics);
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
        if self.focus != Focus::Lyrics {
            self.show_lyrics = false;
        }
    }

    fn toggle_lyrics_focus(&mut self) {
        if self.focus == Focus::Lyrics && self.show_lyrics {
            self.focus = self.valid_focus(self.previous_focus);
            self.show_lyrics = false;
        } else if self.focus == Focus::Lyrics {
            self.show_lyrics = true;
        } else {
            self.previous_focus = self.valid_focus(self.focus);
            self.focus = Focus::Lyrics;
            self.show_lyrics = true;
        }
    }

    fn favorite(&mut self) {
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

    fn enqueue(&mut self) {
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

    fn dequeue(&mut self) {
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

    fn poll_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            changed |= !matches!(
                &event,
                AppEvent::DailyWarmed(_)
                    | AppEvent::RecommendedWarmed(_)
                    | AppEvent::UserPlaylistsWarmed(_)
                    | AppEvent::PlaylistWarmed(_, _, _)
                    | AppEvent::ListeningRankWarmed(_, _)
                    | AppEvent::MetadataWarmFinished
            );
            match event {
                #[cfg(not(test))]
                AppEvent::Identity(result) => {
                    self.auth_task.take();
                    if let Ok(identity) = result {
                        self.identity = Some(identity);
                        self.start_metadata_warmup();
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
                        Err(error) => {
                            self.content = Content::Empty;
                            self.status = error;
                        }
                    }
                    self.selected = 0;
                }
                AppEvent::ColumnLoaded(id, result) => {
                    self.column_tasks.remove(&id);
                    let Some(column) = self.columns.iter_mut().find(|column| column.id == id)
                    else {
                        continue;
                    };
                    match result {
                        Ok(Loaded::Tracks(title, values)) => {
                            column.phase = ColumnPhase::Ready;
                            column.title = title;
                            column.content =
                                Content::Tracks(values.into_iter().map(Into::into).collect());
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
                        }
                        Ok(Loaded::Playlists(values)) => {
                            column.phase = ColumnPhase::Ready;
                            column.content = Content::Playlists(values);
                        }
                        Ok(Loaded::Artists(values)) => {
                            column.phase = ColumnPhase::Ready;
                            column.content = Content::Artists(values);
                        }
                        Ok(Loaded::Albums(values)) => {
                            column.phase = ColumnPhase::Ready;
                            column.content = Content::Albums(values);
                        }
                        Ok(Loaded::LocalTracks(title, values, _)) => {
                            column.phase = ColumnPhase::Ready;
                            column.title = title;
                            column.content =
                                Content::Tracks(values.into_iter().map(Into::into).collect());
                        }
                        Err(error) => {
                            column.content = Content::Empty;
                            column.phase = ColumnPhase::Error(error);
                        }
                    }
                    column.selected = 0;
                }
                AppEvent::StreamReady(generation, track, result) => {
                    if generation != self.stream_generation {
                        continue;
                    }
                    self.stream_task.take();
                    match result {
                        Ok(stream) => self.start_stream_playback(track, stream),
                        Err(error) => self.status = error,
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
                        }
                        Err(error) => self.status = error,
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
                            self.load_local();
                        }
                        Err(error) => self.status = error,
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
                    self.playlist_cache = Some(playlists);
                }
                AppEvent::PlaylistWarmed(id, name, tracks) => {
                    self.playlist_track_cache.insert(id, (name, tracks));
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

    fn needs_animation(&self) -> bool {
        self.loading
            || self.columns.iter().any(BrowserColumn::is_loading)
            || self.active_job.is_some()
            || matches!(self.lyrics, LyricsState::Loading(_))
            || (!self.player_state.title.is_empty()
                && !self.player_state.paused
                && !self.player_state.finished)
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
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
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
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(&mut terminal, App::new(services)).await;
    drop(terminal);
    drop(guard);
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
) -> Result<(), TuiError> {
    let mut dirty = true;
    let mut next_animation = Instant::now() + ANIMATION_INTERVAL;
    loop {
        dirty |= app.poll_events();
        app.poll_login();

        let now = Instant::now();
        if app.needs_animation() && now >= next_animation {
            app.tick = app.tick.wrapping_add(1);
            dirty = true;
            next_animation = now + ANIMATION_INTERVAL;
        }

        if dirty {
            app.refresh_player_state();
            if app.playback_finished() {
                app.handle_completion();
                app.refresh_player_state();
            }
            terminal.draw(|frame| {
                let status_width = text_width(&player_status(app.player_state()));
                app.hits = calculate_hits(
                    frame.area(),
                    status_width,
                    text_width(account_label(&app)),
                    1 + app.columns.len(),
                    app.focus,
                    app.show_lyrics,
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
                draw(frame, &app);
            })?;
            dirty = false;
            if app.needs_animation() && next_animation <= Instant::now() {
                next_animation = Instant::now() + ANIMATION_INTERVAL;
            }
        }

        let timeout = if app.needs_animation() {
            next_animation
                .saturating_duration_since(Instant::now())
                .min(EVENT_POLL_INTERVAL)
        } else {
            EVENT_POLL_INTERVAL
        };
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

async fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    if key.code == KeyCode::Esc {
        return dispatch_action(app, UiAction::Back);
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
                InputMode::Normal => {}
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
enum InputEdit {
    None,
    Submit,
}

fn edit_input(input: &mut String, cursor: &mut usize, key: KeyEvent) -> InputEdit {
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

fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor.min(value.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(offset, _)| cursor + offset)
}

fn normal_action(_focus: Focus, playback_active: bool, key: KeyEvent) -> Option<UiAction> {
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

fn dispatch_action(app: &mut App, action: UiAction) -> bool {
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
        UiAction::Activate => app.activate(),
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
            if app.route == Route::Local {
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

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
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
                app.show_lyrics = false;
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

fn player_control_action(hits: &HitRegions, x: u16, y: u16) -> Option<UiAction> {
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

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn progress_ratio_at(area: Rect, x: u16) -> Option<f64> {
    if area.width == 0 || x < area.x || x >= area.right() {
        return None;
    }
    if area.width == 1 {
        return Some(0.0);
    }
    Some(f64::from(x - area.x) / f64::from(area.width - 1))
}

fn content_action_region(app: &App) -> Rect {
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

fn parse_download(command: &str) -> Result<DownloadRequest, &'static str> {
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

fn query_local(
    library: &Library,
    route: Route,
    query: Option<&str>,
) -> Result<(Vec<Track>, Option<LibraryStats>), String> {
    let is_search = query.is_some();
    let tracks = if let Some(query) = query {
        library.search(query, 10_000)
    } else {
        match route {
            Route::Favorites => library.list(true, 10_000),
            Route::Local => library.list(false, 10_000),
            Route::Recent => library.recent(1_000),
            Route::Queue => library.queue(),
            _ => return Ok((Vec::new(), None)),
        }
    }
    .map_err(|error| error.to_string())?;
    let stats = (route == Route::Local && !is_search)
        .then(|| library.stats().map_err(|error| error.to_string()))
        .transpose()?;
    Ok((tracks, stats))
}

fn app_layout(
    area: Rect,
    column_count: usize,
    focus: Focus,
    lyrics_expanded: bool,
) -> Option<AppLayout> {
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
    let column_count = column_count.max(1);
    let active = match focus {
        Focus::Column(index) => (index + 1).min(column_count - 1),
        Focus::Lyrics => column_count - 1,
        _ => 0,
    };
    let (navigation, browser, lyrics) = if area.width < COMPACT_WIDTH {
        if focus == Focus::Lyrics {
            (None, Vec::new(), Some(rows[1]))
        } else {
            (
                None,
                vec![BrowserPane {
                    index: active,
                    area: rows[1],
                }],
                None,
            )
        }
    } else {
        let nav_width = if area.width >= WIDE_WIDTH { 24 } else { 20 };
        let navigation = Rect::new(rows[1].x, rows[1].y, nav_width, rows[1].height);
        let workspace = Rect::new(
            navigation.right(),
            rows[1].y,
            rows[1].width.saturating_sub(nav_width),
            rows[1].height,
        );
        let lyric_width = if lyrics_expanded && focus == Focus::Lyrics {
            (workspace.width.saturating_mul(2) / 3)
                .min(workspace.width.saturating_sub(24))
                .max(24)
        } else if area.width >= WIDE_WIDTH {
            32.min(workspace.width.saturating_sub(24))
        } else {
            24.min(workspace.width.saturating_sub(24))
        };
        let browser_area = Rect::new(
            workspace.x,
            workspace.y,
            workspace.width.saturating_sub(lyric_width),
            workspace.height,
        );
        let lyrics = Rect::new(
            browser_area.right(),
            workspace.y,
            lyric_width,
            workspace.height,
        );
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
        let browser = (0..visible_count)
            .map(|slot| {
                let width = base_width + u16::from(slot < remainder as usize);
                let pane = BrowserPane {
                    index: start + slot,
                    area: Rect::new(x, browser_area.y, width, browser_area.height),
                };
                x = x.saturating_add(width);
                pane
            })
            .collect();
        (Some(navigation), browser, Some(lyrics))
    };
    Some(AppLayout {
        header: rows[0],
        navigation,
        browser,
        lyrics,
        player: rows[2],
        footer: rows[3],
    })
}

fn calculate_hits(
    area: Rect,
    player_status_width: u16,
    account_width: u16,
    column_count: usize,
    focus: Focus,
    lyrics_expanded: bool,
) -> HitRegions {
    let Some(layout) = app_layout(area, column_count, focus, lyrics_expanded) else {
        return HitRegions::default();
    };
    let player = player_layout(layout.player, player_status_width);
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
            area.right().saturating_sub(account_width.saturating_add(2)),
            area.y,
            account_width,
            u16::from(area.height > 0),
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

fn player_layout(area: Rect, status_width: u16) -> PlayerLayout {
    let inner = inner_rect(area);
    if inner.width < 24 || inner.height < 3 {
        return PlayerLayout::default();
    }

    let volume_width = status_width.min(inner.width / 2);
    let volume = Rect::new(
        inner.right().saturating_sub(volume_width),
        inner.y,
        volume_width,
        1,
    );
    let song_info = Rect::new(
        inner.x,
        inner.y,
        inner.width.saturating_sub(volume_width.saturating_add(1)),
        1,
    );

    let track_width = (inner.width / 2).min(inner.width.saturating_sub(12));
    let timeline_width = track_width.saturating_add(12);
    let timeline_x = inner
        .x
        .saturating_add(inner.width.saturating_sub(timeline_width) / 2);
    let elapsed = Rect::new(timeline_x, inner.y + 1, 5, 1);
    let progress = Rect::new(elapsed.right() + 1, inner.y + 1, track_width, 1);
    let duration = Rect::new(progress.right() + 1, inner.y + 1, 5, 1);

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
    let mut control_x = inner
        .x
        .saturating_add(inner.width.saturating_sub(controls_width) / 2);
    let previous = Rect::new(control_x, inner.y + 2, previous_width, 1);
    control_x = previous.right().saturating_add(gap);
    let pause = Rect::new(control_x, inner.y + 2, pause_width, 1);
    control_x = pause.right().saturating_add(gap);
    let next = Rect::new(control_x, inner.y + 2, next_width, 1);
    control_x = next.right().saturating_add(gap);
    let play_mode = Rect::new(control_x, inner.y + 2, mode_width, 1);

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
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn content_offset(selected: usize, visible_height: u16) -> usize {
    let visible = visible_height as usize;
    selected.saturating_sub(visible.saturating_sub(1))
}

fn grouped_navigation(height: u16) -> bool {
    let groups = Route::ALL
        .iter()
        .map(|route| route.group())
        .collect::<std::collections::HashSet<_>>()
        .len();
    Route::ALL.len().saturating_add(groups) <= height as usize
}

fn nav_index_at(row: usize, height: u16) -> Option<usize> {
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

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.background)),
        area,
    );
    let Some(layout) = app_layout(area, 1 + app.columns.len(), app.focus, app.show_lyrics) else {
        draw_too_small(frame, app, area);
        return;
    };
    draw_header(frame, app, layout.header);
    if let Some(navigation) = layout.navigation {
        draw_navigation(frame, app, navigation);
    }
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
    draw_player(frame, app, layout.player);
    draw_footer(frame, app, layout.footer);
    if layout.navigation.is_none() && app.focus == Focus::Navigation {
        draw_compact_navigation(frame, app, area);
    }
    if app.show_help {
        draw_help(frame, app, area);
    }
    if app.show_details {
        draw_details_overlay(frame, app, area);
    }
}

fn draw_compact_navigation(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered(area, 34, 20.min(area.height.saturating_sub(2)));
    frame.render_widget(Clear, popup);
    draw_navigation(frame, app, popup);
}

fn draw_too_small(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_navigation(frame: &mut Frame, app: &App, area: Rect) {
    let grouped = grouped_navigation(area.height.saturating_sub(2));
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
    frame.render_widget(
        List::new(items).block(panel(app, " 音乐 ", app.focus == Focus::Navigation)),
        area,
    );
}

fn draw_content(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_browser_column(frame: &mut Frame, app: &App, area: Rect, column_index: usize) {
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
        Content::Tracks(values) => {
            draw_tracks(frame, app, area, values, &title, column.selected, focused)
        }
        Content::Playlists(values) => {
            draw_playlists(frame, app, area, values, &title, column.selected, focused)
        }
        Content::Artists(values) => {
            draw_artists(frame, app, area, values, &title, column.selected, focused)
        }
        Content::Albums(values) => {
            draw_albums(frame, app, area, values, &title, column.selected, focused)
        }
        Content::Empty => {
            let (message, style) = match &column.phase {
                ColumnPhase::Loading => (
                    format!("{} 正在加载", SPINNER[app.tick % SPINNER.len()]),
                    Style::default().fg(app.theme.muted),
                ),
                ColumnPhase::Ready => ("暂无内容".to_owned(), Style::default().fg(app.theme.muted)),
                ColumnPhase::Error(error) => (
                    format!("加载失败\n{error}\n\nEsc 返回上一级"),
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

fn draw_lyrics(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_lyrics_body(frame: &mut Frame, app: &App, area: Rect, lyrics: &Lyrics) {
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

enum LyricDisplayRow {
    Original(Vec<Span<'static>>),
    Translation(String),
}

fn wrap_lyric_text(text: &str, width: u16) -> Vec<String> {
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

fn wrap_styled_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Vec<Span<'static>>> {
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

fn wrap_units<T: Copy>(
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

fn coalesce_styled_units(units: Vec<(String, Style)>) -> Vec<Span<'static>> {
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

fn draw_lyrics_message(frame: &mut Frame, app: &App, area: Rect, message: &str) {
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

fn lyric_spans(
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

fn timed_progress(position: Duration, start: Duration, end: Duration) -> f64 {
    if position <= start {
        return 0.0;
    }
    if position >= end || end <= start {
        return 1.0;
    }
    position.saturating_sub(start).as_secs_f64() / end.saturating_sub(start).as_secs_f64()
}

fn push_progress_spans(
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

fn draw_tracks(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    tracks: &[TrackRow],
    title: &str,
    selected: usize,
    focused: bool,
) {
    let visible = inner_rect(area).height as usize;
    let offset = content_offset(selected, visible as u16);
    let wide = area.width >= 88;
    let items = tracks
        .iter()
        .skip(offset)
        .take(visible)
        .map(|track| {
            let lead = if app.current == Some(track.id) {
                "▶"
            } else if track.favorite {
                "♥"
            } else {
                " "
            };
            let duration = format_time(Duration::from_millis(track.duration_ms));
            let line = if wide {
                Line::from(vec![
                    Span::styled(format!(" {lead}  "), Style::default().fg(app.theme.accent)),
                    Span::raw(shorten(&track.title, 28)),
                    Span::styled(
                        format!("  {}", shorten(&track.artists, 20)),
                        Style::default().fg(app.theme.muted),
                    ),
                    Span::styled(
                        format!("  {}", shorten(&track.album, 20)),
                        Style::default().fg(app.theme.muted),
                    ),
                    Span::styled(
                        format!("  {duration}"),
                        Style::default().fg(app.theme.muted),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(format!(" {lead}  "), Style::default().fg(app.theme.accent)),
                    Span::raw(shorten(&track.title, 25)),
                    Span::styled(
                        format!("  {}  {duration}", shorten(&track.artists, 18)),
                        Style::default().fg(app.theme.muted),
                    ),
                ])
            };
            ListItem::new(line)
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default()
        .with_selected((!tracks.is_empty()).then_some(selected.saturating_sub(offset)));
    let title = title_with_position(title, selected, tracks.len());
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(app, &title, focused))
            .highlight_style(selection_style(app, focused)),
        area,
        &mut state,
    );
    render_scrollbar(frame, app, area, tracks.len(), selected);
}

fn draw_playlists(
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

fn draw_albums(
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

fn draw_artists(
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

struct ListSelection {
    selected: usize,
    focused: bool,
    offset: usize,
    len: usize,
}

fn draw_selectable_list(
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

fn render_scrollbar(frame: &mut Frame, app: &App, area: Rect, total: usize, selected: usize) {
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

fn title_with_position(title: &str, selected: usize, total: usize) -> String {
    let title = title.trim();
    if total == 0 {
        format!(" {title} ")
    } else {
        format!(" {title} · {}/{} ", selected.min(total - 1) + 1, total)
    }
}

fn draw_account(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_playback_cache(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_downloads(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_player(frame: &mut Frame, app: &App, area: Rect) {
    let state = app.player_state();
    let status = player_status(state);
    let layout = player_layout(area, text_width(&status));
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
        .map(|track| track.title.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("尚未播放");
    let artists = app
        .current_track
        .as_ref()
        .map(|track| track.artists.as_str())
        .filter(|artists| !artists.is_empty())
        .unwrap_or("选择歌曲开始播放");
    let album = app
        .current_track
        .as_ref()
        .map(|track| track.album.as_str())
        .unwrap_or_default();
    frame.render_widget(panel(app, " 播放器 ", false), area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{icon}  "),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                shorten(title, 30),
                Style::default()
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {}{}",
                    shorten(artists, 20),
                    if album.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", shorten(album, 16))
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

fn account_label(app: &App) -> &str {
    app.identity
        .as_ref()
        .map(|identity| identity.nickname.as_str())
        .unwrap_or("L 登录")
}

fn context_actions(app: &App) -> Vec<UiAction> {
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
            UiAction::Search,
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
        Route::Queue => {
            actions.push(UiAction::Dequeue);
            actions.push(UiAction::Details);
        }
        Route::Local => {
            actions.push(UiAction::Import);
            actions.push(UiAction::Refresh);
            actions.push(UiAction::Search);
            actions.push(UiAction::Details);
            actions.push(UiAction::Organize);
        }
        _ if app.selected_track().is_some() => {
            actions.push(UiAction::TogglePause);
            actions.push(UiAction::ToggleFavorite);
            actions.push(UiAction::Details);
        }
        _ => {}
    }
    actions.push(UiAction::ToggleHelp);
    actions
}

fn format_action_hint(action: UiAction) -> String {
    let hint = action_hint(action);
    format!("{} {}", hint.key, hint.label)
}

fn format_context_action_hint(app: &App, action: UiAction) -> String {
    if app.route == Route::Local && action == UiAction::Refresh {
        "r 扫描".into()
    } else {
        format_action_hint(action)
    }
}

fn hint_line(actions: &[UiAction]) -> String {
    actions
        .iter()
        .copied()
        .map(format_action_hint)
        .collect::<Vec<_>>()
        .join("  ·  ")
}

fn footer_text(app: &App, width: u16) -> String {
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

fn input_footer(prefix: &str, input: &str, cursor: usize, width: u16) -> (String, u16) {
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

fn fit_text(value: &str, width: u16) -> String {
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

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
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
            Line::from("l 聚焦并放大歌词，再按恢复 · [/] 快退/快进"),
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
                "{}  导入本地文件或目录  ·  {}  扫描已配置目录",
                format_action_hint(UiAction::Import),
                format_action_hint(UiAction::Refresh),
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

fn draw_details_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let Some(track) = app.selected_track() else {
        return;
    };
    let popup = centered(area, 68, 18);
    frame.render_widget(Clear, popup);
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
                Span::raw(track.id.to_string()),
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

fn panel<'a>(app: &App, title: &'a str, focused: bool) -> Block<'a> {
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

fn breadcrumb_text(app: &App) -> String {
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

fn truncate_to_width(value: &str, width: u16) -> String {
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

fn overlay_panel<'a>(app: &App, title: &'a str) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.overlay).fg(app.theme.text))
}

fn selection_style(app: &App, active: bool) -> Style {
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
struct QrLayout {
    card: Rect,
    code: Rect,
    status: Rect,
}

fn qr_dimensions(qr: &str) -> (u16, u16) {
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

fn qr_layout(area: Rect, code_width: u16, code_height: u16) -> Option<QrLayout> {
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn help_popup(area: Rect) -> Rect {
    centered(area, 76, 18)
}

fn format_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn format_library_duration(duration_ms: u64) -> String {
    let minutes = duration_ms / 60_000;
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
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

fn format_cache_size_input(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes.is_multiple_of(GIB) {
        format!("{}GiB", bytes / GIB)
    } else if bytes.is_multiple_of(MIB) {
        format!("{}MiB", bytes / MIB)
    } else {
        bytes.to_string()
    }
}

fn parse_cache_size(value: &str) -> Result<u64, &'static str> {
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

fn player_status(state: &PlayerState) -> String {
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

fn text_width(value: &str) -> u16 {
    u16::try_from(Line::from(value).width()).unwrap_or(u16::MAX)
}

fn centered_line(area: Rect, y: u16, width: u16) -> Rect {
    let width = width.min(area.width);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        y,
        width,
        u16::from(area.height > 0),
    )
}

fn shorten(value: &str, max: usize) -> String {
    let length = value.chars().count();
    if length <= max {
        return value.to_owned();
    }
    let visible = max.saturating_sub(1);
    format!("{}…", value.chars().take(visible).collect::<String>())
}

fn search_kind_label(kind: SearchKind) -> &'static str {
    match kind {
        SearchKind::Song => "歌曲",
        SearchKind::Album => "专辑",
        SearchKind::Artist => "歌手",
        SearchKind::Playlist => "歌单",
    }
}

fn draw_local_empty(frame: &mut Frame, app: &App, area: Rect, title: &str) {
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

fn parse_import_path(input: &str) -> Result<PathBuf, String> {
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

fn expand_user_path(value: &str) -> PathBuf {
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn unescape_shell_path(value: &str) -> String {
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

fn empty_message(app: &App) -> String {
    match app.route {
        Route::Daily | Route::Recommended | Route::Created | Route::Subscribed | Route::Albums
            if app.identity.is_none() =>
        {
            "按 L 扫码登录".into()
        }
        Route::Search => "按 / 输入关键词".into(),
        Route::Queue => "播放队列为空".into(),
        Route::Favorites => "还没有喜欢的音乐".into(),
        Route::Local => LOCAL_IMPORT_HINT.into(),
        Route::Recent => "暂无最近播放".into(),
        _ => "暂无内容".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };
    use tempfile::TempDir;

    use crate::ncm_core::{NcmClient, SessionConfig};

    fn test_app() -> (TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let client = NcmClient::new(SessionConfig::default(), Duration::from_secs(1)).unwrap();
        let services = Services {
            authentication: Authentication::new(
                client.clone(),
                directory.path().join("session.json"),
            ),
            discovery: Discovery::new(client.clone()),
            library: Library::open(directory.path()).unwrap(),
            downloader: Downloader::new(client, directory.path(), 1).unwrap(),
            library_roots: vec![directory.path().to_path_buf()],
            playback_cache: PlaybackCache::open_blocking(
                directory.path().join("playback-cache"),
                4 * 1024 * 1024 * 1024,
            )
            .unwrap(),
        };
        (directory, App::new(services))
    }

    fn render_app(app: &App, width: u16, height: u16) -> (String, (u16, u16)) {
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
            },
            PlaylistSummary {
                id: 2,
                name: "我收藏的".into(),
                track_count: 3,
                created_by_user: false,
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
        };
        app.daily_cache = Some(vec![track.clone()]);
        app.recommended_cache = Some(vec![PlaylistSummary {
            id: 42,
            name: "缓存歌单".into(),
            track_count: 1,
            created_by_user: false,
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
            .send(AppEvent::PlaylistWarmed(1, "歌单".into(), vec![]))
            .unwrap();
        app.event_tx
            .send(AppEvent::ListeningRankWarmed(ListeningRank::Week, vec![]))
            .unwrap();
        app.event_tx.send(AppEvent::MetadataWarmFinished).unwrap();

        assert!(!app.poll_events());
        assert!(app.daily_cache.is_some());
        assert!(app.recommended_cache.is_some());
        assert!(app.playlist_cache.is_some());
        assert!(app.playlist_track_cache.contains_key(&1));
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
            BrowserColumn {
                id: 1,
                title: "电子音乐".into(),
                content: Content::Empty,
                selected: 0,
                phase: ColumnPhase::Ready,
            },
            BrowserColumn {
                id: 2,
                title: "夜航".into(),
                content: Content::Empty,
                selected: 0,
                phase: ColumnPhase::Ready,
            },
            BrowserColumn {
                id: 3,
                title: "夜航".into(),
                content: Content::Empty,
                selected: 0,
                phase: ColumnPhase::Ready,
            },
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
        let hits = calculate_hits(
            Rect::new(0, 0, MIN_WIDTH - 1, MIN_HEIGHT),
            16,
            6,
            1,
            Focus::Content,
            false,
        );
        assert_eq!(hits.nav, Rect::default());
        assert_eq!(hits.content, Rect::default());
    }

    #[test]
    fn compact_layout_gives_content_the_full_width() {
        for width in MIN_WIDTH..COMPACT_WIDTH {
            let area = Rect::new(0, 0, width, 24);
            let layout = app_layout(area, 1, Focus::Content, false).unwrap();
            let hits = calculate_hits(area, 16, 6, 1, Focus::Content, false);
            assert_eq!(layout.navigation, None);
            assert_eq!(hits.nav, Rect::default());
            assert_eq!(hits.content.x, 1);
            assert_eq!(hits.content.width, width - 2);
        }
    }

    #[test]
    fn panel_borders_are_not_interactive() {
        let hits = calculate_hits(Rect::new(0, 0, 96, 30), 16, 6, 1, Focus::Content, false);
        assert!(!contains(hits.nav, 0, hits.nav.y));
        assert!(!contains(hits.content, 24, hits.content.y));
        assert_eq!(hits.nav.x, 1);
        assert_eq!(hits.content.x, 25);
    }

    #[test]
    fn wide_layout_keeps_cascade_order_and_a_separate_lyrics_column() {
        let layout = app_layout(Rect::new(0, 0, 160, 30), 4, Focus::Column(2), false).unwrap();
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
    fn compact_lyrics_focus_does_not_compete_with_browser_columns() {
        let layout = app_layout(Rect::new(0, 0, 60, 24), 3, Focus::Lyrics, true).unwrap();

        assert!(layout.browser.is_empty());
        assert_eq!(layout.lyrics, Some(Rect::new(0, 2, 60, 16)));
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
            let layout = player_layout(area, 18);
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
        }
    }

    #[test]
    fn keyboard_and_player_buttons_share_actions() {
        let layout = player_layout(Rect::new(0, 0, 96, 5), 18);
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
        let layout = player_layout(Rect::new(0, 0, 96, 5), 18);
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
                id: 1,
                title: "第一层".into(),
                content: Content::Tracks(vec![test_track(4), test_track(5)]),
                selected: 1,
                phase: ColumnPhase::Ready,
            },
            BrowserColumn {
                id: 2,
                title: "第二层".into(),
                content: Content::Tracks(vec![test_track(6)]),
                selected: 0,
                phase: ColumnPhase::Ready,
            },
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
            id: 1,
            title: "歌单".into(),
            content: Content::Tracks(vec![test_track(3), test_track(4)]),
            selected: 1,
            phase: ColumnPhase::Ready,
        }];
        app.focus = Focus::Column(0);

        app.toggle_lyrics_focus();
        assert_eq!(app.focus, Focus::Lyrics);
        assert!(app.show_lyrics);
        assert_eq!(app.selected, 1);
        assert_eq!(app.columns[0].selected, 1);

        app.toggle_lyrics_focus();
        assert_eq!(app.focus, Focus::Column(0));
        assert!(!app.show_lyrics);
        assert_eq!(app.selected, 1);
        assert_eq!(app.columns[0].selected, 1);
    }

    #[tokio::test]
    async fn footer_is_bounded_and_minimum_sizes_render() {
        let (_directory, mut app) = test_app();
        app.status = "一条很长的状态消息，不应该让底栏换行或污染其他点击区域".repeat(4);
        for width in [60, 80] {
            let footer = footer_text(&app, width);
            assert!(text_width(&footer) <= width);
            assert!(footer.contains("? 帮助"));
            let (screen, _) = render_app(&app, width, MIN_HEIGHT);
            assert!(!screen.contains("终端太小"));
            assert!(screen.contains("♫"));
            assert!(
                screen.replace(' ', "").contains("?帮助"),
                "{width}-column screen:\n{screen}"
            );
        }
    }

    #[tokio::test]
    async fn compact_navigation_is_only_visible_while_focused() {
        let (_directory, mut app) = test_app();
        app.focus = Focus::Navigation;
        let (navigation, _) = render_app(&app, 60, 24);
        assert!(navigation.replace(' ', "").contains("每日推荐"));

        app.focus = Focus::Content;
        let (content, _) = render_app(&app, 60, 24);
        assert!(!content.replace(' ', "").contains("每日推荐"));
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

        let (screen, _) = render_app(&app, 80, 24);
        let compact = screen.replace(' ', "");
        assert!(compact.contains("搜索本地音乐：爵士"));
        assert!(!compact.contains("搜索网易云"));

        dispatch_action(&mut app, UiAction::Back);
        assert!(app.mode == InputMode::Normal);
        assert!(app.input.is_empty());
        assert!(app.loading);
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
        let (screen, _) = render_app(&app, 80, 24);
        let compact = screen.replace(' ', "");
        assert!(
            compact.contains("按I或点击这里导入"),
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
        let (_, cursor) = render_app(&app, 80, 24);
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
            let (screen, _) = render_app(&app, width, 24);
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
}
