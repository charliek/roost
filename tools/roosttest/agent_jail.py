"""The `$HOME` jail every agent-hooks lane runs inside.

Extracted from `test_agent_hooks.py` so `test_host_client.py` can jail
the `roost-session` it spawns with the *same* helper rather than a second
copy of it (plan 046 §3.9). One implementation means one place where the
list of variables can go stale, and `assert_jailed` is the only thing
standing between a bug in the install engine and a developer's real
`~/.claude/settings.json`.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


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
