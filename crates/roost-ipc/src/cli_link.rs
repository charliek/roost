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
    /// moved (`live` false, nothing is there), or it is another Roost's
    /// that still runs (`live` true).
    Stale {
        target: PathBuf,
        live: bool,
    },
    /// Not Roost's: never written, never removed.
    Foreign,
}

/// What `link` is, judged against this Roost's `source`.
pub fn slot(link: &Path, source: &Path) -> Slot {
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
        (resolved, _) => Slot::Stale {
            target,
            live: resolved.is_some(),
        },
    }
}

/// Link presence across the directories — what a menu row is labelled
/// from, never the caller's `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Installed {
        link: PathBuf,
    },
    /// [`Slot::Stale`]'s link, `live` as there.
    Broken {
        link: PathBuf,
        target: PathBuf,
        live: bool,
    },
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
            Slot::Stale { target, live } if broken.is_none() => {
                broken = Some(Status::Broken { link, target, live });
            }
            Slot::Stale { .. } | Slot::Empty | Slot::Foreign => {}
        }
    }
    broken.unwrap_or(Status::Absent)
}

/// A link a Roost made that is not this Roost's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherLink {
    pub link: PathBuf,
    pub target: PathBuf,
    /// [`Slot::Stale`]'s `live`.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created,
    Repaired { was: PathBuf },
    AlreadyThere,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub link: PathBuf,
    pub change: Change,
    /// Links a Roost made in the directories before `link`'s, which this
    /// install left as they are — in practice because Roost can't write
    /// there. A live one is what a bare `roostctl` runs wherever its
    /// directory comes first on `PATH`.
    pub left: Vec<OtherLink>,
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
    /// The link reaches another Roost that is still installed: only
    /// [`Action::Repair`] points it here.
    AnotherRoost {
        source: PathBuf,
        link: PathBuf,
        target: PathBuf,
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
            | Self::AnotherRoost { .. }
            | Self::NoDirectory { .. } => None,
            Self::Occupied { source, links } => {
                links.first().map(|link| link_command(source, link))
            }
            Self::Failed {
                source,
                link,
                removing,
                ..
            } => Some(if *removing {
                remove_command(link)
            } else {
                link_command(source, link)
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
            Self::AnotherRoost {
                source,
                link,
                target,
            } => write!(
                f,
                "{} points at {}, another Roost's roostctl that is still installed, so Roost left \
                 it alone; to point it at this one, choose Repair 'roostctl' Command\u{2026} or \
                 run `{} cli repair`",
                link.display(),
                target.display(),
                source.display()
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

fn link_command(source: &Path, link: &Path) -> String {
    format!("ln -sf {} {}", quote(source), quote(link))
}

fn remove_command(link: &Path) -> String {
    format!("rm {}", quote(link))
}

/// Several by-hand commands as one line, or `None` for none.
fn commands(each: impl Iterator<Item = String>) -> Option<String> {
    let all: Vec<String> = each.collect();
    (!all.is_empty()).then(|| all.join(" && "))
}

/// Whether this user may create and remove entries in `dir`.
pub fn writable(dir: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated pointer this call does not retain.
    dir.is_dir() && unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

fn dir_of(link: &Path) -> &Path {
    link.parent().unwrap_or(Path::new("/"))
}

/// Point a link at `source`: repair a stale link where it is, else create
/// one in the first directory that exists, is writable and is free — the
/// last directory is created if it is missing. A stale link in a directory
/// Roost can't write is passed over like an occupied one, and named in
/// [`Installed::left`]. A link that reaches another Roost still installed
/// is re-pointed only when `repoint` says the user asked for that.
pub fn install(source: &Path, dirs: &[PathBuf], repoint: bool) -> Result<Installed, Refusal> {
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
    let slots: Vec<(PathBuf, Slot)> = dirs
        .iter()
        .map(|dir| {
            let link = dir.join(NAME);
            let found = slot(&link, source);
            (link, found)
        })
        .collect();
    let installed = |index: usize, change: Change| Installed {
        link: slots[index].0.clone(),
        change,
        left: slots[..index]
            .iter()
            .filter_map(|(link, found)| match found {
                Slot::Stale { target, live } => Some(OtherLink {
                    link: link.clone(),
                    target: target.clone(),
                    live: *live,
                }),
                Slot::Empty | Slot::Ours | Slot::Foreign => None,
            })
            .collect(),
    };
    if let Some(index) = slots.iter().position(|(_, found)| *found == Slot::Ours) {
        return Ok(installed(index, Change::AlreadyThere));
    }
    for (index, (link, found)) in slots.iter().enumerate() {
        let Slot::Stale { target, live } = found else {
            continue;
        };
        if !writable(dir_of(link)) {
            continue;
        }
        if *live && !repoint {
            return Err(Refusal::AnotherRoost {
                source: refuse_source(),
                link: link.clone(),
                target: target.clone(),
            });
        }
        return match replace_link(source, link, found) {
            Ok(()) => Ok(installed(
                index,
                Change::Repaired {
                    was: target.clone(),
                },
            )),
            Err(error) => Err(Refusal::Failed {
                source: refuse_source(),
                link: link.clone(),
                removing: false,
                error: error.to_string(),
            }),
        };
    }
    let Some(last) = dirs.len().checked_sub(1) else {
        return Err(Refusal::NoDirectory {
            source: refuse_source(),
        });
    };
    let mut occupied = Vec::new();
    for (index, (link, found)) in slots.iter().enumerate() {
        if index < last && !writable(&dirs[index]) {
            continue;
        }
        match found {
            Slot::Empty => {}
            Slot::Foreign => {
                occupied.push(link.clone());
                continue;
            }
            // Every stale link left by now is in a directory Roost can't
            // write.
            Slot::Ours | Slot::Stale { .. } => continue,
        }
        let made = std::fs::create_dir_all(&dirs[index])
            .and_then(|()| std::os::unix::fs::symlink(source, link));
        return made
            .map(|()| installed(index, Change::Created))
            .map_err(|error| Refusal::Failed {
                source: refuse_source(),
                link: link.clone(),
                removing: false,
                error: error.to_string(),
            });
    }
    if let Some((link, _)) = slots
        .iter()
        .find(|(_, found)| matches!(found, Slot::Stale { .. }))
    {
        return Err(Refusal::Failed {
            source: refuse_source(),
            link: link.clone(),
            removing: false,
            error: format!("Roost can't write to {}", dir_of(link).display()),
        });
    }
    Err(Refusal::Occupied {
        source: refuse_source(),
        links: occupied,
    })
}

/// Re-read `link` right before changing it, and refuse when it is no
/// longer what `decided` saw: the user's own `roostctl` may have landed
/// there since. This narrows that window to the gap between this read and
/// the change; it cannot close it.
fn unchanged(link: &Path, source: &Path, decided: &Slot) -> io::Result<()> {
    if slot(link, source) == *decided {
        Ok(())
    } else {
        Err(io::Error::other("it changed while Roost was working on it"))
    }
}

/// Swap `link` for one to `source` with a rename, so there is no moment
/// with no `roostctl` there.
fn replace_link(source: &Path, link: &Path, decided: &Slot) -> io::Result<()> {
    let dir = link.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let staged = dir.join(format!(".{NAME}.roost-{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(source, &staged)?;
    unchanged(link, source, decided)
        .and_then(|()| std::fs::rename(&staged, link))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staged);
        })
}

fn remove_link(link: &Path, source: &Path, decided: &Slot) -> io::Result<()> {
    unchanged(link, source, decided)?;
    std::fs::remove_file(link)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uninstalled {
    Removed {
        links: Vec<PathBuf>,
        /// The removed links that were not this Roost's.
        others: Vec<OtherLink>,
        /// Links a Roost made that are still there, and why.
        failed: Vec<(PathBuf, String)>,
    },
    NothingThere,
}

/// Remove every link in `dirs` that a Roost made — this one's and stale
/// ones alike. The user's own `roostctl` stays. One that can't be removed
/// is reported and the rest still go; only when none could is it a
/// refusal.
pub fn uninstall(source: &Path, dirs: &[PathBuf]) -> Result<Uninstalled, Refusal> {
    let mut links = Vec::new();
    let mut others = Vec::new();
    let mut failed = Vec::new();
    for dir in dirs {
        let link = dir.join(NAME);
        let found = slot(&link, source);
        if !matches!(found, Slot::Ours | Slot::Stale { .. }) {
            continue;
        }
        match remove_link(&link, source, &found) {
            Ok(()) => {
                if let Slot::Stale { target, live } = found {
                    others.push(OtherLink {
                        link: link.clone(),
                        target,
                        live,
                    });
                }
                links.push(link);
            }
            Err(error) => failed.push((link, error.to_string())),
        }
    }
    if !links.is_empty() {
        return Ok(Uninstalled::Removed {
            links,
            others,
            failed,
        });
    }
    match failed.into_iter().next() {
        Some((link, error)) => Err(Refusal::Failed {
            source: source.to_path_buf(),
            link,
            removing: true,
            error,
        }),
        None => Ok(Uninstalled::NothingThere),
    }
}

/// What a caller asks of [`act`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    /// Link, or repair a link whose app is gone. A link that reaches
    /// another Roost still installed is left alone.
    Install,
    /// [`Action::Install`], and also point a link that reaches another
    /// Roost still installed at this one: the user chose that.
    Repair,
    Uninstall,
}

impl Action {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "status" => Some(Self::Status),
            "install" => Some(Self::Install),
            "repair" => Some(Self::Repair),
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
    /// Directories every new login shell has on `PATH` whatever the
    /// caller's own says ([`login_path_dirs`]).
    pub login_dirs: &'a [PathBuf],
    pub home: Option<&'a Path>,
}

/// Run `action` and say what came of it: the one entry point
/// `app.cli_link`, its palette row and menu item, and `roostctl cli` all
/// call.
pub fn act(action: Action, context: Context<'_>) -> AppCliLinkResult {
    let source = context.source.unwrap_or(Path::new(""));
    let done = match (action, context.source) {
        (Action::Status, _) => Ok(Done::Status),
        (Action::Install | Action::Repair, None) => Err(Refusal::NoRoostctl),
        (Action::Install, Some(source)) => {
            install(source, context.dirs, false).map(Done::Installed)
        }
        (Action::Repair, Some(source)) => install(source, context.dirs, true).map(Done::Installed),
        (Action::Uninstall, _) => uninstall(source, context.dirs).map(Done::Uninstalled),
    };
    let after = status(source, context.dirs);
    let (outcome, message, command, path_line) = match done {
        Ok(Done::Status) => ("status", describe(&after), None, None),
        Ok(Done::Installed(installed)) => {
            let dir = dir_of(&installed.link);
            let reachable =
                on_path(dir, context.path_var) || context.login_dirs.iter().any(|on| on == dir);
            let path_line = (!reachable).then(|| path_line(dir, context.home));
            let (outcome, verb) = match &installed.change {
                Change::Created => ("created", "Installed"),
                Change::Repaired { .. } => ("repaired", "Repaired"),
                Change::AlreadyThere => ("already_installed", "Already installed"),
            };
            let mut message = format!(
                "{verb} 'roostctl': {} → {}",
                installed.link.display(),
                source.display()
            );
            for other in &installed.left {
                message.push_str(&format!(". {}", left_in_front(other, dir)));
            }
            let command = commands(installed.left.iter().map(|other| {
                if other.live {
                    link_command(source, &other.link)
                } else {
                    remove_command(&other.link)
                }
            }));
            (outcome, message, command, path_line)
        }
        Ok(Done::Uninstalled(Uninstalled::Removed {
            links,
            others,
            failed,
        })) => {
            let mut message = format!("Uninstalled 'roostctl': removed {}", join_paths(&links));
            for other in &others {
                message.push_str(&format!(
                    ". {} was {}",
                    other.link.display(),
                    if other.live {
                        format!("another Roost's: it pointed at {}", other.target.display())
                    } else {
                        format!(
                            "a link to {}, which no longer exists",
                            other.target.display()
                        )
                    }
                ));
            }
            for (link, error) in &failed {
                message.push_str(&format!(". Could not remove {}: {error}", link.display()));
            }
            let command = commands(failed.iter().map(|(link, _)| remove_command(link)));
            ("removed", message, command, None)
        }
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

/// A link an install left in a directory before `ours`.
fn left_in_front(other: &OtherLink, ours: &Path) -> String {
    let dir = dir_of(&other.link);
    let mut text = if other.live {
        format!(
            "{} still points at {}, another Roost's roostctl, which a terminal whose PATH lists \
             {} before {} runs instead",
            other.link.display(),
            other.target.display(),
            dir.display(),
            ours.display()
        )
    } else {
        format!(
            "{} still points at {}, which no longer exists",
            other.link.display(),
            other.target.display()
        )
    };
    if !writable(dir) {
        text.push_str(&format!(" (Roost can't write to {})", dir.display()));
    }
    text
}

fn describe(status: &Status) -> String {
    match status {
        Status::Installed { link } => format!("'roostctl' is installed at {}", link.display()),
        Status::Broken { link, target, live } => format!(
            "The 'roostctl' link at {} points at {}, {}",
            link.display(),
            target.display(),
            if *live {
                "another Roost's roostctl"
            } else {
                "which no longer exists"
            }
        ),
        Status::Absent => "'roostctl' is not installed".to_string(),
    }
}

/// What a macOS login shell's `path_helper` puts on `PATH`: the lines of
/// `/etc/paths`, then of each file in `/etc/paths.d` by name. A Roost
/// opened from Finder or the Dock has launchd's `PATH`, which lacks them,
/// but every new Terminal window has them. Empty off macOS.
pub fn login_path_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        path_helper_dirs(Path::new("/etc"))
    } else {
        Vec::new()
    }
}

/// [`login_path_dirs`] read from `etc`.
pub fn path_helper_dirs(etc: &Path) -> Vec<PathBuf> {
    let mut files = vec![etc.join("paths")];
    if let Ok(entries) = std::fs::read_dir(etc.join("paths.d")) {
        let mut more: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        more.sort();
        files.extend(more);
    }
    files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| line.starts_with('/'))
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect()
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

    fn made(link: &Path, change: Change) -> Installed {
        Installed {
            link: link.to_path_buf(),
            change,
            left: Vec::new(),
        }
    }

    fn removed(links: &[PathBuf]) -> Uninstalled {
        Uninstalled::Removed {
            links: links.to_vec(),
            others: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// `dir` read-only until dropped, so a failed assertion still leaves
    /// a temp root the test harness can delete.
    struct ReadOnly<'a>(&'a Path);

    impl<'a> ReadOnly<'a> {
        fn new(dir: &'a Path) -> Self {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
            Self(dir)
        }
    }

    impl Drop for ReadOnly<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
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
            install(&world.source, &world.dirs(), false),
            Ok(made(&link, Change::Created))
        );
        assert_eq!(std::fs::read_link(&link).expect("link"), world.source);
        assert!(!world.user.exists(), "the user dir is not made when unused");
        assert_eq!(
            status(&world.source, &world.dirs()),
            Status::Installed { link: link.clone() }
        );
        assert_eq!(
            install(&world.source, &world.dirs(), false),
            Ok(made(&link, Change::AlreadyThere))
        );
    }

    #[test]
    fn install_falls_back_to_the_user_dir_and_creates_it() {
        let world = World::new();
        let link = world.user.join(NAME);
        assert_eq!(
            install(&world.source, &world.dirs(), false),
            Ok(made(&link, Change::Created))
        );
        assert!(link.is_symlink());
    }

    #[test]
    fn a_read_only_system_dir_is_passed_over() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        let result = {
            let _guard = ReadOnly::new(&world.system);
            install(&world.source, &world.dirs(), false)
        };
        assert_eq!(result, Ok(made(&world.user.join(NAME), Change::Created)));
    }

    #[test]
    fn a_regular_file_is_never_overwritten() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        std::fs::create_dir_all(&world.user).expect("user");
        for dir in [&world.system, &world.user] {
            std::fs::write(dir.join(NAME), "mine").expect("file");
        }
        let refused = install(&world.source, &world.dirs(), false).expect_err("occupied");
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
            install(&world.source, &world.dirs(), false),
            Ok(made(&world.user.join(NAME), Change::Created))
        );
        assert_eq!(
            std::fs::read_link(world.system.join(NAME)).expect("kept"),
            dev
        );
        assert_eq!(
            uninstall(&world.source, &world.dirs()),
            Ok(removed(&[world.user.join(NAME)]))
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
                target: gone.clone(),
                live: false,
            }
        );
        assert_eq!(
            install(&world.source, &world.dirs(), false),
            Ok(made(&link, Change::Repaired { was: gone }))
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
        let gone = world.root.join("Gone.app/Contents/Resources/bin/roostctl");
        symlink(&gone, world.system.join(NAME)).expect("stale");
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
                links: vec![world.system.join(NAME), world.user.join(NAME)],
                others: vec![OtherLink {
                    link: world.system.join(NAME),
                    target: gone,
                    live: false,
                }],
                failed: Vec::new(),
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
            install(&plain, &world.dirs(), false),
            Err(Refusal::NotBundled { .. })
        ));
        assert!(!world.user.exists());
    }

    #[test]
    fn install_refuses_a_translocated_bundle() {
        let world = World::new();
        let moved = bundle(&world.root.join("AppTranslocation/X/d"), "Roost.app");
        let refused = install(&moved, &world.dirs(), false).expect_err("translocated");
        assert!(matches!(refused, Refusal::Translocated { .. }));
        assert_eq!(refused.command(), None);
        assert!(!world.user.exists());
    }

    #[test]
    fn a_write_failure_hands_back_the_command_that_does_it_by_hand() {
        let world = World::new();
        std::fs::create_dir_all(&world.user).expect("user");
        let refused = {
            let _guard = ReadOnly::new(&world.user);
            install(&world.source, &world.dirs(), false).expect_err("read-only")
        };
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
            login_dirs: &[],
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
        for live in [false, true] {
            assert_eq!(
                Status::Broken {
                    link: PathBuf::from("/x"),
                    target: PathBuf::from("/y"),
                    live,
                }
                .label(),
                "Repair 'roostctl' Command\u{2026}"
            );
        }
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

    /// A stale link in a `/usr/local/bin` this user can't write — a link
    /// made with `sudo`, say — is passed over for `~/.local/bin`, and the
    /// result names it. Uninstall then removes what it can and names what
    /// it can't.
    #[test]
    fn a_stale_link_roost_cannot_write_is_passed_over_and_named() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        let gone = world.root.join("Old.app/Contents/Resources/bin/roostctl");
        let stale = world.system.join(NAME);
        symlink(&gone, &stale).expect("stale");
        let left = OtherLink {
            link: stale.clone(),
            target: gone.clone(),
            live: false,
        };
        let _guard = ReadOnly::new(&world.system);

        assert_eq!(
            install(&world.source, &world.dirs(), false),
            Ok(Installed {
                left: vec![left.clone()],
                ..made(&world.user.join(NAME), Change::Created)
            })
        );
        assert_eq!(std::fs::read_link(&stale).expect("kept"), gone);
        assert_eq!(
            status(&world.source, &world.dirs()),
            Status::Installed {
                link: world.user.join(NAME)
            }
        );
        assert_eq!(
            install(&world.source, &world.dirs(), false),
            Ok(Installed {
                left: vec![left],
                ..made(&world.user.join(NAME), Change::AlreadyThere)
            })
        );

        let Ok(Uninstalled::Removed {
            links,
            others,
            failed,
        }) = uninstall(&world.source, &world.dirs())
        else {
            panic!("a partial removal");
        };
        assert_eq!(links, vec![world.user.join(NAME)]);
        assert_eq!(others, Vec::new());
        assert_eq!(
            failed.iter().map(|(link, _)| link).collect::<Vec<_>>(),
            vec![&stale]
        );
        assert_eq!(std::fs::read_link(&stale).expect("kept"), gone);
        let Err(refused) = uninstall(&world.source, &world.dirs()) else {
            panic!("nothing it can remove is a refusal");
        };
        assert_eq!(refused.command(), Some(format!("rm '{}'", stale.display())));
    }

    /// With the user dir taken too, there is nowhere to go: the refusal
    /// names the stale link and why Roost could not change it.
    #[test]
    fn a_stale_link_roost_cannot_write_with_nowhere_else_is_a_refusal() {
        let world = World::new();
        std::fs::create_dir_all(&world.system).expect("system");
        std::fs::create_dir_all(&world.user).expect("user");
        std::fs::write(world.user.join(NAME), "mine").expect("mine");
        let stale = world.system.join(NAME);
        symlink(
            world.root.join("Old.app/Contents/Resources/bin/roostctl"),
            &stale,
        )
        .expect("stale");
        let refused = {
            let _guard = ReadOnly::new(&world.system);
            install(&world.source, &world.dirs(), false).expect_err("nowhere")
        };
        assert_eq!(
            refused.to_string(),
            format!(
                "could not write {}: Roost can't write to {}",
                stale.display(),
                world.system.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(world.user.join(NAME)).expect("kept"),
            "mine"
        );
    }

    #[test]
    fn act_says_what_a_link_it_could_not_change_means() {
        let world = World::new();
        let dirs = world.dirs();
        let other = bundle(&world.root, "Roost-Iced.app");
        std::fs::create_dir_all(&world.system).expect("system");
        let stale = world.system.join(NAME);
        symlink(&other, &stale).expect("other");
        let _guard = ReadOnly::new(&world.system);

        let created = act(Action::Install, context(&world, &dirs, None));
        assert_eq!(created.outcome, "created");
        assert!(
            created.message.contains(&format!(
                "{} still points at {}, another Roost's roostctl, which a terminal whose PATH \
                 lists {} before {} runs instead (Roost can't write to {})",
                stale.display(),
                other.display(),
                world.system.display(),
                world.user.display(),
                world.system.display()
            )),
            "{}",
            created.message
        );
        assert_eq!(
            created.command,
            Some(format!(
                "ln -sf '{}' '{}'",
                world.source.display(),
                stale.display()
            ))
        );

        let removed = act(Action::Uninstall, context(&world, &dirs, None));
        assert_eq!(
            (removed.outcome.as_str(), removed.state.as_str()),
            ("removed", "broken")
        );
        assert!(
            removed
                .message
                .contains(&format!(". Could not remove {}: ", stale.display())),
            "{}",
            removed.message
        );
        assert_eq!(removed.command, Some(format!("rm '{}'", stale.display())));
    }

    /// `install` repairs a link whose app is gone without asking, but a
    /// link to another Roost that still runs is that Roost's working
    /// command: only `repair` re-points it.
    #[test]
    fn a_link_to_another_roost_still_installed_is_repointed_only_on_repair() {
        let world = World::new();
        let dirs = world.dirs();
        let other = bundle(&world.root, "Roost-Iced.app");
        std::fs::create_dir_all(&world.system).expect("system");
        let link = world.system.join(NAME);
        symlink(&other, &link).expect("other");
        assert_eq!(
            status(&world.source, &dirs),
            Status::Broken {
                link: link.clone(),
                target: other.clone(),
                live: true,
            }
        );

        let refused = install(&world.source, &dirs, false).expect_err("another Roost");
        assert_eq!(
            refused,
            Refusal::AnotherRoost {
                source: world.source.clone(),
                link: link.clone(),
                target: other.clone(),
            }
        );
        assert!(
            refused
                .to_string()
                .ends_with(&format!("run `{} cli repair`", world.source.display())),
            "{refused}"
        );
        assert_eq!(std::fs::read_link(&link).expect("kept"), other);
        assert!(!world.user.exists(), "no second link beside it");
        let via_act = act(Action::Install, context(&world, &dirs, None));
        assert_eq!(
            (via_act.outcome.as_str(), via_act.state.as_str()),
            ("refused", "broken")
        );

        let repaired = act(Action::Repair, context(&world, &dirs, None));
        assert_eq!(
            (repaired.outcome.as_str(), repaired.state.as_str()),
            ("repaired", "installed")
        );
        assert_eq!(std::fs::read_link(&link).expect("link"), world.source);
    }

    #[test]
    fn uninstall_names_each_link_that_was_another_roosts() {
        let world = World::new();
        let dirs = world.dirs();
        let other = bundle(&world.root, "Roost-Iced.app");
        let gone = world.root.join("Gone.app/Contents/Resources/bin/roostctl");
        std::fs::create_dir_all(&world.system).expect("system");
        std::fs::create_dir_all(&world.user).expect("user");
        symlink(&other, world.system.join(NAME)).expect("other");
        symlink(&gone, world.user.join(NAME)).expect("gone");

        let removed = act(Action::Uninstall, context(&world, &dirs, None));
        assert_eq!(removed.outcome, "removed");
        assert_eq!(
            removed.message,
            format!(
                "Uninstalled 'roostctl': removed {sys}, {user}. {sys} was another Roost's: it \
                 pointed at {other}. {user} was a link to {gone}, which no longer exists",
                sys = world.system.join(NAME).display(),
                user = world.user.join(NAME).display(),
                other = other.display(),
                gone = gone.display(),
            )
        );
        assert_eq!(removed.command, None);
    }

    /// Roost decides from what a slot held, and changes it a moment
    /// later: a slot that turned into the user's own file in between is
    /// left alone.
    #[test]
    fn a_slot_that_changed_after_the_decision_is_left_alone() {
        let world = World::new();
        std::fs::create_dir_all(&world.user).expect("user");
        let link = world.user.join(NAME);
        symlink(
            world.root.join("Old.app/Contents/Resources/bin/roostctl"),
            &link,
        )
        .expect("stale");
        let decided = slot(&link, &world.source);
        assert!(matches!(decided, Slot::Stale { live: false, .. }));
        std::fs::remove_file(&link).expect("unlink");
        std::fs::write(&link, "mine").expect("mine");

        assert!(replace_link(&world.source, &link, &decided).is_err());
        assert_eq!(std::fs::read_to_string(&link).expect("kept"), "mine");
        assert!(remove_link(&link, &world.source, &decided).is_err());
        assert_eq!(std::fs::read_to_string(&link).expect("kept"), "mine");
        let names: Vec<_> = std::fs::read_dir(&world.user)
            .expect("dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![OsStr::new(NAME).to_os_string()]);
    }

    #[test]
    fn path_helper_dirs_read_etc_paths_then_paths_d_by_name() {
        let etc = tempfile::tempdir().expect("etc");
        std::fs::write(etc.path().join("paths"), "/usr/local/bin\n/usr/bin\n\n").expect("paths");
        std::fs::create_dir(etc.path().join("paths.d")).expect("paths.d");
        std::fs::write(etc.path().join("paths.d/20-b"), "/opt/b\n").expect("b");
        std::fs::write(etc.path().join("paths.d/10-a"), " /opt/a \nrelative\n").expect("a");
        assert_eq!(
            path_helper_dirs(etc.path()),
            ["/usr/local/bin", "/usr/bin", "/opt/a", "/opt/b"]
                .map(PathBuf::from)
                .to_vec()
        );
        assert_eq!(
            path_helper_dirs(&etc.path().join("none")),
            Vec::<PathBuf>::new()
        );
    }

    /// A Roost opened from Finder has launchd's `PATH`; a directory every
    /// new login shell has (macOS's `/etc/paths`) is on `PATH` all the
    /// same, so there is no line to add.
    #[test]
    fn a_directory_a_login_shell_has_needs_no_path_line() {
        let world = World::new();
        let dirs = world.dirs();
        let launchd = OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin");
        let login = [world.user.clone()];
        let created = act(
            Action::Install,
            Context {
                login_dirs: &login,
                ..context(&world, &dirs, Some(launchd))
            },
        );
        assert_eq!(created.outcome, "created");
        assert_eq!(created.path_line, None);
    }

    #[test]
    fn actions_parse_by_name() {
        for (name, action) in [
            ("status", Action::Status),
            ("install", Action::Install),
            ("repair", Action::Repair),
            ("uninstall", Action::Uninstall),
        ] {
            assert_eq!(Action::parse(name), Some(action));
        }
        assert_eq!(Action::parse("force"), None);
    }
}
