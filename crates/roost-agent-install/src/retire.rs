//! The retire step: take out the file entries this build no longer
//! needs, once, and only what Roost can prove it wrote.
//!
//! Claude and opencode now take Roost's hooks from the tab environment,
//! and Codex and Grok report over OSC 7501, so their file entries are
//! dead weight that doubles every event. Cursor is the one agent still
//! delivered by file; its entries stay while it is on, and while its
//! opt-in is unanswered.
//!
//! **Proof, not intent.** A handler goes only when its command is one
//! Roost wrote, its keys and values are exactly what a release wrote
//! for that agent and event, its group carries nothing else, and it is
//! the first such handler on its event. A whole file goes only when it
//! is byte-identical to one a release wrote (opencode) or passes its
//! strict ownership check (Grok). Everything else stays and is recorded
//! as a [`Leftover`] with its reason, for doctor and `agent status`.
//!
//! **Never reformat.** A JSON file Roost's printer does not reproduce
//! byte for byte is left untouched: removing one handler must not
//! restyle the user's text, and that cannot be undone.
//!
//! **Back up first, once per version.** Every file the retire changes
//! or deletes is copied under `~/.config/roost/agent-backups/v4/`
//! first, with an `index.json` naming each original. The outcome per
//! agent is recorded in `~/.config/roost/agent-retired.json`, its own
//! file so an older Roost reading `agent-hooks.json` never sees it. A
//! finished agent is not retired again at this version: an entry that
//! comes back later (a synced dotfile, an older build elsewhere) is
//! reported as `came_back`, never chased, so two builds cannot take
//! turns adding and removing it.
//!
//! **Rollback** is per file: copy the backup the index names over the
//! original. There is no restore verb, because a restore would
//! overwrite whatever the user or the agent changed since.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use roost_agent::{Agent, ALL_AGENTS};
use roost_ui_model::config::{agent_default_on, AgentHooks, AgentSwitches, ConfigLock};
use serde::{Deserialize, Serialize};

use crate::error::{AgentError, InstallError};
use crate::home::Home;
use crate::json::Json;
use crate::jsonedit::{self, Opened, RetireRule};
use crate::plan::{apply, FileEdit, Guard, InstallPlan, Intent};
use crate::state;
use crate::write::{self, PRIVATE_MODE};

/// The integration version the retire runs for. The written entries'
/// own shape ([`crate::INTEGRATION_VERSION`]) did not change, so a
/// Cursor install stays current; this is the version that delivers
/// every other agent without a file.
pub const RETIRE_VERSION: u32 = 4;

/// A file Claude rewrote between plan and rename is retried at the
/// next launch, and becomes a leftover after this many misses in a row.
const MAX_MISSES: u32 = 3;

/// Backups kept per original file.
const BACKUPS_KEPT: usize = 3;

/// Why an entry that looks like Roost's was left in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftoverReason {
    /// The command, a key or a value is not what any release wrote.
    Edited,
    /// The handler's group carries a `matcher` or another key.
    Annotated,
    /// A second exact copy on one event.
    Duplicate,
    /// Roost's printer would not reproduce the file byte for byte.
    WouldReformat,
    /// The file does not parse, is not UTF-8, or has an unexpected
    /// shape. Retried at each launch.
    Unreadable,
    /// A file Roost owns by name that fails its ownership check.
    Foreign,
    /// A Codex handler something follows on its event: removing it
    /// would move the user's handlers off their trusted keys.
    Shifted,
    /// The file kept changing between plan and rename.
    ChangedUnderneath,
}

impl std::fmt::Display for LeftoverReason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            LeftoverReason::Edited => "edited",
            LeftoverReason::Annotated => "annotated",
            LeftoverReason::Duplicate => "duplicate",
            LeftoverReason::WouldReformat => "would_reformat",
            LeftoverReason::Unreadable => "unreadable",
            LeftoverReason::Foreign => "foreign",
            LeftoverReason::Shifted => "shifted",
            LeftoverReason::ChangedUnderneath => "changed_underneath",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leftover {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub reason: LeftoverReason,
}

