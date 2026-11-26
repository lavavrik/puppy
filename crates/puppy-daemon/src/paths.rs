//! Platform paths: data/config dirs, socket name, lock/state/log files.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketName {
    /// Unix domain socket path.
    Path(PathBuf),
    /// Windows named pipe name (without the `\\.\pipe\` prefix).
    Pipe(String),
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub socket: SocketName,
    pub lock_file: PathBuf,
    /// Holds the running daemon's process id (for tests and tooling). A daemon that
    /// crashed leaves it behind, so it names a live daemon only while `lock_file` is
    /// locked; never act on it (e.g. kill) without checking the lock.
    pub pid_file: PathBuf,
    pub state_file: PathBuf,
    pub log_dir: PathBuf,
    /// Where a downloaded update is staged.
    pub update_dir: PathBuf,
    /// Persisted self-update bookkeeping.
    pub update_state: PathBuf,
}

impl Paths {
    /// Resolve paths from the environment (`PUPPY_HOME`, `PUPPY_SOCKET`) and
    /// the platform defaults, creating the directories (mode 0700 on Unix).
    ///
    /// `PUPPY_SOCKET` is a socket file path on Unix and a bare pipe name on
    /// Windows (a leading `\\.\pipe\` is stripped). Without it, on Unix, the socket is
    /// `$XDG_RUNTIME_DIR/puppy/puppy.sock`, or `<data_dir>/puppy.sock` when there is
    /// no runtime dir or `PUPPY_HOME` is set (each home gets its own daemon); on Windows
    /// the pipe is `puppy-<user>`, or `puppy-<user>-<hash of PUPPY_HOME>`. Its parent directory is
    /// created if missing but its permissions are never changed. Unix socket
    /// paths longer than 100 bytes are rejected (`sun_path` limit); tests on
    /// macOS should therefore use short paths, e.g. under `/tmp`.
    pub fn resolve() -> io::Result<Self> {
        let home = std::env::var_os("PUPPY_HOME").filter(|v| !v.is_empty());
        let (data_dir, config_dir) = match &home {
            Some(home) => home_dirs(Path::new(home)),
            None => default_dirs()?,
        };
        let socket_override = std::env::var("PUPPY_SOCKET").ok().filter(|v| !v.is_empty());
        let overridden = socket_override.is_some();
        let socket = socket_name(socket_override, &data_dir, home.as_deref().map(Path::new));
        Self::build(data_dir, config_dir, socket, overridden)
    }

    /// Like [`resolve`](Self::resolve) with `PUPPY_HOME=home` and the given socket
    /// (treated as an override), without reading the environment. For tests and
    /// embedders running several daemons in one process.
    pub fn with_home(home: &Path, socket: SocketName) -> io::Result<Self> {
        let (data_dir, config_dir) = home_dirs(home);
        Self::build(data_dir, config_dir, socket, true)
    }

    fn build(
        data_dir: PathBuf,
        config_dir: PathBuf,
        socket: SocketName,
        socket_overridden: bool,
    ) -> io::Result<Self> {
        check_socket_len(&socket)?;
        create_private_dir(&data_dir)?;
        create_private_dir(&config_dir)?;
        if let SocketName::Path(p) = &socket
            && let Some(parent) = p.parent()
            && !parent.as_os_str().is_empty()
        {
            if socket_overridden {
                // User-chosen location: create if missing, never chmod.
                std::fs::create_dir_all(parent)?;
            } else {
                create_private_dir(parent)?;
            }
        }
        let log_dir = data_dir.join("logs");
        create_private_dir(&log_dir)?;

        Ok(Self {
            lock_file: data_dir.join("puppy.lock"),
            pid_file: data_dir.join("puppyd.pid"),
            state_file: data_dir.join("state.json"),
            update_dir: data_dir.join("update"),
            update_state: data_dir.join("update.json"),
            log_dir,
            data_dir,
            config_dir,
            socket,
        })
    }

    /// Path of the TOML config file.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// puppy's own `known_hosts` (hosts trusted with `puppy ssh trust`).
    pub fn known_hosts(&self) -> PathBuf {
        self.data_dir.join("known_hosts")
    }
}

fn no_home() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "cannot determine data directory")
}

fn home_dirs(home: &Path) -> (PathBuf, PathBuf) {
    (home.join("data"), home.join("config"))
}

fn default_dirs() -> io::Result<(PathBuf, PathBuf)> {
    let data = dirs::data_dir().ok_or_else(no_home)?.join("puppy");
    #[cfg(target_os = "linux")]
    let config = dirs::config_dir().ok_or_else(no_home)?.join("puppy");
    #[cfg(not(target_os = "linux"))]
    let config = data.clone();
    Ok((data, config))
}

