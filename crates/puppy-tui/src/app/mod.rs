//! The dashboard's pure state machine: [`App::update`] folds a [`Msg`] into the state
//! and returns the daemon [`Cmd`]s to run. No terminal I/O, no rendering, no clock:
//! the caller passes `now`.

pub mod forms;
pub mod history;
pub mod input;
pub mod keys;
pub mod picker;

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use puppy_core::types::{AddRequest, DownloadId, EngineDetail, SeedPolicy, Selection, TorrentFile};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Event, Settings, Totals, WireState};
use ratatui::layout::{Position, Rect};

pub use forms::{AddField, AddForm, GlobalLimitsForm, LimitForm, SeedForm};
pub use history::History;
pub use input::TextInput;
pub use keys::KeyAction;
pub use picker::{Picker, PickerOutcome};

use crate::theme::Theme;

/// How long a toast stays up.
pub const TOAST_TTL: Duration = Duration::from_secs(4);
/// Toasts visible at once (the oldest is dropped).
pub const MAX_TOASTS: usize = 3;
pub const MAX_ACTIVE_RANGE: (u16, u16) = (1, 32);
/// Engine details remembered for this many downloads (least recently used goes).
pub const DETAIL_CACHE: usize = 32;
/// After a popup submit, plain keys arriving this soon are the rest of a paste burst
/// (terminals without bracketed paste, e.g. legacy Windows consoles, type it out).
pub const PASTE_BURST: Duration = Duration::from_millis(30);
/// A torrent's per-download limit is taken when it starts, not while it runs.
pub const TORRENT_LIMIT_TOAST: &str = "per-torrent limits can only be set before a torrent starts";
/// Toast prefix for [`CmdError::Unsupported`].
pub const UNSUPPORTED_PREFIX: &str = "not supported for this download: ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

/// A connection problem shown in a band under the title bar until the next connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Banner {
    /// The daemon speaks another protocol version (the detail, for logs); `U` restarts it.
    Incompatible(String),
    /// No daemon could be started; the text says where its log is.
    NoDaemon(String),
    /// Any other reason connecting failed.
    Error(String),
    /// `U` was pressed: the old daemon is being stopped and a new one started.
    Restarting,
}

impl Banner {
    pub const INCOMPATIBLE: &str = "daemon is a different puppy version — press U to restart it";
    /// The key named in [`INCOMPATIBLE`](Self::INCOMPATIBLE) (drawn highlighted).
    pub const RESTART_KEY: &str = "U";
    pub const RESTARTING: &str = "restarting the daemon…";
    pub const ERROR_PREFIX: &str = "cannot connect to the daemon: ";

    /// The banner's text, as drawn.
    pub fn text(&self) -> String {
        match self {
            Banner::Incompatible(_) => Self::INCOMPATIBLE.to_string(),
            Banner::NoDaemon(hint) => hint.clone(),
            Banner::Error(e) => format!("{}{e}", Self::ERROR_PREFIX),
            Banner::Restarting => Self::RESTARTING.to_string(),
        }
    }
}

/// The first line of `s`, trimmed and safe to draw (a banner is one row).
fn one_line(s: &str) -> String {
    let first = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    crate::format::display_safe(first).into_owned()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Popup {
    Add(AddForm),
    Limit(LimitForm),
    GlobalLimits(GlobalLimitsForm),
    ConfirmDelete {
        id: DownloadId,
        name: String,
        delete_files: bool,
    },
    Help,
    /// Choosing a torrent's files (`c`, or opened when a torrent starts waiting).
    Picker(Box<Picker>),
    /// A torrent's seeding policy (`s`).
    Seed(SeedForm),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Normal,
    /// Typing into the filter.
    Filter,
    Popup(Popup),
}

/// What a screen region is, for mouse handling (filled by drawing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    ListRow(DownloadId),
    /// A footer hint; non-char keys use their primary char (`Delete` ⇒ `'d'`).
    FooterKey(char),
    /// The whole list panel (wheel scrolling).
    List,
    PopupInside,
    PopupCheckbox,
    /// A popup's confirm button.
    PopupButton,
    /// A picker row (index in [`Picker::rows`]): a click toggles it.
    PickerRow(usize),
    /// A picker folder's fold arrow (row index).
    PickerFold(usize),
}

pub type HitMap = Vec<(Rect, HitTarget)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub until: Instant,
}

/// Why a daemon command failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CmdError {
    /// The daemon cannot do this for this download (its engine does not support it).
    Unsupported(String),
    /// Any other failure: the text for the toast.
    Failed(String),
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmdError::Unsupported(s) | CmdError::Failed(s) => f.write_str(s),
        }
    }
}

impl From<String> for CmdError {
    fn from(s: String) -> Self {
        CmdError::Failed(s)
    }
}

