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

/// Every shell's script generates without a daemon, offers the subcommands (nested
/// ones too) and leaves out the hidden ones.
#[test]
fn completions_for_every_shell() {
    let env = Env::new();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let (status, out, err) = env.run(&["completions", shell]);
        assert_eq!(status.code(), Some(0), "{shell}: {err}");
        assert!(err.is_empty(), "{shell}: {err}");
        for word in [
            "add",
            "max-active",
            "completions",
            "browser",
            "uninstall",
            "output-dir",
        ] {
            assert!(out.contains(word), "{shell}: no {word:?} in the script");
        }
        assert!(
            !out.contains("native-host"),
            "{shell}: offers a hidden command"
        );
        assert!(
            !out.contains("one page per command"),
            "{shell}: offers `man`"
        );
    }
    assert_eq!(env.daemon_pid(), None, "completions started a daemon");
}

#[test]
fn completions_unknown_shell_exit_2() {
    let env = Env::new();
    let (status, out, err) = env.run(&["completions", "tcsh"]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("bash"), "lists the shells: {err}");
}

/// `puppy man` prints puppy.1 as roff, listing the visible subcommands.
#[test]
fn man_page_renders() {
    let env = Env::new();
    let (status, out, err) = env.run(&["man"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.contains(".TH puppy 1"), "{out}");
    assert!(out.contains(".SH SUBCOMMANDS"), "{out}");
    assert!(out.contains(r"puppy\-add(1)"), "{out}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
    assert!(!out.contains("native"), "{out}");
    // Hidden: not in --help either.
    let (_, help, _) = env.run(&["--help"]);
    assert!(help.contains("completions"), "{help}");
    assert!(!help.contains("\n  man "), "{help}");
}

/// `puppy man --dir` writes one page per visible command, nested ones included.
#[test]
fn man_dir_writes_a_page_per_command() {
    let env = Env::new();
    let dir = env.home.path().join("man/man1");
    let (status, out, err) = env.run(&["man", "--dir", dir.to_str().unwrap()]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(out.is_empty(), "{out}");
    for page in [
        "puppy.1",
        "puppy-add.1",
        "puppy-completions.1",
        "puppy-browser-install.1",
    ] {
        let text =
            std::fs::read_to_string(dir.join(page)).unwrap_or_else(|e| panic!("{page}: {e}"));
        assert!(
            text.starts_with(".ie") || text.contains(".TH"),
            "{page}: {text}"
        );
    }
    let add = std::fs::read_to_string(dir.join("puppy-add.1")).unwrap();
    assert!(add.contains(r"\-\-output\-dir"), "{add}");
    for hidden in ["puppy-man.1", "puppy-native-host.1", "puppy-help.1"] {
        assert!(!dir.join(hidden).exists(), "{hidden} was written");
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
    assert_eq!(v["added"][0]["id"], 1);
    assert_eq!(v["added"][1]["source"], u2.as_str());
    assert_eq!(v["failed"], serde_json::json!([]));
}

/// Failures are part of the one JSON value, not just stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_json_reports_failures_in_the_object() {
    let env = Env::new();
    let u1 = url_of(&env, "a");
    let (status, out, _) = env.run(&["--json", "add", "--paused", &u1, "nope"]);
    assert_eq!(status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["added"][0]["id"], 1);
    assert_eq!(v["failed"][0]["source"], "nope");
    assert!(v["failed"][0]["error"].is_string(), "{out}");
    assert_eq!(v["failed"][0]["code"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_wait_json_is_one_value() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(small_server_cfg()).await;
    let url = s.url("/file.bin").to_string();
    let dir = env.dl_dir();
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--wait",
        "-o",
        dir.to_str().unwrap(),
        &url,
        "nope",
    ]);
    assert_eq!(status.code(), Some(1), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["added"][0]["id"], 1, "{out}");
    assert_eq!(v["failed"][0]["source"], "nope", "{out}");
    assert_eq!(v["final"][0]["state"]["kind"], "completed", "{out}");
    assert_eq!(v["missing"], serde_json::json!([]), "{out}");
}

/// Runs `puppy <args>` with `input` on stdin: (status, stdout, stderr).
fn run_stdin(env: &Env, args: &[&str], input: &[u8]) -> (std::process::ExitStatus, String, String) {
    use std::io::Write;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out_f = env.home.path().join(format!("so-{n}"));
    let err_f = env.home.path().join(format!("se-{n}"));
    let mut child = env
        .command(args)
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&out_f).unwrap())
        .stderr(std::fs::File::create(&err_f).unwrap())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let status = wait_exit(child, TIMEOUT);
    (
        status,
        std::fs::read_to_string(out_f).unwrap(),
        std::fs::read_to_string(err_f).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_stdin_bom_and_utf16() {
    let env = Env::new();
    let (u1, u2, u3) = (url_of(&env, "a"), url_of(&env, "b"), url_of(&env, "c"));
    let mut utf8 = b"\xEF\xBB\xBF".to_vec();
    utf8.extend(format!("{u1}\n").bytes());
    let units = |s: String| s.encode_utf16().collect::<Vec<u16>>();
    let mut le = vec![0xFF, 0xFE];
    le.extend(
        units(format!("{u2}\r\n"))
            .iter()
            .flat_map(|u| u.to_le_bytes()),
    );
    let mut be = vec![0xFE, 0xFF];
    be.extend(
        units(format!("{u3}\r\n"))
            .iter()
            .flat_map(|u| u.to_be_bytes()),
    );
    for input in [utf8, le, be] {
        let (status, out, err) = run_stdin(&env, &["add", "--paused", "-"], &input);
        assert_eq!(status.code(), Some(0), "{err}");
        assert_eq!(out.lines().count(), 1, "{out}");
    }
    let all = snapshots(&env, &["--json", "ls"]);
    let sources: Vec<&str> = all.iter().map(|d| d["source"].as_str().unwrap()).collect();
    assert_eq!(sources, [u1, u2, u3].map(|u| u.leak() as &str));
    // Truncated UTF-16 (a lone byte after the BOM, an unpaired surrogate).
    for bad in [vec![0xFF, 0xFE, 0x41], vec![0xFF, 0xFE, 0x00, 0xD8]] {
        let (status, out, err) = run_stdin(&env, &["add", "--paused", "-"], &bad);
        assert_eq!(status.code(), Some(2), "{err}");
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("stdin is not valid UTF-16"), "{err}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_empty_stdin_is_usage() {
    let env = Env::new();
    let (status, out, err) = run_stdin(&env, &["add", "-"], b"\n# only a comment\n");
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("no sources on stdin"), "{err}");
    assert!(!env.lock_held(), "a usage error must not start a daemon");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_stdin_mixed_lines() {
    let env = Env::new();
    let (u1, u2) = (url_of(&env, "a"), url_of(&env, "b"));
    let input = format!("\r\n# c\n \"{u1}\"\r\nnot a url\n{u2}\n");
    let (status, out, err) = run_stdin(&env, &["add", "--paused", "-"], input.as_bytes());
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
    assert_eq!(names(&["--json", "ls", "--state", "done"]), ["done.bin"]);
    assert!(names(&["--json", "ls", "--state", "failed"]).is_empty());
    assert!(names(&["--json", "ls", "--state", "retrying"]).is_empty());
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
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["final"][0]["state"]["kind"], "completed", "{out}");
    assert_eq!(v["missing"], serde_json::json!([]), "{out}");
    let (status, out, _) = timed(&["--json", "wait", &done.to_string(), &gone.to_string()]);
    assert_eq!(status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["final"].as_array().unwrap().len(), 1, "{out}");
    assert_eq!(v["missing"], serde_json::json!([gone]), "{out}");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_state_retrying_is_not_failed() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        fail_status: Some(503),
        ..small_server_cfg()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "t.bin"), false, None).await.unwrap();
    wait_transient(&c, id).await;
    assert_eq!(
        snapshots(&env, &["--json", "ls", "--state", "retrying"]).len(),
        1
    );
    assert!(snapshots(&env, &["--json", "ls", "--state", "failed"]).is_empty());
}

async fn wait_transient(c: &puppy_client::Client, id: u64) {
    // `wait_state` panics on failures; poll by hand.
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let d = snapshot(c, id).await;
        if matches!(
            d.state,
            puppy_proto::WireState::Failed {
                transient: true,
                ..
            }
        ) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "never failed");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_all_includes_retrying() {
    let env = Env::new();
    let s = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        fail_status: Some(503),
        ..small_server_cfg()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "t.bin"), false, None).await.unwrap();
    wait_transient(&c, id).await;
    let (status, out, err) = env.run(&["pause", "all"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), format!("paused {id}"));
    assert_eq!(state_of(&env, id), "paused");
}

