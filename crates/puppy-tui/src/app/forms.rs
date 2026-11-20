//! Popup forms: their fields, keyboard editing and submission.
//!
//! Esc (close) and Enter (submit) are routed by [`App`](super::App); each form's
//! `submit` turns its fields into a [`Cmd`] or sets `error` and keeps the popup open.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};

use super::Cmd;
use super::input::TextInput;
use crate::format::parse_rate;

pub const ADD_EMPTY_SOURCE: &str = "enter a URL, magnet link or .torrent path";
pub const ADD_BAD_SOURCE: &str = "not an http(s) URL, magnet link or .torrent path";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddField {
    Source,
    Dir,
    Paused,
    Ok,
}

impl AddField {
    const ORDER: [AddField; 4] = [
        AddField::Source,
        AddField::Dir,
        AddField::Paused,
        AddField::Ok,
    ];

    fn step(self, forward: bool) -> Self {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let n = Self::ORDER.len();
        Self::ORDER[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddForm {
    pub source: TextInput,
    /// Output directory; empty ⇒ the daemon's default.
    pub dir: TextInput,
    pub focus: AddField,
    pub start_paused: bool,
    pub error: Option<String>,
}

impl Default for AddForm {
    fn default() -> Self {
        Self {
            source: TextInput::new(),
            dir: TextInput::new(),
            focus: AddField::Source,
            start_paused: false,
            error: None,
        }
    }
}

/// Tab/Shift+Tab (and ↓/↑) cycle focus. Returns whether `key` was used.
fn focus_key(key: &KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Tab | KeyCode::Down => Some(true),
        KeyCode::BackTab | KeyCode::Up => Some(false),
        _ => None,
    }
}

/// Interprets the add form's source text (already trimmed, non-empty).
pub fn parse_source(s: &str) -> Option<Source> {
    if s.len() >= 7 && s[..7].eq_ignore_ascii_case("magnet:") {
        return Some(Source::Magnet { uri: s.to_string() });
    }
    if let Ok(url) = url::Url::parse(s)
        && matches!(url.scheme(), "http" | "https")
    {
        return Some(Source::Http { url });
    }
    if s.to_ascii_lowercase().ends_with(".torrent") {
        return Some(Source::TorrentFile {
            path: PathBuf::from(s),
        });
    }
    None
}

impl AddForm {
    pub fn with_source(source: &str) -> Self {
        Self {
            source: TextInput::with_value(source),
            ..Self::default()
        }
    }

    /// The text input with focus, if any.
    pub fn focused_input(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            AddField::Source => Some(&mut self.source),
            AddField::Dir => Some(&mut self.dir),
            AddField::Paused | AddField::Ok => None,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) {
        if let Some(forward) = focus_key(key) {
            self.focus = self.focus.step(forward);
            return;
        }
        if self.focus == AddField::Paused && key.code == KeyCode::Char(' ') {
            self.start_paused = !self.start_paused;
            return;
        }
        if let Some(input) = self.focused_input()
            && input.handle_key(key)
        {
            self.error = None;
        }
    }

    pub fn submit(&mut self) -> Option<Cmd> {
        let source = self.source.value.trim();
        if source.is_empty() {
            self.error = Some(ADD_EMPTY_SOURCE.into());
            return None;
        }
        let Some(source) = parse_source(source) else {
            self.error = Some(ADD_BAD_SOURCE.into());
            return None;
        };
        let dir = self.dir.value.trim();
        Some(Cmd::Add {
            request: AddRequest {
                source,
                output_dir: (!dir.is_empty()).then(|| PathBuf::from(dir)),
                filename: None,
                context: RequestContext::default(),
            },
            start_paused: self.start_paused,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LimitForm {
    pub id: DownloadId,
    pub input: TextInput,
    pub error: Option<String>,
}

impl LimitForm {
    pub fn new(id: DownloadId) -> Self {
        Self {
            id,
            input: TextInput::new(),
            error: None,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) {
        if self.input.handle_key(key) {
            self.error = None;
        }
    }

    pub fn submit(&mut self) -> Option<Cmd> {
        match parse_rate(&self.input.value) {
            Ok(bps) => Some(Cmd::SetLimit { id: self.id, bps }),
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GlobalLimitsForm {
    pub down: TextInput,
    pub up: TextInput,
    /// 0 = down, 1 = up.
    pub focus: usize,
    pub error: Option<String>,
}

impl GlobalLimitsForm {
    pub fn handle_key(&mut self, key: &KeyEvent) {
        if focus_key(key).is_some() {
            self.focus = 1 - self.focus.min(1);
            return;
        }
        let input = if self.focus == 0 {
            &mut self.down
        } else {
            &mut self.up
        };
        if input.handle_key(key) {
            self.error = None;
        }
    }

    pub fn submit(&mut self) -> Option<Cmd> {
        let parsed = parse_rate(&self.down.value)
            .map_err(|e| (0, e))
            .and_then(|down| {
                parse_rate(&self.up.value)
                    .map(|up| (down, up))
                    .map_err(|e| (1, e))
            });
        match parsed {
            Ok((down, up)) => Some(Cmd::SetGlobalLimits { down, up }),
            Err((field, e)) => {
                self.focus = field;
                self.error = Some(e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_kinds() {
        assert!(matches!(
            parse_source("HTTPS://x.org/a"),
            Some(Source::Http { .. })
        ));
        assert!(matches!(
            parse_source("MAGNET:?xt=urn:btih:abc"),
            Some(Source::Magnet { .. })
        ));
        assert!(matches!(
            parse_source("/home/me/a.TORRENT"),
            Some(Source::TorrentFile { .. })
        ));
        assert_eq!(parse_source("ftp://x/a"), None);
        assert_eq!(parse_source("hello"), None);
    }

    #[test]
    fn add_focus_cycles_and_space_toggles_paused() {
        let mut f = AddForm::default();
        let k = |c| KeyEvent::from(c);
        f.handle_key(&k(KeyCode::BackTab));
        assert_eq!(f.focus, AddField::Ok);
        f.handle_key(&k(KeyCode::Tab));
        assert_eq!(f.focus, AddField::Source);
        f.handle_key(&k(KeyCode::Char(' ')));
        assert_eq!(f.source.value, " ");
        assert!(!f.start_paused);
        f.focus = AddField::Paused;
        f.handle_key(&k(KeyCode::Char(' ')));
        assert!(f.start_paused);
        f.source = TextInput::with_value("nonsense");
        assert_eq!(f.submit(), None);
        assert_eq!(f.error.as_deref(), Some(ADD_BAD_SOURCE));
    }
}
