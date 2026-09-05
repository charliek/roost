//! Where each agent keeps its configuration.
//!
//! Every path this crate touches is derived from one [`Home`], and a
//! `Home` is built either from the environment or from an explicit
//! root. That is not a testing convenience — it is the harness jail:
//! a test that constructs `Home::rooted(tempdir)` cannot reach a real
//! dotfile even if every other guard failed.

use std::path::{Path, PathBuf};

use roost_agent::Agent;

use crate::error::InstallError;

/// Every agent Roost can wire, in the order status and ensure report
/// them.
pub const ALL_AGENTS: [Agent; 5] = [
    Agent::Claude,
    Agent::Codex,
    Agent::Grok,
    Agent::Cursor,
    Agent::Opencode,
];

/// The environment variable that relocates `agent`'s config directory,
/// as that agent itself documents it.
pub const fn config_dir_env(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "CLAUDE_CONFIG_DIR",
        Agent::Codex => "CODEX_HOME",
        Agent::Grok => "GROK_HOME",
        Agent::Cursor => "CURSOR_CONFIG_DIR",
        Agent::Opencode => "OPENCODE_CONFIG_DIR",
    }
}

/// The default location of `agent`'s config directory, relative to the
/// user's home.
const fn default_config_dir(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => ".claude",
        Agent::Codex => ".codex",
        Agent::Grok => ".grok",
        Agent::Cursor => ".cursor",
        Agent::Opencode => ".config/opencode",
    }
}

/// The resolved config directory of every supported agent, plus Roost's
/// own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    home: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    grok: PathBuf,
    cursor: PathBuf,
    opencode: PathBuf,
}

impl Home {
    /// Resolve against `home`, letting `env` relocate any agent.
    ///
    /// `env` is a parameter rather than a `std::env::var` call so the
    /// override behaviour is testable without mutating process-global
    /// state from parallel test threads.
    pub fn resolve(home: impl Into<PathBuf>, env: impl Fn(&str) -> Option<String>) -> Home {
        let home = home.into();
        let dir = |agent: Agent| -> PathBuf {
            match env(config_dir_env(agent)).filter(|v| !v.trim().is_empty()) {
                Some(value) => {
                    let path = PathBuf::from(value);
                    if path.is_absolute() {
                        path
                    } else {
                        home.join(path)
                    }
                }
                None => home.join(default_config_dir(agent)),
            }
        };
        Home {
            claude: dir(Agent::Claude),
            codex: dir(Agent::Codex),
            grok: dir(Agent::Grok),
            cursor: dir(Agent::Cursor),
            opencode: dir(Agent::Opencode),
            home,
        }
    }

    /// The real user's layout: `$HOME` plus whatever the environment
    /// says about each agent.
    pub fn from_env() -> Result<Home, InstallError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or(InstallError::NoHome)?;
        Ok(Home::resolve(home, |key| std::env::var(key).ok()))
    }

    /// Defaults only, under `root`. The environment is ignored, which is
    /// exactly what a test wants.
    pub fn rooted(root: impl Into<PathBuf>) -> Home {
        Home::resolve(root, |_| None)
    }

    pub fn path(&self) -> &Path {
        &self.home
    }

    /// The same home with one agent's directory pointed somewhere else.
    ///
    /// What `agent-hooks = off` needs: the state record can name a
    /// `CODEX_HOME` the user has since moved away from, and the plan
    /// pins that `off` cleans *those* files too rather than orphaning
    /// them. Roost's own paths — the record, the lock — stay put.
    pub fn with_agent_dir(&self, agent: Agent, dir: impl Into<PathBuf>) -> Home {
        let dir = dir.into();
        let mut out = self.clone();
        match agent {
            Agent::Claude => out.claude = dir,
            Agent::Codex => out.codex = dir,
            Agent::Grok => out.grok = dir,
            Agent::Cursor => out.cursor = dir,
            Agent::Opencode => out.opencode = dir,
        }
        out
    }

    pub fn agent_dir(&self, agent: Agent) -> &Path {
        match agent {
            Agent::Claude => &self.claude,
            Agent::Codex => &self.codex,
            Agent::Grok => &self.grok,
            Agent::Cursor => &self.cursor,
            Agent::Opencode => &self.opencode,
        }
    }

    /// "Present" means the agent's config directory exists. Roost never
    /// creates one: wiring an agent that is not installed would leave a
    /// directory the user did not ask for.
    pub fn is_present(&self, agent: Agent) -> bool {
        self.agent_dir(agent).is_dir()
    }

    /// Roost's own configuration directory — where the state record and
    /// the ensure lock live.
    pub fn roost_config_dir(&self) -> PathBuf {
        self.home.join(".config/roost")
    }

    /// `<config dir>/roost/agent-hooks.json`.
    pub fn record_path(&self) -> PathBuf {
        self.roost_config_dir().join("agent-hooks.json")
    }

    /// `<config dir>/roost/agent-hooks.lock`.
    pub fn lock_path(&self) -> PathBuf {
        self.roost_config_dir().join("agent-hooks.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn defaults_sit_where_each_agent_documents_them() {
        let home = Home::rooted("/home/u");
        assert_eq!(home.agent_dir(Agent::Claude), Path::new("/home/u/.claude"));
        assert_eq!(home.agent_dir(Agent::Codex), Path::new("/home/u/.codex"));
        assert_eq!(home.agent_dir(Agent::Grok), Path::new("/home/u/.grok"));
        assert_eq!(home.agent_dir(Agent::Cursor), Path::new("/home/u/.cursor"));
        assert_eq!(
            home.agent_dir(Agent::Opencode),
            Path::new("/home/u/.config/opencode")
        );
        assert_eq!(
            home.record_path(),
            Path::new("/home/u/.config/roost/agent-hooks.json")
        );
    }

    /// Each agent's own override wins. This is the jail the e2e harness
    /// relies on: point all five somewhere else and nothing real is
    /// reachable.
    #[test]
    fn every_agent_has_an_env_override() {
        let env = env_of(&[
            ("CLAUDE_CONFIG_DIR", "/jail/claude"),
            ("CODEX_HOME", "/jail/codex"),
            ("GROK_HOME", "/jail/grok"),
            ("CURSOR_CONFIG_DIR", "/jail/cursor"),
            ("OPENCODE_CONFIG_DIR", "/jail/opencode"),
        ]);
        let home = Home::resolve("/home/u", |k| env.get(k).cloned());
        for agent in ALL_AGENTS {
            assert_eq!(
                home.agent_dir(agent),
                Path::new(&format!("/jail/{}", agent.source())),
                "{}",
                agent.source()
            );
        }
    }

    #[test]
    fn a_relative_override_is_taken_against_home() {
        let env = env_of(&[("CODEX_HOME", "dotfiles/codex")]);
        let home = Home::resolve("/home/u", |k| env.get(k).cloned());
        assert_eq!(
            home.agent_dir(Agent::Codex),
            Path::new("/home/u/dotfiles/codex")
        );
    }

    #[test]
    fn a_blank_override_is_not_an_override() {
        let env = env_of(&[("CLAUDE_CONFIG_DIR", "   ")]);
        let home = Home::resolve("/home/u", |k| env.get(k).cloned());
        assert_eq!(home.agent_dir(Agent::Claude), Path::new("/home/u/.claude"));
    }

    #[test]
    fn presence_is_the_directory_existing() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        assert!(!home.is_present(Agent::Claude));
        std::fs::create_dir_all(home.agent_dir(Agent::Claude)).unwrap();
        assert!(home.is_present(Agent::Claude));
    }
}
