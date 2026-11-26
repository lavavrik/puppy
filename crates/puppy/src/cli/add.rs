//! `puppy add`: queue downloads from URLs, magnet links, `.torrent` files or stdin.

use super::files::parse_spec;
use super::input_file::{self, EntryOptions, LineError, split_header};
use super::output::{err, json, out};
use super::seed;
use super::wait::{Waited, absorb, finish, interruptible, until_done};
use super::{CliError, Ctx};
use puppy_core::checksum::Checksum;
use puppy_core::types::{
    AddRequest, DownloadId, FileChoice, Mirror, RequestContext, SeedPolicy, Source, masked_url,
};
use puppy_proto::{ReplyData, Request};
use puppy_tui::app::forms::{expand_tilde, parse_source, unquote};
use puppy_tui::format::parse_rate;
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    /// `--checksum ALGO=HEX` (one HTTP(S) or FTP(S) source only).
    pub checksum: Option<String>,
    /// `--files` specs (torrents only): numbers, ranges and globs.
    pub files: Vec<String>,
    /// `--choose`: wait for `puppy select` (torrents only).
    pub choose: bool,
    /// `--seed-ratio` (torrents only).
    pub seed_ratio: Option<String>,
    /// `--seed-time` (torrents only).
    pub seed_time: Option<String>,
    /// `-i FILE`: an aria2-style input file (`-`: stdin), added after `sources`.
    pub input_file: Option<PathBuf>,
    /// `--mirror URL` (repeatable): other locations of the one URL source's file.
    pub mirrors: Vec<String>,
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

/// Decodes text from stdin or an input file (`what` names it in errors): strips a UTF-8
/// BOM, decodes UTF-16 (LE or BE) after its BOM, else UTF-8 (invalid bytes replaced).
fn decode_text(bytes: &[u8], what: &str) -> Result<String, CliError> {
    let utf16 = |rest: &[u8], le: bool| -> Result<String, CliError> {
        let bad = || CliError::Usage(format!("{what} is not valid UTF-16"));
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

fn read_stdin() -> Result<String, CliError> {
    let mut text = Vec::new();
    std::io::stdin()
        .read_to_end(&mut text)
        .map_err(|e| anyhow::anyhow!("cannot read stdin: {e}"))?;
    decode_text(&text, "stdin")
}

/// Splits `Name: value` at the first `:`.
fn parse_header(h: &str) -> Result<(String, String), CliError> {
    split_header(h)
        .ok_or_else(|| CliError::Usage(format!("bad --header {h:?}: expected \"Name: value\"")))
}

/// The sources: stdin lines for `-` (blank lines and `#` comments skipped), as given
/// otherwise. `-` reading nothing is an error unless `more` downloads come from
/// elsewhere (an input file).
fn expand_sources(sources: Vec<String>, more: bool) -> Result<Vec<String>, CliError> {
    let mut all = Vec::new();
    let mut stdin = false;
    for s in sources {
        if s != "-" {
            all.push(s);
            continue;
        }
        stdin = true;
        for line in read_stdin()?.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                all.push(line.to_string());
            }
        }
    }
    if stdin && all.is_empty() && !more {
        return Err(CliError::Usage("no sources on stdin".into()));
    }
    Ok(all)
}

/// The command-line flags every source starts from; an input file's options override
/// them per entry.
struct Defaults {
    output_dir: Option<PathBuf>,
    name: Option<String>,
    paused: bool,
    limit: Option<u64>,
    context: RequestContext,
    choice: FileChoice,
    seed: Option<SeedPolicy>,
    checksum: Option<Checksum>,
    /// `--mirror`s (for the one source there is).
    mirrors: Vec<Mirror>,
}

/// One download to add, fully decided.
struct Planned {
    /// The source as the user wrote it (`added[].source`).
    shown: String,
    req: AddRequest,
    paused: bool,
    limit: Option<u64>,
}

