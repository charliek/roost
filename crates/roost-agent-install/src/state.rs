//! `~/.config/roost/agent-hooks.json` — what Roost wired, where, and
//! whether it has told the user yet.
//!
//! Three jobs. It carries `noticed`, which is what makes the "Roost
//! wired agent hooks for …" toast fire **once per agent per machine**
//! instead of on every launch. It names the files, so `agent-hooks =
//! off` can clean a machine whose agents have since been uninstalled, or
//! whose `CODEX_HOME` has moved — the record is trusted *and* the agents
//! are rescanned, because either one alone leaves a case behind. And it
//! carries the **pre-image facts an uninstall cannot re-derive**: which
//! files did not exist before Roost wrote them (so only those are ever
//! deleted), and what codex's `[features] hooks` said before Roost set
//! it (so the user's own switch goes back rather than away).
//!
//! It is Roost's own file, so it is written whole. Losing it costs the
//! toast, the file list, and those two pre-image facts — never the
//! ability to unwire: ownership lives in the entries themselves. What a
//! lost record buys is caution, not damage: an uninstall that does not
//! know Roost created a file leaves the file, and one that does not know
//! Roost set a flag leaves the flag.

use std::collections::BTreeMap;
use std::path::PathBuf;

use roost_agent::Agent;
use serde::{Deserialize, Serialize};

use crate::error::{InstallError, Warning};
use crate::home::Home;
use crate::write;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub integration_version: u32,
    pub files: Vec<String>,
    /// The subset of `files` that did not exist before Roost wrote them.
    /// **Only these may be deleted** by an uninstall: a file the user
    /// already had — an empty one, a `{}` — is theirs, and removing it
    /// takes something Roost never gave.
    #[serde(default)]
    pub created: Vec<String>,
    /// What codex's `[features] hooks` said before Roost first set it,
    /// so an uninstall can put the user's own switch back instead of
    /// deleting it. Absent for every other agent, and for a record
    /// written before this field existed — in which case the uninstall
    /// leaves the flag alone rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_features_hooks: Option<PriorFlag>,
    /// Unix seconds. Diagnostic only — nothing branches on it.
    pub wired_at: i64,
    /// False until the UI has shown the one-time toast for this agent.
    pub noticed: bool,
    /// The client label that wired it, or `local`.
    pub by: String,
}

/// A `true`/`false`/absent switch as it stood before Roost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PriorFlag {
    Absent,
    True,
    False,
}

/// What the record remembers that a *removal* has to honour.
///
/// Loaded once per uninstall plan, so a planner does not have to be
/// handed the whole record — and so `plan(agent, home, mode)` keeps its
/// shape for the callers that only want to look.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prior {
    /// Files Roost created, by the path it was configured with.
    pub created: Vec<PathBuf>,
    pub codex_features_hooks: Option<PriorFlag>,
}

impl Prior {
    /// May an uninstall delete `path` outright?
    pub fn created(&self, path: &std::path::Path) -> bool {
        self.created.iter().any(|p| p == path)
    }
}

/// What the record says about `agent`, for an uninstall.
pub fn prior(home: &Home, agent: Agent) -> Result<Prior, InstallError> {
    let (record, _) = load(home)?;
    Ok(entry(&record, agent)
        .map(|entry| Prior {
            created: entry.created.iter().map(PathBuf::from).collect(),
            codex_features_hooks: entry.codex_features_hooks,
        })
        .unwrap_or_default())
}

/// Agent name → what Roost did for it. `BTreeMap` so the file is stable
/// across runs and a no-op `ensure` really does write nothing.
pub type Record = BTreeMap<String, AgentRecord>;

/// Read the record, or an empty one.
///
/// An unreadable record is a warning, not a failure: every writer here
/// rescans the agents anyway, so the run is still correct without it.
pub fn load(home: &Home) -> Result<(Record, Option<Warning>), InstallError> {
    let path = home.record_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Record::new(), None)),
        Err(e) => return Err(InstallError::io(&path, e)),
    };
    match serde_json::from_slice::<Record>(&bytes) {
        Ok(record) => Ok((record, None)),
        Err(e) => Ok((
            Record::new(),
            Some(Warning::UnreadableRecord {
                path,
                detail: e.to_string(),
            }),
        )),
    }
}