/// Piped output (no COLUMNS, not a terminal) never truncates names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_piped_does_not_truncate_names() {
    let env = Env::new();
    let long = "n".repeat(150);
    let u = url_of(&env, "x");
    let (status, _, err) = env.run(&["add", "--paused", "--name", &long, &u]);
    assert!(status.success(), "{err}");
    let out_f = env.home.path().join("ls-out");
    let status = env
        .command(&["ls"])
        .env_remove("COLUMNS")
        .stdout(std::fs::File::create(&out_f).unwrap())
        .status()
        .unwrap();
    assert!(status.success());
    let out = std::fs::read_to_string(out_f).unwrap();
    assert!(out.contains(&long), "{out}");
    assert!(!out.contains('…'), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_on_paused_says_so_once() {
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.paused_req("p.bin"), true, None).await.unwrap();
    let err_f = env.home.path().join("wait-err");
    let mut child = env
        .command(&["wait", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_f).unwrap())
        .spawn()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    let _ = child.wait();
    let err = std::fs::read_to_string(err_f).unwrap();
    let msg = format!("puppy: {id} is paused; waiting until it is resumed");
    assert_eq!(err.matches(&msg).count(), 1, "{err}");
}

/// The daemon going away while waiting: exit 1, "lost connection".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_daemon_disconnect_exit_1() {
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
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let _ = c.call(puppy_proto::Request::Shutdown).await;
    let status = wait_exit(child, TIMEOUT);
    let err = std::fs::read_to_string(err_f).unwrap();
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(err.contains("lost connection"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_bad_id_and_unsafe_text() {
    let env = Env::new();
    let (status, _, err) = env.run(&["info", "abc"]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("\"abc\" is not a download id"), "{err}");
    let u = url_of(&env, "x");
    let (status, _, err) = env.run(&["add", "--paused", "--name", "a\u{1b}[31mb", &u]);
    assert!(status.success(), "{err}");
    let (status, out, err) = env.run(&["info", "1"]);
    assert!(status.success(), "{err}");
    assert!(!out.contains('\u{1b}'), "{out:?}");
}

/// A daemon speaking another protocol: every command says so; `stop` stops it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_daemon_hint_and_stop() {
    use puppy_proto::{Envelope, ErrorCode, MsgReader, Reply, ReplyData, Request, ServerMsg};
    use tokio::io::BufReader;
    let env = Env::new();
    let path = match &env.socket {
        puppy_daemon::SocketName::Path(p) => p.clone(),
        _ => unreachable!(),
    };
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let sock = path.clone();
    let fake = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = tokio::io::split(stream);
            let mut rd = MsgReader::new(BufReader::new(rd));
            while let Ok(Some(msg)) = rd.next::<Envelope<Request>>().await {
                let result = match msg.body {
                    Request::Hello { .. } => Reply::Err {
                        code: ErrorCode::ProtocolMismatch,
                        message: "protocol mismatch".into(),
                    },
                    Request::Shutdown => Reply::Ok {
                        data: ReplyData::Done,
                    },
                    _ => continue,
                };
                let stop = matches!(result, Reply::Ok { .. });
                let reply = ServerMsg::Response {
                    req_id: msg.req_id,
                    result,
                };
                puppy_proto::write_msg(&mut wr, &reply).await.unwrap();
                if stop {
                    let _ = std::fs::remove_file(&sock);
                    return;
                }
            }
        }
    });
    let hint = "an incompatible puppy daemon is running \u{2014} run `puppy stop` first";
    for args in [&["ls"][..], &["status"], &["info", "1"], &["wait", "1"]] {
        let (status, _, err) = env.run(args);
        assert_eq!(status.code(), Some(1), "{args:?}: {err}");
        assert!(err.trim_end().ends_with(hint), "{args:?}: {err}");
    }
    let (status, out, err) = env.run(&["stop"]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), "stopped");
    tokio::time::timeout(TIMEOUT, fake).await.unwrap().unwrap();
}

