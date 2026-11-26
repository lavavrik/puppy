//! FTP(S) downloads through the URL engine, against the in-process test FTP server.

use puppy_core::checksum::Checksum;
use puppy_core::config::{FtpConfig, FtpTls, HttpConfig};
use puppy_core::engine::{AddOptions, Engine, EngineError, StartMode};
use puppy_core::http::engine::HttpEngine;
use puppy_core::http::resume::{part_path, sidecar_path};
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadState, DownloadStatus, EngineDetail, Mirror,
    RequestContext, Source,
};
use puppy_testserver::ftp::{FtpCfg, FtpServer, FtpTls as ServerTls, LimitReply, test_ca_der};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const KIB: u64 = 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const MDTM: &str = "20261126120000";

fn data(len: u64, version: u32) -> Vec<u8> {
    (0..len).map(|i| body_byte(i, version)).collect()
}

fn cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        connections: 4,
        min_segment_bytes: 32 * KIB,
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 50,
        default_dir: dir.to_path_buf(),
        ftp: FtpConfig {
            tls: FtpTls::Try,
            extra_roots: vec![test_ca_der()],
        },
        ..HttpConfig::default()
    }
}

fn request(url: url::Url, dir: &Path) -> AddRequest {
    AddRequest {
        source: Source::Http { url },
        output_dir: Some(dir.to_path_buf()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum: None,
        mirrors: Vec::new(),
        size: None,
    }
}

async fn server(f: impl FnOnce(&mut FtpCfg)) -> FtpServer {
    let mut c = FtpCfg::default();
    f(&mut c);
    FtpServer::start(c).await
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

/// Downloads `url` with `cfg` and returns the file's bytes.
async fn download(url: url::Url, cfg: HttpConfig, dir: &Path) -> Vec<u8> {
    let e = HttpEngine::new(cfg).unwrap();
    e.add(
        1,
        request(url, dir),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    std::fs::read(completed(&e, 1).await).unwrap()
}

#[tokio::test]
async fn anonymous_download() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    s.set_file("pub/file.bin", data(100 * KIB, 0), MDTM);
    let got = download(s.url("pub/file.bin"), cfg(dir.path()), dir.path()).await;
    assert_eq!(got, data(100 * KIB, 0));
    let cmds = s.commands();
    assert!(cmds.contains(&"USER anonymous".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"TYPE I".to_string()), "{cmds:?}");
    assert!(cmds.iter().any(|c| c == "EPSV"), "{cmds:?}");
    // TLS was tried (and refused by this server), so plain FTP went on.
    assert!(cmds.iter().any(|c| c == "AUTH TLS"), "{cmds:?}");
    assert!(dir.path().join("file.bin").exists());
}

#[tokio::test]
async fn segmented_download_uses_several_rest_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    let len = 512 * KIB;
    s.set_file("big.bin", data(len, 1), MDTM);
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    e.add(
        1,
        request(s.url("big.bin"), dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    let path = completed(&e, 1).await;
    assert_eq!(std::fs::read(path).unwrap(), data(len, 1));
    let mut offsets = s.rest_offsets();
    offsets.sort_unstable();
    offsets.dedup();
    assert!(offsets.len() >= 3, "REST offsets {offsets:?}");
    assert!(offsets.iter().all(|&o| o > 0 && o < len), "{offsets:?}");
    assert!(
        s.max_concurrent() >= 2,
        "max concurrent {}",
        s.max_concurrent()
    );
    let EngineDetail::Http { resumable, .. } = e.detail(1).unwrap() else {
        panic!("not a segmented download");
    };
    assert!(resumable);
}

#[tokio::test]
async fn epsv_refused_falls_back_to_pasv_ignoring_its_address() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.epsv = false).await;
    s.set_file("f", data(200 * KIB, 2), MDTM);
    let got = download(s.url("f"), cfg(dir.path()), dir.path()).await;
    assert_eq!(got, data(200 * KIB, 2));
    let cmds = s.commands();
    // EPSV is given up on after a refusal (by the sessions that opened meanwhile too).
    let epsv = cmds.iter().filter(|c| *c == "EPSV").count();
    assert!((1..=4).contains(&epsv), "{cmds:?}");
    assert!(cmds.iter().any(|c| c == "PASV"), "{cmds:?}");
}

#[tokio::test]
async fn without_rest_or_size_one_stream_downloads_the_file() {
    for (rest, size) in [(false, true), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let s = server(|c| {
            c.rest = rest;
            c.size = size;
        })
        .await;
        s.set_file("f", data(300 * KIB, 3), MDTM);
        let got = download(s.url("f"), cfg(dir.path()), dir.path()).await;
        assert_eq!(got, data(300 * KIB, 3), "rest {rest} size {size}");
        assert!(s.rest_offsets().is_empty(), "rest {rest} size {size}");
        assert_eq!(s.retrs(), 1, "rest {rest} size {size}");
    }
}

/// A server whose `FEAT` lists no `REST` but that cannot list features at all is
/// assumed to restart; refusing `REST` then makes it one stream after all.
#[tokio::test]
async fn refused_rest_without_feat_falls_back_to_one_stream() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.feat = false;
        c.rest = false;
    })
    .await;
    s.set_file("f", data(300 * KIB, 4), MDTM);
    let got = download(s.url("f"), cfg(dir.path()), dir.path()).await;
    assert_eq!(got, data(300 * KIB, 4));
}

