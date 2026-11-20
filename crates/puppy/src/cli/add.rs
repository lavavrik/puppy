//! `puppy add`: queue downloads from URLs, magnet links, `.torrent` files or stdin.

use super::files::parse_spec;
use super::output::{err, json, out};
use super::seed;
use super::wait::{Waited, absorb, finish, interruptible, until_done};
use super::{CliError, Ctx};
use puppy_core::types::{AddRequest, DownloadId, FileChoice, RequestContext, Source};
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
    /// `--files` specs (torrents only): numbers, ranges and globs.
    pub files: Vec<String>,
    /// `--choose`: wait for `puppy select` (torrents only).
    pub choose: bool,
    /// `--seed-ratio` (torrents only).
    pub seed_ratio: Option<String>,
    /// `--seed-time` (torrents only).
    pub seed_time: Option<String>,
}

#[derive(Serialize)]
struct Added {
    id: DownloadId,
    source: String,
}

/// A source that could not be added (`code` is the exit code it contributes).
#[derive(Serialize)]
struct Failed {
    source: String,
    error: String,
    code: u8,
}

/// `puppy --json add`: the one JSON value, with `final`/`missing` after `--wait`.
#[derive(Serialize)]
struct AddJson<'a> {
    added: &'a [Added],
    failed: &'a [Failed],
    #[serde(flatten)]
    waited: Option<&'a Waited>,
}

/// Decodes stdin text: strips a UTF-8 BOM, decodes UTF-16 (LE or BE) after its BOM,
/// else UTF-8 (invalid bytes replaced).
fn decode_stdin(bytes: &[u8]) -> Result<String, CliError> {
    let utf16 = |rest: &[u8], le: bool| -> Result<String, CliError> {
        let bad = || CliError::Usage("stdin is not valid UTF-16".into());
        if !rest.len().is_multiple_of(2) {
            return Err(bad());
        }
        let units = rest.as_chunks::<2>().0.iter().map(|c| {
            if le {
                u16::from_le_bytes(*c)
            } else {
                u16::from_be_bytes(*c)
            }
        });
        char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .map_err(|_| bad())
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => Ok(String::from_utf8_lossy(rest).into_owned()),
        _ => Ok(String::from_utf8_lossy(bytes).into_owned()),
    }
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
        for line in decode_stdin(&text)?.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                all.push(line.to_string());
            }
        }
    }
    if all.is_empty() {
        return Err(CliError::Usage("no sources on stdin".into()));
    }
    Ok(all)
}

/// What `add` did, and its exit code (1 if some sources could not be added; they were
/// reported on stderr).
pub struct AddOutcome {
    added: Vec<Added>,
    failed: Vec<Failed>,
    pub code: u8,
}

/// `add`, then with `wait` waits for the added downloads; the worst code wins. With
/// `--json` prints one value: `{"added":…,"failed":…}` plus `"final"`/`"missing"` after
/// a wait.
pub async fn run(ctx: &Ctx, args: AddArgs, wait: bool) -> Result<(), CliError> {
    if wait && args.paused {
        return Err(CliError::Usage(
            "--wait and --paused do not go together: a paused download never ends".into(),
        ));
    }
    let outcome = add(ctx, args).await?;
    let mut codes = vec![outcome.code];
    let mut waited = None;
    if wait {
        waited = Some(Waited::default());
        if !outcome.added.is_empty() {
            let ids: Vec<DownloadId> = outcome.added.iter().map(|a| a.id).collect();
            let r = match ctx.client().await {
                Ok(mut client) => interruptible(until_done(ctx, &mut client, &ids)).await,
                Err(e) => Err(e),
            };
            waited = absorb(&mut codes, r).or(waited);
        }
    }
    if ctx.json {
        json(&AddJson {
            added: &outcome.added,
            failed: &outcome.failed,
            waited: waited.as_ref(),
        });
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
    if args.choose && !args.files.is_empty() {
        return Err(CliError::Usage(
            "--choose and --files do not go together: choose now, or choose later".into(),
        ));
    }
    let choice = if args.choose {
        FileChoice::Ask
    } else if args.files.is_empty() {
        FileChoice::All
    } else {
        FileChoice::Select {
            selection: parse_spec(&args.files)?,
        }
    };
    let seed = seed::policy(args.seed_ratio.as_deref(), args.seed_time.as_deref())?;
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
    let mut failed = Vec::new();
    let mut parsed = Vec::new();
    for s in &sources {
        match parse_source(s) {
            Ok(source) => parsed.push((unquote(s).to_string(), source)),
            Err(e) => {
                err(format_args!("puppy: {s}: {e}"));
                failed.push(Failed {
                    source: s.clone(),
                    error: e.to_string(),
                    code: 1,
                });
            }
        }
    }

    if choice != FileChoice::All || seed.is_some() {
        let flags = if args.choose {
            "--choose"
        } else if !args.files.is_empty() {
            "--files"
        } else {
            "--seed-ratio/--seed-time"
        };
        if let Some((shown, _)) = parsed
            .iter()
            .find(|(_, s)| matches!(s, Source::Http { .. }))
        {
            return Err(CliError::Usage(format!(
                "{flags} only applies to torrents, but {shown} is not one"
            )));
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
                files: choice.clone(),
                seed: seed.clone(),
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
                    failed.push(Failed {
                        source: shown,
                        error: e.to_string(),
                        code: 1,
                    });
                }
            }
        }
    }
    let mut code = 0;
    if !failed.is_empty() {
        let total = added.len() + failed.len();
        err(format_args!(
            "puppy: {} of {total} sources could not be added",
            failed.len()
        ));
        code = 1;
    }
    Ok(AddOutcome {
        added,
        failed,
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_encodings() {
        assert_eq!(decode_stdin(b"a\nb").unwrap(), "a\nb");
        assert_eq!(decode_stdin(b"\xEF\xBB\xBFa").unwrap(), "a");
        assert_eq!(
            decode_stdin(&[0xFF, 0xFE, b'a', 0, b'\n', 0]).unwrap(),
            "a\n"
        );
        assert_eq!(decode_stdin(&[0xFE, 0xFF, 0, b'a']).unwrap(), "a");
        assert!(decode_stdin(&[0xFF, 0xFE, 0x41]).is_err());
        assert!(decode_stdin(&[0xFF, 0xFE, 0x00, 0xD8]).is_err());
    }
}
