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

#[cfg(unix)]
mod install {
    use super::common::*;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    const ID: &str = "cbfmabianfipnmdcdipmcpaggpejlaeg";

    /// `puppy <args>` with a fake `HOME` and no `XDG_CONFIG_HOME`, so nothing outside the
    /// temp dir is touched.
    fn run(env: &Env, home: &Path, args: &[&str]) -> (std::process::ExitStatus, String, String) {
        let out = home.join("stdout");
        let err = home.join("stderr");
        let child = env
            .command(args)
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .stdout(std::fs::File::create(&out).unwrap())
            .stderr(std::fs::File::create(&err).unwrap())
            .spawn()
            .unwrap();
        let st = wait_exit(child, TIMEOUT);
        (
            st,
            std::fs::read_to_string(out).unwrap(),
            std::fs::read_to_string(err).unwrap(),
        )
    }

    /// (config dir, manifest) of a browser under `home`, per the Linux or macOS layout.
    fn dirs(home: &Path, linux: &str, mac: &str) -> (PathBuf, PathBuf) {
        let cfg = if cfg!(target_os = "macos") {
            home.join("Library/Application Support").join(mac)
        } else {
            home.join(".config").join(linux)
        };
        let m = cfg.join("NativeMessagingHosts/dev.puppy.native_host.json");
        (cfg, m)
    }

    fn chrome(home: &Path) -> (PathBuf, PathBuf) {
        dirs(home, "google-chrome", "Google/Chrome")
    }
    fn brave(home: &Path) -> (PathBuf, PathBuf) {
        dirs(
            home,
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser",
        )
    }
    fn chromium(home: &Path) -> (PathBuf, PathBuf) {
        dirs(home, "chromium", "Chromium")
    }

    fn fake_home(env: &Env) -> PathBuf {
        let h = env.home.path().join("fakehome");
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    fn browser_status(env: &Env, home: &Path, name: &str) -> Value {
        let (st, out, err) = run(env, home, &["--json", "browser", "status"]);
        assert!(st.success(), "{err}");
        let v: Value = serde_json::from_str(&out).unwrap();
        v["browsers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["browser"] == name)
            .unwrap()
            .clone()
    }

    #[test]
    fn install_status_uninstall_under_temp_home() {
        let env = Env::new();
        let home = fake_home(&env);
        let exe = std::fs::canonicalize(BIN).unwrap();
        let (c, cm) = chrome(&home);
        let (b, bm) = brave(&home);
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let (st, _, err) = run(&env, &home, &["browser", "install"]);
        assert!(st.success(), "{err}");
        for m in [&cm, &bm] {
            let v: Value = serde_json::from_slice(&std::fs::read(m).unwrap()).unwrap();
            assert_eq!(v["path"], exe.to_str().unwrap());
            assert_eq!(
                v["allowed_origins"],
                serde_json::json!([format!("chrome-extension://{ID}/")])
            );
        }
        assert!(!chromium(&home).0.exists());

        let ch = browser_status(&env, &home, "chrome");
        assert_eq!(ch["found"], true);
        assert_eq!(ch["host"]["state"], "ok");
        let cr = browser_status(&env, &home, "chromium");
        assert_eq!(cr["found"], false);
        assert_eq!(cr["host"]["state"], "missing");

        let (st, out, _) = run(&env, &home, &["browser", "status"]);
        assert!(st.success());
        let line = out.lines().find(|l| l.starts_with("chrome ")).unwrap();
        assert!(line.contains("ok"), "{out}");
        assert_eq!(out.lines().last().unwrap(), format!("extension id: {ID}"));

        let (st, _, err) = run(&env, &home, &["browser", "uninstall"]);
        assert!(st.success(), "{err}");
        assert!(!cm.exists() && !bm.exists());
        assert_eq!(
            browser_status(&env, &home, "chrome")["host"]["state"],
            "missing"
        );
        let (st, out, _) = run(&env, &home, &["browser", "uninstall"]);
        assert!(st.success());
        assert!(out.contains("nothing to remove"), "{out}");
    }

    #[test]
    fn install_without_browsers_fails_unless_forced() {
        let env = Env::new();
        let home = fake_home(&env);
        let (st, _, err) = run(&env, &home, &["browser", "install"]);
        assert_eq!(st.code(), Some(1));
        assert!(err.contains("--browser"), "{err}");

        // One named browser that is not there: nothing is created without --force.
        let (st, _, err) = run(
            &env,
            &home,
            &["browser", "install", "--browser", "chromium"],
        );
        assert_eq!(st.code(), Some(1), "{err}");
        assert!(
            err.contains("chromium not found") && err.contains("--force"),
            "{err}"
        );
        assert!(!chromium(&home).0.exists());
        assert!(!home.join(".config").exists() && !home.join("Library").exists());

        let (st, _, err) = run(
            &env,
            &home,
            &["browser", "install", "--browser", "chromium", "--force"],
        );
        assert!(st.success(), "{err}");
        assert!(chromium(&home).1.exists());
    }

    #[test]
    fn install_for_a_found_browser_creates_only_the_hosts_dir() {
        let env = Env::new();
        let home = fake_home(&env);
        let (c, cm) = chrome(&home);
        std::fs::create_dir_all(&c).unwrap();
        let (st, _, err) = run(&env, &home, &["browser", "install", "--browser", "chrome"]);
        assert!(st.success(), "{err}");
        assert!(cm.exists());
        // No other browser's config dir appeared.
        assert!(!chromium(&home).0.exists() && !brave(&home).0.exists());
    }

    #[test]
    fn stale_path_is_flagged() {
        let env = Env::new();
        let home = fake_home(&env);
        let (c, cm) = chrome(&home);
        std::fs::create_dir_all(&c).unwrap();
        let (st, _, err) = run(&env, &home, &["browser", "install"]);
        assert!(st.success(), "{err}");

        let mut v: Value = serde_json::from_slice(&std::fs::read(&cm).unwrap()).unwrap();
        v["path"] = "/old/puppy".into();
        std::fs::write(&cm, serde_json::to_vec(&v).unwrap()).unwrap();

        let (st, out, _) = run(&env, &home, &["browser", "status"]);
        assert!(st.success());
        let line = out.lines().find(|l| l.starts_with("chrome ")).unwrap();
        for want in ["stale", "/old/puppy", "re-run puppy browser install"] {
            assert!(line.contains(want), "{line}");
        }
        assert_eq!(
            browser_status(&env, &home, "chrome")["host"],
            serde_json::json!({"state": "stale_path", "found": "/old/puppy"})
        );
    }
}