#[tokio::test]
async fn missing_file_fails_the_add() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let r = e
        .add(
            1,
            request(s.url("nope.bin"), dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await;
    assert!(
        matches!(r, Err(EngineError::Ftp { code: 550, .. })),
        "{r:?}"
    );
}

#[tokio::test]
async fn empty_file_completes() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    s.set_file("empty", Vec::new(), MDTM);
    let got = download(s.url("empty"), cfg(dir.path()), dir.path()).await;
    assert!(got.is_empty());
}

#[tokio::test]
async fn a_dropped_data_connection_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.fail_after_bytes_once = Some(40 * KIB)).await;
    s.set_file("f", data(256 * KIB, 5), MDTM);
    let got = download(s.url("f"), cfg(dir.path()), dir.path()).await;
    assert_eq!(got, data(256 * KIB, 5));
}

async fn connection_limit(reply: LimitReply) {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.max_sessions = Some(2);
        c.limit_reply = reply;
        c.bytes_per_sec = Some(2 * 1024 * KIB);
    })
    .await;
    let len = 768 * KIB;
    s.set_file("f", data(len, 6), MDTM);
    let mut c = cfg(dir.path());
    c.connections = 6;
    let got = download(s.url("f"), c, dir.path()).await;
    assert_eq!(got, data(len, 6), "{reply:?}");
    assert!(s.turned_away() >= 1, "{reply:?}: nothing was turned away");
    assert!(s.max_concurrent() <= 2, "{reply:?}");
    // Backing off means not hammering the server: a few refusals, not one per retry.
    assert!(
        s.turned_away() <= 8,
        "{reply:?}: {} refusals",
        s.turned_away()
    );
}

#[tokio::test]
async fn backs_off_to_fewer_connections_on_421() {
    connection_limit(LimitReply::Greeting421).await;
}

#[tokio::test]
async fn backs_off_to_fewer_connections_on_530() {
    connection_limit(LimitReply::Login530).await;
}

