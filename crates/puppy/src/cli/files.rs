//! Torrent file selection: `puppy files`, `puppy select` and the `SPEC` syntax shared
//! with `puppy add --files`.

use super::control::parse_id_arg;
use super::output::{ColWidth, json, out, table};
use super::{CliError, ClientError, Ctx};
use puppy_core::types::{DownloadId, Selection, TorrentFile};
use puppy_proto::{ErrorCode, ReplyData, Request};
use puppy_tui::format::bytes;
use serde::Serialize;

/// The most files one range may name (a typo like `1-99999999999` must not allocate).
const MAX_RANGE: usize = 100_000;

/// Parses file specs into a [`Selection`]. Specs are comma- or space-separated tokens:
/// a number (`3`) or a range (`2-5`) names files by their 1-based number as `puppy files`
/// shows them; anything else is a glob matched against the file paths (`*.mkv`).
/// Numbers go to the wire 0-based.
pub fn parse_spec(specs: &[String]) -> Result<Selection, CliError> {
    let mut sel = Selection::default();
    for token in specs
        .iter()
        .flat_map(|s| s.split(|c: char| c == ',' || c.is_whitespace()))
        .filter(|t| !t.is_empty())
    {
        let number = |s: &str| -> Result<usize, CliError> {
            let n: usize = s
                .parse()
                .map_err(|_| CliError::Usage(format!("bad file number {s:?} in {token:?}")))?;
            if n == 0 {
                return Err(CliError::Usage(format!(
                    "file numbers start at 1 (in {token:?})"
                )));
            }
            Ok(n)
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        match token.split_once('-') {
            None if digits(token) => sel.indices.push(number(token)? - 1),
            Some((a, b)) if digits(a) && digits(b) => {
                let (a, b) = (number(a)?, number(b)?);
                if b < a {
                    return Err(CliError::Usage(format!(
                        "empty range {token:?}: {b} is less than {a}"
                    )));
                }
                if b - a >= MAX_RANGE {
                    return Err(CliError::Usage(format!("range {token:?} is too large")));
                }
                sel.indices.extend(a - 1..b);
            }
            _ => sel.globs.push(token.to_string()),
        }
    }
    if sel.indices.is_empty() && sel.globs.is_empty() {
        return Err(CliError::Usage(
            "no files named: give file numbers (\"1,3-5\") or globs (\"*.mkv\")".into(),
        ));
    }
    sel.indices.sort_unstable();
    sel.indices.dedup();
    sel.globs.dedup();
    Ok(sel)
}

/// One daemon call about download `id`; an unknown id is exit 3.
async fn call(ctx: &Ctx, id: DownloadId, req: Request) -> Result<ReplyData, CliError> {
    match ctx.client().await?.call(req).await {
        Err(ClientError::Daemon {
            code: ErrorCode::NotFound,
            ..
        }) => Err(CliError::UnknownId(format!("no download with id {id}"))),
        r => Ok(r?),
    }
}

async fn list_files(ctx: &Ctx, id: DownloadId) -> Result<Vec<TorrentFile>, CliError> {
    match call(ctx, id, Request::ListFiles { id }).await? {
        ReplyData::Files { files } => Ok(files),
        other => Err(CliError::Other(anyhow::anyhow!(
            "unexpected reply from the daemon: {other:?}"
        ))),
    }
}

/// `--json` for `files`: a file with its 0-based wire `index` and 1-based `number`.
#[derive(Serialize)]
struct FileJson<'a> {
    index: usize,
    number: usize,
    path: &'a str,
    size: u64,
}

/// `puppy files <id>`: the torrent's files, numbered from 1.
pub async fn files(ctx: &Ctx, id: &str) -> Result<(), CliError> {
    let id = parse_id_arg(id)?;
    let files = list_files(ctx, id).await?;
    if ctx.json {
        let v: Vec<FileJson> = files
            .iter()
            .map(|f| FileJson {
                index: f.index,
                number: f.index + 1,
                path: &f.path,
                size: f.size,
            })
            .collect();
        json(&v);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = files
        .iter()
        .map(|f| vec![(f.index + 1).to_string(), bytes(f.size), f.path.clone()])
        .collect();
    table(
        &["#", "SIZE", "PATH"],
        &rows,
        &[ColWidth::Fixed(5), ColWidth::Fixed(10), ColWidth::Flex],
    );
    Ok(())
}

#[derive(Serialize)]
struct Selected<'a> {
    id: DownloadId,
    selection: &'a Selection,
}

/// `puppy select <id> SPEC...`: chooses the files of a torrent that is waiting for
/// the choice. Numbers past the last file are a usage error (checked against the
/// torrent's file list first).
pub async fn select(ctx: &Ctx, id: &str, specs: &[String]) -> Result<(), CliError> {
    let id = parse_id_arg(id)?;
    let selection = parse_spec(specs)?;
    let count = list_files(ctx, id).await?.len();
    if let Some(n) = selection.indices.iter().find(|i| **i >= count) {
        return Err(CliError::Usage(format!(
            "file {} is out of range: the torrent has {count} file{}",
            n + 1,
            if count == 1 { "" } else { "s" }
        )));
    }
    let req = Request::SelectFiles {
        id,
        selection: selection.clone(),
    };
    call(ctx, id, req).await?;
    if ctx.json {
        json(&Selected {
            id,
            selection: &selection,
        });
    } else {
        out(format_args!("selected files of {id}; downloading"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &[&str]) -> Result<Selection, CliError> {
        parse_spec(&s.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn numbers_ranges_and_globs() {
        let s = spec(&["1,3-5", "*.mkv sub/*"]).unwrap();
        assert_eq!(s.indices, vec![0, 2, 3, 4]);
        assert_eq!(s.globs, vec!["*.mkv", "sub/*"]);
        let s = spec(&["2", "2", "1-2"]).unwrap();
        assert_eq!(s.indices, vec![0, 1]);
        // A name with a dash is a glob, not a range.
        assert_eq!(spec(&["a-b.txt"]).unwrap().globs, vec!["a-b.txt"]);
        assert_eq!(spec(&["1-"]).unwrap().globs, vec!["1-"]);
    }

    #[test]
    fn bad_specs_are_usage_errors() {
        for bad in [&["0"][..], &["0-3"], &["5-2"], &["1-99999999"], &[""], &[]] {
            assert!(matches!(spec(bad), Err(CliError::Usage(_))), "{bad:?}");
        }
    }
}
