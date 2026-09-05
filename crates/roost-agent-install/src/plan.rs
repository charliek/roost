//! The plan/apply split, and the one function that performs a plan.
//!
//! Every writer in this crate produces an [`InstallPlan`] first: the
//! exact bytes it would put where, plus the pre-image it read them
//! against. Nothing in the plan touches the disk. [`apply`] is the only
//! thing that writes, and it does so under two rules — re-check the
//! digest before each rename, and put everything back if a later write
//! in the same plan fails.
//!
//! That second rule is why codex works. It owns two files plus a state
//! table: `hooks.json` carries the handlers, `config.toml` carries the
//! trust hashes that stop codex asking about them. A `hooks.json`
//! written without its `config.toml` is precisely the state the design
//! exists to avoid — a review dialog on the next launch — so a failure
//! on the second file rolls the first one back.

use std::path::PathBuf;

use roost_agent::Agent;

use crate::error::{InstallError, SkipReason, Warning};
use crate::write::{self, Image};

/// Whether a plan puts Roost's entries in or takes them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Install,
    Uninstall,
}

/// One file's worth of change: what was there, and what should be.
#[derive(Debug, Clone)]
pub struct FileEdit {
    pub image: Image,
    /// `None` removes the file.
    pub after: Option<Vec<u8>>,
}

impl FileEdit {
    pub fn path(&self) -> &std::path::Path {
        &self.image.path
    }
}

/// Everything [`crate::plan`] decided about one agent.
#[derive(Debug)]
pub struct InstallPlan {
    pub agent: Agent,
    pub intent: Intent,
    /// Empty when the wired state already matches the desired one.
    /// **This** is the idempotency assertion — a second `ensure` plans
    /// nothing, whatever the file's mtime says.
    pub edits: Vec<FileEdit>,
    /// Set when Roost deliberately left the agent alone.
    pub skipped: Option<SkipReason>,
    /// Facts about the user's files worth reporting but not acting on.
    pub warnings: Vec<Warning>,
    /// The files this agent's install owns, for the state record.
    pub files: Vec<PathBuf>,
}

impl InstallPlan {
    pub fn skip(agent: Agent, intent: Intent, reason: SkipReason) -> InstallPlan {
        InstallPlan {
            agent,
            intent,
            edits: Vec::new(),
            skipped: Some(reason),
            warnings: Vec::new(),
            files: Vec::new(),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.edits.is_empty()
    }
}

/// What [`apply`] actually did.
#[derive(Debug)]
pub struct Applied {
    pub agent: Agent,
    pub files: Vec<PathBuf>,
    pub written: usize,
}

/// The harness jail, resolved once at the boundary.
///
/// A parameter rather than an `std::env::var` call inside [`apply`]:
/// the refusal has to be testable, and mutating process-global
/// environment from parallel test threads is not a test, it is a race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    pub test_mode: bool,
    pub forced: bool,
}

impl Guard {
    /// What the real binaries pass.
    pub fn from_env() -> Guard {
        Guard {
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            forced: std::env::var("ROOST_AGENT_HOOKS_FORCE").as_deref() == Ok("1"),
        }
    }

    /// Not under a harness at all.
    pub const PERMITTED: Guard = Guard {
        test_mode: false,
        forced: false,
    };

    pub fn check(self) -> Result<(), InstallError> {
        if self.test_mode && !self.forced {
            return Err(InstallError::TestModeRefused);
        }
        Ok(())
    }
}

