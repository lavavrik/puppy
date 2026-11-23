//! `puppy native-host`, the native messaging host the browser extension starts.

mod common;

use common::*;
use std::io::{Read, Write};
use std::process::Stdio;

fn frame(v: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap();
    [&(body.len() as u32).to_ne_bytes()[..], &body].concat()
}

/// Splits host output into its messages.
fn frames(mut out: &[u8]) -> Vec<serde_json::Value> {
    let mut msgs = Vec::new();
    while !out.is_empty() {
        let len = u32::from_ne_bytes(out[..4].try_into().unwrap()) as usize;
        msgs.push(serde_json::from_slice(&out[4..4 + len]).unwrap());
        out = &out[4 + len..];
    }
    msgs
}

#[test]
fn native_host_is_hidden_and_takes_browser_args() {
    let env = Env::new();
    let (st, help, _) = env.run(&["--help"]);
    assert!(st.success());
    assert!(!help.contains("native-host"), "{help}");

    let (st, out, err) = env.run(&[
        "native-host",
        "chrome-extension://abcdefghijklmnopabcdefghijklmnop/",
        "--parent-window=0",
    ]);
    assert!(st.success(), "{err}");
    assert!(out.is_empty(), "{out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_host_binary_autostarts_daemon() {
    let env = Env::new();
    assert!(!env.lock_held());
    let mut child = env
        .command(&["native-host", "chrome-extension://x/"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&frame(&serde_json::json!({
            "id": "h1", "protocol": 1, "type": "hello", "extension_version": "0.1.0"
        })))
        .unwrap();
    stdin
        .write_all(&frame(&serde_json::json!({
            "id": "a1", "protocol": 1, "type": "add",
            "url": "https://us%40er:pw@127.0.0.1:9/never.bin",
            "filename": "never.bin",
            "cookies": "sid=SECRET",
        })))
        .unwrap();
    drop(stdin);
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).unwrap();
        out
    });
    let status = wait_exit(child, TIMEOUT);
    assert!(status.success(), "{status:?}");
    let replies = frames(&reader.join().unwrap());
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert_eq!(replies[0]["id"], "h1");
    assert_eq!(replies[0]["type"], "hello");
    assert!(replies[0]["daemon_version"].is_string(), "{replies:?}");
    assert_eq!(replies[1]["id"], "a1");
    assert_eq!(replies[1]["type"], "accepted", "{replies:?}");
    let id = replies[1]["download_id"].as_u64().unwrap();

    let (st, ls, err) = env.run(&["--json", "ls"]);
    assert!(st.success(), "{err}");
    let ls: serde_json::Value = serde_json::from_str(&ls).unwrap();
    let ls = ls.as_array().unwrap();
    assert_eq!(ls.len(), 1, "{ls:?}");
    assert_eq!(ls[0]["id"].as_u64(), Some(id));
    assert_eq!(ls[0]["source"], "https://127.0.0.1:9/never.bin");
    let (_, info, _) = env.run(&["info", &id.to_string()]);
    assert!(
        !info.contains("us@er") && !info.contains("us%40er"),
        "{info}"
    );
    assert!(!info.contains("SECRET"), "{info}");
}
