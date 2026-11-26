//! SFTP downloads through the real binary and daemon: `puppy ssh trust`, the default
//! key in `~/.ssh`, an input file with `sftp://` lines and a password that never shows,
//! and the agent a client offers to a daemon started without one.
#![cfg(unix)]

mod common;

use common::*;
use puppy_testserver::body_byte;
use puppy_testserver::sftp::{HostKey, SftpCfg, SftpServer, key_path, start_agent};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

const SECRET: &str = "hunter2-SECRET";
const MTIME: u32 = 1_795_000_000;

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

/// An environment whose home has no SSH keys or known hosts of the developer's and no
/// agent; `key`: put this test key in `~/.ssh/id_ed25519`.
fn env(key: Option<&str>) -> Env {
    let mut env = Env::new();
    let home = env.home.path().join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    if let Some(k) = key {
        std::fs::copy(key_path(k), home.join(".ssh/id_ed25519")).unwrap();
    }
    env.extra.push(("HOME", home.to_str().unwrap().into()));
    env.extra.push(("SSH_AUTH_SOCK", String::new()));
    // The SSH libraries' debug output is asked for: it must not reach the log.
    env.extra
        .push(("RUST_LOG", "debug,russh=trace,russh_sftp=trace".into()));
    env
}

/// `puppy <args>` with more environment.
fn run_with(env: &Env, args: &[&str], vars: &[(&str, &str)]) -> (ExitStatus, String, String) {
    let n = Instant::now().elapsed().as_nanos() ^ u128::from(std::process::id());
    let out = env.home.path().join(format!("o-{n}-{}", args.len()));
    let err = env.home.path().join(format!("e-{n}-{}", args.len()));
    let mut cmd = env.command(args);
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let child = cmd
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let status = wait_exit(child, TIMEOUT);
    (
        status,
        std::fs::read_to_string(out).unwrap(),
        std::fs::read_to_string(err).unwrap(),
    )
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

fn trust(env: &Env, s: &SftpServer) {
    let target = format!("127.0.0.1:{}", s.port());
    let (status, out, err) = env.run(&["ssh", "trust", "--yes", &target]);
    assert!(status.success(), "{out}\n{err}");
}

/// An unknown host is refused with what to do; `puppy ssh trust` asks (refusing without
/// a terminal), then with `--yes` trusts it, and the download with the default key in
/// `~/.ssh` works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trust_a_host_then_download_with_the_default_key() {
    let env = env(Some("user_ed25519"));
    let s = SftpServer::start(SftpCfg::default()).await;
    s.set_file("/srv/a.bin", data(300 * KIB, 0), MTIME);
    let url = s.url("/srv/a.bin").to_string();
    let target = format!("127.0.0.1:{}", s.port());
    let dl = env.dl_dir();

    let (status, out, err) = env.run(&["add", "--wait", "-o", dl.to_str().unwrap(), &url]);
    assert!(!status.success(), "{out}\n{err}");
    let text = format!("{out}{err}");
    assert!(text.contains("is not known"), "{text}");
    assert!(
        text.contains(&format!("puppy ssh trust {target}")),
        "{text}"
    );
    assert!(s.logins().is_empty(), "nothing is sent to an unknown host");

    // No terminal, no --yes: shown, not trusted.
    let (status, _, err) = env.run(&["ssh", "trust", &target]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("SHA256:"), "{err}");
    assert!(err.contains("--yes"), "{err}");
    assert!(!env.paths().known_hosts().exists());

    let (status, out, err) = env.run(&["--json", "ssh", "trust", "--yes", &target]);
    assert!(status.success(), "{out}\n{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["state"], "added", "{v}");
    assert!(
        v["fingerprint"].as_str().unwrap().starts_with("SHA256:"),
        "{v}"
    );
    let line = std::fs::read_to_string(env.paths().known_hosts()).unwrap();
    assert!(
        line.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", s.port())),
        "{line}"
    );
    let (status, out, _) = env.run(&["ssh", "trust", &target]);
    assert!(status.success());
    assert!(out.contains("already trusted"), "{out}");

    let (status, out, err) = env.run(&["add", "--wait", "-o", dl.to_str().unwrap(), &url]);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(std::fs::read(dl.join("a.bin")).unwrap(), data(300 * KIB, 0));
    assert!(
        s.logins()
            .iter()
            .all(|l| l == "publickey:puppy-test-user_ed25519"),
        "{:?}",
        s.logins()
    );
    assert_eq!(s.agent_requests(), 0);
}

/// `sftp://` lines in an input file, with `dir`, `out`, `checksum`,
/// `max-download-limit` and `pause`; the password in their URLs stays out of every
/// output, the daemon's log and the resume sidecar.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_file_sftp_lines_and_a_private_password() {
    let env = env(None);
    let s = SftpServer::start(SftpCfg {
        password: Some(SECRET.into()),
        authorized: Vec::new(),
        ..SftpCfg::default()
    })
    .await;
    trust(&env, &s);
    let (a, b, c) = (data(256 * KIB, 0), data(512 * KIB, 1), data(16 * KIB, 2));
    s.set_file("/pub/a.bin", a.clone(), MTIME);
    s.set_file("/pub/b.bin", b.clone(), MTIME);
    s.set_file("/pub/c.bin", c, MTIME);
    let url = |path: &str| {
        let mut u = s.url(path);
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
            url("/pub/a.bin"),
            work.join("sub").display(),
            sha256(&a),
            url("/pub/b.bin"),
            url("/pub/c.bin"),
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
    assert!(out.contains("puppy:***@"), "{out}");

    let (status, _, err) = env.run(&["wait", "1"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(std::fs::read(work.join("sub/first.bin")).unwrap(), a);
    assert_eq!(state_of(&env, 1)["download"]["checksum"]["algo"], "sha-256");

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
    assert_eq!(state_of(&env, 3)["download"]["state"]["kind"], "paused");

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
    assert!(out.contains("sftp://puppy:***@127.0.0.1"), "{out}");
    let (status, _, err) = env.run(&["stop"]);
    assert!(status.success(), "{err}");
    env.wait_daemon_gone(TIMEOUT).await;
    let log = std::fs::read_to_string(env.paths().log_dir.join("puppyd.log")).unwrap();
    assert!(!log.contains(SECRET), "{log}");
    assert!(
        !log.contains("russh"),
        "SSH library debug output was logged: {log}"
    );
    let state = std::fs::read_to_string(env.paths().state_file).unwrap();
    assert!(state.contains(SECRET), "only the private state file has it");
}

/// A daemon started without an agent uses the one the client adding the download
/// offers (its `SSH_AUTH_SOCK`, sent with `hello`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_uses_the_agent_a_client_offers() {
    let env = env(None);
    let s = SftpServer::start(SftpCfg {
        authorized: vec!["agent_ed25519"],
        ..SftpCfg::default()
    })
    .await;
    s.set_file("/f", data(100 * KIB, 3), MTIME);
    trust(&env, &s);
    // The daemon runs now, with no agent of its own.
    let (status, _, err) = env.run(&["ls"]);
    assert!(status.success(), "{err}");
    // A short path: Unix socket paths are limited to about 100 bytes.
    let sock = std::path::PathBuf::from(format!("/tmp/pz-{}-agent.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let _agent = start_agent(&sock, &["agent_ed25519"]).await;
    let dl = env.dl_dir();
    let url = s.url("/f").to_string();
    let (status, out, err) = run_with(
        &env,
        &["add", "--wait", "-o", dl.to_str().unwrap(), &url],
        &[("SSH_AUTH_SOCK", sock.to_str().unwrap())],
    );
    let _ = std::fs::remove_file(&sock);
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(std::fs::read(dl.join("f")).unwrap(), data(100 * KIB, 3));
    assert_eq!(
        s.logins(),
        vec!["publickey:puppy-test-agent_ed25519".to_string()]
    );
}

/// `puppy ssh trust` refuses a host that offers only a key of a type not known for it
/// (keys of other types are), unless `--replace-type` says the change is expected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trust_refuses_a_new_key_type_without_replace_type() {
    let env = env(Some("user_ed25519"));
    let s = SftpServer::start(SftpCfg::default()).await;
    s.set_file("/f", data(10 * KIB, 4), MTIME);
    std::fs::write(
        env.home.path().join("home/.ssh/known_hosts"),
        s.known_hosts_line(HostKey::Ecdsa),
    )
    .unwrap();
    let target = format!("127.0.0.1:{}", s.port());
    let (status, out, err) = env.run(&["ssh", "trust", "--yes", &target]);
    assert_eq!(status.code(), Some(1), "{out}\n{err}");
    assert!(err.contains("WARNING"), "{err}");
    assert!(err.contains("ecdsa-sha2-nistp256 in "), "{err}");
    assert!(err.contains("--replace-type"), "{err}");
    assert!(!env.paths().known_hosts().exists());
    let (status, out, err) = env.run(&["ssh", "trust", "--yes", "--replace-type", &target]);
    assert!(status.success(), "{out}\n{err}");
    assert!(err.contains("WARNING"), "{err}");
    let dl = env.dl_dir();
    let url = s.url("/f").to_string();
    let (status, out, err) = env.run(&["add", "--wait", "-o", dl.to_str().unwrap(), &url]);
    assert!(status.success(), "{out}\n{err}");
    // A target that would be a pattern in known_hosts is refused.
    let (status, _, err) = env.run(&["ssh", "trust", "--yes", "*.example,127.0.0.1"]);
    assert_eq!(status.code(), Some(2), "{err}");
}

/// A later client offering another agent (a dashboard opened over SSH with a
/// forwarded, empty agent) does not break a download's logins: the agent it was added
/// with comes first, and an agent without keys is passed over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_clients_empty_agent_does_not_take_over() {
    let env = env(None);
    let s = SftpServer::start(SftpCfg {
        authorized: vec!["agent_ed25519"],
        ..SftpCfg::default()
    })
    .await;
    s.set_file("/f", data(50 * KIB, 5), MTIME);
    trust(&env, &s);
    let pid = std::process::id();
    let good = std::path::PathBuf::from(format!("/tmp/pz-{pid}-good.sock"));
    let empty = std::path::PathBuf::from(format!("/tmp/pz-{pid}-empty.sock"));
    for p in [&good, &empty] {
        let _ = std::fs::remove_file(p);
    }
    let _g = start_agent(&good, &["agent_ed25519"]).await;
    let _e = start_agent(&empty, &[]).await;
    let dl = env.dl_dir();
    let url = s.url("/f").to_string();
    let (status, out, err) = run_with(
        &env,
        &["add", "--paused", "-o", dl.to_str().unwrap(), &url],
        &[("SSH_AUTH_SOCK", good.to_str().unwrap())],
    );
    assert!(status.success(), "{out}\n{err}");
    // The most recent client offers the empty agent and resumes it.
    let (status, out, err) = run_with(
        &env,
        &["resume", "1"],
        &[("SSH_AUTH_SOCK", empty.to_str().unwrap())],
    );
    assert!(status.success(), "{out}\n{err}");
    let (status, out, err) = run_with(
        &env,
        &["wait", "1"],
        &[("SSH_AUTH_SOCK", empty.to_str().unwrap())],
    );
    for p in [&good, &empty] {
        let _ = std::fs::remove_file(p);
    }
    assert!(status.success(), "{out}\n{err}");
    assert_eq!(std::fs::read(dl.join("f")).unwrap(), data(50 * KIB, 5));
}
