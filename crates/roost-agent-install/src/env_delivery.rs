//! The files agents load from the tab environment instead of from their
//! own config: Claude's `roost` plugin, which `CLAUDE_CODE_PLUGIN_DIRS`
//! names, and the opencode plugin, which `OPENCODE_CONFIG_CONTENT`
//! names. Nothing here touches an agent's directory; every byte lands
//! under a directory Roost owns ([`roost_ipc::paths::BundleProfile::agent_integration_dir`]).
//!
//! Layout, under that directory:
//!
//! ```text
//! <version>/roost/.claude-plugin/plugin.json
//! <version>/roost/hooks/hooks.json
//! <version>/roost/skills/roost/SKILL.md
//! <version>/opencode/roost-agent-state.js
//! ```
//!
//! `<version>` is a digest of the contents, so a directory is never
//! rewritten in place: a `claude` started against one build keeps its
//! files when a newer build materializes a different set, and two
//! processes that carry the same files share one directory. The plugin's
//! **name** stays `roost` across versions, because Claude loads only the
//! first of two same-named plugins: a stale Roost folder can shadow the
//! current one but never double-fire, and inside a Roost tab the env copy
//! replaces an installed `roost@roost` marketplace plugin.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use roost_agent::{Agent, CLAUDE_HOOK_EVENTS};
use roost_ipc::paths::AGENT_INTEGRATION_DIR_NAME;
use sha2::{Digest, Sha256};

use crate::command::{installed_command, HOOK_TIMEOUT_SECS};
use crate::json::{Json, Style};
use crate::jsonedit;

/// The Claude plugin's name, and the name of its folder.
pub const CLAUDE_PLUGIN_NAME: &str = "roost";

const OPENCODE_DIR: &str = "opencode";

/// The one SKILL.md. `roostctl skill` prints the same file, and the
/// marketplace plugin serves it from the repo, so the three cannot drift.
const SKILL: &str = include_str!("../../../skills/roost/SKILL.md");

/// Hex digits of the content digest that name a version directory.
const VERSION_LEN: usize = 16;

/// Where the materialized files are, for the caller to name in a tab's
/// environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvAssets {
    /// The folder to append to `CLAUDE_CODE_PLUGIN_DIRS`.
    pub claude_plugin_dir: PathBuf,
    /// The file to name in `OPENCODE_CONFIG_CONTENT`'s `plugin` list.
    pub opencode_plugin: PathBuf,
}

impl EnvAssets {
    fn under(version_dir: &Path) -> EnvAssets {
        EnvAssets {
            claude_plugin_dir: version_dir.join(CLAUDE_PLUGIN_NAME),
            opencode_plugin: version_dir
                .join(OPENCODE_DIR)
                .join(roost_agent::opencode::PLUGIN_FILE_NAME),
        }
    }
}

/// One file of the set: its path under the version directory, and its
/// bytes.
struct Asset {
    path: String,
    bytes: Vec<u8>,
}

fn render(value: &Json) -> Vec<u8> {
    value.render(&Style::default()).into_bytes()
}

fn string(text: &str) -> Json {
    Json::String(text.to_string())
}

fn plugin_manifest() -> Json {
    Json::Object(vec![
        ("name".into(), string(CLAUDE_PLUGIN_NAME)),
        ("version".into(), string(env!("CARGO_PKG_VERSION"))),
        (
            "description".into(),
            string(
                "Reports this session's state to the Roost tab it runs in, and teaches \
                 the agent to drive Roost through roostctl. Loaded by Roost through \
                 CLAUDE_CODE_PLUGIN_DIRS; nothing is installed.",
            ),
        ),
    ])
}

/// One group per event, holding the same handler `settings.json` gets:
/// the same command, so the adapters see no difference in how an event
/// arrived, and the same timeout.
fn claude_hooks() -> Json {
    let handler = jsonedit::handler(&installed_command(Agent::Claude), HOOK_TIMEOUT_SECS, None);
    let events = CLAUDE_HOOK_EVENTS
        .iter()
        .map(|event| {
            let group = Json::Object(vec![("hooks".into(), Json::Array(vec![handler.clone()]))]);
            (event.to_string(), Json::Array(vec![group]))
        })
        .collect();
    Json::Object(vec![("hooks".into(), Json::Object(events))])
}

