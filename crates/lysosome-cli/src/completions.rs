//! `lysosome completions <shell>`, plus the internal `__generate-man <DIR>`
//! entry point the release pipeline uses to ship man pages in archives.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Command};
use clap_complete::Shell;

#[derive(Debug, Clone, Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_name = "SHELL")]
    pub shell: Shell,
}

pub const COMPLETIONS_HELP: &str = "\
Install (pick your shell):
  bash   lysosome completions bash > ~/.local/share/bash-completion/completions/lysosome
  zsh    lysosome completions zsh > \"${fpath[1]}/_lysosome\"
  fish   lysosome completions fish > ~/.config/fish/completions/lysosome.fish
  pwsh   lysosome completions powershell >> $PROFILE

Homebrew installs bash, zsh and fish completions for you.";

/// Completion script for `shell`.
pub fn completions(mut cmd: Command, args: &CompletionsArgs) -> Result<String> {
    let name = cmd.get_name().to_string();
    let mut buf = Vec::new();
    clap_complete::generate(args.shell, &mut cmd, name, &mut buf);
    String::from_utf8(buf).context("completion script was not UTF-8")
}

/// Hidden argv used by the release build: `lysosome __generate-man <DIR>`.
pub const GENERATE_MAN: &str = "__generate-man";

/// Handles `lysosome __generate-man <DIR>` before clap parses anything.
/// Returns `None` for every other invocation.
pub fn generate_man_entry(command: impl FnOnce() -> Command) -> Option<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != GENERATE_MAN {
        return None;
    }
    let (Some(dir), None) = (args.next(), args.next()) else {
        eprintln!("usage: lysosome {GENERATE_MAN} <DIR>");
        return Some(ExitCode::from(2));
    };
    Some(match write_man_pages(command(), Path::new(&dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    })
}

/// Writes `lysosome.1`, then `lysosome-<sub>.1` for every visible subcommand.
pub fn write_man_pages(cmd: Command, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut cmd = cmd;
    cmd.build();
    let parent = cmd.get_name().to_string();
    write_page(cmd.clone(), parent.clone(), dir)?;
    for sub in cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set() && s.get_name() != "help")
    {
        let title = format!("{parent}-{}", sub.get_name());
        write_page(sub.clone(), title, dir)?;
    }
    Ok(())
}

fn write_page(cmd: Command, title: String, dir: &Path) -> Result<()> {
    let path = dir.join(format!("{title}.1"));
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd)
        .title(title)
        .source(concat!("lysosome ", env!("CARGO_PKG_VERSION")))
        .render(&mut buf)
        .with_context(|| format!("rendering {}", path.display()))?;
    fs::write(&path, buf).with_context(|| format!("writing {}", path.display()))
}
