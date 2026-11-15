//! `roostctl` on `PATH` for terminals outside Roost (#261): one symlink,
//! in `/usr/local/bin` or `~/.local/bin`, to the `roostctl` inside an app
//! bundle, and nothing else — no rc file is ever edited.
//!
//! The UI's `app.cli_link` op, its palette row and menu item, and
//! `roostctl cli` all call [`status`], [`install`] and [`uninstall`];
//! the directories are an argument so tests can point them at a temp
//! root.
//!
//! What Roost may touch is decided by the link's own target, read with
//! `readlink` and never followed: a symlink whose target is a `roostctl`
//! in some `<x>.app/Contents/Resources/bin` is one a Roost made. Anything
//! else at the link's name (a file, a directory, a link elsewhere) is the
//! user's, and is never overwritten or removed.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::messages::AppCliLinkResult;

pub const NAME: &str = "roostctl";

/// The preferred link directory, used when it exists and this user can
/// write to it — on macOS it is on every login shell's `PATH` through
/// `/etc/paths`.
pub const SYSTEM_DIR: &str = "/usr/local/bin";

/// Overrides the link directories, `:`-separated and in preference
/// order, for tests. Honoured only under `ROOST_TEST_MODE=1`.
pub const TEST_DIRS_ENV: &str = "ROOST_TEST_CLI_LINK_DIRS";

/// `<x>.app/Contents/Resources/bin`, by its shape alone.
pub fn is_bundle_bin_dir(dir: &Path) -> bool {
    let mut parts = dir.components().rev();
    let mut next_is = |want: &str| matches!(parts.next(), Some(Component::Normal(part)) if part == OsStr::new(want));
    if !(next_is("bin") && next_is("Resources") && next_is("Contents")) {
        return false;
    }
    matches!(
        parts.next(),
        Some(Component::Normal(app)) if app.as_bytes().len() > 4 && app.as_bytes().ends_with(b".app")
    )
}

/// The bundle `bin` directory `roostctl` lies in, if it lies in one.
pub fn bundle_bin_dir_of(roostctl: &Path) -> Option<&Path> {
    roostctl
        .parent()
        .filter(|dir| roostctl.file_name() == Some(OsStr::new(NAME)) && is_bundle_bin_dir(dir))
}

/// A bundle `bin` directory with a `roostctl` in it: the directory some
/// Roost puts first on its tabs' `PATH`. Another app's `Resources/bin`
/// (Docker's, say) holds no `roostctl` and stays.
pub fn is_roost_bundle_bin_dir(dir: &Path) -> bool {
    is_bundle_bin_dir(dir) && dir.join(NAME).is_file()
}

/// macOS runs a quarantined app from a randomized read-only copy until
/// the user moves it; a link into that copy dies with it.
pub fn is_translocated(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str() == OsStr::new("AppTranslocation"))
}

/// The link directories, in preference order: [`SYSTEM_DIR`], then
/// `~/.local/bin`, or [`TEST_DIRS_ENV`]'s under `ROOST_TEST_MODE=1`.
pub fn dirs_from_env() -> Vec<PathBuf> {
    let test_mode = std::env::var_os("ROOST_TEST_MODE").is_some_and(|value| value == "1");
    if let Some(dirs) = std::env::var_os(TEST_DIRS_ENV).filter(|_| test_mode) {
        return std::env::split_paths(&dirs)
            .filter(|dir| dir.is_absolute())
            .collect();
    }
    let mut dirs = vec![PathBuf::from(SYSTEM_DIR)];
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    {
        dirs.push(home.join(".local").join("bin"));
    }
    dirs
}

/// What one directory holds at [`NAME`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Empty,
    /// A link to `source`.
    Ours,
    /// A link into a bundle's `bin` that does not reach `source`: the app
    /// moved, or it is another Roost's.
    Stale {
        target: PathBuf,
    },
    /// Not Roost's: never written, never removed.
    Foreign,
}

