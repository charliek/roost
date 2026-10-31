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
(`readable_shell`) and an opened tab runs `tab_env.sleep_argv`.

Openers reach a tab three ways: a real `client-bridge` in front of the
session (its preamble), a `tab.open` that states `env` or `origin`, and
a hand-written `state.json` for the restore classes no open can write
(a legacy tab).
"""

from __future__ import annotations

import json
import os
import pwd
import sys
import threading

import pytest
import session as sessionlib
import tab_env
from client import Roost, RoostError
from test_ssh_transport import BridgeListener
from ui import TEST_USER_RUNTIME_DIR_ENV

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
#: A local opener's values, distinct from every starter's; one empty.
OPENER_LOCAL = {
    "DISPLAY": ":41",
    "WAYLAND_DISPLAY": "/opener/run/wayland-4",
    "DBUS_SESSION_BUS_ADDRESS": "",
    "SSH_AUTH_SOCK": "/opener/agent",
}
#: A remote opener's identity, as `origin` states it.
OPENER_ORIGIN = {"ssh_connection": "100.70.0.5 52000 100.70.0.1 22", "ssh_client": "100.70.0.5 52000 22"}
OPENER_REMOTE = {"SSH_CONNECTION": OPENER_ORIGIN["ssh_connection"], "SSH_CLIENT": OPENER_ORIGIN["ssh_client"]}
#: The SSH login a real bridge runs in, with a display and agent of its own.
BRIDGE_LOGIN = {
    "SSH_CONNECTION": "100.80.0.9 53000 100.80.0.1 22",
    "SSH_CLIENT": "100.80.0.9 53000 22",
    "DISPLAY": "localhost:10.0",
    "SSH_AUTH_SOCK": "/tmp/ssh-bridge/agent.9",
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
    return {"SHELL": str(tab_env.readable_shell(env.root))}


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


def open_on(client: Roost, profile: sessionlib.SessionEnv, *, title: str = "", project: int | None = None,
            **opener) -> int:
    """A sleeping tab opened on `client`, stating `opener` (`env=` /
    `origin=`) or nothing. Titled tabs are renamed, so their title names
    them across a restart."""
    if project is None:
        project = int(client.list()[0]["id"])
    tab = client.open_tab(project, cwd=str(work_dir(profile)), argv=tab_env.sleep_argv(600), **opener)
    if title:
        client.set_title(tab, title)
    return tab


def open_bare(env: sessionlib.SessionEnv) -> int:
    """A `tab.open` with no opener stated."""
    with env.client() as client:
        return open_on(client, env)


def refused(client: Roost, profile: sessionlib.SessionEnv, project: int, **fields) -> RoostError:
    """A `tab.open` with `fields` that must be refused, and its error."""
    with pytest.raises(RoostError) as caught:
        client.open_tab(project, cwd=str(work_dir(profile)), argv=tab_env.sleep_argv(600), **fields)
    return caught.value


def layout(client: Roost) -> list[tuple[str, list[str]]]:
    """Every project and its tabs, by id."""
    return [(project["id"], [tab["id"] for tab in project["tabs"]]) for project in client.list()]


# ---------------------------------------------------------------------------
# 1. A real client-bridge: the tab is remote with the bridge's identity alone
# ---------------------------------------------------------------------------


def test_a_tab_opened_through_a_real_bridge_has_exactly_its_ssh_identity(env):
    """The daemon is desk-started and the bridge has a display and agent
    of its own: the tab gets neither, only the bridge's two `SSH_*`."""
    daemon = serve(env, **DESK)
    with BridgeListener(env, overrides=BRIDGE_LOGIN) as bridge, bridge.client() as client:
        tab = open_on(client, env)
    assert tab_env.opener_vars(tab_environ(env, daemon, tab)) == {
        "SSH_CONNECTION": BRIDGE_LOGIN["SSH_CONNECTION"],
        "SSH_CLIENT": BRIDGE_LOGIN["SSH_CLIENT"],
    }
    stop(env, daemon)


# ---------------------------------------------------------------------------
# 2–3. A local opener's values, exactly
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "stated",
    [
        pytest.param(OPENER_LOCAL, id="all-four-one-empty"),
        pytest.param({"DISPLAY": ":42", "SSH_AUTH_SOCK": "/opener/agent-2"}, id="two-of-four"),
    ],
)
def test_a_local_open_gets_exactly_the_values_it_states(env, stated):
    """Against a desk-started daemon, so a name the opener left out would
    show the starter's value if anything filled it."""
    daemon = serve(env, **DESK)
    with env.client() as client:
        tab = open_on(client, env, env=stated)
    assert tab_env.opener_vars(tab_environ(env, daemon, tab)) == stated
    stop(env, daemon)


