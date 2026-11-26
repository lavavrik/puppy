//! SFTP downloads through the URL engine, against the in-process test SSH server.

use puppy_core::checksum::Checksum;
use puppy_core::config::{HttpConfig, SftpConfig, SshAgent};
use puppy_core::engine::{AddOptions, Engine, EngineError, StartMode};
use puppy_core::http::engine::HttpEngine;
use puppy_core::http::resume::{part_path, sidecar_path};
use puppy_core::sftp::hostkey::{self, Verdict};
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadState, DownloadStatus, Mirror, RequestContext, Source,
};
use puppy_testserver::sftp::{HostKey, SftpCfg, SftpServer, key_path};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const KIB: u64 = 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const MTIME: u32 = 1_795_000_000;
const SECRET: &str = "hunter2-SECRET";

fn data(len: u64, version: u32) -> Vec<u8> {
    (0..len).map(|i| body_byte(i, version)).collect()
}

/// A test server and a directory with a `known_hosts` that trusts it.
struct Fx {
    dir: tempfile::TempDir,
    s: SftpServer,
}

impl Fx {
    async fn new(f: impl FnOnce(&mut SftpCfg)) -> Fx {
        let mut c = SftpCfg::default();
        f(&mut c);
        let s = SftpServer::start(c).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("known_hosts"),
            s.known_hosts_line(HostKey::Main),
        )
        .unwrap();
        Fx { dir, s }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Logs in with `keys/user_ed25519`; no agent.
    fn cfg(&self) -> HttpConfig {
        HttpConfig {
            connections: 4,
            min_segment_bytes: 32 * KIB,
            backoff_base_ms: 10,
            backoff_max_ms: 100,
            flush_interval_ms: 50,
            default_dir: self.path().to_path_buf(),
            sftp: SftpConfig {
                agent: SshAgent::Off,
                identity_files: vec![key_path("user_ed25519")],
                known_hosts: vec![self.path().join("known_hosts")],
                trusted_hosts: Some(self.path().join("trusted_hosts")),
            },
            ..HttpConfig::default()
        }
    }

    fn cfg_with(&self, f: impl FnOnce(&mut HttpConfig)) -> HttpConfig {
        let mut c = self.cfg();
        f(&mut c);
        c
    }

    fn request(&self, url: url::Url) -> AddRequest {
        AddRequest {
            source: Source::Http { url },
            output_dir: Some(self.path().join("dl")),
            filename: None,
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
            checksum: None,
            mirrors: Vec::new(),
            size: None,
        }
    }

