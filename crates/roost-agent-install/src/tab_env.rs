//! The agent integration's half of a tab's environment: Claude's
//! `CLAUDE_CODE_PLUGIN_DIRS` and opencode's `OPENCODE_CONFIG_CONTENT`,
//! naming the files [`crate::env_delivery`] materializes. The UI and
//! `roost-session` each start one [`TabEnv`] and hand
//! [`TabEnv::apply`] to `roost_engine::process::set_tab_env`, so an
//! in-process tab and a session tab get the same variables.
//!
//! Both variables are the user's as much as Roost's, so Roost's entry is
//! merged in, never set over theirs: appended after a user's plugin
//! folders, added to a user's `plugin` list. A value inherited from an
//! outer Roost (a session or a dev UI started inside a Roost tab) names
//! that Roost's folder, so every Roost entry is stripped first, whether
//! this tab gets one or not; the merge then adds exactly one, this
//! process's.
//!
//! Which agents get one is `config.conf`'s `agent-integration` (or the
//! legacy `agent-hooks` answer), read at each spawn so a switch flipped
//! in the settings reaches the next tab. Claude is skipped where an
//! organization's `disableSideloadFlags` policy would make it exit at
//! start with the variable set: a local managed-settings file says so at
//! once, and `claude plugin list --json` run with the variable, off the
//! UI thread and cached, catches what the files do not (MDM, a cached
//! server policy). The last verdict is kept in [`POLICY_RECORD`] for
//! doctor.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use roost_agent::Agent;
use roost_ipc::paths::BundleProfile;
use roost_ui_model::config::AgentSwitches;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::env_delivery::{self, is_roost_claude_plugin_dir, is_roost_opencode_plugin};

pub const CLAUDE_PLUGIN_DIRS: &str = "CLAUDE_CODE_PLUGIN_DIRS";
pub const OPENCODE_CONFIG_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";

/// The file, in the integration directory, that holds the last Claude
/// policy verdict ([`PolicyRecord`]).
pub const POLICY_RECORD: &str = "claude-policy.json";

/// Where Claude reads machine-wide managed settings from (a
/// `managed-settings.json` and a `managed-settings.d/` of drop-ins).
#[cfg(target_os = "macos")]
const MANAGED_SETTINGS_DIRS: &[&str] = &["/Library/Application Support/ClaudeCode"];
#[cfg(not(target_os = "macos"))]
const MANAGED_SETTINGS_DIRS: &[&str] = &["/etc/claude-code"];