/// `home`: `PUPPY_HOME`, if set; then the shared runtime-dir socket is not used
/// (a daemon of another home could be listening there).
#[cfg(unix)]
fn socket_name(over: Option<String>, data_dir: &Path, home: Option<&Path>) -> SocketName {
    SocketName::Path(match over {
        Some(p) => PathBuf::from(p),
        None => match dirs::runtime_dir().filter(|_| home.is_none()) {
            Some(r) => r.join("puppy").join("puppy.sock"),
            None => data_dir.join("puppy.sock"),
        },
    })
}

/// Windows: the per-user pipe name, per home when `PUPPY_HOME` is set (see
/// [`default_pipe_name`]).
#[cfg(windows)]
fn socket_name(over: Option<String>, _data_dir: &Path, home: Option<&Path>) -> SocketName {
    SocketName::Pipe(match over {
        Some(n) => normalize_pipe_name(&n).to_string(),
        None => default_pipe_name(std::env::var("USERNAME").ok().as_deref(), home),
    })
}

/// `puppy-<user>`, or `puppy-<user>-<8 hex digits>` for a custom home (`PUPPY_HOME`):
/// the digits are a stable hash of the home path, so daemons of different homes never
/// share a pipe (pipe names are global; a second daemon could not create the pipe while
/// the first one listens, as interprocess creates the first instance exclusively).
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn default_pipe_name(user: Option<&str>, home: Option<&Path>) -> String {
    let user = sanitize_username(user);
    match home {
        None => format!("puppy-{user}"),
        Some(home) => {
            let hash = fnv1a32(home.to_string_lossy().as_bytes());
            format!("puppy-{user}-{hash:08x}")
        }
    }
}

/// 32-bit FNV-1a: stable across Rust versions and platforms, unlike `DefaultHasher`.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// Strip a leading `\\.\pipe\` so overrides may be given in either form.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn normalize_pipe_name(name: &str) -> &str {
    name.strip_prefix(r"\\.\pipe\").unwrap_or(name)
}

/// Reject Unix socket paths that cannot fit in `sun_path`.
fn check_socket_len(socket: &SocketName) -> io::Result<()> {
    const MAX: usize = 100;
    #[cfg(unix)]
    if let SocketName::Path(p) = socket {
        use std::os::unix::ffi::OsStrExt;
        let n = p.as_os_str().as_bytes().len();
        if n > MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "socket path is {n} bytes (max {MAX}): {}; set PUPPY_SOCKET to a shorter path",
                    p.display()
                ),
            ));
        }
    }
    let _ = (socket, MAX);
    Ok(())
}

/// Keep `[A-Za-z0-9_-]`; fall back to "user" if nothing remains.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn sanitize_username(name: Option<&str>) -> String {
    let s: String = name
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if s.is_empty() { "user".into() } else { s }
}

