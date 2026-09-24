"""Plan 070 D6 (#531): a startup error after the engine runtime exists
exits the UI with the error; it never wedges.

Under `local-backend = in-process`, hydrate spawns the first tab's shell
before the IPC bind, so a socket path longer than `sun_path` fails the bind
after a shell exists. The UI is launched directly, not through `ui.launch`,
which waits for a socket this UI can never bind.

The abort hangs the shell up within a millisecond of spawning it — usually
before the shell has run a single line — so nothing the shell writes can
prove it was spawned. The persisted tab row can:
`application::spawn_for_row` removes the row again when the spawn fails,
so a tab in `state.json` after the exit means a shell was exec'd before
the error.

Every process the UI starts inherits a marker from its environment, so
"no shell left behind" is a `/proc` sweep for that marker rather than a
pid the shell would have had to report.
"""

from __future__ import annotations

import contextlib
import json
import os
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

import pytest
import ui
from client import scaled_timeout

if sys.platform != "linux":
    pytest.skip(
        "the boot-failure lane is Linux-only (it sweeps /proc for the "
        "processes it started and sizes its socket path against Linux's "
        "sun_path)",
        allow_module_level=True,
    )

# `app.rs`'s own `.context(...)` for the failing step, never Rust std's
# SUN_LEN wording.
BIND_CONTEXT = "bind Iced IPC server"
# `sizeof(sockaddr_un.sun_path)` on Linux.
SUN_PATH_MAX = 108
MARKER_ENV = "ROOST_BOOT_FAILURE_MARKER"
# D6's `BOOT_ABORT_DEADLINE`, plus room for `shutdown_all`'s SIGKILL tail.
ABORT_DEADLINE = 2.0
ABORT_MARGIN = 1.0


def _marked_pids(marker: bytes) -> list[int]:
    """Live processes carrying `marker` in their environment (a zombie's
    environ reads empty)."""
    pids = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            environ = (entry / "environ").read_bytes()
        except OSError:
            continue
        if marker in environ.split(b"\0"):
            pids.append(int(entry.name))
    return pids


def _wait_unmarked(marker: bytes, timeout: float) -> list[int]:
    """Poll until no process carries `marker`; return what is left."""
    deadline = time.monotonic() + timeout
    while (left := _marked_pids(marker)) and time.monotonic() < deadline:
        time.sleep(0.02)
    return left


def _persisted_tabs(state_dir: Path) -> int:
    try:
        state = json.loads((state_dir / "state.json").read_text())
    except FileNotFoundError:
        return 0
    return sum(len(project.get("tabs", [])) for project in state.get("projects", []))


def _tail(log: Path, lines: int = 40) -> str:
    try:
        text = log.read_text(errors="replace")
    except FileNotFoundError:
        return "(no UI output captured)"
    return "UI output (tail):\n" + "\n".join(text.splitlines()[-lines:])


def test_a_bind_failure_after_hydrate_exits_and_reaps_the_shell(tmp_path):
    binary, _ = ui.rust_binary_path("iced")
    if not binary.is_file():
        raise FileNotFoundError(f"no roost-iced binary at {binary}; build it first")

    spec = ui.TARGET_SPECS["iced"]
    runtime_dir = tmp_path / ("r" * 100)
    runtime_dir.mkdir(mode=0o700)
    socket = runtime_dir / spec.linux_namespace / "roost.sock"
    assert len(os.fsencode(socket)) > SUN_PATH_MAX, socket

    config = tmp_path / "config.conf"
    config.write_text("agent-hooks = off\nlocal-backend = in-process\n")
    # An ignored signal stays ignored across `exec`: when the stub gets to
    # run at all, only a SIGKILL removes it.
    stub = tmp_path / "stub-shell"
    stub.write_text("#!/bin/sh\ntrap '' HUP\nexec sleep 300\n")
    stub.chmod(0o755)
    dirs = {name: tmp_path / name for name in ("data", "state", "cache", "state-dir")}
    for path in dirs.values():
        path.mkdir()

    marker_value = uuid.uuid4().hex
    marker = f"{MARKER_ENV}={marker_value}".encode()
    env = dict(os.environ)
    for leaked in ui._UI_ENV_SANITIZE:
        env.pop(leaked, None)
    env.update(
        {
            "ROOST_BUNDLE_PROFILE": spec.profile,
            "ROOST_CONFIG": str(config),
            "ROOST_STATE_DIR": str(dirs["state-dir"]),
            "XDG_RUNTIME_DIR": str(runtime_dir),
            "XDG_DATA_HOME": str(dirs["data"]),
            "XDG_STATE_HOME": str(dirs["state"]),
            "XDG_CACHE_HOME": str(dirs["cache"]),
            "SHELL": str(stub),
            "RUST_LOG": "info",
            MARKER_ENV: marker_value,
        }
    )
    log_path = ui._rust_log_path("iced-boot-failure")
    with open(log_path, "wb") as log_fh:
        proc = subprocess.Popen(
            [str(binary)], cwd=tmp_path, env=env,
            stdout=log_fh, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    try:
        exit_bound = scaled_timeout(20.0)
        with contextlib.suppress(subprocess.TimeoutExpired):
            proc.wait(timeout=exit_bound)

        assert _persisted_tabs(dirs["state-dir"]) >= 1, (
            "hydrate never spawned a shell, so this run never reached a "
            f"failure after the spawn\n{_tail(log_path)}"
        )
        assert proc.returncode is not None, (
            f"the UI was still running {exit_bound:.0f}s after launch: a startup "
            f"error after the runtime exists wedged it\n{_tail(log_path)}"
        )
        assert proc.returncode != 0, _tail(log_path)
        assert BIND_CONTEXT in log_path.read_text(errors="replace"), _tail(log_path)
        left = _wait_unmarked(marker, scaled_timeout(ABORT_DEADLINE + ABORT_MARGIN))
        assert not left, f"the UI's shell outlived its boot abort: {left}\n{_tail(log_path)}"
    finally:
        try:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=scaled_timeout(10.0))
        finally:
            for pid in _marked_pids(marker):
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)
            survivors = _wait_unmarked(marker, scaled_timeout(5.0))
            assert not survivors, f"processes that outlived SIGKILL: {survivors}"
