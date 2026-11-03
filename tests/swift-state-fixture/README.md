# Swift-written `state.json`

`state.json` here was written by the Swift + AppKit `Roost.app` v0.0.27,
the last release that shipped it (2026-11-03, plan 089). Its socket,
state and log paths are the ones the iced `Roost.app` now uses, so an
upgrading user's first launch of the iced app reads this file as its own.

The file is committed verbatim: a bundled v0.0.27 build, run with a
scratch `ROOST_STATE_DIR` and driven over IPC, created two projects and
three tabs on top of the seeded one, then quit. It holds:

- the seeded `Untitled 1` project (id 2, `cwd: ""`) with one tab;
- `Alpha` (id 3) with two tabs, the second one last shown;
- `Beta` (id 4) with one tab, the active project;
- `next_id` 8, and empty `hosts` and `recent_hosts`.

Loader:

- Rust: `a_state_file_the_swift_app_wrote_restores_whole` in
  `crates/roost-engine/src/application.rs` (`cargo test -p
  roost-engine`). It opens a copy through `Workspace::open` and the
  launch's restore, and asserts the projects, the tab layout, the
  selection and the id counter all come back.

Never edit the file to make the test pass: it is the record of what the
Swift app wrote. A loader change that breaks it breaks every upgrading
Swift user's state.