/// A policy can arrive after the first probe (a server policy is fetched
/// by a later Claude run), so a verdict this old is probed again.
const PROBE_TTL: Duration = Duration::from_secs(60 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `current` with every Roost plugin folder taken out and `ours`
/// appended. `None` unsets the variable. A value with no Roost entry and
/// nothing to add comes back byte for byte.
pub fn merge_plugin_dirs(current: Option<&OsStr>, ours: Option<&Path>) -> Option<OsString> {
    let entries: Vec<&[u8]> = current
        .map(|value| value.as_bytes().split(|&b| b == b':').collect())
        .unwrap_or_default();
    let kept: Vec<&[u8]> = entries
        .iter()
        .copied()
        .filter(|entry| !is_roost_claude_plugin_dir(Path::new(OsStr::from_bytes(entry))))
        .collect();
    if ours.is_none() && kept.len() == entries.len() {
        return current.map(OsStr::to_os_string);
    }
    let mut parts: Vec<&[u8]> = kept.into_iter().filter(|entry| !entry.is_empty()).collect();
    if let Some(ours) = ours {
        parts.push(ours.as_os_str().as_bytes());
    }
    (!parts.is_empty()).then(|| OsString::from_vec(parts.join(&b':')))
}

/// `current`, a JSON object of opencode config, with every Roost plugin
/// taken out of its `plugin` list and `ours` added. `None` unsets the
/// variable. A value with no Roost plugin and nothing to add comes back
/// byte for byte; one that is not a JSON object with a `plugin` array (or
/// none) is an error, and the caller leaves it alone.
pub fn merge_opencode_content(
    current: Option<&OsStr>,
    ours: Option<&Path>,
) -> Result<Option<OsString>, String> {
    let mut config = match current {
        None => Value::Object(serde_json::Map::new()),
        Some(text) => {
            serde_json::from_slice(text.as_bytes()).map_err(|error| format!("not JSON: {error}"))?
        }
    };
    let object = config.as_object_mut().ok_or("not a JSON object")?;
    let mut plugins = match object.remove("plugin") {
        None => Vec::new(),
        Some(Value::Array(plugins)) => plugins,
        Some(_) => return Err("`plugin` is not an array".into()),
    };
    let before = plugins.len();
    plugins.retain(|plugin| !plugin.as_str().is_some_and(names_roost_plugin));
    if ours.is_none() && plugins.len() == before {
        return Ok(current.map(OsStr::to_os_string));
    }
    if let Some(ours) = ours {
        plugins.push(Value::String(file_url(ours)));
    }
    if !plugins.is_empty() {
        object.insert("plugin".into(), Value::Array(plugins));
    }
    Ok((!object.is_empty()).then(|| config.to_string().into()))
}

/// Is a `plugin` entry a `file://` URL or path naming a Roost plugin?
/// The screen reads only the path's last components, which Roost names
/// in plain ASCII, so a percent-encoded URL needs no decoding first.
fn names_roost_plugin(entry: &str) -> bool {
    let path = entry.strip_prefix("file://").unwrap_or(entry);
    is_roost_opencode_plugin(Path::new(path))
}

/// `path` as a `file://` URL. Every byte but an unreserved one and `/` is
/// percent-encoded: the Mac's cache dir has a space, and a home may hold
/// anything.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// Whether Claude may be handed a plugin folder on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "kebab-case")]
pub enum ClaudePolicy {
    /// No probe has answered yet.
    Unchecked,
    Allowed,
    /// No `claude` to ask. Nothing would exit, so Claude is injected.
    NotInstalled,
    /// `disableSideloadFlags` is set; the detail says where it was found.
    Blocked(String),
    /// The probe gave no clear answer; Claude is injected.
    Unknown(String),
}

impl ClaudePolicy {
    pub fn blocks(&self) -> bool {
        matches!(self, ClaudePolicy::Blocked(_))
    }
}

/// What [`POLICY_RECORD`] holds, for doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    pub policy: ClaudePolicy,
    /// A cached server-managed settings file exists. Not proof of a
    /// policy, but where one that the probe has not seen yet would come
    /// from.
    pub remote_settings_cached: bool,
}

/// The last verdict a serving process recorded under `dir`.
pub fn read_policy_record(dir: &Path) -> Option<PolicyRecord> {
    serde_json::from_slice(&fs::read(dir.join(POLICY_RECORD)).ok()?).ok()
}

/// The managed-settings file under `roots` that sets
/// `disableSideloadFlags`, applying the base file and then its drop-ins
/// in name order, as Claude does: a later file's value wins.
pub fn managed_block(roots: &[PathBuf]) -> Option<PathBuf> {
    let mut blocking = None;
    for root in roots {
        let mut files = vec![root.join("managed-settings.json")];
        if let Ok(entries) = fs::read_dir(root.join("managed-settings.d")) {
            let mut dropins: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension() == Some(OsStr::new("json")))
                .collect();
            dropins.sort();
            files.extend(dropins);
        }
        for file in files {
            let Some(settings) = fs::read(&file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                continue;
            };
            match settings.get("disableSideloadFlags") {
                Some(Value::Bool(true)) => blocking = Some(file),
                Some(Value::Bool(false)) => blocking = None,
                _ => {}
            }
        }
    }
    blocking
}

