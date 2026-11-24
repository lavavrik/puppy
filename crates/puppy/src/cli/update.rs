//! `puppy update`: look for a newer release and, unless `--check`, install it.

use super::output::{json, out};
use super::{CliError, ClientError, Ctx};
use puppy_client::Client;
use puppy_proto::{ErrorCode, UpdateStatus};
use serde_json::json;
use std::time::{Duration, Instant};

/// How long to wait for the daemon to come back after an install.
const COMEBACK: Duration = Duration::from_secs(15);
const COMEBACK_STEP: Duration = Duration::from_millis(200);

/// Maps a failed update call to the CLI's errors: refusals exit 6, the rest 1.
fn update_error(e: ClientError) -> CliError {
    match e {
        ClientError::Daemon {
            code: ErrorCode::Unsupported,
            message,
        } => CliError::NotPossible(message),
        // A daemon from before self-update does not know the request.
        ClientError::Daemon {
            code: ErrorCode::BadRequest,
            ..
        } => CliError::NotPossible(
            "this daemon is too old to update itself; rerun the install command".into(),
        ),
        ClientError::Daemon {
            code: ErrorCode::Engine,
            message,
        } => CliError::Other(anyhow::anyhow!("update failed: {message}")),
        e => e.into(),
    }
}

/// The line that ends a finished update: the daemon that came back must be the new one.
pub fn finish_line(expected: &str, daemon_version: &str) -> Result<String, CliError> {
    if expected == daemon_version {
        Ok(format!("updated to {expected}"))
    } else {
        Err(CliError::Other(anyhow::anyhow!(
            "the daemon did not come back as {expected}"
        )))
    }
}

/// Waits for a daemon of `expected` to answer; returns the last daemon that did, if
/// any (of `expected` unless none came back as it in time).
async fn wait_for_comeback(ctx: &Ctx, expected: &str) -> Option<Client> {
    let deadline = Instant::now() + COMEBACK;
    let mut last = None;
    while Instant::now() < deadline {
        if let Ok(c) = ctx.client().await {
            let done = c.daemon_version() == expected;
            last = Some(c);
            if done {
                break;
            }
        }
        tokio::time::sleep(COMEBACK_STEP).await;
    }
    last
}

/// After the install of `version` was answered `Installed`, or cut off by the
/// daemon going away: the daemon must come back as `version`. If one that did not
/// knows why its install failed, that is the error.
async fn comeback(ctx: &Ctx, version: &str) -> Result<(), CliError> {
    let last = wait_for_comeback(ctx, version).await;
    let seen = last
        .as_ref()
        .map(|c| c.daemon_version().to_string())
        .unwrap_or_default();
    if seen != version
        && let Some(c) = &last
        && let Ok(UpdateStatus::Failed { error, .. }) = c.get_update().await
    {
        return Err(fail(&error));
    }
    let line = finish_line(version, &seen)?;
    let status = UpdateStatus::Installed {
        version: version.to_string(),
    };
    finish(ctx, &status, line)
}

fn fail(error: &str) -> CliError {
    CliError::Other(anyhow::anyhow!("update failed: {error}"))
}

pub async fn update(ctx: &Ctx, check_only: bool) -> Result<(), CliError> {
    let client = ctx.client().await?;
    let current = client.daemon_version().to_string();
    let found = client.update(true).await.map_err(update_error)?;
    let (version, can_install, reason) = match &found {
        UpdateStatus::UpToDate => {
            return finish(ctx, &found, format!("puppy is up to date ({current})"));
        }
        UpdateStatus::Available {
            version,
            can_install,
            reason,
        } => (version.clone(), *can_install, reason.clone()),
        UpdateStatus::Failed { error, .. } => return Err(fail(error)),
        other => {
            return Err(CliError::Other(anyhow::anyhow!(
                "unexpected update state: {other:?}"
            )));
        }
    };
    if check_only {
        return finish(ctx, &found, format!("puppy {version} is available"));
    }
    if !can_install {
        return Err(CliError::NotPossible(reason.unwrap_or_else(|| {
            "this build was not made by puppy's release workflow".to_string()
        })));
    }
    if !ctx.json {
        out(format_args!(
            "updating puppy {current} \u{2192} {version}\u{2026}"
        ));
    }
    let status = match client.update(false).await {
        Ok(s) => s,
        // The daemon may restart before its reply gets out; what comes back tells.
        Err(ClientError::Disconnected) => {
            drop(client);
            return comeback(ctx, &version).await;
        }
        Err(e) => return Err(update_error(e)),
    };
    match &status {
        UpdateStatus::Installed { version } => {
            drop(client);
            comeback(ctx, version).await
        }
        UpdateStatus::UpToDate => finish(ctx, &status, format!("puppy is up to date ({current})")),
        UpdateStatus::Failed { error, .. } => Err(fail(error)),
        other => Err(CliError::Other(anyhow::anyhow!(
            "unexpected update state: {other:?}"
        ))),
    }
}

/// Prints the final status (JSON) or its line.
fn finish(ctx: &Ctx, status: &UpdateStatus, line: String) -> Result<(), CliError> {
    if ctx.json {
        json(&json!({ "status": status }));
    } else {
        out(line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_line_needs_the_new_version() {
        assert_eq!(finish_line("0.2.0", "0.2.0").unwrap(), "updated to 0.2.0");
        let e = finish_line("0.2.0", "0.1.0").unwrap_err();
        assert!(matches!(e, CliError::Other(_)));
        assert_eq!(e.to_string(), "the daemon did not come back as 0.2.0");
        assert!(finish_line("0.2.0", "").is_err());
    }

    #[test]
    fn old_daemon_cannot_update_itself() {
        let e = update_error(ClientError::Daemon {
            code: ErrorCode::BadRequest,
            message: "unknown variant `update`".into(),
        });
        assert!(matches!(e, CliError::NotPossible(_)), "{e:?}");
        assert_eq!(e.exit_code(), 6);
        assert_eq!(
            e.to_string(),
            "cannot update: this daemon is too old to update itself; rerun the install command"
        );
    }

    #[test]
    fn refusals_exit_6_and_failures_1() {
        let e = update_error(ClientError::Daemon {
            code: ErrorCode::Unsupported,
            message: "not a release build".into(),
        });
        assert_eq!(e.exit_code(), 6);
        assert_eq!(e.to_string(), "cannot update: not a release build");
        let e = update_error(ClientError::Daemon {
            code: ErrorCode::Engine,
            message: "checksum mismatch".into(),
        });
        assert_eq!(e.exit_code(), 1);
        assert_eq!(e.to_string(), "update failed: checksum mismatch");
    }
}