    /// Downloads `url` with `cfg` and returns the file's bytes.
    async fn download(&self, url: url::Url, cfg: HttpConfig) -> Vec<u8> {
        let e = HttpEngine::new(cfg).unwrap();
        e.add(
            1,
            self.request(url),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await
        .unwrap();
        std::fs::read(completed(&e, 1).await).unwrap()
    }

    /// The error adding `url` with `cfg` fails with.
    async fn add_error(&self, url: url::Url, cfg: HttpConfig) -> EngineError {
        let e = HttpEngine::new(cfg).unwrap();
        e.add(
            1,
            self.request(url),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await
        .expect_err("the add fails")
    }
}

fn with_password(mut url: url::Url, pass: &str) -> url::Url {
    url.set_password(Some(pass)).unwrap();
    url
}

async fn wait(e: &HttpEngine, id: u64, pred: impl Fn(&DownloadStatus) -> bool) -> DownloadStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let st = e.status(id).expect("status");
        if pred(&st) {
            return st;
        }
        assert!(Instant::now() < deadline, "timed out; last {st:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn done(s: &DownloadStatus) -> bool {
    matches!(
        s.state,
        DownloadState::Completed
            | DownloadState::Failed { .. }
            | DownloadState::NeedsAttention { .. }
    )
}

async fn completed(e: &HttpEngine, id: u64) -> PathBuf {
    let st = wait(e, id, done).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    st.path
}

fn changed() -> DownloadState {
    DownloadState::NeedsAttention {
        reason: AttentionReason::RemoteChanged,
    }
}

#[tokio::test]
async fn key_file_login_and_a_segmented_download() {
    let fx = Fx::new(|_| {}).await;
    let len = 512 * KIB;
    fx.s.set_file("/srv/big.bin", data(len, 1), MTIME);
    let got = fx.download(fx.s.url("/srv/big.bin"), fx.cfg()).await;
    assert_eq!(got, data(len, 1));
    assert!(fx.path().join("dl/big.bin").exists());
    let logins = fx.s.logins();
    assert!(
        logins
            .iter()
            .all(|l| l == "publickey:puppy-test-user_ed25519"),
        "{logins:?}"
    );
    // Several connections read several ranges.
    let offsets = fx.s.read_offsets();
    let mut starts: Vec<u64> = offsets.iter().map(|o| o / (64 * KIB)).collect();
    starts.dedup();
    assert!(offsets.len() > 8, "{offsets:?}");
    assert!(fx.s.max_concurrent() >= 2, "{}", fx.s.max_concurrent());
    assert!(offsets.iter().any(|&o| o > len / 2), "{offsets:?}");
    assert!(offsets.iter().any(|&o| o > 0 && o < len / 2), "{offsets:?}");
    // Agent forwarding is never asked for.
    assert_eq!(fx.s.agent_requests(), 0);
}

#[tokio::test]
async fn an_rsa_key_logs_in() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("/f", data(64 * KIB, 2), MTIME);
    let cfg = fx.cfg_with(|c| c.sftp.identity_files = vec![key_path("user_rsa")]);
    assert_eq!(fx.download(fx.s.url("/f"), cfg).await, data(64 * KIB, 2));
    assert!(
        fx.s.logins()
            .iter()
            .all(|l| l == "publickey:puppy-test-user_rsa"),
        "{:?}",
        fx.s.logins()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn agent_login() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("/f", data(128 * KIB, 3), MTIME);
    let sock = fx.path().join("agent.sock");
    let _agent = puppy_testserver::sftp::start_agent(&sock, &["agent_ed25519"]).await;
    let cfg = fx.cfg_with(|c| {
        c.sftp.agent = SshAgent::Path(sock.clone());
        c.sftp.identity_files = vec![key_path("user_ed25519")];
    });
    assert_eq!(fx.download(fx.s.url("/f"), cfg).await, data(128 * KIB, 3));
    // The agent's key is tried first and works.
    let logins = fx.s.logins();
    assert!(
        logins
            .iter()
            .all(|l| l == "publickey:puppy-test-agent_ed25519"),
        "{logins:?}"
    );
    assert_eq!(fx.s.agent_requests(), 0);
}

#[tokio::test]
async fn password_login() {
    for kbd in [false, true] {
        let fx = Fx::new(|c| {
            c.password = Some(SECRET.into());
            c.keyboard_interactive = kbd;
            c.authorized = Vec::new();
        })
        .await;
        fx.s.set_file("/f", data(64 * KIB, 4), MTIME);
        let got = fx
            .download(with_password(fx.s.url("/f"), SECRET), fx.cfg())
            .await;
        assert_eq!(got, data(64 * KIB, 4));
        let want = if kbd {
            "keyboard-interactive"
        } else {
            "password"
        };
        assert!(
            fx.s.logins().iter().all(|l| l == want),
            "{:?}",
            fx.s.logins()
        );
    }
}

#[tokio::test]
async fn a_wrong_password_fails_for_good_without_showing_it() {
    let fx = Fx::new(|c| {
        c.password = Some("right".into());
        c.authorized = Vec::new();
    })
    .await;
    fx.s.set_file("/f", data(KIB, 0), MTIME);
    let err = fx
        .add_error(with_password(fx.s.url("/f"), "wrong-SECRET"), fx.cfg())
        .await;
    assert!(matches!(err, EngineError::Ssh(_)), "{err:?}");
    assert!(!err.is_transient());
    let msg = err.to_string();
    assert!(!msg.contains("wrong-SECRET"), "{msg}");
    assert!(msg.contains("login as puppy"), "{msg}");
    assert!(msg.contains("the URL's password"), "{msg}");
}

#[tokio::test]
async fn an_encrypted_key_without_an_agent_says_to_use_ssh_add() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("/f", data(KIB, 0), MTIME);
    let cfg = fx.cfg_with(|c| c.sftp.identity_files = vec![key_path("user_encrypted_ed25519")]);
    let err = fx.add_error(fx.s.url("/f"), cfg).await;
    let msg = err.to_string();
    assert!(!err.is_transient(), "{msg}");
    assert!(msg.contains("user_encrypted_ed25519 is encrypted"), "{msg}");
    assert!(msg.contains("ssh-add"), "{msg}");
    assert!(fx.s.logins().is_empty());
}

#[tokio::test]
async fn an_unknown_host_fails_until_trusted() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("/f", data(64 * KIB, 5), MTIME);
    let cfg = fx.cfg_with(|c| c.sftp.known_hosts = vec![fx.path().join("empty")]);
    let err = fx.add_error(fx.s.url("/f"), cfg.clone()).await;
    let msg = err.to_string();
    assert!(!err.is_transient(), "{msg}");
    assert!(msg.contains("is not known"), "{msg}");
    let fp = hostkey::fingerprint(&puppy_testserver::sftp::public_key("host_ed25519"));
    assert!(msg.contains(&fp), "{msg}");
    let port = fx.s.port();
    assert!(
        msg.contains(&format!("puppy ssh trust 127.0.0.1:{port}")),
        "{msg}"
    );
    // Nothing was sent: no login was attempted.
    assert!(fx.s.logins().is_empty());

