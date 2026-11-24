//! Exposes cargo's target triple to the crate as `PUPPY_TARGET` (used to pick the
//! release asset an update installs).
fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=PUPPY_TARGET={target}");
    println!("cargo:rerun-if-env-changed=PUPPY_RELEASE_BUILD");
    println!("cargo:rerun-if-changed=build.rs");
}
