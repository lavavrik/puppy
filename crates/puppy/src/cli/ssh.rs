//! `puppy ssh trust HOST[:PORT]`: fetches a server's host key, shows its fingerprint and,
//! once confirmed (a terminal prompt, or `--yes`), adds it to puppy's own `known_hosts`,
//! so that SFTP downloads from that host work. A host already known is left alone; a
//! host whose key changed is not trusted over the old key.

use super::output::{err, json, out};
use super::{CliError, Ctx};
use anyhow::{Context, anyhow};
use puppy_core::sftp::hostkey::{self, Verdict};
use std::io::{BufRead, IsTerminal, Write};

/// `host`, `host:port`, `[v6]:port`, a bare IPv6 address or an `sftp://` URL.
pub fn parse_target(s: &str) -> Result<(String, u16), String> {
    let s = s.trim();
    let bad = || format!("{s}: expected HOST, HOST:PORT or [IPv6]:PORT");
    if s.contains("://") {
        let url = url::Url::parse(s).map_err(|e| format!("{s}: {e}"))?;
        let host = match url.host() {
            Some(url::Host::Ipv6(a)) => a.to_string(),
            Some(h) => h.to_string(),
            None => return Err(bad()),
        };
        return Ok((host, url.port().unwrap_or(22)));
    }
    let port = |p: &str| p.parse::<u16>().ok().filter(|&p| p > 0).ok_or_else(bad);
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(bad)?;
        match after {
            "" => (host, 22),
            p => (host, port(p.strip_prefix(':').ok_or_else(bad)?)?),
        }
    } else if s.matches(':').count() > 1 {
        (s, 22)
    } else {
        match s.split_once(':') {
            Some((h, p)) => (h, port(p)?),
            None => (s, 22),
        }
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(bad());
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("cannot read the answer")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub async fn trust(ctx: &Ctx, target: &str, yes: bool) -> Result<(), CliError> {
    let (host, port) = parse_target(target).map_err(CliError::Usage)?;
    let cfg = puppy_daemon::Config::load(&ctx.paths.config_file())
        .map_err(|e| CliError::Other(anyhow!("{e}")))?;
    let trusted = ctx.paths.known_hosts();
    let mut http = cfg.http_config();
    http.sftp.trusted_hosts = Some(trusted.clone());
    let (key, verdict) = puppy_core::sftp::fetch_host_key(&host, port, &http)
        .await
        .map_err(|e| CliError::Other(anyhow!("{e}")))?;
    let name = hostkey::host_pattern(&host, port);
    let report = |trusted_now: bool, state: &str| {
        if ctx.json {
            json(&serde_json::json!({
                "host": host,
                "port": port,
                "key_type": key.algorithm().as_str(),
                "fingerprint": hostkey::fingerprint(&key),
                "state": state,
                "trusted": trusted_now,
                "known_hosts": trusted,
            }));
        }
    };
    match verdict {
        Verdict::Trusted => {
            report(true, "known");
            if !ctx.json {
                out(format_args!(
                    "{name} is already trusted ({})",
                    hostkey::describe(&key)
                ));
            }
            return Ok(());
        }
        Verdict::Changed { file, line } => {
            report(false, "changed");
            return Err(CliError::Other(anyhow!(
                "REMOTE HOST IDENTIFICATION HAS CHANGED for {name}: it presents {}, not \
                 the key in {}:{line}. Someone could be eavesdropping on you, or the host \
                 key was just changed. If the change is expected, remove the old key first \
                 (`ssh-keygen -R '{name}' -f {}`), then run this again",
                hostkey::describe(&key),
                file.display(),
                file.display()
            )));
        }
        Verdict::Revoked { file, line } => {
            report(false, "revoked");
            return Err(CliError::Other(anyhow!(
                "the host key of {name} ({}) is revoked in {}:{line}",
                hostkey::describe(&key),
                file.display()
            )));
        }
        Verdict::Unknown { .. } => {}
    }
    if !ctx.json || !yes {
        err(format_args!(
            "The host key of {name} is {}.",
            hostkey::describe(&key)
        ));
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Usage(
                "not asking without a terminal: check the fingerprint, then pass --yes".into(),
            ));
        }
        let ok = confirm(
            "Check the fingerprint with the server's administrator (or `ssh-keygen -lf` on \
             the server). Trust it?",
        )?;
        if !ok {
            return Err(CliError::Usage("not trusted".into()));
        }
    }
    hostkey::trust(&trusted, &host, port, &key)
        .with_context(|| format!("cannot write {}", trusted.display()))?;
    report(true, "added");
    if !ctx.json {
        out(format_args!(
            "Trusted {name}: added to {}",
            trusted.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_target;

    #[test]
    fn targets() {
        let t = |s| parse_target(s);
        assert_eq!(t("Example.org"), Ok(("example.org".into(), 22)));
        assert_eq!(t("h:2222"), Ok(("h".into(), 2222)));
        assert_eq!(t("[::1]:2222"), Ok(("::1".into(), 2222)));
        assert_eq!(t("[::1]"), Ok(("::1".into(), 22)));
        assert_eq!(t("::1"), Ok(("::1".into(), 22)));
        assert_eq!(t("sftp://u@h:2200/x"), Ok(("h".into(), 2200)));
        assert!(t("h:0").is_err());
        assert!(t("h:x").is_err());
        assert!(t("").is_err());
        assert!(t("[::1]x").is_err());
    }
}