    // Trusting it (what `puppy ssh trust` does) makes the download work.
    let (key, verdict) = puppy_core::sftp::fetch_host_key("127.0.0.1", port, &cfg)
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Unknown { .. }), "{verdict:?}");
    assert_eq!(hostkey::fingerprint(&key), fp);
    hostkey::trust(
        cfg.sftp.trusted_hosts.as_deref().unwrap(),
        "127.0.0.1",
        port,
        &key,
    )
    .unwrap();
    assert_eq!(fx.download(fx.s.url("/f"), cfg).await, data(64 * KIB, 5));
}

#[tokio::test]
async fn a_changed_host_key_fails_loudly() {
    let fx = Fx::new(|c| c.host_key = HostKey::Other).await;
    fx.s.set_file("/f", data(KIB, 0), MTIME);
    let err = fx.add_error(fx.s.url("/f"), fx.cfg()).await;
    let msg = err.to_string();
    assert!(!err.is_transient(), "{msg}");
    assert!(
        msg.contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "{msg}"
    );
    assert!(msg.contains("known_hosts:1"), "{msg}");
    assert!(fx.s.logins().is_empty());
    // `fetch_host_key` reports it as changed too.
    let (_, verdict) = puppy_core::sftp::fetch_host_key("127.0.0.1", fx.s.port(), &fx.cfg())
        .await
        .unwrap();
    assert!(
        matches!(verdict, Verdict::Changed { line: 1, .. }),
        "{verdict:?}"
    );
}

#[tokio::test]
async fn a_relative_path_is_in_the_home_directory() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("docs/f.txt", data(10 * KIB, 6), MTIME);
    assert_eq!(
        fx.download(fx.s.url("~/docs/f.txt"), fx.cfg()).await,
        data(10 * KIB, 6)
    );
}

#[tokio::test]
async fn missing_files_and_directories_fail_the_add() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("/srv/d/f", data(KIB, 0), MTIME);
    let err = fx.add_error(fx.s.url("/nope"), fx.cfg()).await;
    assert!(err.to_string().contains("no such file"), "{err}");
    assert!(!err.is_transient());
    let err = fx.add_error(fx.s.url("/srv/d"), fx.cfg()).await;
    assert!(err.to_string().contains("directory"), "{err}");
}

#[tokio::test]
async fn empty_file_completes() {
    let fx = Fx::new(|_| {}).await;
    fx.s.set_file("/e", Vec::new(), MTIME);
    assert!(fx.download(fx.s.url("/e"), fx.cfg()).await.is_empty());
}

