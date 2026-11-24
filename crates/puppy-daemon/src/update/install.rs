//! Downloading, verifying and swapping in a release.

use super::release::{Release, UpdateError};
use semver::Version;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const BIN_NAME: &str = if cfg!(windows) { "puppy.exe" } else { "puppy" };
/// The largest checksum file we read (a real one is ~100 bytes).
const MAX_CHECKSUM_BYTES: usize = 4096;

fn failed(msg: impl Into<String>) -> UpdateError {
    UpdateError::Failed(msg.into())
}

fn io_failed(what: &str, e: std::io::Error) -> UpdateError {
    failed(format!("{what}: {e}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Downloads the release into `work_dir` (emptied first), verifies its SHA-256, unpacks
/// it and returns the path of the new binary.
pub async fn fetch(
    http: &reqwest::Client,
    release: &Release,
    work_dir: &Path,
) -> Result<PathBuf, UpdateError> {
    if work_dir.exists() {
        std::fs::remove_dir_all(work_dir)
            .map_err(|e| io_failed("could not clear the update folder", e))?;
    }
    std::fs::create_dir_all(work_dir)
        .map_err(|e| io_failed("could not create the update folder", e))?;

    let expected = download_checksum(http, release).await?;

    let archive_path = work_dir.join(&release.archive.name);
    let mut resp = http
        .get(release.archive.url.clone())
        .send()
        .await
        .map_err(|e| {
            failed(format!(
                "could not download {}: {}",
                release.archive.name,
                e.without_url()
            ))
        })?;
    if !resp.status().is_success() {
        return Err(failed(format!(
            "downloading {} answered {}",
            release.archive.name,
            resp.status()
        )));
    }
    let mut file = std::fs::File::create(&archive_path)
        .map_err(|e| io_failed("could not write the download", e))?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        failed(format!(
            "download of {} broke off: {}",
            release.archive.name,
            e.without_url()
        ))
    })? {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| io_failed("could not write the download", e))?;
    }
    file.flush()
        .map_err(|e| io_failed("could not write the download", e))?;
    drop(file);

    let actual = hex(&hasher.finalize());
    if actual != expected {
        return Err(failed(format!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            release.archive.name
        )));
    }

    let name = release.archive.name.clone();
    let dest = work_dir.join(BIN_NAME);
    let out = dest.clone();
    tokio::task::spawn_blocking(move || extract(&archive_path, &name, &out))
        .await
        .map_err(|e| failed(format!("unpacking failed: {e}")))??;
    Ok(dest)
}

async fn download_checksum(
    http: &reqwest::Client,
    release: &Release,
) -> Result<String, UpdateError> {
    let name = &release.checksum.name;
    let resp = http
        .get(release.checksum.url.clone())
        .send()
        .await
        .map_err(|e| failed(format!("could not download {name}: {}", e.without_url())))?;
    if !resp.status().is_success() {
        return Err(failed(format!(
            "downloading {name} answered {}",
            resp.status()
        )));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| failed(format!("could not read {name}: {}", e.without_url())))?;
    if body.len() > MAX_CHECKSUM_BYTES {
        return Err(failed(format!("{name} is too large to be a checksum file")));
    }
    // `<hex>  <name>` or `<hex> *<name>`.
    let text = String::from_utf8_lossy(&body);
    let first = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if first.len() != 64 || !first.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failed(format!("{name} does not hold a SHA-256 checksum")));
    }
    Ok(first)
}

/// Writes the one expected binary from the archive to `dest`. Every other entry is
/// skipped, and an entry's own path is never used as a destination.
fn extract(archive: &Path, archive_name: &str, dest: &Path) -> Result<(), UpdateError> {
    let (stem, zip) = match archive_name.strip_suffix(".tar.gz") {
        Some(s) => (s, false),
        None => match archive_name.strip_suffix(".zip") {
            Some(s) => (s, true),
            None => return Err(failed(format!("unknown archive type: {archive_name}"))),
        },
    };
    let bin = if zip { "puppy.exe" } else { "puppy" };
    let wanted = format!("{stem}/{bin}");
    let file =
        std::fs::File::open(archive).map_err(|e| io_failed("could not open the download", e))?;

    if zip {
        let mut z = zip::ZipArchive::new(file).map_err(|e| failed(format!("bad archive: {e}")))?;
        for i in 0..z.len() {
            let mut entry = z
                .by_index(i)
                .map_err(|e| failed(format!("bad archive: {e}")))?;
            if entry.is_dir() || entry.name().map(|n| n != wanted.as_str()).unwrap_or(true) {
                continue;
            }
            return write_binary(&mut entry, dest);
        }
    } else {
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in t
            .entries()
            .map_err(|e| failed(format!("bad archive: {e}")))?
        {
            let mut entry = entry.map_err(|e| failed(format!("bad archive: {e}")))?;
            if !entry.header().entry_type().is_file()
                || entry.path_bytes().as_ref() != wanted.as_bytes()
            {
                continue;
            }
            return write_binary(&mut entry, dest);
        }
    }
    Err(failed(format!("{wanted} is missing from the archive")))
}

