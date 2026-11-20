//! The normal-mode keymap (global constraints, exact).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What a normal-mode key does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Down,
    Up,
    Top,
    Bottom,
    Add,
    Pause,
    Resume,
    Delete,
    Restart,
    Limit,
    GlobalLimits,
    MoreActive,
    LessActive,
    MoveUp,
    MoveDown,
    Filter,
    /// Esc: clear the filter.
    ClearFilter,
    Help,
    Quit,
}

impl KeyAction {
    /// The primary character for this action: what the footer shows and what a click on
    /// a footer hint ([`HitTarget::FooterKey`](super::HitTarget::FooterKey)) maps to.
    pub fn primary_char(self) -> char {
        match self {
            KeyAction::Down => 'j',
            KeyAction::Up => 'k',
            KeyAction::Top => 'g',
            KeyAction::Bottom => 'G',
            KeyAction::Add => 'a',
            KeyAction::Pause => 'p',
            KeyAction::Resume => 'r',
            KeyAction::Delete => 'd',
            KeyAction::Restart => 'R',
            KeyAction::Limit => 'l',
            KeyAction::GlobalLimits => 'L',
            KeyAction::MoreActive => '+',
            KeyAction::LessActive => '-',
            KeyAction::MoveUp => 'K',
            KeyAction::MoveDown => 'J',
            KeyAction::Filter => '/',
            KeyAction::ClearFilter => '\u{1b}',
            KeyAction::Help => '?',
            KeyAction::Quit => 'q',
        }
    }

    /// Inverse of [`primary_char`](Self::primary_char) (and of the other char keys).
    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            'j' => KeyAction::Down,
            'k' => KeyAction::Up,
            'g' => KeyAction::Top,
            'G' => KeyAction::Bottom,
            'a' => KeyAction::Add,
            'p' => KeyAction::Pause,
            'r' => KeyAction::Resume,
            'd' => KeyAction::Delete,
            'R' => KeyAction::Restart,
            'l' => KeyAction::Limit,
            'L' => KeyAction::GlobalLimits,
            '+' => KeyAction::MoreActive,
            '-' => KeyAction::LessActive,
            'K' => KeyAction::MoveUp,
            'J' => KeyAction::MoveDown,
            '/' => KeyAction::Filter,
            '\u{1b}' => KeyAction::ClearFilter,
            '?' => KeyAction::Help,
            'q' => KeyAction::Quit,
            _ => return None,
        })
    }
}

/// Key releases (reported on Windows) are not actions.
pub fn is_press(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
}

/// `Ctrl+C`: quits from every mode.
pub fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
}

/// Maps a normal-mode key. Character keys ignore Shift (terminals differ in whether
/// they report it for `G`, `+`, `?`); Ctrl/Alt combinations are not actions.
pub fn normal_key(key: &KeyEvent) -> Option<KeyAction> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // Ctrl+Alt is AltGr on Windows: the char it produced counts as typed.
    let altgr = key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::ALT);
    if !altgr
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Down if shift => Some(KeyAction::MoveDown),
        KeyCode::Up if shift => Some(KeyAction::MoveUp),
        KeyCode::Down => Some(KeyAction::Down),
        KeyCode::Up => Some(KeyAction::Up),
        KeyCode::Home => Some(KeyAction::Top),
        KeyCode::End => Some(KeyAction::Bottom),
        KeyCode::Delete => Some(KeyAction::Delete),
        KeyCode::Esc => Some(KeyAction::ClearFilter),
        KeyCode::Char(c) => KeyAction::from_char(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap() {
        let k = |code, m| normal_key(&KeyEvent::new(code, m));
        let none = KeyModifiers::NONE;
        assert_eq!(
            k(KeyCode::Char('G'), KeyModifiers::SHIFT),
            Some(KeyAction::Bottom)
        );
        assert_eq!(k(KeyCode::Up, KeyModifiers::SHIFT), Some(KeyAction::MoveUp));
        assert_eq!(k(KeyCode::Up, none), Some(KeyAction::Up));
        assert_eq!(k(KeyCode::Delete, none), Some(KeyAction::Delete));
        assert_eq!(k(KeyCode::Char('p'), KeyModifiers::CONTROL), None);
        assert_eq!(k(KeyCode::Char('p'), KeyModifiers::ALT), None);
        assert_eq!(
            k(
                KeyCode::Char('/'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            Some(KeyAction::Filter)
        );
        assert_eq!(k(KeyCode::Char('x'), none), None);
        for a in [
            KeyAction::Down,
            KeyAction::Restart,
            KeyAction::MoveDown,
            KeyAction::ClearFilter,
            KeyAction::Quit,
        ] {
            assert_eq!(KeyAction::from_char(a.primary_char()), Some(a));
        }
        let mut release = KeyEvent::new(KeyCode::Char('q'), none);
        release.kind = KeyEventKind::Release;
        assert!(!is_press(&release));
    }
}
