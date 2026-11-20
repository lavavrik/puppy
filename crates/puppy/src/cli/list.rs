//! Listing commands: `status`, `ls` and `info`.

use super::output::{ColWidth, json, out, table};
use super::{CliError, Ctx};
use crate::cli::ClientError;
use clap::ValueEnum;
use puppy_core::types::{DownloadId, EngineDetail};
use puppy_proto::WireState;
use puppy_proto::{DownloadSnapshot, ErrorCode, ReplyData, Request, WireAttention};
use puppy_tui::format::percent;
use puppy_tui::format::{bytes, eta, speed};
use serde::Serialize;

#[derive(Serialize)]
struct Summary {
    downloads: usize,
    active: usize,
}

/// `N downloads, X active` (`1 download, …`), or `{"downloads":N,"active":X}`.
pub async fn status(ctx: &Ctx) -> Result<(), CliError> {
    let downloads = ctx.client().await?.list().await?;
    let active = downloads
        .iter()
        .filter(|d| matches!(d.state, WireState::Starting | WireState::Active))
        .count();
    let n = downloads.len();
    if ctx.json {
        json(&Summary {
            downloads: n,
            active,
        });
    } else {
        let noun = if n == 1 { "download" } else { "downloads" };
        out(format_args!("{n} {noun}, {active} active"));
    }
    Ok(())
}

/// Which states `ls --state` keeps.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateFilter {
    Queued,
    Starting,
    Active,
    Paused,
    Completed,
    Failed,
    Attention,
}

impl StateFilter {
    fn matches(self, s: &WireState) -> bool {
        matches!(
            (self, s),
            (StateFilter::Queued, WireState::Queued)
                | (StateFilter::Starting, WireState::Starting)
                | (StateFilter::Active, WireState::Active)
                | (StateFilter::Paused, WireState::Paused)
                | (StateFilter::Completed, WireState::Completed)
                | (StateFilter::Failed, WireState::Failed { .. })
                | (StateFilter::Attention, WireState::NeedsAttention { .. })
        )
    }
}

/// Downloads (only those in `states`, if any are given) as a table, or a JSON array
/// of snapshots.
pub async fn ls(ctx: &Ctx, states: Vec<StateFilter>) -> Result<(), CliError> {
    let mut downloads = ctx.client().await?.list().await?;
    if !states.is_empty() {
        downloads.retain(|d| states.iter().any(|f| f.matches(&d.state)));
    }
    if ctx.json {
        json(&downloads);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = downloads
        .iter()
        .map(|d| {
            let progress = percent(d.done_bytes, d.total_bytes)
                .map_or("—".to_string(), |p| format!("{:.0}%", p * 100.0));
            vec![
                d.id.to_string(),
                state_word(&d.state).to_string(),
                progress,
                speed(d.down_bps),
                eta(d.eta_secs),
                d.name.clone(),
            ]
        })
        .collect();
    table(
        &["ID", "STATE", "PROGRESS", "SPEED", "ETA", "NAME"],
        &rows,
        &[
            ColWidth::Fixed(6),
            ColWidth::Fixed(9),
            ColWidth::Fixed(8),
            ColWidth::Fixed(12),
            ColWidth::Fixed(8),
            ColWidth::Flex,
        ],
    );
    Ok(())
}

pub fn state_word(s: &WireState) -> &'static str {
    match s {
        WireState::Queued => "queued",
        WireState::Starting => "starting",
        WireState::Active => "active",
        WireState::Paused => "paused",
        WireState::Completed => "done",
        WireState::Failed {
            transient: true, ..
        } => "retrying",
        WireState::Failed { .. } => "failed",
        WireState::NeedsAttention { .. } => "attention",
        WireState::Unknown => "unknown",
    }
}

#[derive(Serialize)]
struct Info<'a> {
    download: &'a DownloadSnapshot,
    detail: &'a Option<EngineDetail>,
}

pub(super) fn attention_text(r: &WireAttention) -> String {
    match r {
        WireAttention::RemoteChanged => "the file changed on the server".to_string(),
        WireAttention::NotResumable => "the server cannot resume this download".to_string(),
        WireAttention::ValidationFailed { detail } => format!("validation failed: {detail}"),
        WireAttention::Unknown => "unknown reason".to_string(),
    }
}

/// Everything about one download: key/value lines, or `{"download":…,"detail":…}`.
pub async fn info(ctx: &Ctx, id: DownloadId) -> Result<(), CliError> {
    let reply = ctx.client().await?.call(Request::Get { id }).await;
    let (d, detail) = match reply {
        Ok(ReplyData::Download { download, detail }) => (download, detail),
        Ok(other) => {
            return Err(CliError::Other(anyhow::anyhow!(
                "unexpected reply from the daemon: {other:?}"
            )));
        }
        Err(ClientError::Daemon {
            code: ErrorCode::NotFound,
            ..
        }) => return Err(CliError::UnknownId(format!("no download with id {id}"))),
        Err(e) => return Err(e.into()),
    };
    if ctx.json {
        json(&Info {
            download: &d,
            detail: &detail,
        });
        return Ok(());
    }
    let mut state = state_word(&d.state).to_string();
    match &d.state {
        WireState::Failed { reason, .. } => state = format!("{state}: {reason}"),
        WireState::NeedsAttention { reason } => {
            state = format!("{state}: {}", attention_text(reason));
        }
        _ => {}
    }
    let kv = |k: &str, v: String| out(format_args!("{k:<12}{v}"));
    kv("id", d.id.to_string());
    kv("name", d.name.clone());
    kv(
        "path",
        d.path
            .as_ref()
            .map_or("—".to_string(), |p| p.display().to_string()),
    );
    kv("source", d.source.clone());
    kv("state", state);
    kv("size", d.total_bytes.map_or("—".to_string(), bytes));
    kv("downloaded", bytes(d.done_bytes));
    kv("speed", speed(d.down_bps));
    kv("eta", eta(d.eta_secs));
    kv(
        "limit",
        d.limit
            .map_or("none".to_string(), |l| format!("{}/s", bytes(l))),
    );
    match detail {
        Some(EngineDetail::Http {
            segments,
            connections,
            resumable,
        }) => {
            let active = segments.iter().filter(|s| s.active).count();
            kv("segments", format!("{} ({active} active)", segments.len()));
            kv("connections", connections.to_string());
            kv(
                "resumable",
                if resumable { "yes" } else { "no" }.to_string(),
            );
        }
        Some(EngineDetail::Torrent {
            piece_count,
            have,
            peers,
            seeds,
            leechers,
            ratio,
        }) => {
            let got: u32 = have.iter().map(|b| b.count_ones()).sum();
            kv("pieces", format!("{got}/{piece_count}"));
            kv(
                "peers",
                format!("{peers} ({seeds} seeds, {leechers} leechers)"),
            );
            kv("ratio", format!("{ratio:.2}"));
        }
        Some(EngineDetail::Unknown) | None => {}
    }
    Ok(())
}