impl Defaults {
    /// `source` with these defaults, overridden by `o` (an input-file entry's options;
    /// empty for a command-line source). An entry's `header` replaces a `-H` of the same
    /// name and adds the rest; a seeding option replaces only its half of the policy.
    /// `mirrors` are the entry's own (TAB-separated URIs); `--mirror`s come after them.
    fn plan(
        &self,
        shown: String,
        source: Source,
        mirrors: Vec<Mirror>,
        o: &EntryOptions,
    ) -> Planned {
        let mut context = self.context.clone();
        context.headers.retain(|(n, _)| {
            !o.headers
                .iter()
                .any(|(m, _)| m.trim().eq_ignore_ascii_case(n.trim()))
        });
        context.headers.extend(o.headers.iter().cloned());
        if let Some(r) = &o.referer {
            context.referer = Some(r.clone());
        }
        if let Some(u) = &o.user_agent {
            context.user_agent = Some(u.clone());
        }
        let seed = if o.seed_ratio.is_some() || o.seed_time.is_some() {
            let base = self.seed.clone().unwrap_or_default();
            Some(SeedPolicy {
                ratio: o.seed_ratio.unwrap_or(base.ratio),
                time_secs: o.seed_time.unwrap_or(base.time_secs),
            })
        } else {
            self.seed.clone()
        };
        let files = match &o.select {
            Some(selection) => FileChoice::Select {
                selection: selection.clone(),
            },
            None => self.choice.clone(),
        };
        // A password in the URL is never echoed back (`added 3 ftp://bob:***@host/f`).
        let shown = match &source {
            Source::Http { url } if url.password().is_some() => masked_url(url),
            _ => shown,
        };
        Planned {
            shown,
            req: AddRequest {
                source,
                output_dir: o.dir.clone().or_else(|| self.output_dir.clone()),
                filename: o.out.clone().or_else(|| self.name.clone()),
                context,
                files,
                seed,
                checksum: o.checksum.clone().or_else(|| self.checksum.clone()),
                mirrors: mirrors
                    .into_iter()
                    .chain(self.mirrors.iter().cloned())
                    .collect(),
            },
            paused: o.pause.unwrap_or(self.paused),
            limit: o.limit.unwrap_or(self.limit),
        }
    }
}

/// Reads `-i FILE` (`-`: stdin) into planned adds. Unsupported options are warned about
/// on stderr. Any invalid entry fails the whole file, every problem reported with its
/// line, so that nothing is added: a script then fixes the file and runs it again,
/// instead of having to work out which half of the list is already queued.
fn plan_input_file(
    path: &Path,
    defaults: &Defaults,
    wait: bool,
) -> Result<(String, Vec<Planned>), CliError> {
    let (label, text) = if path == Path::new("-") {
        ("stdin".to_string(), read_stdin()?)
    } else {
        let label = path.display().to_string();
        let bytes = std::fs::read(path)
            .map_err(|e| CliError::Usage(format!("cannot read input file {label}: {e}")))?;
        let text = decode_text(&bytes, &label)?;
        (label, text)
    };
    let (entries, mut errors) = input_file::parse(&text);
    let mut planned = Vec::new();
    for entry in &entries {
        let got = input_file::interpret(entry);
        for w in got.warnings {
            err(format_args!("puppy: {label}: {w}"));
        }
        let options = got.errors.is_empty().then_some(got.options);
        errors.extend(got.errors);
        let mut bad = |message: String| {
            errors.push(LineError {
                line: entry.line,
                message,
            })
        };
        let uri = &entry.uris[0];
        let source = match parse_source(uri) {
            Ok(s) => s,
            Err(e) => {
                bad(format!("{uri}: {e}"));
                continue;
            }
        };
        let mut mirrors = Vec::new();
        for m in &entry.uris[1..] {
            match parse_mirror(m) {
                Ok(m) => mirrors.push(m),
                Err(e) => bad(e),
            }
        }
        if mirrors.len() + 1 < entry.uris.len() {
            continue;
        }
        if !mirrors.is_empty() && !matches!(source, Source::Http { .. }) {
            bad(format!(
                "TAB-separated mirrors only apply to HTTP(S) and FTP(S) downloads, but {uri} is \
                 a torrent"
            ));
            continue;
        }
        let Some(o) = options else { continue };
        if matches!(source, Source::Http { .. }) {
            if let Some(key) = o.torrent_only() {
                bad(format!(
                    "{key} only applies to torrents, but {uri} is not one"
                ));
                continue;
            }
        } else if o.checksum.is_some() {
            bad(format!(
                "checksum only applies to HTTP(S) and FTP(S) downloads, but {uri} is a torrent \
                 (BitTorrent verifies every piece already)"
            ));
            continue;
        }
        if wait && o.pause == Some(true) {
            bad("pause=true does not go with --wait: a paused download never ends".into());
            continue;
        }
        planned.push(defaults.plan(unquote(uri).to_string(), source, mirrors, &o));
    }
    errors.sort_by_key(|e| e.line);
    match errors.as_slice() {
        [] => Ok((label, planned)),
        [one] => Err(CliError::Usage(format!(
            "{label}: {one} (nothing was added)"
        ))),
        many => {
            for e in many {
                err(format_args!("puppy: {label}: {e}"));
            }
            Err(CliError::Usage(format!(
                "{label}: {} problems, nothing was added",
                many.len()
            )))
        }
    }
}

