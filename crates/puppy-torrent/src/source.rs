//! Source resolution: magnet links (info hash, `dn`) and `.torrent` files (read, parse,
//! layout), and where a torrent's files go.

use std::path::{Path, PathBuf};

use librqbit::{Magnet, ValidatedTorrentMetaV1Info, torrent_from_bytes};
use puppy_core::engine::EngineError;
use puppy_core::fsutil::{is_reserved_windows_name, sanitize_filename, unique_dir_path};
use puppy_core::types::TorrentFile;

/// Largest `.torrent` file accepted (real ones are well under 1 MiB).
pub(crate) const MAX_TORRENT_FILE: u64 = 16 * 1024 * 1024;

/// What puppy needs from a magnet link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MagnetInfo {
    /// Lowercase hex BitTorrent v1 info hash.
    pub info_hash: String,
    /// The `dn` (display name) parameter, if any.
    pub name: Option<String>,
    /// The `tr` (tracker) parameters.
    pub trackers: Vec<String>,
}

impl MagnetInfo {
    /// The name shown until the metadata is known: `dn`, else `magnet <first 8 hex>`.
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("magnet {}", &self.info_hash[..8]))
    }

    /// The folder (under the output dir) the torrent's files go in: the sanitized `dn`, or
    /// the info hash. Single- or multi-file is unknown until the metadata arrives, so a
    /// magnet always gets its own folder.
    pub fn folder_name(&self) -> String {
        match &self.name {
            Some(n) => sanitize_filename(n),
            None => self.info_hash.clone(),
        }
    }
}

/// Parses a magnet link; it must carry a v1 (`btih`) info hash.
pub(crate) fn parse_magnet(uri: &str) -> Result<MagnetInfo, EngineError> {
    let magnet = Magnet::parse(uri)
        .map_err(|e| EngineError::Invalid(format!("invalid magnet link: {e:#}")))?;
    let hash = magnet.as_id20().ok_or_else(|| {
        EngineError::Invalid("invalid magnet link: no BitTorrent v1 (btih) info hash".into())
    })?;
    Ok(MagnetInfo {
        info_hash: hash.as_string(),
        name: magnet.name.filter(|n| !n.trim().is_empty()),
        trackers: magnet.trackers,
    })
}

/// What puppy needs from a `.torrent` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TorrentInfo {
    /// Lowercase hex info hash.
    pub info_hash: String,
    /// The torrent's name (its single file's name, or its folder's).
    pub name: String,
    pub total_bytes: u64,
    /// Single-file layout (`length`, no `files` list): the file's path as librqbit writes
    /// it, directly in the output dir. `None`: the files go in a folder named after the
    /// torrent.
    pub single_file: Option<PathBuf>,
    /// Every (non-padding) file's path relative to the folder librqbit writes the
    /// torrent into (its output folder). Its position is the file's puppy index (0-based,
    /// as on the wire).
    pub files: Vec<PathBuf>,
    /// The length of each of `files`.
    pub sizes: Vec<u64>,
    /// Each of `files`' index in librqbit's file list (which also counts padding files):
    /// what `only_files` takes.
    pub lib_index: Vec<usize>,
}

