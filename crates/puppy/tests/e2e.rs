//! End-to-end: the real `puppy` binary as daemon and client.
//!
//! Each test has its own `PUPPY_HOME` tempdir and a short socket name, passed to every
//! process it starts (never set in this process's environment), with
//! `PUPPY_IDLE_EXIT=1` so auto-spawned daemons go away quickly. [`Env`]'s drop makes
//! sure no daemon outlives its test: it asks the daemon to shut down and, failing
//! that, kills the process named in `<data_dir>/puppyd.pid`.

mod common;

use common::*;
use puppy_client::Client;
use puppy_proto::{ReplyData, Request, WireState};
use puppy_testserver::{ServerCfg, TestServer};
#[cfg(unix)]
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn autospawn_then_idle_exit() {
    let env = Env::new();
    let s = TestServer::start(ServerCfg {
        len: 64 * KIB,
        ..ServerCfg::default()
    })
    .await;
    assert!(!env.lock_held());
    let c = env
        .spawn_client()
        .await
        .expect("hello with a spawned daemon");
    let pid = env.daemon_pid().expect("the daemon wrote its pid");
    assert_ne!(pid, std::process::id(), "a separate daemon process");
    assert!(env.lock_held());
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_completed(&c, id).await;
    env.assert_file("a.bin", 64 * KIB);
    // No clients and no work: the daemon idle-exits after 1 s.
    drop(c);
    env.wait_daemon_gone(Duration::from_secs(15)).await;
    assert!(
        Client::connect(&env.socket, "e2e").await.is_err(),
        "nothing listens any more"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_autospawn_single_daemon() {
    let env = std::sync::Arc::new(Env::new());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let env = env.clone();
        tasks.spawn(async move { env.spawn_client().await.expect("connected") });
    }
    let mut clients = Vec::new();
    while let Some(c) = tasks.join_next().await {
        clients.push(c.unwrap());
    }
    // Exactly one daemon: it holds the lock, and every client talks to it.
    assert!(env.lock_held());
    let pid = env.daemon_pid().expect("the daemon wrote its pid");
    let id = clients[0]
        .add(env.paused_req("a.bin"), true, None)
        .await
        .unwrap();
    for c in &clients {
        let list = c.list().await.unwrap();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].id, id);
    }
    assert_eq!(env.daemon_pid(), Some(pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_daemon_resumes_download() {
    let env = Env::new();
    let len = 1024 * KIB;
    let s = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(32 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&c, id, "transferring", |d| {
        d.state == WireState::Active && d.done_bytes > 0
    })
    .await;
    assert_eq!(c.call(Request::Shutdown).await.unwrap(), ReplyData::Done);
    drop(c);
    env.wait_daemon_gone(TIMEOUT).await;
    let saved = std::fs::read_to_string(env.paths().state_file).unwrap();
    assert!(saved.contains("a.bin"), "{saved}");

    s.set_cfg(|cfg| cfg.bytes_per_sec = None);
    let c = env.spawn_client().await.unwrap();
    let first = snapshot(&c, id).await;
    assert!(first.done_bytes > 0, "progress restored: {first:?}");
    wait_completed(&c, id).await;
    env.assert_file("a.bin", len);
}

/// A daemon killed outright mid-transfer: the next client starts a new one, which
/// resumes the download from its files, and ids keep growing.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigkill_mid_transfer_then_respawn_completes() {
    let env = Env::new();
    let len = 1024 * KIB;
    let s = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(64 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let first = c.add(env.paused_req("p.bin"), true, None).await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&c, id, "transferring", |d| {
        d.state == WireState::Active && d.done_bytes > 0
    })
    .await;
    // Let the debounced state save and a sidecar flush land, as they would in use.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pid = env.live_daemon_pid().expect("a daemon runs");
    drop(c);
    // SAFETY: plain syscall on the daemon this test started (its lock is held).
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + TIMEOUT;
    while env.lock_held() {
        assert!(Instant::now() < deadline, "the killed daemon kept its lock");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    s.set_cfg(|cfg| cfg.bytes_per_sec = None);
    let c = env.spawn_client().await.unwrap();
    assert_ne!(env.live_daemon_pid(), Some(pid));
    wait_completed(&c, id).await;
    env.assert_file("a.bin", len);
    let next = c.add(env.paused_req("q.bin"), true, None).await.unwrap();
    assert!(
        next > id && next > first,
        "id {next} reused (had {first}, {id})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_prints_summary() {
    let env = Env::new();
    let (status, out, err) = env.run(&["status"]);
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(out, "0 downloads, 0 active\n");
    // It started a daemon (which may already have idle-exited): add something and
    // ask again.
    let c = env.spawn_client().await.unwrap();
    c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    let (status, out, err) = env.run(&["status"]);
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(out, "1 download, 0 active\n");
}

#[test]
fn tui_flag_parses() {
    let env = Env::new();
    let (status, out, err) = env.run(&["--theme", "light", "--help"]);
    assert!(status.success(), "{status:?}: {err}");
    assert!(out.contains("--theme"), "{out}");
    let (status, _, err) = env.run(&["--theme", "neon", "status"]);
    assert_eq!(status.code(), Some(2), "{err}");
}

#[test]
fn no_tty_fails_gracefully() {
    let env = Env::new();
    // Piped stdio (not a terminal) for stdin, stdout and stderr.
    let out = env
        .command(&[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("puppy's dashboard needs an interactive terminal"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_daemon_exits_zero() {
    let env = Env::new();
    let _c = env.spawn_client().await.unwrap();
    let (status, _, err) = env.run(&["daemon"]);
    assert!(status.success(), "{status:?}: {err}");
    assert!(err.contains("already running"), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
}

#[test]
fn version_flag() {
    let env = Env::new();
    let (status, out, _) = env.run(&["--version"]);
    assert!(status.success());
    assert_eq!(out.trim(), format!("puppy {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn bad_arguments_fail() {
    let env = Env::new();
    let (status, _, err) = env.run(&["daemon", "--idle-exit", "soon"]);
    assert!(!status.success());
    assert!(err.contains("--idle-exit"), "{err}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_shuts_down_gracefully() {
    signal_shuts_down_gracefully(libc::SIGTERM).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sighup_shuts_down_gracefully() {
    signal_shuts_down_gracefully(libc::SIGHUP).await;
}

/// Two signals in a row: the daemon exits promptly either way, releasing its lock and
/// leaving a readable state file.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_signal_exits_promptly() {
    let env = Env::new();
    let s = TestServer::start(ServerCfg {
        len: 4096 * KIB,
        bytes_per_sec: Some(256 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let child = env
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let c = loop {
        if let Ok(c) = Client::connect(&env.socket, "e2e").await {
            break c;
        }
        assert!(Instant::now() < deadline, "the daemon never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&c, id, "transferring", |d| d.done_bytes > 0).await;
    let pid = child.id() as libc::pid_t;
    for _ in 0..2 {
        // SAFETY: plain syscall on our own child.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    }
    let t = Instant::now();
    tokio::task::spawn_blocking(move || wait_exit(child, TIMEOUT))
        .await
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(!env.lock_held());
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths().state_file).unwrap()).unwrap();
    assert_eq!(state["downloads"][0]["id"], id, "{state:#}");
    drop(c);
}

#[cfg(unix)]
async fn signal_shuts_down_gracefully(signal: libc::c_int) {
    let env = Env::new();
    let child = env
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let c = loop {
        if let Ok(c) = Client::connect(&env.socket, "e2e").await {
            break c;
        }
        assert!(Instant::now() < deadline, "the daemon never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let id = c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    // SAFETY: plain syscall on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
    let status = tokio::task::spawn_blocking(move || wait_exit(child, TIMEOUT))
        .await
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!env.socket_file_exists(), "socket removed");
    assert!(env.daemon_pid().is_none(), "pid file removed");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths().state_file).unwrap()).unwrap();
    assert_eq!(state["downloads"][0]["id"], id, "{state:#}");
    drop(c);
}

/// The cleanup guard never kills the process a stale pid file names (a crashed
/// daemon's pid may since belong to an unrelated process).
#[cfg(unix)]
#[test]
fn cleanup_ignores_stale_pid_file() {
    let mut bystander = Command::new("sleep").arg("30").spawn().unwrap();
    let env = Env::new();
    std::fs::write(env.paths().pid_file, format!("{}\n", bystander.id())).unwrap();
    assert!(!env.lock_held());
    drop(env);
    // A signal is delivered asynchronously: give a kill time to land.
    std::thread::sleep(Duration::from_millis(500));
    let alive = bystander.try_wait().unwrap().is_none();
    let _ = bystander.kill();
    let _ = bystander.wait();
    assert!(alive, "the cleanup killed an unrelated process");
}

/// A system without CA certificates (a minimal container without `ca-certificates`):
/// the daemon falls back to puppy's bundled roots instead of failing to start, and the
/// torrent engine (which builds its own HTTPS client) starts too.
#[cfg(all(unix, not(target_vendor = "apple")))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_starts_without_system_ca_certificates() {
    let mut env = Env::new();
    let nowhere = env.home.path().join("no-certs");
    env.extra = vec![
        (
            "SSL_CERT_FILE",
            nowhere.join("ca.pem").to_str().unwrap().to_string(),
        ),
        ("SSL_CERT_DIR", nowhere.to_str().unwrap().to_string()),
    ];
    let s = TestServer::start(ServerCfg {
        len: 64 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_completed(&c, id).await;
    env.assert_file("a.bin", 64 * KIB);

    let paths = env.paths();
    let pem = std::fs::read_to_string(paths.data_dir.join("ca-roots.pem")).unwrap();
    assert!(
        pem.matches("-----BEGIN CERTIFICATE-----").count() > 100,
        "bundled roots"
    );
    let log = std::fs::read_to_string(paths.log_dir.join("puppyd.log")).unwrap();
    assert!(log.contains("no CA certificates"), "{log}");
    assert!(!log.contains("cannot start the torrent engine"), "{log}");
}

/// A daemon that cannot set updates up says why in its log, not only on a stderr
/// nobody reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn updates_off_reason_is_logged() {
    let mut env = Env::new();
    env.extra
        .push(("PUPPY_UPDATE_URL", "not a url".to_string()));
    let c = env.spawn_client().await.unwrap();
    let log = std::fs::read_to_string(env.paths().log_dir.join("puppyd.log")).unwrap();
    drop(c);
    assert!(log.contains("updates are off"), "{log}");
    assert!(log.contains("PUPPY_UPDATE_URL"), "{log}");
}

/// An installed update replaces the running daemon in place: the new binary is
/// exec'd with the daemon's arguments and keeps its pid.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_execs_the_new_binary() {
    use puppy_testserver::github::{FakeGitHub, FakeRelease};

    let release = FakeRelease::new("9.9.9", puppy_daemon::update::TARGET).binary_script(
        "if [ \"$1\" = \"--version\" ]; then echo 'puppy 9.9.9'; exit 0; fi\n\
         { echo $$; echo \"$@\"; } > \"$PUPPY_HOME/execd.tmp\"\n\
         mv \"$PUPPY_HOME/execd.tmp\" \"$PUPPY_HOME/execd\"\n\
         exec sleep 30",
    );
    let github = FakeGitHub::start(release).await;

    let mut env = Env::new();
    env.extra
        .push(("PUPPY_UPDATE_URL", github.api_base().to_string()));
    env.extra.push(("PUPPY_UPDATE_ALLOW_DEV", "1".to_string()));

    // A copy of the real binary the update may overwrite.
    let bin_dir = tempfile::tempdir().unwrap();
    let exe = bin_dir.path().join("puppy");
    std::fs::copy(BIN, &exe).unwrap();

    let mut cmd = Command::new(&exe);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env.vars() {
        cmd.env(k, v);
    }
    let mut child = spawn_retrying(&mut cmd);
    let pid = child.id();

    let paths = env.paths();
    let client = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(c) = Client::connect(&paths.socket, "e2e").await {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("daemon did not start");
    let before: u32 = std::fs::read_to_string(&paths.pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(before, pid);

    // The reply may or may not arrive before the daemon shuts down.
    let _ = client.call(Request::Update { check_only: false }).await;
    drop(client);

    let execd = env.home.path().join("execd");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !execd.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = std::fs::read_to_string(&execd);
    // Clean up before asserting: the stand-in sleeps 30 s.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let _ = child.wait();
    let text = text.expect("the new binary was not started");
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap().parse::<u32>().unwrap(),
        before,
        "same pid"
    );
    assert!(lines.next().unwrap().contains("daemon"), "{text}");
}
