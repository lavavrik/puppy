//! Colors: semantic tokens over a dark or light palette, degraded to the 16 ANSI colors
//! without truecolor support and to modifiers only under `NO_COLOR`.
//!
//! Widgets never use colors directly: they ask the [`Theme`] for a [`Token`]'s style.

use ratatui::style::{Color, Modifier, Style};

/// Every color the dashboard uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub info: Color,
    pub bar_done: Color,
    pub bar_todo: Color,
    pub seg_active: Color,
    pub seg_done: Color,
    pub seg_todo: Color,
    pub selection_bg: Color,
    pub border: Color,
    pub border_focus: Color,
}

/// `0xRRGGBB` as a truecolor [`Color`].
const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    /// The dark palette (truecolor).
    pub const DARK: Palette = Palette {
        bg: hex(0x1e1e2e),
        fg: hex(0xcdd6f4),
        dim: hex(0x7f849c),
        accent: hex(0x89b4fa),
        ok: hex(0xa6e3a1),
        warn: hex(0xf9e2af),
        err: hex(0xf38ba8),
        info: hex(0x94e2d5),
        bar_done: hex(0x89b4fa),
        bar_todo: hex(0x313244),
        seg_active: hex(0xf9e2af),
        seg_done: hex(0xa6e3a1),
        seg_todo: hex(0x45475a),
        selection_bg: hex(0x313244),
        border: hex(0x45475a),
        border_focus: hex(0x89b4fa),
    };

    /// The light palette (truecolor).
    pub const LIGHT: Palette = Palette {
        bg: hex(0xeff1f5),
        fg: hex(0x4c4f69),
        dim: hex(0x8c8fa1),
        accent: hex(0x1e66f5),
        ok: hex(0x40a02b),
        warn: hex(0xdf8e1d),
        err: hex(0xd20f39),
        info: hex(0x179299),
        bar_done: hex(0x1e66f5),
        bar_todo: hex(0xccd0da),
        seg_active: hex(0xdf8e1d),
        seg_done: hex(0x40a02b),
        seg_todo: hex(0xbcc0cc),
        selection_bg: hex(0xccd0da),
        border: hex(0xbcc0cc),
        border_focus: hex(0x1e66f5),
    };

    /// No colors at all (`NO_COLOR`): the terminal's defaults everywhere.
    pub const MONO: Palette = Palette {
        bg: Color::Reset,
        fg: Color::Reset,
        dim: Color::Reset,
        accent: Color::Reset,
        ok: Color::Reset,
        warn: Color::Reset,
        err: Color::Reset,
        info: Color::Reset,
        bar_done: Color::Reset,
        bar_todo: Color::Reset,
        seg_active: Color::Reset,
        seg_done: Color::Reset,
        seg_todo: Color::Reset,
        selection_bg: Color::Reset,
        border: Color::Reset,
        border_focus: Color::Reset,
    };

    /// The palette mapped to the 16 ANSI colors (no truecolor support): the terminal's
    /// own foreground and background, gray tracks for the dark or `light` background.
    pub const fn ansi(light: bool) -> Palette {
        let muted = if light { Color::Gray } else { Color::DarkGray };
        Palette {
            bg: Color::Reset,
            fg: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Blue,
            ok: Color::Green,
            warn: Color::Yellow,
            err: Color::Red,
            info: Color::Cyan,
            bar_done: Color::Blue,
            bar_todo: muted,
            seg_active: Color::Yellow,
            seg_done: Color::Green,
            seg_todo: muted,
            selection_bg: muted,
            border: muted,
            border_focus: Color::Blue,
        }
    }
}

/// The configured theme (`theme = "auto" | "dark" | "light"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    Auto,
    Dark,
    Light,
}

impl std::str::FromStr for ThemeChoice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            other => Err(format!(
                "unknown theme {other:?} (expected auto, dark or light)"
            )),
        }
    }
}

/// What a piece of text is; the theme decides how it looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Text,
    Dim,
    Accent,
    Ok,
    Warn,
    Err,
    Info,
    Title,
    Selected,
    Border,
    BorderFocus,
    BarDone,
    BarTodo,
    SegActive,
    SegDone,
    SegTodo,
    /// A key in the footer/help (`p`).
    Key,
    /// What the key does (`pause`).
    KeyLabel,
    /// The app name in the title bar: inverted accent.
    Badge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub p: Palette,
    /// `NO_COLOR`: styles carry modifiers only.
    pub mono: bool,
}

