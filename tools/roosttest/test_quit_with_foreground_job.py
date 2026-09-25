"""Plan 071 D4 (#522): one SIGTERM ends an in-process UI whose tab is
running a foreground job, and takes the job with it.

Each job pins a different leg of the quit (see `OwnedRuntime` in `app.rs`):

- `exec sleep 3600` dies of any hangup — the UI's, or the kernel's once the
  exited UI's pty master closes — so it pins only that the quit ends;
- `trap "" HUP; exec sleep 3600` ignores both, so only the UI's SIGKILL
  escalation removes it;
- a HUP-ignoring **grandchild** escapes the reap and keeps its reader
  parked, so only the runtime's drop bound lets the UI exit. The test
  kills the grandchild itself.

The UI is launched directly, as `test_boot_failure.py` launches its own,
under `local-backend = in-process` from its own `ROOST_CONFIG` — the
session backend's UI holds no PTYs, so it has nothing to hang up.
"""

from __future__ import annotations

import contextlib
import os
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

import pytest

if sys.platform != "linux":
    pytest.skip(
        "the quit-with-a-job lane is Linux-only (it sweeps /proc for the "
        "processes the UI started)",
        allow_module_level=True,
    )

import agent_jail  # noqa: E402
import ui  # noqa: E402
from client import Roost, RoostError, scaled_timeout  # noqa: E402
from test_boot_failure import _marked_pids, _tail, _wait_unmarked  # noqa: E402

MARKER_ENV = "ROOST_QUIT_JOB_MARKER"
# `app.rs`'s `QUIT_SHUTDOWN_DEADLINE`, plus room for `shutdown_all`'s
# SIGKILL tail.
QUIT_SHUTDOWN_DEADLINE = 2.0
QUIT_MARGIN = 1.0

JOBS = [
    pytest.param(["/bin/sh", "-c", "exec sleep 3600"], False, id="honors-hup"),
    pytest.param(["/bin/sh", "-c", 'trap "" HUP; exec sleep 3600'], False, id="ignores-hup"),
    # The trailing `:` keeps the outer shell from exec'ing the inner one,
    # so the sleep is a grandchild of the UI, not its child.
    pytest.param(
        ["/bin/sh", "-c", "/bin/sh -c 'trap \"\" HUP; exec sleep 3600'; :"],
        True,
        id="grandchild-ignores-hup",
    ),
]


def _comm(pid: int) -> str | None:
    try:
        return Path(f"/proc/{pid}/comm").read_text().strip()
    except OSError:
        return None


def _marked_sleeps(marker: bytes) -> list[int]:
    return [pid for pid in _marked_pids(marker) if _comm(pid) == "sleep"]


def _wait_for(predicate, timeout: float, interval: float = 0.05):
    deadline = time.monotonic() + timeout
    while not (result := predicate()) and time.monotonic() < deadline:
        time.sleep(interval)
    return result


def _dial_main_loop(socket: Path, proc: subprocess.Popen) -> Roost | None:
    """A client once the UI's main loop answers (see `ui._booted`), or
    `None` if the UI exited first."""
    deadline = time.monotonic() + scaled_timeout(30.0)
    while time.monotonic() < deadline and proc.poll() is None:
        c = None
        try:
            c = Roost(socket, timeout=scaled_timeout(5.0))
            c.sidebar_dump()
            return c
        except (OSError, RoostError):
            if c is not None:
                c.close()
            time.sleep(0.05)
    return None


@pytest.mark.parametrize(("argv", "escapes_the_reap"), JOBS)
def test_one_sigterm_ends_the_ui_and_its_foreground_job(
    argv, escapes_the_reap, tmp_path, short_root, request
):
    binary, _ = ui.rust_binary_path("iced")
    if not binary.is_file():
        raise FileNotFoundError(f"no roost-iced binary at {binary}; build it first")

    spec = ui.TARGET_SPECS["iced"]
    runtime_dir = short_root / "run"
    agent_jail.make_private_runtime_dir(runtime_dir)
    socket = runtime_dir / spec.linux_namespace / "roost.sock"

    config = tmp_path / "config.conf"
    config.write_text("agent-hooks = off\nlocal-backend = in-process\n")
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
            "SHELL": "/bin/sh",
            "RUST_LOG": "info",
            MARKER_ENV: marker_value,
        }
    )
    log_path = ui._rust_log_path(f"iced-quit-{request.node.callspec.id}")
    with open(log_path, "wb") as log_fh:
        proc = subprocess.Popen(
            [str(binary)], cwd=tmp_path, env=env,
            stdout=log_fh, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    try:
        c = _dial_main_loop(socket, proc)
        assert c is not None, f"the UI never answered on {socket}\n{_tail(log_path)}"
        with c:
            project = c.create_project(name="quit-with-job", cwd=str(tmp_path))
            c.open_tab(project, argv=argv)
        assert _wait_for(lambda: _marked_sleeps(marker), scaled_timeout(10.0)), (
            f"the tab's `sleep` never started\n{_tail(log_path)}"
        )

        proc.send_signal(signal.SIGTERM)
        exit_bound = scaled_timeout(10.0)
        with contextlib.suppress(subprocess.TimeoutExpired):
            proc.wait(timeout=exit_bound)
        assert proc.returncode is not None, (
            f"the UI was still running {exit_bound:.0f}s after one SIGTERM: its "
            f"foreground job wedged the quit\n{_tail(log_path)}"
        )
        # 0 only through the run loop's own end: `Drop for App` ran.
        assert proc.returncode == 0, _tail(log_path)

        def outlived():
            return [
                (p, comm)
                for p in _marked_pids(marker)
                if (comm := _comm(p)) != "sleep" or not escapes_the_reap
            ]

        bound = scaled_timeout(QUIT_SHUTDOWN_DEADLINE + QUIT_MARGIN)
        assert _wait_for(lambda: not outlived(), bound), (
            f"a process the quit reaps outlived it: {outlived()}\n{_tail(log_path)}"
        )
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
