//! `puppy wait`, `puppy add --wait` and `puppy watch`: follow downloads through the
//! daemon's event stream.

use super::list::attention_text;
use super::output::{err, json, out, stdout_closed};
use super::{CliError, Ctx, report_error, worst};
use puppy_client::Client;
use puppy_core::types::DownloadId;
use puppy_proto::{DownloadSnapshot, Event, ReplyData, Request, WireState};
use puppy_tui::format::{self, display_safe};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::io::{IsTerminal, Write};

/// Snapshot interval while waiting (drives the progress line).
const WAIT_INTERVAL_MS: u64 = 250;
/// Snapshot interval for `watch` (`--json` prints every snapshot).
const WATCH_INTERVAL_MS: u64 = 1000;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Exit code after Ctrl-C (128 + SIGINT, as shells report it).
const INTERRUPTED: u8 = 130;
/// Erases the current terminal line.
const CLEAR_LINE: &str = "\r\x1b[2K";

fn lost() -> CliError {
    CliError::Other(anyhow::anyhow!("lost connection to puppy"))
}

/// Resolves on Ctrl-C; never, if Ctrl-C cannot be listened for.
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Runs `f` until it ends or Ctrl-C (exit 130). The downloads keep going either way.
pub async fn interruptible(f: impl Future<Output = Result<(), CliError>>) -> Result<(), CliError> {
    tokio::select! {
        r = f => r,
        () = ctrl_c() => {
            if std::io::stderr().is_terminal() {
                let _ = write!(std::io::stderr().lock(), "{CLEAR_LINE}");
            }
            err("puppy: interrupted; the downloads keep running");
            Err(CliError::Reported(INTERRUPTED))
        }
    }
}

/// Adds `r`'s exit code to `codes`, printing its error unless it was already reported.
pub fn collect(codes: &mut Vec<u8>, r: Result<(), CliError>) {
    if let Err(e) = r {
        report_error(&e);
        codes.push(e.exit_code());
    }
}

/// The result for `codes`: the worst wins.
pub fn finish(codes: &[u8]) -> Result<(), CliError> {
    match worst(codes.iter().copied()) {
        0 => Ok(()),
        c => Err(CliError::Reported(c)),
    }
}

