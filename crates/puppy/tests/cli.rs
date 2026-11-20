//! The scriptable subcommands, through the real binary.

mod common;

use common::*;
use std::process::Stdio;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_flag_is_global() {
    let env = Env::new();
    for args in [["--json", "status"], ["status", "--json"]] {
        let (status, out, err) = env.run(&args);
        assert!(status.success(), "{args:?}: {status:?}: {err}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["downloads"], 0, "{out}");
        assert_eq!(v["active"], 0, "{out}");
        assert_eq!(out.lines().count(), 1, "{out}");
    }
}

#[test]
fn unknown_subcommand_exit_2() {
    let env = Env::new();
    let (status, out, err) = env.run(&["frobnicate"]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(out.is_empty(), "{out}");
}

#[test]
fn help_and_version_exit_0() {
    let env = Env::new();
    for args in [&["--help"][..], &["--version"], &["ls", "--help"]] {
        let (status, out, err) = env.run(args);
        assert_eq!(status.code(), Some(0), "{args:?}: {err}");
        assert!(!out.is_empty(), "{args:?}");
    }
}

/// `puppy ls | head -1`: the reader is gone before `puppy` writes; the command still
/// finishes and exits 0 without noise.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdout_broken_pipe_is_quiet() {
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    for i in 0..30 {
        c.add(env.paused_req(&format!("file-{i}.bin")), true, None)
            .await
            .unwrap();
    }
    let mut child = env
        .command(&["ls"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the read end at once: connecting to the daemon takes longer than this,
    // so every write finds the pipe closed.
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn url_of(env: &Env, name: &str) -> String {
    // Never fetched: downloads from this are added paused.
    let _ = env;
    format!("http://127.0.0.1:9/{name}")
}

fn snapshots(env: &Env, args: &[&str]) -> Vec<serde_json::Value> {
    let (status, out, err) = env.run(args);
    assert!(status.success(), "{args:?}: {status:?}: {err}");
    serde_json::from_str::<Vec<serde_json::Value>>(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_http_and_ls_shows_it() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(Default::default()).await;
    let url = s.url("/file.bin").to_string();
    let (status, out, err) = env.run(&["add", "--paused", &url]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.starts_with("added 1 "), "{out}");
    let (status, out, err) = env.run(&["ls"]);
    assert!(status.success(), "{err}");
    assert!(out.contains("paused"), "{out}");
    assert!(out.lines().next().unwrap().starts_with("ID"), "{out}");
    let all = snapshots(&env, &["ls", "--json"]);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["id"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_json_lists_ids_and_sources() {
    let env = Env::new();
    let (u1, u2) = (url_of(&env, "a"), url_of(&env, "b"));
    let (status, out, err) = env.run(&["--json", "add", "--paused", &u1, &u2]);
    assert!(status.success(), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["id"], 1);
    assert_eq!(v[1]["source"], u2.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_stdin_mixed_lines() {
    let env = Env::new();
    let (u1, u2) = (url_of(&env, "a"), url_of(&env, "b"));
    let input = format!("\r\n# c\n \"{u1}\"\r\nnot a url\n{u2}\n");
    let out_f = env.home.path().join("o");
    let err_f = env.home.path().join("e");
    let mut child = env
        .command(&["add", "--paused", "-"])
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&out_f).unwrap())
        .stderr(std::fs::File::create(&err_f).unwrap())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let status = wait_exit(child, TIMEOUT);
    let out = std::fs::read_to_string(out_f).unwrap();
    let err = std::fs::read_to_string(err_f).unwrap();
    assert_eq!(status.code(), Some(1), "{err}");
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(err.contains("not a url"), "{err}");
    assert_eq!(snapshots(&env, &["--json", "ls"]).len(), 2);
}

#[test]
fn add_name_with_two_sources_is_usage() {
    let env = Env::new();
    let (a, b) = (url_of(&env, "a"), url_of(&env, "b"));
    let (status, out, err) = env.run(&["add", "--name", "x", &a, &b]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(out.is_empty(), "{out}");
}

#[test]
fn add_malformed_header_or_limit_is_usage() {
    let env = Env::new();
    let a = url_of(&env, "a");
    for extra in [&["--header", "no-colon"][..], &["--limit", "fast"]] {
        let mut args = vec!["add", "--paused", &a];
        args.extend_from_slice(extra);
        let (status, _, err) = env.run(&args);
        assert_eq!(status.code(), Some(2), "{extra:?}: {err}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_headers_reach_server() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(Default::default()).await;
    let url = s.url("/file.bin").to_string();
    let dir = env.dl_dir();
    let (status, _, err) = env.run(&[
        "add",
        "--header",
        "X-Test: 1",
        "--cookie",
        "a=b",
        "--referer",
        "http://r/",
        "-o",
        dir.to_str().unwrap(),
        &url,
    ]);
    assert!(status.success(), "{err}");
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let seen = s.requests_seen();
        if seen.iter().any(|h| {
            h.get("x-test").is_some_and(|v| v == "1")
                && h.get("cookie").is_some_and(|v| v == "a=b")
                && h.get("referer").is_some_and(|v| v == "http://r/")
        }) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{seen:?}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_state_filter() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(Default::default()).await;
    let c = env.spawn_client().await.unwrap();
    let done = c.add(env.req(&s, "done.bin"), false, None).await.unwrap();
    wait_completed(&c, done).await;
    c.add(env.paused_req("p.bin"), true, None).await.unwrap();
    let names = |args: &[&str]| -> Vec<String> {
        snapshots(&env, args)
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names(&["--json", "ls", "--state", "paused"]), ["p.bin"]);
    assert_eq!(
        names(&["--json", "ls", "--state", "completed"]),
        ["done.bin"]
    );
    assert_eq!(
        names(&["--json", "ls", "--state", "paused,completed"]).len(),
        2
    );
    assert!(names(&["--json", "ls", "--state", "failed"]).is_empty());
    let (status, _, _) = env.run(&["ls", "--state", "bogus"]);
    assert_eq!(status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_table_aligns_wide_names() {
    use unicode_width::UnicodeWidthStr;
    let env = Env::new();
    let long = "a".repeat(80);
    for name in ["日本語ファイル名.iso", long.as_str()] {
        let u = url_of(&env, "x");
        let (status, _, err) = env.run(&["add", "--paused", "--name", name, &u]);
        assert!(status.success(), "{err}");
    }
    let (status, out, err) = env.run(&["ls"]);
    assert!(status.success(), "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[1].contains("日本語ファイル名.iso"), "{out}");
    assert!(lines[2].trim_end().ends_with('…'), "{out}");
    let w = lines[1..].iter().map(|l| l.width()).max().unwrap();
    assert!(w <= 100, "{out}");
    // Columns line up: the NAME column starts at the same display width everywhere.
    let name_col = lines[0]
        .find("NAME")
        .map(|i| lines[0][..i].width())
        .unwrap();
    for l in &lines[1..] {
        let at = |s: &str| l.find(s).map(|i| l[..i].width());
        assert!(
            at("日本語").or(at("aaa")) == Some(name_col),
            "{l:?} vs {name_col}"
        );
    }
    let all = snapshots(&env, &["ls", "--json"]);
    assert!(all.iter().any(|d| d["name"] == long.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_json_has_detail() {
    let env = Env::new();
    let u = url_of(&env, "a");
    env.run(&["add", "--paused", &u]);
    let (status, out, err) = env.run(&["--json", "info", "1"]);
    assert!(status.success(), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["download"]["id"], 1);
    assert!(v.get("detail").is_some(), "{out}");
    let (status, out, err) = env.run(&["info", "1"]);
    assert!(status.success(), "{err}");
    assert!(out.contains("state") && out.contains("paused"), "{out}");
    let (status, _, err) = env.run(&["info", "99"]);
    assert_eq!(status.code(), Some(3), "{err}");
}

/// `puppy add URL1 URL2 >&-`-style: a reader that is gone must not stop the adds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_with_closed_stdout_still_adds_all() {
    let env = Env::new();
    let (u1, u2) = (url_of(&env, "a"), url_of(&env, "b"));
    let mut child = env
        .command(&["add", "--paused", &u1, &u2])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(snapshots(&env, &["--json", "ls"]).len(), 2);
}