/// Write the record — but only when it changed.
pub fn save(home: &Home, record: &Record) -> Result<bool, InstallError> {
    let path = home.record_path();
    let mut text = serde_json::to_string_pretty(record).map_err(|e| InstallError::Io {
        path: path.clone(),
        source: std::io::Error::other(e),
    })?;
    text.push('\n');

    let target = write::resolve_target(&path);
    let on_disk = std::fs::read(&target).ok();
    if on_disk.as_deref() == Some(text.as_bytes()) {
        return Ok(false);
    }
    // An empty record on a machine that never had one is not news:
    // `agent-hooks = off` with nothing to remove has to write *nothing*,
    // and a `{}` here would be something.
    if on_disk.is_none() && record.is_empty() {
        return Ok(false);
    }
    write::write_atomic(&target, text.as_bytes(), write::PRIVATE_MODE)?;
    Ok(true)
}

pub fn entry(record: &Record, agent: Agent) -> Option<&AgentRecord> {
    record.get(agent.source())
}

/// What a fresh `set_wired` learned that the record has to keep.
#[derive(Debug, Clone, Default)]
pub struct Wired {
    pub files: Vec<PathBuf>,
    /// Files this run created. Unioned with what the record already
    /// names: a refresh does not create them again, and forgetting means
    /// a file Roost made outlives the uninstall that should remove it.
    pub created: Vec<PathBuf>,
    pub codex_features_hooks: Option<PriorFlag>,
}

/// Record that `agent` is wired, keeping a `noticed` that is already
/// true — a toast shown once must not come back on the next refresh —
/// and the pre-image facts an uninstall will need.
pub fn set_wired(
    record: &mut Record,
    agent: Agent,
    version: u32,
    wired: &Wired,
    by: &str,
    now: i64,
) {
    let existing = entry(record, agent);
    let already_noticed = existing.is_some_and(|e| e.noticed);
    let mut created: Vec<String> = existing.map(|e| e.created.clone()).unwrap_or_default();
    for path in &wired.created {
        let path = path.to_string_lossy().into_owned();
        if !created.contains(&path) {
            created.push(path);
        }
    }
    // The first observation is the true one: after the first install the
    // file says whatever Roost put there.
    let codex_features_hooks = existing
        .and_then(|e| e.codex_features_hooks)
        .or(wired.codex_features_hooks);

    record.insert(
        agent.source().to_string(),
        AgentRecord {
            integration_version: version,
            files: wired
                .files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            created,
            codex_features_hooks,
            wired_at: now,
            noticed: already_noticed,
            by: by.to_string(),
        },
    );
}

