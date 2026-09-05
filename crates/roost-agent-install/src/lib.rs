//! Writes Roost's hook entries into the agents' own config files — and
//! takes them out again, to the byte.
//!
//! Five agents, five config formats, one contract. `roostctl`, the iced
//! UI and `roost-session` all call [`ensure`]; what lands on disk is one
//! entry per hook event, pointing at `$ROOST_AGENT_HOOK`, identical on
//! every machine (see [`command`]).
//!
//! It lives outside `roost-agent` on purpose. That crate's charter is
//! "no I/O, no socket", and it is what makes the adapters replayable
//! from fixtures; this one is nothing *but* I/O.
//!
//! # The guarantees, stated honestly
//!
//! * **TOML is byte-preserving** outside the tables Roost touches —
//!   `toml_edit`, so `~/.codex/config.toml` keeps its comments and its
//!   layout.
//! * **JSON is not, and cannot be**: parsing and re-serializing loses
//!   the original bytes. The guarantee there is *semantic* — every value
//!   Roost did not add is equal afterwards, **key order is preserved**
//!   (see [`json`], which is why `serde_json`'s `preserve_order` feature
//!   is not used), **numbers keep the token the file spelled them
//!   with**, the file's indent, line ending and trailing-newline
//!   conventions are detected and reused, and **a write happens only
//!   when the parsed value would change**. An unchanged file is not
//!   rewritten at all.
//!
//!   What that does *not* cover, said plainly: escape spellings
//!   (`c` comes back as `c`, `a\/b` as `a/b`) and any layout the
//!   printer does not reproduce — a compact file, an inline array, a
//!   blank line between keys — are normalised the first time Roost has
//!   to write. A file already in the printer's layout round-trips byte
//!   for byte; one that is not comes back semantically equal and
//!   reformatted, and an uninstall cannot undo the reformatting.
//!
//!   **Duplicate keys collapse**, first position and last value. That is
//!   deliberate and is not a loss Roost invents: `serde_json`, `JSON.parse`
//!   and every agent reading these files resolve a duplicate the same
//!   way, so Roost's view of the document is the agent's view of it. A
//!   file with `{"a":1,"a":2}` means `a = 2` to the tool that reads it,
//!   and writing it back with one `a` says exactly what it already said.
//! * **Bytes that are not UTF-8 are a skip, never a substitution.** A
//!   lossy decode would put U+FFFD where the user's byte was and the
//!   next write would persist it over — say — an API token.
//! * **Ownership is exact match, never substring.** An entry is Roost's
//!   only if its command is byte-equal to a string this crate produced,
//!   at any integration version ([`owned_commands`] keeps the retired
//!   spellings so an old install is still recognised and still
//!   cleanable). A user's hook that merely mentions `$ROOST_AGENT_HOOK`
//!   is theirs; a Roost entry someone has edited is no longer ours and
//!   is left where it is, reported rather than rewritten.
//! * **Only a file Roost created is ever deleted**, and only the state
//!   record can say which those are. A `{}` or an empty file that was
//!   already there is the user's, and an uninstall writes it back empty
//!   rather than removing it. Likewise a hook *group* that carries the
//!   user's own keys beside `hooks` survives our handler's removal.
//! * **Every write is atomic, symlink-preserving, and re-checked** — and
//!   the re-check is not a lock. It happens as late as it can, with the
//!   replacement already written and synced, so the gap between looking
//!   and renaming is one `rename(2)` wide. It is not zero and cannot be
//!   made zero; a writer that lands inside it is a lost update this
//!   crate does not detect. [`write::lock`] is what keeps Roost's own
//!   writers out of that gap; nothing can keep a hand edit out of it.
//! * **Nothing is fire-and-forget.** Every path returns a typed error or
//!   a named skip reason, and [`ensure`] hands both back — including a
//!   rollback that could not put a file back
//!   ([`InstallError::RollbackFailed`]).
//!
//! # Shape
//!
//! `plan` reads and decides; [`apply`] writes. A plan with no edits is
//! the idempotency assertion — a second `ensure` plans nothing.
//! [`ensure`] holds one advisory lock across both, because the UI, the
//! Swift app, `roostctl` and a remote connect can all run at once. The
//! wait for that lock is bounded ([`write::LOCK_DEADLINE`]): a caller
//! that cannot get it hears [`InstallError::LockBusy`], because the one
//! thing worse than two writers is a daemon that can never shut down.
//! A run made on somebody else's behalf ([`ensure_on_behalf`]) re-asks
//! their authority *inside* that lock, which is the only place the
//! answer stays true until the write.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod codex_hash;
pub mod command;
pub mod error;
pub mod home;
pub mod json;
pub mod plan;
pub mod state;
pub mod write;

