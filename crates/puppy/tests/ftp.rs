//! FTP downloads through the real binary and daemon: an input file with `ftp://` lines
//! and their options, and a password in a URL that never shows.

mod common;

use common::*;
use puppy_testserver::body_byte;
use puppy_testserver::ftp::{FtpCfg, FtpServer};
use std::time::{Duration, Instant};

const SECRET: &str = "hunter2-SECRET";

fn data(len: u64, version: u32) -> Vec<u8> {
    (0..len).map(|i| body_byte(i, version)).collect()
}

fn sha256(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn state_of(env: &Env, id: u64) -> serde_json::Value {
    let (status, out, err) = env.run(&["--json", "info", &id.to_string()]);
    assert!(status.success(), "{err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

fn wait_for(env: &Env, id: u64, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let v = state_of(env, id);
        if pred(&v) {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out: {v}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `ftp://` lines in an input file, with `dir`, `out`, `checksum`,
/// `max-download-limit` and `pause`; the password in their URLs stays out of every
/// output, the daemon's log and the resume sidecar.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_file_ftp_lines_and_a_private_password() {
    let mut env = Env::new();
    // The FTP library traces every command (PASS too): even asked for, it is not logged.
    env.extra.push(("RUST_LOG", "info,suppaftp=trace".into()));
    let s = FtpServer::start(FtpCfg {
        login: Some(("bob".into(), SECRET.into())),
        ..FtpCfg::default()
    })
    .await;
    let (a, b, c) = (data(256 * KIB, 0), data(512 * KIB, 1), data(16 * KIB, 2));
    s.set_file("pub/a.bin", a.clone(), "20261126120000");
    s.set_file("pub/b.bin", b.clone(), "20261126120000");
    s.set_file("pub/c.bin", c, "20261126120000");
    let url = |path: &str| {
        let mut u = s.url(path);
        u.set_username("bob").unwrap();
        u.set_password(Some(SECRET)).unwrap();
        u.to_string()
    };
    let work = env.home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let list = work.join("list.txt");
    std::fs::write(
        &list,
        format!(
            "{}\n  dir={}\n  out=first.bin\n  checksum=sha-256={}\n{}\n  \
             max-download-limit=64K\n{}\n  pause=true\n",
            url("pub/a.bin"),
            work.join("sub").display(),
            sha256(&a),
            url("pub/b.bin"),
            url("pub/c.bin"),
        ),
    )
    .unwrap();
    let dl = env.dl_dir();
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "-o",
        dl.to_str().unwrap(),
        "-i",
        list.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert!(
        !out.contains(SECRET) && !err.contains(SECRET),
        "{out}\n{err}"
    );
    assert!(out.contains("bob:***@"), "{out}");

    // 1: dir, out and checksum.
    let (status, _, err) = env.run(&["wait", "1"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(std::fs::read(work.join("sub/first.bin")).unwrap(), a);
    let v = state_of(&env, 1);
    assert_eq!(v["download"]["checksum"]["algo"], "sha-256", "{v}");

    // 2: limited to 64 KiB/s, so still running: its sidecar has no password.
    let v = wait_for(&env, 2, |v| {
        v["download"]["done_bytes"].as_u64().unwrap_or(0) > 0
    });
    assert_eq!(v["download"]["limit"], 64 * KIB, "{v}");
    let sidecar = dl.join("b.bin.part.puppy");
    let deadline = Instant::now() + TIMEOUT;
    while !sidecar.exists() {
        assert!(Instant::now() < deadline, "no sidecar");
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = std::fs::read_to_string(&sidecar).unwrap();
    assert!(!text.contains(SECRET), "{text}");
    let (status, _, err) = env.run(&["pause", "2"]);
    assert!(status.success(), "{err}");

    // 3: added paused.
    assert_eq!(state_of(&env, 3)["download"]["state"]["kind"], "paused");

    // Nothing shows the password: ls, info (both forms), the daemon log.
    for args in [
        vec!["--json", "ls"],
        vec!["ls"],
        vec!["--json", "info", "2"],
        vec!["info", "2"],
    ] {
        let (status, out, err) = env.run(&args);
        assert!(status.success(), "{args:?}: {err}");
        assert!(!out.contains(SECRET), "{args:?}: {out}");
    }
    let (_, out, _) = env.run(&["info", "2"]);
    assert!(out.contains("ftp://bob:***@127.0.0.1"), "{out}");
    let (status, _, err) = env.run(&["stop"]);
    assert!(status.success(), "{err}");
    env.wait_daemon_gone(TIMEOUT).await;
    let log = std::fs::read_to_string(env.paths().log_dir.join("puppyd.log")).unwrap();
    assert!(!log.contains(SECRET), "{log}");
    assert!(!log.contains("PASS"), "{log}");
    // Only the private state file has it.
    let state = std::fs::read_to_string(env.paths().state_file).unwrap();
    assert!(state.contains(SECRET));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env.paths().state_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}
