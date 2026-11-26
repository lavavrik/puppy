//! Metalink sources of `puppy add` (on the command line or in an input file): told
//! apart from other sources, read or fetched here in the client, and expanded into one
//! planned download per `<file>`.
//!
//! Expanding in the client keeps the daemon a plain download engine: one `add` request
//! per file, so each file is its own download with its own id, state and errors, and a
//! daemon that predates Metalink takes them as they are. It also means the user's
//! headers and cookies reach the metalink's server as they would a download's, and
//! everything about the document (fetch errors, unsafe names, a `--name` that cannot
//! apply) is known before anything is queued.

use super::add::{Defaults, Planned};
use super::input_file::EntryOptions;
use super::{CliError, Ctx};
use puppy_client::Client;
use puppy_core::config::HttpConfig;
use puppy_core::metalink::{self, Location};
use puppy_core::types::masked_url;
use puppy_proto::{ReplyData, Request};
use puppy_tui::app::forms::{expand_tilde, unquote};
use std::path::PathBuf;

/// How sources are told to be metalinks (`--metalink`, `--no-metalink`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Detect {
    /// Every source is one.
    pub force: bool,
    /// None is (a `.meta4` URL is downloaded as a file).
    pub off: bool,
}

impl Detect {
    /// Where the metalink `s` is, if it is one (see [`metalink::detect`]).
    pub fn check(&self, s: &str) -> Result<Option<Location>, String> {
        if self.off {
            return Ok(None);
        }
        let s = unquote(s);
        // Only a path's `~` is expanded; a URL comes back as it was.
        let local = expand_tilde(s);
        metalink::detect(&local.to_string_lossy(), self.force)
    }
}

/// A metalink source, not yet read.
pub(super) struct Pending {
    /// As the user wrote it.
    pub shown: String,
    pub location: Location,
    /// Its input-file entry's options (empty on the command line).
    pub options: EntryOptions,
    /// `list.txt: line 3` for an input-file entry: prefixes its usage errors.
    pub origin: Option<String>,
}

/// What expanding needs from outside, fetched once: the HTTP settings (proxy) from the
/// configuration, and the daemon's download folder (for a file in a subdirectory when
/// no output folder was given; this connects to the daemon, which is kept for adding).
pub(super) struct Env<'a> {
    ctx: &'a Ctx,
    http: Option<HttpConfig>,
    download_dir: Option<PathBuf>,
    pub client: Option<Client>,
}

impl<'a> Env<'a> {
    pub fn new(ctx: &'a Ctx) -> Self {
        Env {
            ctx,
            http: None,
            download_dir: None,
            client: None,
        }
    }

    fn http(&mut self) -> &HttpConfig {
        let ctx = self.ctx;
        self.http.get_or_insert_with(|| {
            match puppy_daemon::Config::load(&ctx.paths.config_file()) {
                Ok(c) => c.http_config(),
                Err(e) => {
                    super::output::err(format_args!(
                        "puppy: warning: {e}; fetching metalinks without its settings"
                    ));
                    HttpConfig::default()
                }
            }
        })
    }

    async fn download_dir(&mut self) -> Result<PathBuf, CliError> {
        if let Some(d) = &self.download_dir {
            return Ok(d.clone());
        }
        if self.client.is_none() {
            self.client = Some(self.ctx.client().await?);
        }
        let client = self.client.as_ref().expect("connected");
        let dir = match client.call(Request::GetSettings).await? {
            ReplyData::Settings { settings } => settings.download_dir,
            other => {
                return Err(CliError::Other(anyhow::anyhow!(
                    "unexpected reply to get_settings: {other:?}"
                )));
            }
        };
        self.download_dir = Some(dir.clone());
        Ok(dir)
    }
}

/// Reads or fetches `p` and plans its files. `Ok(Err(why))`: the metalink could not be
/// used (fetch failed, not a metalink, an unsafe name, ...): reported like a source
/// that cannot be added. `Err`: a usage error (an option that cannot apply to it).
pub(super) async fn expand(
    env: &mut Env<'_>,
    defaults: &Defaults,
    p: &Pending,
) -> Result<Result<Vec<Planned>, String>, CliError> {
    let o = &p.options;
    let context = defaults.context_for(o);
    let loaded = match metalink::load(&p.location, env.http(), &context).await {
        Ok(l) => l,
        Err(e) => return Ok(Err(e)),
    };
    let files = &loaded.metalink.files;
    let usage = |what: &str| {
        let at = p
            .origin
            .as_deref()
            .map_or(String::new(), |o| format!("{o}: "));
        CliError::Usage(format!(
            "{at}{what} does not go with {}: it lists {} files, each with its own {}",
            p.shown,
            files.len(),
            if what.contains("checksum") {
                "hashes"
            } else if what.contains("mirror") {
                "URLs"
            } else {
                "name"
            }
        ))
    };
    let name = o.out.clone().or_else(|| defaults.name.clone());
    let checksum = o.checksum.clone().or_else(|| defaults.checksum.clone());
    if files.len() > 1 {
        if name.is_some() {
            return Err(usage(if o.out.is_some() { "out" } else { "--name" }));
        }
        if checksum.is_some() {
            return Err(usage(if o.checksum.is_some() {
                "checksum"
            } else {
                "--checksum"
            }));
        }
        if !defaults.mirrors.is_empty() {
            return Err(usage("--mirror"));
        }
    }
    let base = o.dir.clone().or_else(|| defaults.output_dir.clone());
    let mut planned = Vec::with_capacity(files.len());
    for f in files {
        // A given name replaces the whole path, subdirectories included.
        let output_dir = if name.is_some() || !f.path.has_dirs() {
            base.clone()
        } else {
            let base = match &base {
                Some(b) => b.clone(),
                None => env.download_dir().await?,
            };
            Some(f.path.dir_in(&base))
        };
        let mut req = f.request(output_dir, &context, loaded.document.as_ref());
        if let Some(n) = &name {
            req.filename = Some(n.clone());
        }
        if let Some(c) = &checksum {
            req.checksum = Some(c.clone());
        }
        req.mirrors.extend(defaults.mirrors.iter().cloned());
        planned.push(Planned {
            shown: masked_url(&f.urls[0].url),
            label: Some(name.clone().unwrap_or_else(|| f.path.to_string())),
            metalink: Some(p.shown.clone()),
            req,
            paused: o.pause.unwrap_or(defaults.paused),
            limit: o.limit.unwrap_or(defaults.limit),
        });
    }
    Ok(Ok(planned))
}
