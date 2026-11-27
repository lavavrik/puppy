//! What the dashboard remembers between runs, in `<data_dir>/tui.toml`: written by the
//! dashboard itself (not meant to be edited; `config.toml` holds the settings).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::app::Split;

/// The file's name in puppy's data directory.
pub const FILE_NAME: &str = "tui.toml";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Where the divider between the list and the detail pane sits.
    pub split: Split,
}

/// The prefs in `path`; the defaults when it is missing or unreadable (nothing here is
/// worth refusing to start over).
pub fn load(path: &Path) -> Prefs {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Prefs::default();
    };
    let mut prefs: Prefs = toml::from_str(&text).unwrap_or_default();
    prefs.split = prefs.split.sanitized();
    prefs
}

/// Writes `prefs` to `path` whole (through a temporary file renamed over it), so a
/// crash or a second dashboard never leaves half a file.
pub fn save(path: &Path, prefs: &Prefs) -> io::Result<()> {
    let text = toml::to_string(prefs).map_err(io::Error::other)?;
    let text = format!("# Written by puppy's dashboard; settings go in config.toml.\n{text}");
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Panes;
    use ratatui::layout::Rect;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(load(&path), Prefs::default(), "missing file");
        let mut prefs = Prefs::default();
        let wide = Panes {
            body: Rect::new(0, 1, 160, 30),
            wide: true,
        };
        prefs.split.set_list_len(wide, 70);
        save(&path, &prefs).unwrap();
        save(&path, &prefs).unwrap();
        let back = load(&path);
        assert_eq!(back, prefs);
        assert_eq!(back.split.list_width(160), 70);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp left"
        );
    }

    #[test]
    fn bad_files_give_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "split = 3\n").unwrap();
        assert_eq!(load(&path), Prefs::default());
        // Out-of-range shares are dropped, not trusted.
        std::fs::write(&path, "[split]\nwide = 60000\nnarrow = 5000\n").unwrap();
        let p = load(&path);
        assert_eq!(p.split.list_width(160), Split::default().list_width(160));
        assert_eq!(p.split.detail_height(40), Some(20));
        // Unknown keys (from a newer puppy) are ignored.
        std::fs::write(&path, "future = true\n[split]\nnarrow = 5000\n").unwrap();
        assert_eq!(load(&path).split.detail_height(40), Some(20));
    }
}