#[tokio::test]
async fn short_reads_and_a_dropped_read_are_handled() {
    let fx = Fx::new(|c| {
        c.max_read = Some(1000);
        c.fail_after_bytes_once = Some(100 * KIB);
    })
    .await;
    let len = 300 * KIB + 7;
    fx.s.set_file("/f", data(len, 7), MTIME);
    assert_eq!(fx.download(fx.s.url("/f"), fx.cfg()).await, data(len, 7));
}

#[tokio::test]
async fn backs_off_to_the_connections_the_server_allows() {
    let fx = Fx::new(|c| {
        c.max_connections = Some(2);
        c.bytes_per_sec = Some(1024 * KIB);
    })
    .await;
    let len = 768 * KIB;
    fx.s.set_file("/f", data(len, 8), MTIME);
    let cfg = fx.cfg_with(|c| c.connections = 6);
    assert_eq!(fx.download(fx.s.url("/f"), cfg).await, data(len, 8));
    assert!(fx.s.turned_away() >= 1, "nothing was turned away");
    assert!(fx.s.max_concurrent() <= 2);
    assert!(
        fx.s.turned_away() <= 8,
        "{} refusals: not backing off",
        fx.s.turned_away()
    );
}

#[tokio::test]
async fn credentials_stay_out_of_the_sidecar_and_status() {
    let fx = Fx::new(|c| {
        c.password = Some(SECRET.into());
        c.authorized = Vec::new();
        c.bytes_per_sec = Some(128 * KIB);
    })
    .await;
    fx.s.set_file("/f", data(512 * KIB, 9), MTIME);
    let url = with_password(fx.s.url("/f"), SECRET);
    let e = HttpEngine::new(fx.cfg()).unwrap();
    let req = fx.request(url);
    let added = e
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    wait(&e, 1, |s| s.done_bytes > 64 * KIB).await;
    e.pause(1).await.unwrap();
    let sidecar = std::fs::read_to_string(sidecar_path(&added.path)).unwrap();
    assert!(!sidecar.contains("hunter2"), "{sidecar}");
    assert!(
        sidecar.contains("puppy@"),
        "the user name is kept: {sidecar}"
    );
    let shown = format!("{:?} {:?}", e.status(1), e.detail(1));
    assert!(!shown.contains("hunter2"), "{shown}");
    drop(e);
    fx.s.set_cfg(|c| c.bytes_per_sec = None);
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(
        1,
        req,
        StartMode::Restore {
            path: added.path.clone(),
        },
        AddOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(completed(&e, 1).await).unwrap(),
        data(512 * KIB, 9)
    );
}

/// Starts a throttled download, pauses it part-way and drops the engine (as a daemon
/// restart does). Returns the request and the download's path.
async fn interrupted(fx: &Fx, path: &str) -> (AddRequest, PathBuf) {
    fx.s.set_cfg(|c| c.bytes_per_sec = Some(256 * KIB));
    let e = HttpEngine::new(fx.cfg()).unwrap();
    let req = fx.request(fx.s.url(path));
    let added = e
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    wait(&e, 1, |st| st.done_bytes > 128 * KIB).await;
    e.pause(1).await.unwrap();
    assert!(e.status(1).unwrap().done_bytes < 1024 * KIB);
    fx.s.set_cfg(|c| c.bytes_per_sec = None);
    (req, added.path)
}

async fn restored(fx: &Fx, req: AddRequest, path: &Path) -> (HttpEngine, DownloadStatus) {
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(
        1,
        req,
        StartMode::Restore {
            path: path.to_path_buf(),
        },
        AddOptions::default(),
    )
    .await
    .unwrap();
    let st = wait(&e, 1, done).await;
    (e, st)
}