impl Theme {
    /// Picks the palette from the configured `choice` and the environment (`env` looks
    /// a variable up, so tests need not touch the process environment):
    /// - `Auto`: `COLORFGBG`'s background (its last field) 0–6 or 8 ⇒ dark, 7 or 9–15 ⇒
    ///   light; absent or unparseable ⇒ dark.
    /// - `NO_COLOR` set (non-empty) ⇒ monochrome: modifiers only.
    /// - `COLORTERM` other than `truecolor`/`24bit` (and not Windows Terminal, which sets
    ///   `WT_SESSION`) ⇒ the 16 ANSI colors.
    pub fn detect(choice: ThemeChoice, env: &dyn Fn(&str) -> Option<String>) -> Self {
        if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Theme {
                p: Palette::MONO,
                mono: true,
            };
        }
        let light = match choice {
            ThemeChoice::Dark => false,
            ThemeChoice::Light => true,
            ThemeChoice::Auto => env("COLORFGBG").is_some_and(|v| colorfgbg_is_light(&v)),
        };
        // Windows Terminal renders truecolor but does not set COLORTERM.
        let truecolor = env("COLORTERM").is_some_and(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "truecolor" || v == "24bit"
        }) || env("WT_SESSION").is_some_and(|v| !v.is_empty());
        let p = match (truecolor, light) {
            (true, false) => Palette::DARK,
            (true, true) => Palette::LIGHT,
            (false, light) => Palette::ansi(light),
        };
        Theme { p, mono: false }
    }

    /// How `token` looks: palette colors, or modifiers only when monochrome.
    pub fn style(&self, token: Token) -> Style {
        if self.mono {
            return Style::new().add_modifier(mono_modifier(token));
        }
        let p = &self.p;
        let fg = |c: Color| Style::new().fg(c);
        match token {
            Token::Text => fg(p.fg),
            Token::Dim | Token::KeyLabel => fg(p.dim),
            Token::Accent => fg(p.accent),
            Token::Ok => fg(p.ok),
            Token::Warn => fg(p.warn),
            Token::Err => fg(p.err),
            Token::Info => fg(p.info),
            Token::Title | Token::Key => fg(p.accent).add_modifier(Modifier::BOLD),
            Token::Selected => fg(p.fg).bg(p.selection_bg),
            Token::Border => fg(p.border),
            Token::BorderFocus => fg(p.border_focus),
            Token::BarDone => fg(p.bar_done),
            Token::BarTodo => fg(p.bar_todo),
            Token::SegActive => fg(p.seg_active),
            Token::SegDone => fg(p.seg_done),
            Token::SegTodo => fg(p.seg_todo),
            Token::Badge => {
                // ANSI palettes have no concrete background colour to invert with.
                let text = if p.bg == Color::Reset {
                    Color::Black
                } else {
                    p.bg
                };
                Style::new()
                    .fg(text)
                    .bg(p.accent)
                    .add_modifier(Modifier::BOLD)
            }
        }
    }

    /// How `token` looks on a selected row: on the selection background (REVERSED in
    /// mono). A foreground equal to that background (ANSI dark: dim text and the
    /// selection are both DarkGray) is swapped for a visible gray.
    pub fn on_selected(&self, token: Token) -> Style {
        let s = self.style(token);
        if self.mono {
            return s.add_modifier(Modifier::REVERSED);
        }
        let bg = self.p.selection_bg;
        let mut s = s.bg(bg);
        if s.fg == Some(bg) {
            s.fg = Some(contrast(bg));
        }
        s
    }

    /// This theme with bar/segment tracks that stand out from the selection background
    /// (the dark palette's empty-bar colour equals its selection colour), for widgets
    /// drawn on a selected row.
    pub fn for_selection(&self) -> Theme {
        let mut t = *self;
        let sel = t.p.selection_bg;
        let track = if t.p.border != sel {
            t.p.border
        } else {
            contrast(sel)
        };
        if t.p.bar_todo == sel {
            t.p.bar_todo = track;
        }
        if t.p.seg_todo == sel {
            t.p.seg_todo = track;
        }
        t
    }
}

impl Theme {
    /// A gray readable on the ANSI gray `bg` (`Reset` for anything else).
    pub fn contrast(bg: Color) -> Color {
        contrast(bg)
    }
}

