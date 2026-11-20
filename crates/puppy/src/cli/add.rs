//! `puppy add`: queue downloads from URLs, magnet links, `.torrent` files or stdin.

use super::output::{err, json, out};
use super::wait::{collect, finish, interruptible, until_done};
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

/// What `add` did: the ids it added, and its exit code (1 if some sources could not be
/// added; they were reported on stderr).
#[derive(Debug)]
pub struct AddOutcome {
    pub ids: Vec<DownloadId>,
    pub code: u8,
}

/// `add`, then with `wait` waits for the added downloads; the worst code wins.
pub async fn run(ctx: &Ctx, args: AddArgs, wait: bool) -> Result<(), CliError> {
    if wait && args.paused {
        return Err(CliError::Usage(
            "--wait and --paused do not go together: a paused download never ends".into(),
        ));
    }
    let added = add(ctx, args).await?;
    let mut codes = vec![added.code];
    if wait && !added.ids.is_empty() {
        let mut client = ctx.client().await?;
        collect(
            &mut codes,
            interruptible(until_done(ctx, &mut client, &added.ids)).await,
        );
    }
    finish(&codes)
}

/// Adds every source; ones that cannot be added are reported on stderr and make the
/// outcome's code 1, after the others were added.
pub async fn add(ctx: &Ctx, args: AddArgs) -> Result<AddOutcome, CliError> {
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
    let mut code = 0;
    if failures > 0 {
        let total = added.len() + failures;
        err(format_args!(
            "puppy: {failures} of {total} sources could not be added"
        ));
        code = 1;
    }
    Ok(AddOutcome {
        ids: added.into_iter().map(|a| a.id).collect(),
        code,
    })
}
