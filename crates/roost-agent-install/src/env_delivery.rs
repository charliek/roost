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
//!
//! Claude runs what the plugin folder holds, so the directory is private
//! to this user (a `0700` leaf under a parent no other uid can write) and
//! a version directory counts as present only when it holds exactly
//! these files and nothing else.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
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
    /// The file to name in `OPENCODE_CONFIG_CONTENT`'s `plugin` list. A
    /// path, not a URL: it may hold spaces or non-ASCII, so a `file://`
    /// form of it must be percent-encoded.
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

/// Does `dir` hold these bytes at these paths, and nothing else? Claude
/// loads more from a plugin folder than the files Roost writes (an
/// `.mcp.json`, `commands/`, `agents/`, other skills), so an extra entry
/// makes the copy damaged, not present.
fn holds(dir: &Path, assets: &[Asset]) -> bool {
    let want: BTreeSet<PathBuf> = assets
        .iter()
        .flat_map(|asset| Path::new(&asset.path).ancestors())
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    let mut found = BTreeSet::new();
    fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir())
        && entries(dir, Path::new(""), &mut found).is_ok()
        && found == want
        && assets
            .iter()
            .all(|asset| fs::read(dir.join(&asset.path)).is_ok_and(|bytes| bytes == asset.bytes))
}

/// Every path under `root.join(rel)`, relative to `root`. A link or any
/// other kind of entry is an error: nothing Roost writes is one.
fn entries(root: &Path, rel: &Path, found: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let path = rel.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            entries(root, &path, found)?;
        } else if !kind.is_file() {
            return Err(io::Error::other("neither a file nor a directory"));
        }
        found.insert(path);
    }
    Ok(())
}

fn refuse(dir: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("agent integration dir {}: {why}", dir.display()),
    )
}

/// Make `dir` a `0700` directory of this user's, under a parent no other
/// uid can write, or refuse. On the Mac a `HOME`-less process falls back
/// to a cache root under `/tmp`, which another uid could have made first
/// and filled with a plugin Claude would run.
pub(crate) fn prepare(dir: &Path) -> io::Result<()> {
    let euid = roost_ipc::current_euid();
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
        let meta = fs::metadata(parent)?;
        if meta.uid() != euid {
            return Err(refuse(
                dir,
                &format!("parent is owned by uid {}", meta.uid()),
            ));
        }
        if meta.mode() & 0o002 != 0 {
            return Err(refuse(dir, "parent is writable by every user"));
        }
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        // A umask can only clear bits; this puts back owner bits it took.
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(refuse(dir, "not a directory"));
    }
    if meta.uid() != euid {
        return Err(refuse(dir, &format!("owned by uid {}", meta.uid())));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse(
            dir,
            &format!("mode {:04o}; 0700 is required", meta.mode() & 0o7777),
        ));
    }
    Ok(())
}

/// A name no other call, in this process or another, is using.
fn scratch_name(kind: &str) -> String {
    format!(".{kind}-{}", crate::write::nonce())
}

/// Write this build's files under `dir` (normally
/// [`BundleProfile::agent_integration_dir`](roost_ipc::paths::BundleProfile::agent_integration_dir))
/// unless they are already there, and say where they are.
///
/// Cheap when the files are in place (a few stats, directory reads and
/// four small reads), so a caller can run it at every tab spawn and so
/// survive a cache clean. Otherwise the set is staged in a scratch
/// directory and renamed into place whole, so no agent ever sees half a
/// plugin, and a damaged copy (a truncated file, a partial clean, an
/// extra file) is replaced. Safe to race: when two processes materialize
/// at once, one rename wins and the other finds its files in place.
///
/// Blocking file I/O, a miss included, so never from iced's
/// `update`/`view`: call it on the engine's spawn path or a blocking
/// task, in the UI and in `roost-session` alike.
pub fn materialize(dir: &Path) -> io::Result<EnvAssets> {
    let assets = assets();
    prepare(dir)?;
    let target = dir.join(version(&assets));
    if holds(&target, &assets) {
        return Ok(EnvAssets::under(&target));
    }
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
    install_with(staging, target, assets, || {})
}