impl TorrentInfo {
    /// The files as listed to users: index, `/`-separated relative path, size.
    pub fn torrent_files(&self) -> Vec<TorrentFile> {
        self.files
            .iter()
            .zip(&self.sizes)
            .enumerate()
            .map(|(index, (path, &size))| TorrentFile {
                index,
                path: path
                    .iter()
                    .map(|c| c.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
                size,
            })
            .collect()
    }

    /// librqbit's indices of the files at puppy indices `indices`.
    pub fn lib_indices(&self, indices: &[usize]) -> Vec<usize> {
        indices
            .iter()
            .filter_map(|&i| self.lib_index.get(i).copied())
            .collect()
    }
}

/// Parses and validates `.torrent` bytes (as librqbit will when adding them).
pub(crate) fn parse_torrent(bytes: &[u8]) -> Result<TorrentInfo, EngineError> {
    let invalid =
        |e: &dyn std::fmt::Display| EngineError::Invalid(format!("not a valid torrent file: {e}"));
    let meta = torrent_from_bytes(bytes).map_err(|e| invalid(&e))?;
    let info_hash = meta.info_hash.as_string();
    let single = meta.info.data.files.is_none();
    let info = meta.info.data.validate().map_err(|e| invalid(&e))?;
    validate_layout(&info)?;
    let single_file = single
        .then(|| {
            info.iter_file_details()
                .next()
                .map(|f| f.filename.to_pathbuf())
        })
        .flatten();
    let mut files = Vec::new();
    let mut sizes = Vec::new();
    let mut lib_index = Vec::new();
    for (i, f) in info.iter_file_details().enumerate() {
        if f.attrs().padding {
            continue;
        }
        files.push(f.filename.to_pathbuf());
        sizes.push(f.len);
        lib_index.push(i);
    }
    let name = info
        .name()
        .map(|n| n.into_owned())
        .unwrap_or_else(|| format!("torrent {}", &info_hash[..8]));
    Ok(TorrentInfo {
        info_hash,
        name,
        total_bytes: info.lengths().total_length(),
        single_file,
        files,
        sizes,
        lib_index,
    })
}

/// Refuses file paths that are unsafe on any OS puppy supports, beyond what librqbit's
/// `validate()` checks (`..`, separators, empty names): every component of every file
/// path must be one plain name with no `:` (Windows drive prefix such as `C:evil`, which
/// replaces the base dir when joined; NTFS alternate data streams), no control
/// characters, no trailing dot or space (Windows drops them, so `a.` would be `a`), and
/// no Windows device name (`con.txt`, `NUL`, `CONIN$`, `COM1`, `COM¹`...). Applied on every
/// platform, so a torrent behaves the same everywhere.
pub(crate) fn validate_layout<B: AsRef<[u8]>>(
    info: &ValidatedTorrentMetaV1Info<B>,
) -> Result<(), EngineError> {
    for file in info.iter_file_details() {
        for component in file.filename.iter_components() {
            if let Some(why) = unsafe_component(&component) {
                return Err(EngineError::Invalid(format!(
                    "unsafe file name in torrent: {:?} ({why})",
                    file.filename.to_vec().join("/")
                )));
            }
        }
    }
    Ok(())
}

/// Why `c` is not a safe file name component, if it is not.
fn unsafe_component(c: &str) -> Option<&'static str> {
    use std::path::Component;
    if c.is_empty() || c == "." || c == ".." {
        return Some("empty or relative component");
    }
    if c.contains(['/', '\\']) {
        return Some("path separator");
    }
    if c.contains(':') {
        return Some("':' (drive prefix or alternate data stream)");
    }
    if c.chars().any(char::is_control) {
        return Some("control character");
    }
    if c.ends_with(['.', ' ']) {
        return Some("trailing dot or space");
    }
    if is_reserved_windows_name(c) {
        return Some("reserved Windows device name");
    }
    let mut parts = Path::new(c).components();
    if !matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Some("not a plain name");
    }
    None
}

