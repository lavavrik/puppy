//! The secrets of a request (cookies, auth, header values) stay in `state.json` while a
//! download is unfinished, and nowhere else: not in the logs, not in any output, not in
//! the resume sidecar. Once the download completes they are gone from the state too.

mod common;

use common::*;
use puppy_testserver::{ServerCfg, TestServer};
use std::time::{Duration, Instant};

const SECRETS: [&str; 2] = ["COOKIESECRET", "HDRSECRET"];

fn assert_no_secrets(what: &str, text: &str) {
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "{secret} leaked into {what}: {text}"
        );
    }
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// `puppy <args>` with `RUST_LOG=debug`, so an autostarted daemon logs verbosely.
fn run_debug(env: &Env, args: &[&str]) -> (std::process::ExitStatus, String) {
    let out = env.home.path().join("secrets-out");
    let mut cmd = env.command(args);
    cmd.env("RUST_LOG", "debug")
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::process::Stdio::null());
    let status = common::wait_exit(cmd.spawn().unwrap(), TIMEOUT);
    (status, read(&out))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookies_never_leak_and_go_after_completion() {
    let env = Env::new();
    let s = TestServer::start(ServerCfg {
        len: 4096 * KIB,
        bytes_per_sec: Some(128 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let url = s.url("/file.bin").to_string();
    let (st, out) = run_debug(
        &env,
        &[
            "add",
            "--cookie",
            "sid=COOKIESECRET",
            "-H",
            "Authorization: Bearer HDRSECRET",
            "--referer",
            "https://example.com/p",
            "-o",
            env.dl_dir().to_str().unwrap(),
            "--name",
            "a.bin",
            &url,
        ],
    );
    assert!(st.success(), "{out}");
    assert_no_secrets("`puppy add` output", &out);

    // Wait until it transfers.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let (_, json) = run_debug(&env, &["--json", "info", "1"]);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        if v["download"]["done_bytes"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "never transferred: {json}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        s.requests_seen().iter().any(|h| h
            .get("cookie")
            .is_some_and(|v| v.as_bytes() == b"sid=COOKIESECRET")),
        "the server never saw the cookie"
    );

    let paths = env.paths();
    // Persisted privately while unfinished (the debounced save may lag a moment).
    let deadline = Instant::now() + TIMEOUT;
    while !read(&paths.state_file).contains("COOKIESECRET") {
        assert!(Instant::now() < deadline, "the context was not persisted");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sidecar = puppy_core::http::resume::sidecar_path(&env.dl_dir().join("a.bin"));
    assert!(sidecar.exists(), "no sidecar at {sidecar:?}");
    assert_no_secrets("the resume sidecar", &read(&sidecar));
    for args in [
        &["info", "1"][..],
        &["--json", "info", "1"],
        &["--json", "ls"],
        &["ls"],
    ] {
        let (_, out) = run_debug(&env, args);
        assert_no_secrets(&format!("`puppy {}`", args.join(" ")), &out);
    }

    s.set_cfg(|c| c.bytes_per_sec = None);
    let (st, out) = run_debug(&env, &["wait", "1"]);
    assert!(st.success(), "{out}");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = read(&paths.state_file);
        if SECRETS.iter().all(|s| !text.contains(s)) && text.contains("a.bin") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "state.json keeps secrets: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let log = read(&paths.log_dir.join("puppyd.log"));
    assert!(!log.is_empty(), "the daemon logged nothing");
    assert_no_secrets("puppyd.log", &log);
}
