//! Filesystem helpers. Platform-specific code is confined to small `cfg`-gated functions.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_NAME_BYTES: usize = 200;

/// Reduces untrusted input to a safe single path component.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        return "download".to_string();
    }
    truncate_name(cleaned, MAX_NAME_BYTES)
}

/// Truncates to `max` bytes on a char boundary, preserving a short extension.
fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => name.split_at(i),
        _ => (name, ""),
    };
    let mut end = max - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &stem[..end], ext)
}

/// Returns a path in `dir` for `name` that collides with neither `name` nor `name.part`.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let taken = |n: &str| dir.join(n).exists() || dir.join(format!("{n}.part")).exists();
    if !taken(name) {
        return dir.join(name);
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    (1u32..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !taken(c))
        .map(|c| dir.join(c))
        .expect("counter exhausted")
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replaces `path` with `bytes`: temp file in the same dir, fsync, rename,
/// then fsync the directory (Unix). `private` makes the file mode 0600 on Unix.
pub async fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let path = path.to_path_buf();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || atomic_write_blocking(&path, &bytes, private))
        .await
        .map_err(io::Error::other)?
}

fn atomic_write_blocking(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);

    let result = (|| {
        let mut file = create_tmp(&tmp, private)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file); // Windows: no open handle may remain during rename.
        fs::rename(&tmp, path)?;
        sync_parent_dir(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn create_tmp(path: &Path, private: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    if private {
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(windows)]
fn create_tmp(path: &Path, _private: bool) -> io::Result<File> {
    // No mode is applied on Windows; per-user data dirs carry a user-only ACL.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Writes all of `buf` at `offset` (positional write; does not rely on the file cursor).
#[cfg(unix)]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
pub fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sanitize_filename_strips_traversal() {
        assert_eq!(sanitize_filename("../../.bashrc"), ".bashrc");
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a/b"), "b");
    }

    #[test]
    fn sanitize_filename_empty_and_dots() {
        for s in ["", ".", ".."] {
            assert_eq!(sanitize_filename(s), "download");
        }
    }

    #[test]
    fn sanitize_filename_truncates() {
        let raw = format!("{}.iso", "é".repeat(300));
        let out = sanitize_filename(&raw);
        assert!(out.len() <= 200);
        assert!(out.ends_with(".iso"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn unique_path_appends_counter() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(d.path(), "a.iso"), d.path().join("a.iso"));
        fs::write(d.path().join("a.iso"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.iso"), d.path().join("a (1).iso"));
        fs::write(d.path().join("a (1).iso.part"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.iso"), d.path().join("a (2).iso"));
    }

    #[tokio::test]
    async fn atomic_write_replaces() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        atomic_write(&p, b"one", false).await.unwrap();
        atomic_write(&p, b"two", false).await.unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_sets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        atomic_write(&p, b"x", true).await.unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn write_at_offsets() {
        let d = tempfile::tempdir().unwrap();
        let f = fs::File::create(d.path().join("f")).unwrap();
        f.set_len(4).unwrap();
        write_at(&f, b"bb", 2).unwrap();
        write_at(&f, b"aa", 0).unwrap();
        assert_eq!(fs::read(d.path().join("f")).unwrap(), b"aabb");
    }
}