/// Run `claude plugin list --json` with `plugin_dir` named in the
/// variable, as the serving process. Under the policy Claude exits 1 and
/// names `disableSideloadFlags` on stderr, in about 0.1 s; `--version`
/// exits before the check, so it cannot stand in. Stdin is closed and the
/// updater off, so nothing can be answered or installed.
pub fn probe_claude(claude: &Path, plugin_dir: &Path, timeout: Duration) -> ClaudePolicy {
    let spawned = Command::new(claude)
        .args(["plugin", "list", "--json"])
        .env(CLAUDE_PLUGIN_DIRS, plugin_dir)
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return ClaudePolicy::Unknown(format!("could not run {}: {error}", claude.display()))
        }
    };
    // Read on a thread of its own: a descendant that keeps stderr open
    // must not hold the verdict past the timeout.
    let (sent, stderr) = mpsc::channel();
    if let Some(pipe) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.take(64 * 1024).read_to_string(&mut text);
            let _ = sent.send(text);
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return ClaudePolicy::Unknown(format!("no answer in {}s", timeout.as_secs()));
            }
            Err(error) => return ClaudePolicy::Unknown(format!("wait: {error}")),
        }
    };
    let stderr = stderr
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_default();
    match status.code() {
        Some(0) => ClaudePolicy::Allowed,
        Some(1) if stderr.contains("disableSideloadFlags") => ClaudePolicy::Blocked(format!(
            "`claude plugin list` refused the plugin folder: {}",
            stderr.lines().next().unwrap_or_default().trim()
        )),
        code => ClaudePolicy::Unknown(format!("`claude plugin list` exited {code:?}")),
    }
}

/// `claude` as a login shell would find it: on `PATH`, else where its
/// installers put it, since a serving process started from a desktop
/// session may not have the user's `~/.local/bin` on its `PATH`.
pub fn find_claude(path: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let on_path = path.into_iter().flat_map(std::env::split_paths);
    let fallback = home
        .into_iter()
        .flat_map(|home| [home.join(".local/bin"), home.join(".claude/local")])
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    on_path
        .chain(fallback)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("claude"))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

struct Probe {
    policy: ClaudePolicy,
    at: Option<Instant>,
    running: bool,
}

struct Inner {
    dir: PathBuf,
    config: Option<PathBuf>,
    managed_roots: Vec<PathBuf>,
    claude: Option<PathBuf>,
    remote_settings: Option<PathBuf>,
    probe: Mutex<Probe>,
}

/// One serving process's agent variables. Cheap to clone.
#[derive(Clone)]
pub struct TabEnv(Arc<Inner>);