/// How a download ended, if it did: (exit code, message). A transient failure has not
/// ended: puppy retries it.
fn ended(d: &DownloadSnapshot) -> Option<(u8, String)> {
    match &d.state {
        WireState::Completed => Some((0, display_safe(&d.name).into_owned())),
        WireState::Failed {
            reason,
            transient: false,
        } => Some((4, format!("failed: {reason}"))),
        WireState::NeedsAttention { reason } => {
            Some((5, format!("needs attention: {}", attention_text(reason))))
        }
        _ => None,
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The progress line: `⠋ 2/3 · 45% · 11.4 MiB/s · eta 0:41`, plus `· retrying in m:ss`
/// while a download waits for its automatic retry. `waited` are the latest snapshots of
/// every waited download (finished ones included).
fn progress_line(
    frame: usize,
    finished: usize,
    total: usize,
    waited: &[&DownloadSnapshot],
    now_unix: u64,
) -> String {
    let mut parts = vec![format!(
        "{} {finished}/{total}",
        SPINNER[frame % SPINNER.len()]
    )];
    let size = waited
        .iter()
        .map(|d| d.total_bytes)
        .sum::<Option<u64>>()
        .filter(|_| !waited.is_empty());
    let done: u64 = waited
        .iter()
        .map(|d| match (&d.state, d.total_bytes) {
            (WireState::Completed, Some(t)) => t,
            (_, Some(t)) => d.done_bytes.min(t),
            (_, None) => d.done_bytes,
        })
        .sum();
    let running = |d: &&&DownloadSnapshot| ended(d).is_none();
    let bps: u64 = waited.iter().filter(running).map(|d| d.down_bps).sum();
    if let Some(p) = format::percent(done, size) {
        parts.push(format!("{:.0}%", p * 100.0));
    }
    parts.push(format::speed(bps));
    let eta = size
        .filter(|_| bps > 0)
        .map(|s| s.saturating_sub(done).div_ceil(bps));
    parts.push(format!("eta {}", format::eta(eta)));
    let retry = waited
        .iter()
        .filter(|d| {
            matches!(
                d.state,
                WireState::Failed {
                    transient: true,
                    ..
                }
            )
        })
        .filter_map(|d| d.next_retry_unix)
        .min();
    if let Some(at) = retry {
        parts.push(format!(
            "retrying in {}",
            format::eta(Some(at.saturating_sub(now_unix)))
        ));
    }
    parts.join(" · ")
}

/// The one progress line on stderr, rewritten in place (terminals only).
struct Progress {
    tty: bool,
    shown: bool,
    frame: usize,
}

impl Progress {
    fn show(&mut self, line: &str) {
        if self.tty {
            let _ = write!(std::io::stderr().lock(), "{CLEAR_LINE}{line}");
            self.shown = true;
            self.frame += 1;
        }
    }

    fn clear(&mut self) {
        if self.shown {
            let _ = write!(std::io::stderr().lock(), "{CLEAR_LINE}");
            self.shown = false;
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}

struct Waiter<'a> {
    ctx: &'a Ctx,
    /// Waited ids, in the order given (duplicates removed).
    ids: Vec<DownloadId>,
    pending: BTreeSet<DownloadId>,
    last: HashMap<DownloadId, DownloadSnapshot>,
    codes: Vec<u8>,
    progress: Progress,
}

impl Waiter<'_> {
    /// Marks `id` done with `code` and says so: completions on stdout, the rest on
    /// stderr.
    fn end(&mut self, id: DownloadId, code: u8, message: &str) {
        if !self.pending.remove(&id) {
            return;
        }
        self.codes.push(code);
        self.progress.clear();
        if code == 0 {
            if !self.ctx.json {
                out(format_args!("completed {id} {message}"));
            }
        } else {
            err(format_args!("puppy: {id}: {message}"));
        }
    }

    fn retrying(&mut self, id: DownloadId, reason: &str) {
        self.progress.clear();
        err(format_args!("puppy: {id}: {reason}; puppy will retry it"));
    }

    /// Takes in every download's state; `first` is the snapshot taken when waiting
    /// began (an id missing then never existed).
    fn snapshot(&mut self, downloads: Vec<DownloadSnapshot>, first: bool) {
        let mut by_id: HashMap<DownloadId, DownloadSnapshot> =
            downloads.into_iter().map(|d| (d.id, d)).collect();
        for id in self.pending.clone() {
            match by_id.remove(&id) {
                Some(d) => {
                    if let Some((code, message)) = ended(&d) {
                        self.end(id, code, &message);
                    } else if let (true, WireState::Failed { reason, .. }) = (first, &d.state) {
                        let reason = reason.clone();
                        self.retrying(id, &reason);
                    }
                    self.last.insert(id, d);
                }
                None if first => self.end(id, 3, &format!("no download with id {id}")),
                None => self.end(id, 3, "removed"),
            }
        }
        if !self.pending.is_empty() {
            let waited: Vec<&DownloadSnapshot> =
                self.ids.iter().filter_map(|id| self.last.get(id)).collect();
            let line = progress_line(
                self.progress.frame,
                self.ids.len() - self.pending.len(),
                self.ids.len(),
                &waited,
                now_unix(),
            );
            self.progress.show(&line);
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Snapshot { downloads, .. } => self.snapshot(downloads, false),
            Event::Completed { id, name } => self.end(id, 0, &display_safe(&name)),
            Event::Failed {
                id,
                reason,
                transient: false,
            } => self.end(id, 4, &format!("failed: {reason}")),
            Event::Failed {
                id,
                reason,
                transient: true,
            } if self.pending.contains(&id) => self.retrying(id, &reason),
            Event::NeedsAttention { id, reason } => self.end(
                id,
                5,
                &format!("needs attention: {}", attention_text(&reason)),
            ),
            Event::Removed { id } => self.end(id, 3, "removed"),
            _ => {}
        }
    }
}

/// Waits until every download in `ids` has ended: completed (0), failed for good (4),
/// needs attention (5), or removed / unknown (3); the worst code wins. A transient
/// failure is not an end: puppy retries it. With `--json`, prints the final snapshots
/// (as a JSON array) of the ones that still exist.
pub async fn until_done(
    ctx: &Ctx,
    client: &mut Client,
    ids: &[DownloadId],
) -> Result<(), CliError> {
    let mut unique = Vec::new();
    for id in ids {
        if !unique.contains(id) {
            unique.push(*id);
        }
    }
    client.subscribe(WAIT_INTERVAL_MS).await?;
    let mut w = Waiter {
        ctx,
        pending: unique.iter().copied().collect(),
        ids: unique,
        last: HashMap::new(),
        codes: Vec::new(),
        progress: Progress {
            tty: std::io::stderr().is_terminal(),
            shown: false,
            frame: 0,
        },
    };
    // Subscribed first: whatever happens after this snapshot arrives as an event.
    w.snapshot(client.list().await?, true);
    while !w.pending.is_empty() {
        let Some(ev) = client.next_event().await else {
            w.progress.clear();
            return Err(lost());
        };
        w.event(ev);
    }
    if ctx.json {
        let all = client.list().await?;
        let finals: Vec<&DownloadSnapshot> = w
            .ids
            .iter()
            .filter_map(|id| all.iter().find(|d| d.id == *id))
            .collect();
        json(&finals);
    }
    finish(&w.codes)
}

/// `wait <ID>...`: waits for downloads to end; ids that do not parse fail with code 2.
pub async fn wait(ctx: &Ctx, ids: Vec<String>) -> Result<(), CliError> {
    let mut codes = Vec::new();
    let mut parsed = Vec::new();
    for s in &ids {
        match s.trim().parse::<DownloadId>() {
            Ok(id) => parsed.push(id),
            Err(_) => {
                err(format_args!(
                    "puppy: {s}: \"{s}\" is not a download id (see `puppy ls`)"
                ));
                codes.push(2);
            }
        }
    }
    if !parsed.is_empty() {
        let mut client = ctx.client().await?;
        collect(
            &mut codes,
            interruptible(until_done(ctx, &mut client, &parsed)).await,
        );
    }
    finish(&codes)
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// `watch`: prints events as they happen until Ctrl-C (exit 0) or until stdout is
/// closed. Human lines skip snapshots; `--json` prints every event as NDJSON.
pub async fn watch(ctx: &Ctx) -> Result<(), CliError> {
    let mut client = ctx.client().await?;
    client.subscribe(WATCH_INTERVAL_MS).await?;
    let mut names: HashMap<DownloadId, String> = client
        .list()
        .await?
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    let run = async {
        while !stdout_closed() {
            let Some(ev) = client.next_event().await else {
                return Err(lost());
            };
            if ctx.json {
                json(&ev);
                continue;
            }
            let name = |names: &HashMap<DownloadId, String>, id| {
                display_safe(names.get(&id).map_or("", String::as_str)).into_owned()
            };
            let line = match ev {
                Event::Snapshot { downloads, .. } => {
                    names.extend(downloads.into_iter().map(|d| (d.id, d.name)));
                    continue;
                }
                Event::Added { id } => {
                    if let Ok(ReplyData::Download { download, .. }) =
                        client.call(Request::Get { id }).await
                    {
                        names.insert(id, download.name);
                    }
                    format!("added {id} {}", name(&names, id))
                }
                Event::Completed { id, name: n } => {
                    format!("completed {id} {}", display_safe(&n))
                }
                Event::Failed {
                    id,
                    reason,
                    transient,
                } => {
                    let retry = if transient { " (puppy will retry)" } else { "" };
                    format!("failed {id} {}: {reason}{retry}", name(&names, id))
                }
                Event::NeedsAttention { id, reason } => format!(
                    "needs attention {id} {}: {}",
                    name(&names, id),
                    attention_text(&reason)
                ),
                Event::Removed { id } => {
                    let line = format!("removed {id} {}", name(&names, id));
                    names.remove(&id);
                    line
                }
                Event::Unknown => continue,
            };
            out(format_args!("{} {line}", timestamp()));
        }
        Ok(())
    };
    tokio::select! {
        r = run => r,
        () = ctrl_c() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puppy_proto::EngineKindTag;

    fn snap(state: WireState, total: Option<u64>, done: u64, bps: u64) -> DownloadSnapshot {
        DownloadSnapshot {
            id: 1,
            kind: EngineKindTag::Http,
            source: String::new(),
            name: "x".into(),
            path: None,
            state,
            total_bytes: total,
            done_bytes: done,
            down_bps: bps,
            up_bps: 0,
            eta_secs: None,
            limit: None,
            queue_position: None,
            connections: None,
            peers: None,
            next_retry_unix: None,
        }
    }

    #[test]
    fn progress_line_format() {
        let mib = 1 << 20;
        let a = snap(WireState::Completed, Some(10 * mib), 10 * mib, 0);
        let b = snap(WireState::Active, Some(10 * mib), 0, mib);
        assert_eq!(
            progress_line(0, 1, 2, &[&a, &b], 0),
            "⠋ 1/2 · 50% · 1.0 MiB/s · eta 0:10"
        );
        let mut c = snap(
            WireState::Failed {
                reason: "503".into(),
                transient: true,
            },
            None,
            0,
            0,
        );
        c.next_retry_unix = Some(100 + 65);
        assert_eq!(
            progress_line(1, 0, 1, &[&c], 100),
            "⠙ 0/1 · — · eta — · retrying in 1:05"
        );
    }

    #[test]
    fn transient_failure_has_not_ended() {
        let d = snap(
            WireState::Failed {
                reason: "x".into(),
                transient: true,
            },
            None,
            0,
            0,
        );
        assert!(ended(&d).is_none());
        let d = snap(
            WireState::Failed {
                reason: "x".into(),
                transient: false,
            },
            None,
            0,
            0,
        );
        assert_eq!(ended(&d).unwrap().0, 4);
    }
}