/// `install`, with `before_aside` run just before the damaged copy is
/// moved, where a racing repair can get there first.
fn install_with(
    staging: &Path,
    target: &Path,
    assets: &[Asset],
    before_aside: impl FnOnce(),
) -> io::Result<()> {
    if fs::rename(staging, target).is_ok() {
        return Ok(());
    }
    // Another process renamed the same set into place first.
    if holds(target, assets) {
        return Ok(());
    }
    // A damaged copy: a directory cannot be renamed over a non-empty one,
    // so move it aside first.
    let aside = target.with_file_name(scratch_name("damaged"));
    before_aside();
    let replaced = match fs::rename(target, &aside) {
        Ok(()) => {
            let replaced = fs::rename(staging, target);
            let _ = fs::remove_dir_all(&aside);
            replaced
        }
        // Gone: a racing repair moved it aside, or it never existed and
        // the first rename failed for another reason, which retrying
        // reports.
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(staging, target),
        Err(error) => return Err(error),
    };
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

    /// Claude loads more from a plugin folder than Roost writes, so an
    /// extra entry anywhere in the set is damage, put right at the next
    /// call.
    #[test]
    fn an_extra_entry_in_the_set_is_removed() {
        let (_scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        let plugin = &assets.claude_plugin_dir;
        for extra in [
            Extra::File(plugin.join(".mcp.json")),
            Extra::File(plugin.join("commands/run.md")),
            Extra::Dir(plugin.join("agents")),
            Extra::Link(plugin.join("skills/other")),
        ] {
            let path = extra.plant(plugin);
            assert_eq!(materialize(&dir).expect("repair"), assets);
            assert!(fs::symlink_metadata(&path).is_err(), "{}", path.display());
            assert!(holds(
                plugin.parent().expect("version dir"),
                &super::assets()
            ));
        }
    }

    enum Extra {
        File(PathBuf),
        Dir(PathBuf),
        Link(PathBuf),
    }

    impl Extra {
        fn plant(&self, plugin: &Path) -> PathBuf {
            match self {
                Extra::File(path) => {
                    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
                    fs::write(path, b"{}").expect("plant file");
                    path.clone()
                }
                Extra::Dir(path) => {
                    fs::create_dir(path).expect("plant dir");
                    path.clone()
                }
                Extra::Link(path) => {
                    std::os::unix::fs::symlink(plugin.join("skills/roost"), path)
                        .expect("plant link");
                    path.clone()
                }
            }
        }
    }

    /// Another uid's plugin must never be what a tab names: a directory
    /// others can reach into, a link, or one under a parent every user
    /// can write is refused rather than used.
    #[test]
    fn a_directory_others_can_write_is_refused() {
        let (scratch, dir) = root();
        fs::create_dir(&dir).expect("dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = materialize(&dir).expect_err("a 0755 dir is refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");

        let elsewhere = scratch.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("elsewhere");
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).expect("chmod");
        let link = scratch.path().join("linked");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("link");
        let error = materialize(&link).expect_err("a link is refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");

        let shared = scratch.path().join("shared");
        fs::create_dir(&shared).expect("shared");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        let error = materialize(&shared.join(AGENT_INTEGRATION_DIR_NAME))
            .expect_err("a world-writable parent is refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(!shared.join(AGENT_INTEGRATION_DIR_NAME).exists());
    }

    /// Two repairs of one damaged copy: the one that finds the copy
    /// already moved aside by the other still installs, not fails.
    #[test]
    fn a_repair_beaten_to_the_damaged_copy_still_installs() {
        let (scratch, dir) = root();
        let assets = materialize(&dir).expect("materialize");
        let target = assets
            .claude_plugin_dir
            .parent()
            .expect("version dir")
            .to_path_buf();
        fs::write(target.join("roost/hooks/hooks.json"), b"{").expect("damage");

        let set = super::assets();
        let staging = dir.join(scratch_name("staging"));
        stage(&staging, &set).expect("stage");
        let taken = scratch.path().join("taken");
        install_with(&staging, &target, &set, || {
            fs::rename(&target, &taken).expect("the other repair moves it first");
        })
        .expect("install");
        assert!(holds(&target, &set));
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
