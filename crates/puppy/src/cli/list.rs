//! Listing commands: `status` and (for now) a basic `ls`.

use super::output::{ColWidth, json, out, table};
use super::{CliError, Ctx};
use puppy_proto::WireState;
use puppy_tui::format::percent;
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

/// All downloads as a table (or a JSON array of snapshots).
pub async fn ls(ctx: &Ctx) -> Result<(), CliError> {
    let downloads = ctx.client().await?.list().await?;
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
                d.name.clone(),
            ]
        })
        .collect();
    table(
        &["ID", "STATE", "PROGRESS", "NAME"],
        &rows,
        &[
            ColWidth::Fixed(6),
            ColWidth::Fixed(10),
            ColWidth::Fixed(8),
            ColWidth::Flex,
        ],
    );
    Ok(())
}

fn state_word(s: &WireState) -> &'static str {
    match s {
        WireState::Queued => "queued",
        WireState::Starting => "starting",
        WireState::Active => "active",
        WireState::Paused => "paused",
        WireState::Completed => "done",
        WireState::Failed { .. } => "failed",
        WireState::NeedsAttention { .. } => "attention",
        WireState::Unknown => "unknown",
    }
}
