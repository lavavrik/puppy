//! A Metalink `<file name>` as a path below the output folder.
//!
//! A name may hold subdirectories (`iso/x86_64/a.iso`), separated by `/` (or `\`, which
//! is a separator on Windows and so is treated as one everywhere). The document is
//! untrusted, so a name that could leave the output folder is refused rather than
//! repaired: an absolute path, a drive or UNC prefix, a `..` component or a component
//! of dots only, control characters (NUL included), Unicode format characters (bidi
//! overrides and isolates, zero-width characters, the BOM: they make a name look like
//! another). What is merely unportable is
//! repaired per component as for any downloaded file name ([`sanitize_filename`]):
//! `<>:"|?*` become `_`, trailing dots and spaces go, Windows device names (`CON`,
//! `nul.txt`, `COM1`) get a leading `_`, and long components are shortened.

use crate::fsutil::sanitize_filename;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// At most this many components (directories plus the file).
const MAX_COMPONENTS: usize = 32;
/// At most this many bytes in the name as written.
const MAX_NAME_BYTES: usize = 4096;

/// A safe relative path: one or more cleaned components, the last one the file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafePath(Vec<String>);

impl SafePath {
    /// Checks and cleans a `<file name>`.
    pub fn parse(raw: &str) -> Result<SafePath, String> {
        if raw.trim().is_empty() {
            return Err("the file has no name".into());
        }
        if raw.len() > MAX_NAME_BYTES {
            return Err(format!("the name is longer than {MAX_NAME_BYTES} bytes"));
        }
        if raw.chars().any(char::is_control) {
            return Err("the name contains a control character".into());
        }
        if let Some(c) = raw.chars().find(|c| is_format_char(*c)) {
            return Err(format!(
                "the name contains an invisible formatting character (U+{:04X})",
                c as u32
            ));
        }
        if raw.starts_with(['/', '\\']) {
            return Err("the name is an absolute path".into());
        }
        let mut chars = raw.chars();
        if let (Some(d), Some(':')) = (chars.next(), chars.next())
            && d.is_ascii_alphabetic()
        {
            return Err("the name starts with a drive letter".into());
        }
        if raw.ends_with(['/', '\\']) {
            return Err("the name ends in a separator: it names a folder, not a file".into());
        }
        let mut parts = Vec::new();
        for c in raw.split(['/', '\\']) {
            if c.is_empty() || c == "." {
                continue;
            }
            if c.trim_matches([' ', '.']).is_empty() {
                // `..`, and what Windows reads as `.` or `..` (`...`, `.. `).
                return Err(format!(
                    "the name has a {:?} component, which could leave the output folder",
                    c
                ));
            }
            parts.push(sanitize_filename(c));
        }
        if parts.is_empty() {
            return Err("the file has no name".into());
        }
        if parts.len() > MAX_COMPONENTS {
            return Err(format!(
                "the name has more than {MAX_COMPONENTS} components"
            ));
        }
        let path = SafePath(parts);
        // Defence in depth: whatever the platform makes of it, it stays relative.
        if !path
            .relative()
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err("the name is not a plain relative path".into());
        }
        Ok(path)
    }

    /// The file name (the last component).
    pub fn file_name(&self) -> &str {
        self.0.last().expect("at least one component")
    }

    /// The subdirectories, outermost first (empty for a bare file name).
    pub fn dirs(&self) -> &[String] {
        &self.0[..self.0.len() - 1]
    }

    pub fn has_dirs(&self) -> bool {
        self.0.len() > 1
    }

    /// The whole path, relative.
    pub fn relative(&self) -> PathBuf {
        self.0.iter().collect()
    }

    /// The folder the file goes in, below `base`.
    pub fn dir_in(&self, base: &Path) -> PathBuf {
        let mut dir = base.to_path_buf();
        dir.extend(self.dirs());
        dir
    }

    /// [`SafePath::dir_in`], refused if a subdirectory on the way already exists as a
    /// symbolic link (creating the folders would follow it out of `base`) or as
    /// something other than a folder. `base` itself is the user's choice and not checked.
    pub fn checked_dir_in(&self, base: &Path) -> Result<PathBuf, String> {
        let mut dir = base.to_path_buf();
        let mut exists = true;
        for d in self.dirs() {
            dir.push(d);
            if !exists {
                continue;
            }
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(format!(
                        "{} is a symbolic link, which puppy does not follow for a metalink's \
                         subdirectory",
                        dir.display()
                    ));
                }
                Ok(m) if !m.is_dir() => {
                    return Err(format!("{} exists and is not a folder", dir.display()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => exists = false,
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
        }
        Ok(dir)
    }

    /// Comparison key: NFC and lowercased, so two names equal by it would collide on
    /// Windows and macOS (or merely look the same).
    pub(super) fn key(&self) -> String {
        self.0.join("/").nfc().collect::<String>().to_lowercase()
    }
}

/// Unicode format characters (general category Cf): invisible, they make a name display
/// as another (`U+202E` reverses what follows: `exe.txt` shown for `txt.exe`).
fn is_format_char(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// `/`-separated, as written in the document after cleaning.
impl fmt::Display for SafePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> String {
        SafePath::parse(raw)
            .unwrap_or_else(|e| panic!("{raw:?}: {e}"))
            .to_string()
    }

    #[test]
    fn plain_names_and_subdirectories() {
        assert_eq!(ok("a.iso"), "a.iso");
        assert_eq!(ok("iso/x86_64/a.iso"), "iso/x86_64/a.iso");
        assert_eq!(ok("dir\\sub\\a.iso"), "dir/sub/a.iso");
        assert_eq!(ok("./a//b/./c.txt"), "a/b/c.txt");
        assert_eq!(ok("..a/b..c/.hidden"), "..a/b..c/.hidden");
        let p = SafePath::parse("x/y/z.bin").unwrap();
        assert_eq!(p.file_name(), "z.bin");
        assert_eq!(p.dirs(), ["x", "y"]);
        assert!(p.has_dirs());
        assert_eq!(
            p.dir_in(Path::new("/dl")),
            Path::new("/dl").join("x").join("y")
        );
        assert!(!SafePath::parse("z.bin").unwrap().has_dirs());
    }

    #[test]
    fn invisible_formatting_characters_are_refused() {
        for raw in [
            "invoice\u{202E}fdp.exe",
            "a\u{202A}b",
            "a\u{2066}b\u{2069}",
            "zero\u{200B}width",
            "rtl\u{200F}mark",
            "\u{FEFF}bom.txt",
            "dir\u{2060}/f",
            "soft\u{AD}hyphen",
            "tag\u{E0041}",
        ] {
            let e = SafePath::parse(raw).unwrap_err();
            assert!(e.contains("formatting character"), "{raw:?}: {e}");
        }
        // Ordinary non-ASCII names are fine.
        assert_eq!(ok("Ünïcødé/日本語.iso"), "Ünïcødé/日本語.iso");
    }

    #[test]
    fn keys_compare_nfc_and_case() {
        let composed = SafePath::parse("caf\u{e9}/A.txt").unwrap();
        let decomposed = SafePath::parse("cafe\u{301}/a.TXT").unwrap();
        assert_ne!(composed, decomposed);
        assert_eq!(composed.key(), decomposed.key());
    }

    #[cfg(unix)]
    #[test]
    fn subdirectories_never_go_through_symlinks() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let p = SafePath::parse("a/b/c/f.iso").unwrap();
        // Nothing there yet, or real folders: fine.
        assert_eq!(
            p.checked_dir_in(base.path()).unwrap(),
            p.dir_in(base.path())
        );
        std::fs::create_dir_all(base.path().join("a/b")).unwrap();
        assert!(p.checked_dir_in(base.path()).is_ok());
        // `a/b/c` placed as a link elsewhere: refused.
        std::os::unix::fs::symlink(outside.path(), base.path().join("a/b/c")).unwrap();
        let e = p.checked_dir_in(base.path()).unwrap_err();
        assert!(e.contains("symbolic link"), "{e}");
        // A file where a folder should be.
        std::fs::write(base.path().join("x"), "").unwrap();
        let e = SafePath::parse("x/f")
            .unwrap()
            .checked_dir_in(base.path())
            .unwrap_err();
        assert!(e.contains("not a folder"), "{e}");
        // A link as the base itself is the user's choice.
        let linked = base.path().join("linked-base");
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        assert!(
            SafePath::parse("d/f")
                .unwrap()
                .checked_dir_in(&linked)
                .is_ok()
        );
    }

    #[test]
    fn unportable_components_are_repaired() {
        assert_eq!(ok("CON/nul.txt"), "_CON/_nul.txt");
        assert_eq!(ok("com1/LPT9.log"), "_com1/_LPT9.log");
        assert_eq!(ok("a<b>/c:d|e?.txt"), "a_b_/c_d_e_.txt");
        assert_eq!(ok("trailing. /name. "), "trailing/name");
        let long = "x".repeat(300) + ".iso";
        let got = ok(&long);
        assert!(got.len() <= 200 && got.ends_with(".iso"), "{got}");
    }

    #[test]
    fn escaping_and_hostile_names_are_refused() {
        for raw in [
            "",
            "   ",
            "/etc/passwd",
            "\\windows\\system32\\x.dll",
            "\\\\server\\share\\x",
            "C:\\x.iso",
            "c:x.iso",
            "../x",
            "a/../../x",
            "a\\..\\x",
            "a/.../x",
            "a/.. /x",
            "a/ . /x",
            "a\0b",
            "a\nb",
            "a\u{1b}[31mb",
            "a\u{7f}",
            "dir/",
            ".",
            "./",
        ] {
            assert!(SafePath::parse(raw).is_err(), "{raw:?} was accepted");
        }
        let deep = vec!["d"; MAX_COMPONENTS + 1].join("/");
        assert!(SafePath::parse(&deep).is_err());
        assert!(SafePath::parse(&"a".repeat(MAX_NAME_BYTES + 1)).is_err());
    }
}
