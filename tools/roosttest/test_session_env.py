"""A session tab's environment comes from who opened it, never from
whatever started the daemon (plan 085 D1–D3, D6).

Every daemon here is built from a scrubbed, explicit environment
(`make_env(explicit=True)`) plus the start context a case hands it — a
desk's displays and agent, or an SSH login's identity — so the runner's
own environment reaches none of them. Each tab's environment is read
from its process (`tab_env`), and only the opener set (plus
`XDG_RUNTIME_DIR` and `SHELL` where those are the point) is compared:
the restored shell adds variables of its own. On macOS, which withholds a
platform binary's environment, the restored shell is a stand-in
(`readable_shell`) and a bare tab runs `tab_env.sleep_argv`.

Until `tab.open` can carry an opener (C2), the tabs that have one come
from a hand-written `state.json`; a tab opened over the wire states none.
"""

from __future__ import annotations

import json
import os
import pwd
import sys

import pytest
import session as sessionlib
import tab_env
from ui import OPENER_SET, TEST_USER_RUNTIME_DIR_ENV

pytestmark = pytest.mark.session_daemon

#: A desk start: displays, bus and agent, no SSH.
DESK = {
    "DISPLAY": ":31",
    "WAYLAND_DISPLAY": "/desk/run/wayland-3",
    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/desk/run/bus",
    "SSH_AUTH_SOCK": "/desk/agent",
}
#: A start over SSH: the laptop's identity, its TTY and forwarded agent.
LAPTOP = {
    "SSH_CONNECTION": "100.64.0.2 51000 100.64.0.1 22",
    "SSH_CLIENT": "100.64.0.2 51000 22",
    "SSH_TTY": "/dev/pts/7",
    "SSH_AUTH_SOCK": "/tmp/ssh-laptop/agent.7",
}
#: What a saved local tab was opened with.
SAVED_LOCAL = {
    "DISPLAY": ":11",
    "WAYLAND_DISPLAY": "/saved/run/wayland-1",
    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/saved/bus",
    "SSH_AUTH_SOCK": "/saved/agent",
}
#: What a saved remote tab was opened with, and what it brings.
SAVED_REMOTE = {"ssh_connection": "10.1.1.1 41000 10.2.2.2 22", "ssh_client": "10.1.1.1 41000 22"}
REMOTE_VARS = {"SSH_CONNECTION": SAVED_REMOTE["ssh_connection"], "SSH_CLIENT": SAVED_REMOTE["ssh_client"]}

#: A saved tab with no `opener` key: saved before there were openers.
LEGACY = object()


@pytest.fixture
def env():
    made = sessionlib.make_env(explicit=True)
    try:
        yield made
    finally:
        made.teardown()


def readable_shell(env: sessionlib.SessionEnv) -> dict[str, str]:
    """On macOS, a `SHELL` for the daemon to restore tabs into whose
    environment `tab_env` can read: a script run by `tab_env.sleeper()`,
    so the kernel passes the block Roost gave it on unchanged. Nothing on
    Linux, where the harness's `/bin/sh` reads."""
    if sys.platform != "darwin":
        return {}
    shell = env.root / "readable-shell"
    shell.write_text(f"#!{tab_env.sleeper()}\nimport time\ntime.sleep(600)\n")
    shell.chmod(0o755)
    return {"SHELL": str(shell)}


def serve(env: sessionlib.SessionEnv, **context: str) -> sessionlib.Foreground:
    """A foreground daemon started in `context`, answering. Foreground, so
    its pid is a handle this test holds."""
    daemon = env.start_foreground(**readable_shell(env), **context)
    verdict = daemon.verdict()
    assert verdict.kind == "ready", verdict
    env.wait_answering()
    return daemon


def stop(env: sessionlib.SessionEnv, daemon: sessionlib.Foreground) -> None:
    env.stop_over_the_wire()
    daemon.wait()
    env.wait_socket_gone()


def work_dir(env: sessionlib.SessionEnv):
    work = env.root / "work"
    work.mkdir(exist_ok=True)
    return work


def write_state(env: sessionlib.SessionEnv, projects: list[tuple[str, list[tuple[str, object]]]]) -> None:
    """A `state.json` of `(project name, [(tab title, saved opener)])`, each
    tab user-titled so its title names it after the restore."""
    cwd = str(work_dir(env))

    def tab(position: int, title: str, opener: object) -> dict:
        saved = {"title": title, "cwd": cwd, "position": position, "user_titled": True}
        if opener is not LEGACY:
            saved["opener"] = opener
        return saved

    state = {
        "next_id": 100,
        "projects": [
            {
                "id": index + 1,
                "name": name,
                "cwd": cwd,
                "position": index,
                "created_at": 1,
                "tabs": [tab(position, *spec) for position, spec in enumerate(tabs)],
            }
            for index, (name, tabs) in enumerate(projects)
        ],
    }
    env.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    env.state_json.write_text(json.dumps(state))


def only_tab(env: sessionlib.SessionEnv) -> int:
    """The one tab a fresh session seeded."""
    with env.client() as client:
        tabs = client.tabs()
    assert len(tabs) == 1, tabs
    return int(tabs[0]["id"])


def tab_named(env: sessionlib.SessionEnv, title: str) -> int:
    with env.client() as client:
        matches = [int(tab["id"]) for tab in client.tabs() if tab["title"] == title]
    assert len(matches) == 1, f"tabs titled {title!r}: {matches}"
    return matches[0]


def tab_environ(env: sessionlib.SessionEnv, daemon: sessionlib.Foreground, tab_id: int) -> dict[str, str]:
    _, environ = tab_env.tab_process(daemon.proc.pid, tab_id, binaries=[env.binary])
    return environ


