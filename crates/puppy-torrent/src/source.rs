//! Source resolution: magnet links (info hash, `dn`) and `.torrent` files (read, parse,
//! layout), and where a torrent's files go.

use std::path::{Path, PathBuf};

use librqbit::{Magnet, torrent_from_bytes};
use puppy_core::engine::EngineError;
use puppy_core::fsutil::sanitize_filename;

/// Largest `.torrent` file accepted (real ones are well under 1 MiB).
pub(crate) const MAX_TORRENT_FILE: u64 = 16 * 1024 * 1024;

/// What puppy needs from a magnet link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MagnetInfo {
    /// Lowercase hex BitTorrent v1 info hash.
    pub info_hash: String,
    /// The `dn` (display name) parameter, if any.
    pub name: Option<String>,
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
}

/// Parses and validates `.torrent` bytes (as librqbit will when adding them).
pub(crate) fn parse_torrent(bytes: &[u8]) -> Result<TorrentInfo, EngineError> {
    let invalid =
        |e: &dyn std::fmt::Display| EngineError::Invalid(format!("not a valid torrent file: {e}"));
    let meta = torrent_from_bytes(bytes).map_err(|e| invalid(&e))?;
    let info_hash = meta.info_hash.as_string();
    let single = meta.info.data.files.is_none();
    let info = meta.info.data.validate().map_err(|e| invalid(&e))?;
    let single_file = single
        .then(|| {
            info.iter_file_details()
                .next()
                .map(|f| f.filename.to_pathbuf())
        })
        .flatten();
    let name = info
        .name()
        .map(|n| n.into_owned())
        .unwrap_or_else(|| format!("torrent {}", &info_hash[..8]));
    Ok(TorrentInfo {
        info_hash,
        name,
        total_bytes: info.lengths().total_length(),
        single_file,
    })
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Passed to librqbit as `output_folder`: it writes the torrent's files (their paths
    /// within the torrent) directly under it.
    pub output_folder: PathBuf,
    /// Shown as the download's path: the file (single-file) or the folder.
    pub path: PathBuf,
    /// `output_folder` is the shared download dir, holding only one file of ours.
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

    /// Anything else (multi-file, or a magnet whose layout is unknown) goes into
    /// `base/<folder>`.
    pub fn folder(base: &Path, folder: &str) -> Self {
        let dir = base.join(folder);
        Layout {
            output_folder: dir.clone(),
            path: dir,
            single_file: false,
        }
    }

    pub fn for_torrent(base: &Path, t: &TorrentInfo) -> Self {
        match &t.single_file {
            Some(file) => Self::single(base, file),
            None => Self::folder(base, &sanitize_filename(&t.name)),
        }
    }

    pub fn for_magnet(base: &Path, m: &MagnetInfo) -> Self {
        Self::folder(base, &m.folder_name())
    }
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
    fn layouts() {
        let base = Path::new("/dl");
        let single = Layout::single(base, Path::new("a.iso"));
        assert_eq!(single.output_folder, base);
        assert_eq!(single.path, base.join("a.iso"));
        assert!(single.single_file);
        let multi = Layout::folder(base, "bundle");
        assert!(!multi.single_file);
        assert_eq!(multi.output_folder, base.join("bundle"));
        assert_eq!(multi.path, base.join("bundle"));
        let m = parse_magnet(&format!("magnet:?xt=urn:btih:{HASH}")).unwrap();
        assert_eq!(Layout::for_magnet(base, &m).output_folder, base.join(HASH));
    }
}
