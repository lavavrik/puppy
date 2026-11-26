//! Fast resume: the pieces a torrent had when the engine last stopped cleanly, so that a
//! restart does not check all of its data on disk again.
//!
//! librqbit 9.1 loads a torrent's have-pieces bitmap only through its session
//! persistence store (its `BitVFactory` is not public). The engine runs librqbit's JSON
//! store in `<data_dir>/torrent/fastresume/` for the bitmaps alone
//! (`<info hash>.bitv`): everything else librqbit keeps there (its torrent list
//! `session.json`, its own `.torrent` copies) is deleted before every session start
//! ([`prepare_dir`]), so librqbit never restores a torrent by itself and puppy's own
//! store stays the source of truth.
//!
//! A bitmap is only used when puppy vouches for it:
//! - At a clean shutdown, once the session has stopped (nothing writes any more), the
//!   engine writes each torrent's exact piece map to its `.bitv` itself (librqbit
//!   flushes its own only every 16 MiB), then a [`Record`] of the files as they are
//!   now: `<data_dir>/torrents/<id>.resume` ([`save`]).
//! - Before any add to librqbit the bitmap is deleted, unless the add restores the
//!   download and its record matches the torrent and every file on disk (size and
//!   modification time) exactly ([`admit`]). The record is deleted then either way: it
//!   vouches for one restart only, so after a crash the torrent is checked in full.
//! - Then up to [`SPOT_CHECKS`] of the pieces the bitmap claims are hashed (the first
//!   one of each file, then random others; [`spot_check`]): one bad piece and the data
//!   is checked in full. librqbit means to do the same, but its check only notices
//!   pieces it cannot read: a piece whose hash does not match passes it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use librqbit::torrent_from_bytes;
use librqbit_sha1_wrapper::{ISha1, Sha1};
use puppy_core::fsutil;
use serde::{Deserialize, Serialize};

use crate::source::TorrentInfo;

/// The folder of librqbit's persistence store (only its bitmaps are kept).
pub(crate) fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("torrent").join("fastresume")
}

/// The bitmap of torrent `info_hash` (lowercase hex, as librqbit names it).
pub(crate) fn bitv_path(dir: &Path, info_hash: &str) -> PathBuf {
    dir.join(format!("{info_hash}.bitv"))
}

/// One file of the torrent as it was on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    mtime_ns: u64,
}

/// What a stored bitmap was written against: the torrent, where its files are and each
/// file's size and modification time (`None`: not there).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    info_hash: String,
    output_folder: PathBuf,
    files: Vec<Option<Stamp>>,
}

impl Record {
    /// The torrent's files in `output_folder` now; `None` if one of them cannot be
    /// vouched for (not a regular file, no modification time, unreadable).
    pub(crate) fn take(info: &TorrentInfo, output_folder: &Path) -> Option<Record> {
        let files = info
            .files
            .iter()
            .map(|rel| stamp(&output_folder.join(rel)))
            .collect::<Option<Vec<_>>>()?;
        Some(Record {
            info_hash: info.info_hash.clone(),
            output_folder: output_folder.to_owned(),
            files,
        })
    }

    pub(crate) fn read(path: &Path) -> Option<Record> {
        let text = std::fs::read(path).ok()?;
        match serde_json::from_slice(&text) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(path = %path.display(), "ignoring an unreadable resume record: {e}");
                None
            }
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a resume record serializes")
    }
}

/// `Some(None)`: no file there; `None`: something that cannot be vouched for.
fn stamp(path: &Path) -> Option<Option<Stamp>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    if !meta.file_type().is_file() {
        return None;
    }
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Some(Stamp {
        len: meta.len(),
        mtime_ns: u64::try_from(since.as_nanos()).ok()?,
    }))
}

/// Removes `path`; already gone is fine.
fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A restored download about to be added to librqbit.
pub(crate) struct Restored<'a> {
    pub info: &'a TorrentInfo,
    /// The `.torrent` (its piece hashes).
    pub torrent: &'a [u8],
    pub output_folder: &'a Path,
}