impl Leftover {
    fn new(file: &Path, event: Option<String>, reason: LeftoverReason) -> Leftover {
        Leftover {
            file: file.display().to_string(),
            event,
            reason,
        }
    }
}

/// What the retire did for one agent: the tombstone #485 asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub version: u32,
    /// Unix seconds of the last run.
    pub at: i64,
    /// False while a retry is due: an unreadable file, or one that
    /// changed underneath fewer than [`MAX_MISSES`] times.
    pub complete: bool,
    #[serde(default)]
    pub misses: u32,
    /// Files changed or deleted.
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub backups: Vec<String>,
    #[serde(default)]
    pub leftovers: Vec<Leftover>,
    /// False until the UI has shown the one-time toast. Only meaningful
    /// when something was changed.
    #[serde(default)]
    pub noticed: bool,
}

impl Retired {
    pub fn finished(&self) -> bool {
        self.complete && self.version >= RETIRE_VERSION
    }
}

/// Agent name to its retire.
pub type Tombstones = BTreeMap<String, Retired>;

pub fn tombstone_path(home: &Home) -> PathBuf {
    home.roost_config_dir().join("agent-retired.json")
}

pub fn backups_dir(home: &Home) -> PathBuf {
    home.roost_config_dir()
        .join("agent-backups")
        .join(format!("v{RETIRE_VERSION}"))
}

/// The tombstones, or none. An unreadable file reads as none: the
/// retire re-runs, and what it finds is still only what it can prove.
pub fn load(home: &Home) -> Result<Tombstones, InstallError> {
    let path = tombstone_path(home);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tombstones::new()),
        Err(e) => Err(InstallError::io(&path, e)),
    }
}

fn save(home: &Home, tombstones: &Tombstones) -> Result<bool, InstallError> {
    save_json(&tombstone_path(home), tombstones)
}

/// Write `value` as pretty JSON, private, unless the file already says
/// exactly that.
fn save_json(path: &Path, value: &impl Serialize) -> Result<bool, InstallError> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| InstallError::io(path, std::io::Error::other(e)))?;
    text.push('\n');
    if std::fs::read(path).ok().as_deref() == Some(text.as_bytes()) {
        return Ok(false);
    }
    write::write_atomic(path, text.as_bytes(), PRIVATE_MODE)?;
    Ok(true)
}

/// Whether this build still delivers `agent` by file, which is the only
/// thing that makes its entries needed. Cursor's stay while it is on,
/// and while nobody has answered its opt-in, so a "yes" does not churn
/// the file.
pub fn needed(agent: Agent, switches: &AgentSwitches) -> bool {
    agent == Agent::Cursor && (switches.on(agent) || !answered(switches))
}

/// Whether the user has given any answer: either key set. Until then
/// Cursor's opt-in is the one question still open.
pub fn answered(switches: &AgentSwitches) -> bool {
    switches.integration.is_some() || !matches!(switches.legacy, AgentHooks::Ask)
}

/// What one agent's strict plan found.
#[derive(Debug, Default)]
pub struct RetirePlan {
    pub edits: Vec<FileEdit>,
    /// What `edits` takes out, as `(file, event)`; `None` for a whole
    /// file. A trust key or flag that goes with a handler is not listed.
    pub removes: Vec<(String, Option<String>)>,
    pub leftovers: Vec<Leftover>,
    /// A file could not be read; try again at the next launch.
    pub retry: bool,
}

impl RetirePlan {
    pub(crate) fn unreadable(path: &Path) -> RetirePlan {
        RetirePlan {
            leftovers: vec![Leftover::new(path, None, LeftoverReason::Unreadable)],
            retry: true,
            ..RetirePlan::default()
        }
    }
}

/// A handler exactly as Claude's and Cursor's writer printed it in every
/// release: `type`, `command` and a ten-second `timeout`, nothing else.
pub(crate) fn fits_plain(_event: &str, handler: &Json) -> bool {
    has_exactly(handler, &["type", "command", "timeout"])
        && handler.get("type").and_then(Json::as_str) == Some("command")
        && number_is(handler.get("timeout"), crate::command::HOOK_TIMEOUT_SECS)
}

