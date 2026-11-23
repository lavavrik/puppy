//! Starting the daemon in the background, detached from the client's terminal.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Idle timeout (seconds) for a daemon started by a client, unless `PUPPY_IDLE_EXIT`
/// says otherwise.
const DEFAULT_IDLE_EXIT_SECS: u64 = 30;

/// Starts `exe daemon --idle-exit <PUPPY_IDLE_EXIT or 30>` detached from this process
/// (its own session on Unix; no console and its own process group on Windows), with
/// stdin/stdout/stderr null, in `cwd` (the daemon's data dir) if it is a directory,
/// else in the filesystem root, never in the caller's working directory (which it
/// would keep busy). `PUPPY_HOME` and `PUPPY_SOCKET` are forwarded when set, then
/// `extra_env` is applied; `PUPPY_IDLE_EXIT` is looked up in `extra_env` first.
/// Returns once the process is started; whether it becomes the daemon (or finds one
/// already running) is up to it.
pub fn spawn_daemon_detached(
    exe: &Path,
    cwd: Option<&Path>,
    extra_env: &[(&str, &str)],
) -> io::Result<()> {
    let idle_exit = extra_env
        .iter()
        .find(|(k, _)| *k == "PUPPY_IDLE_EXIT")
        .map(|(_, v)| v.to_string())
        .or_else(|| std::env::var("PUPPY_IDLE_EXIT").ok());
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("--idle-exit")
        .arg(idle_exit_secs(idle_exit.as_deref()).to_string())
        .current_dir(match cwd {
            Some(d) if d.is_dir() => d.to_path_buf(),
            _ => root_dir(),
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in ["PUPPY_HOME", "PUPPY_SOCKET"] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = spawn_detached(&mut cmd)?;
    reap(child);
    Ok(())
}

fn idle_exit_secs(var: Option<&str>) -> u64 {
    var.and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_IDLE_EXIT_SECS)
}

#[cfg(unix)]
fn root_dir() -> PathBuf {
    PathBuf::from("/")
}

/// The system drive's root, e.g. `C:\`.
#[cfg(windows)]
fn root_dir() -> PathBuf {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    PathBuf::from(format!("{drive}\\"))
}

#[cfg(unix)]
fn spawn_detached(cmd: &mut Command) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the forked child before exec and only calls setsid
    // and reads errno, both async-signal-safe; it allocates nothing and takes no locks.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Also breaks away from the caller's job object, so the daemon outlives a client a
/// browser started (Chrome runs native messaging hosts in a job that is closed with
/// them). A job that forbids breaking away fails the spawn with access denied: then
/// the daemon is started inside it.
#[cfg(windows)]
fn spawn_detached(cmd: &mut Command) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };
    const ERROR_ACCESS_DENIED: i32 = 5;
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            cmd.creation_flags(flags);
            cmd.spawn()
        }
        r => r,
    }
}

/// Unix: wait for the child in the background so it does not linger as a zombie of a
/// long-running client after it exits.
#[cfg(unix)]
fn reap(mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("puppy-daemon-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Windows has no zombies: dropping the handle is enough.
#[cfg(not(unix))]
fn reap(_child: std::process::Child) {}
