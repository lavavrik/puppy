//! Control commands: `pause`, `resume`, `restart`, `rm`, `move`, `limit`,
//! `max-active` and `stop`.

use super::output::{err, json, out};
use super::{CliError, ClientError, Ctx, worst};
use puppy_client::Client;
use puppy_core::types::DownloadId;
use puppy_proto::{ErrorCode, ReplyData, Request, Settings, WireState};
use puppy_tui::format::{bytes, parse_rate};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// How long `stop` waits for the daemon to go away.
const STOP_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Pause,
    Resume,
    Restart,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Pause => "pause",
            Action::Resume => "resume",
            Action::Restart => "restart",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Action::Pause => "paused",
            Action::Resume => "resumed",
            Action::Restart => "restarted",
        }
    }

    fn request(self, id: DownloadId) -> Request {
        match self {
            Action::Pause => Request::Pause { id },
            Action::Resume => Request::Resume { id },
            Action::Restart => Request::Restart { id },
        }
    }

    /// Whether `all` acts on a download in this state.
    fn applies(self, s: &WireState) -> bool {
        match self {
            Action::Pause => matches!(
                s,
                WireState::Queued | WireState::Starting | WireState::Active
            ),
            Action::Resume => matches!(s, WireState::Paused | WireState::Failed { .. }),
            Action::Restart => {
                matches!(
                    s,
                    WireState::Failed { .. } | WireState::NeedsAttention { .. }
                )
            }
        }
    }
}

pub enum Targets {
    All,
    Ids(Vec<String>),
}

/// The result for one id.
#[derive(Serialize)]
struct Outcome {
    id: Value,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<u8>,
    #[serde(skip)]
    message: String,
}

impl Outcome {
    fn ok(id: DownloadId, message: String) -> Self {
        Outcome {
            id: json!(id),
            ok: true,
            error: None,
            code: None,
            message,
        }
    }

    fn fail(id: Value, error: String, code: u8) -> Self {
        Outcome {
            id,
            ok: false,
            message: error.clone(),
            error: Some(error),
            code: Some(code),
        }
    }
}

/// Maps a failed call for `id` to its outcome.
fn failure(id: DownloadId, e: ClientError) -> Outcome {
    match e {
        ClientError::Daemon {
            code: ErrorCode::NotFound,
            ..
        } => Outcome::fail(json!(id), format!("no download with id {id}"), 3),
        ClientError::Daemon { message, .. } => Outcome::fail(json!(id), message, 1),
        e => Outcome::fail(json!(id), e.to_string(), 1),
    }
}

fn parse_id(s: &str) -> Result<DownloadId, Outcome> {
    s.trim().parse::<DownloadId>().map_err(|_| {
        Outcome::fail(
            json!(s),
            format!("\"{s}\" is not a download id (see `puppy ls`)"),
            2,
        )
    })
}

/// Prints every outcome and returns the command's result: the worst code wins.
fn report(ctx: &Ctx, outcomes: &[Outcome]) -> Result<(), CliError> {
    if ctx.json {
        json(&outcomes);
    }
    for o in outcomes {
        if o.ok {
            if !ctx.json {
                out(&o.message);
            }
        } else {
            err(format_args!("puppy: {}: {}", o.id_text(), o.message));
        }
    }
    match worst(outcomes.iter().filter_map(|o| o.code)) {
        0 => Ok(()),
        c => Err(CliError::Reported(c)),
    }
}

impl Outcome {
    fn id_text(&self) -> String {
        match &self.id {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        }
    }
}

/// Runs `make` for each parsed id; ids that do not parse fail with code 2.
async fn each(
    client: &Client,
    ids: &[String],
    make: impl Fn(DownloadId) -> (Request, String),
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for s in ids {
        match parse_id(s) {
            Err(o) => outcomes.push(o),
            Ok(id) => {
                let (req, message) = make(id);
                outcomes.push(match client.call(req).await {
                    Ok(_) => Outcome::ok(id, message),
                    Err(e) => failure(id, e),
                });
            }
        }
    }
    outcomes
}

/// `pause`, `resume` or `restart` the given downloads, or all where it applies.
pub async fn act(ctx: &Ctx, action: Action, targets: Targets) -> Result<(), CliError> {
    let client = ctx.client().await?;
    let outcomes = match targets {
        Targets::Ids(ids) => {
            each(&client, &ids, |id| {
                (action.request(id), format!("{} {id}", action.past()))
            })
            .await
        }
        Targets::All => {
            let ids: Vec<String> = client
                .list()
                .await?
                .iter()
                .filter(|d| action.applies(&d.state))
                .map(|d| d.id.to_string())
                .collect();
            if ids.is_empty() && !ctx.json {
                out(format_args!("nothing to {}", action.verb()));
            }
            each(&client, &ids, |id| {
                (action.request(id), format!("{} {id}", action.past()))
            })
            .await
        }
    };
    report(ctx, &outcomes)
}

