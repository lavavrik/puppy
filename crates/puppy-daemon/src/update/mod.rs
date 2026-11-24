//! Self-update: facts about this build, finding a release, and swapping it in.

mod install;
mod release;
mod service;

pub use install::{cannot_install_reason, fetch, remove_old_exe, replace_exe, sanity_check};
pub use release::{Asset, Release, UpdateError, archive_name, latest};
pub use service::{UpdateOptions, Updater};

/// The target triple this binary was built for (names its release asset).
pub const TARGET: &str = env!("PUPPY_TARGET");

/// True only for binaries made by the release workflow (`PUPPY_RELEASE_BUILD=1`);
/// every other build refuses to install updates.
pub const RELEASE_BUILD: bool = is_one(option_env!("PUPPY_RELEASE_BUILD"));

const fn is_one(v: Option<&str>) -> bool {
    match v {
        Some(s) => {
            let b = s.as_bytes();
            b.len() == 1 && b[0] == b'1'
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_is_a_triple() {
        assert!(TARGET.split('-').count() >= 3, "{TARGET}");
    }
}