#[tokio::test]
async fn wrong_password_fails_without_retrying_forever() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.login = Some(("bob".into(), "right".into()))).await;
    s.set_file("f", data(10 * KIB, 0), MDTM);
    let mut url = s.url("f");
    url.set_username("bob").unwrap();
    url.set_password(Some("wrong")).unwrap();
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let r = e
        .add(
            1,
            request(url, dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await;
    let err = r.expect_err("login fails");
    assert!(matches!(err, EngineError::Ftp { code: 530, .. }), "{err:?}");
    assert!(!err.is_transient());
    assert!(!err.to_string().contains("wrong"), "{err}");
}

#[tokio::test]
async fn credentials_stay_out_of_the_sidecar_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.login = Some(("bob".into(), "hunter2-SECRET".into()));
        c.bytes_per_sec = Some(128 * KIB);
    })
    .await;
    s.set_file("f", data(512 * KIB, 7), MDTM);
    let mut url = s.url("f");
    url.set_username("bob").unwrap();
    url.set_password(Some("hunter2-SECRET")).unwrap();
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let req = request(url.clone(), dir.path());
    let added = e
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    wait(&e, 1, |s| s.done_bytes > 64 * KIB).await;
    e.pause(1).await.unwrap();
    let sidecar = std::fs::read_to_string(sidecar_path(&added.path)).unwrap();
    assert!(!sidecar.contains("hunter2"), "{sidecar}");
    assert!(sidecar.contains("bob@"), "the user name is kept: {sidecar}");
    let shown = format!("{:?} {:?}", e.status(1), e.detail(1));
    assert!(!shown.contains("hunter2"), "{shown}");
    // A restore with the daemon's record (password included) continues.
    drop(e);
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    s.set_cfg(|c| c.bytes_per_sec = None);
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
        data(512 * KIB, 7)
    );
}

/// Starts a throttled download, pauses it part-way and drops the engine (as a daemon
/// restart does). Returns the request and the download's path.
async fn interrupted(s: &FtpServer, dir: &Path, path: &str) -> (AddRequest, PathBuf) {
    s.set_cfg(|c| c.bytes_per_sec = Some(256 * KIB));
    let e = HttpEngine::new(cfg(dir)).unwrap();
    let req = request(s.url(path), dir);
    let added = e
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    wait(&e, 1, |st| st.done_bytes > 128 * KIB).await;
    e.pause(1).await.unwrap();
    assert!(e.status(1).unwrap().done_bytes < 1024 * KIB);
    s.set_cfg(|c| c.bytes_per_sec = None);
    (req, added.path)
}

async fn restored(req: AddRequest, path: &Path, dir: &Path) -> (HttpEngine, DownloadStatus) {
    let e = HttpEngine::new(cfg(dir)).unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    let len = 1024 * KIB;
    s.set_file("f", data(len, 8), MDTM);
    let (req, path) = interrupted(&s, dir.path(), "f").await;
    let retrs = s.retrs();
    let (_e, st) = restored(req, &path, dir.path()).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    assert_eq!(std::fs::read(&st.path).unwrap(), data(len, 8));
    assert!(s.retrs() > retrs);
    assert!(!part_path(&path).exists() && !sidecar_path(&path).exists());
}

#[tokio::test]
async fn a_changed_remote_file_needs_attention() {
    // Another modification time, the same size; then another size.
    for (mdtm, len) in [("20261127090000", 1024 * KIB), (MDTM, 1000 * KIB)] {
        let dir = tempfile::tempdir().unwrap();
        let s = server(|_| {}).await;
        s.set_file("f", data(1024 * KIB, 9), MDTM);
        let (req, path) = interrupted(&s, dir.path(), "f").await;
        s.set_file("f", data(len, 10), mdtm);
        let (_e, st) = restored(req, &path, dir.path()).await;
        assert_eq!(
            st.state,
            DownloadState::NeedsAttention {
                reason: AttentionReason::RemoteChanged
            },
            "{mdtm} {len}"
        );
        assert!(part_path(&path).exists(), "the partial file is kept");
    }
}

#[tokio::test]
async fn without_mdtm_downloaded_bytes_are_compared() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.mdtm = false).await;
    let len = 1024 * KIB;
    s.set_file("f", data(len, 11), MDTM);
    let (req, path) = interrupted(&s, dir.path(), "f").await;
    // Same size, same bytes: resumed (after fetching some bytes again to compare).
    let rests = s.rest_offsets().len();
    let (_e, st) = restored(req.clone(), &path, dir.path()).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    assert_eq!(std::fs::read(&st.path).unwrap(), data(len, 11));
    assert!(s.rest_offsets().len() > rests);

    // Same size, other bytes: needs attention.
    let dir = tempfile::tempdir().unwrap();
    s.set_file("f", data(len, 11), MDTM);
    let (req, path) = interrupted(&s, dir.path(), "f").await;
    s.set_file("f", data(len, 12), MDTM);
    let (_e, st) = restored(req, &path, dir.path()).await;
    assert_eq!(
        st.state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    );
}