/// Perform a plan, or leave the disk exactly as it was.
///
/// Callers hold [`crate::write::lock`] across `plan` **and** this, so
/// two ensures cannot interleave a read and a write. The digest
/// re-check below is the second line: it turns whatever still slips
/// through — another Roost on another machine sharing a synced
/// directory, Claude rewriting its own settings — into a reported skip
/// rather than a lost update.
pub fn apply(plan: &InstallPlan, guard: Guard) -> Result<Applied, InstallError> {
    guard.check()?;

    let mut done: Vec<Done<'_>> = Vec::new();
    for edit in &plan.edits {
        let result = match &edit.after {
            // Stage first, look second: everything slow — writing the
            // replacement, `fsync` — happens before the digest check, so
            // the gap between checking and renaming is one syscall wide.
            Some(bytes) => write::stage(&edit.image.target, bytes, edit.image.create_mode)
                .and_then(|staged| {
                    write::check_unchanged(&edit.image)?;
                    staged.commit(&edit.image.target)
                }),
            None => {
                write::check_unchanged(&edit.image).and_then(|()| write::remove(&edit.image.target))
            }
        };
        if let Err(e) = result {
            return Err(unwind(e, &done));
        }
        done.push(Done {
            edit,
            wrote: edit.after.as_deref().map(write::digest),
        });
    }

    Ok(Applied {
        agent: plan.agent,
        files: plan.files.clone(),
        written: plan.edits.len(),
    })
}

/// One edit this apply already performed, and the digest of what it put
/// there — the proof a rollback needs before it overwrites anything.
struct Done<'a> {
    edit: &'a FileEdit,
    /// `None` when the edit removed the file.
    wrote: Option<[u8; 32]>,
}

