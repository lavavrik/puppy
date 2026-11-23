//! `puppy browser install | uninstall | status`: registers puppy's native messaging host
//! with Chrome-family browsers, so the extension can hand downloads to puppy.

use super::CliError;
use super::output::{json, out};
use crate::browser::{
    Allowed, Browser, BrowserEnv, EXTENSION_ID, HostState, Location, Os, canonical_exe,
    check_manifest, host_manifest, location, shared_registration, uninstall_plan,
};
use anyhow::Context;
use serde_json::{Value, json};
use std::path::Path;

fn targets(only: Option<Browser>) -> Vec<Browser> {
    only.map_or(Browser::ALL.to_vec(), |b| vec![b])
}

/// Writes the manifest file and (Windows) points the registry key at it.
async fn write_registration(loc: &Location, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = loc.manifest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    puppy_core::fsutil::atomic_write(&loc.manifest, bytes, false)
        .await
        .with_context(|| format!("cannot write {}", loc.manifest.display()))?;
    #[cfg(windows)]
    if let Some(key) = &loc.registry_key {
        crate::browser::registry::set(key, &loc.manifest)
            .with_context(|| format!("cannot write HKCU\\{key}"))?;
    }
    Ok(())
}

/// Removes the manifest file and (Windows) the registry key; whether either existed.
fn remove_registration(loc: &Location) -> anyhow::Result<bool> {
    let mut any = false;
    match std::fs::remove_file(&loc.manifest) {
        Ok(()) => any = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("cannot remove {}", loc.manifest.display()))
            );
        }
    }
    #[cfg(windows)]
    if let Some(key) = &loc.registry_key {
        any |= crate::browser::registry::delete(key)
            .with_context(|| format!("cannot remove HKCU\\{key}"))?;
    }
    Ok(any)
}

/// Registers for every browser found, or only `only`. A browser that is not found is
/// skipped, or (named with `only`) an error unless `force`: its config dir, where the
/// Linux and macOS manifests live, is never created on a guess.
pub async fn install(json_out: bool, only: Option<Browser>, force: bool) -> Result<(), CliError> {
    let os = Os::current();
    let env = BrowserEnv::from_env()?;
    let exe = canonical_exe()?;
    let bytes =
        serde_json::to_vec_pretty(&host_manifest(&exe, Allowed::ChromeOrigin(EXTENSION_ID)))
            .context("cannot encode the host manifest")?;
    let shared = shared_registration(os, &env);
    let (mut installed, mut skipped) = (Vec::new(), Vec::new());
    for b in targets(only) {
        let loc = location(b, os, &env);
        if !loc.config_dir.exists() {
            if only.is_none() {
                skipped.push(b.name());
                continue;
            }
            if !force {
                return Err(CliError::Other(anyhow::anyhow!(
                    "{} not found (no {}); use --force to register anyway",
                    b.name(),
                    loc.config_dir.display()
                )));
            }
        }
        write_registration(&loc, &bytes).await?;
        // Windows: Brave gets its own key and, as a fallback, the shared (Chrome) one.
        if b == Browser::Brave
            && let Some(sh) = &shared
        {
            write_registration(sh, &bytes).await?;
        }
        if !json_out {
            out(format_args!(
                "installed for {}: {}",
                b.name(),
                loc.manifest.display()
            ));
        }
        installed.push(json!({
            "browser": b.name(),
            "manifest": loc.manifest,
            "registry_key": loc.registry_key,
        }));
    }
    if installed.is_empty() {
        return Err(CliError::Other(anyhow::anyhow!(
            "no Chrome-family browser found (Flatpak and Snap browsers are not supported); \
             use --browser <name> --force to install for one anyway"
        )));
    }
    if json_out {
        json(&json!({ "installed": installed, "skipped": skipped }));
    }
    Ok(())
}