#[tokio::test]
async fn a_file_changing_mid_download_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.bytes_per_sec = Some(256 * KIB);
        // The data connection drops after 200 KiB: the retry finds the new file.
        c.fail_after_bytes_once = Some(200 * KIB);
    })
    .await;
    s.set_file("f", data(1024 * KIB, 13), MDTM);
    let mut c = cfg(dir.path());
    c.connections = 1;
    let e = HttpEngine::new(c).unwrap();
    e.add(
        1,
        request(s.url("f"), dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait(&e, 1, |st| st.done_bytes > 64 * KIB).await;
    s.set_file("f", data(1024 * KIB, 14), "20261127090000");
    let st = wait(&e, 1, done).await;
    assert_eq!(
        st.state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    );
}

#[tokio::test]
async fn a_resume_in_the_same_run_validates() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    s.set_file("f", data(1024 * KIB, 13), MDTM);
    let mut c = cfg(dir.path());
    // One connection at first: later ones (stolen ranges) open new sessions.
    c.connections = 1;
    let e = HttpEngine::new(c).unwrap();
    e.add(
        1,
        request(s.url("f"), dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait(&e, 1, |st| st.done_bytes > 64 * KIB).await;
    e.pause(1).await.unwrap();
    s.set_file("f", data(1024 * KIB, 14), "20261127090000");
    s.set_cfg(|c| c.bytes_per_sec = None);
    // A resume in the same run validates as well.
    e.resume(1).await.unwrap();
    let st = wait(&e, 1, done).await;
    assert_eq!(
        st.state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    );
}

#[tokio::test]
async fn pause_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let len = 1024 * KIB;
    s.set_file("f", data(len, 15), MDTM);
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    e.add(
        1,
        request(s.url("f"), dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait(&e, 1, |st| st.done_bytes > 64 * KIB).await;
    e.pause(1).await.unwrap();
    assert_eq!(e.status(1).unwrap().state, DownloadState::Paused);
    // No session stays open while paused.
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.sessions() > 0 {
        assert!(
            Instant::now() < deadline,
            "{} sessions still open",
            s.sessions()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    s.set_cfg(|c| c.bytes_per_sec = None);
    e.resume(1).await.unwrap();
    assert_eq!(
        std::fs::read(completed(&e, 1).await).unwrap(),
        data(len, 15)
    );
}

#[tokio::test]
async fn per_download_limit_applies() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    let len = 256 * KIB;
    s.set_file("f", data(len, 16), MDTM);
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let t = Instant::now();
    e.add(
        1,
        request(s.url("f"), dir.path()),
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
    // 256 KiB at 128 KiB/s: about 2 s (a burst of a quarter second allowed).
    assert!(secs > 1.5, "took {secs}s");
}

#[tokio::test]
async fn checksum_is_verified() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let s = server(|_| {}).await;
    let body = data(200 * KIB, 17);
    s.set_file("f", body.clone(), MDTM);
    let hex: String = Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let mut req = request(s.url("f"), dir.path());
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

#[tokio::test]
async fn explicit_ftps() {
    let s = server(|c| c.tls = ServerTls::Explicit).await;
    s.set_file("f", data(300 * KIB, 18), MDTM);
    // `ftp://` with TLS tried, `ftp://` with TLS required, and `ftpes://`.
    for (scheme, tls) in [
        ("ftp", FtpTls::Try),
        ("ftp", FtpTls::Require),
        ("ftpes", FtpTls::Off),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.ftp.tls = tls;
        let got = download(s.url_with(scheme, "f"), c, dir.path()).await;
        assert_eq!(got, data(300 * KIB, 18), "{scheme} {tls:?}");
    }
    let cmds = s.commands();
    assert!(cmds.iter().any(|c| c == "PROT P"), "{cmds:?}");
    // Every session secured itself before logging in.
    let auths = cmds.iter().filter(|c| *c == "AUTH TLS").count();
    let users = cmds.iter().filter(|c| c.starts_with("USER ")).count();
    assert_eq!(auths, users, "{cmds:?}");
}

#[tokio::test]
async fn implicit_ftps() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.tls = ServerTls::Implicit).await;
    s.set_file("f", data(300 * KIB, 19), MDTM);
    let url = s.url("f");
    assert_eq!(url.scheme(), "ftps");
    let got = download(url, cfg(dir.path()), dir.path()).await;
    assert_eq!(got, data(300 * KIB, 19));
    let cmds = s.commands();
    assert!(cmds.iter().any(|c| c == "PROT P"), "{cmds:?}");
    assert!(!cmds.iter().any(|c| c == "AUTH TLS"), "{cmds:?}");
}

#[tokio::test]
async fn required_tls_is_never_dropped() {
    let s = server(|c| c.tls = ServerTls::Off).await;
    s.set_file("f", data(10 * KIB, 0), MDTM);
    for (scheme, tls) in [("ftp", FtpTls::Require), ("ftpes", FtpTls::Try)] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.ftp.tls = tls;
        let e = HttpEngine::new(c).unwrap();
        let r = e
            .add(
                1,
                request(s.url_with(scheme, "f"), dir.path()),
                StartMode::Fresh,
                AddOptions::default(),
            )
            .await;
        let err = r.expect_err("refused");
        assert!(matches!(err, EngineError::Tls(_)), "{scheme}: {err:?}");
        assert!(!err.is_transient());
    }
    // Nobody logged in: credentials never went over the plain connection.
    assert!(
        !s.commands().iter().any(|c| c.starts_with("USER")),
        "{:?}",
        s.commands()
    );
}

#[tokio::test]
async fn untrusted_certificate_fails_instead_of_downgrading() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.tls = ServerTls::Explicit).await;
    s.set_file("f", data(10 * KIB, 0), MDTM);
    let mut c = cfg(dir.path());
    c.ftp.extra_roots.clear();
    let e = HttpEngine::new(c).unwrap();
    let r = e
        .add(
            1,
            request(s.url("f"), dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await;
    let err = r.expect_err("certificate refused");
    assert!(matches!(err, EngineError::Tls(_)), "{err:?}");
    assert!(
        !s.commands().iter().any(|c| c.starts_with("USER")),
        "{:?}",
        s.commands()
    );
}

#[tokio::test]
async fn tls_off_never_asks() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| c.tls = ServerTls::Explicit).await;
    s.set_file("f", data(10 * KIB, 0), MDTM);
    let mut c = cfg(dir.path());
    c.ftp.tls = FtpTls::Off;
    download(s.url("f"), c, dir.path()).await;
    assert!(!s.commands().iter().any(|c| c.starts_with("AUTH")));
}