/// Reads a `.torrent` file (at most [`MAX_TORRENT_FILE`] bytes).
pub(crate) async fn read_torrent_file(path: &Path) -> Result<Vec<u8>, EngineError> {
    let len = tokio::fs::metadata(path).await?.len();
    if len > MAX_TORRENT_FILE {
        return Err(EngineError::Invalid(format!(
            "{} is too large for a torrent file ({len} bytes)",
            path.display()
        )));
    }
    let bytes = tokio::fs::read(path).await?;
    if bytes.len() as u64 > MAX_TORRENT_FILE {
        return Err(EngineError::Invalid(format!(
            "{} is too large for a torrent file",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Where a torrent's files go.
///
/// A fresh add never goes where something already is (see [`Layout::fresh_torrent`]):
/// a taken folder name gets a counter (`name (1)`), and a single file whose name is
/// taken goes into a fresh folder of its own. librqbit is told not to overwrite for
/// fresh adds either; only restored torrents (whose files are ours) may write over
/// existing files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Passed to librqbit as `output_folder`: it writes the torrent's files (their paths
    /// within the torrent) directly under it.
    pub output_folder: PathBuf,
    /// Shown as the download's path: the file (single-file) or the folder.
    pub path: PathBuf,
    /// `output_folder` is the shared download dir, holding only one file of ours (it is
    /// never removed). Otherwise the output folder is the torrent's own.
    pub single_file: bool,
}

impl Layout {
    /// A single file (`file`: its path within the torrent) goes directly into `base`.
    pub fn single(base: &Path, file: &Path) -> Self {
        Layout {
            output_folder: base.to_owned(),
            path: base.join(file),
            single_file: true,
        }
    }

    /// A single file in a fresh folder of its own, `base/<folder>/<file>` (its name
    /// was taken in `base`).
    fn single_in_folder(dir: PathBuf, file: &Path) -> Self {
        Layout {
            path: dir.join(file),
            output_folder: dir,
            single_file: false,
        }
    }

    /// Where a new download of `t` goes in `base`, never where something is already:
    /// a single file directly in `base` unless its name is taken there (then in a fresh
    /// folder `base/<name> (n)/`); anything else in a fresh folder named after the
    /// torrent (`<name>`, else `<name> (1)`...). `taken` adds paths claimed by other
    /// downloads.
    pub fn fresh_torrent(base: &Path, t: &TorrentInfo, taken: impl Fn(&Path) -> bool) -> Self {
        let name = sanitize_filename(&t.name);
        match &t.single_file {
            Some(file) => {
                let plain = base.join(file);
                if std::fs::symlink_metadata(&plain).is_err() && !taken(&plain) {
                    Self::single(base, file)
                } else {
                    Self::single_in_folder(unique_dir_path(base, &name, taken), file)
                }
            }
            None => {
                let dir = unique_dir_path(base, &name, taken);
                Layout {
                    output_folder: dir.clone(),
                    path: dir,
                    single_file: false,
                }
            }
        }
    }

    /// Where a new magnet goes in `base`: a fresh folder (see [`Layout::fresh_torrent`]).
    pub fn fresh_magnet(base: &Path, m: &MagnetInfo, taken: impl Fn(&Path) -> bool) -> Self {
        let dir = unique_dir_path(base, &m.folder_name(), taken);
        Layout {
            output_folder: dir.clone(),
            path: dir,
            single_file: false,
        }
    }

    /// The layout of a torrent restored from `path` (what [`Layout::path`] was when it
    /// was added), so its files land where they were: a single file's output folder is
    /// its parent; otherwise the folder is `path` itself.
    pub fn restored(path: &Path, single_file: bool) -> Self {
        match path.parent() {
            Some(parent) if single_file => Layout {
                output_folder: parent.to_owned(),
                path: path.to_owned(),
                single_file: !in_own_folder(path),
            },
            _ => Layout {
                output_folder: path.to_owned(),
                path: path.to_owned(),
                single_file: false,
            },
        }
    }
}

/// Whether a single file at `path` sits in a folder of its own made by
/// [`Layout::fresh_torrent`]: `<dir>/<file> (n)/<file>`.
fn in_own_folder(path: &Path) -> bool {
    let (Some(file), Some(folder)) = (
        path.file_name().and_then(|f| f.to_str()),
        path.parent()
            .and_then(Path::file_name)
            .and_then(|f| f.to_str()),
    ) else {
        return false;
    };
    folder
        .strip_prefix(&sanitize_filename(file))
        .and_then(|rest| rest.strip_prefix(" ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// How a torrent's files were placed when it was added, recorded next to its stored
/// `.torrent` copy: what a completed download's recorded path is cannot be told from
/// the disk later (a folder may be gone, or replaced by anything).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// A single file directly in the shared download dir: the path is the file.
    Direct,
    /// A single file in a folder of its own (`<name> (n)/<file>`): the path is the file.
    FileInFolder,
    /// The torrent's own folder (multi-file, or any magnet): the path is that folder.
    Folder,
}

impl Placement {
    /// How `layout` places the torrent's files.
    pub fn of(layout: &Layout) -> Self {
        if layout.single_file {
            Placement::Direct
        } else if layout.output_folder == layout.path {
            Placement::Folder
        } else {
            Placement::FileInFolder
        }
    }

    /// As written to the marker file.
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Direct => "direct",
            Placement::FileInFolder => "file-in-folder",
            Placement::Folder => "folder",
        }
    }

    /// Reads a marker; anything unknown is the torrent's own folder (the choice that
    /// never reaches outside the recorded path).
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "direct" => Placement::Direct,
            "file-in-folder" => Placement::FileInFolder,
            _ => Placement::Folder,
        }
    }
}

/// Where a completed torrent recorded at `path` (placed as `placement`) has its files:
/// `(output folder, the folder above it that is never removed)`. Decided from what
/// puppy recorded, never from the disk. A single file (`Direct`, `FileInFolder`) only
/// if the metainfo is single-file and `path` is named after that file; directly in the
/// download dir: `(its dir, its dir)`; in a folder of its own: `(that folder, the dir
/// above)`. Otherwise `path` is the torrent's own folder: `(path, its parent)`. `None`:
/// nothing to delete.
pub(crate) fn completed_folders(
    path: &Path,
    t: &TorrentInfo,
    placement: Placement,
) -> Option<(PathBuf, PathBuf)> {
    let parent = path.parent()?.to_owned();
    if placement == Placement::Folder {
        return Some((path.to_owned(), parent));
    }
    let file = t.single_file.as_ref()?;
    if file.components().count() != 1 || path.file_name() != file.file_name() {
        return None;
    }
    match placement {
        Placement::Direct => Some((parent.clone(), parent)),
        _ => {
            let above = parent.parent()?.to_owned();
            Some((parent, above))
        }
    }
}

/// What is at `output_folder/rel`, walked one component at a time without following
/// any symlink.
enum Walk {
    /// Nothing there (or a component on the way is gone).
    Missing,
    /// A symlink (the output folder, a folder on the way, or the entry itself), or a
    /// non-directory on the way.
    Unsafe(PathBuf),
    /// The entry, reached through real directories only.
    Found(std::fs::Metadata),
}

fn walk(output_folder: &Path, rel: &Path) -> std::io::Result<Walk> {
    use std::path::Component;
    let mut cur = output_folder.to_owned();
    let mut md = match std::fs::symlink_metadata(&cur) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Walk::Missing),
        Err(e) => return Err(e),
    };
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Ok(Walk::Unsafe(cur.join(c)));
        };
        // Everything up to here must be a real directory.
        if md.file_type().is_symlink() || !md.is_dir() {
            return Ok(Walk::Unsafe(cur));
        }
        cur.push(name);
        md = match std::fs::symlink_metadata(&cur) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Walk::Missing),
            Err(e) => return Err(e),
        };
    }
    if md.file_type().is_symlink() {
        return Ok(Walk::Unsafe(cur));
    }
    Ok(Walk::Found(md))
}

