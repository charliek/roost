//! `Workspace::open_tab`'s empty-cwd resolution (#266): requested cwd →
//! the project's cwd → `$HOME` → `/`.
//!
//! Lives in its own integration-test binary because it mutates the
//! process-global `HOME` env var — the same reason `pty_env_terminfo.rs`
//! is split out. Unlike that file, three tests here all touch `HOME`, so
//! they additionally serialize on a mutex rather than relying on being
//! the file's only test.

use std::ffi::OsString;
use std::sync::Mutex;

use roost_engine::Workspace;

static HOME_LOCK: Mutex<()> = Mutex::new(());

// Lock acquisition below recovers from poisoning: a panicking test
// must not take the other two down with it via a poisoned mutex — an
// assertion failure in one should not be masked as a lock error in
// the next.

/// Sets (or clears) `HOME` for its lifetime and restores whatever the
/// var held beforehand on drop, so a failing assertion can't leak a
/// clobbered `HOME` into a later test.
struct HomeVar {
    prev: Option<OsString>,
}

impl HomeVar {
    fn set(value: &str) -> Self {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", value);
        HomeVar { prev }
    }

    fn clear() -> Self {
        let prev = std::env::var_os("HOME");
        std::env::remove_var("HOME");
        HomeVar { prev }
    }
}

impl Drop for HomeVar {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[test]
fn open_tab_with_an_empty_cwd_uses_the_projects_cwd() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = HomeVar::set("/should-not-be-used");

    let workspace = Workspace::new();
    let project = workspace.create_project("proj", "/usr/local").unwrap();

    let tab = workspace.open_tab(project.id, "", "").unwrap();

    assert_eq!(tab.cwd, "/usr/local");
    assert_eq!(tab.title, "local");
}

#[test]
fn open_tab_falls_back_to_home_when_the_project_has_none() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = HomeVar::set("/home/tester");

    let workspace = Workspace::new();
    // The real case, not a synthetic one: `ensure_default_project("")` is
    // what `ops::TAB_OPEN`, the facade, and Swift's `IPCHandlerImpl` call
    // for `project_id == 0`, so a fresh workspace's Default project
    // persists `cwd: ""` and every later bare `tab.open` into it resolves
    // to `$HOME`. That matches Mac and is correct, not a bug to fix.
    let project_id = workspace.ensure_default_project("");

    let tab = workspace.open_tab(project_id, "", "").unwrap();

    assert_eq!(tab.cwd, "/home/tester");
    assert_eq!(tab.title, "tester");
}

#[test]
fn open_tab_falls_back_to_slash_without_home() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = HomeVar::clear();

    let workspace = Workspace::new();
    let project_id = workspace.ensure_default_project("");

    let tab = workspace.open_tab(project_id, "", "").unwrap();

    assert_eq!(tab.cwd, "/");
    assert_eq!(tab.title, "/");
}