/// An HTTP download with an FTP mirror of the same file uses both.
#[tokio::test]
async fn http_and_ftp_locations_mix() {
    let dir = tempfile::tempdir().unwrap();
    let len = 1024 * KIB;
    let http = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(1024 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let ftp = server(|c| c.bytes_per_sec = Some(1024 * KIB)).await;
    ftp.set_file("file.bin", data(len, 0), MDTM);
    let mut req = request(http.url("/file.bin"), dir.path());
    req.mirrors = vec![Mirror::new(ftp.url("file.bin"))];
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let path = completed(&e, 1).await;
    assert_eq!(std::fs::read(path).unwrap(), data(len, 0));
    assert!(ftp.retrs() > 0, "the FTP mirror was not used");
    assert!(http.range_requests() > 0, "the HTTP source was not used");
}

/// An FTP mirror without `MDTM` has no validator: like an HTTP one without, it serves
/// ranges only after downloaded bytes were compared against it.
#[tokio::test]
async fn an_ftp_mirror_without_mdtm_is_checked_before_use() {
    let dir = tempfile::tempdir().unwrap();
    let len = 2048 * KIB;
    let mut c = cfg(dir.path());
    c.connections = 4;
    // The HTTP primary alone needs 8 s (4 connections at 64 KiB/s each, the server's
    // limit is per response): the mirror is only checked once downloaded bytes exist,
    // and that check (connect, login, SIZE, RETR a window) must not race the end of
    // the download on a slow runner.
    let http = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(64 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let ftp = server(|c| {
        c.mdtm = false;
        c.bytes_per_sec = Some(2048 * KIB);
    })
    .await;
    ftp.set_file("file.bin", data(len, 0), MDTM);
    let mut req = request(http.url("/file.bin"), dir.path());
    req.mirrors = vec![Mirror::new(ftp.url("file.bin"))];
    let e = HttpEngine::new(c).unwrap();
    e.add(1, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    let path = completed(&e, 1).await;
    assert_eq!(std::fs::read(path).unwrap(), data(len, 0));
    // The check's own RETR is one; the ranges it then served are more.
    assert!(ftp.retrs() >= 2, "the FTP mirror was never used");
    let EngineDetail::Http { mirrors, .. } = e.detail(1).unwrap() else {
        panic!("not a segmented download");
    };
    assert!(mirrors[1].done_bytes > 0, "{mirrors:?}");
}

/// A server that lowers its session limit while sessions are open: the download backs
/// off to what it allows now, without hammering it with refused connections.
#[tokio::test]
async fn a_lowered_session_limit_is_followed_without_a_reconnect_storm() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.max_sessions = Some(2);
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let len = 1024 * KIB;
    s.set_file("f", data(len, 20), MDTM);
    let mut c = cfg(dir.path());
    c.connections = 4;
    let e = HttpEngine::new(c).unwrap();
    e.add(
        1,
        request(s.url("f"), dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    s.set_cfg(|c| c.max_sessions = Some(1));
    let path = completed(&e, 1).await;
    assert_eq!(std::fs::read(path).unwrap(), data(len, 20));
    assert!(
        s.turned_away() <= 40,
        "{} refused connections",
        s.turned_away()
    );
}

/// A server that accepts `AUTH TLS` and then refuses to protect the session is not one
/// without TLS: with TLS only tried, the download still fails rather than log in in the
/// clear.
#[tokio::test]
async fn a_failure_after_auth_tls_never_falls_back_to_plain() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.tls = ServerTls::Explicit;
        c.refuse_prot = true;
    })
    .await;
    s.set_file("f", data(10 * KIB, 0), MDTM);
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let r = e
        .add(
            1,
            request(s.url("f"), dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await;
    let err = r.expect_err("TLS set-up fails");
    assert!(
        matches!(&err, EngineError::Tls(m) if m.contains("accepted AUTH TLS")),
        "{err:?}"
    );
    let cmds = s.commands();
    assert!(!cmds.iter().any(|c| c.starts_with("USER")), "{cmds:?}");
    assert_eq!(
        cmds.iter().filter(|c| *c == "AUTH TLS").count(),
        1,
        "{cmds:?}"
    );
}

/// A server that stops reporting the modification time it reported before cannot vouch
/// for the file: the next transfer treats it as changed.
#[tokio::test]
async fn mdtm_disappearing_mid_download_needs_attention() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(|c| {
        c.bytes_per_sec = Some(256 * KIB);
        c.fail_after_bytes_once = Some(200 * KIB);
    })
    .await;
    s.set_file("f", data(1024 * KIB, 21), MDTM);
    let mut c = cfg(dir.path());
    c.connections = 1;
    let e = HttpEngine::new(c).unwrap();
    e.add(
        1,
        request(s.url("f"), dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait(&e, 1, |st| st.done_bytes > 64 * KIB).await;
    // `set_cfg` re-arms the one-time failure: the retry after it asks for MDTM again.
    s.set_cfg(|c| c.mdtm = false);
    let st = wait(&e, 1, done).await;
    assert_eq!(
        st.state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    );
}