fn slot(link: &Path, source: &Path) -> Slot {
    let meta = match std::fs::symlink_metadata(link) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Slot::Empty,
        Err(_) => return Slot::Foreign,
    };
    if !meta.file_type().is_symlink() {
        return Slot::Foreign;
    }
    let Ok(raw) = std::fs::read_link(link) else {
        return Slot::Foreign;
    };
    let target = match link.parent() {
        Some(dir) if raw.is_relative() => dir.join(&raw),
        _ => raw,
    };
    if bundle_bin_dir_of(&target).is_none() {
        return Slot::Foreign;
    }
    let reaches = |path: &Path| std::fs::canonicalize(path).ok();
    match (reaches(link), reaches(source)) {
        (Some(resolved), Some(source)) if resolved == source => Slot::Ours,
        _ => Slot::Stale { target },
    }
}

/// Link presence across the directories — what a menu row is labelled
/// from, never the caller's `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Installed { link: PathBuf },
    Broken { link: PathBuf, target: PathBuf },
    Absent,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Installed { .. } => "installed",
            Self::Broken { .. } => "broken",
            Self::Absent => "absent",
        }
    }

    /// The row and menu item's label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Installed { .. } => "Uninstall 'roostctl' Command",
            Self::Broken { .. } => "Repair 'roostctl' Command\u{2026}",
            Self::Absent => "Install 'roostctl' Command",
        }
    }

    pub fn link(&self) -> Option<&Path> {
        match self {
            Self::Installed { link } | Self::Broken { link, .. } => Some(link),
            Self::Absent => None,
        }
    }
}

/// A link to `source` anywhere wins over a stale one; the first stale one
/// in directory order is the one to repair.
pub fn status(source: &Path, dirs: &[PathBuf]) -> Status {
    let mut broken = None;
    for dir in dirs {
        let link = dir.join(NAME);
        match slot(&link, source) {
            Slot::Ours => return Status::Installed { link },
            Slot::Stale { target } if broken.is_none() => {
                broken = Some(Status::Broken { link, target });
            }
            Slot::Stale { .. } | Slot::Empty | Slot::Foreign => {}
        }
    }
    broken.unwrap_or(Status::Absent)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    Created { link: PathBuf },
    Repaired { link: PathBuf, was: PathBuf },
    AlreadyThere { link: PathBuf },
}

impl Installed {
    pub fn link(&self) -> &Path {
        match self {
            Self::Created { link } | Self::Repaired { link, .. } | Self::AlreadyThere { link } => {
                link
            }
        }
    }
}

/// Why nothing was linked or unlinked. Each says what the user can do,
/// and [`Refusal::command`] is the command that does it by hand, when
/// there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// This Roost found no `roostctl` of its own to link.
    NoRoostctl,
    /// This `roostctl` is not inside an app bundle: it is installed where
    /// it is, and a link would add nothing.
    NotBundled {
        source: PathBuf,
    },
    Translocated {
        source: PathBuf,
    },
    /// Every directory already holds a `roostctl` that is not Roost's.
    Occupied {
        source: PathBuf,
        links: Vec<PathBuf>,
    },
    NoDirectory {
        source: PathBuf,
    },
    Failed {
        source: PathBuf,
        link: PathBuf,
        removing: bool,
        error: String,
    },
}

