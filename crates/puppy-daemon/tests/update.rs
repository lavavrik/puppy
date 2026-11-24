//! Release lookup, verified download and swap-in, against a fake GitHub.

use puppy_daemon::update::{UpdateError, archive_name, cannot_install_reason, fetch, latest};
use puppy_testserver::github::{FakeGitHub, FakeRelease};
use semver::Version;

const TARGET: &str = "x86_64-unknown-linux-gnu";

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn lookup(
    fake: &FakeGitHub,
    current: &str,
) -> Result<Option<puppy_daemon::update::Release>, UpdateError> {
    latest(&client(), &fake.api_base(), &v(current), TARGET).await
}

#[tokio::test]
async fn newer_release_is_found() {
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET)).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().expect("a release");
    assert_eq!(r.version, v("0.2.0"));
    assert_eq!(r.archive.name, archive_name(&v("0.2.0"), TARGET));
    assert_eq!(r.checksum.name, format!("{}.sha256", r.archive.name));
}

#[tokio::test]
async fn same_or_older_is_none() {
    for fake_version in ["0.1.0", "0.0.9"] {
        let fake = FakeGitHub::start(FakeRelease::new(fake_version, TARGET)).await;
        assert!(
            lookup(&fake, "0.1.0").await.unwrap().is_none(),
            "{fake_version}"
        );
    }
}

#[tokio::test]
async fn no_asset_for_target_is_none() {
    let other = FakeGitHub::start(FakeRelease::new("0.2.0", "aarch64-apple-darwin")).await;
    assert!(lookup(&other, "0.1.0").await.unwrap().is_none());
    let unsummed = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET).without_checksum()).await;
    assert!(lookup(&unsummed, "0.1.0").await.unwrap().is_none());
}

#[tokio::test]
async fn api_error_is_failed() {
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET)).await;
    fake.set_status(403);
    match lookup(&fake, "0.1.0").await {
        Err(UpdateError::Failed(msg)) => assert!(msg.contains("403"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.hits(), 1);
}

#[tokio::test]
async fn bad_checksum_is_rejected() {
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET).bad_checksum()).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().unwrap();
    let work = tempfile::tempdir().unwrap();
    match fetch(&client(), &r, work.path()).await {
        Err(UpdateError::Failed(msg)) => assert!(msg.contains("checksum"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(!work.path().join("puppy").exists());
}

#[tokio::test]
async fn extracts_only_the_binary() {
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET)).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let work = parent.path().join("work");
    let bin = fetch(&client(), &r, &work).await.unwrap();
    assert_eq!(bin, work.join("puppy"));
    let mut names: Vec<_> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut want = vec![r.archive.name.clone(), "puppy".to_string()];
    want.sort();
    assert_eq!(names, want);
    assert!(!parent.path().join("evil").exists());
    assert!(!parent.path().parent().unwrap().join("evil").exists());
}

