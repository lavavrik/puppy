//! Metalinks through the real binary and daemon: local and fetched documents, mirrors
//! over HTTP and FTP, hashes and sizes, several files with subdirectories, input files
//! and `--json`.

mod common;

use common::*;
use puppy_testserver::ftp::{FtpCfg, FtpServer};
use puppy_testserver::{ServerCfg, TestServer, body_byte, body_sha256};
use std::path::{Path, PathBuf};

const LEN: u64 = 4 * 1024 * KIB;

fn data(len: u64) -> Vec<u8> {
    (0..len).map(|i| body_byte(i, 0)).collect()
}

async fn server(len: u64) -> TestServer {
    TestServer::start(ServerCfg {
        len,
        ..ServerCfg::default()
    })
    .await
}

/// A Metalink 4 document of `files`: (name, size, hash, urls).
fn meta4(files: &[(&str, u64, Option<String>, Vec<String>)]) -> String {
    let mut doc = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <metalink xmlns=\"urn:ietf:params:xml:ns:metalink\">\n\
         <generator>puppy-tests</generator>\n",
    );
    for (name, size, hash, urls) in files {
        doc.push_str(&format!("<file name=\"{name}\">\n<size>{size}</size>\n"));
        if let Some(h) = hash {
            doc.push_str(&format!("<hash type=\"md5\">{}</hash>\n", "0".repeat(32)));
            doc.push_str(&format!("<hash type=\"sha-256\">{h}</hash>\n"));
        }
        for (i, u) in urls.iter().enumerate() {
            doc.push_str(&format!(
                "<url priority=\"{}\" location=\"de\">{}</url>\n",
                i + 1,
                u.replace('&', "&amp;")
            ));
        }
        doc.push_str("<metaurl mediatype=\"torrent\">http://127.0.0.1:9/x.torrent</metaurl>\n");
        doc.push_str("</file>\n");
    }
    doc.push_str("</metalink>\n");
    doc
}

fn write(env: &Env, name: &str, body: &str) -> PathBuf {
    let dir = env.home.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn json(out: &str) -> serde_json::Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

fn assert_data(path: &Path, len: u64) {
    let got = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(got == data(len), "{} has other bytes", path.display());
}

fn ids(env: &Env) -> usize {
    let (status, out, err) = env.run(&["--json", "ls"]);
    assert!(status.success(), "{err}");
    json(&out).as_array().unwrap().len()
}

/// A local `.meta4` with two HTTP mirrors and a sha-256: one download, from both
/// locations, verified; `--json` names the file and the metalink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_meta4_with_two_mirrors() {
    let env = Env::new();
    let (a, b) = (server(LEN).await, server(LEN).await);
    let doc = meta4(&[(
        "image.iso",
        LEN,
        Some(body_sha256(LEN, 0)),
        vec![
            a.url("/file.bin").to_string(),
            b.url("/file.bin").to_string(),
        ],
    )]);
    let ml = write(&env, "image.iso.meta4", &doc);
    let dl = env.dl_dir();
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        ml.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert!(!err.contains("warning"), "{err}");
    let v = json(&out);
    let added = v["added"].as_array().unwrap();
    assert_eq!(added.len(), 1, "{out}");
    assert_eq!(added[0]["name"], "image.iso", "{out}");
    assert_eq!(added[0]["metalink"], ml.to_str().unwrap(), "{out}");
    assert_eq!(added[0]["source"], a.url("/file.bin").to_string(), "{out}");
    let fin = &v["final"][0];
    assert_eq!(fin["state"]["kind"], "completed", "{out}");
    assert_eq!(fin["checksum"]["algo"], "sha-256", "{out}");
    assert_eq!(
        fin["mirrors"],
        serde_json::json!([b.url("/file.bin").to_string()]),
        "{out}"
    );
    assert_data(&dl.join("image.iso"), LEN);
    assert!(
        a.range_requests() > 0 && b.range_requests() > 0,
        "both mirrors serve ranges: {} and {}",
        a.range_requests(),
        b.range_requests()
    );

    // The text form says what was added.
    let (status, out, err) = env.run(&[
        "add",
        "--paused",
        "-o",
        dl.to_str().unwrap(),
        ml.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(out.trim(), "added 2 image.iso");
}

/// HTTP and FTP locations of one file in one metalink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_and_ftp_mirrors() {
    let env = Env::new();
    let h = server(LEN).await;
    let f = FtpServer::start(FtpCfg::default()).await;
    f.set_file("pub/file.bin", data(LEN), "20261126120000");
    let doc = meta4(&[(
        "mixed.bin",
        LEN,
        Some(body_sha256(LEN, 0)),
        vec![
            f.url("pub/file.bin").to_string(),
            h.url("/file.bin").to_string(),
        ],
    )]);
    let ml = write(&env, "mixed.meta4", &doc);
    let dl = env.dl_dir();
    let (status, out, err) = env.run(&[
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        ml.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert_data(&dl.join("mixed.bin"), LEN);
    assert!(f.retrs() > 0, "the FTP location was not used");
    assert!(h.range_requests() > 0, "the HTTP location was not used");
}

/// A wrong hash fails the download (exit 4); a wrong size fails the add.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hash_and_size_mismatches_fail() {
    let env = Env::new();
    let s = server(256 * KIB).await;
    let url = vec![s.url("/file.bin").to_string()];
    let dl = env.dl_dir();
    let bad_hash = write(
        &env,
        "bad-hash.meta4",
        &meta4(&[("a.bin", 256 * KIB, Some("ab".repeat(32)), url.clone())]),
    );
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        bad_hash.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(4), "{out}\n{err}");
    let v = json(&out);
    let reason = v["final"][0]["state"]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("checksum"), "{out}");

    let bad_size = write(
        &env,
        "bad-size.meta4",
        &meta4(&[("b.bin", 256 * KIB + 1, None, url)]),
    );
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        bad_size.to_str().unwrap(),
    ]);
    assert_ne!(status.code(), Some(0), "{out}\n{err}");
    assert!(
        out.contains("not the expected 262145") || err.contains("not the expected 262145"),
        "{out}\n{err}"
    );
    assert!(!dl.join("b.bin").exists());
}

