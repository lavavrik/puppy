//! `puppy completions <shell>` and the hidden `puppy man`: shell completion scripts
//! and manual pages, both generated from the clap definitions so they always match
//! the binary.

use super::CliError;
use super::output::is_closed_pipe;
use anyhow::Context;
use clap_complete::Shell;
use std::io::Write;
use std::path::Path;

/// `cmd` without its hidden subcommands (`native-host`, `man`), which
/// `clap_complete` would otherwise offer. The man pages leave them out on their own.
fn visible(cmd: clap::Command) -> clap::Command {
    let mut out = clap::Command::new("puppy")
        .version(env!("CARGO_PKG_VERSION"))
        .args(cmd.get_arguments().cloned())
        .subcommands(cmd.get_subcommands().filter(|s| !s.is_hide_set()).cloned());
    if let Some(a) = cmd.get_about() {
        out = out.about(a.clone());
    }
    if let Some(a) = cmd.get_long_about() {
        out = out.long_about(a.clone());
    }
    out
}

/// The completion script for `shell`.
pub fn completion_script(cmd: clap::Command, shell: Shell) -> Vec<u8> {
    let mut cmd = visible(cmd);
    let name = cmd.get_name().to_string();
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut cmd, name, &mut buf);
    buf
}

/// The top-level manual page (`puppy.1`) as roff.
pub fn man_page(cmd: clap::Command) -> Vec<u8> {
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd.disable_help_subcommand(true))
        .render(&mut buf)
        .expect("writing to a Vec cannot fail");
    buf
}

/// Prints the completion script for `shell` on stdout.
pub fn completions(cmd: clap::Command, shell: Shell) -> Result<(), CliError> {
    write_stdout(&completion_script(cmd, shell))
}

/// Prints `puppy.1` on stdout, or, with `dir`, writes `puppy.1` and one page per
/// visible subcommand (`puppy-add.1`, `puppy-browser-install.1`, ...) into it.
pub fn man(cmd: clap::Command, dir: Option<&Path>) -> Result<(), CliError> {
    let Some(dir) = dir else {
        return write_stdout(&man_page(cmd));
    };
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    clap_mangen::generate_to(cmd, dir)
        .with_context(|| format!("cannot write the man pages to {}", dir.display()))?;
    Ok(())
}

/// Writes `bytes` to stdout; a reader that went away (`| head`) is not an error.
fn write_stdout(bytes: &[u8]) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        Err(e) if !is_closed_pipe(&e) => Err(anyhow::Error::new(e)
            .context("cannot write to stdout")
            .into()),
        _ => Ok(()),
    }
}