/// The object has exactly these keys, in any order: a tool that
/// rewrites the file may reorder them without a person touching it.
pub(crate) fn has_exactly(value: &Json, keys: &[&str]) -> bool {
    value.as_object().is_some_and(|entries| {
        entries.len() == keys.len() && entries.iter().all(|(key, _)| keys.contains(&key.as_str()))
    })
}

pub(crate) fn number_is(value: Option<&Json>, want: u64) -> bool {
    matches!(value, Some(Json::Number(n)) if n.as_str() == want.to_string())
}

/// One grouped or flat JSON hooks file through the strict rule. The
/// document after the removal comes back too, `None` when the file was
/// left untouched for its own sake.
pub(crate) fn plan_json(
    agent: Agent,
    path: &Path,
    rule: &RetireRule<'_>,
    may_delete: bool,
    tidy: &dyn Fn(&mut Json),
) -> Result<(RetirePlan, Option<Json>), InstallError> {
    let mut file = match jsonedit::open(path, PRIVATE_MODE)? {
        Opened::Skip(_) => return Ok((RetirePlan::unreadable(path), None)),
        Opened::Ready(file) => file,
    };
    if !file.existed() {
        return Ok((RetirePlan::default(), Some(file.doc)));
    }
    let (removed, left) = match jsonedit::retire(&mut file.doc, path, agent, rule) {
        Ok(found) => found,
        Err(_) => return Ok((RetirePlan::unreadable(path), None)),
    };
    let mut plan = RetirePlan {
        leftovers: left
            .into_iter()
            .map(|(event, reason)| Leftover::new(path, Some(event), reason))
            .collect(),
        ..RetirePlan::default()
    };
    if removed.is_empty() {
        return Ok((plan, Some(file.doc)));
    }
    if !file.round_trips() {
        plan.leftovers.extend(
            removed
                .into_iter()
                .map(|event| Leftover::new(path, Some(event), LeftoverReason::WouldReformat)),
        );
        return Ok((plan, None));
    }
    plan.removes = removed
        .into_iter()
        .map(|event| (path.display().to_string(), Some(event)))
        .collect();
    tidy(&mut file.doc);
    let after = file.doc.clone();
    plan.edits.extend(file.finish_after_removal(may_delete));
    Ok((plan, Some(after)))
}

/// The strict plan for one agent in one home.
pub fn plan_retire(agent: Agent, home: &Home) -> Result<RetirePlan, InstallError> {
    match agent {
        Agent::Claude => crate::claude::plan_retire(home, &state::prior(home, agent)?),
        Agent::Codex => crate::codex::plan_retire(home, &state::prior(home, agent)?),
        Agent::Grok => crate::grok::plan_retire(home),
        Agent::Cursor => crate::cursor::plan_retire(home, &state::prior(home, agent)?),
        Agent::Opencode => crate::opencode::plan_retire(home),
    }
}

/// What [`retire`] did.
#[derive(Debug, Default)]
pub struct RetireOutcome {
    /// An older `roost-session` serves this machine; nothing was done.
    pub deferred: bool,
    /// Agents whose entries this run changed.
    pub retired: Vec<Agent>,
    pub leftovers: Vec<(Agent, Leftover)>,
    pub errors: Vec<AgentError>,
    /// Agents whose retire changed something and has not been announced:
    /// the toast list, flipped by [`mark_noticed`] once it is shown.
    pub unnoticed: Vec<Agent>,
    /// `config.conf` could not take the carry-over. The removals and the
    /// tombstones stand; the keys are written at the next run.
    pub key_error: Option<InstallError>,
    /// This run carried a legacy `agent-hooks = off` over to
    /// `agent-integration = off`. It happens once, so it is what the
    /// one-time "they stay off" notice keys on.
    pub carried_off: bool,
    pub wrote: bool,
}

