//! Filesystem helpers. Platform-specific code is confined to small `cfg`-gated functions.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_NAME_BYTES: usize = 200;

/// Reduces untrusted input to a single path component that is safe on every OS.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        return "download".to_string();
    }
    let name = if is_reserved_windows_name(cleaned) {
        format!("_{cleaned}")
    } else {
        cleaned.to_string()
    };
    truncate_name(&name, MAX_NAME_BYTES)
}

/// Windows device names (CON, NUL, COM1..9, ...) are reserved with or without an extension.
fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    let up = stem.to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((up.starts_with("COM") || up.starts_with("LPT"))
            && up.len() == 4
            && matches!(up.as_bytes()[3], b'1'..=b'9'))
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

/// Returns a path in `dir` for `name` that collides with none of `name`, `name.part` and
/// `name.part.puppy`.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let taken = |n: &str| {
        [
            n.to_string(),
            format!("{n}.part"),
            format!("{n}.part.puppy"),
        ]
        .iter()
        .any(|c| fs::symlink_metadata(dir.join(c)).is_ok())
    };
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

/// fsyncs the directory containing `path` so a rename/create in it is durable (Unix only).
#[cfg(unix)]
pub fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

#[cfg(windows)]
pub fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Renames `from` to `to`, failing with `ErrorKind::AlreadyExists` instead of replacing
/// an existing `to` (e.g. a file the user created there meanwhile).
#[cfg(not(windows))]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_exclusive(from, to) {
        Err(e) if is_unsupported(&e) => rename_no_replace_fallback(from, to),
        r => r,
    }
}

/// Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`. Antivirus scanners and the
/// indexer briefly lock new files, so access errors are retried with backoff.
#[cfg(windows)]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (from_w, to_w) = (wide(from), wide(to));
    let mut delays = [100u64, 200, 400, 800, 1600].into_iter();
    loop {
        // SAFETY: both arguments are NUL-terminated UTF-16 strings that outlive the call.
        let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), 0) };
        if ok != 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match delays.next() {
            // 32 = ERROR_SHARING_VIOLATION, 33 = ERROR_LOCK_VIOLATION (scanner/indexer holds the file).
            Some(ms)
                if err.kind() == io::ErrorKind::PermissionDenied
                    || matches!(err.raw_os_error(), Some(32 | 33)) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            _ => return Err(err),
        }
    }
}

#[cfg(unix)]
fn c_path(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// Linux: `renameat2(RENAME_NOREPLACE)` (via `syscall`, so no minimum glibc is needed).
#[cfg(target_os = "linux")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (c_path(from)?, c_path(to)?);
    // SAFETY: valid NUL-terminated paths; AT_FDCWD resolves them like rename(2).
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// macOS: `renamex_np(RENAME_EXCL)`.
#[cfg(target_os = "macos")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (c_path(from)?, c_path(to)?);
    // SAFETY: valid NUL-terminated paths.
    let r = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_exclusive(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The exclusive rename is not available on this system or file system.
#[cfg(not(windows))]
fn is_unsupported(e: &io::Error) -> bool {
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return matches!(code, libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
            || code == libc::EOPNOTSUPP;
    }
    e.kind() == io::ErrorKind::Unsupported
}

/// Without an exclusive rename: `hard_link` (which never replaces) and remove `from`; if
/// the file system has no hard links, check for `to` and rename (a small race remains).
#[cfg(not(windows))]
fn rename_no_replace_fallback(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => {
            if let Err(e) = fs::remove_file(from) {
                tracing::warn!(path = %from.display(), "cannot remove after linking: {e}");
            }
            Ok(())
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
            ) =>
        {
            Err(e)
        }
        Err(_) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
    }
}

/// Marks a newly created file sparse so that writing far past its end does not first
/// zero-fill the gap (NTFS does that synchronously). A no-op where files are sparse anyway.
#[cfg(windows)]
pub fn mark_sparse(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    let mut returned = 0u32;
    // SAFETY: a valid open handle; no input/output buffers; synchronous call.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
pub fn mark_sparse(_file: &File) -> io::Result<()> {
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
    fn sanitize_filename_windows_invalid_chars() {
        assert_eq!(
            sanitize_filename("a<b>c:d\"e|f?g*h.txt"),
            "a_b_c_d_e_f_g_h.txt"
        );
        assert_eq!(sanitize_filename("name. . "), "name");
        assert_eq!(sanitize_filename("report.pdf  "), "report.pdf");
        assert_eq!(sanitize_filename("???"), "___");
        assert_eq!(sanitize_filename(" . "), "download");
    }

    #[test]
    fn sanitize_filename_reserved_names() {
        assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("com1"), "_com1");
        assert_eq!(sanitize_filename("Lpt9.tar.gz"), "_Lpt9.tar.gz");
        assert_eq!(sanitize_filename("aux."), "_aux");
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
        assert_eq!(sanitize_filename("com10"), "com10");
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
        // A leftover sidecar also claims its name.
        fs::write(d.path().join("a (2).iso.part.puppy"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.iso"), d.path().join("a (3).iso"));
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
    fn rename_no_replace_refuses_existing() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::write(&a, b"new").unwrap();
        fs::write(&b, b"user").unwrap();
        let err = rename_no_replace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&a).unwrap(), b"new");
        assert_eq!(fs::read(&b).unwrap(), b"user");
        fs::remove_file(&b).unwrap();
        rename_no_replace(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read(&b).unwrap(), b"new");
    }

    #[cfg(not(windows))]
    #[test]
    fn rename_no_replace_fallback_refuses_existing() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::write(&a, b"new").unwrap();
        fs::write(&b, b"user").unwrap();
        let err = rename_no_replace_fallback(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"user");
        fs::remove_file(&b).unwrap();
        rename_no_replace_fallback(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read(&b).unwrap(), b"new");
    }

    #[test]
    fn mark_sparse_ok() {
        let d = tempfile::tempdir().unwrap();
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(d.path().join("s"))
            .unwrap();
        mark_sparse(&f).unwrap();
        f.set_len(1 << 20).unwrap();
        write_at(&f, b"x", (1 << 20) - 1).unwrap();
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