impl Refusal {
    pub fn command(&self) -> Option<String> {
        match self {
            Self::NoRoostctl
            | Self::NotBundled { .. }
            | Self::Translocated { .. }
            | Self::NoDirectory { .. } => None,
            Self::Occupied { source, links } => links
                .first()
                .map(|link| format!("ln -sf {} {}", quote(source), quote(link))),
            Self::Failed {
                source,
                link,
                removing,
                ..
            } => Some(if *removing {
                format!("rm {}", quote(link))
            } else {
                format!("ln -sf {} {}", quote(source), quote(link))
            }),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRoostctl => write!(f, "this Roost has no roostctl of its own to link"),
            Self::NotBundled { source } => write!(
                f,
                "{} is not inside an app bundle, so there is nothing to link: add its directory \
                 to PATH instead",
                source.display()
            ),
            Self::Translocated { source } => write!(
                f,
                "Roost is running from a temporary copy macOS made ({}); move Roost.app to \
                 /Applications, open it from there, and try again",
                source.display()
            ),
            Self::Occupied { links, .. } => write!(
                f,
                "{} already exists and is not a link Roost made; Roost leaves it alone",
                join_paths(links)
            ),
            Self::NoDirectory { .. } => {
                write!(f, "no directory to put the link in (HOME is not set)")
            }
            Self::Failed {
                link,
                removing,
                error,
                ..
            } => write!(
                f,
                "could not {} {}: {error}",
                if *removing { "remove" } else { "write" },
                link.display()
            ),
        }
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `path` single-quoted for a POSIX shell.
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

fn writable(dir: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated pointer this call does not retain.
    dir.is_dir() && unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

/// Point a link at `source`: repair a stale one where it is, else create
/// one in the first directory that exists, is writable and is free — the
/// last directory is created if it is missing.
pub fn install(source: &Path, dirs: &[PathBuf]) -> Result<Installed, Refusal> {
    let refuse_source = || source.to_path_buf();
    if bundle_bin_dir_of(source).is_none() {
        return Err(Refusal::NotBundled {
            source: refuse_source(),
        });
    }
    if is_translocated(source) {
        return Err(Refusal::Translocated {
            source: refuse_source(),
        });
    }
    match status(source, dirs) {
        Status::Installed { link } => return Ok(Installed::AlreadyThere { link }),
        Status::Broken { link, target } => {
            return match replace_link(source, &link) {
                Ok(()) => Ok(Installed::Repaired { link, was: target }),
                Err(error) => Err(Refusal::Failed {
                    source: refuse_source(),
                    link,
                    removing: false,
                    error: error.to_string(),
                }),
            };
        }
        Status::Absent => {}
    }
    let Some(last) = dirs.len().checked_sub(1) else {
        return Err(Refusal::NoDirectory {
            source: refuse_source(),
        });
    };
    let mut occupied = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        let link = dir.join(NAME);
        if index < last && !writable(dir) {
            continue;
        }
        if slot(&link, source) != Slot::Empty {
            occupied.push(link);
            continue;
        }
        let made =
            std::fs::create_dir_all(dir).and_then(|()| std::os::unix::fs::symlink(source, &link));
        return made
            .map(|()| Installed::Created { link: link.clone() })
            .map_err(|error| Refusal::Failed {
                source: refuse_source(),
                link,
                removing: false,
                error: error.to_string(),
            });
    }
    Err(Refusal::Occupied {
        source: refuse_source(),
        links: occupied,
    })
}

/// Swap `link` for one to `source` with a rename, so there is no moment
/// with no `roostctl` there.
fn replace_link(source: &Path, link: &Path) -> io::Result<()> {
    let dir = link.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let staged = dir.join(format!(".{NAME}.roost-{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(source, &staged)?;
    std::fs::rename(&staged, link).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uninstalled {
    Removed { links: Vec<PathBuf> },
    NothingThere,
}

/// Remove every link in `dirs` that a Roost made — this one's and stale
/// ones alike. The user's own `roostctl` stays.
pub fn uninstall(source: &Path, dirs: &[PathBuf]) -> Result<Uninstalled, Refusal> {
    let mut removed = Vec::new();
    for dir in dirs {
        let link = dir.join(NAME);
        if matches!(slot(&link, source), Slot::Ours | Slot::Stale { .. }) {
            std::fs::remove_file(&link).map_err(|error| Refusal::Failed {
                source: source.to_path_buf(),
                link: link.clone(),
                removing: true,
                error: error.to_string(),
            })?;
            removed.push(link);
        }
    }
    Ok(if removed.is_empty() {
        Uninstalled::NothingThere
    } else {
        Uninstalled::Removed { links: removed }
    })
}

/// What a caller asks of [`act`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Install,
    Uninstall,
}

impl Action {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "status" => Some(Self::Status),
            "install" => Some(Self::Install),
            "uninstall" => Some(Self::Uninstall),
            _ => None,
        }
    }
}

/// The caller's view of the world, for [`act`].
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The `roostctl` to link, or `None` when this Roost has none.
    pub source: Option<&'a Path>,
    pub dirs: &'a [PathBuf],
    /// The caller's `PATH`, to say whether a new link's directory is on it.
    pub path_var: Option<&'a OsStr>,
    pub home: Option<&'a Path>,
}

/// Run `action` and say what came of it: the one entry point
/// `app.cli_link`, its palette row and menu item, and `roostctl cli` all
/// call.
pub fn act(action: Action, context: Context<'_>) -> AppCliLinkResult {
    let source = context.source.unwrap_or(Path::new(""));
    let done = match (action, context.source) {
        (Action::Status, _) => Ok(Done::Status),
        (Action::Install, None) => Err(Refusal::NoRoostctl),
        (Action::Install, Some(source)) => install(source, context.dirs).map(Done::Installed),
        (Action::Uninstall, _) => uninstall(source, context.dirs).map(Done::Uninstalled),
    };
    let after = status(source, context.dirs);
    let (outcome, message, command, path_line) = match done {
        Ok(Done::Status) => ("status", describe(&after), None, None),
        Ok(Done::Installed(installed)) => {
            let link = installed.link();
            let dir = link.parent().unwrap_or(Path::new("/"));
            let path_line = (!on_path(dir, context.path_var)).then(|| path_line(dir, context.home));
            let (outcome, verb) = match &installed {
                Installed::Created { .. } => ("created", "Installed"),
                Installed::Repaired { .. } => ("repaired", "Repaired"),
                Installed::AlreadyThere { .. } => ("already_installed", "Already installed"),
            };
            let message = format!(
                "{verb} 'roostctl': {} → {}",
                link.display(),
                source.display()
            );
            (outcome, message, None, path_line)
        }
        Ok(Done::Uninstalled(Uninstalled::Removed { links })) => (
            "removed",
            format!("Uninstalled 'roostctl': removed {}", join_paths(&links)),
            None,
            None,
        ),
        Ok(Done::Uninstalled(Uninstalled::NothingThere)) => (
            "nothing_to_remove",
            "No 'roostctl' link to remove".to_string(),
            None,
            None,
        ),
        Err(refusal) => ("refused", refusal.to_string(), refusal.command(), None),
    };
    AppCliLinkResult {
        state: after.as_str().into(),
        label: after.label().into(),
        link: after.link().map(|link| link.display().to_string()),
        target: match &after {
            Status::Broken { target, .. } => Some(target.display().to_string()),
            Status::Installed { .. } | Status::Absent => None,
        },
        source: context.source.map(|source| source.display().to_string()),
        bundled: context.source.and_then(bundle_bin_dir_of).is_some(),
        outcome: outcome.into(),
        message,
        command,
        path_line,
    }
}

enum Done {
    Status,
    Installed(Installed),
    Uninstalled(Uninstalled),
}

fn describe(status: &Status) -> String {
    match status {
        Status::Installed { link } => format!("'roostctl' is installed at {}", link.display()),
        Status::Broken { link, target } => format!(
            "The 'roostctl' link at {} points at {}, which is not this Roost's",
            link.display(),
            target.display()
        ),
        Status::Absent => "'roostctl' is not installed".to_string(),
    }
}

/// Whether `dir` is an entry of `path_var`.
pub fn on_path(dir: &Path, path_var: Option<&OsStr>) -> bool {
    path_var.is_some_and(|path| std::env::split_paths(path).any(|entry| entry == dir))
}

/// The line that puts `dir` on a login shell's `PATH`, written with
/// `$HOME` when `dir` is under `home`.
pub fn path_line(dir: &Path, home: Option<&Path>) -> String {
    let spelled = home
        .and_then(|home| dir.strip_prefix(home).ok())
        .map(|rest| format!("$HOME/{}", rest.display()))
        .unwrap_or_else(|| dir.display().to_string());
    format!("export PATH=\"{spelled}:$PATH\"")
}

/// The first `roostctl` on `path_var` this user can run, as `PATH` spells
/// it. Relative and empty entries are skipped: they name a directory only
/// relative to this process's cwd.
pub fn find_on_path(path_var: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(NAME))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated pointer this call does not retain.
    path.is_file() && unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A temp root with a bundle holding a `roostctl`, and two link
    /// directories — the first missing until a test makes it.
    struct World {
        _root: tempfile::TempDir,
        root: PathBuf,
        source: PathBuf,
        system: PathBuf,
        user: PathBuf,
    }