/// Retire every agent's no-longer-needed entries, once per version.
///
/// `defer` is the caller saying an older `roost-session` still serves
/// this machine: its tabs get no env delivery, so their file entries are
/// still needed, and the retire waits for that session's own v4 start.
pub fn retire(home: &Home, guard: Guard, defer: bool) -> Result<RetireOutcome, InstallError> {
    guard.check()?;
    if defer {
        // The session's own start may already have retired: its notice
        // is still this machine's to show.
        return Ok(RetireOutcome {
            deferred: true,
            unnoticed: unnoticed(&load(home)?),
            ..RetireOutcome::default()
        });
    }
    let lock = home.config_lock()?;
    retire_locked(home, guard, &lock, state::now_secs())
}

fn retire_locked(
    home: &Home,
    guard: Guard,
    lock: &ConfigLock,
    now: i64,
) -> Result<RetireOutcome, InstallError> {
    let switches = switches(home)?;
    let mut tombstones = load(home)?;
    let (mut record, _) = state::load(home)?;
    let mut outcome = RetireOutcome::default();

    for agent in ALL_AGENTS {
        if needed(agent, &switches) {
            // A file agent turned back on: whatever it is turned off
            // again, that is a new removal and is retired again.
            outcome.wrote |= tombstones.remove(agent.source()).is_some();
            continue;
        }
        let previous = tombstones.get(agent.source()).cloned();
        if previous.as_ref().is_some_and(Retired::finished) {
            continue;
        }
        let mut entry = Retired {
            version: RETIRE_VERSION,
            at: now,
            complete: true,
            misses: 0,
            changed: previous
                .as_ref()
                .map(|p| p.changed.clone())
                .unwrap_or_default(),
            backups: previous
                .as_ref()
                .map(|p| p.backups.clone())
                .unwrap_or_default(),
            leftovers: Vec::new(),
            noticed: previous.as_ref().is_some_and(|p| p.noticed),
        };
        let mut missed: Vec<PathBuf> = Vec::new();
        let mut changed_now = false;

        for at in crate::ensure::unwire_homes(home, agent, &record) {
            let plan = match plan_retire(agent, &at) {
                Ok(plan) => plan,
                Err(error) => {
                    outcome.errors.push(AgentError { agent, error });
                    entry.complete = false;
                    continue;
                }
            };
            entry.complete &= !plan.retry;
            entry.leftovers.extend(plan.leftovers);
            if plan.edits.is_empty() {
                continue;
            }
            let files: Vec<PathBuf> = plan.edits.iter().map(|e| e.path().to_path_buf()).collect();
            match backup(home, agent, &plan.edits, now) {
                Ok(paths) => entry.backups.extend(paths),
                Err(error) => {
                    outcome.errors.push(AgentError { agent, error });
                    entry.complete = false;
                    continue;
                }
            }
            let install = InstallPlan {
                agent,
                intent: Intent::Uninstall,
                edits: plan.edits,
                skipped: None,
                warnings: Vec::new(),
                files: Vec::new(),
            };
            match apply(&install, guard) {
                Ok(_) => {
                    changed_now = true;
                    for file in files {
                        let file = file.display().to_string();
                        if !entry.changed.contains(&file) {
                            entry.changed.push(file);
                        }
                    }
                }
                Err(InstallError::ChangedUnderneath { .. }) => missed.extend(files),
                Err(error) => {
                    outcome.errors.push(AgentError { agent, error });
                    entry.complete = false;
                }
            }
        }

        if !missed.is_empty() {
            entry.misses = previous.as_ref().map_or(0, |p| p.misses) + 1;
            if entry.misses >= MAX_MISSES {
                entry.leftovers.extend(
                    missed
                        .iter()
                        .map(|f| Leftover::new(f, None, LeftoverReason::ChangedUnderneath)),
                );
            } else {
                entry.complete = false;
            }
        }
        if changed_now {
            outcome.retired.push(agent);
            outcome.wrote = true;
        }
        // The record's facts (files created, codex's flag pre-image) are
        // what an explicit uninstall of a leftover still needs. A kept
        // entry is never announced as wired: the agent no longer is.
        if entry.complete && entry.leftovers.is_empty() {
            outcome.wrote |= record.remove(agent.source()).is_some();
        } else if let Some(kept) = record.get_mut(agent.source()) {
            kept.noticed = true;
        }
        outcome
            .leftovers
            .extend(entry.leftovers.iter().cloned().map(|l| (agent, l)));
        tombstones.insert(agent.source().to_string(), entry);
    }

    // The tombstones before the keys: a removal recorded as finished is
    // never repeated, where one whose key write failed after it would
    // be redone, with a fresh backup, at every launch.
    outcome.wrote |= state::save(home, &record)?;
    outcome.wrote |= save(home, &tombstones)?;
    match carry_over(home, &switches, lock) {
        Ok(carried) => {
            outcome.wrote |= carried.wrote;
            outcome.carried_off = carried.from_off;
        }
        Err(error) => outcome.key_error = Some(error),
    }
    outcome.unnoticed = unnoticed(&tombstones);
    Ok(outcome)
}

