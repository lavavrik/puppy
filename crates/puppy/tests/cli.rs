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