// ---- torrent file selection and seeding (through the real binary, local seeder) ----

// The torrent engine's local seeder harness (librqbit is a dev-dependency here only).
#[path = "../../puppy-torrent/tests/support/mod.rs"]
mod support;

use support::{MULTI_NAME, Seeder};

const KIB_USIZE: usize = 1024;
/// A multi-file torrent of ~1 MiB.
const MULTI: &[(&str, usize)] = &[
    ("a.bin", 600 * KIB_USIZE),
    ("sub/b.bin", 400 * KIB_USIZE + 123),
];

/// An environment whose daemon (and so every torrent) knows the seeders, with a
/// connection held open so the daemon does not idle-exit between commands.
async fn torrent_env(seeders: &[&Seeder]) -> (Env, puppy_client::Client) {
    let mut env = Env::new();
    env.peers = seeders.iter().map(|s| s.addr).collect();
    let c = env.spawn_client().await.unwrap();
    (env, c)
}

/// Writes the seeder's `.torrent` into the test home; returns its path.
fn torrent_path(env: &Env, seeder: &Seeder) -> String {
    let path = env
        .home
        .path()
        .join(format!("{}.torrent", seeder.info_hash_hex));
    std::fs::write(&path, &seeder.torrent_bytes).unwrap();
    path.to_str().unwrap().to_string()
}

