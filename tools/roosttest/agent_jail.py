"""The `$HOME` jail every agent-hooks lane runs inside, and the jailed
UI launcher that runs inside it.

Extracted from `test_agent_hooks.py` so `test_host_client.py` can jail
the `roost-session` it spawns with the *same* helper rather than a second
copy of it (plan 046 §3.9). One implementation means one place where the
list of variables can go stale, and `assert_jailed` is the only thing
standing between a bug in the install engine and a developer's real
`~/.claude/settings.json`.

`jailed_ui` and its helpers followed the jail here for the same reason
(plan 064 C7): three modules now launch a second, fully redirected Roost
— the startup-ensure lanes, the host-client lane, and the consent-card
lanes — and a launcher that drifted between copies would be a launcher
that stopped being jailed in one of them. It is also the ONE place in
the tree that sets `ROOST_AGENT_HOOKS_FORCE=1`, which lifts the install
engine's test-mode refusal; everything it can reach is inside the jail,
and `assert_jailed` on the merged environment is what says so.
"""

from __future__ import annotations

import contextlib
import json
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest
import ui
from client import Roost, RoostError, scaled_timeout
from util import REPO_ROOT


# Agent name → the environment variable that relocates its config dir.
# MIRRORS `roost_agent_install::home::config_dir_env`; the five are what
# make the jail complete.
AGENT_CONFIG_DIR_ENV = {
    "claude": "CLAUDE_CONFIG_DIR",
    "codex": "CODEX_HOME",
    "grok": "GROK_HOME",
    "cursor": "CURSOR_CONFIG_DIR",
    "opencode": "OPENCODE_CONFIG_DIR",
}

# Report order of `roost_agent_install::ALL_AGENTS`.
INSTALLABLE_AGENTS = ("claude", "codex", "grok", "cursor", "opencode")

# The variables §3.9 pins. `XDG_CONFIG_HOME` is belt and braces:
# Roost's own state record is `$HOME/.config/roost/agent-hooks.json`
# whatever XDG says, so `HOME` already covers it — but a future move to
# the XDG dir must not silently unjail this suite.
#
# `ROOST_CONFIG` is not belt and braces. Since plan 064 the install
# engine *writes* `agent-hooks` into `config.conf` — `Home::config_path`
# resolves it through the same `ROOST_CONFIG`-then-`$HOME` rule the UI
# uses — so a stray value in a developer's shell would send a raise at
# their real config file while every other variable here stayed jailed.
JAIL_ENV_KEYS = ("HOME", "XDG_CONFIG_HOME", "ROOST_CONFIG", *AGENT_CONFIG_DIR_ENV.values())

# Checked only when set — see `Jail.assert_jailed` for why absence is
# safe for this one and for nothing else here.
OPTIONAL_JAIL_ENV_KEYS = ("ROOST_CONFIG",)


def make_private_runtime_dir(path: Path) -> None:
    """A private `XDG_RUNTIME_DIR` for a redirected UI, so its socket and
    single-instance locks cannot collide with the session UI's.

    On the Wayland lane that also moves the compositor out of reach —
    `WAYLAND_DISPLAY` is a socket *name*, resolved against
    `XDG_RUNTIME_DIR` — so the real one is linked back in. Without this
    the redirected UI would fail to open a window on the weston lane, and
    only there."""
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)
    display = os.environ.get("WAYLAND_DISPLAY", "")
    if not display or os.path.isabs(display):
        return
    real = Path(os.environ.get("XDG_RUNTIME_DIR", "")) / display
    link = path / display
    if real.exists() and not link.exists():
        link.symlink_to(real)