/// Just before a torrent is added to librqbit: keeps its stored bitmap `bitv` only for
/// a restored download whose record (at `record`) matches it and its files on disk,
/// and whose pieces pass the [`spot_check`]; otherwise deletes the bitmap, so librqbit
/// checks the data in full. Deletes the record in any case (used once). Returns whether
/// the bitmap is kept. Blocking (one `stat` per file, a few pieces read).
///
/// An error (the bitmap could not be deleted) must fail the add: librqbit would use a
/// bitmap nothing vouches for.
pub(crate) fn admit(
    bitv: &Path,
    record: &Path,
    restored: Option<Restored<'_>>,
) -> io::Result<bool> {
    let vouched = restored.as_ref().is_some_and(|r| {
        Record::read(record)
            .is_some_and(|rec| Record::take(r.info, r.output_folder).as_ref() == Some(&rec))
    });
    if let Err(e) = remove(record) {
        tracing::warn!(path = %record.display(), "removing a used resume record: {e}");
    }
    if let (true, Some(r)) = (vouched, &restored) {
        match std::fs::read(bitv) {
            Ok(bits) if spot_check(r.torrent, r.output_folder, &bits) => return Ok(true),
            Ok(_) => tracing::warn!(
                torrent = %r.info.name,
                "a piece recorded at shutdown is bad: checking all the data"
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => tracing::warn!(path = %bitv.display(), "reading the stored pieces: {e}"),
        }
    }
    remove(bitv)?;
    Ok(false)
}

/// How many of the pieces a stored bitmap claims are hashed before it is used.
pub(crate) const SPOT_CHECKS: usize = 64;

/// Hashes up to [`SPOT_CHECKS`] of the pieces `bits` (MSB first) claims, read from the
/// torrent's files in `output_folder`: the first claimed piece of each file (up to half
/// of them), then random others. False if one does not match or cannot be read, or the
/// bitmap does not fit the torrent.
pub(crate) fn spot_check(torrent: &[u8], output_folder: &Path, bits: &[u8]) -> bool {
    check(torrent, output_folder, bits, true)
}

/// Whether every piece `bits` claims lies inside its files as they are on disk (each
/// file it overlaps is at least that long), and (`hash`) the [`spot_check`] passes. A
/// file cut short while the daemon ran would otherwise be padded with zeros by librqbit
/// and those zeros served as the claimed pieces.
fn check(torrent: &[u8], output_folder: &Path, bits: &[u8], hash: bool) -> bool {
    let Ok(meta) = torrent_from_bytes(torrent) else {
        return false;
    };
    let Ok(info) = meta.info.data.validate() else {
        return false;
    };
    let lengths = *info.lengths();
    let pieces = lengths.total_pieces();
    if bits.len() != lengths.piece_bitfield_bytes() {
        return false;
    }
    let has = |p: u32| bits[(p / 8) as usize] & (0x80 >> (p % 8)) != 0;
    // `(path, offset, length)` in torrent order; padding files (zeros): no path.
    let mut files = Vec::new();
    let mut offset = 0u64;
    for f in info.iter_file_details() {
        let path = (!f.attrs().padding).then(|| output_folder.join(f.filename.to_pathbuf()));
        files.push((path, offset, f.len));
        offset += f.len;
    }
    let plen = u64::from(lengths.default_piece_length());
    let total = lengths.total_length();
    let claimed: Vec<u32> = (0..pieces).filter(|&p| has(p)).collect();
    // Files and pieces both in torrent order: one pass over the files.
    let mut sizes: HashMap<usize, u64> = HashMap::new();
    let mut first = 0;
    for &p in &claimed {
        let start = u64::from(p) * plen;
        let end = (start + plen).min(total);
        while first < files.len() && files[first].1 + files[first].2 <= start {
            first += 1;
        }
        for (i, (path, off, len)) in files.iter().enumerate().skip(first) {
            if *off >= end {
                break;
            }
            let (Some(path), true) = (path, *len > 0) else {
                continue;
            };
            let size = match sizes.get(&i) {
                Some(&n) => n,
                None => match std::fs::metadata(path) {
                    Ok(m) => *sizes.entry(i).or_insert(m.len()),
                    Err(_) => return false,
                },
            };
            if size < end.min(off + len) - off {
                return false;
            }
        }
    }
    if !hash {
        return true;
    }
    let mut picked = BTreeSet::new();
    for &(ref path, off, len) in &files {
        if picked.len() >= SPOT_CHECKS / 2 {
            break;
        }
        if path.is_none() || len == 0 {
            continue;
        }
        let range = (off / plen) as u32..=((off + len - 1) / plen) as u32;
        if let Some(p) = range.into_iter().find(|&p| has(p)) {
            picked.insert(p);
        }
    }
    let mut x = RandomState::new().hash_one(0u8) | 1;
    for _ in 0..claimed.len().min(SPOT_CHECKS * 4) {
        if picked.len() >= SPOT_CHECKS.min(claimed.len()) {
            break;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        picked.insert(claimed[(x % claimed.len() as u64) as usize]);
    }
    let mut open: HashMap<usize, File> = HashMap::new();
    picked.into_iter().all(|p| {
        let Some(valid) = lengths.validate_piece_index(p) else {
            return false;
        };
        let start = lengths.piece_offset(valid);
        let end = start + u64::from(lengths.piece_length(valid));
        let mut h = Sha1::new();
        let mut buf = vec![0u8; 64 * 1024];
        for (i, (path, off, len)) in files.iter().enumerate() {
            let (from, to) = (start.max(*off), end.min(off + len));
            if from >= to {
                continue;
            }
            let mut left = (to - from) as usize;
            let file = match path {
                None => None,
                Some(path) => {
                    if let std::collections::hash_map::Entry::Vacant(v) = open.entry(i) {
                        match File::open(path) {
                            Ok(f) => {
                                v.insert(f);
                            }
                            Err(_) => return false,
                        }
                    }
                    let f = open.get_mut(&i).expect("opened above");
                    if f.seek(SeekFrom::Start(from - off)).is_err() {
                        return false;
                    }
                    Some(f)
                }
            };
            let mut file = file;
            while left > 0 {
                let chunk = left.min(buf.len());
                match &mut file {
                    None => buf[..chunk].fill(0),
                    Some(f) => {
                        if f.read_exact(&mut buf[..chunk]).is_err() {
                            return false;
                        }
                    }
                }
                h.update(&buf[..chunk]);
                left -= chunk;
            }
        }
        info.info().compare_hash(p, h.finish()) == Some(true)
    })
}

/// What a clean shutdown stores for one torrent (the session stopped: librqbit writes
/// nothing any more).
pub(crate) struct Save {
    pub bitv: PathBuf,
    pub record: PathBuf,
    /// The exact piece map (MSB first, byte padded: librqbit's bitmap format).
    pub bits: Vec<u8>,
    /// The `.torrent`.
    pub torrent: bytes::Bytes,
    pub info: std::sync::Arc<TorrentInfo>,
    pub output_folder: PathBuf,
    /// A time by which every write librqbit made to the files was done (`None`: not
    /// known).
    pub wrote_ns: Option<u64>,
}

impl Save {
    /// Writes the piece map to `bitv`, then the record of the files to `record`, both
    /// atomically and durably, in that order (the record vouches for the map), if the
    /// files can be vouched for ([`vouch`]); otherwise no record (a full check after the
    /// restart).
    pub(crate) async fn run(self) -> io::Result<()> {
        let Save {
            bitv,
            record,
            bits,
            torrent,
            info,
            output_folder,
            wrote_ns,
        } = self;
        let claimed = bits.clone();
        let rec = tokio::task::spawn_blocking(move || {
            vouch(&torrent, &info, &output_folder, &claimed, wrote_ns)
        })
        .await
        .map_err(io::Error::other)?;
        // An old record never vouches for the new map, even half-written.
        remove(&record)?;
        let Some(rec) = rec else {
            return Ok(());
        };
        fsutil::atomic_write(&bitv, &bits, true).await?;
        fsutil::atomic_write(&record, &rec.to_bytes(), true).await
    }
}

/// The record of torrent `info`'s files in `output_folder`, if they can vouch for the
/// claimed pieces `bits`: each claimed piece lies inside its files ([`check`]), every
/// file's data is on disk (synced: librqbit never syncs it, and a power loss could
/// otherwise keep a recorded modification time over lost data), and no file was
/// modified after `wrote_ns` (librqbit's last write: anything later is someone
/// else's, for example a file rewritten in place while it was seeding). Blocking.
fn vouch(
    torrent: &[u8],
    info: &TorrentInfo,
    output_folder: &Path,
    bits: &[u8],
    wrote_ns: Option<u64>,
) -> Option<Record> {
    if !check(torrent, output_folder, bits, false) {
        tracing::warn!(torrent = %info.name, "a file is shorter than the pieces it had");
        return None;
    }
    for rel in &info.files {
        let path = output_folder.join(rel);
        // Windows needs write access to flush a file; opening it changes nothing.
        let mut opts = std::fs::OpenOptions::new();
        if cfg!(windows) {
            opts.write(true);
        } else {
            opts.read(true);
        }
        match opts.open(&path).and_then(|f| f.sync_data()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), "syncing for a fast resume: {e}");
                return None;
            }
        }
    }
    let rec = Record::take(info, output_folder)?;
    let wrote = wrote_ns?;
    if rec.files.iter().flatten().any(|s| s.mtime_ns > wrote) {
        tracing::warn!(
            torrent = %info.name,
            "a file changed after the torrent last wrote to it: checking it in full next time"
        );
        return None;
    }
    Some(rec)
}

