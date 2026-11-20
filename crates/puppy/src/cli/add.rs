//! `puppy add`: queue downloads from URLs, magnet links, `.torrent` files or stdin.

use super::output::{err, json, out};
use super::{CliError, Ctx};
use puppy_core::types::{AddRequest, DownloadId, RequestContext};
use puppy_tui::app::forms::{expand_tilde, parse_source, unquote};
use puppy_tui::format::parse_rate;
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;

/// What `puppy add` was asked to do.
#[derive(Debug, Default)]
pub struct AddArgs {
    /// URLs, magnet links, `.torrent` paths; `-` reads them from stdin, one per line.
    pub sources: Vec<String>,
    pub output_dir: Option<PathBuf>,
    pub name: Option<String>,
    pub paused: bool,
    pub limit: Option<String>,
    /// `Name: value`.
    pub headers: Vec<String>,
    pub cookie: Option<String>,
    pub referer: Option<String>,
}

#[derive(Serialize)]
struct Added {
    id: DownloadId,
    source: String,
}

/// Splits `Name: value` at the first `:`.
fn parse_header(h: &str) -> Result<(String, String), CliError> {
    match h.split_once(':') {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.trim().to_string())),
        _ => Err(CliError::Usage(format!(
            "bad --header {h:?}: expected \"Name: value\""
        ))),
    }
}

/// The sources: stdin lines for `-` (blank lines and `#` comments skipped), as given
/// otherwise.
fn expand_sources(sources: Vec<String>) -> Result<Vec<String>, CliError> {
    let mut all = Vec::new();
    for s in sources {
        if s != "-" {
            all.push(s);
            continue;
        }
        let mut text = Vec::new();
        std::io::stdin()
            .read_to_end(&mut text)
            .map_err(|e| anyhow::anyhow!("cannot read stdin: {e}"))?;
        for line in String::from_utf8_lossy(&text).lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                all.push(line.to_string());
            }
        }
    }
    Ok(all)
}

/// Adds every source; ones that cannot be added are reported on stderr and make the
/// command fail (exit 1) after the others were added.
pub async fn add(ctx: &Ctx, args: AddArgs) -> Result<Vec<DownloadId>, CliError> {
    let limit = match args.limit.as_deref() {
        Some(l) => parse_rate(l).map_err(|e| CliError::Usage(format!("bad --limit: {e}")))?,
        None => None,
    };
    let headers = args
        .headers
        .iter()
        .map(|h| parse_header(h))
        .collect::<Result<Vec<_>, _>>()?;
    let output_dir = args
        .output_dir
        .map(|p| {
            let p = expand_tilde(&p.to_string_lossy());
            std::path::absolute(&p).unwrap_or(p)
        })
        .map(|p| p.to_path_buf());
    if args.name.is_some() && args.sources.len() != 1 {
        return Err(CliError::Usage(
            "--name needs exactly one source".to_string(),
        ));
    }
    let sources = expand_sources(args.sources)?;
    if args.name.is_some() && sources.len() != 1 {
        return Err(CliError::Usage(
            "--name needs exactly one source".to_string(),
        ));
    }
    let context = RequestContext {
        headers,
        cookies: args.cookie,
        referer: args.referer,
        ..RequestContext::default()
    };

    // Parse everything first: usage-level problems should not leave a half-done add.
    let mut failures = 0usize;
    let mut parsed = Vec::new();
    for s in &sources {
        match parse_source(s) {
            Ok(source) => parsed.push((unquote(s).to_string(), source)),
            Err(e) => {
                err(format_args!("puppy: {s}: {e}"));
                failures += 1;
            }
        }
    }

    let mut added = Vec::new();
    if !parsed.is_empty() {
        let client = ctx.client().await?;
        for (shown, source) in parsed {
            let req = AddRequest {
                source,
                output_dir: output_dir.clone(),
                filename: args.name.clone(),
                context: context.clone(),
            };
            match client.add(req, args.paused, limit).await {
                Ok(id) => {
                    if !ctx.json {
                        out(format_args!(
                            "added {id} {}",
                            args.name.as_deref().unwrap_or(&shown)
                        ));
                    }
                    added.push(Added { id, source: shown });
                }
                Err(e) => {
                    err(format_args!("puppy: {shown}: {e}"));
                    failures += 1;
                }
            }
        }
    }
    if ctx.json {
        json(&added);
    }
    if failures > 0 {
        let total = added.len() + failures;
        return Err(CliError::Other(anyhow::anyhow!(
            "{failures} of {total} sources could not be added"
        )));
    }
    Ok(added.into_iter().map(|a| a.id).collect())
}