/// A mirror URL (`--mirror`, a TAB-separated URI): absolute HTTP(S) or FTP(S), whatever
/// the source's protocol.
fn parse_mirror(s: &str) -> Result<Mirror, String> {
    let s = unquote(s.trim());
    match url::Url::parse(s) {
        Ok(u) if matches!(parse_source(s), Ok(Source::Http { .. })) => Ok(Mirror::new(u)),
        Ok(u) => Err(format!(
            "mirror {s}: only HTTP(S) and FTP(S) mirrors are supported, not {}:",
            u.scheme()
        )),
        Err(e) => Err(format!("mirror {s}: not a URL ({e})")),
    }
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
    let outcome = add(ctx, args, wait).await?;
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

/// Adds every source (command-line ones first, then the input file's); ones that cannot
/// be added are reported on stderr and make the outcome's code 1, after the others were
/// added. `wait`: the caller waits afterwards (an entry's `pause=true` is refused).
pub async fn add(ctx: &Ctx, args: AddArgs, wait: bool) -> Result<AddOutcome, CliError> {
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
    let file = args.input_file;
    if args.name.is_some() && file.is_none() && args.sources.len() != 1 {
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
    let checksum = match args.checksum.as_deref() {
        Some(c) => Some(
            c.parse::<Checksum>()
                .map_err(|e| CliError::Usage(format!("bad --checksum {c:?}: {e}")))?,
        ),
        None => None,
    };
    if checksum.is_some() && file.is_none() && args.sources.len() != 1 {
        return Err(CliError::Usage(
            "--checksum needs exactly one source: one digest cannot match several files".into(),
        ));
    }
    let mirrors = args
        .mirrors
        .iter()
        .map(|m| parse_mirror(m).map_err(|e| CliError::Usage(format!("bad --mirror: {e}"))))
        .collect::<Result<Vec<_>, _>>()?;
    if !mirrors.is_empty() && file.is_none() && args.sources.len() != 1 {
        return Err(CliError::Usage(
            "--mirror needs exactly one source: mirrors are other places of one file".into(),
        ));
    }
    if file.as_deref() == Some(Path::new("-")) && args.sources.iter().any(|s| s == "-") {
        return Err(CliError::Usage(
            "stdin can only be read once: give `-` as a source or `-i -`, not both".into(),
        ));
    }
    let defaults = Defaults {
        output_dir,
        name: args.name,
        paused: args.paused,
        limit,
        context: RequestContext {
            headers,
            cookies: args.cookie,
            referer: args.referer,
            ..RequestContext::default()
        },
        choice,
        seed,
        checksum,
        mirrors,
    };

    // Plan everything first: usage-level problems should not leave a half-done add.
    let from_file = match &file {
        Some(path) => Some(plan_input_file(path, &defaults, wait)?),
        None => None,
    };
    let file_count = from_file.as_ref().map_or(0, |(_, p)| p.len());
    let sources = expand_sources(args.sources, file_count > 0)?;
    if let Some((label, _)) = &from_file
        && sources.is_empty()
        && file_count == 0
    {
        return Err(CliError::Usage(format!("no downloads in {label}")));
    }
    let total = sources.len() + file_count;
    if defaults.name.is_some() && total != 1 {
        return Err(CliError::Usage(
            "--name needs exactly one source".to_string(),
        ));
    }
    if defaults.checksum.is_some() && total != 1 {
        return Err(CliError::Usage(
            "--checksum needs exactly one source: one digest cannot match several files".into(),
        ));
    }
    if !defaults.mirrors.is_empty() && total != 1 {
        return Err(CliError::Usage(
            "--mirror needs exactly one source: mirrors are other places of one file".into(),
        ));
    }

    let mut failed = Vec::new();
    let mut planned = Vec::new();
    let none = EntryOptions::default();
    for s in &sources {
        match parse_source(s) {
            Ok(source) => {
                planned.push(defaults.plan(unquote(s).to_string(), source, Vec::new(), &none))
            }
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
    planned.extend(from_file.into_iter().flat_map(|(_, p)| p));

    if defaults.choice != FileChoice::All || defaults.seed.is_some() {
        let flags = if args.choose {
            "--choose"
        } else if !args.files.is_empty() {
            "--files"
        } else {
            "--seed-ratio/--seed-time"
        };
        if let Some(p) = planned
            .iter()
            .find(|p| matches!(p.req.source, Source::Http { .. }))
        {
            return Err(CliError::Usage(format!(
                "{flags} only applies to torrents, but {} is not one",
                p.shown
            )));
        }
    }

    if defaults.checksum.is_some()
        && let Some(p) = planned
            .iter()
            .find(|p| !matches!(p.req.source, Source::Http { .. }))
    {
        return Err(CliError::Usage(format!(
            "--checksum only applies to HTTP(S) and FTP(S) downloads, but {} is a torrent \
             (BitTorrent verifies every piece already)",
            p.shown
        )));
    }

    if !defaults.mirrors.is_empty()
        && let Some(p) = planned
            .iter()
            .find(|p| !matches!(p.req.source, Source::Http { .. }))
    {
        return Err(CliError::Usage(format!(
            "--mirror only applies to HTTP(S) and FTP(S) downloads, but {} is a torrent",
            p.shown
        )));
    }
    for p in &planned {
        if !p.req.mirrors.is_empty() && p.req.checksum.is_none() {
            err(format_args!(
                "puppy: {}: warning: mirrors without a checksum: a mirror that serves \
                 different bytes of the same size would go unnoticed (add --checksum)",
                p.shown
            ));
        }
    }

    let mut added = Vec::new();
    if !planned.is_empty() {
        let client = ctx.client().await?;
        for Planned {
            shown,
            req,
            paused,
            limit,
        } in planned
        {
            let verify = req.checksum.is_some();
            let mirrored = !req.mirrors.is_empty();
            let name = req.filename.clone();
            let result = match client.add(req, paused, limit).await {
                Ok(id) if verify || mirrored => {
                    check_kept(&client, id, &shown, (verify, mirrored)).await
                }
                r => r.map_err(Into::into),
            };
            match result {
                Ok(id) => {
                    if !ctx.json {
                        out(format_args!(
                            "added {id} {}",
                            name.as_deref().unwrap_or(&shown)
                        ));
                    }
                    added.push(Added { id, source: shown });
                }
                Err(e) => {
                    err(format_args!("puppy: {shown}: {e}"));
                    // The daemon refused the request itself (e.g. `--files 9` past the
                    // last file of a torrent): a usage problem, not a failed attempt.
                    let code = match e {
                        AddError::Client(super::ClientError::Daemon {
                            code: puppy_proto::ErrorCode::BadRequest,
                            ..
                        }) => 2,
                        _ => 1,
                    };
                    failed.push(Failed {
                        source: shown,
                        error: e.to_string(),
                        code,
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
        code = failed.iter().map(|f| f.code).max().unwrap_or(1);
    }
    Ok(AddOutcome {
        added,
        failed,
        code,
    })
}

/// Why one source was not added.
#[derive(Debug, thiserror::Error)]
enum AddError {
    #[error(transparent)]
    Client(#[from] super::ClientError),
    #[error(
        "the running puppy daemon is too old to verify checksums (download removed); \
         run `puppy stop` and try again"
    )]
    NoChecksumSupport,
}

/// A daemon older than checksums or mirrors ignores those fields. Without its checksum
/// the download would never be verified, so it is removed again. Without its mirrors it
/// still downloads the right file, from the primary only: that is only warned about.
async fn check_kept(
    client: &puppy_client::Client,
    id: DownloadId,
    shown: &str,
    (want_checksum, want_mirrors): (bool, bool),
) -> Result<DownloadId, AddError> {
    let (checksum, mirrors) = match client.call(Request::Get { id }).await? {
        ReplyData::Download { download, .. } => {
            (download.checksum.is_some(), !download.mirrors.is_empty())
        }
        _ => (false, false),
    };
    if want_mirrors && !mirrors {
        err(format_args!(
            "puppy: {shown}: warning: the running puppy daemon is too old for mirrors, \
             so it downloads from the first URL only; run `puppy stop` to restart it"
        ));
    }
    if checksum || !want_checksum {
        return Ok(id);
    }
    if let Err(e) = client
        .call(Request::Remove {
            id,
            delete_files: true,
        })
        .await
    {
        err(format_args!("puppy: removing download {id}: {e}"));
    }
    Err(AddError::NoChecksumSupport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_encodings() {
        assert_eq!(decode_text(b"a\nb", "stdin").unwrap(), "a\nb");
        assert_eq!(decode_text(b"\xEF\xBB\xBFa", "stdin").unwrap(), "a");
        assert_eq!(
            decode_text(&[0xFF, 0xFE, b'a', 0, b'\n', 0], "stdin").unwrap(),
            "a\n"
        );
        assert_eq!(decode_text(&[0xFE, 0xFF, 0, b'a'], "stdin").unwrap(), "a");
        assert!(decode_text(&[0xFF, 0xFE, 0x41], "stdin").is_err());
        assert!(decode_text(&[0xFF, 0xFE, 0x00, 0xD8], "stdin").is_err());
        let Err(CliError::Usage(m)) = decode_text(&[0xFE, 0xFF, 0x41], "list.txt") else {
            panic!("odd UTF-16 must be refused");
        };
        assert_eq!(m, "list.txt is not valid UTF-16");
    }
}