/// Deletes exactly `files` (relative to `output_folder`), then every folder they were
/// in that is now empty, up to `output_folder` itself unless it is `keep`. Never
/// follows a symlink: a file reached through one (the output folder, a folder on the
/// way, or the file itself) is skipped, and only real, empty directories inside the
/// torrent's folder are removed. Goes on past errors and returns the first.
pub(crate) fn delete_listed(
    output_folder: &Path,
    files: &[PathBuf],
    keep: &Path,
) -> std::io::Result<()> {
    use std::collections::BTreeSet;
    let mut first_err = None;
    let mut dirs = BTreeSet::new();
    for rel in files {
        let path = output_folder.join(rel);
        match walk(output_folder, rel) {
            Ok(Walk::Found(m)) if !m.is_dir() => {
                if let Err(e) = std::fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %path.display(), "cannot delete: {e}");
                    first_err.get_or_insert(e);
                }
            }
            Ok(Walk::Found(_)) => {
                tracing::warn!(path = %path.display(), "not deleting a directory");
            }
            Ok(Walk::Unsafe(at)) => tracing::warn!(
                path = %path.display(),
                at = %at.display(),
                "not deleting: reached through a symlink"
            ),
            Ok(Walk::Missing) => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), "cannot delete: {e}");
                first_err.get_or_insert(e);
            }
        }
        let mut dir = rel.parent();
        while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            dirs.insert(d.to_owned());
            dir = d.parent();
        }
    }
    let is_real_dir =
        |rel: &Path| matches!(walk(output_folder, rel), Ok(Walk::Found(m)) if m.is_dir());
    // Deepest first.
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        if is_real_dir(&d) {
            // Only removes an empty directory.
            let _ = std::fs::remove_dir(output_folder.join(d));
        }
    }
    if output_folder != keep && is_real_dir(Path::new("")) {
        let _ = std::fs::remove_dir(output_folder);
    }
    first_err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn magnet_dn_parsing() {
        let m = parse_magnet(&format!("magnet:?xt=urn:btih:{HASH}&dn=My%20Show%20S01")).unwrap();
        assert_eq!(m.info_hash, HASH);
        assert_eq!(m.name.as_deref(), Some("My Show S01"));
        assert_eq!(m.display_name(), "My Show S01");
        assert_eq!(m.folder_name(), "My Show S01");

        // `dn` with a path separator: the folder name is sanitized.
        let m = parse_magnet(&format!("magnet:?xt=urn:btih:{HASH}&dn=..%2Fetc")).unwrap();
        assert_eq!(m.display_name(), "../etc");
        assert_eq!(m.folder_name(), "etc");

        // No (or empty) `dn`: named after the info hash.
        for uri in [
            format!("magnet:?xt=urn:btih:{HASH}"),
            format!("magnet:?xt=urn:btih:{HASH}&dn="),
            format!("magnet:?xt=urn:btih:{}", HASH.to_uppercase()),
        ] {
            let m = parse_magnet(&uri).unwrap();
            assert_eq!(m.info_hash, HASH, "{uri}");
            assert_eq!(m.name, None, "{uri}");
            assert_eq!(m.display_name(), "magnet 01234567");
            assert_eq!(m.folder_name(), HASH);
        }
    }

    #[test]
    fn invalid_magnets_are_rejected() {
        for uri in [
            "magnet:?xt=bad",
            "magnet:?dn=nothing",
            "magnet:?xt=urn:btih:xyz",
            "http://example.com/a.torrent",
            "not a url",
        ] {
            let err = parse_magnet(uri).unwrap_err();
            assert!(
                matches!(&err, EngineError::Invalid(m) if m.contains("magnet")),
                "{uri}: {err}"
            );
        }
    }

    #[test]
    fn garbage_torrent_is_invalid() {
        for bytes in [&b"garbage"[..], b"", b"d4:infod4:name1:xee"] {
            let err = parse_torrent(bytes).unwrap_err();
            assert!(
                matches!(&err, EngineError::Invalid(m) if m.starts_with("not a valid torrent")),
                "{err}"
            );
        }
    }

    #[test]
    fn unsafe_components() {
        for bad in [
            "C:evil",
            "a:b",
            "con.txt",
            "CON",
            "nul",
            "com1.log",
            "LPT9",
            "a\u{1}b",
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "trailing.",
            "trailing ",
            "CONIN$",
            "conout$.txt",
            "COM\u{b9}",
            "lpt\u{b2}.bin",
            "LPT\u{b3}",
        ] {
            assert!(unsafe_component(bad).is_some(), "{bad:?} accepted");
        }
        for ok in [
            "a.bin",
            "console.txt",
            "com10",
            ".hidden",
            "Some Show S01",
            "файл",
        ] {
            assert_eq!(unsafe_component(ok), None, "{ok:?} refused");
        }
    }

    fn info(name: &str, single: bool) -> TorrentInfo {
        TorrentInfo {
            info_hash: HASH.into(),
            name: name.into(),
            total_bytes: 1,
            single_file: single.then(|| PathBuf::from(name)),
            files: if single {
                vec![PathBuf::from(name)]
            } else {
                vec![PathBuf::from("a.bin"), PathBuf::from("sub/b.bin")]
            },
            sizes: if single { vec![1] } else { vec![1, 0] },
            lib_index: if single { vec![0] } else { vec![0, 1] },
        }
    }

    #[test]
    fn fresh_layouts_avoid_existing_paths() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        let none = |_: &Path| false;
        let single = Layout::fresh_torrent(base, &info("a.iso", true), none);
        assert_eq!(single, Layout::single(base, Path::new("a.iso")));
        let multi = Layout::fresh_torrent(base, &info("bundle", false), none);
        assert!(!multi.single_file);
        assert_eq!(multi.output_folder, base.join("bundle"));
        assert_eq!(multi.path, base.join("bundle"));
        let m = parse_magnet(&format!("magnet:?xt=urn:btih:{HASH}")).unwrap();
        assert_eq!(Layout::fresh_magnet(base, &m, none).path, base.join(HASH));

        std::fs::write(base.join("a.iso"), b"user").unwrap();
        std::fs::create_dir(base.join("bundle")).unwrap();
        std::fs::create_dir(base.join(HASH)).unwrap();
        let single = Layout::fresh_torrent(base, &info("a.iso", true), none);
        assert_eq!(single.output_folder, base.join("a.iso (1)"));
        assert_eq!(single.path, base.join("a.iso (1)/a.iso"));
        assert!(!single.single_file, "its own folder");
        let multi = Layout::fresh_torrent(base, &info("bundle", false), none);
        assert_eq!(multi.path, base.join("bundle (1)"));
        // Claimed by another download.
        let claimed = base.join("bundle (1)");
        let multi = Layout::fresh_torrent(base, &info("bundle", false), |p| p == claimed);
        assert_eq!(multi.path, base.join("bundle (2)"));
        assert_eq!(
            Layout::fresh_magnet(base, &m, none).path,
            base.join(format!("{HASH} (1)"))
        );
    }

    #[test]
    fn restored_layouts_match_the_original() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        let none = |_: &Path| false;
        let single = Layout::fresh_torrent(base, &info("a.iso", true), none);
        assert_eq!(Layout::restored(&single.path, true), single);
        let multi = Layout::fresh_torrent(base, &info("bundle", false), none);
        assert_eq!(Layout::restored(&multi.path, false), multi);
        std::fs::write(base.join("a.iso"), b"user").unwrap();
        let own = Layout::fresh_torrent(base, &info("a.iso", true), none);
        assert_eq!(Layout::restored(&own.path, true), own);
    }

    #[test]
    fn delete_listed_removes_only_the_torrents_files() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("dl");
        let folder = base.join("bundle");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::create_dir_all(folder.join("other")).unwrap();
        std::fs::write(folder.join("a.bin"), b"a").unwrap();
        std::fs::write(folder.join("sub/b.bin"), b"b").unwrap();
        std::fs::write(base.join("keep.txt"), b"k").unwrap();
        let t = info("bundle", false);
        let (out, keep) = completed_folders(&folder, &t, Placement::Folder).unwrap();
        assert_eq!((&out, &keep), (&folder, &base));
        // Something of the user's in the torrent's folder keeps the folder.
        std::fs::write(folder.join("other/x"), b"x").unwrap();
        delete_listed(&out, &t.files, &keep).unwrap();
        assert!(!folder.join("a.bin").exists());
        assert!(!folder.join("sub").exists(), "empty sub-folder removed");
        assert!(folder.join("other/x").exists());
        std::fs::remove_file(folder.join("other/x")).unwrap();
        delete_listed(&out, &t.files, &keep).expect("already gone is fine");
        assert!(
            folder.join("other").exists(),
            "a folder not of the torrent stays"
        );
        assert!(base.join("keep.txt").exists());

        // Single file directly in the download dir: the dir stays.
        let t = info("a.iso", true);
        std::fs::write(base.join("a.iso"), b"a").unwrap();
        let (out, keep) = completed_folders(&base.join("a.iso"), &t, Placement::Direct).unwrap();
        assert_eq!((&out, &keep), (&base, &base));
        std::fs::remove_file(base.join("keep.txt")).unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        delete_listed(&out, &t.files, &keep).unwrap();
        assert!(!base.join("a.iso").exists());
        assert!(base.is_dir(), "the download dir is kept even if empty");

        // In a folder of its own: that folder goes too.
        let own = base.join("a.iso (1)");
        std::fs::create_dir(&own).unwrap();
        std::fs::write(own.join("a.iso"), b"a").unwrap();
        let (out, keep) =
            completed_folders(&own.join("a.iso"), &t, Placement::FileInFolder).unwrap();
        assert_eq!((&out, &keep), (&own, &base));
        delete_listed(&out, &t.files, &keep).unwrap();
        assert!(!own.exists());
        // A magnet's single file (path: its folder).
        let mag = base.join(HASH);
        std::fs::create_dir(&mag).unwrap();
        std::fs::write(mag.join("a.iso"), b"a").unwrap();
        let (out, keep) = completed_folders(&mag, &t, Placement::Folder).unwrap();
        assert_eq!((&out, &keep), (&mag, &base));
        delete_listed(&out, &t.files, &keep).unwrap();
        assert!(!mag.exists());
        assert!(base.is_dir());
    }

    #[test]
    fn placement_follows_the_layout_and_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        let none = |_: &Path| false;
        let direct = Layout::fresh_torrent(base, &info("a.iso", true), none);
        assert_eq!(Placement::of(&direct), Placement::Direct);
        let multi = Layout::fresh_torrent(base, &info("bundle", false), none);
        assert_eq!(Placement::of(&multi), Placement::Folder);
        std::fs::write(base.join("a.iso"), b"user").unwrap();
        let own = Layout::fresh_torrent(base, &info("a.iso", true), none);
        assert_eq!(Placement::of(&own), Placement::FileInFolder);
        let m = parse_magnet(&format!("magnet:?xt=urn:btih:{HASH}&dn=a.iso")).unwrap();
        assert_eq!(
            Placement::of(&Layout::fresh_magnet(base, &m, none)),
            Placement::Folder
        );
        for p in [
            Placement::Direct,
            Placement::FileInFolder,
            Placement::Folder,
        ] {
            assert_eq!(Placement::parse(p.as_str()), p);
        }
        assert_eq!(Placement::parse("garbage"), Placement::Folder);
    }

    #[test]
    fn completed_folders_never_falls_back_to_the_download_dir() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("dl");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("ubuntu.iso"), b"user").unwrap();
        let t = info("ubuntu.iso", true);
        // A magnet's single file, its folder gone: nothing outside it is touched.
        let mag = base.join("ubuntu.iso (1)");
        let (out, keep) = completed_folders(&mag, &t, Placement::Folder).unwrap();
        assert_eq!((&out, &keep), (&mag, &base));
        delete_listed(&out, &t.files, &keep).unwrap();
        assert_eq!(std::fs::read(base.join("ubuntu.iso")).unwrap(), b"user");
        // ... or replaced by a file.
        std::fs::write(&mag, b"user 2").unwrap();
        delete_listed(&out, &t.files, &keep).unwrap();
        assert_eq!(std::fs::read(&mag).unwrap(), b"user 2");
        assert!(base.join("ubuntu.iso").exists());
        // A magnet named exactly like its file, its folder gone: still a folder.
        let (out, _) = completed_folders(&base.join("ubuntu.iso"), &t, Placement::Folder).unwrap();
        assert_eq!(out, base.join("ubuntu.iso"));
        delete_listed(&out, &t.files, &base).unwrap();
        assert!(base.join("ubuntu.iso").exists());

        // A single-file placement needs single-file metainfo named like the path.
        assert_eq!(
            completed_folders(&base.join("other.iso"), &t, Placement::Direct),
            None
        );
        assert_eq!(
            completed_folders(
                &base.join("bundle"),
                &info("bundle", false),
                Placement::Direct
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_listed_never_follows_symlinks() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("dl");
        let outside = d.path().join("outside");
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        for f in ["a.bin", "b.bin", "sub/b.bin"] {
            std::fs::write(outside.join(f), b"user").unwrap();
        }
        let t = info("bundle", false);
        let intact = || {
            for f in ["a.bin", "b.bin", "sub/b.bin"] {
                assert_eq!(std::fs::read(outside.join(f)).unwrap(), b"user", "{f}");
            }
        };

        // A symlinked sub-folder.
        let folder = base.join("bundle");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.bin"), b"a").unwrap();
        symlink(&outside, folder.join("sub")).unwrap();
        delete_listed(&folder, &t.files, &base).unwrap();
        intact();
        assert!(!folder.join("a.bin").exists());
        assert!(std::fs::symlink_metadata(folder.join("sub")).is_ok());
        assert!(folder.is_dir(), "still holds the symlink");
        std::fs::remove_dir_all(&folder).unwrap();

        // The torrent's folder itself a symlink.
        symlink(outside.join("sub"), &folder).unwrap();
        std::fs::write(outside.join("sub/a.bin"), b"user").unwrap();
        let (out, keep) = completed_folders(&folder, &t, Placement::Folder).unwrap();
        delete_listed(&out, &t.files, &keep).unwrap();
        intact();
        assert!(outside.join("sub/a.bin").exists());
        assert!(std::fs::symlink_metadata(&folder).is_ok());
        std::fs::remove_file(&folder).unwrap();

        // A listed file that is a symlink.
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        symlink(outside.join("a.bin"), folder.join("a.bin")).unwrap();
        std::fs::write(folder.join("sub/b.bin"), b"b").unwrap();
        delete_listed(&folder, &t.files, &base).unwrap();
        intact();
        assert!(std::fs::symlink_metadata(folder.join("a.bin")).is_ok());
        assert!(!folder.join("sub").exists());

        // The download dir a symlink (single file directly in it): skipped.
        let real = d.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.iso"), b"user").unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        let t = info("a.iso", true);
        let (out, keep) = completed_folders(&link.join("a.iso"), &t, Placement::Direct).unwrap();
        delete_listed(&out, &t.files, &keep).unwrap();
        assert!(real.join("a.iso").exists());
    }
}