mod claude;
mod codex;
mod cursor;
mod ensure;
mod grok;
mod jsonedit;
mod opencode;

#[cfg(test)]
mod acceptance;

pub use command::{installed_command, is_roost_command, owned_commands, INTEGRATION_VERSION};
pub use ensure::{
    agent_names, ensure, ensure_on_behalf, install, plan, skip_list, status, Mode, Outcome, Status,
};
pub use error::{AgentError, AgentSkip, AgentWarning, InstallError, SkipReason, Warning};
pub use home::{Home, ALL_AGENTS};
pub use plan::{apply, Applied, FileEdit, Guard, InstallPlan, Intent};
pub use state::mark_noticed;

/// `ensure`'s counterpart for a named set of agents. Re-exported here
/// rather than as `ensure::uninstall` so the four verbs read alike.
pub use ensure::uninstall;

/// The files Roost owns or merges into, per agent — what
/// `agent status` prints and what an uninstall touches.
pub fn owned_files(home: &Home, agent: roost_agent::Agent) -> Vec<std::path::PathBuf> {
    use roost_agent::Agent;
    match agent {
        Agent::Claude => vec![claude::settings_path(home)],
        Agent::Codex => vec![codex::hooks_path(home), codex::config_path(home)],
        Agent::Grok => vec![grok::hooks_path(home)],
        Agent::Cursor => vec![cursor::hooks_path(home)],
        Agent::Opencode => vec![opencode::plugin_path(home)],
    }
}

/// The agent config directory a file the state record names sits in, or
/// `None` when the path is not one this agent's install ever writes.
///
/// `agent-hooks = off` needs it: a record naming a `CODEX_HOME` or
/// `GROK_HOME` the user has since moved away from still has to be
/// cleaned, and the only thing that says where that directory was is the
/// recorded path itself. Derived from [`owned_files`] against a sentinel
/// root rather than a second hard-coded table, so an agent whose layout
/// changes cannot leave this behind.
pub fn agent_dir_of(
    agent: roost_agent::Agent,
    file: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let probe = Home::rooted("/roost-agent-install-probe");
    let base = probe.agent_dir(agent).to_path_buf();
    owned_files(&probe, agent).iter().find_map(|owned| {
        let relative = owned.strip_prefix(&base).ok()?;
        let mut dir = file.to_path_buf();
        for part in relative.components().collect::<Vec<_>>().into_iter().rev() {
            if dir.file_name() != Some(part.as_os_str()) {
                return None;
            }
            dir.pop();
        }
        Some(dir)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_agent::Agent;

    /// Every agent has somewhere to write. A variant added to
    /// `roost_agent::Agent` without a writer here would otherwise be a
    /// silent no-op that looks like a working install.
    #[test]
    fn every_agent_has_files_and_a_command() {
        let home = Home::rooted("/home/u");
        for agent in ALL_AGENTS {
            assert!(!owned_files(&home, agent).is_empty(), "{}", agent.source());
            assert!(!installed_command(agent).is_empty(), "{}", agent.source());
        }
        assert_eq!(ALL_AGENTS.len(), 5);
    }

    /// The inventory here and the adapters' one are the same list.
    #[test]
    fn the_agent_list_matches_the_adapter_crate() {
        for agent in ALL_AGENTS {
            assert_eq!(Agent::parse(agent.source()), Some(agent));
        }
    }
}