fn assets() -> Vec<Asset> {
    let plugin = CLAUDE_PLUGIN_NAME;
    vec![
        Asset {
            path: format!("{plugin}/.claude-plugin/plugin.json"),
            bytes: render(&plugin_manifest()),
        },
        Asset {
            path: format!("{plugin}/hooks/hooks.json"),
            bytes: render(&claude_hooks()),
        },
        Asset {
            path: format!("{plugin}/skills/{plugin}/SKILL.md"),
            bytes: SKILL.as_bytes().to_vec(),
        },
        Asset {
            path: format!("{OPENCODE_DIR}/{}", roost_agent::opencode::PLUGIN_FILE_NAME),
            bytes: roost_agent::opencode::PLUGIN_SOURCE.as_bytes().to_vec(),
        },
    ]
}

/// The version directory's name: a digest over every path and its bytes,
/// each length-prefixed so no two different sets can hash alike.
fn version(assets: &[Asset]) -> String {
    let mut digest = Sha256::new();
    for asset in assets {
        for part in [asset.path.as_bytes(), asset.bytes.as_slice()] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part);
        }
    }
    let mut hex = crate::codex_hash::hex(&digest.finalize().into());
    hex.truncate(VERSION_LEN);
    hex
}

/// Does `dir` hold exactly these bytes at these paths? Extra files are
/// not checked: nothing loads them.
fn holds(dir: &Path, assets: &[Asset]) -> bool {
    assets
        .iter()
        .all(|asset| fs::read(dir.join(&asset.path)).is_ok_and(|bytes| bytes == asset.bytes))
}

/// A name no other call, in this process or another, is using.
fn scratch_name(kind: &str) -> String {
    format!(".{kind}-{}", crate::write::nonce())
}

/// Write this build's files under `dir` (normally
/// [`BundleProfile::agent_integration_dir`](roost_ipc::paths::BundleProfile::agent_integration_dir))
/// unless they are already there, and say where they are.
///
/// Cheap when the files are in place (four small reads), so a caller can
/// run it at every tab spawn and so survive a cache clean. Otherwise the
/// set is staged in a scratch directory and renamed into place whole, so
/// no agent ever sees half a plugin, and a damaged copy (a truncated file,
/// a partial clean) is replaced. Safe to race: when two processes
/// materialize at once, one rename wins and the other finds its files in
/// place.
pub fn materialize(dir: &Path) -> io::Result<EnvAssets> {
    let assets = assets();
    let target = dir.join(version(&assets));
    if holds(&target, &assets) {
        return Ok(EnvAssets::under(&target));
    }
    fs::create_dir_all(dir)?;
    let staging = dir.join(scratch_name("staging"));
    let installed = stage(&staging, &assets).and_then(|()| install(&staging, &target, &assets));
    // Gone already when the rename went through.
    let _ = fs::remove_dir_all(&staging);
    installed.map(|()| EnvAssets::under(&target))
}

fn stage(staging: &Path, assets: &[Asset]) -> io::Result<()> {
    for asset in assets {
        let path = staging.join(&asset.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &asset.bytes)?;
    }
    Ok(())
}

fn install(staging: &Path, target: &Path, assets: &[Asset]) -> io::Result<()> {
    let Err(error) = fs::rename(staging, target) else {
        return Ok(());
    };
    // Another process renamed the same set into place first.
    if holds(target, assets) {
        return Ok(());
    }
    if !target.exists() {
        return Err(error);
    }
    // A damaged copy: a directory cannot be renamed over a non-empty one,
    // so move it aside first.
    let aside = target.with_file_name(scratch_name("damaged"));
    fs::rename(target, &aside)?;
    let replaced = fs::rename(staging, target);
    let _ = fs::remove_dir_all(&aside);
    match replaced {
        Err(_) if holds(target, assets) => Ok(()),
        other => other,
    }
}