/// Runs `args`, expecting exit `code`; returns (stdout, stderr).
fn run_code(env: &Env, args: &[&str], code: i32) -> (String, String) {
    let (status, out, err) = env.run(args);
    assert_eq!(status.code(), Some(code), "{args:?}: out {out}, err {err}");
    (out, err)
}

/// `puppy --json add ...`: the id of the one torrent added.
fn add_torrent(env: &Env, args: &[&str]) -> String {
    let mut all = vec!["--json", "add"];
    all.extend(args);
    let (out, _) = run_code(env, &all, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    v["added"][0]["id"].to_string()
}

/// Polls `puppy --json ls` until the download's state word is `state`.
fn wait_ls_state(env: &Env, id: &str, state: &str) -> serde_json::Value {
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let all = snapshots(env, &["--json", "ls"]);
        let d = all
            .into_iter()
            .find(|d| d["id"] == id.parse::<u64>().unwrap())
            .unwrap();
        let kind = d["state"]["kind"].as_str().unwrap_or("").to_string();
        if kind == state {
            return d;
        }
        assert!(std::time::Instant::now() < deadline, "never {state}: {d}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn assert_only_b_downloaded(env: &Env) {
    let folder = env.dl_dir().join(MULTI_NAME);
    assert!(
        std::fs::read(folder.join("sub/b.bin")).unwrap()
            == support::content("sub/b.bin", MULTI[1].1),
        "the selected file differs"
    );
    // a.bin is not a whole number of pieces: its last piece is shared with b.bin, and
    // librqbit may write that piece's bytes into a.bin too (up to a.bin's full length).
    // So compare contents, not the size.
    let a = std::fs::read(folder.join("a.bin")).unwrap_or_default();
    assert!(
        a != support::content("a.bin", MULTI[0].1),
        "the unselected file was downloaded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_files_spec_and_files_listing() {
    let seeder = Seeder::start(MULTI).await;
    let (env, _c) = torrent_env(&[&seeder]).await;
    let torrent = torrent_path(&env, &seeder);
    let dl = env.dl_dir();
    let dl = dl.to_str().unwrap();

    // Usage errors, before anything is added.
    for bad in [
        &["--files", "0"][..],
        &["--files", "3-1"],
        &["--files", ""],
        &["--files", "1", "--choose"],
        &["--seed-ratio", "lots"],
        &["--seed-time", "5"],
    ] {
        let mut args = vec!["add", &torrent];
        args.extend(bad);
        run_code(&env, &args, 2);
    }
    let (_, err) = run_code(
        &env,
        &["add", "--files", "1", "http://127.0.0.1:9/x.bin"],
        2,
    );
    assert!(err.contains("only applies to torrents"), "{err}");
    assert!(snapshots(&env, &["--json", "ls"]).is_empty());

    // A number past the last file: the daemon refuses it up front; usage-class failure.
    let (out, _) = run_code(&env, &["--json", "add", "--files", "9", &torrent], 2);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["failed"][0]["code"], 2, "{out}");
    assert!(v["added"].as_array().unwrap().is_empty(), "{out}");
    assert!(snapshots(&env, &["--json", "ls"]).is_empty());

    // A glob selects `sub/b.bin` only.
    let id = add_torrent(&env, &["-o", dl, "--files", "sub/*", &torrent]);
    run_code(&env, &["wait", &id], 0);
    assert_only_b_downloaded(&env);

    // `puppy files`: numbered from 1; JSON has 0-based `index` and 1-based `number`.
    let (out, _) = run_code(&env, &["--json", "files", &id], 0);
    let files: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(files.len(), 2, "{out}");
    for (i, f) in files.iter().enumerate() {
        assert_eq!(f["index"], i);
        assert_eq!(f["number"], i + 1);
    }
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(
        paths.contains(&"a.bin") && paths.contains(&"sub/b.bin"),
        "{out}"
    );
    assert!(files.iter().all(|f| f["size"].as_u64().unwrap() > 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn choose_then_select() {
    let seeder = Seeder::start(MULTI).await;
    let (env, _c) = torrent_env(&[&seeder]).await;
    let torrent = torrent_path(&env, &seeder);
    let dl = env.dl_dir();
    let id = add_torrent(&env, &["-o", dl.to_str().unwrap(), "--choose", &torrent]);
    wait_ls_state(&env, &id, "choosing_files");

    // The text listing: header, then `#  SIZE  PATH` rows numbered from 1.
    let (out, _) = run_code(&env, &["files", &id], 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with('#') && lines[0].contains("SIZE") && lines[0].contains("PATH"));
    assert!(
        lines[1].starts_with('1') && lines[2].starts_with('2'),
        "{out}"
    );
    let number_of_b = lines
        .iter()
        .find(|l| l.ends_with("sub/b.bin"))
        .map(|l| l.split_whitespace().next().unwrap().to_string())
        .expect("b.bin listed");

    // Bad choices keep it choosing: out of range is a usage error, unknown id exit 3.
    let (_, err) = run_code(&env, &["select", &id, "9"], 2);
    assert!(err.contains("out of range"), "{err}");
    run_code(&env, &["select", &id, "0"], 2);
    run_code(&env, &["select", "999", "1"], 3);
    run_code(&env, &["files", "999"], 3);
    wait_ls_state(&env, &id, "choosing_files");

    run_code(&env, &["select", &id, &number_of_b], 0);
    run_code(&env, &["wait", &id], 0);
    assert_only_b_downloaded(&env);
    // The choice is made once.
    run_code(&env, &["select", &id, "1"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_on_choosing_exits_5() {
    let seeder = Seeder::start(MULTI).await;
    let (env, _c) = torrent_env(&[&seeder]).await;
    let torrent = torrent_path(&env, &seeder);
    let (out, err) = run_code(&env, &["--json", "add", "--choose", "--wait", &torrent], 5);
    assert!(err.contains("choose files: puppy select"), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["final"][0]["state"]["kind"], "choosing_files", "{out}");
    let id = v["added"][0]["id"].to_string();
    let (_, err) = run_code(&env, &["wait", &id], 5);
    assert!(err.contains(&format!("puppy select {id}")), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_seeding_exits_0() {
    let seeder = Seeder::start(MULTI).await;
    let (env, _c) = torrent_env(&[&seeder]).await;
    let torrent = torrent_path(&env, &seeder);
    let dl = env.dl_dir();
    let (out, _) = run_code(
        &env,
        &["add", "--wait", "-o", dl.to_str().unwrap(), &torrent],
        0,
    );
    assert!(out.contains("completed"), "{out}");
    seeder.assert_downloaded_into(&dl);
    let id = out
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    // Done downloading, now seeding (the default ratio of 1.0 is not reached).
    let d = wait_ls_state(&env, id, "seeding");
    assert_eq!(d["state"]["ratio_target"], 1.0);
    run_code(&env, &["wait", id], 0);
    let (out, _) = run_code(&env, &["info", id], 0);
    assert!(out.contains("seeding") && out.contains("stops at"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_command_overrides() {
    let seeder = Seeder::start(MULTI).await;
    let (env, _c) = torrent_env(&[&seeder]).await;
    let torrent = torrent_path(&env, &seeder);
    let dl = env.dl_dir();
    let id = add_torrent(
        &env,
        &[
            "-o",
            dl.to_str().unwrap(),
            "--seed-ratio",
            "2.5",
            "--seed-time",
            "45m",
            &torrent,
        ],
    );
    let d = wait_ls_state(&env, &id, "seeding");
    assert_eq!(d["state"]["ratio_target"], 2.5, "{d}");
    assert_eq!(d["state"]["time_target_secs"], 45 * 60, "{d}");

    // `seed` replaces the whole policy: the time goes back to the daemon's 2h.
    let (out, _) = run_code(&env, &["seed", &id, "--ratio", "0"], 0);
    assert!(out.contains("ratio off"), "{out}");
    let d = wait_ls_state(&env, &id, "seeding");
    assert_eq!(d["state"]["ratio_target"], serde_json::Value::Null, "{d}");
    assert_eq!(d["state"]["time_target_secs"], 2 * 3600, "{d}");

    run_code(&env, &["seed", &id, "--ratio", "3", "--time", "10m"], 0);
    let d = wait_ls_state(&env, &id, "seeding");
    assert_eq!(d["state"]["ratio_target"], 3.0, "{d}");
    assert_eq!(d["state"]["time_target_secs"], 600, "{d}");
    run_code(
        &env,
        &["seed", &id, "--ratio", "default", "--time", "default"],
        0,
    );
    let d = wait_ls_state(&env, &id, "seeding");
    assert_eq!(d["state"]["ratio_target"], 1.0, "{d}");

    // Usage errors; unknown id.
    run_code(&env, &["seed", &id], 2);
    run_code(&env, &["seed", &id, "--time", "tomorrow"], 2);
    run_code(&env, &["seed", "999", "--ratio", "1"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_torrent_downloads_have_no_files_or_seed_policy() {
    let server = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        len: 16 * KIB,
        ..Default::default()
    })
    .await;
    let env = Env::new();
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&server, "f.bin"), false, None).await.unwrap();
    wait_completed(&c, id).await;
    let id = id.to_string();
    for args in [
        vec!["files", &id],
        vec!["select", &id, "1"],
        vec!["seed", &id, "--ratio", "1"],
    ] {
        let (_, err) = run_code(&env, &args, 1);
        assert!(err.contains("Unsupported"), "{args:?}: {err}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_new_states() {
    let one = Seeder::start(MULTI).await;
    let two = Seeder::start(&[("only.bin", 40 * KIB_USIZE)]).await;
    let (env, _c) = torrent_env(&[&one, &two]).await;
    let dl = env.dl_dir();
    let dl = dl.to_str().unwrap();
    let choosing = add_torrent(&env, &["-o", dl, "--choose", &torrent_path(&env, &one)]);
    let seeding = add_torrent(&env, &["-o", dl, &torrent_path(&env, &two)]);
    wait_ls_state(&env, &choosing, "choosing_files");
    wait_ls_state(&env, &seeding, "seeding");

    let ids = |args: &[&str]| -> Vec<String> {
        snapshots(&env, args)
            .iter()
            .map(|d| d["id"].to_string())
            .collect()
    };
    assert_eq!(
        ids(&["--json", "ls", "--state", "choosing"]),
        std::slice::from_ref(&choosing)
    );
    assert_eq!(
        ids(&["--json", "ls", "--state", "seeding"]),
        std::slice::from_ref(&seeding)
    );
    assert_eq!(ids(&["--json", "ls", "-s", "choosing,seeding"]).len(), 2);
    // `done` is completed or seeding; `completed` is completed only.
    assert_eq!(
        ids(&["--json", "ls", "--state", "done"]),
        std::slice::from_ref(&seeding)
    );
    assert!(ids(&["--json", "ls", "--state", "completed"]).is_empty());

    let (out, _) = run_code(&env, &["ls"], 0);
    let row = |id: &str| {
        out.lines()
            .find(|l| l.split_whitespace().next() == Some(id))
            .unwrap_or_else(|| panic!("no row {id}: {out}"))
            .to_string()
    };
    assert!(row(&choosing).contains("choosing"), "{out}");
    let s = row(&seeding);
    assert!(s.contains("seeding") && s.contains("100%"), "{out}");
}

fn update_env(github: &puppy_testserver::github::FakeGitHub) -> Env {
    let mut env = Env::new();
    env.extra
        .push(("PUPPY_UPDATE_URL", github.api_base().to_string()));
    env
}

fn fake_release() -> puppy_testserver::github::FakeRelease {
    puppy_testserver::github::FakeRelease::new("0.2.0", puppy_daemon::update::TARGET)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_check_reports_available() {
    let github = puppy_testserver::github::FakeGitHub::start(fake_release()).await;
    let env = update_env(&github);
    let (status, out, err) = env.run(&["update", "--check"]);
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(out.trim(), "puppy 0.2.0 is available", "{err}");

    let (status, out, err) = env.run(&["update", "--check", "--json"]);
    assert!(status.success(), "{status:?}: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"]["state"], "available", "{out}");
    assert_eq!(v["status"]["version"], "0.2.0", "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_refused_in_dev_build() {
    let github = puppy_testserver::github::FakeGitHub::start(fake_release()).await;
    let env = update_env(&github);
    let (status, out, err) = env.run(&["update"]);
    assert_eq!(status.code(), Some(6), "{out} {err}");
    assert!(err.contains("puppy: cannot update: "), "{err}");
    assert!(err.contains("release workflow"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_mentions_update() {
    let github = puppy_testserver::github::FakeGitHub::start(fake_release()).await;
    let mut env = update_env(&github);
    // `status` reads what the daemon `update --check` talked to learnt: it must not
    // idle-exit in between (a new one starts `NotChecked` and has no check due).
    // Env's drop stops it.
    env.extra.push(("PUPPY_IDLE_EXIT", "30".to_string()));
    let (status, _, err) = env.run(&["update", "--check"]);
    assert!(status.success(), "{err}");
    let (status, out, err) = env.run(&["status"]);
    assert!(status.success(), "{err}");
    assert!(
        out.contains("update available: puppy 0.2.0 (puppy update)"),
        "{out}"
    );
    let (status, out, err) = env.run(&["status", "--json"]);
    assert!(status.success(), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["update"]["state"], "available", "{out}");
    assert_eq!(v["downloads"], 0, "{out}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_reports_missing_comeback() {
    // A stand-in that answers `--version` and otherwise sleeps instead of serving.
    let release = fake_release().binary_script(
        "if [ \"$1\" = \"--version\" ]; then echo 'puppy 0.2.0'; exit 0; fi\nexec sleep 30",
    );
    let github = puppy_testserver::github::FakeGitHub::start(release).await;
    let mut env = update_env(&github);
    env.extra.push(("PUPPY_UPDATE_ALLOW_DEV", "1".to_string()));

    // A copy of the real binary the update may overwrite.
    let bin_dir = tempfile::tempdir().unwrap();
    let exe = bin_dir.path().join("puppy");
    std::fs::copy(BIN, &exe).unwrap();

    let out_path = env.home.path().join("update-out");
    let err_path = env.home.path().join("update-err");
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("update").stdin(Stdio::null());
    for (k, v) in env.vars() {
        cmd.env(k, v);
    }
    cmd.stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap());
    let child = spawn_retrying(&mut cmd);
    let pid = env.daemon_pid();
    let status = wait_exit(child, std::time::Duration::from_secs(60));
    let out = std::fs::read_to_string(&out_path).unwrap();
    let err = std::fs::read_to_string(&err_path).unwrap();
    // The stand-in keeps the daemon's pid and sleeps; do not leave it behind.
    if let Some(p) = env.daemon_pid().or(pid) {
        unsafe { libc::kill(p as i32, libc::SIGKILL) };
    }
    assert_eq!(status.code(), Some(1), "{out} {err}");
    assert!(out.contains("updating puppy "), "{out}");
    assert!(out.contains("\u{2192} 0.2.0\u{2026}"), "{out}");
    assert!(
        err.contains("puppy: the daemon did not come back as 0.2.0"),
        "{err}"
    );
}

/// A test daemon that was not pointed at a fake release server still checks for
/// updates (main turns them on), but only ever at a dead local address: tests never
/// contact GitHub.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_daemons_stay_off_github() {
    let env = Env::new();
    let url = env
        .vars()
        .into_iter()
        .rev()
        .find(|(k, _)| *k == "PUPPY_UPDATE_URL")
        .map(|(_, v)| v);
    assert_eq!(url.as_deref(), Some("http://127.0.0.1:9"));
    // Connected, so the daemon stays up past its first check (5 s after start).
    let client = env.spawn_client().await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        let s = client.get_update().await.unwrap();
        if s != puppy_proto::UpdateStatus::NotChecked {
            break s;
        }
        assert!(std::time::Instant::now() < deadline, "no update check ran");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    match status {
        puppy_proto::UpdateStatus::Failed { error, .. } => {
            assert!(
                error.contains("could not reach the release server"),
                "{error}"
            );
        }
        s => panic!("the check reached a release server: {s:?}"),
    }
}