/// Report `cause`, naming anything the rollback could not put back.
fn unwind(cause: InstallError, done: &[Done<'_>]) -> InstallError {
    let unrestored = rollback(done);
    if unrestored.is_empty() {
        return cause;
    }
    InstallError::RollbackFailed {
        source: Box::new(cause),
        paths: unrestored,
    }
}

/// Put back what this plan already wrote, newest first. Answers with the
/// files it could **not** put back.
///
/// A restore is a write like any other, so it gets the same courtesy the
/// forward path does: if what is on disk is no longer what this apply
/// left there, somebody else has written since and their bytes are not
/// ours to discard. Such a file is named rather than overwritten — and
/// so is one the restore genuinely failed on, because a `hooks.json`
/// left behind without its trust hashes is a review dialog on every
/// codex launch and the user has to be told.
fn rollback(done: &[Done<'_>]) -> Vec<PathBuf> {
    let mut unrestored = Vec::new();
    for step in done.iter().rev() {
        let image = &step.edit.image;
        let now = match std::fs::read(&image.target) {
            Ok(bytes) => Some(write::digest(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                unrestored.push(image.target.clone());
                continue;
            }
        };
        if now != step.wrote {
            unrestored.push(image.target.clone());
            continue;
        }
        let restored = match &image.bytes {
            Some(before) => write::write_atomic(&image.target, before, image.create_mode),
            None => write::remove(&image.target),
        };
        if restored.is_err() {
            unrestored.push(image.target.clone());
        }
    }
    unrestored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{read_image, PRIVATE_MODE};

    fn edit(path: &std::path::Path, after: Option<&str>) -> FileEdit {
        FileEdit {
            image: read_image(path, PRIVATE_MODE).unwrap(),
            after: after.map(|s| s.as_bytes().to_vec()),
        }
    }

    fn plan_of(edits: Vec<FileEdit>) -> InstallPlan {
        InstallPlan {
            agent: Agent::Codex,
            intent: Intent::Install,
            edits,
            skipped: None,
            warnings: Vec::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn the_test_mode_refusal_needs_an_explicit_override() {
        assert!(Guard::PERMITTED.check().is_ok());
        assert!(matches!(
            Guard {
                test_mode: true,
                forced: false
            }
            .check(),
            Err(InstallError::TestModeRefused)
        ));
        assert!(Guard {
            test_mode: true,
            forced: true
        }
        .check()
        .is_ok());
    }

    /// The jail has to hold before a single byte moves, not after.
    #[test]
    fn a_refused_apply_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let plan = plan_of(vec![edit(&path, Some("written"))]);

        let err = apply(
            &plan,
            Guard {
                test_mode: true,
                forced: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::TestModeRefused));
        assert!(!path.exists());
    }

    /// codex's two-file case: `hooks.json` lands, `config.toml` cannot,
    /// and the first file goes back to what it was. A `hooks.json`
    /// without its trust hashes is a review dialog on the next launch —
    /// exactly the state this design exists to prevent.
    #[test]
    fn a_failed_second_write_rolls_the_first_one_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("hooks.json");
        std::fs::write(&first, "original\n").unwrap();

        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let second = locked.join("config.toml");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let plan = plan_of(vec![
            edit(&first, Some("rewritten\n")),
            edit(&second, Some("trusted\n")),
        ]);
        let err = apply(&plan, Guard::PERMITTED).unwrap_err();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(err, InstallError::ReadOnly { .. }), "{err:?}");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "original\n");
    }

    /// Same rollback, for a file the plan created rather than replaced:
    /// putting it back means removing it.
    #[test]
    fn a_rollback_removes_a_file_the_plan_created() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("hooks.json");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let plan = plan_of(vec![
            edit(&first, Some("new\n")),
            edit(&locked.join("config.toml"), Some("trusted\n")),
        ]);
        let err = apply(&plan, Guard::PERMITTED).unwrap_err();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(err, InstallError::ReadOnly { .. }), "{err:?}");
        assert!(!first.exists(), "the created file survived the rollback");
    }

    /// A rollback is a write, and a write that lands on somebody else's
    /// edit is the thing this crate exists not to do. The pre-image only
    /// goes back when what is there is still what we put there.
    #[test]
    fn a_rollback_will_not_overwrite_an_edit_made_since_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "before\n").unwrap();
        let done = edit(&path, Some("ours\n"));

        std::fs::write(&path, "ours\n").unwrap();
        std::fs::write(&path, "the user got here first\n").unwrap();

        let unrestored = rollback(&[Done {
            edit: &done,
            wrote: Some(write::digest(b"ours\n")),
        }]);
        assert_eq!(unrestored, vec![done.image.target.clone()]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the user got here first\n"
        );
    }

    /// The same rollback when nothing has changed: the pre-image goes
    /// back, and nothing is reported.
    #[test]
    fn a_rollback_restores_what_it_still_recognises() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "before\n").unwrap();
        let done = edit(&path, Some("ours\n"));
        std::fs::write(&path, "ours\n").unwrap();

        let unrestored = rollback(&[Done {
            edit: &done,
            wrote: Some(write::digest(b"ours\n")),
        }]);
        assert!(unrestored.is_empty(), "{unrestored:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
    }

    /// And what `apply` hands back says so, rather than reporting only
    /// the write that failed and leaving the user to discover the rest.
    #[test]
    fn an_incomplete_rollback_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "before\n").unwrap();
        let done = edit(&path, Some("ours\n"));
        std::fs::write(&path, "somebody else got here\n").unwrap();

        let cause = InstallError::ReadOnly {
            path: dir.path().join("config.toml"),
        };
        let reported = unwind(
            cause,
            &[Done {
                edit: &done,
                wrote: Some(write::digest(b"ours\n")),
            }],
        );

        let InstallError::RollbackFailed { source, paths } = reported else {
            panic!("the rollback failure was swallowed: {reported:?}");
        };
        assert!(matches!(*source, InstallError::ReadOnly { .. }), "{source}");
        assert_eq!(paths, vec![done.image.target.clone()]);
        // And it says both halves out loud, because the machine is now
        // in a state neither Roost nor the user chose.
        let rendered = InstallError::RollbackFailed { source, paths }.to_string();
        assert!(rendered.contains("not writable"), "{rendered}");
        assert!(rendered.contains("could not be put back"), "{rendered}");
    }

    #[test]
    fn a_file_that_changed_since_the_plan_is_skipped_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "as planned\n").unwrap();
        let plan = plan_of(vec![edit(&path, Some("ours\n"))]);

        std::fs::write(&path, "someone else wrote this\n").unwrap();
        let err = apply(&plan, Guard::PERMITTED).unwrap_err();

        assert!(
            matches!(err, InstallError::ChangedUnderneath { .. }),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "someone else wrote this\n"
        );
    }
}
