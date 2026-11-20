//! Popup forms: their fields, keyboard editing and submission.
//!
//! Esc (close) and Enter (submit) are routed by [`App`](super::App); each form's
//! `submit` turns its fields into a [`Cmd`] or sets `error` and keeps the popup open.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};

use super::Cmd;
use super::input::TextInput;
use crate::format::{parse_rate, rate_input};

pub const ADD_EMPTY_SOURCE: &str = "enter a URL, magnet link or .torrent path";
pub const ADD_BAD_SOURCE: &str = "not a URL, magnet link or .torrent file";
pub const ADD_BAD_DIR: &str = "use an absolute folder path";

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
    /// The field `error` is about (drawn under it).
    pub error_on: AddField,
}

impl Default for AddForm {
    fn default() -> Self {
        Self {
            source: TextInput::new(),
            dir: TextInput::new(),
            focus: AddField::Source,
            start_paused: false,
            error: None,
            error_on: AddField::Source,
        }
    }
}

/// Tab/Shift+Tab (and ↓/↑) cycle focus: `Some(forward)` for a focus key.
fn focus_key(key: &KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Tab | KeyCode::Down => Some(true),
        KeyCode::BackTab | KeyCode::Up => Some(false),
        _ => None,
    }
}

/// Strips surrounding whitespace and one pair of matching quotes (paths dragged into a
/// terminal arrive quoted).
fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\'', '`'] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner.trim();
        }
    }
    s
}

/// Expands a leading `~` (alone or followed by a path separator) to the home directory.
pub fn expand_tilde(s: &str) -> PathBuf {
    expand_tilde_with(s, dirs::home_dir().as_deref())
}

fn expand_tilde_with(s: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home
        && let Some(rest) = s.strip_prefix('~')
    {
        if rest.is_empty() {
            return home.to_path_buf();
        }
        if let Some(rest) = rest.strip_prefix(['/', std::path::MAIN_SEPARATOR]) {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Interprets the add form's source: an http(s) URL, a `magnet:?` link, or an existing
/// `.torrent` file (made absolute: the daemon has another working directory).
/// Surrounding whitespace and quotes are ignored.
pub fn parse_source(s: &str) -> Result<Source, String> {
    let s = unquote(s);
    let bad = || ADD_BAD_SOURCE.to_string();
    if s.len() >= 8 && s.is_char_boundary(8) && s[..8].eq_ignore_ascii_case("magnet:?") {
        return Ok(Source::Magnet { uri: s.to_string() });
    }
    if let Ok(url) = url::Url::parse(s)
        && matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|h| !h.is_empty())
    {
        return Ok(Source::Http { url });
    }
    if s.to_lowercase().ends_with(".torrent") {
        let path = expand_tilde(s);
        if path.is_file() {
            let path = std::path::absolute(&path).unwrap_or(path);
            return Ok(Source::TorrentFile { path });
        }
    }
    Err(bad())
}

/// Interprets "Save to": empty ⇒ `None` (the daemon's default); otherwise an absolute
/// folder after `~` expansion.
pub fn parse_dir(s: &str) -> Result<Option<PathBuf>, String> {
    let s = unquote(s);
    if s.is_empty() {
        return Ok(None);
    }
    let path = expand_tilde(s);
    // `has_root`: on Windows `\\data` (current drive) counts; `absolute` adds the drive.
    if path.has_root() {
        Ok(Some(std::path::absolute(&path).unwrap_or(path)))
    } else {
        Err(ADD_BAD_DIR.to_string())
    }
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

    /// Bracketed paste into the focused field.
    pub fn paste(&mut self, text: &str) {
        if let Some(input) = self.focused_input() {
            input.paste(text);
            self.error = None;
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

    fn fail(&mut self, field: AddField, error: String) -> Option<Cmd> {
        self.error = Some(error);
        self.error_on = field;
        self.focus = field;
        None
    }

    /// The add command, or `None` with `error` set on the field at fault (which gets
    /// focus).
    pub fn submit(&mut self) -> Option<Cmd> {
        if unquote(&self.source.value).is_empty() {
            return self.fail(AddField::Source, ADD_EMPTY_SOURCE.into());
        }
        let source = match parse_source(&self.source.value) {
            Ok(s) => s,
            Err(e) => return self.fail(AddField::Source, e),
        };
        let output_dir = match parse_dir(&self.dir.value) {
            Ok(d) => d,
            Err(e) => return self.fail(AddField::Dir, e),
        };
        Some(Cmd::Add {
            request: AddRequest {
                source,
                output_dir,
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
    /// A form for download `id`, prefilled with its current limit (empty for none).
    pub fn new(id: DownloadId, current: Option<u64>) -> Self {
        Self {
            id,
            input: TextInput::with_value(&current.map(rate_input).unwrap_or_default()),
            error: None,
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.input.paste(text);
        self.error = None;
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
    /// The field `error` is about (0 = down, 1 = up).
    pub error_field: usize,
}

impl GlobalLimitsForm {
    /// Prefilled with the daemon's current global limits.
    pub fn new(down: Option<u64>, up: Option<u64>) -> Self {
        let field = |v: Option<u64>| TextInput::with_value(&v.map(rate_input).unwrap_or_default());
        Self {
            down: field(down),
            up: field(up),
            ..Self::default()
        }
    }

    fn focused(&mut self) -> &mut TextInput {
        if self.focus == 0 {
            &mut self.down
        } else {
            &mut self.up
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.focused().paste(text);
        self.error = None;
    }

    pub fn handle_key(&mut self, key: &KeyEvent) {
        if focus_key(key).is_some() {
            self.focus = 1 - self.focus.min(1);
            return;
        }
        if self.focused().handle_key(key) {
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
                self.error_field = field;
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
            Ok(Source::Http { .. })
        ));
        assert!(matches!(
            parse_source("MAGNET:?xt=urn:btih:abc"),
            Ok(Source::Magnet { .. })
        ));
        // A .torrent path that does not exist is not accepted.
        assert!(parse_source("/nonexistent/puppy/a.TORRENT").is_err());
        assert!(parse_source("ftp://x/a").is_err());
        assert!(parse_source("hello").is_err());
        assert!(parse_source("magnet:é").is_err());
    }

    #[test]
    fn tilde_and_quotes() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde_with("~", Some(home)), home);
        assert_eq!(expand_tilde_with("~/a/b", Some(home)), home.join("a/b"));
        assert_eq!(expand_tilde_with("~bob/a", Some(home)), Path::new("~bob/a"));
        assert_eq!(expand_tilde_with("~/a", None), Path::new("~/a"));
        assert_eq!(unquote("  '/a b/c.torrent' "), "/a b/c.torrent");
        assert_eq!(unquote("\"x\""), "x");
        assert_eq!(unquote("'x\""), "'x\"");
        assert_eq!(parse_dir(""), Ok(None));
        assert_eq!(parse_dir("rel"), Err(ADD_BAD_DIR.to_string()));
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