def openers(environ: dict[str, str]) -> dict[str, str]:
    return {name: value for name, value in environ.items() if name in OPENER_SET}


def open_bare(env: sessionlib.SessionEnv) -> int:
    """A `tab.open` with no opener stated."""
    with env.client() as client:
        project = int(client.list()[0]["id"])
        return client.open_tab(project, cwd=str(work_dir(env)), argv=tab_env.sleep_argv(600))


SAVED_TABS = [
    ("legacy", LEGACY),
    ("none", "none"),
    ("local", {"local": SAVED_LOCAL}),
    ("remote", {"remote": SAVED_REMOTE}),
]


# ---------------------------------------------------------------------------
# 8. Restore: saved classes, the refresh, and legacy tabs
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("starter", "local_gets"),
    [
        pytest.param(DESK, DESK, id="local-starter-refreshes"),
        pytest.param(LAPTOP, SAVED_LOCAL, id="remote-starter-keeps"),
        pytest.param({}, SAVED_LOCAL, id="value-less-starter-keeps"),
    ],
)
def test_a_restore_keeps_each_saved_class_and_only_a_desk_start_refreshes_local_values(
    env, starter, local_gets
):
    write_state(env, [("saved", SAVED_TABS)])
    daemon = serve(env, **starter)

    got = {title: openers(tab_environ(env, daemon, tab_named(env, title))) for title, _ in SAVED_TABS}
    assert got == {
        "legacy": {},
        "none": {},
        "local": local_gets,
        "remote": REMOTE_VARS,
    }
    saved = {tab["title"]: tab.get("opener") for tab in env.state()["projects"][0]["tabs"]}
    assert saved["local"] == {"local": local_gets}, "the restored class is written back as opened"
    assert saved["legacy"] == "none", "a restored legacy tab is saved with the class it got"
    stop(env, daemon)


# ---------------------------------------------------------------------------
# 9. XDG_RUNTIME_DIR is derived per uid, never inherited (Linux)
# ---------------------------------------------------------------------------


@pytest.mark.skipif(sys.platform != "linux", reason="/run/user/<uid> is Linux's; macOS keeps the baseline's")
def test_every_tab_gets_the_derived_runtime_dir_and_the_daemons_when_none_derives(env):
    """The knob stands in for `/run/user/<uid>`. Pointed at a directory
    other than the daemon's own runtime dir, so a tab that inherited
    would show it."""
    user_run = env.root / "user-run"
    user_run.mkdir(mode=0o700)
    tabs = [("local", {"local": SAVED_LOCAL}), ("remote", {"remote": SAVED_REMOTE})]
    for knob, expected in [
        (user_run, user_run),
        (env.root / "missing-run", env.env["XDG_RUNTIME_DIR"]),
    ]:
        write_state(env, [("saved", tabs)])
        daemon = serve(env, **{TEST_USER_RUNTIME_DIR_ENV: str(knob)})
        ids = [tab_named(env, "local"), tab_named(env, "remote"), open_bare(env)]
        got = [tab_environ(env, daemon, tab_id).get("XDG_RUNTIME_DIR") for tab_id in ids]
        assert got == [str(expected)] * 3, f"knob {knob}"
        stop(env, daemon)


# ---------------------------------------------------------------------------
# 10. Start contexts: a desk-started and an SSH-started daemon
# ---------------------------------------------------------------------------


def test_a_desk_started_daemon_gives_its_own_tabs_the_desk_and_no_other_tab_any_of_it(env):
    daemon = serve(env, **DESK)
    seed = only_tab(env)
    seeded = tab_environ(env, daemon, seed)
    assert openers(seeded) == DESK, "the seed tab is the daemon's own"
    assert seeded["SHELL"] == pwd.getpwuid(os.getuid()).pw_shell, "SHELL is the passwd shell"
    assert openers(tab_environ(env, daemon, open_bare(env))) == {}
    stop(env, daemon)

    write_state(env, [("saved", [("remote", {"remote": SAVED_REMOTE}), ("none", "none")]), ("bare", [])])
    daemon = serve(env, **DESK)
    assert openers(tab_environ(env, daemon, tab_named(env, "remote"))) == REMOTE_VARS
    assert openers(tab_environ(env, daemon, tab_named(env, "none"))) == {}
    with env.client() as client:
        bare = next(project for project in client.list() if project["name"] == "bare")
    assert len(bare["tabs"]) == 1, bare
    assert openers(tab_environ(env, daemon, int(bare["tabs"][0]["id"]))) == DESK, (
        "a project that saved no tabs gets the daemon's own"
    )
    stop(env, daemon)


def test_an_ssh_started_daemon_opens_its_own_tab_as_the_laptop_and_no_other_tab_as_it(env):
    daemon = serve(env, **LAPTOP)
    seed = only_tab(env)
    assert openers(tab_environ(env, daemon, seed)) == {
        "SSH_CONNECTION": LAPTOP["SSH_CONNECTION"],
        "SSH_CLIENT": LAPTOP["SSH_CLIENT"],
    }, "a remote opener brings its identity alone: no TTY, no agent"
    assert openers(tab_environ(env, daemon, open_bare(env))) == {}
    stop(env, daemon)

    write_state(env, [("saved", [("local", {"local": {"DISPLAY": ":12"}}), ("none", "none")])])
    daemon = serve(env, **LAPTOP)
    assert openers(tab_environ(env, daemon, tab_named(env, "local"))) == {"DISPLAY": ":12"}
    assert openers(tab_environ(env, daemon, tab_named(env, "none"))) == {}
    stop(env, daemon)
