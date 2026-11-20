//! `puppy seed`: a torrent's seeding policy.

use super::control::parse_id_arg;
use super::output::{json, out};
use super::{CliError, ClientError, Ctx};
use puppy_core::types::{DownloadId, SeedPolicy};
use puppy_proto::{ErrorCode, Request};
use serde::Serialize;
use std::time::Duration;

/// `--seed-ratio` / `--ratio`: a number (`0` turns the ratio trigger off) or `default`
/// (inherit the daemon's `seed_ratio`).
pub fn parse_ratio(s: &str) -> Result<Option<f32>, CliError> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    match s.parse::<f32>() {
        Ok(r) if r.is_finite() && r >= 0.0 => Ok(Some(r)),
        _ => Err(CliError::Usage(format!(
            "bad seed ratio {s:?}: expected a number like 1.5, 0 or `default`"
        ))),
    }
}

/// `--seed-time` / `--time`: a duration (`90m`, `2h`; `0` turns the time trigger off)
/// or `default` (inherit the daemon's `seed_time`).
pub fn parse_time(s: &str) -> Result<Option<u64>, CliError> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    if s == "0" {
        return Ok(Some(0));
    }
    humantime::parse_duration(s)
        .map(|d: Duration| Some(d.as_secs()))
        .map_err(|e| {
            CliError::Usage(format!(
                "bad seed time {s:?}: {e} (expected e.g. 90m, 2h, 0 or `default`)"
            ))
        })
}

/// A policy from the flags; `None` when neither was given.
pub fn policy(ratio: Option<&str>, time: Option<&str>) -> Result<Option<SeedPolicy>, CliError> {
    if ratio.is_none() && time.is_none() {
        return Ok(None);
    }
    Ok(Some(SeedPolicy {
        ratio: ratio.map(parse_ratio).transpose()?.flatten(),
        time_secs: time.map(parse_time).transpose()?.flatten(),
    }))
}

#[derive(Serialize)]
struct SeedJson {
    id: DownloadId,
    policy: SeedPolicy,
}

fn describe(p: &SeedPolicy) -> String {
    let ratio = match p.ratio {
        None => "default".to_string(),
        Some(0.0) => "off".to_string(),
        Some(r) => format!("{r}"),
    };
    let time = match p.time_secs {
        None => "default".to_string(),
        Some(0) => "off".to_string(),
        Some(s) => humantime::format_duration(Duration::from_secs(s)).to_string(),
    };
    format!("ratio {ratio}, time {time}")
}

/// `puppy seed <id> [--ratio R|default] [--time DUR|default]`. Replaces the torrent's
/// whole policy: what is not given inherits the daemon's setting again.
pub async fn seed(
    ctx: &Ctx,
    id: &str,
    ratio: Option<&str>,
    time: Option<&str>,
) -> Result<(), CliError> {
    let id = parse_id_arg(id)?;
    let Some(policy) = policy(ratio, time)? else {
        return Err(CliError::Usage(
            "give --ratio and/or --time (`--ratio default --time default` restores the daemon's settings)"
                .into(),
        ));
    };
    let req = Request::SetSeed {
        id,
        policy: policy.clone(),
    };
    match ctx.client().await?.call(req).await {
        Ok(_) => {}
        Err(ClientError::Daemon {
            code: ErrorCode::NotFound,
            ..
        }) => return Err(CliError::UnknownId(format!("no download with id {id}"))),
        Err(e) => return Err(e.into()),
    }
    if ctx.json {
        json(&SeedJson { id, policy });
    } else {
        out(format_args!("seed {id}: {}", describe(&policy)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_and_time() {
        assert_eq!(parse_ratio("1.5").unwrap(), Some(1.5));
        assert_eq!(parse_ratio("0").unwrap(), Some(0.0));
        assert_eq!(parse_ratio("DEFAULT").unwrap(), None);
        for bad in ["-1", "nan", "inf", "x", ""] {
            assert!(parse_ratio(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_time("90m").unwrap(), Some(5400));
        assert_eq!(parse_time("0").unwrap(), Some(0));
        assert_eq!(parse_time("default").unwrap(), None);
        assert!(parse_time("soon").is_err());
        assert!(parse_time("5").is_err());
    }

    #[test]
    fn policy_replaces_whole() {
        assert_eq!(policy(None, None).unwrap(), None);
        assert_eq!(
            policy(Some("2"), None).unwrap(),
            Some(SeedPolicy {
                ratio: Some(2.0),
                time_secs: None
            })
        );
    }
}
