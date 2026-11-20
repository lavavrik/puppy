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

// ---- control commands ----

async fn slow_server() -> puppy_testserver::TestServer {
    puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        len: 1 << 24,
        bytes_per_sec: Some(2000),
        ..Default::default()
    })
    .await
}

fn state_of(env: &Env, id: u64) -> String {
    let all = snapshots(env, &["ls", "--json"]);
    let d = all.iter().find(|d| d["id"] == id).expect("download listed");
    d["state"]["kind"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_resume_by_id() {
    let env = Env::new();
    let s = slow_server().await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let (status, out, err) = env.run(&["pause", &id.to_string()]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), format!("paused {id}"));
    assert_eq!(state_of(&env, id), "paused");
    let (status, out, err) = env.run(&["resume", &id.to_string()]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), format!("resumed {id}"));
    assert_ne!(state_of(&env, id), "paused");
    let (status, out, _) = env.run(&["--json", "pause", &id.to_string()]);
    assert!(status.success());
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["id"], id);
    assert_eq!(v[0]["ok"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_all_skips_completed() {
    let env = Env::new();
    let fast = puppy_testserver::TestServer::start(Default::default()).await;
    let s = slow_server().await;
    let c = env.spawn_client().await.unwrap();
    let done = c.add(env.req(&fast, "f.bin"), false, None).await.unwrap();
    wait_completed(&c, done).await;
    let paused = c.add(env.paused_req("p.bin"), true, None).await.unwrap();
    let slow = c.add(env.req(&s, "s.bin"), false, None).await.unwrap();
    let (status, out, err) = env.run(&["pause", "all"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), format!("paused {slow}"));
    assert_eq!(state_of(&env, done), "completed");
    assert_eq!(state_of(&env, paused), "paused");
    let (status, out, _) = env.run(&["pause", "all"]);
    assert_eq!(status.code(), Some(0));
    assert_eq!(out.trim(), "nothing to pause");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_valid_and_unknown_ids() {
    let env = Env::new();
    let s = slow_server().await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let (status, out, err) = env.run(&["pause", &id.to_string(), "99", "abc"]);
    assert_eq!(status.code(), Some(3), "{err}");
    assert_eq!(out.trim(), format!("paused {id}"));
    assert!(err.contains("99"), "{err}");
    assert!(err.contains("abc"), "{err}");
    assert_eq!(state_of(&env, id), "paused");
    let (status, _, err) = env.run(&["pause", "abc"]);
    assert_eq!(status.code(), Some(2), "{err}");
    // An explicit id in the wrong state: message, code 1.
    let (status, _, err) = env.run(&["restart", &id.to_string()]);
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(err.contains(&id.to_string()), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rm_delete_files_removes_file() {
    let env = Env::new();
    let fast = puppy_testserver::TestServer::start(Default::default()).await;
    let c = env.spawn_client().await.unwrap();
    let a = c.add(env.req(&fast, "a.bin"), false, None).await.unwrap();
    let b = c.add(env.req(&fast, "b.bin"), false, None).await.unwrap();
    wait_completed(&c, a).await;
    wait_completed(&c, b).await;
    let (status, out, err) = env.run(&["rm", &a.to_string(), "--delete-files"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), format!("removed {a} (files deleted)"));
    assert!(!env.dl_dir().join("a.bin").exists());
    let (status, out, err) = env.run(&["rm", &b.to_string()]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), format!("removed {b}"));
    assert!(env.dl_dir().join("b.bin").exists());
    assert!(snapshots(&env, &["ls", "--json"]).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_reorders_queue() {
    let env = Env::new();
    let s = slow_server().await;
    let c = env.spawn_client().await.unwrap();
    let (status, _, err) = env.run(&["max-active", "1"]);
    assert_eq!(status.code(), Some(0), "{err}");
    let mut ids = vec![];
    for n in ["a", "b", "c"] {
        ids.push(c.add(env.req(&s, n), false, None).await.unwrap());
    }
    let pos = |env: &Env, id: u64| {
        let all = snapshots(env, &["ls", "--json"]);
        all.iter().find(|d| d["id"] == id).unwrap()["queue_position"].as_u64()
    };
    assert!(pos(&env, ids[2]) > pos(&env, ids[1]));
    let (status, out, err) = env.run(&["move", &ids[2].to_string(), "0"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.contains("moved"), "{out}");
    assert!(pos(&env, ids[2]) < pos(&env, ids[1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limit_per_download_and_global() {
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    let (status, out, err) = env.run(&["limit", &id.to_string(), "500K"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.contains("500"), "{out}");
    let all = snapshots(&env, &["ls", "--json"]);
    assert_eq!(all[0]["limit"], 500 * 1024);
    let (status, _, err) = env.run(&["limit", &id.to_string(), "none"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(snapshots(&env, &["ls", "--json"])[0]["limit"].is_null());
    let (status, _, err) = env.run(&["limit", &id.to_string(), "banana"]);
    assert_eq!(status.code(), Some(2), "{err}");
    let (status, _, _) = env.run(&["limit", &id.to_string()]);
    assert_eq!(status.code(), Some(2));
    let (status, _, _) = env.run(&["limit", "--global", &id.to_string(), "1M"]);
    assert_eq!(status.code(), Some(2));
    let (status, _, err) = env.run(&["limit", "99", "1M"]);
    assert_eq!(status.code(), Some(3), "{err}");

    let (status, _, err) = env.run(&["limit", "--global", "--down", "1M"]);
    assert_eq!(status.code(), Some(0), "{err}");
    let (status, _, err) = env.run(&["limit", "--global", "--up", "200K"]);
    assert_eq!(status.code(), Some(0), "{err}");
    let (status, out, err) = env.run(&["--json", "limit", "--global"]);
    assert_eq!(status.code(), Some(0), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["global_down_limit"], 1 << 20, "{out}");
    assert_eq!(v["global_up_limit"], 200 * 1024, "{out}");
    let (status, out, _) = env.run(&["limit", "--global"]);
    assert!(status.success());
    assert!(out.contains("download") && out.contains("upload"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_active_zero_is_usage() {
    let env = Env::new();
    let (status, _, err) = env.run(&["max-active", "0"]);
    assert_eq!(status.code(), Some(2), "{err}");
    let (status, out, err) = env.run(&["max-active", "2"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), "max-active 2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_when_running_then_not_running() {
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    let (status, out, err) = env.run(&["stop"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), "stopped");
    drop(c);
    env.wait_daemon_gone(std::time::Duration::from_secs(10))
        .await;
    let (status, out, err) = env.run(&["stop"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.contains("not running"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_json_says_what_happened() {
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    let (status, out, err) = env.run(&["--json", "stop"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), r#"{"stopped":true}"#);
    drop(c);
    env.wait_daemon_gone(std::time::Duration::from_secs(10))
        .await;
    let (status, out, err) = env.run(&["--json", "stop"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), r#"{"running":false}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_failure_shape() {
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    let (status, out, _) = env.run(&["--json", "resume", "abc", &id.to_string()]);
    assert_eq!(status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["id"], "abc");
    assert_eq!(v[0]["ok"], false);
    assert!(v[0]["error"].is_string(), "{out}");
    assert_eq!(v[0]["code"], 2);
    assert_eq!(v[1], serde_json::json!({"id": id, "ok": true}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rm_unknown_id_exit_3() {
    let env = Env::new();
    let (status, out, err) = env.run(&["rm", "99"]);
    assert_eq!(status.code(), Some(3), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("99"), "{err}");
}

// ---- wait, add --wait, watch ----

fn small_server_cfg() -> puppy_testserver::ServerCfg {
    puppy_testserver::ServerCfg {
        len: 256 * KIB,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_wait_completes_exit_0() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let url = s.url("/file.bin").to_string();
    let dir = env.dl_dir();
    let (status, out, err) = env.run(&[
        "add",
        "--wait",
        "-o",
        dir.to_str().unwrap(),
        "--name",
        "a.bin",
        &url,
    ]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.contains("added 1"), "{out}");
    assert!(out.contains("completed 1 a.bin"), "{out}");
    env.assert_file("a.bin", 256 * KIB);
    // Progress lines only go to a terminal.
    assert!(!err.contains('⠋'), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_wait_with_a_bad_source_waits_for_the_rest() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let url = s.url("/file.bin").to_string();
    let dir = env.dl_dir();
    let (status, out, err) = env.run(&["add", "--wait", "-o", dir.to_str().unwrap(), &url, "nope"]);
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(out.contains("completed 1"), "{out}");
    assert!(err.contains("nope"), "{err}");
    let (status, _, _) = env.run(&["add", "--wait", "--paused", &url]);
    assert_eq!(status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_failed_exit_4() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let url = s.url("/missing").to_string();
    let (status, _, err) = env.run(&["add", "--wait", &url]);
    assert_eq!(status.code(), Some(4), "{err}");
    assert!(err.contains("failed"), "{err}");
    let (status, _, err) = env.run(&["wait", "1"]);
    assert_eq!(status.code(), Some(4), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_attention_exit_5() {
    let env = Env::new();
    let s = slow_server().await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let out_f = env.home.path().join("wait-out");
    let err_f = env.home.path().join("wait-err");
    let child = env
        .command(&["wait", &id.to_string()])
        .stdout(std::fs::File::create(&out_f).unwrap())
        .stderr(std::fs::File::create(&err_f).unwrap())
        .spawn()
        .unwrap();
    wait_state(&c, id, "receiving bytes", |d| d.done_bytes > 0).await;
    c.call(puppy_proto::Request::Pause { id }).await.unwrap();
    s.set_version(1);
    c.call(puppy_proto::Request::Resume { id }).await.unwrap();
    let status = wait_exit(child, TIMEOUT);
    let err = std::fs::read_to_string(err_f).unwrap();
    assert_eq!(status.code(), Some(5), "{err}");
    assert!(err.contains("needs attention"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_already_terminal_states() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let c = env.spawn_client().await.unwrap();
    let done = c.add(env.req(&s, "d.bin"), false, None).await.unwrap();
    wait_completed(&c, done).await;
    let gone = c.add(env.paused_req("g.bin"), true, None).await.unwrap();
    c.call(puppy_proto::Request::Remove {
        id: gone,
        delete_files: false,
    })
    .await
    .unwrap();
    let timed = |args: &[&str]| {
        let t = std::time::Instant::now();
        let r = env.run(args);
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{args:?}");
        r
    };
    let (status, out, err) = timed(&["wait", &done.to_string()]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.contains("completed"), "{out}");
    let (status, _, err) = timed(&["wait", &gone.to_string()]);
    assert_eq!(status.code(), Some(3), "{err}");
    let (status, _, err) = timed(&["wait", &done.to_string(), &gone.to_string()]);
    assert_eq!(status.code(), Some(3), "{err}");
    let (status, out, err) = timed(&["--json", "wait", &done.to_string()]);
    assert_eq!(status.code(), Some(0), "{err}");
    let v: Vec<puppy_proto::DownloadSnapshot> = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0].state, puppy_proto::WireState::Completed);
    let (status, _, _) = timed(&["wait", "abc"]);
    assert_eq!(status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_transient_keeps_waiting() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        fail_status: Some(503),
        ..small_server_cfg()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "t.bin"), false, None).await.unwrap();
    let transient = |d: &puppy_proto::DownloadSnapshot| {
        matches!(
            d.state,
            puppy_proto::WireState::Failed {
                transient: true,
                ..
            }
        )
    };
    // `wait_state` panics on failures; poll by hand.
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !transient(&snapshot(&c, id).await) {
        assert!(std::time::Instant::now() < deadline, "never failed");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let err_f = env.home.path().join("wait-err");
    let mut child = env
        .command(&["wait", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_f).unwrap())
        .spawn()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "wait gave up on a transient failure"
    );
    // The server recovers; resume instead of waiting for the automatic retry (30 s).
    s.set_cfg(|c| c.fail_status = None);
    c.call(puppy_proto::Request::Resume { id }).await.unwrap();
    let status = wait_exit(child, TIMEOUT);
    let err = std::fs::read_to_string(err_f).unwrap();
    assert_eq!(status.code(), Some(0), "{err}");
    env.assert_file("t.bin", 256 * KIB);
}

/// Ctrl-C while waiting: exit 130, the download keeps going.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_interrupted_exit_130() {
    let env = Env::new();
    let s = slow_server().await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let child = env
        .command(&["wait", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    // SAFETY: plain syscall on our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let status = wait_exit(child, TIMEOUT);
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert_eq!(state_of(&env, id), "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_json_is_ndjson() {
    use std::io::BufRead;
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let c = env.spawn_client().await.unwrap();
    let mut child = env
        .command(&["--json", "watch"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // The first line (a snapshot) says watch is subscribed.
    let first = rx.recv_timeout(TIMEOUT).expect("no output from watch");
    let mut lines = vec![first];
    let id = c.add(env.req(&s, "w.bin"), false, None).await.unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut kinds = vec![];
    'read: loop {
        for line in lines.drain(..) {
            let ev: puppy_proto::Event =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
            kinds.push(serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"].clone());
            if ev
                == (puppy_proto::Event::Completed {
                    id,
                    name: "w.bin".into(),
                })
            {
                break 'read;
            }
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        lines.push(rx.recv_timeout(left).expect("no completed event"));
    }
    assert!(kinds.contains(&"snapshot".into()), "{kinds:?}");
    assert!(kinds.contains(&"added".into()), "{kinds:?}");
    let _ = child.kill();
    let _ = child.wait();
    reader.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_human_lines() {
    use std::io::BufRead;
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let c = env.spawn_client().await.unwrap();
    let mut child = env
        .command(&["watch"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // Nothing is printed until something happens: give watch time to subscribe.
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let id = c.add(env.req(&s, "h.bin"), false, None).await.unwrap();
    let added = rx.recv_timeout(TIMEOUT).expect("no added line");
    assert!(added.ends_with(&format!(" added {id} h.bin")), "{added}");
    assert_eq!(added.as_bytes()[2], b':', "{added}");
    let completed = rx.recv_timeout(TIMEOUT).expect("no completed line");
    assert!(
        completed.ends_with(&format!(" completed {id} h.bin")),
        "{completed}"
    );
    let _ = child.kill();
    let _ = child.wait();
    reader.join().unwrap();
}
