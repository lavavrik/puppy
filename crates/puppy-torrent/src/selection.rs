//! Which files of a torrent to download: resolving a [`Selection`] against the torrent's
//! file list, and the `<id>.selection` sidecar that records the choice.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobBuilder};
use puppy_core::types::{FileChoice, Selection, TorrentFile};
use serde::{Deserialize, Serialize};

use crate::source::TorrentInfo;

/// The 0-based indices (into `files`) that `sel` picks: the union of its indices and of
/// the files its globs match, sorted, without duplicates.
///
/// Globs are case-sensitive and matched against a file's path relative to the torrent's
/// folder (`/`-separated, e.g. `season 1/e01.mkv`). `*`, `?` and `[..]` never cross a
/// `/`; `**` does. A glob without any `/` is matched against the file name alone, so
/// `*.mkv` picks every `.mkv` in every folder (`sub/*.mkv` only those directly in
/// `sub`).
///
/// Errors (one message naming the culprit; files numbered from 1 as users see them): an
/// empty selection, an index out of range,
/// an invalid glob, or a glob that matches no file.
pub fn resolve(files: &[TorrentFile], sel: &Selection) -> Result<Vec<usize>, String> {
    if sel.indices.is_empty() && sel.globs.is_empty() {
        return Err("no files selected".into());
    }
    let mut picked = BTreeSet::new();
    for &i in &sel.indices {
        if i >= files.len() {
            // Numbered from 1, as the CLI and the dashboard show files.
            return Err(format!(
                "file {} doesn't exist — the torrent has {} file{}",
                i + 1,
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ));
        }
        picked.insert(i);
    }
    for pattern in &sel.globs {
        let matcher = compile(pattern)?;
        let by_name = !pattern.contains('/');
        let mut any = false;
        for (i, f) in files.iter().enumerate() {
            let subject = if by_name {
                f.path.rsplit('/').next().unwrap_or(&f.path)
            } else {
                f.path.as_str()
            };
            if matcher.is_match(subject) {
                picked.insert(i);
                any = true;
            }
        }
        if !any {
            return Err(format!("pattern {pattern:?} matches no file"));
        }
    }
    Ok(picked.into_iter().collect())
}

fn compile(pattern: &str) -> Result<globset::GlobMatcher, String> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(false)
        .backslash_escape(true)
        .build()
        .map_err(|e| format!("invalid pattern {pattern:?}: {}", e.kind()))?;
    Ok(glob.compile_matcher())
}

/// What `<data_dir>/torrents/<id>.selection` records (JSON). No sidecar: every file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Sidecar {
    /// Waiting for the user to choose (the stored `.torrent` copy holds the contents).
    /// `reason`: why a choice given up front could not be used (a magnet's).
    Choosing {
        #[serde(default)]
        reason: Option<String>,
    },
    /// Only these files are downloaded.
    Selected {
        /// 0-based indices into the torrent's (non-padding) file list, as on the wire.
        indices: Vec<usize>,
        /// The placeholders librqbit created for unselected files (see [`Record`]).
        #[serde(default)]
        record: Record,
    },
}

/// The unselected files librqbit created for a download. librqbit opens every file of
/// a torrent, selected or not, and may write the part of a piece shared with a selected
/// file into them: those placeholders are puppy's to delete, as long as they are still
/// the files librqbit made ([`Stamp`]). An unselected file that was already there when
/// the torrent was added is never listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// Indices of the unselected files created by this download.
    #[serde(default)]
    pub created: Vec<usize>,
    /// Their identity, taken once librqbit has created them (after its initial check).
    #[serde(default)]
    pub stamps: Vec<Stamp>,
    /// When librqbit last wrote this torrent's data, as far as puppy saw (Unix ns).
    #[serde(default)]
    pub last_write_ns: Option<u64>,
}

/// Which file is at a path: Unix `(dev, ino)`, elsewhere its creation time; plus its
/// length and modification time when stamped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stamp {
    pub index: usize,
    #[serde(default)]
    pub dev: Option<u64>,
    #[serde(default)]
    pub ino: Option<u64>,
    #[serde(default)]
    pub created_ns: Option<u64>,
    pub len: u64,
    pub modified_ns: u64,
}

fn unix_ns(t: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    let d = t.ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(d.as_nanos()).ok()
}

/// Nanoseconds since the Unix epoch, now.
pub(crate) fn now_ns() -> u64 {
    unix_ns(Ok(std::time::SystemTime::now())).unwrap_or(0)
}

