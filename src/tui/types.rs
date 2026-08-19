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

pub(super) const MIN_WIDTH: u16 = 60;
pub(super) const MIN_HEIGHT: u16 = 24;
pub(super) const WIDE_WIDTH: u16 = 96;
#[allow(dead_code)]
pub(super) const COMPACT_WIDTH: u16 = 72;
pub(super) const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub(super) const QR_PADDING_X: u16 = 2;
pub(super) const QR_PADDING_Y: u16 = 1;
pub(super) const QR_STATUS_GAP: u16 = 1;
pub(super) const UI_TICK_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const PROGRESS_INTERVAL: Duration = Duration::from_millis(80);
pub(super) const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
pub(super) const HELP_MAX_SCROLL: u16 = 8;
pub(super) const LOCAL_IMPORT_HINT: &str = "暂无本地音乐 · 按 I 或点击这里导入";
pub(super) const TOAST_TTL: Duration = Duration::from_secs(4);

#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) background: Color,
    pub(super) panel: Color,
    pub(super) panel_highlight: Color,
    pub(super) overlay: Color,
    pub(super) border: Color,
    pub(super) text: Color,
    pub(super) muted: Color,
    pub(super) accent: Color,
    pub(super) qr_light: Color,
    pub(super) qr_dark: Color,
}