#[tokio::test]
async fn missing_binary_is_failed() {
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET).without_binary()).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().unwrap();
    let work = tempfile::tempdir().unwrap();
    match fetch(&client(), &r, work.path()).await {
        Err(UpdateError::Failed(msg)) => assert!(msg.contains("missing"), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn fetch_empties_the_work_dir_first() {
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET)).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("stale"), b"x").unwrap();
    let bin = fetch(&client(), &r, work.path()).await.unwrap();
    assert!(bin.starts_with(work.path()));
    assert!(!work.path().join("stale").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn fetch_check_and_replace() {
    use puppy_daemon::update::{replace_exe, sanity_check};
    use std::os::unix::fs::PermissionsExt;
    let fake = FakeGitHub::start(FakeRelease::new("0.2.0", TARGET)).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().unwrap();
    let work = tempfile::tempdir().unwrap();
    let new = fetch(&client(), &r, work.path()).await.unwrap();
    sanity_check(&new, &r.version).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("puppy");
    std::fs::write(&exe, b"old").unwrap();
    replace_exe(&new, &exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(&new).unwrap());
    assert_eq!(
        std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!dir.path().join(".puppy-new").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sanity_failure_leaves_exe_untouched() {
    use puppy_daemon::update::sanity_check;
    let fake =
        FakeGitHub::start(FakeRelease::new("0.2.0", TARGET).binary_prints("puppy 9.9.8")).await;
    let r = lookup(&fake, "0.1.0").await.unwrap().unwrap();
    let work = tempfile::tempdir().unwrap();
    let new = fetch(&client(), &r, work.path()).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("puppy");
    std::fs::write(&exe, b"old").unwrap();
    assert!(matches!(
        sanity_check(&new, &r.version).await,
        Err(UpdateError::Failed(_))
    ));
    assert_eq!(std::fs::read(&exe).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn read_only_dir_cannot_install() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("puppy");
    std::fs::write(&exe, b"x").unwrap();
    // Dev builds are refused unless the caller allows them.
    if !puppy_daemon::update::RELEASE_BUILD {
        let reason = cannot_install_reason(&exe, false).unwrap();
        assert!(reason.contains("release workflow"), "{reason}");
    }
    assert!(cannot_install_reason(&exe, true).is_none());

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let denied = std::fs::File::create(dir.path().join("probe")).is_err();
    if denied {
        let reason = cannot_install_reason(&exe, true).unwrap();
        assert!(reason.contains("cannot write"), "{reason}");
    } // else: running as root, nothing to test
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
#[test]
fn replace_exe_moves_old_aside() {
    use puppy_daemon::update::{remove_old_exe, replace_exe};
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("puppy.exe");
    let new = dir.path().join("new.exe");
    std::fs::write(&exe, b"old").unwrap();
    std::fs::write(&new, b"new").unwrap();
    replace_exe(&new, &exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), b"new");
    let old = dir.path().join("puppy.exe.old");
    assert!(old.exists());
    remove_old_exe(&exe);
    assert!(!old.exists());
}

// ---- The updater service, in a daemon run in-process ----

mod daemon {
    use interprocess::local_socket::tokio::{Stream, prelude::*};
    use interprocess::local_socket::{GenericFilePath, GenericNamespaced};
    use puppy_core::config::HttpConfig;
    use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
    use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
    use puppy_daemon::notify::{Notification, NotifyKind, RecordingNotifier};
    use puppy_daemon::update::UpdateOptions;
    use puppy_daemon::{Paths, SocketName};
    use puppy_proto::WireState;
    use puppy_proto::{
        Envelope, ErrorCode, Event, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request,
        ServerMsg, UpdateStatus, write_msg,
    };
    use puppy_testserver::github::{FakeGitHub, FakeRelease};
    use puppy_testserver::{ServerCfg, TestServer};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::io::{BufReader, ReadHalf, WriteHalf};
    use tokio::task::JoinHandle;

    const TIMEOUT: Duration = Duration::from_secs(20);
    const DAY: Duration = Duration::from_secs(24 * 3600);
    /// The quiet time before an automatic install, in the tests that want one.
    const SETTLE: Duration = Duration::from_secs(1);
    const KIB: u64 = 1024;

    type Daemon = JoinHandle<anyhow::Result<RunOutcome>>;

    fn socket_name() -> SocketName {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        if cfg!(windows) {
            SocketName::Pipe(format!("puppy-up-{pid}-{n}"))
        } else {
            SocketName::Path(PathBuf::from(format!("/tmp/pu-{pid}-{n}.sock")))
        }
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    struct Env {
        home: tempfile::TempDir,
        socket: SocketName,
        exe: PathBuf,
        fake: FakeGitHub,
    }

    impl Env {
        async fn new() -> Self {
            Self::with(FakeRelease::new("0.2.0", puppy_daemon::update::TARGET)).await
        }

        async fn with(release: FakeRelease) -> Self {
            let home = tempfile::tempdir().unwrap();
            let bin = home.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let exe = bin.join(if cfg!(windows) { "puppy.exe" } else { "puppy" });
            std::fs::write(&exe, b"old").unwrap();
            Env {
                home,
                socket: socket_name(),
                exe,
                fake: FakeGitHub::start(release).await,
            }
        }

        fn paths(&self) -> Paths {
            Paths::with_home(self.home.path(), self.socket.clone()).unwrap()
        }

        fn update_opts(&self, allow_dev: bool) -> UpdateOptions {
            UpdateOptions {
                api_base: self.fake.api_base(),
                exe: self.exe.clone(),
                allow_dev,
                settle: Duration::from_secs(60),
                check_delay: Duration::from_millis(100),
                check_every: DAY,
            }
        }

        fn write_config(&self, text: &str) {
            let p = self.paths().config_file();
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }

        fn opts(&self, allow_dev: bool) -> DaemonOptions {
            DaemonOptions {
                paths: Some(self.paths()),
                torrent_test: Some(Default::default()),
                update: Some(self.update_opts(allow_dev)),
                ..DaemonOptions::default()
            }
        }

        fn dl_dir(&self) -> PathBuf {
            self.home.path().join("dl")
        }

        /// Like `opts(true)`, settling after `SETTLE` and downloading into `dl_dir`.
        fn settling(&self) -> DaemonOptions {
            let mut o = self.opts(true);
            o.update.as_mut().unwrap().settle = SETTLE;
            o.http = Some(HttpConfig {
                backoff_base_ms: 10,
                backoff_max_ms: 100,
                flush_interval_ms: 100,
                read_timeout_ms: 2_000,
                validate_timeout_ms: 1_500,
                default_dir: self.dl_dir(),
                ..HttpConfig::default()
            });
            o
        }

        fn req(&self, s: &TestServer, name: &str) -> AddRequest {
            AddRequest {
                source: Source::Http {
                    url: s.url("/file.bin"),
                },
                output_dir: Some(self.dl_dir()),
                filename: Some(name.into()),
                context: RequestContext::default(),
                files: Default::default(),
                seed: None,
            }
        }

        fn exe_swapped(&self) -> bool {
            std::fs::read(&self.exe).unwrap() != b"old"
        }

        async fn start(&self, allow_dev: bool) -> Daemon {
            self.launch(self.opts(allow_dev)).await
        }

        /// Starts a daemon with `opts` and waits until it accepts connections.
        async fn launch(&self, opts: DaemonOptions) -> Daemon {
            let d = tokio::spawn(run_daemon(opts));
            let deadline = Instant::now() + TIMEOUT;
            loop {
                assert!(!d.is_finished(), "daemon exited early: {:?}", d.await);
                if self.try_connect().await.is_some() {
                    return d;
                }
                assert!(Instant::now() < deadline, "daemon never listened");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        async fn try_connect(&self) -> Option<Client> {
            let stream = match &self.socket {
                SocketName::Path(p) => {
                    Stream::connect(p.as_path().to_fs_name::<GenericFilePath>().ok()?).await
                }
                SocketName::Pipe(n) => {
                    Stream::connect(n.as_str().to_ns_name::<GenericNamespaced>().ok()?).await
                }
            }
            .ok()?;
            let (rd, wr) = tokio::io::split(stream);
            Some(Client {
                rd: MsgReader::new(BufReader::new(rd)),
                wr,
                next_id: 1,
            })
        }

        async fn client(&self) -> Client {
            let mut c = self.try_connect().await.expect("connect");
            let r = c
                .request(Request::Hello {
                    client: "test".into(),
                    protocol: PROTOCOL_VERSION,
                })
                .await;
            assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
            c
        }

        async fn stop(&self, d: Daemon) -> RunOutcome {
            let mut c = self.client().await;
            c.request(Request::Shutdown).await;
            exited(d).await
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            if let SocketName::Path(p) = &self.socket {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    async fn exited(d: Daemon) -> RunOutcome {
        tokio::time::timeout(TIMEOUT, d)
            .await
            .expect("daemon did not exit in time")
            .expect("daemon task panicked")
            .expect("daemon failed")
    }

    struct Client {
        rd: MsgReader<BufReader<ReadHalf<Stream>>>,
        wr: WriteHalf<Stream>,
        next_id: u64,
    }

    impl Client {
        async fn request(&mut self, body: Request) -> Reply {
            let req_id = self.next_id;
            self.next_id += 1;
            write_msg(&mut self.wr, &Envelope { req_id, body })
                .await
                .unwrap();
            loop {
                let msg = tokio::time::timeout(TIMEOUT, self.rd.next::<ServerMsg>())
                    .await
                    .expect("timed out waiting for a response")
                    .ok()
                    .flatten();
                match msg {
                    Some(ServerMsg::Response { req_id: r, result }) if r == req_id => {
                        return result;
                    }
                    Some(_) => {}
                    None => panic!("connection closed waiting for response {req_id}"),
                }
            }
        }

        async fn update(&mut self, check_only: bool) -> Reply {
            self.request(Request::Update { check_only }).await
        }

        async fn get_update(&mut self) -> UpdateStatus {
            match self.request(Request::GetUpdate).await {
                Reply::Ok {
                    data: ReplyData::Update { status },
                } => status,
                r => panic!("{r:?}"),
            }
        }

        /// Waits (up to `within`) for an update event matching `want`.
        async fn wait_update(
            &mut self,
            within: Duration,
            want: impl Fn(&UpdateStatus) -> bool,
        ) -> UpdateStatus {
            let deadline = tokio::time::Instant::now() + within;
            loop {
                let msg = tokio::time::timeout_at(deadline, self.rd.next::<ServerMsg>())
                    .await
                    .expect("no matching update event in time")
                    .ok()
                    .flatten()
                    .expect("connection closed");
                if let ServerMsg::Event {
                    event: Event::Update { status },
                } = msg
                    && want(&status)
                {
                    return status;
                }
            }
        }
    }

    fn reply_status(r: Reply) -> UpdateStatus {
        match r {
            Reply::Ok {
                data: ReplyData::Update { status },
            } => status,
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_available_after_start_check() {
        let env = Env::new().await;
        let d = env.start(true).await;
        let mut c = env.client().await;
        c.request(Request::Subscribe { interval_ms: 1000 }).await;
        let s = c
            .wait_update(Duration::from_secs(5), |s| {
                matches!(s, UpdateStatus::Available { .. })
            })
            .await;
        match s {
            UpdateStatus::Available {
                version,
                can_install,
                ..
            } => {
                assert_eq!(version, "0.2.0");
                assert!(can_install);
            }
            _ => unreachable!(),
        }
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn check_only_does_not_install() {
        let env = Env::new().await;
        let d = env.start(true).await;
        let mut c = env.client().await;
        let s = reply_status(c.update(true).await);
        assert!(
            matches!(&s, UpdateStatus::Available { version, can_install: true, .. } if version == "0.2.0"),
            "{s:?}"
        );
        assert_eq!(std::fs::read(&env.exe).unwrap(), b"old");
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    /// Waits until the scheduled lookup has reached the (slow) fake.
    async fn scheduled_lookup_in_flight(fake: &FakeGitHub) {
        let deadline = Instant::now() + TIMEOUT;
        while fake.hits() == 0 {
            assert!(Instant::now() < deadline, "no scheduled lookup");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn check_during_scheduled_check_gets_its_result() {
        let env = Env::new().await;
        env.fake.set_delay(Duration::from_millis(1500));
        let d = env.start(true).await;
        scheduled_lookup_in_flight(&env.fake).await;
        let mut c = env.client().await;
        let s = reply_status(c.update(true).await);
        assert!(
            matches!(&s, UpdateStatus::Available { version, .. } if version == "0.2.0"),
            "{s:?}"
        );
        assert_eq!(
            env.fake.hits(),
            1,
            "the waiting check shares the running one"
        );
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn install_during_scheduled_check_with_nothing_newer_is_up_to_date() {
        let env = Env::with(FakeRelease::new("0.1.0", puppy_daemon::update::TARGET)).await;
        env.fake.set_delay(Duration::from_millis(1500));
        let d = env.start(true).await;
        scheduled_lookup_in_flight(&env.fake).await;
        let mut c = env.client().await;
        assert_eq!(reply_status(c.update(false).await), UpdateStatus::UpToDate);
        assert_eq!(std::fs::read(&env.exe).unwrap(), b"old");
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn not_due_means_no_request() {
        let env = Env::new().await;
        let state = env.paths().update_state;
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::fs::write(&state, format!(r#"{{"last_check":{}}}"#, unix_now() - 3600)).unwrap();
        let d = env.start(true).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(env.fake.hits(), 0);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn check_disabled_never_contacts() {
        let env = Env::new().await;
        env.write_config("[update]\ncheck = false\n");
        let d = env.start(true).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(env.fake.hits(), 0);
        let mut c = env.client().await;
        assert_eq!(c.get_update().await, UpdateStatus::NotChecked);
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    /// `check = false`: an update found by an explicit check is never installed by
    /// the daemon itself, neither when quiet nor before an idle exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn check_disabled_never_installs_by_itself() {
        let env = Env::new().await;
        env.write_config("[update]\ncheck = false\n");
        let d = env.launch(env.settling()).await;
        let mut c = env.client().await;
        let s = reply_status(c.update(true).await);
        assert!(
            matches!(
                &s,
                UpdateStatus::Available {
                    can_install: true,
                    ..
                }
            ),
            "{s:?}"
        );
        drop(c);
        tokio::time::sleep(SETTLE + Duration::from_secs(2)).await;
        assert!(!d.is_finished(), "the daemon stopped");
        assert_eq!(env.fake.downloads(), 0, "downloaded the update by itself");
        assert!(!env.exe_swapped(), "installed by itself");
        assert_eq!(env.stop(d).await, RunOutcome::Exited);

        let mut opts = env.opts(true);
        opts.idle_exit = Some(Duration::from_secs(1));
        let d = env.launch(opts).await;
        let mut c = env.client().await;
        let s = reply_status(c.update(true).await);
        assert!(matches!(s, UpdateStatus::Available { .. }), "{s:?}");
        drop(c);
        assert_eq!(exited(d).await, RunOutcome::Exited);
        assert_eq!(
            env.fake.downloads(),
            0,
            "downloaded the update before exiting"
        );
        assert!(!env.exe_swapped(), "installed before exiting");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_check_waits_a_day() {
        let env = Env::new().await;
        env.fake.set_status(503);
        let d = env.start(true).await;
        let mut c = env.client().await;
        c.request(Request::Subscribe { interval_ms: 1000 }).await;
        let s = c
            .wait_update(Duration::from_secs(5), |s| {
                matches!(s, UpdateStatus::Failed { .. })
            })
            .await;
        match s {
            UpdateStatus::Failed { error, .. } => assert!(error.contains("503"), "{error}"),
            _ => unreachable!(),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(env.fake.hits(), 1);
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn install_request_swaps_and_restarts() {
        let env = Env::new().await;
        let d = env.start(true).await;
        let mut c = env.client().await;
        let s = reply_status(c.update(false).await);
        assert_eq!(
            s,
            UpdateStatus::Installed {
                version: "0.2.0".into()
            }
        );
        assert_eq!(exited(d).await, RunOutcome::Restart);
        let out = std::process::Command::new(&env.exe)
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "puppy 0.2.0");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cannot_install_is_unsupported() {
        let env = Env::new().await;
        let d = env.start(false).await;
        let mut c = env.client().await;
        match c.update(false).await {
            Reply::Err { code, message } => {
                assert_eq!(code, ErrorCode::Unsupported);
                assert!(message.contains("release workflow"), "{message}");
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(std::fs::read(&env.exe).unwrap(), b"old");
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    // ---- Automatic installs ----

    impl Client {
        async fn add(&mut self, req: AddRequest) -> DownloadId {
            match self
                .request(Request::Add {
                    request: req,
                    start_paused: false,
                    limit: None,
                })
                .await
            {
                Reply::Ok {
                    data: ReplyData::Added { id },
                } => id,
                r => panic!("add failed: {r:?}"),
            }
        }

        async fn get(&mut self, id: DownloadId) -> (WireState, u64) {
            match self.request(Request::Get { id }).await {
                Reply::Ok {
                    data: ReplyData::Download { download, .. },
                } => (download.state, download.done_bytes),
                r => panic!("get failed: {r:?}"),
            }
        }

        /// Polls download `id` until `want` holds; returns its done bytes then.
        async fn wait_download(
            &mut self,
            id: DownloadId,
            want: impl Fn(&WireState, u64) -> bool,
        ) -> u64 {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let (st, done) = self.get(id).await;
                if want(&st, done) {
                    return done;
                }
                assert!(Instant::now() < deadline, "never reached: {st:?} {done}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Polls the update status until an installable release is known.
        async fn wait_available(&mut self) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let s = self.get_update().await;
                if matches!(
                    s,
                    UpdateStatus::Available {
                        can_install: true,
                        ..
                    }
                ) {
                    return;
                }
                assert!(Instant::now() < deadline, "no update found: {s:?}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
        let mut c = ServerCfg::default();
        f(&mut c);
        TestServer::start(c).await
    }

    /// The version the new binary reports.
    #[cfg(unix)]
    fn exe_version(env: &Env) -> String {
        let out = std::process::Command::new(&env.exe)
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn quiet_daemon_installs_by_itself() {
        let env = Env::new().await;
        let d = env.launch(env.settling()).await;
        let outcome = tokio::time::timeout(Duration::from_secs(10), d)
            .await
            .expect("no automatic install within 10 s")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RunOutcome::Restart);
        assert_eq!(exe_version(&env), "puppy 0.2.0");
    }

    /// The HTTP engine, except that a finished download seeds (like a torrent would).
    #[cfg(unix)]
    struct Seeder(puppy_core::http::engine::HttpEngine);

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl puppy_core::engine::Engine for Seeder {
        fn kind(&self) -> puppy_core::engine::EngineKind {
            self.0.kind()
        }
        fn accepts(&self, source: &Source) -> bool {
            self.0.accepts(source)
        }
        async fn add(
            &self,
            id: DownloadId,
            req: AddRequest,
            mode: puppy_core::engine::StartMode,
            opts: puppy_core::engine::AddOptions,
        ) -> Result<puppy_core::engine::Added, puppy_core::engine::EngineError> {
            self.0.add(id, req, mode, opts).await
        }
        async fn pause(&self, id: DownloadId) -> Result<(), puppy_core::engine::EngineError> {
            self.0.pause(id).await
        }
        async fn resume(&self, id: DownloadId) -> Result<(), puppy_core::engine::EngineError> {
            self.0.resume(id).await
        }
        async fn remove(
            &self,
            id: DownloadId,
            delete_files: bool,
        ) -> Result<(), puppy_core::engine::EngineError> {
            self.0.remove(id, delete_files).await
        }
        fn status(&self, id: DownloadId) -> Option<puppy_core::types::DownloadStatus> {
            use puppy_core::types::DownloadState;
            self.0.status(id).map(|mut s| {
                if s.state == DownloadState::Completed {
                    s.state = DownloadState::Seeding {
                        uploaded: 0,
                        ratio: 0.0,
                        seeded_secs: 0,
                        ratio_target: None,
                        time_target_secs: None,
                    };
                }
                s
            })
        }
        fn detail(&self, id: DownloadId) -> Option<puppy_core::types::EngineDetail> {
            self.0.detail(id)
        }
        fn set_download_limit(
            &self,
            id: DownloadId,
            bps: Option<u64>,
        ) -> Result<(), puppy_core::engine::EngineError> {
            self.0.set_download_limit(id, bps)
        }
        fn set_global_download_budget(&self, bps: Option<u64>) {
            self.0.set_global_download_budget(bps)
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<puppy_core::engine::EngineEvent> {
            self.0.subscribe()
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn seeding_does_not_block() {
        let env = Env::new().await;
        let s = server(|c| c.len = 16 * KIB).await;
        let mut opts = env.settling();
        let http = opts.http.clone().unwrap();
        opts.engine = Some(Arc::new(Seeder(
            puppy_core::http::engine::HttpEngine::new(http).unwrap(),
        )));
        let d = env.launch(opts).await;
        let mut c = env.client().await;
        let id = c.add(env.req(&s, "a.bin")).await;
        c.wait_download(id, |st, _| matches!(st, WireState::Seeding { .. }))
            .await;
        c.wait_available().await;
        drop(c);
        let outcome = tokio::time::timeout(Duration::from_secs(10), d)
            .await
            .expect("seeding blocked the automatic install")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RunOutcome::Restart);
        assert_eq!(exe_version(&env), "puppy 0.2.0");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn active_download_blocks() {
        let env = Env::new().await;
        let s = server(|c| {
            c.len = 2048 * KIB;
            c.bytes_per_sec = Some(32 * KIB);
        })
        .await;
        let d = env.launch(env.settling()).await;
        let mut c = env.client().await;
        let id = c.add(env.req(&s, "a.bin")).await;
        c.wait_download(id, |st, _| *st == WireState::Active).await;
        c.wait_available().await;
        drop(c);
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(env.fake.hits() >= 1, "no check ran");
        assert!(!d.is_finished(), "the daemon stopped");
        assert!(!env.exe_swapped(), "installed during a download");
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn client_during_settle_resets_it() {
        // Longer than `SETTLE`: the second client must arrive well within it, however
        // slow the machine.
        let settle = Duration::from_secs(3);
        let env = Env::new().await;
        let mut opts = env.settling();
        opts.update.as_mut().unwrap().settle = settle;
        let d = env.launch(opts).await;
        let mut c = env.client().await;
        c.wait_available().await;
        drop(c);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let c = env.client().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!d.is_finished(), "installed while a client was connected");
        assert!(!env.exe_swapped(), "installed while a client was connected");
        drop(c);
        let quiet_from = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(10), d)
            .await
            .expect("no automatic install after the client left")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RunOutcome::Restart);
        assert!(
            quiet_from.elapsed() >= settle,
            "installed {:?} after the client left",
            quiet_from.elapsed()
        );
        assert_eq!(exe_version(&env), "puppy 0.2.0");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forced_install_restarts_with_state_saved() {
        let env = Env::new().await;
        let s = server(|c| {
            c.len = 8192 * KIB;
            c.bytes_per_sec = Some(128 * KIB);
        })
        .await;
        let d = env.launch(env.settling()).await;
        let mut c = env.client().await;
        let id = c.add(env.req(&s, "a.bin")).await;
        let before = c
            .wait_download(id, |st, done| *st == WireState::Active && done >= 128 * KIB)
            .await;
        let s2 = reply_status(c.update(false).await);
        assert!(matches!(s2, UpdateStatus::Installed { .. }), "{s2:?}");
        assert_eq!(exited(d).await, RunOutcome::Restart);
        assert!(env.exe_swapped());
        let marker = env.paths().update_dir.join("updated-from");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            env!("CARGO_PKG_VERSION")
        );

        // The restarted daemon picks the download up where it was paused.
        let mut opts = env.settling();
        opts.update.as_mut().unwrap().settle = Duration::from_secs(60);
        let d = env.launch(opts).await;
        assert!(!marker.exists(), "the restarted daemon takes the marker");
        let mut c = env.client().await;
        let (st, done) = c.get(id).await;
        assert!(
            done >= before,
            "restarted from {done} bytes, had {before} ({st:?})"
        );
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_exit_installs_first() {
        let env = Env::new().await;
        let mut opts = env.opts(true);
        opts.idle_exit = Some(Duration::from_secs(1));
        let d = env.launch(opts).await;
        // Kept busy until the check has found the release.
        let mut c = env.client().await;
        c.wait_available().await;
        drop(c);
        assert_eq!(exited(d).await, RunOutcome::Exited);
        assert_eq!(exe_version(&env), "puppy 0.2.0");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn updated_marker_notifies_once() {
        let env = Env::new().await;
        let marker = env.paths().update_dir.join("updated-from");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "0.0.1").unwrap();
        let rec = RecordingNotifier::default();
        let notifying = || DaemonOptions {
            notifier: Some(Arc::new(rec.clone())),
            ..env.opts(true)
        };
        let d = env.launch(notifying()).await;
        assert!(!marker.exists(), "the marker was not removed");
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
        assert_eq!(
            rec.take(),
            [Notification {
                kind: NotifyKind::Updated,
                title: "puppy updated".into(),
                body: format!("puppy updated to {}", env!("CARGO_PKG_VERSION")),
            }]
        );
        // Once only.
        let d = env.launch(notifying()).await;
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
        assert!(rec.take().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_install_leaves_no_marker() {
        let env = Env::with(
            FakeRelease::new("0.2.0", puppy_daemon::update::TARGET).binary_prints("puppy 9.9.8"),
        )
        .await;
        let d = env.start(true).await;
        let mut c = env.client().await;
        match c.update(false).await {
            Reply::Err { message, .. } => assert!(message.contains("9.9.8"), "{message}"),
            r => panic!("{r:?}"),
        }
        assert!(!env.paths().update_dir.join("updated-from").exists());
        assert!(!env.exe_swapped());
        drop(c);
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    /// Polls the update status with short-lived clients until `want` holds.
    #[cfg(unix)]
    async fn wait_status(env: &Env, d: &Daemon, want: impl Fn(&UpdateStatus) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(!d.is_finished(), "the daemon stopped");
            let s = env.client().await.get_update().await;
            if want(&s) {
                return;
            }
            assert!(Instant::now() < deadline, "status never reached: {s:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until the fake has been asked for the release archive.
    #[cfg(unix)]
    async fn archive_requested(fake: &FakeGitHub) {
        let deadline = Instant::now() + TIMEOUT;
        while fake.downloads() == 0 {
            assert!(Instant::now() < deadline, "the update was never downloaded");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// An automatic install that turns out not to be possible (the folder became
    /// read-only) marks the release as not installable instead of retrying forever.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_install_not_possible_stops_retrying() {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new().await;
        let bin = env.exe.parent().unwrap().to_path_buf();
        let d = env.launch(env.settling()).await;
        let mut c = env.client().await;
        c.wait_available().await;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(bin.join("probe")).is_ok() {
            // Running as root: the folder stays writable, nothing to test.
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            drop(c);
            env.stop(d).await;
            return;
        }
        drop(c);
        wait_status(&env, &d, |s| {
            matches!(
                s,
                UpdateStatus::Available { can_install: false, reason: Some(r), .. }
                    if r.contains("cannot write")
            )
        })
        .await;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(env.fake.downloads(), 0);
        assert!(!env.exe_swapped());
        assert_eq!(env.stop(d).await, RunOutcome::Exited);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_install_aborts_when_download_starts_during_fetch() {
        let env = Env::new().await;
        // Long enough for the download below to be added and become active while the
        // update is still being fetched, however slow the machine.
        env.fake.set_download_delay(Duration::from_secs(5));
        let s = server(|c| {
            c.len = 8192 * KIB;
            c.bytes_per_sec = Some(64 * KIB);
        })
        .await;
        let d = env.launch(env.settling()).await;
        archive_requested(&env.fake).await;
        let id = {
            let mut c = env.client().await;
            let id = c.add(env.req(&s, "a.bin")).await;
            c.wait_download(id, |st, _| *st == WireState::Active).await;
            id
        };
        // The fetch finishes, the binary checks out, then the install gives way.
        wait_status(&env, &d, |s| matches!(s, UpdateStatus::Installing { .. })).await;
        wait_status(&env, &d, |s| {
            matches!(
                s,
                UpdateStatus::Available {
                    can_install: true,
                    ..
                }
            )
        })
        .await;
        assert!(!env.exe_swapped(), "swapped during a download");
        let mut c = env.client().await;
        let (st, _) = c.get(id).await;
        assert_eq!(st, WireState::Active, "the download was disturbed");

        // Once the download stops and the daemon settles, the staged binary goes in.
        c.request(Request::Pause { id }).await;
        drop(c);
        assert_eq!(exited(d).await, RunOutcome::Restart);
        assert_eq!(exe_version(&env), "puppy 0.2.0");
        assert_eq!(env.fake.downloads(), 1, "downloaded the update again");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_exit_install_aborts_when_client_connects() {
        let env = Env::new().await;
        env.fake.set_download_delay(Duration::from_secs(2));
        let mut opts = env.opts(true);
        opts.idle_exit = Some(Duration::from_secs(1));
        let d = env.launch(opts).await;
        let mut c = env.client().await;
        c.wait_available().await;
        drop(c);
        archive_requested(&env.fake).await;
        let mut c = env.client().await;
        // The install gives way and the daemon keeps serving.
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let s = c.get_update().await;
            if matches!(s, UpdateStatus::Available { .. }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the install never gave way: {s:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!d.is_finished(), "exited with a client connected");
        assert!(!env.exe_swapped(), "swapped with a client connected");
        assert!(matches!(c.request(Request::List).await, Reply::Ok { .. }));
        // Idle again: it installs (without downloading again) and exits.
        drop(c);
        assert_eq!(exited(d).await, RunOutcome::Exited);
        assert_eq!(exe_version(&env), "puppy 0.2.0");
        assert_eq!(env.fake.downloads(), 1, "downloaded the update again");
    }
}