impl Stamp {
    /// The stamp of the regular file `md` describes (`None`: not a regular file).
    pub fn of(index: usize, md: &std::fs::Metadata) -> Option<Stamp> {
        if !md.file_type().is_file() {
            return None;
        }
        #[cfg(unix)]
        let (dev, ino, created_ns) = {
            use std::os::unix::fs::MetadataExt;
            (Some(md.dev()), Some(md.ino()), None)
        };
        #[cfg(not(unix))]
        let (dev, ino, created_ns) = (None, None, unix_ns(md.created()));
        Some(Stamp {
            index,
            dev,
            ino,
            created_ns,
            len: md.len(),
            modified_ns: unix_ns(md.modified()).unwrap_or(u64::MAX),
        })
    }

    /// Whether `now` (the file at the path today) is still the placeholder this stamp
    /// describes, not written by anyone after librqbit (`last_write_ns`): the same file
    /// (identity), and either unchanged since stamped or last modified no later than the
    /// torrent's last write.
    pub fn still_ours(&self, now: &Stamp, last_write_ns: Option<u64>) -> bool {
        let same = match (self.ino, now.ino, self.created_ns, now.created_ns) {
            (Some(a), Some(b), _, _) => a == b && self.dev == now.dev,
            (_, _, Some(a), Some(b)) => a == b,
            _ => false,
        };
        let untouched = (self.len, self.modified_ns) == (now.len, now.modified_ns);
        same && (untouched || last_write_ns.is_some_and(|w| now.modified_ns <= w))
    }
}

impl Sidecar {
    pub fn read(path: &Path) -> Option<Sidecar> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&text) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(path = %path.display(), "ignoring an unreadable selection: {e}");
                None
            }
        }
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("selection.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// The recorded selection, if any, for `info`.
    pub fn chosen(self, info: &TorrentInfo) -> Option<Chosen> {
        match self {
            Sidecar::Selected { indices, record } => Some(Chosen::new(info, indices, record)),
            Sidecar::Choosing { .. } => None,
        }
    }
}

/// The files chosen for a download (only when not every file is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chosen {
    /// Puppy indices of the selected files, sorted.
    pub indices: Vec<usize>,
    pub record: Record,
    /// `(librqbit file index, length)` of each selected file (status sizes).
    pub lib_sizes: Vec<(usize, u64)>,
}

impl Chosen {
    pub fn new(info: &TorrentInfo, indices: Vec<usize>, record: Record) -> Self {
        let lib_sizes = indices
            .iter()
            .filter_map(|&i| Some((*info.lib_index.get(i)?, *info.sizes.get(i)?)))
            .collect();
        Chosen {
            indices,
            record,
            lib_sizes,
        }
    }

    pub fn sidecar(&self) -> Sidecar {
        Sidecar::Selected {
            indices: self.indices.clone(),
            record: self.record.clone(),
        }
    }

    pub fn total(&self) -> u64 {
        self.lib_sizes.iter().map(|&(_, len)| len).sum()
    }

    /// Whether some created placeholder has no stamp yet.
    pub fn needs_stamps(&self) -> bool {
        self.record
            .created
            .iter()
            .any(|i| !self.record.stamps.iter().any(|s| s.index == *i))
    }

    /// Stamps the created placeholders that have none yet (call once librqbit has
    /// created the files). Never through a symlink. Returns whether anything changed.
    pub fn stamp(&mut self, output_folder: &Path, info: &TorrentInfo) -> bool {
        let mut changed = false;
        for &i in &self.record.created {
            if self.record.stamps.iter().any(|s| s.index == i) {
                continue;
            }
            let Some(rel) = info.files.get(i) else {
                continue;
            };
            if let Ok(md) = std::fs::symlink_metadata(output_folder.join(rel))
                && let Some(st) = Stamp::of(i, &md)
            {
                self.record.stamps.push(st);
                changed = true;
            }
        }
        changed
    }

    /// The files puppy may delete (relative to `output_folder`): the selected ones, and
    /// the placeholders librqbit created that are still as it left them
    /// ([`Stamp::still_ours`]). A placeholder never stamped, replaced, or changed after
    /// the torrent's last write is kept.
    pub fn deletable(&self, output_folder: &Path, info: &TorrentInfo) -> Vec<PathBuf> {
        let mut keep: BTreeSet<usize> = self.indices.iter().copied().collect();
        for &i in &self.record.created {
            let (Some(rel), Some(stamp)) = (
                info.files.get(i),
                self.record.stamps.iter().find(|s| s.index == i),
            ) else {
                continue;
            };
            let now = std::fs::symlink_metadata(output_folder.join(rel))
                .ok()
                .and_then(|md| Stamp::of(i, &md));
            if now.is_some_and(|now| stamp.still_ours(&now, self.record.last_write_ns)) {
                keep.insert(i);
            }
        }
        keep.into_iter()
            .filter_map(|i| info.files.get(i).cloned())
            .collect()
    }
}

