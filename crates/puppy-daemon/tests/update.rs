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
    use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
    use puppy_daemon::update::UpdateOptions;
    use puppy_daemon::{Paths, SocketName};
    use puppy_proto::{
        Envelope, ErrorCode, Event, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request,
        ServerMsg, UpdateStatus, write_msg,
    };
    use puppy_testserver::github::{FakeGitHub, FakeRelease};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::io::{BufReader, ReadHalf, WriteHalf};
    use tokio::task::JoinHandle;

    const TIMEOUT: Duration = Duration::from_secs(20);
    const DAY: Duration = Duration::from_secs(24 * 3600);

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

        async fn start(&self, allow_dev: bool) -> Daemon {
            let d = tokio::spawn(run_daemon(DaemonOptions {
                paths: Some(self.paths()),
                torrent_test: Some(Default::default()),
                update: Some(self.update_opts(allow_dev)),
                ..DaemonOptions::default()
            }));
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
}
