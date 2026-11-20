//! The dashboard's pure state machine: [`App::update`] folds a [`Msg`] into the state
//! and returns the daemon [`Cmd`]s to run. No terminal I/O, no rendering, no clock:
//! the caller passes `now`.

pub mod forms;
pub mod history;
pub mod input;
pub mod keys;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use puppy_core::types::{AddRequest, DownloadId, EngineDetail};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Event, Settings, Totals, WireState};
use ratatui::layout::{Position, Rect};

pub use forms::{AddField, AddForm, GlobalLimitsForm, LimitForm};
pub use history::History;
pub use input::TextInput;
pub use keys::KeyAction;

use crate::theme::Theme;

/// How long a toast stays up.
pub const TOAST_TTL: Duration = Duration::from_secs(4);
/// Toasts visible at once (the oldest is dropped).
pub const MAX_TOASTS: usize = 3;
pub const MAX_ACTIVE_RANGE: (u16, u16) = (1, 32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
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

#[derive(Clone, Debug, PartialEq)]
pub struct CmdOutcome {
    /// What was attempted (`pause`), for the error toast.
    pub label: String,
    pub result: Result<(), String>,
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
    CmdResult(CmdOutcome),
    Detail(DownloadId, Option<EngineDetail>),
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
    pub detail: Option<(DownloadId, EngineDetail)>,
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
    /// The last left click on a list row, for double-click detection.
    last_click: Option<(DownloadId, Instant)>,
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
            history: History::default(),
            toasts: VecDeque::new(),
            size: (80, 24),
            theme,
            should_quit: false,
            hits: HitMap::new(),
            now_unix: 0,
            ticks: 0,
            help_scroll: 0,
            last_click: None,
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
            Action::Pause => matches!(
                d.state,
                WireState::Queued | WireState::Starting | WireState::Active
            ),
            Action::Resume => matches!(d.state, WireState::Paused | WireState::Failed { .. }),
            Action::Restart => matches!(
                d.state,
                WireState::Failed { .. } | WireState::NeedsAttention { .. }
            ),
            Action::Limit => !matches!(d.state, WireState::Completed),
            Action::MoveUp => d.queue_position.is_some_and(|p| p > 0),
            Action::MoveDown => d.queue_position.is_some_and(|p| p + 1 < self.queue_len()),
        }
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
        match msg {
            Msg::Key(key) => self.on_key(key, now),
            Msg::Mouse(m) => self.on_mouse(m, now),
            Msg::Paste(text) => {
                self.on_paste(&text);
                Vec::new()
            }
            Msg::Resize(w, h) => {
                self.size = (w, h);
                Vec::new()
            }
            Msg::Tick => self.on_tick(now),
            Msg::Daemon(event) => {
                self.on_event(event, now);
                Vec::new()
            }
            Msg::Connected {
                version,
                settings,
                downloads,
            } => {
                self.conn = Conn::Connected;
                self.daemon_version = Some(version);
                self.settings = Some(settings);
                self.totals = totals_of(&downloads, self.totals.down_bps, self.totals.up_bps);
                self.history.prune(&downloads);
                self.replace_downloads(downloads);
                Vec::new()
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
            Msg::CmdResult(outcome) => {
                if let Err(e) = outcome.result {
                    self.push_toast(format!("{}: {e}", outcome.label), ToastKind::Error, now);
                }
                Vec::new()
            }
            Msg::Detail(id, detail) => {
                if self.selected == Some(id) {
                    self.detail = detail.map(|d| (id, d));
                }
                Vec::new()
            }
        }
    }

    fn on_tick(&mut self, now: Instant) -> Vec<Cmd> {
        self.ticks = self.ticks.wrapping_add(1);
        self.toasts.retain(|t| t.until > now);
        match self.selection() {
            Some(d)
                if matches!(d.kind, EngineKindTag::Http | EngineKindTag::Torrent)
                    && matches!(
                        d.state,
                        WireState::Active | WireState::Starting | WireState::Paused
                    ) =>
            {
                vec![Cmd::FetchDetail(d.id)]
            }
            _ => Vec::new(),
        }
    }

    fn on_event(&mut self, event: Event, now: Instant) {
        match event {
            Event::Snapshot { downloads, totals } => {
                self.history.record(&downloads, &totals);
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
            Event::Added { .. } | Event::Removed { .. } | Event::Unknown => {}
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
        self.fix_selection(old_index);
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
            self.detail = None;
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
        match self.mode {
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
            Mode::Popup(_) => self.on_popup_key(key),
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
                    if self
                        .hit_at(x, y, |t| *t == HitTarget::PopupCheckbox)
                        .is_some()
                    {
                        self.click_checkbox();
                    } else if self
                        .hit_at(x, y, |t| *t == HitTarget::PopupInside)
                        .is_none()
                    {
                        self.mode = Mode::Normal;
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
                if let Mode::Popup(Popup::Help) = self.mode {
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

    fn on_popup_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let Mode::Popup(popup) = &mut self.mode else {
            return Vec::new();
        };
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
        };
        match submitted {
            Some(cmd) => {
                self.mode = Mode::Normal;
                vec![cmd]
            }
            None => Vec::new(),
        }
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
        completed: count(|s| matches!(s, WireState::Completed)),
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
        assert!(a.detail.is_none());
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