/// What to do with a torrent whose contents are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Download every file.
    All,
    /// Download only `indices`; `record`: the placeholders an earlier add of this
    /// download created (restore).
    Select { indices: Vec<usize>, record: Record },
    /// Wait for the user's choice (`reason`: why the choice given could not be used).
    Choose(Option<String>),
}

/// The decision for a new download of `info`. An invalid selection is an error for a
/// `.torrent` (`magnet: false`: refused up front) and makes a magnet wait for a choice
/// (its contents only became known now). Asking about a single file, or selecting
/// every file, is the same as no selection.
pub(crate) fn decide(
    info: &TorrentInfo,
    choice: &FileChoice,
    magnet: bool,
) -> Result<Decision, String> {
    match choice {
        FileChoice::All => Ok(Decision::All),
        FileChoice::Ask if info.files.len() > 1 => Ok(Decision::Choose(None)),
        FileChoice::Ask => Ok(Decision::All),
        FileChoice::Select { selection } => match resolve(&info.torrent_files(), selection) {
            Ok(indices) if indices.len() == info.files.len() => Ok(Decision::All),
            Ok(indices) => Ok(Decision::Select {
                indices,
                record: Record::default(),
            }),
            Err(m) if magnet => Ok(Decision::Choose(Some(m))),
            Err(m) => Err(m),
        },
    }
}

/// The decision recorded for a restored download (no sidecar: every file). A recorded
/// selection that does not fit the torrent is dropped for a new choice.
pub(crate) fn decide_restored(sidecar: Option<Sidecar>, info: &TorrentInfo) -> Decision {
    match sidecar {
        None => Decision::All,
        Some(Sidecar::Choosing { reason }) => Decision::Choose(reason),
        Some(Sidecar::Selected { indices, record }) => {
            let n = info.files.len();
            if indices.is_empty() || indices.iter().chain(&record.created).any(|&i| i >= n) {
                tracing::warn!("the recorded file selection does not fit the torrent");
                return Decision::Choose(Some("the recorded file selection was invalid".into()));
            }
            Decision::Select { indices, record }
        }
    }
}

