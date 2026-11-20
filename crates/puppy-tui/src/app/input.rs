//! A single-line text input whose cursor moves by grapheme cluster, so CJK, emoji and
//! combining sequences are edited as the user sees them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

/// Maximum length of an input's value, in bytes.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    pub value: String,
    /// Grapheme index (0 = before the first grapheme, `len` = at the end).
    pub cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// An input holding `value` with the cursor at its end.
    pub fn with_value(value: &str) -> Self {
        let mut t = Self::new();
        t.paste(value);
        t
    }

    /// Number of graphemes in the value.
    pub fn len(&self) -> usize {
        self.value.graphemes(true).count()
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Byte offset of grapheme `idx` (the value's length for `idx >= len`).
    fn byte_at(&self, idx: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .nth(idx)
            .map_or(self.value.len(), |(i, _)| i)
    }

    /// Inserts `s` at the cursor (as much as fits in [`MAX_INPUT_BYTES`]) and moves the
    /// cursor past it.
    pub fn insert_str(&mut self, s: &str) {
        let room = MAX_INPUT_BYTES.saturating_sub(self.value.len());
        let mut take = 0;
        for g in s.graphemes(true) {
            if take + g.len() > room {
                break;
            }
            take += g.len();
        }
        if take == 0 {
            return;
        }
        let at = self.byte_at(self.cursor);
        self.value.insert_str(at, &s[..take]);
        let end = at + take;
        // Count graphemes starting before the end of the insertion: a combining mark
        // merging into the previous grapheme leaves the cursor after that grapheme.
        self.cursor = self
            .value
            .grapheme_indices(true)
            .take_while(|(i, _)| *i < end)
            .count();
    }

    pub fn insert(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    /// Deletes the grapheme before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let (from, to) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
        self.value.replace_range(from..to, "");
        self.cursor -= 1;
    }

    /// Deletes the grapheme under the cursor.
    pub fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let (from, to) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
        self.value.replace_range(from..to, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }

    /// Ctrl+U: clears the whole input.
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// Ctrl+W: deletes the word before the cursor (and the whitespace after it).
    pub fn delete_word(&mut self) {
        let graphemes: Vec<&str> = self.value.graphemes(true).collect();
        let is_space = |g: &str| g.chars().all(char::is_whitespace);
        let mut start = self.cursor.min(graphemes.len());
        while start > 0 && is_space(graphemes[start - 1]) {
            start -= 1;
        }
        while start > 0 && !is_space(graphemes[start - 1]) {
            start -= 1;
        }
        let (from, to) = (self.byte_at(start), self.byte_at(self.cursor));
        self.value.replace_range(from..to, "");
        self.cursor = start;
    }

    /// Inserts the first line of `s` (capped at [`MAX_INPUT_BYTES`] overall).
    pub fn paste(&mut self, s: &str) {
        let line = s.split(['\n', '\r']).next().unwrap_or("");
        self.insert_str(line);
    }

    /// Applies an editing key. Returns whether the key was an editing key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if ctrl => self.clear(),
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char('a') if ctrl => self.home(),
            KeyCode::Char('e') if ctrl => self.end(),
            KeyCode::Char('h') if ctrl => self.backspace(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_input_unicode_editing() {
        let mut t = TextInput::new();
        for c in "a日本".chars() {
            t.insert(c);
        }
        assert_eq!((t.value.as_str(), t.cursor), ("a日本", 3));
        t.left();
        t.insert('語');
        assert_eq!((t.value.as_str(), t.cursor), ("a日語本", 3));
        t.backspace();
        assert_eq!((t.value.as_str(), t.cursor), ("a日本", 2));
        // A combining sequence is one grapheme: e + U+0301.
        t.end();
        t.insert('e');
        t.insert('\u{301}');
        assert_eq!(t.cursor, 4);
        assert_eq!(t.len(), 4);
        t.backspace();
        assert_eq!(t.value, "a日本");
        // Flag emoji (two code points) moves and deletes as one.
        t.insert_str("🇺🇦");
        assert_eq!(t.cursor, 4);
        t.left();
        assert_eq!(t.cursor, 3);
        t.delete();
        assert_eq!((t.value.as_str(), t.cursor), ("a日本", 3));
        t.home();
        t.delete();
        assert_eq!(t.value, "日本");
        t.right();
        t.right();
        t.right();
        assert_eq!(t.cursor, 2);
    }

    #[test]
    fn ctrl_u_and_ctrl_w() {
        let mut t = TextInput::with_value("foo bar  baz");
        t.delete_word();
        assert_eq!(t.value, "foo bar  ");
        t.delete_word();
        assert_eq!(t.value, "foo ");
        t.home();
        t.delete_word();
        assert_eq!(t.value, "foo ");
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(t.handle_key(&ctrl_u));
        assert_eq!((t.value.as_str(), t.cursor), ("", 0));
    }

    #[test]
    fn paste_multiline_takes_first_line() {
        let mut t = TextInput::new();
        t.paste("magnet:?xt=1\r\nsecond line\n");
        assert_eq!(t.value, "magnet:?xt=1");
        assert_eq!(t.cursor, t.len());

        let mut t = TextInput::new();
        t.paste(&"x".repeat(9 * 1024));
        assert_eq!(t.value.len(), MAX_INPUT_BYTES);
        // Full input: further typing is ignored.
        t.insert('y');
        assert_eq!(t.value.len(), MAX_INPUT_BYTES);
        // Multi-byte text is cut at a grapheme boundary.
        let mut t = TextInput::new();
        t.paste(&"日".repeat(3000));
        assert!(t.value.len() <= MAX_INPUT_BYTES);
        assert_eq!(t.value.len() % 3, 0);
    }
}
