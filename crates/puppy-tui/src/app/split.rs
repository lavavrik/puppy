//! Where the divider between the download list and the detail pane sits: the default
//! split, or where the user dragged it (or moved it with `<` / `>`), kept per layout as
//! a share of the body so it scales when the terminal is resized.

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

/// Share of the body width the list gets in the wide layout by default.
const LIST_PERCENT: u32 = 58;
/// Narrow layout by default: the detail pane gets this share of the body height,
/// within `DETAIL_MIN..=DETAIL_MAX` rows.
const DETAIL_PERCENT: u32 = 45;
const DETAIL_MAX: u16 = 18;
/// Narrow layout: the list keeps at least this many rows; the detail pane needs at
/// least `DETAIL_MIN` rows (else it is omitted).
pub const LIST_MIN: u16 = 8;
pub const DETAIL_MIN: u16 = 7;
/// Wide layout: the narrowest the list and the detail pane may be dragged.
pub const LIST_MIN_W: u16 = 40;
pub const DETAIL_MIN_W: u16 = 30;
/// Shares are stored in units of `1 / SCALE` of the body.
const SCALE: u32 = 10_000;

/// The body holding the list and the detail pane, as last drawn (set by drawing when
/// the detail pane shows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Panes {
    pub body: Rect,
    /// Side by side (the divider is vertical) rather than stacked.
    pub wide: bool,
}

impl Panes {
    /// The body's extent along the split (its width when wide, else its height).
    fn len(&self) -> u16 {
        if self.wide {
            self.body.width
        } else {
            self.body.height
        }
    }

    /// Where the body starts along the split.
    fn start(&self) -> u16 {
        if self.wide { self.body.x } else { self.body.y }
    }

    /// The coordinate of `(x, y)` along the split.
    pub fn along(&self, x: u16, y: u16) -> u16 {
        if self.wide { x } else { y }
    }

    /// The region a press on starts a drag: the two border lines that meet at the
    /// divider (the list's last column or row and the detail pane's first).
    pub fn divider(&self, split: &Split) -> Rect {
        let at = self.start() + split.list_len(*self);
        let b = self.body;
        if self.wide {
            Rect::new(at.saturating_sub(1), b.y, 2, b.height)
        } else {
            Rect::new(b.x, at.saturating_sub(1), b.width, 2)
        }
    }
}

/// Saved between runs ([`crate::prefs`]): each share is in ten-thousandths of the body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Split {
    /// Wide layout: the list's share of the body width (`None`: the default).
    #[serde(skip_serializing_if = "Option::is_none")]
    wide: Option<u16>,
    /// Narrow layout: the list's share of the body height (`None`: the default).
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow: Option<u16>,
}

impl Split {
    /// Without shares out of range (a hand-edited or damaged file).
    pub fn sanitized(self) -> Self {
        let ok = |s: Option<u16>| s.filter(|&s| u32::from(s) <= SCALE);
        Self {
            wide: ok(self.wide),
            narrow: ok(self.narrow),
        }
    }

    /// Wide layout: the list's width in a body `w` columns wide; the detail pane gets
    /// the rest.
    pub fn list_width(&self, w: u16) -> u16 {
        let lw = match self.wide {
            Some(s) => of(w, s),
            None => (u32::from(w) * LIST_PERCENT / 100) as u16,
        };
        clamp_width(lw, w)
    }

    /// Narrow layout: the detail pane's height in a body `h` rows high, or `None` when
    /// it does not fit; the list gets the rest.
    pub fn detail_height(&self, h: u16) -> Option<u16> {
        let max = h.saturating_sub(LIST_MIN);
        if max < DETAIL_MIN {
            return None;
        }
        let dh = match self.narrow {
            Some(s) => h.saturating_sub(of(h, s)),
            None => ((u32::from(h) * DETAIL_PERCENT / 100) as u16).clamp(DETAIL_MIN, DETAIL_MAX),
        };
        Some(dh.clamp(DETAIL_MIN, max))
    }

    /// The list's extent along the split of `p`.
    pub fn list_len(&self, p: Panes) -> u16 {
        if p.wide {
            self.list_width(p.body.width)
        } else {
            // Drawing only records `Panes` when the detail pane shows.
            p.body.height - self.detail_height(p.body.height).unwrap_or(0)
        }
    }

    /// Makes the list `len` columns (wide) or rows (narrow) long, within the limits.
    pub fn set_list_len(&mut self, p: Panes, len: i32) {
        let total = p.len();
        let len = u16::try_from(len.max(0)).unwrap_or(u16::MAX).min(total);
        if p.wide {
            self.wide = Some(share(clamp_width(len, total), total));
        } else {
            let Some(max) = total.checked_sub(DETAIL_MIN) else {
                return;
            };
            if max < LIST_MIN {
                return;
            }
            self.narrow = Some(share(len.clamp(LIST_MIN, max), total));
        }
    }