/// A gray readable on the ANSI gray `bg`.
fn contrast(bg: Color) -> Color {
    match bg {
        Color::DarkGray => Color::Gray,
        Color::Gray => Color::DarkGray,
        _ => Color::Reset,
    }
}

/// The modifiers standing in for colors in monochrome mode.
fn mono_modifier(token: Token) -> Modifier {
    match token {
        Token::Text | Token::Ok | Token::Info | Token::BarDone | Token::SegDone => {
            Modifier::empty()
        }
        Token::Dim | Token::KeyLabel | Token::Border | Token::BarTodo | Token::SegTodo => {
            Modifier::DIM
        }
        Token::Accent
        | Token::Warn
        | Token::Err
        | Token::Title
        | Token::BorderFocus
        | Token::SegActive
        | Token::Key => Modifier::BOLD,
        Token::Selected => Modifier::REVERSED,
        Token::Badge => Modifier::REVERSED.union(Modifier::BOLD),
    }
}

/// `COLORFGBG` is `fg;bg` (rxvt also sends `fg;default;bg`): the background is the
/// last field. 7 (light gray) and 9–15 (bright colors) are light backgrounds.
fn colorfgbg_is_light(v: &str) -> bool {
    let bg = v.rsplit(';').next().unwrap_or("").trim();
    matches!(bg.parse::<u8>(), Ok(7 | 9..=15))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn detect(choice: ThemeChoice, vars: &[(&str, &str)]) -> Theme {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Theme::detect(choice, &|k| vars.get(k).cloned())
    }

    const TRUE: (&str, &str) = ("COLORTERM", "truecolor");

    #[test]
    fn theme_choice_from_str() {
        assert_eq!("dark".parse(), Ok(ThemeChoice::Dark));
        assert_eq!(" Light ".parse(), Ok(ThemeChoice::Light));
        assert_eq!("auto".parse(), Ok(ThemeChoice::Auto));
        assert!("neon".parse::<ThemeChoice>().is_err());
    }

    #[test]
    fn palettes_are_exact() {
        let d = Palette::DARK;
        assert_eq!(d.bg, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(d.fg, Color::Rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(d.dim, Color::Rgb(0x7f, 0x84, 0x9c));
        assert_eq!(d.accent, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(d.ok, Color::Rgb(0xa6, 0xe3, 0xa1));
        assert_eq!(d.warn, Color::Rgb(0xf9, 0xe2, 0xaf));
        assert_eq!(d.err, Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(d.info, Color::Rgb(0x94, 0xe2, 0xd5));
        assert_eq!(d.bar_done, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(d.bar_todo, Color::Rgb(0x31, 0x32, 0x44));
        assert_eq!(d.seg_active, Color::Rgb(0xf9, 0xe2, 0xaf));
        assert_eq!(d.seg_done, Color::Rgb(0xa6, 0xe3, 0xa1));
        assert_eq!(d.seg_todo, Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(d.selection_bg, Color::Rgb(0x31, 0x32, 0x44));
        assert_eq!(d.border, Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(d.border_focus, Color::Rgb(0x89, 0xb4, 0xfa));

        let l = Palette::LIGHT;
        assert_eq!(l.bg, Color::Rgb(0xef, 0xf1, 0xf5));
        assert_eq!(l.fg, Color::Rgb(0x4c, 0x4f, 0x69));
        assert_eq!(l.dim, Color::Rgb(0x8c, 0x8f, 0xa1));
        assert_eq!(l.accent, Color::Rgb(0x1e, 0x66, 0xf5));
        assert_eq!(l.ok, Color::Rgb(0x40, 0xa0, 0x2b));
        assert_eq!(l.warn, Color::Rgb(0xdf, 0x8e, 0x1d));
        assert_eq!(l.err, Color::Rgb(0xd2, 0x0f, 0x39));
        assert_eq!(l.info, Color::Rgb(0x17, 0x92, 0x99));
        assert_eq!(l.bar_done, Color::Rgb(0x1e, 0x66, 0xf5));
        assert_eq!(l.bar_todo, Color::Rgb(0xcc, 0xd0, 0xda));
        assert_eq!(l.seg_active, Color::Rgb(0xdf, 0x8e, 0x1d));
        assert_eq!(l.seg_done, Color::Rgb(0x40, 0xa0, 0x2b));
        assert_eq!(l.seg_todo, Color::Rgb(0xbc, 0xc0, 0xcc));
        assert_eq!(l.selection_bg, Color::Rgb(0xcc, 0xd0, 0xda));
        assert_eq!(l.border, Color::Rgb(0xbc, 0xc0, 0xcc));
        assert_eq!(l.border_focus, Color::Rgb(0x1e, 0x66, 0xf5));
    }

    #[test]
    fn ansi_mapping() {
        for (light, todo) in [(false, Color::DarkGray), (true, Color::Gray)] {
            let a = Palette::ansi(light);
            assert_eq!(a.accent, Color::Blue);
            assert_eq!(a.ok, Color::Green);
            assert_eq!(a.warn, Color::Yellow);
            assert_eq!(a.err, Color::Red);
            assert_eq!(a.info, Color::Cyan);
            assert_eq!(a.dim, Color::DarkGray);
            assert_eq!(a.fg, Color::Reset);
            assert_eq!(a.bg, Color::Reset);
            assert_eq!(a.bar_done, Color::Blue);
            assert_eq!(a.seg_active, Color::Yellow);
            assert_eq!(a.seg_done, Color::Green);
            assert_eq!(a.border_focus, Color::Blue);
            for c in [a.bar_todo, a.seg_todo, a.selection_bg, a.border] {
                assert_eq!(c, todo, "light: {light}");
            }
        }
    }

    #[test]
    fn theme_detect_rules() {
        // COLORFGBG: background 0–6 or 8 ⇒ dark, 7 or 9–15 ⇒ light.
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "15;0")]).p,
            Palette::DARK
        );
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "0;15")]).p,
            Palette::LIGHT
        );
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "0;7")]).p,
            Palette::LIGHT
        );
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "7;8")]).p,
            Palette::DARK
        );
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "0;6")]).p,
            Palette::DARK
        );
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "0;9")]).p,
            Palette::LIGHT
        );
        // rxvt's three-field form: the background is last.
        assert_eq!(
            detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", "0;default;15")]).p,
            Palette::LIGHT
        );
        // Absent or unparseable ⇒ dark.
        assert_eq!(detect(ThemeChoice::Auto, &[TRUE]).p, Palette::DARK);
        for junk in ["", "default;default", "0;99", "x"] {
            assert_eq!(
                detect(ThemeChoice::Auto, &[TRUE, ("COLORFGBG", junk)]).p,
                Palette::DARK,
                "{junk:?}"
            );
        }
        // An explicit choice wins over COLORFGBG.
        assert_eq!(
            detect(ThemeChoice::Dark, &[TRUE, ("COLORFGBG", "0;15")]).p,
            Palette::DARK
        );
        assert_eq!(
            detect(ThemeChoice::Light, &[TRUE, ("COLORFGBG", "15;0")]).p,
            Palette::LIGHT
        );

        // COLORTERM: truecolor/24bit keep the RGB palette; anything else ⇒ ANSI 16.
        assert_eq!(
            detect(ThemeChoice::Dark, &[("COLORTERM", "24bit")]).p,
            Palette::DARK
        );
        assert_eq!(
            detect(ThemeChoice::Dark, &[("COLORTERM", "TrueColor")]).p,
            Palette::DARK
        );
        assert_eq!(detect(ThemeChoice::Dark, &[]).p, Palette::ansi(false));
        assert_eq!(detect(ThemeChoice::Light, &[]).p, Palette::ansi(true));
        assert_eq!(
            detect(ThemeChoice::Dark, &[("COLORTERM", "yes")]).p,
            Palette::ansi(false)
        );
        assert_eq!(
            detect(ThemeChoice::Auto, &[("COLORFGBG", "0;15")]).p,
            Palette::ansi(true)
        );
        assert!(!detect(ThemeChoice::Dark, &[]).mono);
        // Windows Terminal supports truecolor without setting COLORTERM.
        assert_eq!(
            detect(ThemeChoice::Dark, &[("WT_SESSION", "8f2c")]).p,
            Palette::DARK
        );

        // NO_COLOR (non-empty) ⇒ monochrome, whatever else is set.
        let t = detect(ThemeChoice::Auto, &[TRUE, ("NO_COLOR", "1")]);
        assert!(t.mono);
        assert!(detect(ThemeChoice::Light, &[("NO_COLOR", "yes")]).mono);
        assert!(!detect(ThemeChoice::Dark, &[TRUE, ("NO_COLOR", "")]).mono);
    }

    #[test]
    fn styles_use_the_palette() {
        let t = detect(ThemeChoice::Dark, &[TRUE]);
        let p = Palette::DARK;
        assert_eq!(t.style(Token::Text).fg, Some(p.fg));
        assert_eq!(t.style(Token::Dim).fg, Some(p.dim));
        assert_eq!(t.style(Token::Ok).fg, Some(p.ok));
        assert_eq!(t.style(Token::Warn).fg, Some(p.warn));
        assert_eq!(t.style(Token::Err).fg, Some(p.err));
        assert_eq!(t.style(Token::Info).fg, Some(p.info));
        assert_eq!(t.style(Token::Accent).fg, Some(p.accent));
        assert_eq!(t.style(Token::Border).fg, Some(p.border));
        assert_eq!(t.style(Token::BorderFocus).fg, Some(p.border_focus));
        assert_eq!(t.style(Token::BarDone).fg, Some(p.bar_done));
        assert_eq!(t.style(Token::BarTodo).fg, Some(p.bar_todo));
        assert_eq!(t.style(Token::SegActive).fg, Some(p.seg_active));
        assert_eq!(t.style(Token::SegDone).fg, Some(p.seg_done));
        assert_eq!(t.style(Token::SegTodo).fg, Some(p.seg_todo));
        let title = t.style(Token::Title);
        assert_eq!(title.fg, Some(p.accent));
        assert!(title.add_modifier.contains(Modifier::BOLD));
        let sel = t.style(Token::Selected);
        assert_eq!(sel.bg, Some(p.selection_bg));
        assert_eq!(t.style(Token::Key).fg, Some(p.accent));
        assert!(t.style(Token::Key).add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.style(Token::KeyLabel).fg, Some(p.dim));
    }

    #[test]
    fn selected_styles_stay_readable() {
        for t in [
            detect(ThemeChoice::Dark, &[TRUE]),
            detect(ThemeChoice::Light, &[TRUE]),
            detect(ThemeChoice::Dark, &[]),
            detect(ThemeChoice::Light, &[]),
        ] {
            for tok in [
                Token::Text,
                Token::Dim,
                Token::KeyLabel,
                Token::Ok,
                Token::Err,
            ] {
                let s = t.on_selected(tok);
                assert_eq!(s.bg, Some(t.p.selection_bg));
                assert_ne!(s.fg, s.bg, "{tok:?} invisible in {:?}", t.p);
            }
            let sel = t.for_selection();
            assert_ne!(sel.p.bar_todo, t.p.selection_bg);
            assert_ne!(sel.p.seg_todo, t.p.selection_bg);
        }
        // ANSI dark: dim text on the selection is gray, still dimmer than plain text.
        let ansi = detect(ThemeChoice::Dark, &[]);
        assert_eq!(ansi.on_selected(Token::Dim).fg, Some(Color::Gray));
        let mono = detect(ThemeChoice::Dark, &[("NO_COLOR", "1")]);
        let s = mono.on_selected(Token::Dim);
        assert!(s.add_modifier.contains(Modifier::REVERSED | Modifier::DIM));
    }

    #[test]
    fn mono_styles_are_modifiers_only() {
        let t = detect(ThemeChoice::Dark, &[TRUE, ("NO_COLOR", "1")]);
        let all = [
            Token::Text,
            Token::Dim,
            Token::Accent,
            Token::Ok,
            Token::Warn,
            Token::Err,
            Token::Info,
            Token::Title,
            Token::Selected,
            Token::Border,
            Token::BorderFocus,
            Token::BarDone,
            Token::BarTodo,
            Token::SegActive,
            Token::SegDone,
            Token::SegTodo,
            Token::Key,
            Token::KeyLabel,
        ];
        for tok in all {
            let s = t.style(tok);
            assert_eq!((s.fg, s.bg), (None, None), "{tok:?} has a color");
        }
        assert!(
            t.style(Token::Selected)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(t.style(Token::Title).add_modifier.contains(Modifier::BOLD));
        assert!(t.style(Token::Dim).add_modifier.contains(Modifier::DIM));
        assert!(t.style(Token::Err).add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.style(Token::Text), Style::default());
    }
}