impl TabEnv {
    /// The production instance for `profile`: files under its
    /// [`agent_integration_dir`](BundleProfile::agent_integration_dir),
    /// switches from this user's `config.conf`, the machine's
    /// managed-settings dirs, and `claude` from this process's
    /// environment. Starts the policy probe on a thread of its own, so
    /// call it after any fork. `None`, logged, when the profile has no
    /// integration dir: tabs then get no agent plugins.
    pub fn start(profile: &BundleProfile) -> Option<TabEnv> {
        let dir = profile
            .agent_integration_dir()
            .map_err(|error| tracing::warn!(%error, "no agent integration dir; tabs get no agent plugins"))
            .ok()?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let claude_config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".claude")));
        let tab_env = TabEnv::new(
            dir,
            roost_ui_model::config::config_path(),
            MANAGED_SETTINGS_DIRS.iter().map(PathBuf::from).collect(),
            find_claude(std::env::var_os("PATH").as_deref(), home.as_deref()),
            claude_config.map(|dir| dir.join("remote-settings.json")),
        );
        tab_env.refresh_if_stale();
        Some(tab_env)
    }

    fn new(
        dir: PathBuf,
        config: Option<PathBuf>,
        managed_roots: Vec<PathBuf>,
        claude: Option<PathBuf>,
        remote_settings: Option<PathBuf>,
    ) -> TabEnv {
        TabEnv(Arc::new(Inner {
            dir,
            config,
            managed_roots,
            claude,
            remote_settings,
            probe: Mutex::new(Probe {
                policy: ClaudePolicy::Unchecked,
                at: None,
                running: false,
            }),
        }))
    }

    /// Merge this process's agent variables into a tab's environment.
    /// Blocking file I/O (the config, the managed settings, the
    /// integration files), all small; it runs on the engine's spawn path.
    pub fn apply(&self, env: &mut BTreeMap<OsString, OsString>) {
        let switches = self
            .0
            .config
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|text| AgentSwitches::read(&text))
            .unwrap_or_default();
        let claude = switches.on(Agent::Claude) && !self.claude_blocked();
        let opencode = switches.on(Agent::Opencode);
        let assets = (claude || opencode)
            .then(|| env_delivery::materialize(&self.0.dir))
            .and_then(|made| {
                made.map_err(|error| {
                    tracing::warn!(%error, "agent integration files unavailable; tabs get no agent plugins")
                })
                .ok()
            });

        let plugin_dir = assets
            .as_ref()
            .filter(|_| claude)
            .map(|assets| assets.claude_plugin_dir.as_path())
            .filter(|dir| !dir.as_os_str().as_bytes().contains(&b':'));
        let merged = merge_plugin_dirs(
            env.get(OsStr::new(CLAUDE_PLUGIN_DIRS))
                .map(OsString::as_os_str),
            plugin_dir,
        );
        set(env, CLAUDE_PLUGIN_DIRS, merged);

        let plugin = assets
            .as_ref()
            .filter(|_| opencode)
            .map(|assets| assets.opencode_plugin.as_path());
        match merge_opencode_content(
            env.get(OsStr::new(OPENCODE_CONFIG_CONTENT))
                .map(OsString::as_os_str),
            plugin,
        ) {
            Ok(merged) => set(env, OPENCODE_CONFIG_CONTENT, merged),
            Err(why) => tracing::warn!(
                why,
                "OPENCODE_CONFIG_CONTENT left as inherited; opencode gets no Roost plugin"
            ),
        }
        self.refresh_if_stale();
    }

    /// The probe's last verdict, or the managed-settings file's when one
    /// sets the policy.
    pub fn claude_policy(&self) -> ClaudePolicy {
        if let Some(file) = managed_block(&self.0.managed_roots) {
            return ClaudePolicy::Blocked(format!("disableSideloadFlags in {}", file.display()));
        }
        self.lock().policy.clone()
    }

    fn claude_blocked(&self) -> bool {
        self.claude_policy().blocks()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Probe> {
        self.0
            .probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start a probe on its own thread unless one is running or the last
    /// verdict is fresh.
    fn refresh_if_stale(&self) {
        {
            let mut probe = self.lock();
            if probe.running || probe.at.is_some_and(|at| at.elapsed() < PROBE_TTL) {
                return;
            }
            probe.running = true;
        }
        let tab_env = self.clone();
        let spawned = std::thread::Builder::new()
            .name("roost-claude-policy".into())
            .spawn(move || tab_env.probe_now());
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the Claude policy probe");
            self.lock().running = false;
        }
    }

    fn probe_now(&self) {
        let policy = match (&self.0.claude, env_delivery::materialize(&self.0.dir)) {
            (None, _) => ClaudePolicy::NotInstalled,
            (Some(_), Err(error)) => ClaudePolicy::Unknown(format!("no plugin folder: {error}")),
            (Some(claude), Ok(assets)) => {
                probe_claude(claude, &assets.claude_plugin_dir, PROBE_TIMEOUT)
            }
        };
        match &policy {
            ClaudePolicy::Blocked(why) => {
                tracing::warn!(why, "Claude's managed policy blocks plugin folders; Claude tabs get no Roost plugin")
            }
            ClaudePolicy::Unknown(why) => {
                tracing::info!(why, "Claude policy probe gave no answer; injecting")
            }
            _ => tracing::debug!(?policy, "Claude policy probe"),
        }
        {
            let mut probe = self.lock();
            probe.policy = policy;
            probe.at = Some(Instant::now());
            probe.running = false;
        }
        self.record();
    }

    /// Write the verdict where doctor reads it.
    fn record(&self) {
        let record = PolicyRecord {
            policy: self.claude_policy(),
            remote_settings_cached: self
                .0
                .remote_settings
                .as_ref()
                .is_some_and(|path| path.exists()),
        };
        let staged = self
            .0
            .dir
            .join(format!(".{POLICY_RECORD}-{}", crate::write::nonce()));
        let written = serde_json::to_vec(&record)
            .map_err(std::io::Error::other)
            .and_then(|bytes| fs::write(&staged, bytes))
            .and_then(|()| fs::rename(&staged, self.0.dir.join(POLICY_RECORD)));
        if let Err(error) = written {
            tracing::debug!(%error, "could not record the Claude policy verdict");
        }
    }
}