impl Theme {
    pub(super) fn miku() -> Self {
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

    pub(super) fn from_env() -> Self {
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
    pub ui_state_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Route {
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
    pub(super) const ALL: [Self; 14] = [
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

    pub(super) fn label(self) -> &'static str {
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

    pub(super) fn group(self) -> &'static str {
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
pub(super) enum Focus {
    Navigation,
    Content,
    Column(usize),
    Lyrics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InputMode {
    Normal,
    Search,
    LocalSearch,
    Download,
    CacheSize,
    Import,
    Palette,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UiAction {
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
    Palette,
    HideLyrics,
    Expand,
    Sort,
    ClearQueue,
    EditCacheSize,
    ClearCache,
    OpenRoute(Route),
}

#[derive(Clone, Copy)]
pub(super) struct ActionHint {
    pub(super) key: &'static str,
    pub(super) label: &'static str,
}

pub(super) const PREVIOUS_ICON: &str = "‹";
pub(super) const NEXT_ICON: &str = "›";
pub(super) const PLAY_ICON: &str = "▶";
pub(super) const PAUSE_ICON: &str = "Ⅱ";

pub(super) fn action_hint(action: UiAction) -> ActionHint {
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
        UiAction::Palette => ActionHint {
            key: "Ctrl+P",
            label: "命令",
        },
        UiAction::HideLyrics => ActionHint {
            key: "h",
            label: "歌词栏",
        },
        UiAction::Expand => ActionHint {
            key: "e",
            label: "展开",
        },
        UiAction::Sort => ActionHint {
            key: "s",
            label: "排序",
        },
        UiAction::ClearQueue => ActionHint {
            key: "c",
            label: "清空队列",
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
pub(super) enum JobKind {
    Download,
    Organize,
    Scan,
    Import,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlayMode {
    Sequential,
    RepeatOne,
    RepeatAll,
    Shuffle,
}

impl PlayMode {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Sequential => Self::RepeatOne,
            Self::RepeatOne => Self::RepeatAll,
            Self::RepeatAll => Self::Shuffle,
            Self::Shuffle => Self::Sequential,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Sequential => "顺序",
            Self::RepeatOne => "单曲",
            Self::RepeatAll => "循环",
            Self::Shuffle => "随机",
        }
    }
}

pub(super) enum LyricsState {
    Idle,
    Loading(u64),
    Ready(u64, Lyrics),
    Error(u64, String),
}

#[derive(Clone)]
pub(super) struct TrackRow {
    pub(super) id: u64,
    pub(super) title: String,
    pub(super) artists: String,
    pub(super) album: String,
    pub(super) duration_ms: u64,
    pub(super) path: Option<PathBuf>,
    pub(super) favorite: bool,
    pub(super) play_count: u64,
    pub(super) format: String,
    pub(super) bytes: u64,
    pub(super) cover_url: String,
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
            cover_url: String::new(),
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
            cover_url: track.cover_url,
        }
    }
}

pub(super) enum Content {
    Tracks(Vec<TrackRow>),
    Playlists(Vec<PlaylistSummary>),
    Artists(Vec<ArtistSummary>),
    Albums(Vec<AlbumSummary>),
    LocalMenu(Vec<LocalMenuItem>),
    Empty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalMenuItem {
    Tracks(TrackView),
    Albums,
    Artists,
    Collections,
}

impl LocalMenuItem {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Tracks(TrackView::All) => "全部歌曲",
            Self::Albums => "专辑",
            Self::Artists => "歌手",
            Self::Collections => "本地歌单",
            Self::Tracks(TrackView::RecentAdded) => "最近添加",
            Self::Tracks(TrackView::Unplayed) => "未播放",
            Self::Tracks(TrackView::Frequent) => "常听",
            Self::Tracks(TrackView::Incomplete) => "元数据不全",
            Self::Tracks(TrackView::Large) => "大文件",
            Self::Tracks(TrackView::Missing) => "缺失文件",
            Self::Tracks(TrackView::Favorites) => "本地红心",
        }
    }
}

pub(super) fn local_menu_items() -> Vec<LocalMenuItem> {
    vec![
        LocalMenuItem::Tracks(TrackView::All),
        LocalMenuItem::Albums,
        LocalMenuItem::Artists,
        LocalMenuItem::Collections,
        LocalMenuItem::Tracks(TrackView::RecentAdded),
        LocalMenuItem::Tracks(TrackView::Unplayed),
        LocalMenuItem::Tracks(TrackView::Frequent),
        LocalMenuItem::Tracks(TrackView::Incomplete),
        LocalMenuItem::Tracks(TrackView::Large),
        LocalMenuItem::Tracks(TrackView::Missing),
        LocalMenuItem::Tracks(TrackView::Favorites),
    ]
}

impl Content {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Tracks(values) => values.len(),
            Self::Playlists(values) => values.len(),
            Self::Artists(values) => values.len(),
            Self::Albums(values) => values.len(),
            Self::LocalMenu(values) => values.len(),
            Self::Empty => 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub(super) enum Loaded {
    Tracks(String, Vec<OnlineTrack>),
    RankedTracks(String, Vec<RankedTrack>),
    Playlists(Vec<PlaylistSummary>),
    #[allow(dead_code)]
    Artists(Vec<ArtistSummary>),
    #[allow(dead_code)]
    Albums(Vec<AlbumSummary>),
    LocalTracks(String, Vec<Track>, Option<LibraryStats>),
    TrackPage(TrackPage, PagedSource),
    SearchPage(SearchPage, PagedSource),
    PlaylistPage(Vec<PlaylistSummary>, PaginationInfo, PagedSource),
    AlbumPage(Vec<AlbumSummary>, PaginationInfo, PagedSource),
    ArtistPage(Vec<ArtistSummary>, PaginationInfo, PagedSource),
}

pub(super) struct ActiveJob {
    pub(super) id: u64,
    pub(super) kind: JobKind,
    pub(super) handle: JoinHandle<()>,
}

pub(super) struct BrowserColumn {
    pub(super) id: u64,
    pub(super) title: String,
    pub(super) content: Content,
    pub(super) selected: usize,
    pub(super) phase: ColumnPhase,
    pub(super) pagination: PaginationInfo,
    pub(super) source: Option<PagedSource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ColumnPhase {
    Loading,
    Ready,
    Error(String),
}

impl BrowserColumn {
    pub(super) fn loading(id: u64, title: String) -> Self {
        Self {
            id,
            title,
            content: Content::Empty,
            selected: 0,
            phase: ColumnPhase::Loading,
            pagination: PaginationInfo::default(),
            source: None,
        }
    }

    pub(super) fn is_loading(&self) -> bool {
        self.phase == ColumnPhase::Loading || self.pagination.loading
    }

    pub(super) fn ready(id: u64, title: String, content: Content) -> Self {
        Self {
            id,
            title,
            content,
            selected: 0,
            phase: ColumnPhase::Ready,
            pagination: PaginationInfo::default(),
            source: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PagedSource {
    Playlist { id: u64, name: String },
    Album { id: u64, name: String },
    Artist { id: u64, name: String },
    Search { query: String, kind: SearchKind },
    UserPlaylists { user_id: u64, scope: PlaylistScope },
    SubscribedAlbums,
    SubscribedArtists,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToastKind {
    Success,
    Warn,
    Error,
}

pub(super) struct Toast {
    pub(super) kind: ToastKind,
    pub(super) message: String,
    pub(super) until: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PaletteCommand {
    Route(Route),
    Import,
    Scan,
    Organize,
    Login,
    Download,
    ToggleLyrics,
    TogglePause,
    NextTrack,
    PreviousTrack,
    Details,
    Help,
    HideLyrics,
    Expand,
}

pub(super) enum AppEvent {
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
    PageLoaded {
        column_id: Option<u64>,
        generation: u64,
        source: PagedSource,
        offset: usize,
        result: Result<Loaded, String>,
    },
    LyricsLoaded(u64, Result<Lyrics, String>),
    CoverLoaded(u64, Vec<u8>),
    DailyWarmed(Vec<OnlineTrack>),
    RecommendedWarmed(Vec<PlaylistSummary>),
    UserPlaylistsWarmed(Vec<PlaylistSummary>),
    AlbumsWarmed(Vec<AlbumSummary>),
    ArtistsWarmed(Vec<ArtistSummary>),
    ListeningRankWarmed(ListeningRank, Vec<RankedTrack>),
    MetadataWarmFinished,
    CacheLimitUpdated(Result<CacheStats, String>),
    CacheCleared(Result<ClearReport, String>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ColumnHit {
    pub(super) index: usize,
    pub(super) area: Rect,
    pub(super) offset: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct HitRegions {
    pub(super) account: Rect,
    pub(super) nav: Rect,
    pub(super) content: Rect,
    pub(super) columns: Vec<ColumnHit>,
    pub(super) lyrics: Rect,
    pub(super) progress: Rect,
    pub(super) previous: Rect,
    pub(super) pause: Rect,
    pub(super) next: Rect,
    pub(super) play_mode: Rect,
    pub(super) volume: Rect,
    pub(super) content_offset: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PlayerLayout {
    pub(super) song_info: Rect,
    pub(super) elapsed: Rect,
    pub(super) progress: Rect,
    pub(super) duration: Rect,
    pub(super) previous: Rect,
    pub(super) pause: Rect,
    pub(super) next: Rect,
    pub(super) play_mode: Rect,
    pub(super) volume: Rect,
    pub(super) cover: Rect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BrowserPane {
    pub(super) index: usize,
    pub(super) area: Rect,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct AppLayout {
    pub(super) header: Rect,
    pub(super) navigation: Option<Rect>,
    pub(super) browser: Vec<BrowserPane>,
    pub(super) lyrics: Option<Rect>,
    pub(super) player: Rect,
    pub(super) footer: Rect,
}