/// `prev` plus every unselected file that is not under `output_folder` yet (librqbit is
/// about to create it; a stamp it had is dropped: the new file gets a new one). A file
/// already there that is not in `prev.created` was not made by this download and stays
/// off the list.
pub(crate) fn created_stubs(
    output_folder: &Path,
    info: &TorrentInfo,
    indices: &[usize],
    prev: &Record,
) -> Record {
    let missing = |i: usize| {
        matches!(
            std::fs::symlink_metadata(output_folder.join(&info.files[i])),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        )
    };
    let created: Vec<usize> = (0..info.files.len())
        .filter(|i| !indices.contains(i))
        .filter(|&i| prev.created.contains(&i) || missing(i))
        .collect();
    let stamps = prev
        .stamps
        .iter()
        .filter(|s| created.contains(&s.index) && !missing(s.index))
        .cloned()
        .collect();
    Record {
        created,
        stamps,
        last_write_ns: prev.last_write_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(paths: &[&str]) -> TorrentInfo {
        TorrentInfo {
            info_hash: "0".repeat(40),
            name: "t".into(),
            total_bytes: paths.len() as u64,
            single_file: None,
            files: paths.iter().map(PathBuf::from).collect(),
            sizes: vec![1; paths.len()],
            // A padding file before the last one.
            lib_index: (0..paths.len())
                .map(|i| if i + 1 == paths.len() { i + 1 } else { i })
                .collect(),
        }
    }

    fn created(c: &[usize]) -> Record {
        Record {
            created: c.to_vec(),
            ..Record::default()
        }
    }

    #[test]
    fn decisions() {
        let t = info(&["a.mkv", "b/c.mkv", "d.txt"]);
        let pick = |idx: &[usize], globs: &[&str]| FileChoice::Select {
            selection: Selection {
                indices: idx.to_vec(),
                globs: globs.iter().map(|g| g.to_string()).collect(),
            },
        };
        assert_eq!(decide(&t, &FileChoice::All, false), Ok(Decision::All));
        assert_eq!(
            decide(&t, &FileChoice::Ask, false),
            Ok(Decision::Choose(None))
        );
        assert_eq!(
            decide(&info(&["one"]), &FileChoice::Ask, true),
            Ok(Decision::All)
        );
        assert_eq!(decide(&t, &pick(&[0, 1, 2], &[]), false), Ok(Decision::All));
        assert_eq!(
            decide(&t, &pick(&[2], &["*.mkv"]), false),
            Ok(Decision::All)
        );
        assert_eq!(
            decide(&t, &pick(&[], &["*.mkv"]), false),
            Ok(Decision::Select {
                indices: vec![0, 1],
                record: Record::default(),
            })
        );
        assert!(decide(&t, &pick(&[3], &[]), false).is_err());
        assert!(matches!(
            decide(&t, &pick(&[3], &[]), true),
            Ok(Decision::Choose(Some(m))) if m.contains("file 4 doesn't exist")
        ));
        // librqbit's indices skip padding files.
        assert_eq!(t.lib_indices(&[0, 2]), vec![0, 3]);
        let c = Chosen::new(&t, vec![2], created(&[0]));
        assert_eq!(c.lib_sizes, vec![(3, 1)]);
        // A recorded selection that does not fit: choose again.
        assert!(matches!(
            decide_restored(
                Some(Sidecar::Selected {
                    indices: vec![5],
                    record: Record::default(),
                }),
                &t
            ),
            Decision::Choose(Some(_))
        ));
        assert_eq!(decide_restored(None, &t), Decision::All);
    }

    #[test]
    fn created_stubs_skip_files_already_there() {
        let d = tempfile::tempdir().unwrap();
        let t = info(&["a.mkv", "b/c.mkv", "d.txt"]);
        std::fs::write(d.path().join("d.txt"), b"user").unwrap();
        assert_eq!(
            created_stubs(d.path(), &t, &[0], &Record::default()).created,
            vec![1]
        );
        // Recorded before (created by this download): still listed, with its stamp; a
        // recorded file gone meanwhile loses its stamp (it is created anew).
        let mut c = Chosen::new(&t, vec![0], created(&[1, 2]));
        std::fs::create_dir(d.path().join("b")).unwrap();
        std::fs::write(d.path().join("b/c.mkv"), b"").unwrap();
        assert!(c.needs_stamps());
        assert!(c.stamp(d.path(), &t));
        assert!(!c.needs_stamps());
        std::fs::remove_file(d.path().join("b/c.mkv")).unwrap();
        let r = created_stubs(d.path(), &t, &[0], &c.record);
        assert_eq!(r.created, vec![1, 2]);
        assert_eq!(
            r.stamps.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn placeholders_deleted_only_while_still_ours() {
        let d = tempfile::tempdir().unwrap();
        let t = info(&["a", "b", "c", "d"]);
        for f in ["a", "b", "c"] {
            std::fs::write(d.path().join(f), b"").unwrap();
        }
        // a selected; b, c, d placeholders (d never created, so never stamped).
        let mut c = Chosen::new(&t, vec![0], created(&[1, 2, 3]));
        c.stamp(d.path(), &t);
        assert_eq!(
            c.deletable(d.path(), &t),
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        c.record.last_write_ns = Some(now_ns());
        std::thread::sleep(std::time::Duration::from_millis(20));
        // librqbit-like write before last_write is fine; a user's after it is not.
        std::fs::write(d.path().join("c"), b"user").unwrap();
        assert_eq!(
            c.deletable(d.path(), &t),
            vec![PathBuf::from("a"), PathBuf::from("b")]
        );
        // Replaced by another file of the same length: kept.
        std::fs::remove_file(d.path().join("b")).unwrap();
        std::fs::write(d.path().join("b"), b"").unwrap();
        c.record.last_write_ns = None;
        let kept = c.deletable(d.path(), &t);
        assert!(!kept.contains(&PathBuf::from("b")), "{kept:?}");
    }

    #[test]
    fn sidecar_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("1.selection");
        let t = info(&["a", "b"]);
        std::fs::write(d.path().join("b"), b"").unwrap();
        let mut c = Chosen::new(&t, vec![0], created(&[1]));
        c.stamp(d.path(), &t);
        c.record.last_write_ns = Some(5);
        for s in [
            Sidecar::Choosing { reason: None },
            Sidecar::Choosing {
                reason: Some("x".into()),
            },
            c.sidecar(),
        ] {
            s.write(&p).unwrap();
            assert_eq!(Sidecar::read(&p), Some(s));
        }
        std::fs::write(&p, r#"{"kind":"selected","indices":[3]}"#).unwrap();
        assert_eq!(
            Sidecar::read(&p),
            Some(Sidecar::Selected {
                indices: vec![3],
                record: Record::default(),
            })
        );
        std::fs::write(&p, "garbage").unwrap();
        assert_eq!(Sidecar::read(&p), None);
    }
}