/// Served over HTTP: a `.meta4` URL is fetched and expanded; any other URL is with
/// `--metalink` (sent with an Accept asking for Metalink); `--no-metalink` downloads
/// the document itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metalinks_served_over_http() {
    let env = Env::new();
    let s = server(256 * KIB).await;
    let doc = meta4(&[(
        "served.bin",
        256 * KIB,
        Some(body_sha256(256 * KIB, 0)),
        vec![s.url("/file.bin").to_string()],
    )]);
    s.set_doc(
        "/served.bin.meta4",
        "application/metalink4+xml",
        doc.clone(),
    );
    s.set_doc("/metalink", "application/metalink4+xml", doc.clone());
    let dl = env.dl_dir();
    let dir = dl.to_str().unwrap();
    let (status, out, err) = env.run(&[
        "add",
        "--wait",
        "-o",
        dir,
        s.url("/served.bin.meta4").as_str(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert_data(&dl.join("served.bin"), 256 * KIB);
    let accept = &s.requests_seen()[0]["accept"];
    assert!(
        accept
            .to_str()
            .unwrap()
            .starts_with("application/metalink4+xml"),
        "{accept:?}"
    );

    let other = s.url("/metalink?repo=x&arch=y");
    let (status, out, err) = env.run(&["--json", "add", "--paused", "-o", dir, other.as_str()]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert!(json(&out)["added"][0]["metalink"].is_null(), "{out}");
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--metalink",
        "--paused",
        "-o",
        dir,
        other.as_str(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert_eq!(json(&out)["added"][0]["name"], "served.bin", "{out}");

    let (status, out, err) = env.run(&[
        "add",
        "--no-metalink",
        "--wait",
        "-o",
        dir,
        s.url("/served.bin.meta4").as_str(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert_eq!(
        std::fs::read_to_string(dl.join("served.bin.meta4")).unwrap(),
        doc
    );

    // An error page instead of a metalink: not added (exit 1).
    s.set_doc("/gone.meta4", "text/html", "<html>gone</html>");
    let (status, _, err) = env.run(&["add", "-o", dir, s.url("/gone.meta4").as_str()]);
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(
        err.contains("not a metalink: the server sent text/html"),
        "{err}"
    );
}

/// Several files: one download each, subdirectories kept and names made safe, in the
/// output folder or (without `-o`) the daemon's download folder. `--name` and
/// `--checksum` cannot apply; an unsafe name refuses the whole metalink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_file_metalink() {
    let env = Env::new();
    let daemon_dir = env.home.path().join("daemon-dl");
    let config = env.paths().config_file();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!("download_dir = {:?}\n", daemon_dir.to_str().unwrap()),
    )
    .unwrap();
    let s = server(64 * KIB).await;
    let url = || vec![s.url("/file.bin").to_string()];
    let sum = || Some(body_sha256(64 * KIB, 0));
    let doc = meta4(&[
        ("top.bin", 64 * KIB, sum(), url()),
        ("iso/x86_64/deep.bin", 64 * KIB, sum(), url()),
        ("CON/a:b?.bin", 64 * KIB, sum(), url()),
    ]);
    let ml = write(&env, "set.meta4", &doc);
    let dl = env.dl_dir();
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        ml.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    let v = json(&out);
    let names: Vec<&str> = v["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["top.bin", "iso/x86_64/deep.bin", "_CON/a_b_.bin"]);
    for (i, f) in v["final"].as_array().unwrap().iter().enumerate() {
        assert_eq!(f["state"]["kind"], "completed", "{i}: {out}");
    }
    assert_data(&dl.join("top.bin"), 64 * KIB);
    assert_data(&dl.join("iso").join("x86_64").join("deep.bin"), 64 * KIB);
    assert_data(&dl.join("_CON").join("a_b_.bin"), 64 * KIB);

    // No -o: below the daemon's download folder.
    let (status, out, err) = env.run(&["add", "--wait", ml.to_str().unwrap()]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert_data(
        &daemon_dir.join("iso").join("x86_64").join("deep.bin"),
        64 * KIB,
    );
    assert_data(&daemon_dir.join("top.bin"), 64 * KIB);

    // Options for one file only.
    let before = ids(&env);
    for (flag, value) in [
        ("--name", "x.bin".to_string()),
        ("--checksum", format!("sha-256={}", "ab".repeat(32))),
        ("--mirror", s.url("/file.bin").to_string()),
    ] {
        let (status, _, err) = env.run(&["add", flag, &value, ml.to_str().unwrap()]);
        assert_eq!(status.code(), Some(2), "{flag}: {err}");
        assert!(err.contains("it lists 3 files"), "{flag}: {err}");
    }
    assert_eq!(ids(&env), before, "nothing was added");

    // A name that leaves the output folder: nothing of that metalink is added.
    let evil = write(
        &env,
        "evil.meta4",
        &meta4(&[
            ("fine.bin", 64 * KIB, sum(), url()),
            ("../../escape.bin", 64 * KIB, sum(), url()),
        ]),
    );
    let (status, _, err) = env.run(&["add", "-o", dl.to_str().unwrap(), evil.to_str().unwrap()]);
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(err.contains("could leave the output folder"), "{err}");
    assert_eq!(ids(&env), before);
    assert!(!dl.join("fine.bin").exists());
}

/// An input file with a metalink line (its options apply to every file) next to a
/// plain URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_file_with_a_metalink_line() {
    let env = Env::new();
    let s = server(64 * KIB).await;
    let url = || vec![s.url("/file.bin").to_string()];
    let sum = || Some(body_sha256(64 * KIB, 0));
    s.set_doc(
        "/pair.meta4",
        "application/metalink4+xml",
        meta4(&[
            ("one.bin", 64 * KIB, sum(), url()),
            ("sub/two.bin", 64 * KIB, sum(), url()),
        ]),
    );
    let local = write(
        &env,
        "single.meta4",
        &meta4(&[("three.bin", 64 * KIB, sum(), url())]),
    );
    let dl = env.dl_dir();
    let other = env.home.path().join("other");
    let list = write(
        &env,
        "list.txt",
        &format!(
            "{}\n  dir={}\n  max-download-limit=1M\n{}\n  out=renamed.bin\n{}\n  out=plain.bin\n",
            s.url("/pair.meta4"),
            other.display(),
            local.display(),
            s.url("/file.bin"),
        ),
    );
    let (status, out, err) = env.run(&[
        "--json",
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        "-i",
        list.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    let v = json(&out);
    assert_eq!(v["added"].as_array().unwrap().len(), 4, "{out}");
    assert_eq!(v["final"][0]["limit"], 1024 * KIB, "{out}");
    assert_eq!(v["final"][1]["limit"], 1024 * KIB, "{out}");
    assert_data(&other.join("one.bin"), 64 * KIB);
    assert_data(&other.join("sub").join("two.bin"), 64 * KIB);
    assert_data(&dl.join("renamed.bin"), 64 * KIB);
    assert_data(&dl.join("plain.bin"), 64 * KIB);

    // `out` cannot name two files, and nothing is added then.
    let before = ids(&env);
    let bad = write(
        &env,
        "bad.txt",
        &format!("{}\n  out=x.bin\n", s.url("/pair.meta4")),
    );
    let (status, _, err) = env.run(&["add", "-i", bad.to_str().unwrap()]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("line 1: out does not go with"), "{err}");
    // Neither can TAB-separated mirrors.
    let tabbed = write(
        &env,
        "tabbed.txt",
        &format!("{}\t{}\n", local.display(), s.url("/file.bin")),
    );
    let (status, _, err) = env.run(&["add", "-i", tabbed.to_str().unwrap()]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("is a metalink"), "{err}");
    assert_eq!(ids(&env), before);
}

#[test]
fn metalink_flag_usage_errors() {
    let env = Env::new();
    for (args, want) in [
        (
            vec!["add", "--metalink", "magnet:?xt=urn:btih:abc"],
            "not an HTTP(S) URL",
        ),
        (
            vec!["add", "--metalink", "/nonexistent/a.meta4"],
            "no such file",
        ),
        (
            vec!["add", "--metalink", "--no-metalink", "/x.meta4"],
            "cannot be used with",
        ),
    ] {
        let (status, _, err) = env.run(&args);
        assert_eq!(status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains(want), "{args:?}: {err}");
    }
}

/// An SFTP location next to an HTTP one: the scheme filter is the transports', so a
/// metalink's `sftp://` URLs are used like any other.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sftp_and_http_mirrors() {
    use puppy_testserver::sftp::{SftpCfg, SftpServer, key_path};
    let mut env = Env::new();
    let home = env.home.path().join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::copy(key_path("user_ed25519"), home.join(".ssh/id_ed25519")).unwrap();
    env.extra.push(("HOME", home.to_str().unwrap().into()));
    env.extra.push(("SSH_AUTH_SOCK", String::new()));
    let sftp = SftpServer::start(SftpCfg::default()).await;
    sftp.set_file("/srv/file.bin", data(LEN), 1_795_000_000);
    let target = format!("127.0.0.1:{}", sftp.port());
    let (status, out, err) = env.run(&["ssh", "trust", "--yes", &target]);
    assert!(status.success(), "{out}\n{err}");
    let h = server(LEN).await;
    let doc = meta4(&[(
        "via-sftp.bin",
        LEN,
        Some(body_sha256(LEN, 0)),
        vec![
            sftp.url("/srv/file.bin").to_string(),
            h.url("/file.bin").to_string(),
        ],
    )]);
    let ml = write(&env, "sftp.meta4", &doc);
    let dl = env.dl_dir();
    let (status, out, err) = env.run(&[
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        ml.to_str().unwrap(),
    ]);
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert_data(&dl.join("via-sftp.bin"), LEN);
    assert!(sftp.opens() > 0, "the SFTP location was not used");
    assert!(h.range_requests() > 0, "the HTTP location was not used");
}

/// Cookies, headers and a referer cannot be scoped to a site for a local metalink:
/// refused (exit 2, nothing added); a user agent is fine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_with_a_local_metalink_are_refused() {
    let env = Env::new();
    let s = server(64 * KIB).await;
    let ml = write(
        &env,
        "local.meta4",
        &meta4(&[(
            "l.bin",
            64 * KIB,
            None,
            vec![s.url("/file.bin").to_string()],
        )]),
    );
    let ml = ml.to_str().unwrap();
    for extra in [
        vec!["--cookie", "sid=X"],
        vec!["-H", "Authorization: Bearer T"],
        vec!["-H", "X-Custom: c"],
        vec!["--referer", "https://a.example/"],
    ] {
        let mut args = vec!["add", "--paused"];
        args.extend(&extra);
        args.push(ml);
        let (status, _, err) = env.run(&args);
        assert_eq!(status.code(), Some(2), "{extra:?}: {err}");
        assert!(err.contains("do not go with a local metalink"), "{err}");
    }
    let list = write(&env, "l.txt", &format!("{ml}\n  header=X-Custom: c\n"));
    let (status, _, err) = env.run(&["add", "--paused", "-i", list.to_str().unwrap()]);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("line 1: --cookie"), "{err}");
    assert_eq!(ids(&env), 0);

    let dl = env.dl_dir();
    let (status, _, err) = env.run(&[
        "add",
        "--wait",
        "-o",
        dl.to_str().unwrap(),
        "-H",
        "User-Agent: custom-agent",
        ml,
    ]);
    assert_eq!(status.code(), Some(0), "{err}");
    let seen = s.requests_seen();
    assert!(
        seen.iter().all(|h| h["user-agent"] == "custom-agent"),
        "{seen:?}"
    );
}