#[tokio::test]
async fn resumes_after_a_restart() {
    let fx = Fx::new(|_| {}).await;
    let len = 1024 * KIB;
    fx.s.set_file("/f", data(len, 10), MTIME);
    let (req, path) = interrupted(&fx, "/f").await;
    let reads = fx.s.read_offsets().len();
    let (_e, st) = restored(&fx, req, &path).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    assert_eq!(std::fs::read(&st.path).unwrap(), data(len, 10));
    // Validated by size and modification time: no downloaded bytes read again to
    // compare (the first segment's start would be).
    let again = &fx.s.read_offsets()[reads..];
    assert!(!again.contains(&0), "{again:?}");
    assert!(!part_path(&path).exists() && !sidecar_path(&path).exists());
}

#[tokio::test]
async fn a_changed_remote_file_needs_attention() {
    // Another modification time, the same size; then another size.
    for (mtime, len) in [(MTIME + 60, 1024 * KIB), (MTIME, 1000 * KIB)] {
        let fx = Fx::new(|_| {}).await;
        fx.s.set_file("/f", data(1024 * KIB, 11), MTIME);
        let (req, path) = interrupted(&fx, "/f").await;
        fx.s.set_file("/f", data(len, 12), mtime);
        let (_e, st) = restored(&fx, req, &path).await;
        assert_eq!(st.state, changed(), "{mtime} {len}");
        assert!(part_path(&path).exists(), "the partial file is kept");
    }
}

#[tokio::test]
async fn without_mtime_downloaded_bytes_are_compared() {
    let fx = Fx::new(|c| c.mtime = false).await;
    let len = 1024 * KIB;
    fx.s.set_file("/f", data(len, 13), MTIME);
    let (req, path) = interrupted(&fx, "/f").await;
    let (_e, st) = restored(&fx, req, &path).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    assert_eq!(std::fs::read(&st.path).unwrap(), data(len, 13));

    let fx = Fx::new(|c| c.mtime = false).await;
    fx.s.set_file("/f", data(len, 13), MTIME);
    let (req, path) = interrupted(&fx, "/f").await;
    fx.s.set_file("/f", data(len, 14), MTIME);
    let (_e, st) = restored(&fx, req, &path).await;
    assert_eq!(st.state, changed());
}