/// `rm`: removes downloads, and their files with `delete_files`.
pub async fn rm(ctx: &Ctx, ids: Vec<String>, delete_files: bool) -> Result<(), CliError> {
    let client = ctx.client().await?;
    let outcomes = each(&client, &ids, |id| {
        let suffix = if delete_files { " (files deleted)" } else { "" };
        (
            Request::Remove { id, delete_files },
            format!("removed {id}{suffix}"),
        )
    })
    .await;
    report(ctx, &outcomes)
}

/// `move`: puts a queued download at `position` in the queue.
pub async fn move_to(ctx: &Ctx, id: String, position: usize) -> Result<(), CliError> {
    let client = ctx.client().await?;
    let outcomes = each(&client, &[id], |id| {
        (
            Request::Move { id, position },
            format!("moved {id} to position {position}"),
        )
    })
    .await;
    report(ctx, &outcomes)
}

pub struct LimitArgs {
    pub id: Option<String>,
    pub rate: Option<String>,
    pub global: bool,
    pub down: Option<String>,
    pub up: Option<String>,
}

fn rate_text(r: Option<u64>) -> String {
    r.map_or("none".to_string(), |l| format!("{}/s", bytes(l)))
}

fn rate_arg(s: &str) -> Result<Option<u64>, CliError> {
    parse_rate(s).map_err(CliError::Usage)
}

fn print_settings(ctx: &Ctx, s: &Settings) {
    if ctx.json {
        json(s);
        return;
    }
    out(format_args!("download  {}", rate_text(s.global_down_limit)));
    out(format_args!("upload    {}", rate_text(s.global_up_limit)));
}

async fn settings(client: &Client) -> Result<Settings, CliError> {
    match client.call(Request::GetSettings).await? {
        ReplyData::Settings { settings } => Ok(settings),
        other => Err(CliError::Other(anyhow::anyhow!(
            "unexpected reply from the daemon: {other:?}"
        ))),
    }
}

/// `limit <ID> <RATE>` or `limit --global [--down RATE] [--up RATE]`.
pub async fn limit(ctx: &Ctx, a: LimitArgs) -> Result<(), CliError> {
    if a.global {
        if a.id.is_some() || a.rate.is_some() {
            return Err(CliError::Usage(
                "use either `limit <ID> <RATE>` or `limit --global`, not both".into(),
            ));
        }
        let down = a.down.as_deref().map(rate_arg).transpose()?;
        let up = a.up.as_deref().map(rate_arg).transpose()?;
        let client = ctx.client().await?;
        if down.is_some() || up.is_some() {
            // Read-modify-write: a concurrent change to the other limit between these two
            // calls is overwritten (acceptable for a manual command).
            let cur = settings(&client).await?;
            client
                .call(Request::SetGlobalLimits {
                    down: down.unwrap_or(cur.global_down_limit),
                    up: up.unwrap_or(cur.global_up_limit),
                })
                .await?;
        }
        let s = settings(&client).await?;
        print_settings(ctx, &s);
        return Ok(());
    }
    if a.down.is_some() || a.up.is_some() {
        return Err(CliError::Usage("--down and --up need --global".into()));
    }
    let (Some(id), Some(rate)) = (a.id, a.rate) else {
        return Err(CliError::Usage(
            "usage: puppy limit <ID> <RATE> | puppy limit --global [--down RATE] [--up RATE]"
                .into(),
        ));
    };
    let bps = rate_arg(&rate)?;
    let client = ctx.client().await?;
    let outcomes = each(&client, &[id], |id| {
        (
            Request::SetLimit { id, bps },
            format!("limit {id}: {}", rate_text(bps)),
        )
    })
    .await;
    report(ctx, &outcomes)
}

/// `max-active N`: how many downloads run at once.
pub async fn max_active(ctx: &Ctx, n: u16) -> Result<(), CliError> {
    if n == 0 {
        return Err(CliError::Usage("max-active must be at least 1".into()));
    }
    let client = ctx.client().await?;
    client.call(Request::SetMaxActive { n }).await?;
    if ctx.json {
        json(&json!({ "max_active": n }));
    } else {
        out(format_args!("max-active {n}"));
    }
    Ok(())
}

/// `stop`: shuts the daemon down and waits for it to go away.
pub async fn stop(ctx: &Ctx) -> Result<(), CliError> {
    // JSON: `{"running":false}` when there was nothing to stop, `{"stopped":true}`
    // after stopping it.
    let say = |stopped: bool, text: &str| {
        if !ctx.json {
            out(text);
        } else if stopped {
            json(&json!({ "stopped": true }));
        } else {
            json(&json!({ "running": false }));
        }
    };
    let client = match Client::connect(&ctx.paths.socket, "puppy-cli").await {
        Ok(c) => c,
        Err(ClientError::Io(_)) => {
            say(false, "puppy is not running");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    match client.call(Request::Shutdown).await {
        Ok(_) | Err(ClientError::Disconnected) => {}
        Err(e) => return Err(e.into()),
    }
    drop(client);
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        match Client::connect(&ctx.paths.socket, "puppy-cli").await {
            Err(ClientError::Io(_)) => break,
            _ if Instant::now() >= deadline => {
                return Err(CliError::Other(anyhow::anyhow!(
                    "the daemon did not stop within {} s",
                    STOP_WAIT.as_secs()
                )));
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    say(true, "stopped");
    Ok(())
}