    /// Grows (or, negative, shrinks) the list by `delta` cells.
    pub fn nudge(&mut self, p: Panes, delta: i32) {
        let len = i32::from(self.list_len(p));
        self.set_list_len(p, len + delta);
    }

    /// Drag: the divider follows the pointer at `pos` along the split, `grab` cells
    /// from where it was pressed relative to the detail pane's first line.
    pub fn drag(&mut self, p: Panes, pos: u16, grab: i32) {
        let len = i32::from(pos) - grab - i32::from(p.start());
        self.set_list_len(p, len);
    }

    /// Back to the default split for `p`'s layout.
    pub fn reset(&mut self, p: Panes) {
        if p.wide {
            self.wide = None;
        } else {
            self.narrow = None;
        }
    }

    /// How far `pos` along the split is from the detail pane's first line (−1 on the
    /// list's border, 0 on the detail pane's): kept while dragging so the divider does
    /// not jump when pressed.
    pub fn grab(&self, p: Panes, pos: u16) -> i32 {
        i32::from(pos) - i32::from(p.start() + self.list_len(p))
    }
}

/// The list width within the wide limits (the detail pane's minimum wins).
fn clamp_width(lw: u16, w: u16) -> u16 {
    lw.max(LIST_MIN_W).min(w.saturating_sub(DETAIL_MIN_W))
}

/// `n` cells of `total` as a share; [`of`] gives `n` back for any `total` up to
/// `SCALE`.
fn share(n: u16, total: u16) -> u16 {
    if total == 0 {
        return 0;
    }
    (u32::from(n) * SCALE).div_ceil(u32::from(total)).min(SCALE) as u16
}

/// The cells `s` is of `total`.
fn of(total: u16, s: u16) -> u16 {
    (u32::from(total) * u32::from(s) / SCALE) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(w: u16) -> Panes {
        Panes {
            body: Rect::new(0, 1, w, 30),
            wide: true,
        }
    }

    fn narrow(h: u16) -> Panes {
        Panes {
            body: Rect::new(0, 1, 90, h),
            wide: false,
        }
    }

    #[test]
    fn defaults_match_the_old_layout() {
        let s = Split::default();
        assert_eq!(s.list_width(160), 92);
        assert_eq!(s.detail_height(37), Some(16));
        assert_eq!(s.detail_height(60), Some(18));
        assert_eq!(s.detail_height(14), None);
        assert_eq!(s.detail_height(15), Some(7));
    }

    #[test]
    fn set_is_exact_and_clamped() {
        let mut s = Split::default();
        for w in [110, 160, 333, 1000] {
            for len in LIST_MIN_W..=w - DETAIL_MIN_W {
                s.set_list_len(wide(w), i32::from(len));
                assert_eq!(s.list_width(w), len, "{w} {len}");
            }
        }
        s.set_list_len(wide(160), 5);
        assert_eq!(s.list_width(160), LIST_MIN_W);
        s.set_list_len(wide(160), 500);
        assert_eq!(s.list_width(160), 160 - DETAIL_MIN_W);

        let p = narrow(40);
        s.set_list_len(p, 30);
        assert_eq!(s.list_len(p), 30);
        s.set_list_len(p, 100);
        assert_eq!(s.detail_height(40), Some(DETAIL_MIN));
        s.set_list_len(p, 0);
        assert_eq!(s.list_len(p), LIST_MIN);
        // The narrow split does not move the wide one.
        assert_eq!(s.list_width(160), 160 - DETAIL_MIN_W);
    }

    #[test]
    fn share_scales_with_the_terminal() {
        let mut s = Split::default();
        s.set_list_len(wide(120), 60);
        assert_eq!(s.list_width(240), 120);
    }

    #[test]
    fn drag_follows_the_grab_point() {
        let mut s = Split::default();
        let p = wide(160);
        let border = s.list_width(160) - 1;
        let grab = s.grab(p, border);
        assert_eq!(grab, -1);
        s.drag(p, border, grab);
        assert_eq!(s.list_width(160), 92, "no jump on press");
        s.drag(p, border - 10, grab);
        assert_eq!(s.list_width(160), 82);
        s.reset(p);
        assert_eq!(s.list_width(160), 92);
    }

    #[test]
    fn nudge() {
        let mut s = Split::default();
        let p = narrow(40);
        let before = s.list_len(p);
        s.nudge(p, -1);
        assert_eq!(s.list_len(p), before - 1);
    }
}