/// The two agent keys in this home's `config.conf`; an absent file
/// answers nothing.
pub fn switches(home: &Home) -> Result<AgentSwitches, InstallError> {
    match std::fs::read_to_string(home.config_path()) {
        Ok(text) => Ok(AgentSwitches::read(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AgentSwitches::default()),
        Err(e) => Err(InstallError::io(home.config_path(), e)),
    }
}

fn unnoticed(tombstones: &Tombstones) -> Vec<Agent> {
    ALL_AGENTS
        .into_iter()
        .filter(|agent| {
            tombstones
                .get(agent.source())
                .is_some_and(|t| !t.noticed && !t.changed.is_empty())
        })
        .collect()
}

/// Flip `noticed` for the named agents' retire, after the toast is shown.
pub fn mark_noticed(home: &Home, agents: &[Agent]) -> Result<bool, InstallError> {
    let _lock = home.config_lock()?;
    let mut tombstones = load(home)?;
    let mut changed = false;
    for agent in agents {
        if let Some(entry) = tombstones.get_mut(agent.source()) {
            changed |= !std::mem::replace(&mut entry.noticed, true);
        }
    }
    if !changed {
        return Ok(false);
    }
    save(home, &tombstones)
}

/// Write `agent-integration` from an answered `agent-hooks`, then narrow
/// `agent-hooks` to the agents this build still delivers by file.
///
/// The narrowed key is a shadow for older builds and for this build's
/// file `ensure`: both wire only what it lists, so neither puts back
/// what the retire just took out. An unanswered key stays unanswered.
fn carry_over(
    home: &Home,
    switches: &AgentSwitches,
    lock: &ConfigLock,
) -> Result<CarriedOver, InstallError> {
    if !answered(switches) {
        return Ok(CarriedOver::default());
    }
    let path = home.config_path();
    let set = |key: &str, value: &str| {
        roost_ui_model::config::set_key_locked(lock, path, key, value)
            .map_err(|e| InstallError::io(path, e))
    };
    let mut carried = CarriedOver::default();
    if switches.integration.is_none() {
        set("agent-integration", &integration_from(&switches.legacy))?;
        carried.wrote = true;
        carried.from_off = switches.legacy == AgentHooks::Off;
    }
    let shadow = if switches.on(Agent::Cursor) {
        AgentHooks::allow([Agent::Cursor.source()])
    } else {
        AgentHooks::Off
    };
    if switches.legacy != shadow {
        let value = shadow
            .to_config_value()
            .expect("an allow-list or off always has a spelling");
        set("agent-hooks", &value)?;
        carried.wrote = true;
    }
    Ok(carried)
}

#[derive(Debug, Default)]
struct CarriedOver {
    wrote: bool,
    from_off: bool,
}

/// An answered `agent-hooks` spelled as `agent-integration`: what it
/// listed is on and every default-on agent it left out is off, so the
/// carry-over changes nothing about which agents are on.
pub(crate) fn integration_from(legacy: &AgentHooks) -> String {
    let AgentHooks::Allow { agents, unknown } = legacy else {
        return "off".to_string();
    };
    let mut tokens: Vec<String> = ALL_AGENTS
        .into_iter()
        .filter_map(|agent| {
            let listed = agents.iter().any(|name| name == agent.source());
            match (listed, agent_default_on(agent)) {
                (true, _) => Some(agent.source().to_string()),
                (false, true) => Some(format!("-{}", agent.source())),
                (false, false) => None,
            }
        })
        .collect();
    tokens.extend(unknown.iter().cloned());
    if tokens.is_empty() {
        return "off".to_string();
    }
    tokens.join(", ")
}

/// One entry in `index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEntry {
    pub backup: String,
    pub original: String,
    pub at: i64,
}

