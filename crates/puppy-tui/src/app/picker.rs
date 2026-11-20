//! The torrent file picker's state: a folder tree over the torrent's files with a
//! checkbox per file (and a tri-state one per folder), folding, a filter and the cursor.
//!
//! Esc (close) is routed here too: while typing a filter it clears the filter instead.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use puppy_core::types::{DownloadId, Selection, TorrentFile};

use super::Cmd;
use super::input::TextInput;

/// Enter with nothing selected.
pub const PICK_NONE: &str = "select at least one file";
/// Added to a refusal because a file of the torrent is already on disk: the layout was
/// fixed when the torrent started waiting.
pub const EXISTS_HINT: &str = "remove the torrent and add it again to choose differently";
/// Rows a page key moves before the picker was first drawn.
const DEFAULT_PAGE: usize = 10;

/// A row of the tree: a folder or a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// The last path component.
    pub name: String,
    /// Nesting below the top level (0).
    pub depth: usize,
    /// Index in [`Picker::nodes`] past this node's subtree (a file: its own + 1).
    pub end: usize,
    /// A file node: its position in [`Picker::files`].
    pub file: Option<usize>,
    /// A file's size, or the total of a folder's files.
    pub size: u64,
}

impl Node {
    pub fn is_folder(&self) -> bool {
        self.file.is_none()
    }
}

/// A folder's checkbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    All,
    None,
    /// Some of its files.
    Some,
}

/// What a key did that the app must act on (short-lived, like [`Cmd`]).
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum PickerOutcome {
    Close,
    Submit(Cmd),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Picker {
    pub id: DownloadId,
    /// The torrent's name (the title).
    pub name: String,
    /// Waiting for the file list.
    pub loading: bool,
    /// As the daemon listed them.
    pub files: Vec<TorrentFile>,
    /// The tree, depth first: folders before files, each in the torrent's order.
    pub nodes: Vec<Node>,
    /// Per file (same order as `files`).
    pub checked: Vec<bool>,
    /// Folded folders (node indices).
    pub collapsed: HashSet<usize>,
    /// Case-insensitive substring on the file's path.
    pub filter: TextInput,
    /// Typing into the filter.
    pub filtering: bool,
    /// Index in [`rows`](Self::rows).
    pub cursor: usize,
    /// First visible row (maintained by drawing).
    pub offset: usize,
    /// Rows shown at once (set by drawing; what PageUp/PageDown move).
    pub page: usize,
    /// The selection was sent; waiting for the daemon (toggles are ignored).
    pub submitting: bool,
    /// The hoisted root folder (`Show/`): the filter matches paths below it.
    pub root: String,
    pub error: Option<String>,
}

/// A folder while building the tree.
#[derive(Default)]
struct Dir {
    name: String,
    dirs: Vec<Dir>,
    files: Vec<usize>,
}

impl Dir {
    fn insert(&mut self, comps: &[&str], file: usize) {
        match comps {
            [] | [_] => self.files.push(file),
            [first, rest @ ..] => {
                let k = match self.dirs.iter().position(|d| d.name == *first) {
                    Some(k) => k,
                    None => {
                        self.dirs.push(Dir {
                            name: (*first).to_string(),
                            ..Dir::default()
                        });
                        self.dirs.len() - 1
                    }
                };
                self.dirs[k].insert(rest, file);
            }
        }
    }

    /// Appends this folder's contents at `depth`; returns their total size.
    fn flatten(&self, depth: usize, files: &[TorrentFile], nodes: &mut Vec<Node>) -> u64 {
        let mut total = 0;
        for d in &self.dirs {
            let at = nodes.len();
            nodes.push(Node {
                name: d.name.clone(),
                depth,
                end: 0,
                file: None,
                size: 0,
            });
            let size = d.flatten(depth + 1, files, nodes);
            nodes[at].end = nodes.len();
            nodes[at].size = size;
            total += size;
        }
        for &f in &self.files {
            let name = files[f].path.rsplit('/').next().unwrap_or_default();
            nodes.push(Node {
                name: name.to_string(),
                depth,
                end: nodes.len() + 1,
                file: Some(f),
                size: files[f].size,
            });
            total += files[f].size;
        }
        total
    }
}

fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|c| !c.is_empty()).collect()
}