pub async fn uninstall(json_out: bool, only: Option<Browser>) -> Result<(), CliError> {
    let os = Os::current();
    let env = BrowserEnv::from_env()?;
    let shared = shared_registration(os, &env);
    let (mut removed, mut kept) = (Vec::new(), Vec::new());
    for b in targets(only) {
        let loc = location(b, os, &env);
        let plan = uninstall_plan(b, &loc, shared.as_ref(), only.is_none(), |o| {
            location(o, os, &env).config_dir.exists()
        });
        let mut any = false;
        for reg in &plan.delete {
            any |= remove_registration(reg)?;
        }
        if !plan.kept.is_empty() {
            let names: Vec<_> = plan.kept.iter().map(|b| b.name()).collect();
            let msg = format!("kept the shared registration for {}", names.join(", "));
            if !json_out {
                out(&msg);
            }
            kept.push(msg);
        }
        if any {
            if !json_out {
                out(format_args!(
                    "removed for {}: {}",
                    b.name(),
                    loc.manifest.display()
                ));
            }
            removed.push(json!({ "browser": b.name(), "manifest": loc.manifest }));
        }
    }
    if json_out {
        json(&json!({ "removed": removed, "kept": kept }));
    } else if removed.is_empty() && kept.is_empty() {
        out("nothing to remove");
    }
    Ok(())
}

/// What is registered for one browser. On Windows the registry key is read first, and
/// Brave also needs the shared registration (its fallback).
fn host_state(b: Browser, os: Os, env: &BrowserEnv, exe: &Path) -> HostState {
    let own = registered_state(&location(b, os, env), exe);
    match shared_registration(os, env) {
        Some(sh) if b == Browser::Brave && own == HostState::Ok => registered_state(&sh, exe),
        _ => own,
    }
}

fn registered_state(loc: &Location, exe: &Path) -> HostState {
    #[cfg(windows)]
    if let Some(key) = &loc.registry_key {
        match crate::browser::registry::get(key) {
            Ok(None) => return HostState::Missing,
            Ok(Some(p)) if !crate::browser::same_path(&p, &loc.manifest) => {
                return HostState::StalePath { found: p };
            }
            Ok(Some(_)) => {}
            Err(e) => {
                return HostState::Unreadable {
                    error: e.to_string(),
                };
            }
        }
    }
    check_manifest(&loc.manifest, exe)
}

pub async fn status(json_out: bool) -> Result<(), CliError> {
    let os = Os::current();
    let env = BrowserEnv::from_env()?;
    let exe = canonical_exe()?;
    let mut rows: Vec<Value> = Vec::new();
    let mut lines = Vec::new();
    for b in Browser::ALL {
        let loc = location(b, os, &env);
        let found = loc.config_dir.exists();
        let host = host_state(b, os, &env, &exe);
        // Vivaldi has no registration of its own on Windows: Chrome's.
        let shares_chrome = b == Browser::Vivaldi && shared_registration(os, &env).is_some();
        let text = match &host {
            _ if !found && (host == HostState::Missing || shares_chrome) => "not found".to_string(),
            HostState::Missing => "not installed".to_string(),
            HostState::Ok if shares_chrome => {
                format!("ok (shared with chrome)  {}", loc.manifest.display())
            }
            HostState::Ok => format!("ok  {}", loc.manifest.display()),
            HostState::StalePath { found } => format!(
                "stale: points to {} \u{2014} re-run puppy browser install",
                found.display()
            ),
            HostState::WrongOrigin => {
                "wrong extension id \u{2014} re-run puppy browser install".to_string()
            }
            HostState::Unreadable { error } => format!("unreadable: {error}"),
        };
        lines.push(format!("{:<9}  {text}", b.name()));
        rows.push(json!({
            "browser": b.name(),
            "found": found,
            "manifest": loc.manifest,
            "registry_key": loc.registry_key,
            "host": host,
        }));
    }
    if json_out {
        json(&json!({
            "host_path": exe,
            "extension_id": EXTENSION_ID,
            "browsers": rows,
        }));
    } else {
        for l in lines {
            out(l);
        }
        out(format_args!("extension id: {EXTENSION_ID}"));
    }
    Ok(())
}