# ---------------------------------------------------------------------------
# 4–5. Refusals open nothing
# ---------------------------------------------------------------------------


def test_a_refused_opener_opens_no_tab_and_no_project(env):
    """With no project at all, `project_id: 0` would create the default
    one: a refusal must come first."""
    daemon = serve(env, ROOST_SESSION_NO_SEED="1")
    with env.client() as client:
        assert layout(client) == [], "the fixture starts with nothing"
        for key in ("SSH_AUTH_SOCKX", "SSH_CONNECTION", "PATH"):
            error = refused(client, env, 0, env={"DISPLAY": ":1", key: "x"})
            assert error.code == "invalid-param" and f"`{key}`" in error.message, error
            assert layout(client) == [], key
        project = client.create_project("p", str(work_dir(env)))
        before = layout(client)
        for project_id in (project, 0):
            error = refused(client, env, project_id, env={"PATH": "/bin"})
            assert error.code == "invalid-param", error
            assert layout(client) == before, project_id
    stop(env, daemon)


def test_env_with_origin_either_on_a_bridge_an_unknown_origin_key_and_other_shapes_are_refused(env):
    daemon = serve(env)
    with env.client() as client:
        project = client.create_project("p", str(work_dir(env)))
        before = layout(client)
        error = refused(client, env, project, env={}, origin=OPENER_ORIGIN)
        assert error.code == "invalid-param" and "exclusive" in error.message, error
        error = refused(client, env, project, origin={**OPENER_ORIGIN, "ssh_tty": "/dev/pts/1"})
        assert error.code == "unknown-field" and "`ssh_tty`" in error.message, error
        assert "/dev/pts/1" not in error.message, error
        secret = OPENER_ORIGIN["ssh_connection"]
        for field, shape in [
            ("origin", [secret, OPENER_ORIGIN["ssh_client"]]),
            ("origin", secret),
            ("env", {"DISPLAY": [secret]}),
            ("env", secret),
        ]:
            error = refused(client, env, project, **{field: shape})
            assert error.code == "invalid-param" and f"`{field}` must be an object" in error.message, error
            assert secret not in error.message, (field, shape, error)
        with BridgeListener(env, overrides=BRIDGE_LOGIN) as bridge, bridge.client() as bridged:
            for fields in ({"env": {"DISPLAY": ":1"}}, {"env": {}}, {"origin": OPENER_ORIGIN}):
                error = refused(bridged, env, project, **fields)
                assert error.code == "invalid-param" and "bridged" in error.message, (fields, error)
        assert layout(client) == before
    stop(env, daemon)


# ---------------------------------------------------------------------------
# 6. Simultaneous local and remote opens do not cross
# ---------------------------------------------------------------------------


def test_simultaneous_local_and_remote_opens_each_get_their_own(env):
    daemon = serve(env)
    with env.client() as setup:
        project = int(setup.list()[0]["id"])
    barrier = threading.Barrier(2, timeout=60)
    opened: dict[str, int] = {}
    failures: list[BaseException] = []

    def open_after_barrier(name: str, client: Roost, **opener) -> None:
        try:
            barrier.wait()
            opened[name] = open_on(client, env, project=project, **opener)
        except BaseException as error:  # noqa: BLE001 - re-raised on the main thread
            failures.append(error)

    with BridgeListener(env, overrides=BRIDGE_LOGIN) as bridge, bridge.client() as remote, \
            env.client() as local:
        # Dialled first, so the barrier releases two connections that are
        # already past the bridge's preamble.
        remote.identify()
        threads = [
            threading.Thread(target=open_after_barrier, args=("remote", remote)),
            threading.Thread(target=open_after_barrier, args=("local", local), kwargs={"env": OPENER_LOCAL}),
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=120)
    assert not failures, failures
    assert tab_env.opener_vars(tab_environ(env, daemon, opened["local"])) == OPENER_LOCAL
    assert tab_env.opener_vars(tab_environ(env, daemon, opened["remote"])) == {
        "SSH_CONNECTION": BRIDGE_LOGIN["SSH_CONNECTION"],
        "SSH_CLIENT": BRIDGE_LOGIN["SSH_CLIENT"],
    }
    stop(env, daemon)


