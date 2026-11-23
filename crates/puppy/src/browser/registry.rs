//! HKCU native messaging host registration (Windows).

use std::io;
use std::path::{Path, PathBuf};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

/// Points `key`'s default value at `manifest`, creating the key.
pub fn set(key: &str, manifest: &Path) -> io::Result<()> {
    let (k, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(key)?;
    k.set_value("", &manifest.as_os_str())
}

/// The manifest path `key` points at, or `None` if the key (or its value) is absent.
pub fn get(key: &str) -> io::Result<Option<PathBuf>> {
    let k = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_READ) {
        Ok(k) => k,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match k.get_value::<std::ffi::OsString, _>("") {
        Ok(v) => Ok(Some(PathBuf::from(v))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Deletes `key`; whether it existed.
pub fn delete(key: &str) -> io::Result<bool> {
    match RegKey::predef(HKEY_CURRENT_USER).delete_subkey(key) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