/// Before the session starts: creates the store's folder (owner-only, like the engine's
/// other dirs) and deletes everything in it but the bitmaps a record in `records_dir`
/// (`<data_dir>/torrents/`) vouches for. Fails if librqbit's torrent list cannot be
/// deleted (librqbit would restore those torrents itself).
pub(crate) fn prepare_dir(dir: &Path, records_dir: &Path) -> io::Result<()> {
    crate::engine::create_private_dir(dir)?;
    let vouched: HashSet<String> = match std::fs::read_dir(records_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "resume"))
            .filter_map(|p| Record::read(&p))
            .map(|r| r.info_hash)
            .collect(),
        Err(_) => HashSet::new(),
    };
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let keep = name
            .strip_suffix(".bitv")
            .is_some_and(|hash| vouched.contains(hash));
        if keep || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        match remove(&path) {
            Ok(()) => {}
            Err(e) if name.starts_with("session.json") => return Err(e),
            Err(e) => tracing::warn!(path = %path.display(), "removing stale resume data: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection;

    /// Piece length of the test torrents (the smallest librqbit takes).
    const PLEN: usize = 16 * 1024;

    /// Bencodes a multi-file torrent `t` of `files` (`(path, content)`), with the real
    /// piece hashes.
    fn torrent(files: &[(&str, &[u8])]) -> Vec<u8> {
        fn bstr(out: &mut Vec<u8>, s: &[u8]) {
            out.extend_from_slice(format!("{}:", s.len()).as_bytes());
            out.extend_from_slice(s);
        }
        let data: Vec<u8> = files.iter().flat_map(|f| f.1.iter().copied()).collect();
        let mut out = b"d4:infod5:filesl".to_vec();
        for (path, content) in files {
            out.extend_from_slice(format!("d6:lengthi{}e4:pathl", content.len()).as_bytes());
            for c in path.split('/') {
                bstr(&mut out, c.as_bytes());
            }
            out.extend_from_slice(b"ee");
        }
        out.extend_from_slice(b"e4:name1:t");
        out.extend_from_slice(format!("12:piece lengthi{PLEN}e").as_bytes());
        let mut pieces = Vec::new();
        for chunk in data.chunks(PLEN) {
            let mut h = Sha1::new();
            h.update(chunk);
            pieces.extend_from_slice(&h.finish());
        }
        bstr(&mut out, b"pieces");
        bstr(&mut out, &pieces);
        out.extend_from_slice(b"ee");
        out
    }

    /// Two files: `a` (1.5 pieces) and `sub/b` (1 piece and a bit): 3 pieces, the
    /// second shared by both.
    struct Fixture {
        dir: tempfile::TempDir,
        out: PathBuf,
        bytes: Vec<u8>,
        info: TorrentInfo,
        bitv: PathBuf,
        rec: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let a: Vec<u8> = (0..PLEN * 3 / 2).map(|i| (i % 251) as u8).collect();
            let b: Vec<u8> = (0..PLEN + 100).map(|i| (i % 241) as u8).collect();
            let bytes = torrent(&[("a", &a), ("sub/b", &b)]);
            let info = crate::source::parse_torrent(&bytes).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("out");
            std::fs::create_dir_all(out.join("sub")).unwrap();
            std::fs::write(out.join("a"), &a).unwrap();
            std::fs::write(out.join("sub/b"), &b).unwrap();
            let (bitv, rec) = (dir.path().join("x.bitv"), dir.path().join("1.resume"));
            Fixture {
                dir,
                out,
                bytes,
                info,
                bitv,
                rec,
            }
        }

        /// A bitmap claiming every piece, and a record of the files as they are.
        fn record(&self) {
            std::fs::write(&self.bitv, [0xE0]).unwrap();
            let rec = Record::take(&self.info, &self.out).unwrap();
            std::fs::write(&self.rec, rec.to_bytes()).unwrap();
        }

        fn restored(&self) -> Option<Restored<'_>> {
            Some(Restored {
                info: &self.info,
                torrent: &self.bytes,
                output_folder: &self.out,
            })
        }

        fn admit(&self) -> bool {
            admit(&self.bitv, &self.rec, self.restored()).unwrap()
        }

        /// Overwrites byte `at` of `rel`, keeping its modification time.
        fn corrupt(&self, rel: &str, at: usize) {
            let path = self.out.join(rel);
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let mut data = std::fs::read(&path).unwrap();
            data[at] ^= 0xFF;
            std::fs::write(&path, &data).unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_modified(mtime).unwrap();
        }
    }

    #[test]
    fn admit_keeps_a_vouched_bitmap_once() {
        let f = Fixture::new();
        f.record();
        assert!(f.admit());
        assert!(
            f.bitv.exists() && !f.rec.exists(),
            "kept; the record is used up"
        );
        // No record any more: dropped.
        assert!(!f.admit());
        assert!(!f.bitv.exists());
    }

    #[test]
    fn admit_drops_the_bitmap_when_anything_differs() {
        let f = Fixture::new();
        let gone = |f: &Fixture| !f.bitv.exists() && !f.rec.exists();

        // A fresh add never trusts a bitmap.
        f.record();
        assert!(!admit(&f.bitv, &f.rec, None).unwrap());
        assert!(gone(&f));
        // Another folder.
        f.record();
        let elsewhere = Restored {
            output_folder: f.dir.path(),
            ..f.restored().unwrap()
        };
        assert!(!admit(&f.bitv, &f.rec, Some(elsewhere)).unwrap());
        assert!(gone(&f));
        // Another torrent.
        f.record();
        let mut other = f.info.clone();
        other.info_hash = "cd".repeat(20);
        let other = Restored {
            info: &other,
            ..f.restored().unwrap()
        };
        assert!(!admit(&f.bitv, &f.rec, Some(other)).unwrap());
        assert!(gone(&f));
        // A file changed size.
        f.record();
        std::fs::write(f.out.join("sub/b"), b"short").unwrap();
        assert!(!f.admit());
        assert!(gone(&f));
        // A file is gone.
        f.record();
        std::fs::remove_file(f.out.join("sub/b")).unwrap();
        assert!(!f.admit());
        // A garbled record.
        f.record();
        std::fs::write(&f.rec, b"{").unwrap();
        assert!(!f.admit());
        assert!(gone(&f));
    }

    #[test]
    fn admit_drops_a_bitmap_with_a_bad_piece() {
        let f = Fixture::new();
        // Size and modification time as recorded, content not.
        f.record();
        f.corrupt("a", 10);
        assert!(!f.admit(), "piece 0 is checked: the first of a");
        assert!(!f.bitv.exists());
    }

    #[test]
    fn spot_check_hashes_the_claimed_pieces() {
        let f = Fixture::new();
        let check = |bits: &[u8]| spot_check(&f.bytes, &f.out, bits);
        assert!(check(&[0xE0]));
        assert!(check(&[0x00]), "nothing claimed, nothing to check");
        assert!(!check(&[0xE0, 0]), "a bitmap of another size");
        // Piece 1 spans both files: its part in sub/b is corrupt.
        f.corrupt("sub/b", 5);
        assert!(!check(&[0xE0]));
        assert!(!check(&[0x40]));
        assert!(check(&[0xA0]), "pieces 0 and 2 are fine");
        std::fs::remove_file(f.out.join("a")).unwrap();
        assert!(!check(&[0x80]), "unreadable");
    }

    #[test]
    fn claimed_pieces_must_lie_inside_the_files() {
        let f = Fixture::new();
        let fits = |bits: &[u8]| check(&f.bytes, &f.out, bits, false);
        assert!(fits(&[0xE0]));
        // sub/b loses its last bytes: piece 2 (its tail) no longer fits, pieces 0 and 1
        // (a, and a's tail with b's head) still do.
        let b = f.out.join("sub/b");
        let len = std::fs::metadata(&b).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&b)
            .unwrap()
            .set_len(len - 1)
            .unwrap();
        assert!(!fits(&[0xE0]));
        assert!(!fits(&[0x20]));
        assert!(
            fits(&[0xC0]),
            "the shortened file is not claimed past its end"
        );
        // The hashes alone could miss it: the size check sees every claimed piece.
        assert!(!spot_check(&f.bytes, &f.out, &[0x20]));
    }

    #[test]
    fn vouch_needs_fitting_files_unchanged_since_the_last_write() {
        let f = Fixture::new();
        let later = selection::now_ns() + 1_000_000_000;
        let vouch = |wrote| vouch(&f.bytes, &f.info, &f.out, &[0xE0], wrote);
        assert!(vouch(Some(later)).is_some());
        assert!(vouch(None).is_none(), "librqbit's last write unknown");
        // Written after librqbit's last write: by someone else.
        let earlier = selection::now_ns() - 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(f.out.join("a"), vec![7u8; PLEN * 3 / 2]).unwrap();
        assert!(vouch(Some(earlier)).is_none());
        assert!(vouch(Some(selection::now_ns())).is_some());
        // Cut short (whatever its modification time).
        std::fs::write(f.out.join("sub/b"), b"x").unwrap();
        assert!(vouch(Some(later)).is_none());
    }

    #[tokio::test]
    async fn save_writes_no_record_it_cannot_vouch_for() {
        let f = Fixture::new();
        let save = |wrote_ns| Save {
            bitv: f.bitv.clone(),
            record: f.rec.clone(),
            bits: vec![0xE0],
            torrent: f.bytes.clone().into(),
            info: std::sync::Arc::new(f.info.clone()),
            output_folder: f.out.clone(),
            wrote_ns,
        };
        save(Some(selection::now_ns())).run().await.unwrap();
        assert!(f.rec.exists() && f.bitv.exists());
        assert!(f.admit());
        std::fs::write(&f.rec, b"stale").unwrap();
        save(None).run().await.unwrap();
        assert!(!f.rec.exists(), "an old record is not left behind");
    }

    #[test]
    fn a_modified_file_is_noticed() {
        let f = Fixture::new();
        let before = Record::take(&f.info, &f.out).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(f.out.join("a"))
            .unwrap();
        file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        assert_ne!(Record::take(&f.info, &f.out).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_be_vouched_for() {
        let f = Fixture::new();
        let a = f.out.join("a");
        std::fs::rename(&a, f.dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(f.dir.path().join("elsewhere"), &a).unwrap();
        assert!(Record::take(&f.info, &f.out).is_none());
    }

    #[test]
    fn prepare_dir_keeps_only_vouched_bitmaps() {
        let f = Fixture::new();
        let (store, records) = (f.dir.path().join("fr"), f.dir.path().join("torrents"));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&records).unwrap();
        let kept = bitv_path(&store, &f.info.info_hash);
        let orphan = bitv_path(&store, &"cd".repeat(20));
        for p in [&kept, &orphan] {
            std::fs::write(p, [0]).unwrap();
        }
        std::fs::write(store.join("session.json"), b"{}").unwrap();
        std::fs::write(store.join(format!("{}.torrent", f.info.info_hash)), b"x").unwrap();
        std::fs::write(
            records.join("3.resume"),
            Record::take(&f.info, &f.out).unwrap().to_bytes(),
        )
        .unwrap();

        prepare_dir(&store, &records).unwrap();
        let left: Vec<_> = std::fs::read_dir(&store)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(left, vec![kept]);
    }
}