/// Is `component` shaped like a version directory's name?
fn is_version(component: &std::ffi::OsStr) -> bool {
    component.to_str().is_some_and(|name| {
        name.len() == VERSION_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

/// Do the last components of `path` read `agent-integration/<version>`
/// followed by `tail`? A path-shape test, so an inherited environment
/// value can be screened without touching the disk, and a Roost folder
/// from any profile (a UI's, a session's, another build's) is recognised.
fn ends_in_version_dir(path: &Path, tail: &[&str]) -> bool {
    let mut components = path.components().rev();
    for want in tail.iter().rev() {
        if components.next().map(|c| c.as_os_str()) != Some(std::ffi::OsStr::new(want)) {
            return false;
        }
    }
    components.next().is_some_and(|c| is_version(c.as_os_str()))
        && components.next().map(|c| c.as_os_str())
            == Some(std::ffi::OsStr::new(AGENT_INTEGRATION_DIR_NAME))
}

/// Is `path` a Claude plugin folder some Roost materialized?
pub fn is_roost_claude_plugin_dir(path: &Path) -> bool {
    ends_in_version_dir(path, &[CLAUDE_PLUGIN_NAME])
}

/// Is `path` an opencode plugin file some Roost materialized?
pub fn is_roost_opencode_plugin(path: &Path) -> bool {
    ends_in_version_dir(
        path,
        &[OPENCODE_DIR, roost_agent::opencode::PLUGIN_FILE_NAME],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::Home;
    use std::os::unix::fs::MetadataExt;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let scratch = tempfile::tempdir().expect("scratch");
        let dir = scratch.path().join(AGENT_INTEGRATION_DIR_NAME);
        (scratch, dir)
    }

    fn read_json(path: &Path) -> Json {
        Json::parse(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
            .expect("valid JSON")
    }

    /// The env plugin registers the hooks `settings.json` gets today,
    /// entry for entry: what the Claude installer would merge into an
    /// empty settings file is exactly the plugin's `hooks` object, so the
    /// adapters cannot tell the two deliveries apart.
    #[test]
    fn the_plugin_hooks_are_the_settings_json_entries() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        let hooks = read_json(&assets.claude_plugin_dir.join("hooks/hooks.json"));

        let home_dir = tempfile::tempdir().expect("home");
        let home = Home::rooted(home_dir.path());
        fs::create_dir_all(home.agent_dir(Agent::Claude)).expect("claude dir");
        let plan = crate::claude::plan_install(&home).expect("plan");
        let settings = plan.edits[0].after.as_deref().expect("a write");
        let settings = Json::parse(settings).expect("settings JSON");

        assert_eq!(hooks.get("hooks"), settings.get("hooks"));
        let events: Vec<&str> = hooks
            .get("hooks")
            .and_then(Json::as_object)
            .expect("hooks object")
            .iter()
            .map(|(event, _)| event.as_str())
            .collect();
        assert_eq!(events, CLAUDE_HOOK_EVENTS);
    }

    /// The plugin's name is what lets the env copy replace an installed
    /// `roost@roost` and keeps a stale Roost folder from double-firing.
    #[test]
    fn the_claude_plugin_is_named_roost() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        assert!(assets.claude_plugin_dir.ends_with("roost"));
        let manifest = read_json(&assets.claude_plugin_dir.join(".claude-plugin/plugin.json"));
        assert_eq!(manifest.get("name").and_then(Json::as_str), Some("roost"));
    }

    /// The skill in the env plugin is the repo's SKILL.md, byte for byte,
    /// read off disk here rather than through the constant the writer uses.
    #[test]
    fn the_skill_is_the_repo_skill() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/roost/SKILL.md");
        assert_eq!(
            fs::read(assets.claude_plugin_dir.join("skills/roost/SKILL.md")).expect("skill"),
            fs::read(&repo).expect("repo skill")
        );
    }

    #[test]
    fn the_opencode_plugin_is_the_shipped_asset() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../roost-agent/assets/opencode/roost-agent-state.js");
        assert_eq!(
            fs::read(&assets.opencode_plugin).expect("plugin"),
            fs::read(&shipped).expect("shipped plugin")
        );
    }

    /// A second call finds the files and leaves the directory alone: a
    /// running agent's plugin is never rewritten under it.
    #[test]
    fn a_second_materialize_writes_nothing() {
        let (_scratch, dir) = root();
        let first = materialize(&dir).expect("first");
        let inode = fs::metadata(&first.claude_plugin_dir).expect("dir").ino();
        let modified = fs::metadata(first.claude_plugin_dir.join("hooks/hooks.json"))
            .and_then(|m| m.modified())
            .expect("mtime");

        let second = materialize(&dir).expect("second");
        assert_eq!(first, second);
        assert_eq!(
            fs::metadata(&second.claude_plugin_dir).expect("dir").ino(),
            inode
        );
        assert_eq!(
            fs::metadata(second.claude_plugin_dir.join("hooks/hooks.json"))
                .and_then(|m| m.modified())
                .expect("mtime"),
            modified
        );
    }

    /// A truncated file, or a cache clean that took part of the tree, is
    /// put back whole at the next call.
    #[test]
    fn a_damaged_or_missing_copy_is_replaced() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        let hooks = assets.claude_plugin_dir.join("hooks/hooks.json");
        let want = fs::read(&hooks).expect("hooks");

        fs::write(&hooks, b"{").expect("truncate");
        assert_eq!(materialize(&dir).expect("repair"), assets);
        assert_eq!(fs::read(&hooks).expect("hooks"), want);

        fs::remove_file(&assets.opencode_plugin).expect("remove");
        assert_eq!(materialize(&dir).expect("repair"), assets);
        assert!(assets.opencode_plugin.exists());

        fs::remove_dir_all(&dir).expect("clean");
        assert_eq!(materialize(&dir).expect("rewrite"), assets);
        assert_eq!(fs::read(&hooks).expect("hooks"), want);
    }

    /// Processes racing to materialize the same set all succeed, all
    /// name the same directory, and leave no scratch directory behind.
    #[test]
    fn concurrent_materializes_agree() {
        let (_scratch, dir) = root();
        let results: Vec<EnvAssets> = std::thread::scope(|scope| {
            // Every thread is started before any is joined.
            let handles: [_; 8] = std::array::from_fn(|_| scope.spawn(|| materialize(&dir)));
            handles
                .into_iter()
                .map(|h| h.join().expect("thread").expect("materialize"))
                .collect()
        });
        assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(holds(
            results[0].claude_plugin_dir.parent().expect("version dir"),
            &assets()
        ));
        let entries: Vec<_> = fs::read_dir(&dir)
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
    }

    /// Different bytes, different directory: a build whose files differ
    /// never overwrites another's.
    #[test]
    fn the_version_follows_the_contents() {
        let mut changed = assets();
        assert_eq!(version(&changed), version(&assets()));
        changed[2].bytes.push(b'\n');
        assert_ne!(version(&changed), version(&assets()));
        assert_eq!(version(&changed).len(), VERSION_LEN);
    }

    #[test]
    fn materialized_paths_are_recognised_and_lookalikes_are_not() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        assert!(is_roost_claude_plugin_dir(&assets.claude_plugin_dir));
        assert!(is_roost_opencode_plugin(&assets.opencode_plugin));
        assert!(!is_roost_claude_plugin_dir(&assets.opencode_plugin));
        assert!(!is_roost_opencode_plugin(&assets.claude_plugin_dir));

        for foreign in [
            "/home/u/plugins/roost",
            "/home/u/.cache/roost/agent-integration/roost",
            "/home/u/.cache/roost/agent-integration/0123456789ABCDEF/roost",
            "/home/u/.cache/roost/agent-integration/0123456789abcde/roost",
            "/home/u/.cache/roost/integration/0123456789abcdef/roost",
            "/home/u/.cache/roost/agent-integration/0123456789abcdef/roost/x",
        ] {
            assert!(!is_roost_claude_plugin_dir(Path::new(foreign)), "{foreign}");
        }
        assert!(is_roost_claude_plugin_dir(Path::new(
            "/Users/u/Library/Caches/Roost-session/agent-integration/0123456789abcdef/roost"
        )));
    }
}