impl From<&str> for CmdError {
    fn from(s: &str) -> Self {
        CmdError::Failed(s.to_string())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CmdOutcome {
    /// What was attempted (`pause`), for the error toast.
    pub label: String,
    pub result: Result<(), CmdError>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    /// Every 500 ms.
    Tick,
    Daemon(Event),
    Connected {
        version: String,
        settings: Settings,
        downloads: Vec<DownloadSnapshot>,
    },
    Disconnected,
    /// Connecting found a daemon speaking another protocol version.
    Incompatible(String),
    /// No daemon answered, even after starting one (a hint naming its log).
    NoDaemon(String),
    /// Connecting failed for another reason.
    ConnectError(String),
    CmdResult(CmdOutcome),
    Detail(DownloadId, Option<EngineDetail>),
    /// The daemon's settings, fetched again after changing them.
    Settings(Settings),
    /// A torrent's file list ([`Cmd::FetchFiles`]).
    Files(DownloadId, Vec<TorrentFile>),
    /// [`Cmd::FetchFiles`] failed.
    FilesFailed(DownloadId, CmdError),
    /// The answer to [`Cmd::SelectFiles`].
    Selected(DownloadId, Result<(), CmdError>),
    /// Quit (a termination signal).
    Quit,
}

// Cmds are short-lived (one per key press), so the inline `AddRequest` costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Add {
        request: AddRequest,
        start_paused: bool,
    },
    Pause(DownloadId),
    Resume(DownloadId),
    Remove {
        id: DownloadId,
        delete_files: bool,
    },
    Restart(DownloadId),
    Move {
        id: DownloadId,
        position: usize,
    },
    SetLimit {
        id: DownloadId,
        bps: Option<u64>,
    },
    SetGlobalLimits {
        down: Option<u64>,
        up: Option<u64>,
    },
    SetMaxActive(u16),
    FetchDetail(DownloadId),
    /// Answered with [`Msg::Files`] (or [`Msg::FilesFailed`]).
    FetchFiles(DownloadId),
    /// Answered with [`Msg::Selected`]. Indices are 0-based (the wire's).
    SelectFiles {
        id: DownloadId,
        selection: Selection,
    },
    /// Replaces a torrent's seeding policy.
    SetSeed {
        id: DownloadId,
        policy: SeedPolicy,
    },
    /// Answered with [`Msg::Settings`]; the runtime issues it after changing settings.
    FetchSettings,
    /// Stop the incompatible daemon (`U`); the runtime then reconnects, which starts
    /// this version's daemon.
    RestartDaemon,
    Quit,
}

/// Actions on one download whose validity depends on its state (keys and footer hints).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Pause,
    Resume,
    Delete,
    Restart,
    Limit,
    MoveUp,
    MoveDown,
    /// Open the file picker of a torrent waiting in "choosing files".
    ChooseFiles,
    /// Set a torrent's seeding policy.
    Seed,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Pause => "pause",
            Action::Resume => "resume",
            Action::Delete => "delete",
            Action::Restart => "restart",
            Action::Limit => "limit",
            Action::MoveUp | Action::MoveDown => "move",
            Action::ChooseFiles => "choose files for",
            Action::Seed => "set seeding for",
        }
    }
}

/// "a completed download", "a download that needs attention", … for toasts.
fn describe(state: &WireState) -> &'static str {
    match state {
        WireState::Queued => "a queued download",
        WireState::Starting => "a starting download",
        WireState::Active => "an active download",
        WireState::Paused => "a paused download",
        WireState::Completed => "a completed download",
        WireState::ChoosingFiles => "a download waiting for a file choice",
        WireState::Seeding { .. } => "a seeding download",
        WireState::Failed { .. } => "a failed download",
        WireState::NeedsAttention { .. } => "a download that needs attention",
        WireState::Unknown => "a download in an unknown state",
    }
}

#[derive(Clone, Debug)]
pub struct App {
    pub conn: Conn,
    pub daemon_version: Option<String>,
    /// In daemon order.
    pub downloads: Vec<DownloadSnapshot>,
    pub totals: Totals,
    pub settings: Option<Settings>,
    /// Selection by id: survives reordering.
    pub selected: Option<DownloadId>,
    /// First visible list row (maintained by drawing).
    pub list_offset: usize,
    /// Case-insensitive substring filter on name/source.
    pub filter: TextInput,
    pub mode: Mode,
    /// The selection's engine detail (from the cache until a fresh one arrives).
    pub detail: Option<(DownloadId, EngineDetail)>,
    /// Recent details by download, least recently used first (at most
    /// [`DETAIL_CACHE`]), so moving back to an item shows its detail at once.
    detail_cache: VecDeque<(DownloadId, EngineDetail)>,
    pub history: History,
    /// Oldest first.
    pub toasts: VecDeque<Toast>,
    pub size: (u16, u16),
    pub theme: Theme,
    pub should_quit: bool,
    /// Hit regions set by drawing.
    pub hits: HitMap,
    /// Wall-clock Unix seconds for "retry in m:ss" (the runtime sets it before each
    /// [`Msg::Tick`]; `update` itself never reads a clock).
    pub now_unix: u64,
    /// Ticks seen so far: animates indeterminate progress bars.
    pub ticks: u64,
    /// First visible row of the help popup (clamped by drawing).
    pub help_scroll: u16,
    /// A connection problem to show (cleared on connect).
    pub banner: Option<Banner>,
    /// The last left click on a list row, for double-click detection.
    last_click: Option<(DownloadId, Instant)>,
    /// When a popup was last submitted (paste burst guard, [`PASTE_BURST`]).
    submitted_at: Option<Instant>,
    /// Torrents whose picker opened by itself this session (it does so once per
    /// torrent; `c` reopens it).
    asked: HashSet<DownloadId>,
    /// The torrent whose file list was fetched to open its picker by itself.
    auto_pending: Option<DownloadId>,
}

impl App {
    pub fn new(theme: Theme) -> Self {
        Self {
            conn: Conn::Connecting,
            daemon_version: None,
            downloads: Vec::new(),
            totals: Totals::default(),
            settings: None,
            selected: None,
            list_offset: 0,
            filter: TextInput::new(),
            mode: Mode::Normal,
            detail: None,
            detail_cache: VecDeque::new(),
            history: History::default(),
            toasts: VecDeque::new(),
            size: (80, 24),
            theme,
            should_quit: false,
            hits: HitMap::new(),
            now_unix: 0,
            ticks: 0,
            help_scroll: 0,
            banner: None,
            last_click: None,
            submitted_at: None,
            asked: HashSet::new(),
            auto_pending: None,
        }
    }

    /// Downloads matching the filter, in daemon order.
    pub fn visible(&self) -> Vec<&DownloadSnapshot> {
        self.visible_iter().collect()
    }

    /// [`visible`](Self::visible) without collecting: drawing walks it every frame, so
    /// matching allocates nothing per download.
    pub fn visible_iter(&self) -> impl Iterator<Item = &DownloadSnapshot> + Clone {
        let needle: Vec<char> = self
            .filter
            .value
            .trim()
            .chars()
            .flat_map(char::to_lowercase)
            .collect();
        self.downloads.iter().filter(move |d| {
            needle.is_empty()
                || contains_folded(&d.name, &needle)
                || contains_folded(&d.source, &needle)
        })
    }