class Jail:
    """A throwaway home with its own `config.conf`, its own agent config
    directories, and the environment that points every relevant tool at
    them."""

    def __init__(
        self,
        root,
        *,
        agent_hooks: "str | None" = ", ".join(INSTALLABLE_AGENTS),
        present=INSTALLABLE_AGENTS,
    ):
        self.root = root.resolve()
        self.home = self.root / "home"
        self.config = self.home / ".config/roost/config.conf"
        self.record = self.home / ".config/roost/agent-hooks.json"
        self.state_dir = self.root / "state"
        self.runtime_dir = self.root / "run"
        self.agent_dirs = {name: self.root / "agents" / name for name in AGENT_CONFIG_DIR_ENV}
        # Distinct log file per launch, so a relaunch's boot output does
        # not overwrite the evidence of the launch before it.
        self.launches = 0

        for name in present:
            self.agent_dirs[name].mkdir(parents=True, exist_ok=True)
        self.state_dir.mkdir(parents=True, exist_ok=True)
        make_private_runtime_dir(self.runtime_dir)
        self.write_config(agent_hooks=agent_hooks)

        self.env = {
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            # The same file `HOME` already resolves to, said explicitly:
            # a raise writes this key, and a process that reached it by
            # a different rule than the one the jail asserts on would
            # write somewhere `assert_jailed` never looked.
            "ROOST_CONFIG": str(self.config),
            **{
                AGENT_CONFIG_DIR_ENV[name]: str(path)
                for name, path in self.agent_dirs.items()
            },
        }

    def write_config(self, *, agent_hooks: "str | None") -> None:
        """Seed this jail's `config.conf`. `None` leaves the key absent,
        which is the unanswered state a host starts in before anyone has
        consented on it."""
        self.config.parent.mkdir(parents=True, exist_ok=True)
        self.config.write_text("" if agent_hooks is None else f"agent-hooks = {agent_hooks}\n")

    def read_key(self) -> "str | None":
        """This jail's `agent-hooks` value, or `None` if the key is
        absent. Last-wins, matching the parser."""
        found = None
        for line in self.config.read_text().splitlines():
            key, _, value = line.partition("=")
            if key.strip() == "agent-hooks":
                found = value.strip()
        return found

    def assert_jailed(self, env: dict) -> None:
        """Every jail variable is set, absolute, and inside this root.

        Run on the *merged* environment a spawn is about to get, right
        before the spawn. An assertion on `self.env` would prove only
        that the dict was built correctly.

        `ROOST_CONFIG` is the one variable allowed to be *absent*: unset
        means the config resolves through `HOME`, which is required and
        checked here — so absence is jailed by construction, and a
        launch that deliberately drops it (the `roostctl session start`
        shape, which inherits no override) stays inside the fence. What
        must never happen is a value pointing OUT of the jail, since a
        raise writes `agent-hooks` through exactly that rule."""
        for key in JAIL_ENV_KEYS:
            value = env.get(key)
            if value is None and key in OPTIONAL_JAIL_ENV_KEYS:
                continue
            assert value, f"{key} is not set: the jail is not in force"
            path = Path(value)
            assert path.is_absolute(), f"{key}={value} is not absolute"
            assert path.resolve().is_relative_to(self.root), (
                f"{key}={value} escapes the jail at {self.root}"
            )

    def read_record(self) -> dict:
        return json.loads(self.record.read_text())

    def owned_files(self, agent: str) -> list:
        """The files the state record says Roost wrote for `agent` — read
        back rather than hardcoded here, so the five per-agent layouts
        live in exactly one place (the install crate)."""
        return [Path(p) for p in self.read_record()[agent]["files"]]


def jailed_ui_env(jail: Jail, *, force: bool = True) -> dict:
    """The environment a jailed UI launch gets: the agent jail, XDG dirs
    inside it (so the socket, the log and the caches land there too), and
    the two variables that let the install engine run under
    `ROOST_TEST_MODE`.

    `force=False` drops `ROOST_AGENT_HOOKS_FORCE`, leaving the harness
    fence in force — the shape every OTHER lane in this suite runs in,
    and the one case that has to prove a test-mode UI asks nothing and
    writes nothing."""
    env = {**os.environ}
    # Same list `ui.launch` strips, and for the same reason: per-tab
    # values Roost injects itself, plus the selectors set explicitly
    # below. Stripped first so the explicit values cannot be undone.
    for leaked in ui._UI_ENV_SANITIZE:
        env.pop(leaked, None)
    env.update(jail.env)
    env.update(
        {
            "XDG_RUNTIME_DIR": str(jail.runtime_dir),
            "XDG_DATA_HOME": str(jail.home / ".local/share"),
            "XDG_STATE_HOME": str(jail.home / ".local/state"),
            "XDG_CACHE_HOME": str(jail.home / ".cache"),
            "ROOST_BUNDLE_PROFILE": ui.TARGET_SPECS["iced"].profile,
            "ROOST_CONFIG": str(jail.config),
            "ROOST_STATE_DIR": str(jail.state_dir),
            "ROOST_TEST_MODE": "1",
            "RUST_LOG": os.environ.get("RUST_LOG", "warn") + ",roost_iced=info",
        }
    )
    if force:
        # The one place in the tree that lifts the install engine's
        # test-mode refusal. Everything it can reach is in the jail.
        env["ROOST_AGENT_HOOKS_FORCE"] = "1"
    else:
        env.pop("ROOST_AGENT_HOOKS_FORCE", None)
    return env