#[cfg(unix)]
fn create_private_dir(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)?;
    // Tighten the leaf even if it pre-existed with looser permissions.
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(p: &Path) -> io::Result<()> {
    std::fs::create_dir_all(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ENV_LOCK;

    /// Run `f` with the given env vars set/unset, restoring afterwards.
    fn with_env<R>(vars: &[(&str, Option<&Path>)], f: impl FnOnce() -> R) -> R {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            // SAFETY: env-mutating tests are serialised by ENV_LOCK.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let r = f();
        for (k, v) in saved {
            // SAFETY: still holding ENV_LOCK.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        r
    }

    #[test]
    fn puppy_home_overrides_dirs() {
        let d = tempfile::tempdir().unwrap();
        let rt = d.path().join("rt");
        let p = with_env(
            &[
                ("PUPPY_HOME", Some(d.path())),
                ("PUPPY_SOCKET", None),
                ("XDG_RUNTIME_DIR", Some(&rt)),
            ],
            || Paths::resolve().unwrap(),
        );
        assert_eq!(p.data_dir, d.path().join("data"));
        assert_eq!(p.config_dir, d.path().join("config"));
        assert_eq!(p.lock_file, d.path().join("data/puppy.lock"));
        assert_eq!(p.state_file, d.path().join("data/state.json"));
        assert_eq!(p.log_dir, d.path().join("data/logs"));
        assert_eq!(p.update_dir, d.path().join("data/update"));
        assert_eq!(p.update_state, d.path().join("data/update.json"));
        assert!(p.data_dir.is_dir() && p.config_dir.is_dir() && p.log_dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(&p.data_dir).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o700);
        }
    }

    #[test]
    fn puppy_socket_override() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            let sock = d.path().join("s/x.sock");
            let p = with_env(
                &[
                    ("PUPPY_HOME", Some(d.path())),
                    ("PUPPY_SOCKET", Some(&sock)),
                ],
                || Paths::resolve().unwrap(),
            );
            assert_eq!(p.socket, SocketName::Path(sock.clone()));
            assert!(sock.parent().unwrap().is_dir());
        }
        #[cfg(windows)]
        {
            let p = with_env(
                &[
                    ("PUPPY_HOME", Some(d.path())),
                    ("PUPPY_SOCKET", Some(Path::new("testpipe"))),
                    ("XDG_RUNTIME_DIR", Some(d.path())),
                ],
                || Paths::resolve().unwrap(),
            );
            assert_eq!(p.socket, SocketName::Pipe("testpipe".into()));
        }
    }

    /// With `PUPPY_HOME` the socket lives in that home, never in the shared runtime dir.
    #[cfg(unix)]
    #[test]
    fn puppy_home_gets_its_own_socket() {
        let d = tempfile::tempdir().unwrap();
        let rt = d.path().join("rt");
        std::fs::create_dir(&rt).unwrap();
        let p = with_env(
            &[
                ("PUPPY_HOME", Some(d.path())),
                ("PUPPY_SOCKET", None),
                ("XDG_RUNTIME_DIR", Some(&rt)),
            ],
            || Paths::resolve().unwrap(),
        );
        assert_eq!(p.socket, SocketName::Path(d.path().join("data/puppy.sock")));
    }

    #[cfg(unix)]
    #[test]
    fn default_socket_in_runtime_dir() {
        let rt = tempfile::tempdir().unwrap();
        let data = Path::new("/nonexistent/data");
        let name = with_env(&[("XDG_RUNTIME_DIR", Some(rt.path()))], || {
            socket_name(None, data, None)
        });
        if cfg!(target_os = "linux") {
            assert_eq!(name, SocketName::Path(rt.path().join("puppy/puppy.sock")));
        }
        let name = with_env(&[("XDG_RUNTIME_DIR", Some(rt.path()))], || {
            socket_name(None, data, Some(Path::new("/home/x")))
        });
        assert_eq!(name, SocketName::Path(data.join("puppy.sock")));
    }

    #[cfg(unix)]
    #[test]
    fn socket_override_does_not_chmod_existing_dir() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let shared = d.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sock = shared.join("x.sock");
        with_env(
            &[
                ("PUPPY_HOME", Some(d.path())),
                ("PUPPY_SOCKET", Some(&sock)),
                ("XDG_RUNTIME_DIR", Some(d.path())),
            ],
            || Paths::resolve().unwrap(),
        );
        let m = std::fs::metadata(&shared).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn overlong_socket_path_rejected() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("a".repeat(120)).join("x.sock");
        let e = with_env(
            &[
                ("PUPPY_HOME", Some(d.path())),
                ("PUPPY_SOCKET", Some(&sock)),
                ("XDG_RUNTIME_DIR", Some(d.path())),
            ],
            || Paths::resolve().unwrap_err(),
        );
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("PUPPY_SOCKET"), "{e}");
    }

    #[test]
    fn pipe_name_prefix_stripped() {
        assert_eq!(normalize_pipe_name(r"\\.\pipe\foo"), "foo");
        assert_eq!(normalize_pipe_name("foo"), "foo");
    }

    #[test]
    fn username_sanitised() {
        assert_eq!(sanitize_username(Some("Jo Smith\\x!")), "JoSmithx");
        assert_eq!(sanitize_username(Some("a_b-c1")), "a_b-c1");
        assert_eq!(sanitize_username(Some("!!!")), "user");
        assert_eq!(sanitize_username(None), "user");
    }

    #[test]
    fn default_pipe_name_per_home() {
        assert_eq!(default_pipe_name(Some("jo"), None), "puppy-jo");
        let a = default_pipe_name(Some("jo"), Some(Path::new(r"C:\puppy\a")));
        let b = default_pipe_name(Some("jo"), Some(Path::new(r"C:\puppy\b")));
        assert_ne!(a, b, "homes must not share a pipe");
        let (prefix, hash) = a.rsplit_once('-').unwrap();
        assert_eq!(prefix, "puppy-jo");
        assert_eq!(hash.len(), 8);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // Stable: the same home always maps to the same pipe (across builds too).
        assert_eq!(
            a,
            default_pipe_name(Some("jo"), Some(Path::new(r"C:\puppy\a")))
        );
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(
            default_pipe_name(None, Some(Path::new("h"))),
            format!("puppy-user-{:08x}", fnv1a32(b"h"))
        );
    }
}