fn set(env: &mut BTreeMap<OsString, OsString>, name: &str, value: Option<OsString>) {
    match value {
        Some(value) => env.insert(name.into(), value),
        None => env.remove(OsStr::new(name)),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOST_DIR: &str = "/home/u/.cache/roost/agent-integration/0123456789abcdef/roost";
    const OLD_ROOST_DIR: &str =
        "/home/u/.cache/roost-session/agent-integration/fedcba9876543210/roost";
    const OLD_PLUGIN: &str =
        "file:///home/u/.cache/roost/agent-integration/fedcba9876543210/opencode/roost-agent-state.js";

    fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    fn dirs(current: Option<&str>, ours: Option<&str>) -> Option<String> {
        merge_plugin_dirs(current.map(OsStr::new), ours.map(Path::new))
            .map(|value| value.into_string().expect("UTF-8"))
    }

    #[test]
    fn plugin_dirs_append_after_the_users_folders() {
        assert_eq!(dirs(None, Some(ROOST_DIR)).as_deref(), Some(ROOST_DIR));
        assert_eq!(
            dirs(Some("/u/one:~/two"), Some(ROOST_DIR)),
            Some(format!("/u/one:~/two:{ROOST_DIR}"))
        );
    }

    /// An inherited value that already names a Roost folder, this
    /// profile's or another's, comes out with exactly one: the current.
    #[test]
    fn plugin_dirs_hold_exactly_one_roost_folder() {
        let inherited = format!("{OLD_ROOST_DIR}:/u/one:{ROOST_DIR}/");
        assert_eq!(
            dirs(Some(&inherited), Some(ROOST_DIR)),
            Some(format!("/u/one:{ROOST_DIR}"))
        );
        assert_eq!(dirs(Some(&inherited), None).as_deref(), Some("/u/one"));
        assert_eq!(dirs(Some(OLD_ROOST_DIR), None), None);
    }

    #[test]
    fn plugin_dirs_with_nothing_of_roosts_are_left_byte_for_byte() {
        assert_eq!(
            dirs(Some("/u/one::/u/two:"), None).as_deref(),
            Some("/u/one::/u/two:")
        );
        assert_eq!(dirs(None, None), None);
    }

    fn content(current: Option<&str>, ours: Option<&str>) -> Result<Option<Value>, String> {
        merge_opencode_content(current.map(OsStr::new), ours.map(Path::new)).map(|value| {
            value.map(|text| serde_json::from_slice(text.as_bytes()).expect("JSON out"))
        })
    }

    #[test]
    fn opencode_content_carries_only_a_plugin_entry() {
        let ours = "/Users/u/Library/Caches/Roost/agent-integration/0123456789abcdef/opencode/roost-agent-state.js";
        assert_eq!(
            content(None, Some(ours)),
            Ok(Some(serde_json::json!({
                "plugin": ["file:///Users/u/Library/Caches/Roost/agent-integration/0123456789abcdef/opencode/roost-agent-state.js"]
            })))
        );
    }

    #[test]
    fn opencode_content_merges_into_the_users_value() {
        let user = format!(r#"{{"model":"m","plugin":["user-plugin","{OLD_PLUGIN}"]}}"#);
        let ours = "/h/a b/é/agent-integration/0123456789abcdef/opencode/roost-agent-state.js";
        let merged = content(Some(&user), Some(ours))
            .expect("merges")
            .expect("a value");
        assert_eq!(merged["model"], "m");
        assert_eq!(
            merged["plugin"],
            serde_json::json!([
                "user-plugin",
                "file:///h/a%20b/%C3%A9/agent-integration/0123456789abcdef/opencode/roost-agent-state.js"
            ])
        );
        // The encoded URL is recognised as Roost's on the way back.
        let again = merged.to_string();
        let stripped = content(Some(&again), None)
            .expect("merges")
            .expect("a value");
        assert_eq!(stripped["plugin"], serde_json::json!(["user-plugin"]));
    }

    #[test]
    fn opencode_content_drops_a_value_left_with_nothing() {
        let inherited = format!(r#"{{"plugin":["{OLD_PLUGIN}"]}}"#);
        assert_eq!(content(Some(&inherited), None), Ok(None));
        assert_eq!(
            merge_opencode_content(Some(OsStr::new("{\"plugin\": [ ]}")), None),
            Ok(Some(os("{\"plugin\": [ ]}"))),
            "nothing of Roost's: untouched"
        );
    }

    #[test]
    fn an_opencode_value_roost_cannot_read_is_an_error() {
        for bad in ["{not json", "[1]", r#"{"plugin":"x"}"#] {
            assert!(content(Some(bad), Some("/x/roost.js")).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_managed_drop_in_decides_after_the_base_file() {
        let root = tempfile::tempdir().expect("root");
        let roots = vec![root.path().to_path_buf()];
        assert_eq!(managed_block(&roots), None);
        let base = root.path().join("managed-settings.json");
        fs::write(&base, r#"{"disableSideloadFlags": true}"#).unwrap();
        assert_eq!(managed_block(&roots), Some(base.clone()));
        fs::create_dir(root.path().join("managed-settings.d")).unwrap();
        fs::write(
            root.path().join("managed-settings.d/10-allow.json"),
            r#"{"disableSideloadFlags": false}"#,
        )
        .unwrap();
        assert_eq!(managed_block(&roots), None);
        let dropin = root.path().join("managed-settings.d/20-deny.json");
        fs::write(&dropin, r#"{"disableSideloadFlags": true}"#).unwrap();
        assert_eq!(managed_block(&roots), Some(dropin));
    }

    /// A stand-in `claude` that runs `body`, through the committed fixture
    /// (see its header for the ETXTBSY race a written script would run).
    fn fake_claude(dir: &Path, body: &str) -> PathBuf {
        fs::write(dir.join("claude.conf"), body).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/roosttest/fixtures/fake-roost-session.sh")
            .canonicalize()
            .expect("the fake-roost-session fixture must exist");
        let path = dir.join("claude");
        std::os::unix::fs::symlink(fixture, &path).unwrap();
        path
    }

    #[test]
    fn the_probe_reads_claudes_three_answers() {
        let plugin = Path::new(ROOST_DIR);
        let cases = [
            // The variable must reach Claude, or the probe proves nothing.
            (r#"[ "$CLAUDE_CODE_PLUGIN_DIRS" = "/home/u/.cache/roost/agent-integration/0123456789abcdef/roost" ] && [ "$DISABLE_AUTOUPDATER" = 1 ] && exit 0; exit 3"#, "allowed"),
            ("echo 'CLAUDE_CODE_PLUGIN_DIRS is disabled by your organization (disableSideloadFlags).' >&2; exit 1", "blocked"),
            ("echo 'some other failure' >&2; exit 1", "unknown"),
            ("sleep 5", "unknown"),
        ];
        for (body, want) in cases {
            let dir = tempfile::tempdir().expect("dir");
            let policy = probe_claude(
                &fake_claude(dir.path(), body),
                plugin,
                Duration::from_millis(500),
            );
            let got = match policy {
                ClaudePolicy::Allowed => "allowed",
                ClaudePolicy::Blocked(_) => "blocked",
                ClaudePolicy::Unknown(_) => "unknown",
                _ => "other",
            };
            assert_eq!(got, want, "{body}: {policy:?}");
        }
    }

    #[test]
    fn claude_is_found_off_path_where_its_installer_puts_it() {
        let home = tempfile::tempdir().expect("home");
        assert_eq!(find_claude(Some(OsStr::new("")), Some(home.path())), None);
        fs::create_dir_all(home.path().join(".local/bin")).unwrap();
        let claude = fake_claude(&home.path().join(".local/bin"), "exit 0");
        assert_eq!(
            find_claude(Some(OsStr::new("/nowhere")), Some(home.path())),
            Some(claude)
        );
    }

    struct World {
        _root: tempfile::TempDir,
        config: PathBuf,
        managed: PathBuf,
        tab_env: TabEnv,
    }

    /// A [`TabEnv`] over a temp root whose probe has already answered
    /// `policy`, so no probe thread starts.
    fn world(policy: ClaudePolicy) -> World {
        let root = tempfile::tempdir().expect("root");
        let config = root.path().join("config.conf");
        let managed = root.path().join("managed");
        let tab_env = TabEnv::new(
            root.path().join("cache/agent-integration"),
            Some(config.clone()),
            vec![managed.clone()],
            None,
            None,
        );
        {
            let mut probe = tab_env.lock();
            probe.policy = policy;
            probe.at = Some(Instant::now());
        }
        World {
            _root: root,
            config,
            managed,
            tab_env,
        }
    }

    impl World {
        fn tab(&self, config: &str, inherited: &[(&str, &str)]) -> BTreeMap<String, String> {
            fs::write(&self.config, config).unwrap();
            let mut env: BTreeMap<OsString, OsString> = inherited
                .iter()
                .map(|(name, value)| (os(name), os(value)))
                .collect();
            self.tab_env.apply(&mut env);
            env.into_iter()
                .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
                .collect()
        }
    }

    fn roost_dirs(env: &BTreeMap<String, String>) -> Vec<&str> {
        env.get(CLAUDE_PLUGIN_DIRS)
            .map(|value| {
                value
                    .split(':')
                    .filter(|dir| is_roost_claude_plugin_dir(Path::new(dir)))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn by_default_a_tab_gets_both_variables_beside_the_users() {
        let world = world(ClaudePolicy::Allowed);
        let env = world.tab(
            "",
            &[
                (CLAUDE_PLUGIN_DIRS, "/u/one"),
                (OPENCODE_CONFIG_CONTENT, r#"{"plugin":["mine"]}"#),
            ],
        );
        let dirs: Vec<&str> = env[CLAUDE_PLUGIN_DIRS].split(':').collect();
        assert_eq!(dirs[0], "/u/one");
        assert!(
            Path::new(dirs[1])
                .join(".claude-plugin/plugin.json")
                .is_file(),
            "{dirs:?}"
        );
        let opencode: Value = serde_json::from_str(&env[OPENCODE_CONFIG_CONTENT]).unwrap();
        assert_eq!(opencode["plugin"][0], "mine");
        assert!(names_roost_plugin(opencode["plugin"][1].as_str().unwrap()));
    }

    #[test]
    fn off_gets_neither_and_strips_an_inherited_roost_entry() {
        let world = world(ClaudePolicy::Allowed);
        let inherited = [
            (CLAUDE_PLUGIN_DIRS, OLD_ROOST_DIR),
            (
                OPENCODE_CONFIG_CONTENT,
                &*format!(r#"{{"plugin":["{OLD_PLUGIN}"]}}"#),
            ),
        ];
        for config in [
            "agent-integration = off",
            "agent-hooks = off",
            "agent-hooks = codex",
        ] {
            let env = world.tab(config, &inherited);
            assert_eq!(env.get(CLAUDE_PLUGIN_DIRS), None, "{config}");
            assert_eq!(env.get(OPENCODE_CONFIG_CONTENT), None, "{config}");
        }
    }

    #[test]
    fn one_agent_off_leaves_the_other() {
        let world = world(ClaudePolicy::Allowed);
        let env = world.tab(
            "agent-integration = -claude",
            &[(CLAUDE_PLUGIN_DIRS, OLD_ROOST_DIR)],
        );
        assert_eq!(env.get(CLAUDE_PLUGIN_DIRS), None);
        assert!(env.contains_key(OPENCODE_CONFIG_CONTENT));
        let env = world.tab("agent-integration = -opencode", &[]);
        assert_eq!(roost_dirs(&env).len(), 1);
        assert_eq!(env.get(OPENCODE_CONFIG_CONTENT), None);
    }

    #[test]
    fn a_blocking_policy_skips_claude_only() {
        let world = world(ClaudePolicy::Blocked("probe".into()));
        let env = world.tab(
            "",
            &[(CLAUDE_PLUGIN_DIRS, &format!("/u/one:{OLD_ROOST_DIR}"))],
        );
        assert_eq!(
            env.get(CLAUDE_PLUGIN_DIRS).map(String::as_str),
            Some("/u/one")
        );
        assert!(env.contains_key(OPENCODE_CONFIG_CONTENT));

        let world = self::world(ClaudePolicy::Allowed);
        fs::create_dir_all(&world.managed).unwrap();
        fs::write(
            world.managed.join("managed-settings.json"),
            r#"{"disableSideloadFlags":true}"#,
        )
        .unwrap();
        assert_eq!(world.tab("", &[]).get(CLAUDE_PLUGIN_DIRS), None);
        assert!(world.tab_env.claude_policy().blocks());
    }

    #[test]
    fn the_verdict_is_recorded_for_doctor() {
        let world = world(ClaudePolicy::Unchecked);
        world.tab_env.lock().running = true;
        world.tab_env.probe_now();
        assert_eq!(
            read_policy_record(&world.tab_env.0.dir),
            Some(PolicyRecord {
                policy: ClaudePolicy::NotInstalled,
                remote_settings_cached: false
            })
        );
    }
}
