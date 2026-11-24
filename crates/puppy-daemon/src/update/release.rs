//! Asking GitHub for the latest release.

use semver::Version;
use serde::Deserialize;
use url::Url;

/// Why an update did not happen.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// This puppy cannot update itself here (dev build, read-only install folder).
    #[error("{0}")]
    NotPossible(String),
    /// Something went wrong while looking, downloading or installing.
    #[error("{0}")]
    Failed(String),
}

/// A downloadable release file.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: Url,
}

/// A release newer than the running puppy, with the files for this platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    pub archive: Asset,
    pub checksum: Asset,
}

/// The archive file name for a release: `puppy-<version>-<target>.tar.gz`, or `.zip`
/// for Windows targets.
pub fn archive_name(version: &Version, target: &str) -> String {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("puppy-{version}-{target}.{ext}")
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// Looks up the latest release. `Ok(None)`: it is not newer than `current`, or it has no
/// archive and checksum for `target`.
pub async fn latest(
    http: &reqwest::Client,
    api_base: &Url,
    current: &Version,
    target: &str,
) -> Result<Option<Release>, UpdateError> {
    let url = format!(
        "{}/repos/lavavrik/puppy/releases/latest",
        api_base.as_str().trim_end_matches('/')
    );
    let resp = http
        .get(&url)
        .header("User-Agent", format!("puppy/{current}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| {
            UpdateError::Failed(format!(
                "could not reach the release server: {}",
                e.without_url()
            ))
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(UpdateError::Failed(format!(
            "the release server answered {status}"
        )));
    }
    let body = resp.bytes().await.map_err(|e| {
        UpdateError::Failed(format!(
            "could not read the release answer: {}",
            e.without_url()
        ))
    })?;
    let api: ApiRelease = serde_json::from_slice(&body)
        .map_err(|e| UpdateError::Failed(format!("the release answer was not valid JSON: {e}")))?;

    let Ok(version) = Version::parse(api.tag_name.trim_start_matches('v')) else {
        return Ok(None);
    };
    if version <= *current {
        return Ok(None);
    }
    let archive_file = archive_name(&version, target);
    let checksum_file = format!("{archive_file}.sha256");
    let find = |name: &str| {
        api.assets.iter().find(|a| a.name == name).and_then(|a| {
            Some(Asset {
                name: a.name.clone(),
                url: Url::parse(&a.browser_download_url).ok()?,
            })
        })
    };
    match (find(&archive_file), find(&checksum_file)) {
        (Some(archive), Some(checksum)) => Ok(Some(Release {
            version,
            archive,
            checksum,
        })),
        _ => Ok(None),
    }
}
