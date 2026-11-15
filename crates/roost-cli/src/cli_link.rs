//! `roostctl cli install|uninstall|status` — the `roostctl` link for
//! terminals outside Roost (#261), through the same
//! `roost_ipc::cli_link::act` the UI's `app.cli_link` calls. Never dials
//! a UI: it has to work with none running.

use std::io::Write;
use std::path::PathBuf;

use clap::Subcommand;
use roost_ipc::cli_link::{self, Action, Context};
use roost_ipc::messages::AppCliLinkResult;

use crate::error::CliError;

#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliCmd {
    /// Link this `roostctl` into /usr/local/bin (when it exists and you
    /// can write to it) or ~/.local/bin, or repair a link a moved or
    /// replaced Roost.app left behind. Only a `roostctl` inside an app
    /// bundle has anything to link. Writes that one symlink and nothing
    /// else — no shell rc file is edited.
    Install,
    /// Remove the link `install` made, and any other link into a Roost
    /// app bundle in those directories. A `roostctl` that is not such a
    /// link is left alone.
    Uninstall,
    /// Say what those directories hold.
    Status,
}

impl CliCmd {
    fn action(self) -> Action {
        match self {
            Self::Install => Action::Install,
            Self::Uninstall => Action::Uninstall,
            Self::Status => Action::Status,
        }
    }
}

pub fn run(cmd: CliCmd, json: bool) -> Result<i32, CliError> {
    let source = std::env::current_exe()
        .ok()
        .and_then(|exe| std::fs::canonicalize(exe).ok());
    let dirs = cli_link::dirs_from_env();
    let path_var = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let result = cli_link::act(
        cmd.action(),
        Context {
            source: source.as_deref(),
            dirs: &dirs,
            path_var: path_var.as_deref(),
            home: home.as_deref(),
        },
    );
    if json {
        crate::print_json(&result)?;
        return verdict(&result);
    }
    verdict(&result)?;
    let mut out = std::io::stdout().lock();
    write_text(&mut out, &result)
        .map_err(|error| CliError::Failed(format!("write the result: {error}")))?;
    Ok(0)
}

/// A refusal is a failure: exit 1 with what to do by hand.
fn verdict(result: &AppCliLinkResult) -> Result<i32, CliError> {
    if result.outcome != "refused" {
        return Ok(0);
    }
    Err(CliError::Failed(match &result.command {
        Some(command) => format!("{}; to do it by hand: {command}", result.message),
        None => result.message.clone(),
    }))
}

fn write_text(out: &mut impl Write, result: &AppCliLinkResult) -> std::io::Result<()> {
    writeln!(out, "{}", result.message)?;
    if let Some(line) = &result.path_line {
        writeln!(
            out,
            "That directory is not on PATH here. Add this line to ~/.zprofile (zsh) or \
             ~/.bash_profile (bash), then open a new terminal:\n  {line}"
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outcome: &str) -> AppCliLinkResult {
        AppCliLinkResult {
            state: "installed".into(),
            outcome: outcome.into(),
            message: "Installed 'roostctl': /u/.local/bin/roostctl → /A.app/…".into(),
            ..AppCliLinkResult::default()
        }
    }

    #[test]
    fn the_text_adds_the_path_line_only_when_there_is_one() {
        let mut plain = Vec::new();
        write_text(&mut plain, &result("created")).unwrap();
        assert_eq!(
            String::from_utf8(plain).unwrap(),
            "Installed 'roostctl': /u/.local/bin/roostctl → /A.app/…\n"
        );

        let mut hinted = Vec::new();
        let line = "export PATH=\"$HOME/.local/bin:$PATH\"";
        write_text(
            &mut hinted,
            &AppCliLinkResult {
                path_line: Some(line.into()),
                ..result("created")
            },
        )
        .unwrap();
        let text = String::from_utf8(hinted).unwrap();
        assert!(text.ends_with(&format!("  {line}\n")), "{text}");
    }

    #[test]
    fn a_refusal_exits_one_with_the_command() {
        assert_eq!(verdict(&result("created")), Ok(0));
        assert_eq!(verdict(&result("nothing_to_remove")), Ok(0));
        let refused = AppCliLinkResult {
            message: "/usr/local/bin/roostctl already exists".into(),
            command: Some("ln -sf 'a' 'b'".into()),
            ..result("refused")
        };
        let error = verdict(&refused).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error,
            CliError::Failed(
                "/usr/local/bin/roostctl already exists; to do it by hand: ln -sf 'a' 'b'".into()
            )
        );
    }
}