# ---------------------------------------------------------------------------
# 7. A declared origin is remote; a field-less open is the baseline
# ---------------------------------------------------------------------------


def test_a_declared_origin_is_remote_and_a_field_less_open_is_the_baseline(env):
    """Against an SSH-started daemon, whose own identity a field-less tab
    would show if it still inherited."""
    daemon = serve(env, **LAPTOP)
    with env.client() as client:
        declared = open_on(client, env, origin=OPENER_ORIGIN)
        half = open_on(client, env, origin={"ssh_client": OPENER_ORIGIN["ssh_client"]})
        bare = open_on(client, env)
    assert tab_env.opener_vars(tab_environ(env, daemon, declared)) == OPENER_REMOTE
    assert tab_env.opener_vars(tab_environ(env, daemon, half)) == {"SSH_CLIENT": OPENER_ORIGIN["ssh_client"]}
    assert tab_env.opener_vars(tab_environ(env, daemon, bare)) == {}
    stop(env, daemon)


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

    got = {title: tab_env.opener_vars(tab_environ(env, daemon, tab_named(env, title))) for title, _ in SAVED_TABS}
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


@pytest.mark.parametrize(
    ("starter", "local_gets"),
    [
        pytest.param(LAPTOP, OPENER_LOCAL, id="remote-starter-keeps"),
        pytest.param(DESK, DESK, id="local-starter-refreshes"),
        pytest.param({}, OPENER_LOCAL, id="value-less-starter-keeps"),
    ],
)
def test_tabs_opened_over_the_wire_restore_with_the_opener_they_were_opened_with(env, starter, local_gets):
    """The same three classes, written by `tab.open` itself rather than by
    hand: opened under a starter with values of its own, so a tab that
    saved the starter's instead of its opener's would show it."""
    daemon = serve(env, **DESK)
    with env.client() as client:
        open_on(client, env, title="local", env=OPENER_LOCAL)
        open_on(client, env, title="remote", origin=OPENER_ORIGIN)
        open_on(client, env, title="bare")
    stop(env, daemon)

    daemon = serve(env, **starter)
    got = {title: tab_env.opener_vars(tab_environ(env, daemon, tab_named(env, title))) for title in ("local", "remote", "bare")}
    assert got == {"local": local_gets, "remote": OPENER_REMOTE, "bare": {}}
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
    assert tab_env.opener_vars(seeded) == DESK, "the seed tab is the daemon's own"
    assert seeded["SHELL"] == pwd.getpwuid(os.getuid()).pw_shell, "SHELL is the passwd shell"
    assert tab_env.opener_vars(tab_environ(env, daemon, open_bare(env))) == {}
    stop(env, daemon)

    write_state(env, [("saved", [("remote", {"remote": SAVED_REMOTE}), ("none", "none")]), ("bare", [])])
    daemon = serve(env, **DESK)
    assert tab_env.opener_vars(tab_environ(env, daemon, tab_named(env, "remote"))) == REMOTE_VARS
    assert tab_env.opener_vars(tab_environ(env, daemon, tab_named(env, "none"))) == {}
    with env.client() as client:
        bare = next(project for project in client.list() if project["name"] == "bare")
    assert len(bare["tabs"]) == 1, bare
    assert tab_env.opener_vars(tab_environ(env, daemon, int(bare["tabs"][0]["id"]))) == DESK, (
        "a project that saved no tabs gets the daemon's own"
    )
    stop(env, daemon)


def test_an_ssh_started_daemon_opens_its_own_tab_as_the_laptop_and_no_other_tab_as_it(env):
    daemon = serve(env, **LAPTOP)
    seed = only_tab(env)
    assert tab_env.opener_vars(tab_environ(env, daemon, seed)) == {
        "SSH_CONNECTION": LAPTOP["SSH_CONNECTION"],
        "SSH_CLIENT": LAPTOP["SSH_CLIENT"],
    }, "a remote opener brings its identity alone: no TTY, no agent"
    assert tab_env.opener_vars(tab_environ(env, daemon, open_bare(env))) == {}
    stop(env, daemon)

    write_state(env, [("saved", [("local", {"local": {"DISPLAY": ":12"}}), ("none", "none")])])
    daemon = serve(env, **LAPTOP)
    assert tab_env.opener_vars(tab_environ(env, daemon, tab_named(env, "local"))) == {"DISPLAY": ":12"}
    assert tab_env.opener_vars(tab_environ(env, daemon, tab_named(env, "none"))) == {}
    stop(env, daemon)