def jailed_socket(jail: Jail) -> Path:
    """Where a UI launched with `jailed_ui_env` binds. MIRRORS
    `ui.socket_path`, rooted in the jail rather than in `$HOME` /
    `$XDG_RUNTIME_DIR`."""
    spec = ui.TARGET_SPECS["iced"]
    if platform.system() == "Darwin":
        return jail.home / f"Library/Caches/{spec.mac_label}/roost.sock"
    return jail.runtime_dir / spec.linux_namespace / "roost.sock"


@contextlib.contextmanager
def jailed_ui(jail: Jail, *, force: bool = True):
    """Launch a jailed iced UI, yield `(process, log path)`, and stop it.

    Teardown waits for the process to *exit* before the caller's
    assertions run against the jail. That is what makes "nothing was
    written" an assertion rather than a race: a dead process has no more
    writes left in it."""
    binary, explicit = ui.rust_binary_path("iced")
    if not binary.is_file():
        if explicit:
            pytest.skip(f"explicit iced binary does not exist: {binary}")
        subprocess.run(["cargo", "build", "-p", "roost-iced"], cwd=REPO_ROOT, check=True)

    env = jailed_ui_env(jail, force=force)
    jail.assert_jailed(env)
    log = jail.root / f"ui-{jail.launches}.log"
    jail.launches += 1
    with open(log, "wb") as handle:
        proc = subprocess.Popen(
            [str(binary)],
            cwd=REPO_ROOT,
            env=env,
            stdout=handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    try:
        yield proc, log
    finally:
        if proc.poll() is None:
            proc.terminate()
        try:
            proc.wait(timeout=scaled_timeout(20))
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=scaled_timeout(10))


def wait_for_jailed_window(jail: Jail, proc, log: Path) -> None:
    """Block until the jailed UI has a window on screen.

    `app.screenshot` is answered only once `window_id` is set, and that
    happens inside the same `window_opened` handler that decides whether
    to start the agent-hooks ensure — *before* it returns. So a
    screenshot that comes back proves the decision has been made, which
    is what lets the `off` case assert on an empty jail instead of racing
    a write that was never going to happen."""
    sock = jailed_socket(jail)

    def windowed() -> bool:
        if proc.poll() is not None:
            raise AssertionError(
                f"jailed UI exited {proc.returncode} before opening a window:\n"
                f"{log.read_text(errors='replace')}"
            )
        if not sock.exists():
            return False
        try:
            with Roost(str(sock), timeout=scaled_timeout(10)) as roost:
                roost.screenshot()
            return True
        except (OSError, RoostError):
            return False

    Roost._wait(windowed, 60.0, f"the jailed UI to open a window ({sock})")


def wait_for_log_line(log: Path, needle: str, what: str) -> str:
    """Block until a line of the jailed UI's log contains `needle`, and
    return that line.

    The log is how this module reads the status banner. No IPC op
    carries it — `app.*` exposes the menu, the dialogs and the sidebar's
    last-rendered rows, and `tab.dump` is the terminal grid; the
    transient line is drawn straight from `App::status` in `view` and is
    exposed nowhere else — so a test that wants to know what the banner
    says has the UI's own log and nothing better."""
    found: list[str] = []

    def seen() -> bool:
        for line in log.read_text(errors="replace").splitlines():
            if needle in line:
                found.append(line)
                return True
        return False

    Roost._wait(seen, 30.0, what)
    return found[0]


@pytest.fixture
def short_root():
    """A jail root short enough to hold a Unix socket path.

    `sun_path` is 104 bytes on macOS, and pytest's `tmp_path` spends
    ~70 of them before this test adds
    `home/Library/Caches/Roost-iced/roost.sock` — the jailed UI then
    refuses to bind. `/tmp` is the only root with room, and it is short
    on Linux too."""
    root = Path(tempfile.mkdtemp(prefix="roost-jail-", dir="/tmp"))
    try:
        yield root
    finally:
        shutil.rmtree(root, ignore_errors=True)
