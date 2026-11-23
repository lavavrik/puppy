//! Where Chrome-family browsers look for native messaging hosts, and the host manifest
//! `puppy browser install` writes there (see the README's "Browser downloads").

#[cfg(windows)]
pub mod registry;

use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The native messaging host's name, as the extension passes it to the browser.
pub const HOST_NAME: &str = "dev.puppy.native_host";
pub const HOST_DESCRIPTION: &str = "puppy download manager";
/// The extension's fixed ID: derived from the public key in `extension/manifest.json`.
pub const EXTENSION_ID: &str = "cbfmabianfipnmdcdipmcpaggpejlaeg";

#[derive(clap::ValueEnum, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Browser {
    Chrome,
    Chromium,
    Brave,
    Edge,
    Vivaldi,
}

impl Browser {
    pub const ALL: [Browser; 5] = [
        Browser::Chrome,
        Browser::Chromium,
        Browser::Brave,
        Browser::Edge,
        Browser::Vivaldi,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Brave => "brave",
            Browser::Edge => "edge",
            Browser::Vivaldi => "vivaldi",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl Os {
    /// The running OS; other Unixes use the Linux layout.
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

/// The environment the locations derive from.
pub struct BrowserEnv {
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
}

impl BrowserEnv {
    /// `HOME` (`USERPROFILE` on Windows), `XDG_CONFIG_HOME` (empty counts as unset)
    /// and `LOCALAPPDATA`.
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let local_app_data = var("LOCALAPPDATA");
        let home = var("HOME").or_else(|| var("USERPROFILE"));
        match (home, &local_app_data) {
            (Some(home), _) => Ok(BrowserEnv {
                home,
                xdg_config_home: var("XDG_CONFIG_HOME"),
                local_app_data,
            }),
            (None, Some(l)) => Ok(BrowserEnv {
                home: l.clone(),
                xdg_config_home: var("XDG_CONFIG_HOME"),
                local_app_data,
            }),
            (None, None) => anyhow::bail!("cannot find your home directory (HOME is not set)"),
        }
    }
}

/// Where one browser keeps its config and native messaging host registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The browser's own config dir; its existence means the browser is installed.
    pub config_dir: PathBuf,
    /// The host manifest file puppy writes.
    pub manifest: PathBuf,
    /// Windows only: the HKCU key whose default value points at `manifest`.
    pub registry_key: Option<String>,
}

fn manifest_file() -> String {
    format!("{HOST_NAME}.json")
}

pub fn location(b: Browser, os: Os, env: &BrowserEnv) -> Location {
    match os {
        Os::Linux => {
            let c = env
                .xdg_config_home
                .clone()
                .unwrap_or_else(|| env.home.join(".config"));
            let dir = match b {
                Browser::Chrome => "google-chrome",
                Browser::Chromium => "chromium",
                Browser::Brave => "BraveSoftware/Brave-Browser",
                Browser::Edge => "microsoft-edge",
                Browser::Vivaldi => "vivaldi",
            };
            let config_dir = c.join(dir);
            Location {
                manifest: config_dir
                    .join("NativeMessagingHosts")
                    .join(manifest_file()),
                config_dir,
                registry_key: None,
            }
        }
        Os::MacOs => {
            let dir = match b {
                Browser::Chrome => "Google/Chrome",
                Browser::Chromium => "Chromium",
                Browser::Brave => "BraveSoftware/Brave-Browser",
                Browser::Edge => "Microsoft Edge",
                Browser::Vivaldi => "Vivaldi",
            };
            let config_dir = env.home.join("Library/Application Support").join(dir);
            Location {
                manifest: config_dir
                    .join("NativeMessagingHosts")
                    .join(manifest_file()),
                config_dir,
                registry_key: None,
            }
        }
        Os::Windows => {
            let l = env
                .local_app_data
                .clone()
                .unwrap_or_else(|| env.home.join("AppData").join("Local"));
            let vendor = match b {
                Browser::Chrome => r"Google\Chrome",
                Browser::Chromium => r"Chromium",
                Browser::Brave => r"BraveSoftware\Brave-Browser",
                Browser::Edge => r"Microsoft\Edge",
                Browser::Vivaldi => r"Vivaldi",
            };
            let config_dir = vendor
                .split('\\')
                .fold(l.clone(), |p, part| p.join(part))
                .join("User Data");
            // Vivaldi reads Chrome's registry key (KeePassXC and others rely on it), so
            // it shares Chrome's key and manifest file.
            let reg = if b == Browser::Vivaldi {
                r"Google\Chrome"
            } else {
                vendor
            };
            let dir = if b == Browser::Vivaldi {
                Browser::Chrome.name()
            } else {
                b.name()
            };
            Location {
                config_dir,
                manifest: l
                    .join("puppy")
                    .join("NativeMessagingHosts")
                    .join(dir)
                    .join(manifest_file()),
                registry_key: Some(format!(r"Software\{reg}\NativeMessagingHosts\{HOST_NAME}")),
            }
        }
    }
}

/// Who may start the host: a Chrome extension origin, or a Firefox extension id.
pub enum Allowed<'a> {
    ChromeOrigin(&'a str),
    /// Designed in but never installed: Firefox is not shipped yet.
    #[allow(dead_code)]
    FirefoxExtension(&'a str),
}

pub fn host_manifest(exe: &Path, allowed: Allowed<'_>) -> Value {
    let mut m = json!({
        "name": HOST_NAME,
        "description": HOST_DESCRIPTION,
        "path": exe.to_string_lossy(),
        "type": "stdio",
    });
    match allowed {
        Allowed::ChromeOrigin(id) => {
            m["allowed_origins"] = json!([format!("chrome-extension://{id}/")]);
        }
        Allowed::FirefoxExtension(id) => m["allowed_extensions"] = json!([id]),
    }
    m
}

/// Chrome cannot start `\\?\C:\...`: strips the verbatim prefix before a drive letter
/// (`\\?\UNC\...` and ordinary paths stay).
pub fn manifest_exe_path(p: &str) -> String {
    match p.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => p.to_string(),
    }
}

/// The path to write into the manifests: this executable, canonicalized.
pub fn canonical_exe() -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("cannot find the puppy executable")?;
    let exe =
        std::fs::canonicalize(&exe).with_context(|| format!("cannot resolve {}", exe.display()))?;
    Ok(PathBuf::from(manifest_exe_path(&exe.to_string_lossy())))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HostState {
    Missing,
    Ok,
    StalePath { found: PathBuf },
    WrongOrigin,
    Unreadable { error: String },
}

/// Whether the manifest at `manifest` is one this puppy (`exe`) wrote.
pub fn check_manifest(manifest: &Path, exe: &Path) -> HostState {
    let bytes = match std::fs::read(manifest) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HostState::Missing,
        Err(e) => {
            return HostState::Unreadable {
                error: e.to_string(),
            };
        }
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return HostState::Unreadable {
                error: e.to_string(),
            };
        }
    };
    let Some(path) = v.get("path").and_then(Value::as_str) else {
        return HostState::Unreadable {
            error: "no \"path\" in the manifest".into(),
        };
    };
    if Path::new(path) != exe {
        return HostState::StalePath {
            found: PathBuf::from(path),
        };
    }
    let want = json!([format!("chrome-extension://{EXTENSION_ID}/")]);
    if v.get("allowed_origins") != Some(&want) {
        return HostState::WrongOrigin;
    }
    HostState::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: [(Browser, &str); 5] = [
        (Browser::Chrome, "google-chrome"),
        (Browser::Chromium, "chromium"),
        (Browser::Brave, "BraveSoftware/Brave-Browser"),
        (Browser::Edge, "microsoft-edge"),
        (Browser::Vivaldi, "vivaldi"),
    ];

    fn env(xdg: Option<&str>) -> BrowserEnv {
        BrowserEnv {
            home: "/h".into(),
            xdg_config_home: xdg.map(Into::into),
            local_app_data: Some(r"L".into()),
        }
    }

    #[test]
    fn linux_locations() {
        for (b, dir) in ROWS {
            for (xdg, base) in [(None, "/h/.config"), (Some("/x"), "/x")] {
                let l = location(b, Os::Linux, &env(xdg));
                let cfg = PathBuf::from(format!("{base}/{dir}"));
                assert_eq!(l.config_dir, cfg);
                assert_eq!(
                    l.manifest,
                    cfg.join("NativeMessagingHosts/dev.puppy.native_host.json")
                );
                assert_eq!(l.registry_key, None);
            }
        }
    }

    #[test]
    fn macos_locations() {
        let rows = [
            (Browser::Chrome, "Google/Chrome"),
            (Browser::Chromium, "Chromium"),
            (Browser::Brave, "BraveSoftware/Brave-Browser"),
            (Browser::Edge, "Microsoft Edge"),
            (Browser::Vivaldi, "Vivaldi"),
        ];
        for (b, dir) in rows {
            let l = location(b, Os::MacOs, &env(Some("/ignored")));
            let cfg = PathBuf::from(format!("/h/Library/Application Support/{dir}"));
            assert_eq!(l.config_dir, cfg);
            assert_eq!(
                l.manifest,
                cfg.join("NativeMessagingHosts/dev.puppy.native_host.json")
            );
            assert_eq!(l.registry_key, None);
        }
    }

    #[test]
    fn windows_locations() {
        // (browser, vendor path components, registry vendor, manifest dir)
        let rows: [(Browser, &[&str], &str, &str); 5] = [
            (
                Browser::Chrome,
                &["Google", "Chrome"],
                r"Google\Chrome",
                "chrome",
            ),
            (Browser::Chromium, &["Chromium"], "Chromium", "chromium"),
            (
                Browser::Brave,
                &["BraveSoftware", "Brave-Browser"],
                r"BraveSoftware\Brave-Browser",
                "brave",
            ),
            (
                Browser::Edge,
                &["Microsoft", "Edge"],
                r"Microsoft\Edge",
                "edge",
            ),
            // Vivaldi reads Chrome's key, so it shares Chrome's key and manifest.
            (Browser::Vivaldi, &["Vivaldi"], r"Google\Chrome", "chrome"),
        ];
        for (b, parts, reg, mdir) in rows {
            let l = location(b, Os::Windows, &env(None));
            let mut cfg = PathBuf::from("L");
            cfg.extend(parts);
            cfg.push("User Data");
            assert_eq!(l.config_dir, cfg, "{b:?}");
            assert_eq!(
                l.manifest,
                PathBuf::from("L")
                    .join("puppy")
                    .join("NativeMessagingHosts")
                    .join(mdir)
                    .join("dev.puppy.native_host.json")
            );
            assert_eq!(
                l.registry_key,
                Some(format!(
                    r"Software\{reg}\NativeMessagingHosts\dev.puppy.native_host"
                ))
            );
        }
        assert_eq!(
            location(Browser::Chrome, Os::Windows, &env(None)).registry_key,
            Some(r"Software\Google\Chrome\NativeMessagingHosts\dev.puppy.native_host".into())
        );
    }

    #[test]
    fn chrome_manifest_json() {
        assert_eq!(
            host_manifest(Path::new("/opt/puppy"), Allowed::ChromeOrigin(EXTENSION_ID)),
            json!({
                "name": "dev.puppy.native_host",
                "description": "puppy download manager",
                "path": "/opt/puppy",
                "type": "stdio",
                "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
            })
        );
    }

    #[test]
    fn firefox_manifest_json() {
        let m = host_manifest(
            Path::new("/opt/puppy"),
            Allowed::FirefoxExtension("puppy@example"),
        );
        assert_eq!(m["allowed_extensions"], json!(["puppy@example"]));
        assert!(m.get("allowed_origins").is_none());
    }

    #[test]
    fn verbatim_prefix_stripped() {
        assert_eq!(
            manifest_exe_path(r"\\?\C:\bin\puppy.exe"),
            r"C:\bin\puppy.exe"
        );
        for same in [r"\\?\UNC\srv\x\puppy.exe", "/usr/bin/puppy"] {
            assert_eq!(manifest_exe_path(same), same);
        }
    }

    #[test]
    fn extension_id_shape() {
        assert_eq!(EXTENSION_ID.len(), 32);
        assert!(EXTENSION_ID.chars().all(|c| ('a'..='p').contains(&c)));
    }

    #[test]
    fn check_manifest_states() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.json");
        let (a, b) = (Path::new("/bin/a"), Path::new("/bin/b"));
        assert_eq!(check_manifest(&file, a), HostState::Missing);

        let write = |m: Value| std::fs::write(&file, serde_json::to_vec(&m).unwrap()).unwrap();
        write(host_manifest(a, Allowed::ChromeOrigin(EXTENSION_ID)));
        assert_eq!(check_manifest(&file, a), HostState::Ok);
        assert_eq!(
            check_manifest(&file, b),
            HostState::StalePath { found: a.into() }
        );
        write(host_manifest(a, Allowed::ChromeOrigin("other")));
        assert_eq!(check_manifest(&file, a), HostState::WrongOrigin);
        std::fs::write(&file, "{").unwrap();
        assert!(matches!(
            check_manifest(&file, a),
            HostState::Unreadable { .. }
        ));
    }
}