    impl World {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = std::fs::canonicalize(tmp.path()).expect("canonical");
            let source = bundle(&root, "Roost.app");
            Self {
                system: root.join("usr-local-bin"),
                user: root.join("home/.local/bin"),
                _root: tmp,
                root,
                source,
            }
        }

        fn dirs(&self) -> Vec<PathBuf> {
            vec![self.system.clone(), self.user.clone()]
        }
    }

    fn bundle(root: &Path, app: &str) -> PathBuf {
        let bin = root
            .join("Applications")
            .join(app)
            .join("Contents/Resources/bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let roostctl = bin.join(NAME);
        std::fs::write(&roostctl, "#!/bin/sh\n").expect("roostctl");
        std::fs::set_permissions(&roostctl, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        roostctl
    }

    #[test]
    fn a_bundle_bin_dir_is_told_by_its_shape() {
        assert!(is_bundle_bin_dir(Path::new(
            "/Applications/Roost.app/Contents/Resources/bin"
        )));
        assert!(is_bundle_bin_dir(Path::new(
            "/Users/me/Roost Beta.app/Contents/Resources/bin/"
        )));
        for not in [
            "/usr/bin",
            "/home/me/roost/target/debug",
            "/Applications/Roost.app/Contents/MacOS",
            "/Applications/.app/Contents/Resources/bin",
            "/Applications/Roost.app/Contents/Resources/app/bin",
            "Contents/Resources/bin",
        ] {
            assert!(!is_bundle_bin_dir(Path::new(not)), "{not}");
        }
        assert_eq!(
            bundle_bin_dir_of(Path::new(
                "/Applications/Roost.app/Contents/Resources/bin/roostctl"
            )),
            Some(Path::new("/Applications/Roost.app/Contents/Resources/bin"))
        );
        assert_eq!(
            bundle_bin_dir_of(Path::new(
                "/Applications/Roost.app/Contents/Resources/bin/roost-session"
            )),
            None
        );
    }

    #[test]
    fn only_a_bundle_dir_holding_roostctl_is_a_roosts() {
        let world = World::new();
        let roost_bin = world.source.parent().expect("bin").to_path_buf();
        assert!(is_roost_bundle_bin_dir(&roost_bin));
        let docker_bin = world
            .root
            .join("Applications/Docker.app/Contents/Resources/bin");
        std::fs::create_dir_all(&docker_bin).expect("docker");
        std::fs::write(docker_bin.join("docker"), "").expect("docker");
        assert!(!is_roost_bundle_bin_dir(&docker_bin));
    }

    #[test]
    fn translocation_is_spotted_anywhere_in_the_path() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/x/T/AppTranslocation/ABC/d/Roost.app/Contents/Resources/bin/roostctl"
        )));
        assert!(!is_translocated(Path::new(
            "/Applications/Roost.app/Contents/Resources/bin/roostctl"
        )));
    }

    #[test]
    fn install_prefers_the_system_dir_when_it_is_writable() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        assert_eq!(status(&world.source, &world.dirs()), Status::Absent);

        let link = world.system.join(NAME);
        assert_eq!(
            install(&world.source, &world.dirs()),
            Ok(Installed::Created { link: link.clone() })
        );
        assert_eq!(std::fs::read_link(&link).expect("link"), world.source);
        assert!(!world.user.exists(), "the user dir is not made when unused");
        assert_eq!(
            status(&world.source, &world.dirs()),
            Status::Installed { link: link.clone() }
        );
        assert_eq!(
            install(&world.source, &world.dirs()),
            Ok(Installed::AlreadyThere { link })
        );
    }

    #[test]
    fn install_falls_back_to_the_user_dir_and_creates_it() {
        let world = World::new();
        let link = world.user.join(NAME);
        assert_eq!(
            install(&world.source, &world.dirs()),
            Ok(Installed::Created { link: link.clone() })
        );
        assert!(link.is_symlink());
    }

    #[test]
    fn a_read_only_system_dir_is_passed_over() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        std::fs::set_permissions(&world.system, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        let result = install(&world.source, &world.dirs());
        std::fs::set_permissions(&world.system, std::fs::Permissions::from_mode(0o755))
            .expect("chmod back");
        assert_eq!(
            result,
            Ok(Installed::Created {
                link: world.user.join(NAME)
            })
        );
    }

    #[test]
    fn a_regular_file_is_never_overwritten() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        std::fs::create_dir_all(&world.user).expect("user");
        for dir in [&world.system, &world.user] {
            std::fs::write(dir.join(NAME), "mine").expect("file");
        }
        let refused = install(&world.source, &world.dirs()).expect_err("occupied");
        assert!(matches!(refused, Refusal::Occupied { ref links, .. } if links.len() == 2));
        assert!(refused
            .command()
            .expect("command")
            .starts_with(&format!("ln -sf '{}'", world.source.display())));
        for dir in [&world.system, &world.user] {
            assert_eq!(
                std::fs::read_to_string(dir.join(NAME)).expect("kept"),
                "mine"
            );
        }
        assert_eq!(status(&world.source, &world.dirs()), Status::Absent);
    }

    #[test]
    fn a_foreign_link_is_skipped_for_the_next_dir() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        let dev = world.root.join("target/debug/roostctl");
        std::fs::create_dir_all(dev.parent().expect("dir")).expect("dev");
        std::fs::write(&dev, "").expect("dev");
        symlink(&dev, world.system.join(NAME)).expect("foreign");

        assert_eq!(
            install(&world.source, &world.dirs()),
            Ok(Installed::Created {
                link: world.user.join(NAME)
            })
        );
        assert_eq!(
            std::fs::read_link(world.system.join(NAME)).expect("kept"),
            dev
        );
        assert_eq!(
            uninstall(&world.source, &world.dirs()),
            Ok(Uninstalled::Removed {
                links: vec![world.user.join(NAME)]
            })
        );
        assert_eq!(
            std::fs::read_link(world.system.join(NAME)).expect("kept"),
            dev
        );
    }

    #[test]
    fn a_dangling_link_into_a_bundle_is_broken_and_repaired_in_place() {
        let world = World::new();
        std::fs::create_dir_all(&world.user).expect("user");
        let gone = world
            .root
            .join("Old/Roost.app/Contents/Resources/bin/roostctl");
        let link = world.user.join(NAME);
        symlink(&gone, &link).expect("dangling");

        assert_eq!(
            status(&world.source, &world.dirs()),
            Status::Broken {
                link: link.clone(),
                target: gone.clone()
            }
        );
        assert_eq!(
            install(&world.source, &world.dirs()),
            Ok(Installed::Repaired {
                link: link.clone(),
                was: gone
            })
        );
        assert_eq!(std::fs::read_link(&link).expect("link"), world.source);
        let leftovers: Vec<_> = std::fs::read_dir(&world.user)
            .expect("dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, vec![OsStr::new(NAME).to_os_string()]);
    }

    #[test]
    fn a_link_into_another_roost_is_broken_too() {
        let world = World::new();
        let other = bundle(&world.root, "Roost-Iced.app");
        std::fs::create_dir_all(&world.system).expect("system");
        symlink(&other, world.system.join(NAME)).expect("other");
        assert!(matches!(
            status(&world.source, &world.dirs()),
            Status::Broken { ref target, .. } if *target == other
        ));
    }

    #[test]
    fn a_link_to_this_roost_wins_over_a_stale_one() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        std::fs::create_dir_all(&world.user).expect("user");
        symlink(
            world.root.join("Gone.app/Contents/Resources/bin/roostctl"),
            world.system.join(NAME),
        )
        .expect("stale");
        symlink(&world.source, world.user.join(NAME)).expect("ours");
        assert_eq!(
            status(&world.source, &world.dirs()),
            Status::Installed {
                link: world.user.join(NAME)
            }
        );
        assert_eq!(
            uninstall(&world.source, &world.dirs()),
            Ok(Uninstalled::Removed {
                links: vec![world.system.join(NAME), world.user.join(NAME)]
            })
        );
        assert_eq!(status(&world.source, &world.dirs()), Status::Absent);
    }

    #[test]
    fn a_relative_link_into_the_bundle_counts_as_ours() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        symlink(
            "../Applications/Roost.app/Contents/Resources/bin/roostctl",
            world.system.join(NAME),
        )
        .expect("relative");
        assert_eq!(
            status(&world.source, &world.dirs()),
            Status::Installed {
                link: world.system.join(NAME)
            }
        );
    }

    #[test]
    fn uninstall_with_nothing_there_says_so() {
        let world = World::new();
        assert_eq!(
            uninstall(&world.source, &world.dirs()),
            Ok(Uninstalled::NothingThere)
        );
    }

    #[test]
    fn install_refuses_a_roostctl_outside_a_bundle() {
        let world = World::new();
        let plain = world.root.join("usr/bin/roostctl");
        assert!(matches!(
            install(&plain, &world.dirs()),
            Err(Refusal::NotBundled { .. })
        ));
        assert!(!world.user.exists());
    }

    #[test]
    fn install_refuses_a_translocated_bundle() {
        let world = World::new();
        let moved = bundle(&world.root.join("AppTranslocation/X/d"), "Roost.app");
        let refused = install(&moved, &world.dirs()).expect_err("translocated");
        assert!(matches!(refused, Refusal::Translocated { .. }));
        assert_eq!(refused.command(), None);
        assert!(!world.user.exists());
    }

    #[test]
    fn a_write_failure_hands_back_the_command_that_does_it_by_hand() {
        let world = World::new();
        std::fs::create_dir_all(&world.user).expect("user");
        std::fs::set_permissions(&world.user, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        let refused = install(&world.source, &world.dirs()).expect_err("read-only");
        std::fs::set_permissions(&world.user, std::fs::Permissions::from_mode(0o755))
            .expect("chmod back");
        assert_eq!(
            refused.command(),
            Some(format!(
                "ln -sf '{}' '{}'",
                world.source.display(),
                world.user.join(NAME).display()
            ))
        );
    }

    fn context<'a>(
        world: &'a World,
        dirs: &'a [PathBuf],
        path_var: Option<&'a OsStr>,
    ) -> Context<'a> {
        Context {
            source: Some(&world.source),
            dirs,
            path_var,
            home: Some(Path::new("/nowhere")),
        }
    }

    #[test]
    fn act_reports_each_outcome_with_the_state_after_it() {
        let world = World::new();
        let dirs = world.dirs();
        let status = act(Action::Status, context(&world, &dirs, None));
        assert_eq!(
            (status.state.as_str(), status.outcome.as_str()),
            ("absent", "status")
        );
        assert_eq!(status.label, "Install 'roostctl' Command");
        assert!(status.bundled);

        let created = act(Action::Install, context(&world, &dirs, None));
        assert_eq!(
            (created.state.as_str(), created.outcome.as_str()),
            ("installed", "created")
        );
        assert_eq!(created.label, "Uninstall 'roostctl' Command");
        assert_eq!(
            created.link.as_deref(),
            Some(world.user.join(NAME).to_str().unwrap())
        );
        assert_eq!(
            created.path_line.as_deref(),
            Some(format!("export PATH=\"{}:$PATH\"", world.user.display()).as_str()),
            "the user dir is not on an empty PATH"
        );

        let on = std::env::join_paths([&world.user]).unwrap();
        let again = act(Action::Install, context(&world, &dirs, Some(&on)));
        assert_eq!(again.outcome, "already_installed");
        assert_eq!(again.path_line, None, "the directory is on this PATH");

        let removed = act(Action::Uninstall, context(&world, &dirs, None));
        assert_eq!(
            (removed.state.as_str(), removed.outcome.as_str()),
            ("absent", "removed")
        );
        let none = act(Action::Uninstall, context(&world, &dirs, None));
        assert_eq!(none.outcome, "nothing_to_remove");
    }

    #[test]
    fn act_hands_a_refusal_its_command() {
        let world = World::new();
        std::fs::create_dir_all(&world.user).unwrap();
        std::fs::write(world.user.join(NAME), "mine").unwrap();
        let dirs = vec![world.user.clone()];
        let refused = act(Action::Install, context(&world, &dirs, None));
        assert_eq!(
            (refused.state.as_str(), refused.outcome.as_str()),
            ("absent", "refused")
        );
        assert!(refused
            .command
            .as_deref()
            .is_some_and(|c| c.starts_with("ln -sf ")));

        let none = act(
            Action::Install,
            Context {
                source: None,
                ..context(&world, &dirs, None)
            },
        );
        assert_eq!(none.outcome, "refused");
        assert!(!none.bundled);
        assert_eq!(none.source, None);
    }

    #[test]
    fn labels_follow_link_presence() {
        assert_eq!(Status::Absent.label(), "Install 'roostctl' Command");
        assert_eq!(
            Status::Installed {
                link: PathBuf::from("/x")
            }
            .label(),
            "Uninstall 'roostctl' Command"
        );
        assert_eq!(
            Status::Broken {
                link: PathBuf::from("/x"),
                target: PathBuf::from("/y")
            }
            .label(),
            "Repair 'roostctl' Command\u{2026}"
        );
    }

    #[test]
    fn the_path_line_spells_home_as_home() {
        assert_eq!(
            path_line(
                Path::new("/Users/me/.local/bin"),
                Some(Path::new("/Users/me"))
            ),
            "export PATH=\"$HOME/.local/bin:$PATH\""
        );
        assert_eq!(
            path_line(Path::new("/opt/bin"), Some(Path::new("/Users/me"))),
            "export PATH=\"/opt/bin:$PATH\""
        );
    }

    #[test]
    fn quoting_survives_a_quote_in_the_path() {
        assert_eq!(quote(Path::new("/a/it's.app")), r"'/a/it'\''s.app'");
    }

    #[test]
    fn find_on_path_skips_relative_entries() {
        let world = World::new();
        let bin = world.source.parent().expect("bin");
        let path = std::env::join_paths([Path::new("relative"), bin]).expect("join");
        assert_eq!(find_on_path(Some(&path)), Some(world.source.clone()));
        assert_eq!(find_on_path(Some(OsStr::new("relative"))), None);
        assert!(on_path(bin, Some(&path)));
        assert!(!on_path(&world.user, Some(&path)));
    }
}
