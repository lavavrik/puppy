//! The dashboard's pure state machine: [`App::update`] folds a [`Msg`] into the state
//! and returns the daemon [`Cmd`]s to run. No terminal I/O, no rendering, no clock:
//! the caller passes `now`.

pub mod forms;
pub mod history;
pub mod input;
pub mod keys;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use puppy_core::types::{AddRequest, DownloadId, EngineDetail};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Event, Settings, Totals, WireState};
use ratatui::layout::Rect;

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
        }
    }

    /// Downloads matching the filter, in daemon order.
    pub fn visible(&self) -> Vec<&DownloadSnapshot> {
        let needle = self.filter.value.trim().to_lowercase();
        self.downloads
            .iter()
            .filter(|d| {
                needle.is_empty()
                    || d.name.to_lowercase().contains(&needle)
                    || d.source.to_lowercase().contains(&needle)
            })
            .collect()
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

    pub fn push_toast(&mut self, text: impl Into<String>, kind: ToastKind, now: Instant) {
        self.toasts.push_back(Toast {
            text: text.into(),
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
            // Mouse handling arrives with the hit map (drawing).
            Msg::Mouse(_) => Vec::new(),
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
            Event::Failed { id, reason } => {
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
            Mode::Popup(Popup::Add(form)) => {
                if let Some(input) = form.focused_input() {
                    input.paste(text);
                    form.error = None;
                }
            }
            Mode::Popup(Popup::Limit(form)) => form.input.paste(text),
            Mode::Popup(Popup::GlobalLimits(form)) => {
                if form.focus == 0 {
                    form.down.paste(text);
                } else {
                    form.up.paste(text);
                }
            }
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

    fn quit(&mut self) -> Vec<Cmd> {
        self.should_quit = true;
        vec![Cmd::Quit]
    }

    /// The selection if `action` is valid for it; otherwise an Info toast says why.
    fn target(&mut self, action: Action, now: Instant) -> Option<DownloadSnapshot> {
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
                    self.mode = Mode::Popup(Popup::Limit(LimitForm::new(d.id)));
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
                self.mode = Mode::Popup(Popup::GlobalLimits(GlobalLimitsForm::default()));
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
            KeyAction::Help => self.mode = Mode::Popup(Popup::Help),
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
                if matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.mode = Mode::Normal;
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
                KeyCode::Enter | KeyCode::Char('y') => Some(Cmd::Remove {
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
            }),
            now,
        );
        assert_eq!(a.toasts[0].text, "n1 failed: timeout");
        assert_eq!(a.toasts[0].kind, ToastKind::Error);
    }
}
