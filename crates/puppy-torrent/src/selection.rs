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
/// Errors (one message naming the culprit): an empty selection, an index out of range,
/// an invalid glob, or a glob that matches no file.
pub fn resolve(files: &[TorrentFile], sel: &Selection) -> Result<Vec<usize>, String> {
    if sel.indices.is_empty() && sel.globs.is_empty() {
        return Err("no files selected".into());
    }
    let mut picked = BTreeSet::new();
    for &i in &sel.indices {
        if i >= files.len() {
            return Err(format!(
                "no file with index {i} (0-based): the torrent has {} file{}",
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
        /// Unselected files that librqbit created for this download (it opens every
        /// file of a torrent, selected or not, and may write the part of a piece shared
        /// with a selected file into them): they are puppy's to delete. An unselected
        /// file that was already there when the torrent was added is never listed.
        #[serde(default)]
        created: Vec<usize>,
    },
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
}

/// The files chosen for a download (only when not every file is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chosen {
    /// Puppy indices of the selected files, sorted.
    pub indices: Vec<usize>,
    /// See [`Sidecar::Selected::created`].
    pub created: Vec<usize>,
    /// `(librqbit file index, length)` of each selected file (status sizes).
    pub lib_sizes: Vec<(usize, u64)>,
}

impl Chosen {
    pub fn new(info: &TorrentInfo, indices: Vec<usize>, created: Vec<usize>) -> Self {
        let lib_sizes = indices
            .iter()
            .filter_map(|&i| Some((*info.lib_index.get(i)?, *info.sizes.get(i)?)))
            .collect();
        Chosen {
            indices,
            created,
            lib_sizes,
        }
    }

    pub fn sidecar(&self) -> Sidecar {
        Sidecar::Selected {
            indices: self.indices.clone(),
            created: self.created.clone(),
        }
    }

    pub fn total(&self) -> u64 {
        self.lib_sizes.iter().map(|&(_, len)| len).sum()
    }

    /// The files puppy may delete (relative to the output folder): the selected ones
    /// and the unselected ones librqbit created for this download.
    pub fn deletable(&self, info: &TorrentInfo) -> Vec<PathBuf> {
        let keep: BTreeSet<usize> = self.indices.iter().chain(&self.created).copied().collect();
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
    /// Download only `indices`; `created`: unselected files recorded as created by an
    /// earlier add of this download (restore).
    Select {
        indices: Vec<usize>,
        created: Vec<usize>,
    },
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
                created: Vec::new(),
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
        Some(Sidecar::Selected { indices, created }) => {
            let n = info.files.len();
            if indices.is_empty() || indices.iter().chain(&created).any(|&i| i >= n) {
                tracing::warn!("the recorded file selection does not fit the torrent");
                return Decision::Choose(Some("the recorded file selection was invalid".into()));
            }
            Decision::Select { indices, created }
        }
    }
}

/// `created` plus every unselected file that is not under `output_folder` yet (librqbit
/// is about to create it). A file already there that is not in `created` was not made
/// by this download and stays off the list.
pub(crate) fn created_stubs(
    output_folder: &Path,
    info: &TorrentInfo,
    indices: &[usize],
    created: &[usize],
) -> Vec<usize> {
    (0..info.files.len())
        .filter(|i| !indices.contains(i))
        .filter(|i| {
            created.contains(i)
                || matches!(
                    std::fs::symlink_metadata(output_folder.join(&info.files[*i])),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                )
        })
        .collect()
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
                created: vec![]
            })
        );
        assert!(decide(&t, &pick(&[3], &[]), false).is_err());
        assert!(matches!(
            decide(&t, &pick(&[3], &[]), true),
            Ok(Decision::Choose(Some(m))) if m.contains('3')
        ));
        // librqbit's indices skip padding files.
        assert_eq!(t.lib_indices(&[0, 2]), vec![0, 3]);
        let c = Chosen::new(&t, vec![2], vec![0]);
        assert_eq!(c.lib_sizes, vec![(3, 1)]);
        assert_eq!(
            c.deletable(&t),
            vec![PathBuf::from("a.mkv"), PathBuf::from("d.txt")]
        );
        // A recorded selection that does not fit: choose again.
        assert!(matches!(
            decide_restored(
                Some(Sidecar::Selected {
                    indices: vec![5],
                    created: vec![]
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
        assert_eq!(created_stubs(d.path(), &t, &[0], &[]), vec![1]);
        // Recorded before (created by this download): still listed.
        assert_eq!(created_stubs(d.path(), &t, &[0], &[2]), vec![1, 2]);
    }

    #[test]
    fn sidecar_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("1.selection");
        for s in [
            Sidecar::Choosing { reason: None },
            Sidecar::Choosing {
                reason: Some("x".into()),
            },
            Sidecar::Selected {
                indices: vec![0, 2],
                created: vec![1],
            },
        ] {
            s.write(&p).unwrap();
            assert_eq!(Sidecar::read(&p), Some(s));
        }
        std::fs::write(&p, r#"{"kind":"selected","indices":[3]}"#).unwrap();
        assert_eq!(
            Sidecar::read(&p),
            Some(Sidecar::Selected {
                indices: vec![3],
                created: vec![]
            })
        );
        std::fs::write(&p, "garbage").unwrap();
        assert_eq!(Sidecar::read(&p), None);
    }
}