fn write_binary(from: &mut impl Read, dest: &Path) -> Result<(), UpdateError> {
    let mut out =
        std::fs::File::create(dest).map_err(|e| io_failed("could not write the new binary", e))?;
    std::io::copy(from, &mut out).map_err(|e| io_failed("could not unpack the new binary", e))?;
    out.flush()
        .map_err(|e| io_failed("could not unpack the new binary", e))?;
    drop(out);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_failed("could not make the new binary executable", e))?;
    }
    Ok(())
}

/// Runs `bin --version` (10 s limit); stdout must be exactly `puppy <version>` (trimmed).
pub async fn sanity_check(bin: &Path, version: &Version) -> Result<(), UpdateError> {
    let run = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .map_err(|_| failed("the new puppy did not answer --version within 10 seconds"))?
        .map_err(|e| failed(format!("could not run the new puppy: {e}")))?;
    if !out.status.success() {
        return Err(failed(format!(
            "the new puppy exited with {} on --version",
            out.status
        )));
    }
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let want = format!("puppy {version}");
    if said != want {
        return Err(failed(format!(
            "the new puppy reported {said:?}, expected {want:?}"
        )));
    }
    Ok(())
}

/// Atomically puts `new` at `exe` (mode 0755 on Unix). Unix: copy next to `exe`, rename
/// over it. Windows: rename the running `exe` to `<exe>.old`, then move `new` in.
pub fn replace_exe(new: &Path, exe: &Path) -> Result<(), UpdateError> {
    let dir = exe
        .parent()
        .ok_or_else(|| failed("the puppy executable has no folder"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let staged = dir.join(".puppy-new");
        let result = std::fs::copy(new, &staged)
            .and_then(|_| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
            .and_then(|_| std::fs::rename(&staged, exe));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&staged);
            return Err(io_failed("could not install the new puppy", e));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let old = old_path(exe);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)
            .map_err(|e| io_failed("could not move the running puppy aside", e))?;
        let staged = dir.join(".puppy-new");
        let put = std::fs::copy(new, &staged).and_then(|_| std::fs::rename(&staged, exe));
        if let Err(e) = put {
            let _ = std::fs::remove_file(&staged);
            let _ = std::fs::rename(&old, exe);
            return Err(io_failed("could not install the new puppy", e));
        }
        Ok(())
    }
}

#[cfg(not(unix))]
fn old_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

/// Deletes a leftover `<exe>.old` (Windows; nothing happens elsewhere).
pub fn remove_old_exe(exe: &Path) {
    #[cfg(not(unix))]
    {
        let _ = std::fs::remove_file(old_path(exe));
    }
    #[cfg(unix)]
    let _ = exe;
}

/// Why this binary cannot replace itself, or `None`. Checks the release-build flag
/// (unless `allow_dev`), then that `exe`'s folder accepts a new file.
pub fn cannot_install_reason(exe: &Path, allow_dev: bool) -> Option<String> {
    if !super::RELEASE_BUILD && !allow_dev {
        return Some("this build was not made by puppy's release workflow".to_string());
    }
    let dir = exe.parent()?;
    let probe = dir.join(format!(".puppy-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(_) => Some(format!(
            "puppy is installed in a folder it cannot write to ({}); rerun the install command to update",
            dir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in files {
            z.start_file(*name, opts).unwrap();
            z.write_all(body).unwrap();
        }
        z.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn zip_extracts_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("puppy-1.0.0-x86_64-pc-windows-msvc.zip");
        std::fs::write(
            &archive,
            zip_with(&[("puppy-1.0.0-x86_64-pc-windows-msvc/puppy.exe", b"bin")]),
        )
        .unwrap();
        let dest = dir.path().join("out");
        extract(&archive, "puppy-1.0.0-x86_64-pc-windows-msvc.zip", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"bin");
    }

    #[test]
    fn zip_without_the_binary_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        let name = "puppy-1.0.0-t-windows.zip";
        std::fs::write(&archive, zip_with(&[("../evil", b"x")])).unwrap();
        let dest = dir.path().join("out");
        let err = extract(&archive, name, &dest).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        assert!(!dest.exists());
    }
}
