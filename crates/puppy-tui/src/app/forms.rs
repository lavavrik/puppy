//! Popup forms: their fields, keyboard editing and submission.
//!
//! Esc (close) and Enter (submit) are routed by [`App`](super::App); each form's
//! `submit` turns its fields into a [`Cmd`] or sets `error` and keeps the popup open.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use puppy_core::types::{AddRequest, DownloadId, FileChoice, RequestContext, SeedPolicy, Source};
use puppy_proto::{DownloadSnapshot, WireState};

use super::Cmd;
use super::input::TextInput;
use crate::format::{parse_rate, rate_input};

pub const ADD_EMPTY_SOURCE: &str = "enter a URL, magnet link or .torrent path";
pub const ADD_BAD_SOURCE: &str = "not a URL, magnet link or .torrent file";
pub const ADD_BAD_DIR: &str = "use an absolute folder path";
pub const ADD_CHECKSUM_NOT_HTTP: &str =
    "a checksum only applies to HTTP(S) and FTP(S) downloads (BitTorrent verifies its pieces)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddField {
    Source,
    Dir,
    Checksum,
    Paused,
    Ok,
}

impl AddField {
    const ORDER: [AddField; 5] = [
        AddField::Source,
        AddField::Dir,
        AddField::Checksum,
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
    /// Optional `ALGO=HEX` (HTTP only); empty ⇒ no check.
    pub checksum: TextInput,
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
            checksum: TextInput::new(),
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
pub fn unquote(s: &str) -> &str {
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

/// Interprets the add form's source: an http(s) or ftp(s) URL (`ftp`, `ftps`, `ftpes`), a
/// `magnet:?` link, or an existing
/// `.torrent` file (made absolute: the daemon has another working directory).
/// Surrounding whitespace and quotes are ignored.
pub fn parse_source(s: &str) -> Result<Source, String> {
    let s = unquote(s);
    let bad = || ADD_BAD_SOURCE.to_string();
    if s.len() >= 8 && s.is_char_boundary(8) && s[..8].eq_ignore_ascii_case("magnet:?") {
        return Ok(Source::Magnet { uri: s.to_string() });
    }
    if let Ok(url) = url::Url::parse(s)
        && (matches!(url.scheme(), "http" | "https") || puppy_core::ftp::is_ftp(&url))
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
            AddField::Checksum => Some(&mut self.checksum),
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
        let checksum = match unquote(&self.checksum.value) {
            "" => None,
            _ if !matches!(source, Source::Http { .. }) => {
                return self.fail(AddField::Checksum, ADD_CHECKSUM_NOT_HTTP.into());
            }
            spec => match spec.parse::<puppy_core::checksum::Checksum>() {
                Ok(c) => Some(c),
                Err(e) => return self.fail(AddField::Checksum, format!("bad checksum: {e}")),
            },
        };
        // The dashboard asks which files of a multi-file torrent to download.
        let files = match source {
            Source::Magnet { .. } | Source::TorrentFile { .. } => FileChoice::Ask,
            _ => FileChoice::All,
        };
        Some(Cmd::Add {
            request: AddRequest {
                source,
                output_dir,
                filename: None,
                context: RequestContext::default(),
                files,
                seed: None,
                checksum,
                mirrors: Vec::new(),
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

pub const SEED_BAD_RATIO: &str = "not a ratio: use a number like 1.5, 0 = off, or leave it empty";
pub const SEED_BAD_TIME: &str = "not a duration: use e.g. 2h, 30m, 0 = off, or leave it empty";

/// A seed ratio as typed and shown: `1.0`, `1.5`, `0.25`.
pub fn ratio_text(r: f32) -> String {
    if r.fract() == 0.0 {
        format!("{r:.1}")
    } else {
        format!("{r}")
    }
}

/// A duration as the seeding popup takes it: `2h`, `1h 30m`, `0`.
pub fn duration_text(secs: u64) -> String {
    if secs == 0 {
        return "0".into();
    }
    humantime::format_duration(std::time::Duration::from_secs(secs)).to_string()
}

/// A typed seed ratio: empty ⇒ `None` (the daemon's default); `0` disables.
pub fn parse_ratio(s: &str) -> Result<Option<f32>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match s.parse::<f32>() {
        Ok(r) if r.is_finite() && r >= 0.0 => Ok(Some(r)),
        _ => Err(SEED_BAD_RATIO.into()),
    }
}

/// A typed seed time (`2h`, `90m`, `1h 30m`): empty ⇒ `None` (the default); `0`
/// disables.
pub fn parse_duration(s: &str) -> Result<Option<u64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if s.chars().all(|c| c == '0') {
        return Ok(Some(0));
    }
    humantime::parse_duration(s)
        .map(|d| Some(d.as_secs()))
        .map_err(|_| SEED_BAD_TIME.into())
}

/// The seeding popup (`s`): ratio and time triggers; Enter replaces the torrent's whole
/// policy (an empty field inherits the daemon's default).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SeedForm {
    pub id: DownloadId,
    pub ratio: TextInput,
    pub time: TextInput,
    /// 0 = ratio, 1 = time.
    pub focus: usize,
    pub error: Option<String>,
    /// The field `error` is about (0 = ratio, 1 = time).
    pub error_field: usize,
    /// The torrent's own policy when the popup opened: Enter without a change sends
    /// nothing (an inherited value stays inherited).
    pub initial: SeedPolicy,
    /// Placeholders of the empty (inherited) fields: `default (1.0)`, `default (2h)`
    /// while seeding (the effective value), else `default`.
    pub ratio_default: String,
    pub time_default: String,
}

/// An empty seeding field's placeholder.
pub const SEED_DEFAULT: &str = "default";

impl SeedForm {
    /// Each field prefilled only from the torrent's own (explicit) value; an inherited
    /// one is empty, its placeholder naming the default in effect when known.
    pub fn new(d: &DownloadSnapshot) -> Self {
        let own = d.seed.clone().unwrap_or_default();
        let (ratio_default, time_default) = match &d.state {
            WireState::Seeding {
                ratio_target,
                time_target_secs,
                ..
            } => (
                format!(
                    "{SEED_DEFAULT} ({})",
                    ratio_target.map_or_else(|| "off".into(), ratio_text)
                ),
                format!(
                    "{SEED_DEFAULT} ({})",
                    time_target_secs.map_or_else(|| "off".into(), duration_text)
                ),
            ),
            _ => (SEED_DEFAULT.into(), SEED_DEFAULT.into()),
        };
        Self {
            id: d.id,
            ratio: TextInput::with_value(&own.ratio.map(ratio_text).unwrap_or_default()),
            time: TextInput::with_value(&own.time_secs.map(duration_text).unwrap_or_default()),
            ratio_default,
            time_default,
            initial: own,
            ..Self::default()
        }
    }

    /// The fields parse to the policy the torrent already has.
    pub fn unchanged(&self) -> bool {
        matches!(
            (parse_ratio(&self.ratio.value), parse_duration(&self.time.value)),
            (Ok(ratio), Ok(time_secs)) if SeedPolicy { ratio, time_secs } == self.initial
        )
    }

    fn focused(&mut self) -> &mut TextInput {
        if self.focus == 0 {
            &mut self.ratio
        } else {
            &mut self.time
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
        let parsed = parse_ratio(&self.ratio.value)
            .map_err(|e| (0, e))
            .and_then(|ratio| {
                parse_duration(&self.time.value)
                    .map(|time| (ratio, time))
                    .map_err(|e| (1, e))
            });
        match parsed {
            Ok((ratio, time_secs)) => Some(Cmd::SetSeed {
                id: self.id,
                policy: SeedPolicy { ratio, time_secs },
            }),
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
        for ftp in ["ftp://x/a", "FTPS://u:p@x:990/a", "ftpes://x/a"] {
            assert!(
                matches!(parse_source(ftp), Ok(Source::Http { .. })),
                "{ftp}"
            );
        }
        assert!(parse_source("gopher://x/a").is_err());
        assert!(parse_source("ftp:///a").is_err());
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

    #[test]
    fn seed_values() {
        assert_eq!(ratio_text(1.0), "1.0");
        assert_eq!(ratio_text(0.25), "0.25");
        assert_eq!(duration_text(7200), "2h");
        assert_eq!(parse_duration(&duration_text(5400)), Ok(Some(5400)));
        assert_eq!(parse_duration(" "), Ok(None));
        assert_eq!(parse_duration("00"), Ok(Some(0)));
        assert!(parse_duration("90").is_err());
        assert_eq!(parse_ratio("2"), Ok(Some(2.0)));
        assert!(parse_ratio("-1").is_err());
        assert!(parse_ratio("inf").is_err());
    }
}