/// Seconds since the epoch, or 0 if the clock is before it.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Flip `noticed` for the named agents.
///
/// The UI calls this **after** the toast is on screen. A crash in
/// between loses the toast rather than repeating it, which is the right
/// way round for something that says "Roost just changed your files".
pub fn mark_noticed(home: &Home, agents: &[Agent]) -> Result<bool, InstallError> {
    let _lock = write::lock(&home.lock_path())?;
    let (mut record, _) = load(home)?;
    let mut changed = false;
    for agent in agents {
        if let Some(entry) = record.get_mut(agent.source()) {
            if !entry.noticed {
                entry.noticed = true;
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(false);
    }
    save(home, &record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_a_second_save_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());

        let mut record = Record::new();
        set_wired(
            &mut record,
            Agent::Claude,
            1,
            &Wired {
                files: vec![PathBuf::from("/home/u/.claude/settings.json")],
                created: vec![PathBuf::from("/home/u/.claude/settings.json")],
                codex_features_hooks: None,
            },
            "local",
            1_700_000_000,
        );
        assert!(save(&home, &record).unwrap());
        assert!(
            !save(&home, &record).unwrap(),
            "an unchanged record rewrote"
        );

        let (loaded, warning) = load(&home).unwrap();
        assert!(warning.is_none());
        assert_eq!(loaded, record);
        let entry = entry(&loaded, Agent::Claude).unwrap();
        assert_eq!(entry.files, ["/home/u/.claude/settings.json"]);
        assert_eq!(entry.created, ["/home/u/.claude/settings.json"]);
    }

    /// The record is what an uninstall reads to know what it may delete
    /// and what switch to put back. A refresh must not forget either —
    /// the second run creates nothing, and the file it observes now says
    /// whatever Roost put there.
    #[test]
    fn a_refresh_keeps_what_the_first_install_learned() {
        let mut record = Record::new();
        set_wired(
            &mut record,
            Agent::Codex,
            1,
            &Wired {
                files: vec![PathBuf::from("/home/u/.codex/hooks.json")],
                created: vec![PathBuf::from("/home/u/.codex/hooks.json")],
                codex_features_hooks: Some(PriorFlag::False),
            },
            "local",
            1,
        );
        set_wired(
            &mut record,
            Agent::Codex,
            2,
            &Wired {
                files: vec![PathBuf::from("/home/u/.codex/hooks.json")],
                created: Vec::new(),
                codex_features_hooks: Some(PriorFlag::True),
            },
            "local",
            2,
        );

        let entry = entry(&record, Agent::Codex).unwrap();
        assert_eq!(entry.integration_version, 2);
        assert_eq!(entry.created, ["/home/u/.codex/hooks.json"]);
        assert_eq!(entry.codex_features_hooks, Some(PriorFlag::False));
    }

    /// A record written by a Roost that predates these two fields still
    /// loads; the uninstall it drives simply knows less and so does
    /// less — it deletes nothing and puts no switch back.
    #[test]
    fn a_record_from_an_older_roost_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        std::fs::create_dir_all(home.roost_config_dir()).unwrap();
        std::fs::write(
            home.record_path(),
            br#"{"codex":{"integration_version":1,"files":["/home/u/.codex/hooks.json"],
                "wired_at":1,"noticed":true,"by":"local"}}"#,
        )
        .unwrap();

        let (record, warning) = load(&home).unwrap();
        assert!(warning.is_none());
        let entry = entry(&record, Agent::Codex).unwrap();
        assert!(entry.created.is_empty());
        assert_eq!(entry.codex_features_hooks, None);

        let prior = prior(&home, Agent::Codex).unwrap();
        assert!(!prior.created(std::path::Path::new("/home/u/.codex/hooks.json")));
        assert_eq!(prior.codex_features_hooks, None);
    }

    /// The toast fires once. A refresh must not undo that.
    #[test]
    fn noticed_survives_a_refresh_and_flips_at_most_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());

        let mut record = Record::new();
        set_wired(&mut record, Agent::Codex, 1, &Wired::default(), "local", 1);
        assert!(!entry(&record, Agent::Codex).unwrap().noticed);
        save(&home, &record).unwrap();

        assert!(mark_noticed(&home, &[Agent::Codex]).unwrap());
        assert!(
            !mark_noticed(&home, &[Agent::Codex]).unwrap(),
            "a second mark rewrote the record"
        );

        let (mut record, _) = load(&home).unwrap();
        assert!(entry(&record, Agent::Codex).unwrap().noticed);
        set_wired(&mut record, Agent::Codex, 2, &Wired::default(), "local", 2);
        assert!(
            entry(&record, Agent::Codex).unwrap().noticed,
            "a refresh reset the toast"
        );
    }

    #[test]
    fn an_unreadable_record_is_a_warning_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        std::fs::create_dir_all(home.roost_config_dir()).unwrap();
        std::fs::write(home.record_path(), b"{not json").unwrap();

        let (record, warning) = load(&home).unwrap();
        assert!(record.is_empty());
        assert!(matches!(warning, Some(Warning::UnreadableRecord { .. })));
    }

    #[test]
    fn an_absent_record_is_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let (record, warning) = load(&Home::rooted(dir.path())).unwrap();
        assert!(record.is_empty());
        assert!(warning.is_none());
    }
}