/// Copy every file `edits` will change or delete into the backup
/// folder, before any of them is touched, and keep [`BACKUPS_KEPT`] per
/// original (see [`prune`]).
fn backup(
    home: &Home,
    agent: Agent,
    edits: &[FileEdit],
    now: i64,
) -> Result<Vec<String>, InstallError> {
    let root = backups_dir(home);
    let dir = root.join(agent.source());
    private_dir(&dir)?;
    for parent in [root.as_path(), root.parent().unwrap_or(&root)] {
        set_mode(parent, 0o700)?;
    }
    let index_path = root.join("index.json");
    let mut index: Vec<BackupEntry> = match std::fs::read(&index_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(InstallError::io(&index_path, e)),
    };

    let mut made = Vec::new();
    for edit in edits {
        let Some(bytes) = edit.image.bytes.as_deref() else {
            continue;
        };
        let original = edit.path();
        let stem = format!(
            "{}-{}",
            hex8(&write::digest(original.as_os_str().as_encoded_bytes())),
            original
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let mut name = format!("{stem}.{now}");
        let mut n = 1;
        while dir.join(&name).exists() {
            name = format!("{stem}.{now}-{n}");
            n += 1;
        }
        let path = dir.join(&name);
        write::write_atomic(&path, bytes, PRIVATE_MODE)?;
        let path = path.display().to_string();
        index.push(BackupEntry {
            backup: path.clone(),
            original: original.display().to_string(),
            at: now,
        });
        prune(&mut index, &original.display().to_string())?;
        made.push(path);
    }

    save_json(&index_path, &index)?;
    Ok(made)
}

/// Keep the first backup of `original` and the newest of the rest, up
/// to [`BACKUPS_KEPT`] in all. The first is the file before any retire
/// touched it: the one a rollback wants, and the one a retire that
/// repeats itself would otherwise push out.
fn prune(index: &mut Vec<BackupEntry>, original: &str) -> Result<(), InstallError> {
    let mine: Vec<usize> = index
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.original == original)
        .map(|(i, _)| i)
        .collect();
    if mine.len() <= BACKUPS_KEPT {
        return Ok(());
    }
    let drop = &mine[1..mine.len() - (BACKUPS_KEPT - 1)];
    for &i in drop.iter().rev() {
        let entry = index.remove(i);
        write::remove(Path::new(&entry.backup))?;
    }
    Ok(())
}

fn private_dir(dir: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| InstallError::io(dir, e))?;
    set_mode(dir, 0o700)
}

fn set_mode(dir: &Path, mode: u32) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
        .map_err(|e| InstallError::io(dir, e))
}

fn hex8(digest: &[u8; 32]) -> String {
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Read-only: an agent whose retire finished but whose files carry an
/// exact Roost entry again. Reported, never removed again at this
/// version.
///
/// Not an agent this build delivers by file again (Cursor turned back
/// on wrote its entries on purpose), and not an exact copy the retire
/// itself left as a `duplicate` or `shifted` leftover: that one is
/// already reported, and no older Roost put it back.
pub fn came_back(home: &Home, agent: Agent, tombstones: &Tombstones) -> bool {
    let Some(retired) = tombstones.get(agent.source()).filter(|t| t.finished()) else {
        return false;
    };
    if needed(agent, &switches(home).unwrap_or_default()) {
        return false;
    }
    let left_here = |file: &str, event: &Option<String>| {
        retired.leftovers.iter().any(|l| {
            matches!(
                l.reason,
                LeftoverReason::Duplicate | LeftoverReason::Shifted
            ) && l.file == file
                && l.event == *event
        })
    };
    plan_retire(agent, home).is_ok_and(|plan| {
        plan.removes
            .iter()
            .any(|(file, event)| !left_here(file, event))
    })
}