/// Whether `hay`, lowercased, contains the lowercased `needle`.
fn contains_folded(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

impl Picker {
    /// A picker for `id` waiting for its file list.
    pub fn new(id: DownloadId, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            loading: true,
            files: Vec::new(),
            nodes: Vec::new(),
            checked: Vec::new(),
            collapsed: HashSet::new(),
            filter: TextInput::new(),
            filtering: false,
            cursor: 0,
            offset: 0,
            page: DEFAULT_PAGE,
            submitting: false,
            error: None,
            root: String::new(),
        }
    }

    pub fn with_files(id: DownloadId, name: &str, files: Vec<TorrentFile>) -> Self {
        let mut p = Self::new(id, name);
        p.set_files(files);
        p
    }

    /// The file list arrived: builds the tree, everything selected.
    pub fn set_files(&mut self, files: Vec<TorrentFile>) {
        let mut root = Dir::default();
        for (k, f) in files.iter().enumerate() {
            root.insert(&components(&f.path), k);
        }
        // The torrent's own folder is the title already: its contents are the top level.
        self.root.clear();
        if root.files.is_empty() && root.dirs.len() == 1 && root.dirs[0].name == self.name {
            root = root.dirs.pop().unwrap_or_default();
            self.root = format!("{}/", root.name);
        }
        let mut nodes = Vec::with_capacity(files.len() + files.len() / 4);
        root.flatten(0, &files, &mut nodes);
        self.checked = vec![true; files.len()];
        self.files = files;
        self.nodes = nodes;
        self.collapsed.clear();
        self.cursor = 0;
        self.offset = 0;
        self.loading = false;
    }

    /// The filter, lowercased and trimmed (empty: no filter).
    fn needle(&self) -> String {
        self.filter.value.trim().to_lowercase()
    }

    /// Per file: whether it matches the filter.
    pub fn matches(&self) -> Vec<bool> {
        let needle = self.needle();
        if needle.is_empty() {
            return vec![true; self.files.len()];
        }
        self.files
            .iter()
            .map(|f| {
                let path = f.path.trim_start_matches('/');
                contains_folded(path.strip_prefix(&self.root).unwrap_or(path), &needle)
            })
            .collect()
    }

    /// The visible rows (node indices): folded folders hide their contents; a filter
    /// shows the matching files with their folders (unfolded).
    pub fn rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        if self.needle().is_empty() {
            let mut i = 0;
            while i < self.nodes.len() {
                rows.push(i);
                let n = &self.nodes[i];
                i = if n.is_folder() && self.collapsed.contains(&i) {
                    n.end
                } else {
                    i + 1
                };
            }
            return rows;
        }
        let matching = self.matches();
        // A folder is shown when a file below it matches (walked backwards: a folder's
        // subtree comes after it).
        let mut shown = vec![false; self.nodes.len()];
        for i in (0..self.nodes.len()).rev() {
            let n = &self.nodes[i];
            shown[i] = match n.file {
                Some(f) => matching[f],
                None => {
                    // Children: the direct ones, each followed by its subtree.
                    let mut j = i + 1;
                    let mut any = false;
                    while j < n.end {
                        any |= shown[j];
                        j = self.nodes[j].end;
                    }
                    any
                }
            };
        }
        rows.extend((0..self.nodes.len()).filter(|&i| shown[i]));
        rows
    }

    /// The files (positions in `files`) below node `i` that the filter shows.
    fn files_under(&self, i: usize, matching: &[bool]) -> Vec<usize> {
        let end = self.nodes.get(i).map_or(i, |n| n.end);
        self.nodes[i.min(self.nodes.len())..end]
            .iter()
            .filter_map(|n| n.file)
            .filter(|&f| matching[f])
            .collect()
    }

    /// Node `i`'s checkbox (a folder: over the files the filter shows).
    pub fn check_of(&self, i: usize) -> Check {
        self.check_with(i, &self.matches())
    }

    /// [`check_of`](Self::check_of) with the filter's matches computed once.
    pub fn check_with(&self, i: usize, matching: &[bool]) -> Check {
        let (mut on, mut off) = (false, false);
        let end = self.nodes.get(i).map_or(i, |n| n.end);
        for n in &self.nodes[i.min(self.nodes.len())..end] {
            if let Some(f) = n.file
                && matching[f]
            {
                if self.checked[f] {
                    on = true;
                } else {
                    off = true;
                }
                if on && off {
                    return Check::Some;
                }
            }
        }
        if on { Check::All } else { Check::None }
    }

    /// `(selected files, files, selected bytes, bytes)`.
    pub fn totals(&self) -> (usize, usize, u64, u64) {
        let mut n = 0;
        let mut bytes = 0;
        let mut total = 0;
        for (f, &on) in self.files.iter().zip(&self.checked) {
            total += f.size;
            if on {
                n += 1;
                bytes += f.size;
            }
        }
        (n, self.files.len(), bytes, total)
    }

    /// The node under the cursor.
    pub fn current(&self) -> Option<usize> {
        self.rows().get(self.cursor).copied()
    }

    /// Runs `change` (which may change the visible rows), keeping the cursor on the
    /// same node, or the nearest one after it.
    fn keep_cursor(&mut self, change: impl FnOnce(&mut Self)) {
        let node = self.current();
        change(self);
        let rows = self.rows();
        self.cursor = match node {
            Some(n) => rows
                .iter()
                .position(|&r| r >= n)
                .unwrap_or(rows.len().saturating_sub(1)),
            None => 0,
        };
    }

    /// Mouse wheel: moves the cursor `delta` rows.
    pub fn scroll(&mut self, delta: isize) {
        self.move_cursor(delta);
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.rows().len();
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(n.saturating_sub(1));
    }

    /// Toggles row `row`: a file flips; a folder selects all of its (shown) files
    /// unless all are selected already, which clears them.
    pub fn toggle_row(&mut self, row: usize) {
        if self.submitting {
            return;
        }
        let Some(&i) = self.rows().get(row) else {
            return;
        };
        let matching = self.matches();
        let on = self.check_with(i, &matching) != Check::All;
        for f in self.files_under(i, &matching) {
            self.checked[f] = on;
        }
        self.error = None;
    }

    /// Selects (`on`) or clears every file the filter shows.
    pub fn set_all(&mut self, on: bool) {
        if self.submitting {
            return;
        }
        let matching = self.matches();
        for (c, m) in self.checked.iter_mut().zip(matching) {
            if m {
                *c = on;
            }
        }
        self.error = None;
    }

    /// Folds or unfolds the folder at row `row` (a file: nothing).
    pub fn toggle_fold(&mut self, row: usize) {
        let Some(&i) = self.rows().get(row) else {
            return;
        };
        if !self.nodes[i].is_folder() || !self.needle().is_empty() {
            return;
        }
        self.cursor = row;
        if !self.collapsed.remove(&i) {
            self.collapsed.insert(i);
        }
    }

    /// ←: folds an open folder, else goes to the parent folder.
    fn left(&mut self) {
        let Some(i) = self.current() else { return };
        let n = &self.nodes[i];
        if n.is_folder() && !self.collapsed.contains(&i) && self.needle().is_empty() {
            self.collapsed.insert(i);
            return;
        }
        if n.depth == 0 {
            return;
        }
        let depth = n.depth - 1;
        if let Some(parent) = (0..i).rev().find(|&j| self.nodes[j].depth == depth)
            && let Some(row) = self.rows().iter().position(|&r| r == parent)
        {
            self.cursor = row;
        }
    }

    /// →: unfolds a folded folder, else goes to an open folder's first entry.
    fn right(&mut self) {
        let Some(i) = self.current() else { return };
        if !self.nodes[i].is_folder() {
            return;
        }
        if self.collapsed.remove(&i) {
            return;
        }
        if self.nodes[i].end > i + 1 {
            self.move_cursor(1);
        }
    }

    /// The selection command, or `None` with `error` set (nothing selected) or while
    /// waiting for the daemon.
    pub fn submit(&mut self) -> Option<Cmd> {
        if self.submitting || self.loading {
            return None;
        }
        let indices: Vec<usize> = self
            .files
            .iter()
            .zip(&self.checked)
            .filter(|(_, on)| **on)
            .map(|(f, _)| f.index)
            .collect();
        if indices.is_empty() {
            self.error = Some(PICK_NONE.to_string());
            return None;
        }
        self.error = None;
        self.submitting = true;
        Some(Cmd::SelectFiles {
            id: self.id,
            selection: Selection {
                indices,
                globs: Vec::new(),
            },
        })
    }

    /// The daemon refused the selection: shown inline, with the way out when a file of
    /// the torrent is in the way.
    pub fn refused(&mut self, error: &str) {
        self.submitting = false;
        self.error = Some(if error.contains("already exists") {
            format!("{error} — {EXISTS_HINT}")
        } else {
            error.to_string()
        });
    }

    pub fn paste(&mut self, text: &str) {
        if self.filtering {
            self.keep_cursor(|p| p.filter.paste(text));
        }
    }

    /// Handles a key (Esc included).
    pub fn handle_key(&mut self, key: &KeyEvent) -> Option<PickerOutcome> {
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.keep_cursor(|p| p.filter.clear());
                    self.filtering = false;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Down => self.move_cursor(1),
                KeyCode::Up => self.move_cursor(-1),
                _ => self.keep_cursor(|p| {
                    p.filter.handle_key(key);
                }),
            }
            return None;
        }
        if key.code == KeyCode::Esc {
            return Some(PickerOutcome::Close);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let page = self.page.max(1) as isize;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX),
            KeyCode::Char(' ') => self.toggle_row(self.cursor),
            KeyCode::Char('a') => self.set_all(true),
            KeyCode::Char('n') => self.set_all(false),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Left | KeyCode::Char('h') => self.left(),
            KeyCode::Right | KeyCode::Char('l') => self.right(),
            KeyCode::Enter | KeyCode::Char('y') => return self.submit().map(PickerOutcome::Submit),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(index: usize, path: &str) -> TorrentFile {
        TorrentFile {
            index,
            path: path.into(),
            size: 1,
        }
    }

    #[test]
    fn tree_ranges_and_parents() {
        let mut p = Picker::with_files(
            1,
            "t",
            vec![f(0, "a/b/c"), f(1, "a/d"), f(2, "e"), f(3, "a/b/f")],
        );
        let names: Vec<&str> = p.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "f", "d", "e"]);
        assert_eq!(p.nodes[0].end, 5);
        assert_eq!(p.nodes[1].end, 4);
        assert_eq!(p.nodes[0].size, 3);
        // ← on `f` goes to `b`, again folds `b`, again goes to `a`.
        p.cursor = 3;
        let left = KeyEvent::from(KeyCode::Left);
        p.handle_key(&left);
        assert_eq!(p.cursor, 1);
        p.handle_key(&left);
        assert!(p.collapsed.contains(&1));
        p.handle_key(&left);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.rows(), [0, 1, 4, 5]);
    }
}
