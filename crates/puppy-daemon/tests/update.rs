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
    let reason = cannot_install_reason(&exe, false).unwrap();
    assert!(reason.contains("release workflow"), "{reason}");
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