#[tokio::test]
async fn a_file_changing_mid_download_is_noticed() {
    let fx = Fx::new(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    fx.s.set_file("/f", data(1024 * KIB, 15), MTIME);
    let e = HttpEngine::new(fx.cfg_with(|c| c.connections = 1)).unwrap();
    e.add(
        1,
        fx.request(fx.s.url("/f")),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait(&e, 1, |st| st.done_bytes > 64 * KIB).await;
    fx.s.set_file("/f", data(1024 * KIB, 16), MTIME + 60);
    let st = wait(&e, 1, done).await;
    assert_eq!(st.state, changed(), "{st:?}");
}

#[tokio::test]
async fn pause_and_resume() {
    let fx = Fx::new(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let len = 1024 * KIB;
    fx.s.set_file("/f", data(len, 17), MTIME);
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(
        1,
        fx.request(fx.s.url("/f")),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait(&e, 1, |st| st.done_bytes > 64 * KIB).await;
    e.pause(1).await.unwrap();
    assert_eq!(e.status(1).unwrap().state, DownloadState::Paused);
    // No connection stays open while paused.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fx.s.open_connections() > 0 {
        assert!(
            Instant::now() < deadline,
            "{} connections still open",
            fx.s.open_connections()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fx.s.set_cfg(|c| c.bytes_per_sec = None);
    e.resume(1).await.unwrap();
    assert_eq!(
        std::fs::read(completed(&e, 1).await).unwrap(),
        data(len, 17)
    );
}

#[tokio::test]
async fn per_download_limit_applies() {
    let fx = Fx::new(|_| {}).await;
    let len = 256 * KIB;
    fx.s.set_file("/f", data(len, 18), MTIME);
    let e = HttpEngine::new(fx.cfg()).unwrap();
    let t = Instant::now();
    e.add(
        1,
        fx.request(fx.s.url("/f")),
        StartMode::Fresh,
        AddOptions {
            limit: Some(128 * KIB),
            ..AddOptions::default()
        },
    )
    .await
    .unwrap();
    completed(&e, 1).await;
    let secs = t.elapsed().as_secs_f64();
    assert!(secs > 1.5, "took {secs}s");
}

#[tokio::test]
async fn checksum_is_verified() {
    use sha2::{Digest, Sha256};
    let fx = Fx::new(|_| {}).await;
    let body = data(200 * KIB, 19);
    fx.s.set_file("/f", body.clone(), MTIME);
    let hex: String = Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let e = HttpEngine::new(fx.cfg()).unwrap();
    let mut req = fx.request(fx.s.url("/f"));
    req.checksum = Some(format!("sha-256={hex}").parse::<Checksum>().unwrap());
    e.add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    completed(&e, 1).await;
    let mut bad = req;
    bad.checksum = Some(
        format!("sha-256={}", "0".repeat(64))
            .parse::<Checksum>()
            .unwrap(),
    );
    bad.filename = Some("other".into());
    e.add(2, bad, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let st = wait(&e, 2, done).await;
    assert!(
        matches!(&st.state, DownloadState::Failed { reason, transient: false } if reason.contains("checksum")),
        "{st:?}"
    );
}

/// An HTTP download with an SFTP mirror of the same file uses both.
#[tokio::test]
async fn http_and_sftp_locations_mix() {
    let fx = Fx::new(|c| c.bytes_per_sec = Some(1024 * KIB)).await;
    let len = 1024 * KIB;
    let http = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(1024 * KIB),
        ..ServerCfg::default()
    })
    .await;
    fx.s.set_file("/file.bin", data(len, 0), MTIME);
    let mut req = fx.request(http.url("/file.bin"));
    req.mirrors = vec![Mirror::new(fx.s.url("/file.bin"))];
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let path = completed(&e, 1).await;
    assert_eq!(std::fs::read(path).unwrap(), data(len, 0));
    assert!(
        !fx.s.read_offsets().is_empty(),
        "the SFTP mirror was not used"
    );
    assert!(http.range_requests() > 0, "the HTTP source was not used");
}

/// An SFTP mirror without modification times has no validator: it serves ranges only
/// after downloaded bytes were compared against it.
#[tokio::test]
async fn an_sftp_mirror_without_mtime_is_checked_before_use() {
    let fx = Fx::new(|c| {
        c.mtime = false;
        c.bytes_per_sec = Some(2048 * KIB);
    })
    .await;
    let len = 2048 * KIB;
    let http = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(512 * KIB),
        ..ServerCfg::default()
    })
    .await;
    fx.s.set_file("/file.bin", data(len, 0), MTIME);
    let mut req = fx.request(http.url("/file.bin"));
    req.mirrors = vec![Mirror::new(fx.s.url("/file.bin"))];
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let path = completed(&e, 1).await;
    assert_eq!(std::fs::read(path).unwrap(), data(len, 0));
    assert!(
        !fx.s.read_offsets().is_empty(),
        "the SFTP mirror was never used"
    );
}

/// A server answering a read with more bytes than asked for breaks the protocol: none
/// of the excess is written, the location is refused (the download fails rather than
/// completing with wrong bytes), and as a mirror it is dropped while the others finish.
#[tokio::test]
async fn oversized_read_answers_are_refused() {
    let fx = Fx::new(|c| c.oversize_reads = Some(1000)).await;
    let len = 3 * 1024 * KIB + 7;
    fx.s.set_file("/f", data(len, 20), MTIME);
    let e = HttpEngine::new(fx.cfg_with(|c| c.connections = 1)).unwrap();
    e.add(
        1,
        fx.request(fx.s.url("/f")),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    let st = wait(&e, 1, done).await;
    assert!(
        matches!(&st.state, DownloadState::Failed { reason, transient: false } if reason.contains("bytes for a")),
        "{st:?}"
    );

    // As a mirror of an HTTP source: dropped, and the file is right.
    let http = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(4096 * KIB),
        ..ServerCfg::default()
    })
    .await;
    fx.s.set_file("/file.bin", data(len, 0), MTIME);
    let mut req = fx.request(http.url("/file.bin"));
    req.mirrors = vec![Mirror::new(fx.s.url("/file.bin"))];
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let path = completed(&e, 1).await;
    assert!(std::fs::read(path).unwrap() == data(len, 0), "wrong bytes");

    // Without modification times the mirror is compared byte for byte first: that
    // fetch refuses the oversized answers too.
    fx.s.set_cfg(|c| c.mtime = false);
    let mut req = fx.request(http.url("/file.bin"));
    req.filename = Some("again.bin".into());
    req.mirrors = vec![Mirror::new(fx.s.url("/file.bin"))];
    let e = HttpEngine::new(fx.cfg()).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let path = completed(&e, 1).await;
    assert!(std::fs::read(path).unwrap() == data(len, 0), "wrong bytes");
}

/// A host known only with keys of other types, presenting a key of a new type: refused
/// as a possible attack, pointing at `--replace-type`.
#[tokio::test]
async fn a_host_offering_only_an_unknown_key_type_is_refused() {
    let fx = Fx::new(|_| {}).await;
    std::fs::write(
        fx.path().join("known_hosts"),
        fx.s.known_hosts_line(HostKey::Ecdsa),
    )
    .unwrap();
    fx.s.set_file("/f", data(KIB, 0), MTIME);
    let err = fx.add_error(fx.s.url("/f"), fx.cfg()).await;
    let msg = err.to_string();
    assert!(!err.is_transient(), "{msg}");
    assert!(msg.contains("man-in-the-middle"), "{msg}");
    assert!(msg.contains("--replace-type"), "{msg}");
    assert!(fx.s.logins().is_empty());
    let (_, verdict) = puppy_core::sftp::fetch_host_key("127.0.0.1", fx.s.port(), &fx.cfg())
        .await
        .unwrap();
    assert!(
        matches!(verdict, Verdict::Unknown { other_types: true }),
        "{verdict:?}"
    );
}

/// With `SshAgent::Auto` the agent of the client that added the download comes first,
/// then the agents clients offered; one that is gone or holds no keys is passed over.
#[cfg(unix)]
#[tokio::test]
async fn agents_without_keys_or_gone_are_passed_over() {
    let fx = Fx::new(|c| c.authorized = vec!["agent_ed25519"]).await;
    fx.s.set_file("/f", data(64 * KIB, 21), MTIME);
    let empty = fx.path().join("empty.sock");
    let good = fx.path().join("good.sock");
    let _e = puppy_testserver::sftp::start_agent(&empty, &[]).await;
    let _g = puppy_testserver::sftp::start_agent(&good, &["agent_ed25519"]).await;
    puppy_core::sftp::offer_agent(good.to_str().unwrap());
    puppy_core::sftp::offer_agent(fx.path().join("gone.sock").to_str().unwrap());
    let cfg = fx.cfg_with(|c| {
        c.sftp.agent = SshAgent::Auto;
        c.sftp.identity_files = Vec::new();
    });
    let mut req = fx.request(fx.s.url("/f"));
    // The adding client's agent: reachable, but without keys.
    req.context.ssh_agent = Some(empty.to_str().unwrap().into());
    let e = HttpEngine::new(cfg).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(completed(&e, 1).await).unwrap(),
        data(64 * KIB, 21)
    );
    let logins = fx.s.logins();
    assert!(!logins.is_empty());
    assert!(
        logins
            .iter()
            .all(|l| l == "publickey:puppy-test-agent_ed25519"),
        "{logins:?}"
    );
}

/// A server that never answers the SFTP subsystem request, while sending other channel
/// messages, is given up on within the read timeout (not kept waiting by each message).
#[tokio::test]
async fn a_stalled_subsystem_request_times_out() {
    let fx = Fx::new(|c| c.stall_subsystem = true).await;
    fx.s.set_file("/f", data(KIB, 0), MTIME);
    let cfg = fx.cfg_with(|c| {
        c.read_timeout_ms = 500;
        c.max_retries = 0;
    });
    let t = Instant::now();
    let e = HttpEngine::new(cfg).unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        e.add(
            1,
            fx.request(fx.s.url("/f")),
            StartMode::Fresh,
            AddOptions::default(),
        ),
    )
    .await
    .expect("the add hangs");
    assert!(r.is_err(), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}
