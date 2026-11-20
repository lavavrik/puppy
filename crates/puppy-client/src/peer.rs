//! Checks that the process at the other end of the daemon socket runs as the current
//! user, so a client never talks to (and hands download requests, cookies and auth
//! to) a socket or pipe squatted by someone else.

use interprocess::local_socket::tokio::{Stream, prelude::*};
use std::io;

/// `Ok(false)`: the peer runs as another user (or, on Windows, cannot even be
/// inspected by us, which also means it is not ours).
#[cfg(unix)]
pub(crate) fn owned_by_current_user(stream: &Stream) -> io::Result<bool> {
    let creds = stream.peer_creds()?;
    let Some(peer) = creds.euid() else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the daemon's user id is not available on this platform",
        ));
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    Ok(peer == me)
}

/// Compares the user SID of the pipe server's process (`GetNamedPipeServerProcessId`,
/// via interprocess) with the current process's.
#[cfg(windows)]
pub(crate) fn owned_by_current_user(stream: &Stream) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let Some(pid) = stream.peer_creds()?.pid() else {
        return Err(io::Error::other("the daemon's process id is not available"));
    };
    let denied = |e: &io::Error| e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    let me = token_user(unsafe { GetCurrentProcess() })?;
    // SAFETY: plain Win32 call; a non-null handle is closed below on every path.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        let e = io::Error::last_os_error();
        return if denied(&e) { Ok(false) } else { Err(e) };
    }
    let peer = token_user(process);
    // SAFETY: `process` is a valid handle we own.
    unsafe { CloseHandle(process) };
    let peer = match peer {
        Ok(p) => p,
        Err(e) if denied(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    // SAFETY: both buffers hold a TOKEN_USER whose SID points into the same buffer,
    // and both stay alive for the call.
    Ok(unsafe { EqualSid(sid(&me), sid(&peer)) } != 0)
}

/// The `TOKEN_USER` of a process, in a u64-aligned buffer (the SID it points to
/// lives in the same buffer).
#[cfg(windows)]
fn token_user(process: windows_sys::Win32::Foundation::HANDLE) -> io::Result<Vec<u64>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenUser};
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    // SAFETY: plain Win32 calls; the token handle is closed on every path after it
    // was opened, and the buffer is sized by the first GetTokenInformation call.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        let err = io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(err);
        }
        Ok(buf)
    }
}

#[cfg(windows)]
fn sid(token_user: &[u64]) -> windows_sys::Win32::Security::PSID {
    use windows_sys::Win32::Security::TOKEN_USER;
    // SAFETY: the buffer was filled by GetTokenInformation(TokenUser) and is aligned.
    unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}