    /// The selected download's snapshot.
    pub fn selection(&self) -> Option<&DownloadSnapshot> {
        let id = self.selected?;
        self.downloads.iter().find(|d| d.id == id)
    }

    /// Index of the selection in [`visible`](Self::visible).
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.visible().iter().position(|d| d.id == id)
    }

    /// Whether `action` is valid for `d` in its current state.
    pub fn can(&self, action: Action, d: &DownloadSnapshot) -> bool {
        if matches!(d.state, WireState::Unknown) || d.kind == EngineKindTag::Unknown {
            return action == Action::Delete;
        }
        match action {
            Action::Delete => true,
            // Pausing a seeding torrent stops its seeding.
            Action::Pause => matches!(
                d.state,
                WireState::Queued
                    | WireState::Starting
                    | WireState::Active
                    | WireState::Seeding { .. }
            ),
            // Also a seeding torrent whose data failed its check: resuming downloads it
            // again (other reasons need a restart).
            Action::Resume => match &d.state {
                WireState::Paused | WireState::Failed { .. } => true,
                WireState::NeedsAttention { reason } => reason.is_resumable(),
                _ => false,
            },
            Action::Restart => matches!(
                d.state,
                WireState::Failed { .. } | WireState::NeedsAttention { .. }
            ),
            // A torrent takes its limit when it starts (or once its files are chosen).
            Action::Limit if d.kind == EngineKindTag::Torrent => {
                matches!(d.state, WireState::Queued | WireState::ChoosingFiles)
            }
            Action::Limit => !matches!(d.state, WireState::Completed),
            Action::MoveUp => d.queue_position.is_some_and(|p| p > 0),
            Action::MoveDown => d.queue_position.is_some_and(|p| p + 1 < self.queue_len()),
            Action::ChooseFiles => Self::choosing(d),
            // While seeding, or ahead of it while the torrent still downloads.
            Action::Seed => {
                d.kind == EngineKindTag::Torrent
                    && matches!(
                        d.state,
                        WireState::Seeding { .. }
                            | WireState::Queued
                            | WireState::Starting
                            | WireState::Active
                            | WireState::Paused
                            | WireState::ChoosingFiles
                    )
            }
        }
    }

    /// Whether the daemon has an engine detail for `d` worth showing (it is fetched
    /// while the download runs or is paused).
    pub fn fetches_detail(d: &DownloadSnapshot) -> bool {
        matches!(d.kind, EngineKindTag::Http | EngineKindTag::Torrent)
            && matches!(
                d.state,
                WireState::Active
                    | WireState::Starting
                    | WireState::Paused
                    | WireState::Seeding { .. }
            )
    }

    /// The cached engine detail of `id`, if any.
    pub fn cached_detail(&self, id: DownloadId) -> Option<&EngineDetail> {
        self.detail_cache
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, d)| d)
    }

    /// Remembers `detail` for `id` as the most recently used entry.
    fn cache_detail(&mut self, id: DownloadId, detail: EngineDetail) {
        self.detail_cache.retain(|(i, _)| *i != id);
        self.detail_cache.push_back((id, detail));
        while self.detail_cache.len() > DETAIL_CACHE {
            self.detail_cache.pop_front();
        }
    }

    /// The cached detail of `id`, marked as just used.
    fn take_cached_detail(&mut self, id: DownloadId) -> Option<EngineDetail> {
        let k = self.detail_cache.iter().position(|(i, _)| *i == id)?;
        let entry = self.detail_cache.remove(k)?;
        self.detail_cache.push_back(entry.clone());
        Some(entry.1)
    }

    fn queue_len(&self) -> usize {
        self.downloads
            .iter()
            .filter(|d| d.queue_position.is_some())
            .count()
    }

    /// Shows a toast for [`TOAST_TTL`]; a toast with the same text still showing is not
    /// repeated.
    pub fn push_toast(&mut self, text: impl Into<String>, kind: ToastKind, now: Instant) {
        let text = text.into();
        if self.toasts.iter().any(|t| t.text == text && t.until > now) {
            return;
        }
        self.toasts.push_back(Toast {
            text,
            kind,
            until: now + TOAST_TTL,
        });
        while self.toasts.len() > MAX_TOASTS {
            self.toasts.pop_front();
        }
    }

    pub fn update(&mut self, msg: Msg, now: Instant) -> Vec<Cmd> {
        let before = (self.selected, std::mem::discriminant(&self.mode));
        // A left click manages the double-click memory itself (selecting a row is how
        // a double click starts).
        let click =
            matches!(&msg, Msg::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left));
        let mut cmds = self.update_inner(msg, now);
        if !click && before != (self.selected, std::mem::discriminant(&self.mode)) {
            self.last_click = None;
        }
        // A new selection gets its detail now, not at the next tick.
        if self.selected != before.0
            && let Some(d) = self.selection()
            && Self::fetches_detail(d)
            && !cmds.contains(&Cmd::FetchDetail(d.id))
        {
            cmds.push(Cmd::FetchDetail(d.id));
        }
        cmds
    }

    fn update_inner(&mut self, msg: Msg, now: Instant) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => self.on_key(key, now),
            Msg::Mouse(m) => self.on_mouse(m, now),
            Msg::Paste(text) => {
                self.on_paste(&text);
                Vec::new()
            }
            Msg::Resize(w, h) => {
                self.size = (w, h);
                // The regions are stale until the next frame.
                self.hits.clear();
                Vec::new()
            }
            Msg::Tick => self.on_tick(now),
            Msg::Daemon(event) => {
                self.on_event(event, now);
                self.auto_open()
            }
            Msg::Connected {
                version,
                settings,
                downloads,
            } => {
                self.conn = Conn::Connected;
                self.banner = None;
                self.daemon_version = Some(version);
                self.settings = Some(settings);
                self.totals = totals_of(&downloads, self.totals.down_bps, self.totals.up_bps);
                self.history.prune(&downloads);
                self.replace_downloads(downloads);
                self.auto_open()
            }
            Msg::Disconnected => {
                self.conn = match self.conn {
                    Conn::Reconnecting { attempt } => Conn::Reconnecting {
                        attempt: attempt.saturating_add(1),
                    },
                    _ => Conn::Reconnecting { attempt: 1 },
                };
                Vec::new()
            }
            Msg::Incompatible(detail) => {
                self.banner = Some(Banner::Incompatible(one_line(&detail)));
                Vec::new()
            }
            Msg::NoDaemon(hint) => {
                self.banner = Some(Banner::NoDaemon(one_line(&hint)));
                Vec::new()
            }
            Msg::ConnectError(e) => {
                self.banner = Some(Banner::Error(one_line(&e)));
                Vec::new()
            }
            Msg::CmdResult(outcome) => {
                if let Err(e) = outcome.result {
                    self.cmd_error(&outcome.label, e, now);
                }
                Vec::new()
            }
            Msg::Settings(settings) => {
                self.settings = Some(settings);
                Vec::new()
            }
            Msg::Files(id, files) => {
                self.on_files(id, files);
                Vec::new()
            }
            Msg::FilesFailed(id, e) => {
                if self.auto_pending == Some(id) {
                    self.auto_pending = None;
                }
                if matches!(&self.mode, Mode::Popup(Popup::Picker(p)) if p.id == id) {
                    self.mode = Mode::Normal;
                }
                self.cmd_error("files", e, now);
                Vec::new()
            }
            Msg::Selected(id, result) => {
                let open = matches!(&self.mode, Mode::Popup(Popup::Picker(p)) if p.id == id);
                match result {
                    Ok(()) if open => self.mode = Mode::Normal,
                    Ok(()) => {}
                    Err(e) => match &mut self.mode {
                        Mode::Popup(Popup::Picker(p)) if p.id == id => p.refused(&e.to_string()),
                        _ => self.cmd_error("choose files", e, now),
                    },
                }
                Vec::new()
            }
            Msg::Quit => self.quit(),
            Msg::Detail(id, detail) => {
                // A late reply for a download that stopped running (or is gone) is stale.
                if !self
                    .downloads
                    .iter()
                    .any(|d| d.id == id && Self::fetches_detail(d))
                {
                    return Vec::new();
                }
                match &detail {
                    Some(d) => self.cache_detail(id, d.clone()),
                    None => self.detail_cache.retain(|(i, _)| *i != id),
                }
                if self.selected == Some(id) {
                    self.detail = detail.map(|d| (id, d));
                }
                Vec::new()
            }
        }
    }

    /// The toast for a failed command.
    fn cmd_error(&mut self, label: &str, e: CmdError, now: Instant) {
        match e {
            CmdError::Unsupported(m) => {
                self.push_toast(format!("{UNSUPPORTED_PREFIX}{m}"), ToastKind::Info, now);
            }
            CmdError::Failed(e) => {
                self.push_toast(format!("{label}: {e}"), ToastKind::Error, now);
            }
        }
    }

    /// Whether `d` waits in "choosing files".
    fn choosing(d: &DownloadSnapshot) -> bool {
        d.kind == EngineKindTag::Torrent && matches!(d.state, WireState::ChoosingFiles)
    }

    /// Fetches the file list of a torrent that started waiting for a file choice, to
    /// open its picker — once per torrent, only while no popup is open; the selection
    /// goes first.
    fn auto_open(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Normal || self.auto_pending.is_some() {
            return Vec::new();
        }
        let waiting = |d: &&DownloadSnapshot| Self::choosing(d) && !self.asked.contains(&d.id);
        let Some(id) = self
            .selection()
            .filter(waiting)
            .or_else(|| self.downloads.iter().find(waiting))
            .map(|d| d.id)
        else {
            return Vec::new();
        };
        self.asked.insert(id);
        self.auto_pending = Some(id);
        vec![Cmd::FetchFiles(id)]
    }

    /// A file list arrived: fills the picker waiting for it, or opens one by itself.
    fn on_files(&mut self, id: DownloadId, files: Vec<TorrentFile>) {
        let auto = self.auto_pending == Some(id);
        if auto {
            self.auto_pending = None;
        }
        let name = self
            .downloads
            .iter()
            .find(|d| d.id == id && Self::choosing(d))
            .map(|d| d.name.clone());
        match &mut self.mode {
            Mode::Popup(Popup::Picker(p)) if p.id == id && p.loading => p.set_files(files),
            Mode::Normal if auto => {
                if let Some(name) = name {
                    self.mode = Mode::Popup(Popup::Picker(Box::new(Picker::with_files(
                        id, &name, files,
                    ))));
                }
            }
            // The user is busy elsewhere: asked again once the popup closes.
            _ if auto => {
                self.asked.remove(&id);
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, now: Instant) -> Vec<Cmd> {
        self.ticks = self.ticks.wrapping_add(1);
        self.toasts.retain(|t| t.until > now);
        match self.selection() {
            Some(d) if Self::fetches_detail(d) => vec![Cmd::FetchDetail(d.id)],
            _ => Vec::new(),
        }
    }

    fn on_event(&mut self, event: Event, now: Instant) {
        match event {
            Event::Snapshot { downloads, totals } => {
                self.history.record(&downloads, &totals, now);
                self.totals = totals;
                self.replace_downloads(downloads);
            }
            Event::Completed { name, .. } => {
                self.push_toast(format!("completed: {name}"), ToastKind::Info, now);
            }
            // A transient failure is retried by the daemon; its row shows the retry.
            Event::Failed {
                transient: true, ..
            } => {}
            Event::Failed { id, reason, .. } => {
                let text = format!("{} failed: {reason}", self.name_of(id));
                self.push_toast(text, ToastKind::Error, now);
            }
            Event::NeedsAttention { id, .. } => {
                let text = format!("{} needs attention", self.name_of(id));
                self.push_toast(text, ToastKind::Error, now);
            }
            Event::Added { .. }
            | Event::Removed { .. }
            | Event::SeedingFinished { .. }
            | Event::ChoosingFiles { .. }
            | Event::Update { .. }
            | Event::Unknown => {}
        }
    }

    fn name_of(&self, id: DownloadId) -> String {
        self.downloads
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| format!("download {id}"), |d| d.name.clone())
    }

    /// Replaces the list, keeping the selection by id; a vanished selection falls back
    /// to the item now at the same visible index (or the last).
    fn replace_downloads(&mut self, downloads: Vec<DownloadSnapshot>) {
        let old_index = self.selected_index();
        self.downloads = downloads;
        // Details of downloads that are gone or no longer running are stale (a
        // finished download's segment strip would stay frozen).
        let live: HashSet<DownloadId> = self
            .downloads
            .iter()
            .filter(|d| Self::fetches_detail(d))
            .map(|d| d.id)
            .collect();
        self.detail_cache.retain(|(id, _)| live.contains(id));
        if self
            .detail
            .as_ref()
            .is_some_and(|(id, _)| !live.contains(id))
        {
            self.detail = None;
        }
        self.fix_selection(old_index);
        // Chosen elsewhere (the CLI) or removed: the picker has nothing left to do.
        if let Mode::Popup(Popup::Picker(p)) = &self.mode
            && !self
                .downloads
                .iter()
                .any(|d| d.id == p.id && Self::choosing(d))
        {
            self.mode = Mode::Normal;
        }
    }

    fn fix_selection(&mut self, fallback: Option<usize>) {
        let ids: Vec<DownloadId> = self.visible().iter().map(|d| d.id).collect();
        self.selected = match self.selected {
            // Everything filtered out: keep the selection for when it reappears.
            Some(id) if ids.is_empty() && self.downloads.iter().any(|d| d.id == id) => Some(id),
            _ if ids.is_empty() => None,
            Some(id) if ids.contains(&id) => Some(id),
            _ => Some(ids[fallback.unwrap_or(0).min(ids.len() - 1)]),
        };
        if self.detail.as_ref().map(|(id, _)| *id) != self.selected {
            self.detail = self
                .selected
                .and_then(|id| self.take_cached_detail(id).map(|d| (id, d)));
        }
        self.list_offset = self.list_offset.min(ids.len().saturating_sub(1));
    }

    fn select_index(&mut self, index: usize) {
        let ids: Vec<DownloadId> = self.visible().iter().map(|d| d.id).collect();
        if let Some(last) = ids.len().checked_sub(1) {
            self.selected = Some(ids[index.min(last)]);
            self.fix_selection(None);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let index = match self.selected_index() {
            Some(i) => i.saturating_add_signed(delta),
            None => 0,
        };
        self.select_index(index);
    }

    /// Runs `edit` on the filter, then keeps the selection visible.
    fn edit_filter(&mut self, edit: impl FnOnce(&mut TextInput)) {
        let old_index = self.selected_index();
        edit(&mut self.filter);
        self.fix_selection(old_index);
    }

    fn on_paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Normal => {
                // Pasting a link on the dashboard starts adding it.
                let form = AddForm::with_source(text.trim());
                if !form.source.is_empty() {
                    self.mode = Mode::Popup(Popup::Add(form));
                }
            }
            Mode::Filter => self.edit_filter(|f| f.paste(text)),
            Mode::Popup(Popup::Add(form)) => form.paste(text),
            Mode::Popup(Popup::Limit(form)) => form.paste(text),
            Mode::Popup(Popup::GlobalLimits(form)) => form.paste(text),
            Mode::Popup(Popup::Picker(p)) => p.paste(text),
            Mode::Popup(Popup::Seed(form)) => form.paste(text),
            Mode::Popup(Popup::ConfirmDelete { .. } | Popup::Help) => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Cmd> {
        if !keys::is_press(&key) {
            return Vec::new();
        }
        if keys::is_ctrl_c(&key) {
            return self.quit();
        }
        // The rest of a paste burst that a popup submit cut short: dropped, so it
        // does not act as dashboard keys.
        if self
            .submitted_at
            .is_some_and(|at| now.saturating_duration_since(at) < PASTE_BURST)
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Vec::new();
        }
        match self.mode {
            Mode::Normal
                if matches!(self.banner, Some(Banner::Incompatible(_)))
                    && key.code == KeyCode::Char('U')
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.banner = Some(Banner::Restarting);
                vec![Cmd::RestartDaemon]
            }
            Mode::Normal => match keys::normal_key(&key) {
                Some(action) => self.on_action(action, now),
                None => Vec::new(),
            },
            Mode::Filter => {
                match key.code {
                    KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Esc => {
                        self.edit_filter(TextInput::clear);
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Down => self.move_selection(1),
                    KeyCode::Up => self.move_selection(-1),
                    _ => self.edit_filter(|f| {
                        f.handle_key(&key);
                    }),
                }
                Vec::new()
            }
            Mode::Popup(_) => self.on_popup_key(key, now),
        }
    }

    /// The hit region (topmost last) under `(x, y)` matching `pred`.
    fn hit_at(&self, x: u16, y: u16, pred: impl Fn(&HitTarget) -> bool) -> Option<HitTarget> {
        let p = Position::new(x, y);
        self.hits
            .iter()
            .rev()
            .find(|(r, t)| r.contains(p) && pred(t))
            .map(|(_, t)| *t)
    }

    /// Mouse input goes through the same paths as keys: hint clicks synthesise the
    /// key, wheel notches are `j`/`k`, a popup click outside is `Esc`.
    fn on_mouse(&mut self, m: MouseEvent, now: Instant) -> Vec<Cmd> {
        const DOUBLE_CLICK: Duration = Duration::from_millis(400);
        let (x, y) = (m.column, m.row);
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let popup = matches!(self.mode, Mode::Popup(_));
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(HitTarget::FooterKey(c)) =
                    self.hit_at(x, y, |t| matches!(t, HitTarget::FooterKey(_)))
                {
                    self.last_click = None;
                    let code = if c == '\u{1b}' {
                        KeyCode::Esc
                    } else {
                        KeyCode::Char(c)
                    };
                    return self.on_key(press(code), now);
                }
                if popup {
                    self.last_click = None;
                    let hit = self.hit_at(x, y, |t| {
                        matches!(
                            t,
                            HitTarget::PopupCheckbox
                                | HitTarget::PopupButton
                                | HitTarget::PickerRow(_)
                                | HitTarget::PickerFold(_)
                        )
                    });
                    let inside = self
                        .hit_at(x, y, |t| *t == HitTarget::PopupInside)
                        .is_some();
                    match (hit, &mut self.mode) {
                        (Some(HitTarget::PickerRow(row)), Mode::Popup(Popup::Picker(p))) => {
                            p.cursor = row;
                            p.toggle_row(row);
                        }
                        (Some(HitTarget::PickerFold(row)), Mode::Popup(Popup::Picker(p))) => {
                            p.toggle_fold(row);
                        }
                        (Some(HitTarget::PopupButton), Mode::Popup(Popup::Picker(p))) => {
                            return p.submit().into_iter().collect();
                        }
                        (Some(HitTarget::PopupCheckbox), _) => self.click_checkbox(),
                        _ if !inside => self.mode = Mode::Normal,
                        _ => {}
                    }
                    return Vec::new();
                }
                let Some(HitTarget::ListRow(id)) =
                    self.hit_at(x, y, |t| matches!(t, HitTarget::ListRow(_)))
                else {
                    self.last_click = None;
                    return Vec::new();
                };
                self.selected = Some(id);
                self.fix_selection(None);
                let double = matches!(self.last_click,
                    Some((last, at)) if last == id && now.saturating_duration_since(at) <= DOUBLE_CLICK);
                if !double {
                    self.last_click = Some((id, now));
                    return Vec::new();
                }
                self.last_click = None;
                let Some(d) = self.selection().cloned() else {
                    return Vec::new();
                };
                if self.can(Action::Pause, &d) {
                    self.on_action(KeyAction::Pause, now)
                } else if self.can(Action::Resume, &d) {
                    self.on_action(KeyAction::Resume, now)
                } else {
                    Vec::new()
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                let step = if m.modifiers.contains(KeyModifiers::SHIFT) {
                    3
                } else {
                    1
                };
                if let Mode::Popup(Popup::Picker(p)) = &mut self.mode {
                    p.scroll(if up { -3 } else { 3 });
                } else if let Mode::Popup(Popup::Help) = self.mode {
                    let s = &mut self.help_scroll;
                    *s = if up {
                        s.saturating_sub(3)
                    } else {
                        s.saturating_add(3)
                    };
                } else if !popup
                    && self
                        .hit_at(x, y, |t| {
                            matches!(t, HitTarget::List | HitTarget::ListRow(_))
                        })
                        .is_some()
                {
                    let action = if up { KeyAction::Up } else { KeyAction::Down };
                    for _ in 0..step {
                        self.on_action(action, now);
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// A click on a popup checkbox: focuses and toggles it.
    fn click_checkbox(&mut self) {
        match &mut self.mode {
            Mode::Popup(Popup::Add(form)) => {
                form.focus = AddField::Paused;
                form.handle_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            }
            Mode::Popup(Popup::ConfirmDelete { delete_files, .. }) => {
                *delete_files = !*delete_files
            }
            _ => {}
        }
    }

    fn quit(&mut self) -> Vec<Cmd> {
        self.should_quit = true;
        vec![Cmd::Quit]
    }

    /// The selection if `action` is valid for it; otherwise an Info toast says why.
    fn target(&mut self, action: Action, now: Instant) -> Option<DownloadSnapshot> {
        // A selection hidden by the filter is not acted on.
        self.selected_index()?;
        let d = self.selection()?.clone();
        if self.can(action, &d) {
            return Some(d);
        }
        let text = match (action, d.queue_position) {
            (Action::ChooseFiles, _) if d.kind != EngineKindTag::Torrent => {
                "only torrents have files to choose".to_string()
            }
            (Action::Seed, _) if d.kind != EngineKindTag::Torrent => {
                "only torrents seed".to_string()
            }
            (Action::Limit, _)
                if d.kind == EngineKindTag::Torrent
                    && !matches!(d.state, WireState::Completed | WireState::Unknown) =>
            {
                TORRENT_LIMIT_TOAST.to_string()
            }
            (Action::MoveUp, Some(_)) if !matches!(d.state, WireState::Unknown) => {
                "already first in the queue".to_string()
            }
            (Action::MoveDown, Some(_)) if !matches!(d.state, WireState::Unknown) => {
                "already last in the queue".to_string()
            }
            _ => format!("can't {} {}", action.verb(), describe(&d.state)),
        };
        self.push_toast(text, ToastKind::Info, now);
        None
    }

    fn on_action(&mut self, action: KeyAction, now: Instant) -> Vec<Cmd> {
        match action {
            KeyAction::Down => self.move_selection(1),
            KeyAction::Up => self.move_selection(-1),
            KeyAction::Top => self.select_index(0),
            KeyAction::Bottom => self.select_index(usize::MAX),
            KeyAction::Add => self.mode = Mode::Popup(Popup::Add(AddForm::default())),
            KeyAction::Pause => {
                if let Some(d) = self.target(Action::Pause, now) {
                    return vec![Cmd::Pause(d.id)];
                }
            }
            KeyAction::Resume => {
                if let Some(d) = self.target(Action::Resume, now) {
                    return vec![Cmd::Resume(d.id)];
                }
            }
            KeyAction::Restart => {
                if let Some(d) = self.target(Action::Restart, now) {
                    return vec![Cmd::Restart(d.id)];
                }
            }
            KeyAction::Delete => {
                if let Some(d) = self.target(Action::Delete, now) {
                    self.mode = Mode::Popup(Popup::ConfirmDelete {
                        id: d.id,
                        name: d.name,
                        delete_files: false,
                    });
                }
            }
            KeyAction::Limit => {
                if let Some(d) = self.target(Action::Limit, now) {
                    self.mode = Mode::Popup(Popup::Limit(LimitForm::new(d.id, d.limit)));
                }
            }
            KeyAction::MoveUp | KeyAction::MoveDown => {
                let up = action == KeyAction::MoveUp;
                let act = if up { Action::MoveUp } else { Action::MoveDown };
                if let Some(d) = self.target(act, now)
                    && let Some(pos) = d.queue_position
                {
                    let position = if up { pos - 1 } else { pos + 1 };
                    return vec![Cmd::Move { id: d.id, position }];
                }
            }
            KeyAction::ChooseFiles => {
                if let Some(d) = self.target(Action::ChooseFiles, now) {
                    self.asked.insert(d.id);
                    self.mode = Mode::Popup(Popup::Picker(Box::new(Picker::new(d.id, &d.name))));
                    return vec![Cmd::FetchFiles(d.id)];
                }
            }
            KeyAction::Seed => {
                if let Some(d) = self.target(Action::Seed, now) {
                    self.mode = Mode::Popup(Popup::Seed(SeedForm::new(&d)));
                }
            }
            KeyAction::GlobalLimits => {
                let (down, up) = self
                    .settings
                    .as_ref()
                    .map_or((None, None), |s| (s.global_down_limit, s.global_up_limit));
                self.mode = Mode::Popup(Popup::GlobalLimits(GlobalLimitsForm::new(down, up)));
            }
            KeyAction::MoreActive | KeyAction::LessActive => {
                let (lo, hi) = MAX_ACTIVE_RANGE;
                if let Some(s) = &mut self.settings {
                    let next = if action == KeyAction::MoreActive {
                        s.max_active.saturating_add(1)
                    } else {
                        s.max_active.saturating_sub(1)
                    }
                    .clamp(lo, hi);
                    if next != s.max_active {
                        s.max_active = next;
                        return vec![Cmd::SetMaxActive(next)];
                    }
                }
            }
            KeyAction::Filter => self.mode = Mode::Filter,
            KeyAction::ClearFilter => self.edit_filter(TextInput::clear),
            KeyAction::Help => {
                self.help_scroll = 0;
                self.mode = Mode::Popup(Popup::Help);
            }
            KeyAction::Quit => return self.quit(),
        }
        Vec::new()
    }

    fn on_popup_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Cmd> {
        let Mode::Popup(popup) = &mut self.mode else {
            return Vec::new();
        };
        // The picker stays open while its selection is sent (a refusal shows in it).
        if let Popup::Picker(p) = popup {
            return match p.handle_key(&key) {
                Some(PickerOutcome::Close) => {
                    self.mode = Mode::Normal;
                    Vec::new()
                }
                Some(PickerOutcome::Submit(cmd)) => vec![cmd],
                None => Vec::new(),
            };
        }
        // Popups without a text input take no Ctrl/Alt combinations (Ctrl+C quit
        // before getting here).
        if matches!(popup, Popup::ConfirmDelete { .. } | Popup::Help)
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return Vec::new();
        }
        let submitted = match popup {
            Popup::Help => {
                // Drawing clamps the scroll to the help's height.
                let s = &mut self.help_scroll;
                match key.code {
                    KeyCode::Enter | KeyCode::Char('?') | KeyCode::Char('q') => {
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Char('j') | KeyCode::Down => *s = s.saturating_add(1),
                    KeyCode::Char('k') | KeyCode::Up => *s = s.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => *s = s.saturating_add(10),
                    KeyCode::PageUp => *s = s.saturating_sub(10),
                    KeyCode::Char('g') | KeyCode::Home => *s = 0,
                    KeyCode::Char('G') | KeyCode::End => *s = u16::MAX,
                    _ => {}
                }
                return Vec::new();
            }
            Popup::ConfirmDelete {
                id, delete_files, ..
            } => match key.code {
                KeyCode::Char('f') | KeyCode::Char(' ') => {
                    *delete_files = !*delete_files;
                    None
                }
                KeyCode::Enter => Some(Cmd::Remove {
                    id: *id,
                    delete_files: *delete_files,
                }),
                KeyCode::Char('n') => {
                    self.mode = Mode::Normal;
                    return Vec::new();
                }
                _ => None,
            },
            Popup::Add(form) => {
                if key.code == KeyCode::Enter {
                    form.submit()
                } else {
                    form.handle_key(&key);
                    None
                }
            }
            Popup::Limit(form) => {
                if key.code == KeyCode::Enter {
                    form.submit()
                } else {
                    form.handle_key(&key);
                    None
                }
            }
            Popup::GlobalLimits(form) => {
                if key.code == KeyCode::Enter {
                    form.submit()
                } else {
                    form.handle_key(&key);
                    None
                }
            }
            Popup::Seed(form) => {
                if key.code == KeyCode::Enter && form.unchanged() {
                    // Nothing to change: an inherited value stays inherited.
                    self.mode = Mode::Normal;
                    return Vec::new();
                }
                if key.code == KeyCode::Enter {
                    form.submit()
                } else {
                    form.handle_key(&key);
                    None
                }
            }
            // Handled above.
            Popup::Picker(_) => None,
        };
        let Some(cmd) = submitted else {
            return Vec::new();
        };
        self.mode = Mode::Normal;
        self.submitted_at = Some(now);
        // Shown at once; the runtime fetches the daemon's settings afterwards, which
        // also undoes this if the command failed.
        match &cmd {
            Cmd::SetGlobalLimits { down, up } => {
                if let Some(s) = &mut self.settings {
                    s.global_down_limit = *down;
                    s.global_up_limit = *up;
                }
            }
            Cmd::SetLimit { id, bps } => {
                if let Some(d) = self.downloads.iter_mut().find(|d| d.id == *id) {
                    d.limit = *bps;
                }
            }
            _ => {}
        }
        vec![cmd]
    }
}

/// Whether `hay`, lowercased, contains the already lowercased `needle` (no allocation).
fn contains_folded(hay: &str, needle: &[char]) -> bool {
    let mut starts = hay.char_indices();
    loop {
        let rest = match starts.next() {
            Some((i, _)) => &hay[i..],
            None => return false,
        };
        let mut folded = rest.chars().flat_map(char::to_lowercase);
        if needle.iter().all(|n| folded.next() == Some(*n)) {
            return true;
        }
    }
}

/// Counts for a freshly connected list (the next snapshot brings the daemon's own).
fn totals_of(downloads: &[DownloadSnapshot], down_bps: u64, up_bps: u64) -> Totals {
    let count = |f: fn(&WireState) -> bool| downloads.iter().filter(|d| f(&d.state)).count();
    Totals {
        down_bps,
        up_bps,
        active: count(|s| matches!(s, WireState::Active | WireState::Starting)),
        queued: count(|s| matches!(s, WireState::Queued)),
        paused: count(|s| matches!(s, WireState::Paused)),
        completed: count(|s| matches!(s, WireState::Completed | WireState::Seeding { .. })),
        failed: count(|s| {
            matches!(
                s,
                WireState::Failed { .. } | WireState::NeedsAttention { .. }
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeChoice;

    fn d(id: DownloadId, state: WireState) -> DownloadSnapshot {
        DownloadSnapshot {
            id,
            kind: EngineKindTag::Http,
            source: format!("http://h/{id}"),
            name: format!("n{id}"),
            path: None,
            state,
            total_bytes: None,
            done_bytes: 0,
            down_bps: 0,
            up_bps: 0,
            eta_secs: None,
            limit: None,
            queue_position: None,
            connections: None,
            peers: None,
            next_retry_unix: None,
            note: None,
            seed: None,
        }
    }

    fn app() -> App {
        App::new(Theme::detect(ThemeChoice::Dark, &|_| None))
    }

    #[test]
    fn can_matrix() {
        let a = app();
        let failed = WireState::Failed {
            reason: String::new(),
            transient: true,
        };
        assert!(a.can(Action::Pause, &d(1, WireState::Starting)));
        assert!(!a.can(Action::Pause, &d(1, WireState::Paused)));
        assert!(a.can(Action::Resume, &d(1, failed.clone())));
        assert!(a.can(Action::Restart, &d(1, failed)));
        assert!(!a.can(Action::Limit, &d(1, WireState::Completed)));
        use puppy_proto::WireAttention;
        let attention = |detail: &str| WireState::NeedsAttention {
            reason: WireAttention::ValidationFailed {
                detail: detail.into(),
            },
        };
        // A failed re-check resumes (downloads again); other reasons only restart.
        let recheck = d(1, attention(puppy_core::types::RECHECK_FAILED));
        assert!(a.can(Action::Resume, &recheck));
        assert!(a.can(Action::Restart, &recheck));
        assert!(!a.can(Action::Resume, &d(1, attention("checksum mismatch"))));
        assert!(!a.can(
            Action::Resume,
            &d(
                1,
                WireState::NeedsAttention {
                    reason: WireAttention::RemoteChanged
                }
            )
        ));
        // A torrent choosing its files takes a limit (applied once it starts).
        let mut choosing = d(1, WireState::ChoosingFiles);
        choosing.kind = EngineKindTag::Torrent;
        assert!(a.can(Action::Limit, &choosing));
        let mut active = d(1, WireState::Active);
        active.kind = EngineKindTag::Torrent;
        assert!(!a.can(Action::Limit, &active));
        let mut unknown_kind = d(1, WireState::Active);
        unknown_kind.kind = EngineKindTag::Unknown;
        assert!(!a.can(Action::Pause, &unknown_kind));
        assert!(a.can(Action::Delete, &unknown_kind));
    }

    #[test]
    fn filter_matching_is_case_insensitive() {
        let needle = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<Vec<_>>();
        assert!(contains_folded("Ubuntu-ISO", &needle("iso")));
        assert!(contains_folded("ÄBC", &needle("äb")));
        assert!(contains_folded("abc", &needle("c")));
        assert!(!contains_folded("abc", &needle("abcd")));
        assert!(!contains_folded("", &needle("a")));
    }

    #[test]
    fn detail_kept_only_for_selection() {
        let mut a = app();
        let now = Instant::now();
        a.update(
            Msg::Daemon(Event::Snapshot {
                downloads: vec![d(1, WireState::Active), d(2, WireState::Active)],
                totals: Totals::default(),
            }),
            now,
        );
        a.update(Msg::Detail(2, Some(EngineDetail::Unknown)), now);
        assert!(a.detail.is_none());
        a.update(Msg::Detail(1, Some(EngineDetail::Unknown)), now);
        assert!(a.detail.is_some());
        a.update(Msg::Key(KeyEvent::from(KeyCode::Down)), now);
        assert_eq!(a.selected, Some(2));
        // Detail 2 arrived earlier: cached, shown at once.
        assert!(matches!(a.detail, Some((2, _))));
    }

    #[test]
    fn paste_in_normal_mode_opens_add() {
        let mut a = app();
        a.update(Msg::Paste("magnet:?xt=1\n".into()), Instant::now());
        match &a.mode {
            Mode::Popup(Popup::Add(f)) => assert_eq!(f.source.value, "magnet:?xt=1"),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn failed_event_toasts_with_name() {
        let mut a = app();
        let now = Instant::now();
        a.update(
            Msg::Daemon(Event::Snapshot {
                downloads: vec![d(1, WireState::Active)],
                totals: Totals::default(),
            }),
            now,
        );
        a.update(
            Msg::Daemon(Event::Failed {
                id: 1,
                reason: "timeout".into(),
                transient: false,
            }),
            now,
        );
        assert_eq!(a.toasts[0].text, "n1 failed: timeout");
        assert_eq!(a.toasts[0].kind, ToastKind::Error);
        // Same failure again while the toast is up: not repeated.
        a.update(
            Msg::Daemon(Event::Failed {
                id: 1,
                reason: "timeout".into(),
                transient: false,
            }),
            now + Duration::from_secs(1),
        );
        assert_eq!(a.toasts.len(), 1);
    }
}
