"""End-to-end lane for the headless host session (`roost-session`).

Every test here spawns a REAL daemon against a throwaway profile (see
`session.py` for the per-OS isolation) and drives it over the same
newline-JSON IPC `roostctl` speaks. Nothing in this module touches a UI:
a session has no window, and the whole point of HS-1 is that the op set
works without one.

Two launch shapes, both exercised:

* `roost-session start --foreground` — the process that says `ready` is
  the process that serves. Cheapest to reason about, so most start-race
  cases use it.
* `roost-session start` — fork, `setsid`, verdict down a pipe, parent
  relays it to stdout and exits. This is what `roostctl session start`
  drives and what an operator runs, so the lifecycle cases use it.

And one case drives `roostctl session start/stop/status` itself, so the
CLI's own confirm-don't-trust-the-verdict logic is covered end to end
rather than only in its unit tests.

Condition waits only — `session.wait_until` scales every budget by
`ROOST_TEST_TIMEOUT_SCALE`, and no test uses a sleep for
synchronization.
"""

from __future__ import annotations

import os
import signal
import stat
import threading

import pytest
import session as sessionlib
from client import RoostError
from eventstream import EventStream

pytestmark = pytest.mark.session_daemon


# ---------------------------------------------------------------------------
# Fixtures + helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def env():
    """A throwaway session profile, torn down (processes first, then the
    directory) whatever the test did to it."""
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


def started(env, **overrides) -> sessionlib.Launch:
    """Daemonize a session and assert it came up. The common prologue."""
    launch = env.start_daemonized(**overrides)
    assert launch.returncode == 0, f"start failed: {launch.stdout!r} / {launch.stderr!r}"
    assert launch.verdict.kind == "ready", launch.verdict
    assert launch.verdict.pid and launch.verdict.pid > 0
    env.wait_answering()
    return launch


def tab_ids(client) -> set[int]:
    return {int(tab["id"]) for tab in client.tabs()}


def mode_of(path) -> int:
    return stat.S_IMODE(os.stat(path).st_mode)


def settled_ids(report: dict) -> set[int]:
    """The tabs a `session.stop` genuinely settled, and the assertion
    that it abandoned none.

    A reap report's three lists partition the tabs that were live when
    the stop began, but they are not equally good news: `abandoned` means
    the session stopped waiting on a child, so that process may still be
    running with nobody left to reap it. Every child these tests spawn is
    a cooperative `/bin/sh` or `sleep` that takes the default SIGHUP
    action, so a non-empty `abandoned` is a regression in the shutdown
    path rather than a property of the workload — which is why the check
    lives here, in the helper every "was my tab accounted for" assertion
    already goes through (plan 035 acceptance criterion 1).
    """
    assert report["abandoned"] == [], f"the stop abandoned children: {report}"
    return {int(i) for i in report["reaped"] + report["killed"]}


def first_project(client) -> int:
    """A fresh session hydrates exactly one project; every test that
    just needs *a* project to open tabs in reads it off here."""
    return int(client.list()[0]["id"])


def racer_argv(pidfile) -> list[str]:
    """A child that publishes its pid and then refuses to end on its own.

    `exec` keeps the pid: the file names the process the session has to
    account for, so "did anything outlive the stop" is a question the
    test can actually answer. `sleep` takes the default SIGHUP action, so
    a session that hangs its tabs up on the way out reaps this cleanly —
    only a session that *forgot* one leaves it running.
    """
    return ["/bin/sh", "-c", f"echo $$ > '{pidfile}'; exec sleep 300"]


def start_and_join(threads: list[threading.Thread], what: str, timeout: float = 120.0) -> None:
    """Start every thread, then join each — asserting none is still
    alive after its (scaled) budget. The common shape behind this
    module's racing-call tests."""
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=sessionlib.scaled_timeout(timeout))
        assert not thread.is_alive(), what


# ---------------------------------------------------------------------------
# 1. The lifecycle, daemonized
# ---------------------------------------------------------------------------


def test_daemonized_start_identify_stop(env):
    launch = started(env)

    identity = env.identify()
    assert identity["session_id"]
    assert identity["session_protocol"] >= 1
    assert identity["app_version"]
    assert identity["started_at"]
    # Every tab has an authoritative server terminal behind it, so the
    # session advertises what it can encode one as and which libghostty
    # pin it speaks. The exact sha belongs to `third_party/ghostty` — the
    # Rust side pins the literal — so this asserts the shape a client
    # negotiates on, not the value.
    assert "ghostty-snapshot" in identity["payload_kinds"]
    assert identity["libghostty_build"].startswith("ghostty-")

    report = env.stop_over_the_wire()
    assert set(report) >= {"reaped", "killed", "abandoned"}
    # The seeded first tab has to be settled — and a cooperative shell
    # must never end up abandoned (`settled_ids` asserts that).
    assert settled_ids(report), "a session with a hydrated tab reported an empty reap"

    env.wait_socket_gone()
    env.wait_pid_gone(launch.verdict.pid)
    assert env.state() is not None, "a clean stop must leave state.json behind"


# ---------------------------------------------------------------------------
# 2. SIGTERM takes the same path as `session.stop`
# ---------------------------------------------------------------------------


def test_sigterm_converges_with_stop_semantics(env):
    launch = started(env)
    pid = launch.verdict.pid

    # A witness for each half of "same path as session.stop": a titled
    # tab for the flush, and a long-lived child for the reap. Without the
    # child, a SIGTERM that flushed state but leaked every shell would
    # pass — and that is exactly the failure the signal path's self-dial
    # exists to prevent.
    pidfile = env.root / "sigterm-child.pid"
    with env.client() as client:
        project = first_project(client)
        tab = client.open_tab(project, cwd=str(env.launch_cwd), title="sigterm-witness")
        client.set_title(tab, "sigterm-witness")
        client.open_tab(project, cwd=str(env.launch_cwd), argv=racer_argv(pidfile))
    child = sessionlib.wait_until(
        lambda: sessionlib.read_pidfile(pidfile), 20.0, "the child to publish its pid"
    )

    os.kill(pid, signal.SIGTERM)

    env.wait_socket_gone()
    env.wait_pid_gone(pid)
    # The session hung its children up on the way out, not merely itself.
    env.wait_pid_gone(child, timeout=20.0)

    # `state.json` CONTENT, not an op's word for it.
    state = env.state()
    assert state is not None, "SIGTERM must leave a flushed state.json"
    titles = [t["title"] for p in state["projects"] for t in p["tabs"]]
    assert "sigterm-witness" in titles, state


# ---------------------------------------------------------------------------
# 3. Readiness failure: the runtime dir is not a directory
# ---------------------------------------------------------------------------


def test_a_bad_runtime_dir_fails_the_start_with_an_error_verdict(env):
    # `validate_runtime_dir` rejects rather than repairs, so a plain file
    # where the socket directory belongs is a permanent refusal — and it
    # happens in the forked child, so this also proves the parent relays
    # a failure verdict rather than hanging on its pipe.
    env.socket.parent.write_bytes(b"not a directory")

    launch = env.start_daemonized()

    assert launch.returncode == 1, launch
    assert launch.verdict.kind == "error", launch.verdict
    assert "runtime dir" in launch.verdict.reason, launch.verdict.reason
    assert not env.socket.exists()


# ---------------------------------------------------------------------------
# 4. The child dies on its way up; the parent still reports
# ---------------------------------------------------------------------------


def test_a_child_that_cannot_come_up_fails_the_parent(env):
    """A start whose state directory can never be created.

    The pinned case is "child death → the parent reports a failure". A
    *silent* death (pipe EOF with no verdict) is not producible from
    outside the binary — every failure inside `start` is caught and
    reported as an `error:` verdict, and the only remaining route is an
    abort, which the daemon has no test hook for. So this drives the
    reachable half of the same contract: the child fails after the fork,
    and the parent exits nonzero with a readable reason instead of
    reporting a session that is not there.

    The EOF-before-verdict branch is covered where it *is* reachable —
    in-process, against the reader rather than the fork:
    `daemonize::read_verdict` returns `Ok(None)` on EOF, and `roost-cli`
    pins the same shape end-to-end over a real process in
    `session::tests::a_launcher_that_says_nothing_at_all_fails_the_start`
    and `…::an_unterminated_verdict_is_not_accepted_from_a_real_process`.
    """
    blocker = env.root / "blocked"
    blocker.write_bytes(b"")

    launch = env.start_daemonized(ROOST_STATE_DIR=str(blocker / "state"))

    assert launch.returncode != 0, launch
    assert launch.verdict.kind == "error", launch.verdict
    assert launch.verdict.reason, "a failure verdict must carry a reason"
    assert env.answering() is None


# ---------------------------------------------------------------------------
# 5. Duplicate stop
# ---------------------------------------------------------------------------


def test_a_second_concurrent_stop_is_terminal(env):
    """Two `session.stop` calls in flight: one owns the shutdown.

    Both outcomes for the loser are accepted because both are terminal
    and which one it gets is a timing detail: `shutting-down` when it
    reaches the latch, or a closed connection when the winner's finalizer
    cancels it first. What must never happen is two reap reports.
    """
    started(env)

    winners: list[dict] = []
    losers: list[Exception] = []
    barrier = threading.Barrier(2)

    def fire(client):
        barrier.wait()
        try:
            winners.append(client.call("session.stop"))
        except (RoostError, OSError) as error:
            losers.append(error)

    first, second = env.client(timeout=90.0), env.client(timeout=90.0)
    threads = [threading.Thread(target=fire, args=(c,)) for c in (first, second)]
    start_and_join(threads, "a session.stop never returned")
    first.close()
    second.close()

    assert len(winners) == 1, f"exactly one stop may own the shutdown (got {winners})"
    assert len(losers) == 1, losers
    loser = losers[0]
    if isinstance(loser, RoostError):
        assert loser.code == "shutting-down", loser

    env.wait_socket_gone()


# ---------------------------------------------------------------------------
# 6. Two concurrent first starts (D2b)
# ---------------------------------------------------------------------------


def test_two_concurrent_first_starts_elect_one_server(env):
    """Two starts against one empty profile elect exactly one server.

    What this can and cannot prove: Python can only launch two processes
    and read what they say, so a run where the first finishes binding
    before the second reaches the flock passes identically to one where
    they genuinely collide. The *narrow* D2b race — two processes inside
    the create-or-validate and lock windows at once — is pinned where it
    can be forced deterministically, in Rust:
    `roost-ipc/tests/runtime_dir_test.rs::racing_creators_all_validate_the_same_leaf`
    and
    `roost-engine/tests/instance_locks_test.rs::exactly_one_of_two_starts_wins_a_stale_socket`.

    What this adds on top is the end-to-end shape those cannot reach: two
    real `roost-session` processes, a `ready`/`already-running` split on
    the wire, exit 0 for the loser, and a socket that answers afterwards.
    """
    launched: list[sessionlib.Foreground] = []
    barrier = threading.Barrier(2)
    lock = threading.Lock()

    def launch():
        barrier.wait()
        foreground = env.start_foreground()
        with lock:
            launched.append(foreground)

    threads = [threading.Thread(target=launch) for _ in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=sessionlib.scaled_timeout(60.0))
    assert len(launched) == 2

    verdicts = [foreground.verdict() for foreground in launched]
    kinds = sorted(verdict.kind for verdict in verdicts)
    assert kinds == ["already-running", "ready"], [v.raw for v in verdicts]

    ready = next(f for f, v in zip(launched, verdicts) if v.kind == "ready")
    loser = next(f for f, v in zip(launched, verdicts) if v.kind == "already-running")

    # Losing the race is a successful no-op, not a failure.
    assert loser.wait() == 0
    loser.assert_single_stdout_line()

    identity = env.wait_answering()
    assert identity["session_id"]

    env.stop_over_the_wire()
    env.wait_socket_gone()
    assert ready.wait() == 0
    # The winner served a whole session between its verdict and its exit
    # and still said nothing more on stdout — the log goes to the file
    # and the stderr tee, which is what keeps stdout parser-free.
    ready.assert_single_stdout_line()


# ---------------------------------------------------------------------------
# 7. A killed session leaves a stale socket the next start recovers from
# ---------------------------------------------------------------------------


def test_a_restart_recovers_from_a_sigkilled_session(env):
    first = started(env)
    first_pid = first.verdict.pid

    os.kill(first_pid, signal.SIGKILL)
    env.wait_pid_gone(first_pid)
    # SIGKILL runs no finalizer, so the socket file is still on disk and
    # the next start has to probe it, find it dead, and unlink it.
    assert env.socket.exists(), "SIGKILL should leave the socket behind"

    second = started(env)
    assert second.verdict.pid != first_pid
    identity = env.identify()
    assert identity["session_id"]

    env.stop_over_the_wire()
    env.wait_socket_gone()


# ---------------------------------------------------------------------------
# 8. The flock loser reports already-running (daemonized shape)
# ---------------------------------------------------------------------------


def test_a_second_daemonized_start_reports_already_running(env):
    first = started(env)

    second = env.start_daemonized()
    assert second.returncode == 0, second
    assert second.verdict.kind == "already-running", second.verdict
    if second.verdict.pid is not None:
        assert second.verdict.pid == first.verdict.pid

    # The original is still the one serving.
    assert env.identify()["session_id"]
    env.stop_over_the_wire()
    env.wait_socket_gone()


# ---------------------------------------------------------------------------
# 9. File modes (the umask posture)
# ---------------------------------------------------------------------------


def test_the_session_owns_its_files_alone(env):
    started(env)

    assert mode_of(env.socket) == 0o600, oct(mode_of(env.socket))
    assert mode_of(env.runtime_dir) == 0o700, oct(mode_of(env.runtime_dir))
    assert mode_of(env.state_dir) == 0o700, oct(mode_of(env.state_dir))
    assert mode_of(env.log_dir) == 0o700, oct(mode_of(env.log_dir))
    assert env.state_json.is_file(), "hydration writes state.json through"
    assert mode_of(env.state_json) == 0o600, oct(mode_of(env.state_json))
    assert env.log_file.is_file()
    assert mode_of(env.log_file) == 0o600, oct(mode_of(env.log_file))

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 10. The saved layout comes back
# ---------------------------------------------------------------------------


def test_the_layout_is_restored_across_a_restart(env):
    started(env)

    alpha = env.root / "alpha"
    beta = env.root / "beta"
    alpha.mkdir()
    beta.mkdir()

    with env.client() as client:
        projects = client.list()
        assert len(projects) == 1, projects
        # A first-ever start seeds its project from the launch directory.
        assert projects[0]["cwd"] == str(env.launch_cwd)
        project = int(projects[0]["id"])

        first = client.open_tab(project, cwd=str(alpha))
        second = client.open_tab(project, cwd=str(beta))
        client.set_title(second, "renamed-by-hand")
        client.focus(second)

        before = [(t["cwd"], t["title"]) for t in client.tabs()]
        assert (str(beta), "renamed-by-hand") in before
        assert first != second

    settled_ids(env.stop_over_the_wire())
    env.wait_socket_gone()

    # The file is the contract between the two runs.
    state = env.state()
    saved = [(t["cwd"], t["title"], t["user_titled"]) for t in state["projects"][0]["tabs"]]
    assert (str(beta), "renamed-by-hand", True) in saved

    started(env)
    with env.client() as client:
        restored = client.tabs()
        cwds = [tab["cwd"] for tab in restored]
        assert cwds == [cwd for cwd, _ in before], restored
        titles = {tab["title"] for tab in restored}
        assert "renamed-by-hand" in titles
        # Fresh shells, not the previous run's processes: the ids are all new.
        assert first not in tab_ids(client) and second not in tab_ids(client)
        # The selection was restored by position, so it lands on the same
        # row rather than on an id that no longer exists.
        active = client.call("identify")["active_tab_id"]
        active_row = next(tab for tab in restored if tab["id"] == active)
        assert active_row["cwd"] == str(beta)
        # The title LOCK, not just the title: hydration re-asserts it, so
        # the restored tab reports user_titled on the wire and a later
        # `cd` cannot silently re-derive the name from the new basename.
        restored_beta = next(tab for tab in restored if tab["cwd"] == str(beta))
        assert restored_beta["user_titled"] is True, restored_beta
        assert all(
            tab["user_titled"] is False for tab in restored if tab is not restored_beta
        ), restored

    settled_ids(env.stop_over_the_wire())
    env.wait_socket_gone()

    # And it survives the SECOND write-out too. Restoring the flag into
    # the live model is only half of it — a hydration that set the title
    # without the lock would pass every assertion above and then persist
    # `user_titled: false`, losing the rename on the *next* restart.
    after = env.state()
    assert (str(beta), "renamed-by-hand", True) in [
        (t["cwd"], t["title"], t["user_titled"]) for t in after["projects"][0]["tabs"]
    ], after


# ---------------------------------------------------------------------------
# 11. A shell that exits closes its row, headlessly
# ---------------------------------------------------------------------------


def test_a_natural_shell_exit_closes_the_row(env):
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = client.open_tab(
            project, cwd=str(env.launch_cwd), argv=["/bin/sh", "-c", "exit 0"]
        )
        sessionlib.wait_until(
            lambda: tab not in tab_ids(client), 20.0, f"tab {tab} to close on its own"
        )

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 12. OSC from a real child reaches tab.list
# ---------------------------------------------------------------------------


def test_osc_title_and_cwd_from_a_child_reach_tab_list(env):
    started(env)

    target = env.root / "osc-cwd"
    target.mkdir()
    # OSC 7 first, then OSC 0: a cwd change re-derives a non-user title
    # from the basename, so the title has to be the later write.
    script = (
        f"printf '\\033]7;file://%s\\007' '{target}'; "
        "printf '\\033]0;osc-titled\\007'; "
        "exec sleep 300"
    )

    with env.client() as client:
        project = first_project(client)
        tab = client.open_tab(project, cwd=str(env.launch_cwd), argv=["/bin/sh", "-c", script])

        def settled():
            row = next((t for t in client.tabs() if int(t["id"]) == tab), None)
            if row and row["title"] == "osc-titled" and row["cwd"] == str(target):
                return row
            return None

        sessionlib.wait_until(settled, 20.0, f"tab {tab} to report its OSC title + cwd")

    report = env.stop_over_the_wire()
    assert tab in settled_ids(report), (tab, report)
    env.wait_socket_gone()


# ---------------------------------------------------------------------------
# 13. tab.open racing session.stop leaves nothing behind
# ---------------------------------------------------------------------------


def test_a_tab_that_beat_the_stop_is_reaped_not_orphaned(env):
    """The settled half of the race: a tab that is already open when the
    stop begins is reaped or killed — never abandoned — and its process
    is gone."""
    started(env)

    pidfile = env.root / "settled.pid"
    with env.client() as client:
        project = first_project(client)
        tab = client.open_tab(project, cwd=str(env.launch_cwd), argv=racer_argv(pidfile))
    pid = sessionlib.wait_until(
        lambda: sessionlib.read_pidfile(pidfile), 20.0, "the racer to publish its pid"
    )

    report = env.stop_over_the_wire()
    # `settled_ids` is the strong form: it excludes `abandoned` AND
    # asserts the list is empty, so "the session gave up on it" cannot
    # satisfy "the session accounted for it".
    assert tab in settled_ids(report), (tab, report)
    env.wait_socket_gone()
    env.wait_pid_gone(pid, timeout=20.0)


def test_a_tab_open_racing_stop_leaves_no_orphan(env):
    started(env)

    pidfile = env.root / "racer.pid"
    argv = racer_argv(pidfile)

    opened: list[int] = []
    refused: list[Exception] = []
    report: list[dict] = []
    stop_failure: list[Exception] = []
    barrier = threading.Barrier(2)

    with env.client() as control:
        project = first_project(control)

    stopper = env.client(timeout=90.0)
    opener = env.client(timeout=90.0)

    def do_stop():
        barrier.wait()
        try:
            report.append(stopper.call("session.stop"))
        except (RoostError, OSError) as error:
            stop_failure.append(error)

    def do_open():
        barrier.wait()
        try:
            opened.append(opener.open_tab(project, cwd=str(env.launch_cwd), argv=argv))
        except (RoostError, OSError) as error:
            refused.append(error)

    threads = [threading.Thread(target=do_stop), threading.Thread(target=do_open)]
    start_and_join(threads, "a racing call never returned")
    stopper.close()
    opener.close()

    assert report, f"the stop must still have produced a reap report ({stop_failure})"
    assert opened or refused, "the racing open neither opened nor was refused"
    if opened:
        # An open that got past the latch was admitted through the
        # mutation barrier, which the stop waits out — so its tab is in
        # the reap set by construction, never an unowned process.
        assert opened[0] in settled_ids(report[0]), (opened, report[0])
    else:
        error = refused[0]
        if isinstance(error, RoostError):
            assert error.code in {"shutting-down", "not-found"}, error

    env.wait_socket_gone()

    # The real assertion: whatever the race decided, no shell outlives it.
    #
    # A bare `pidfile.exists()` here would be a TOCTOU hole on the
    # interesting branch: an open that SUCCEEDED spawned a child, and a
    # child that had not been scheduled yet at check time writes its
    # pidfile a moment later — after the test has already declared
    # victory. So when the open won, wait a bounded window for the pid to
    # appear before concluding it never ran; only the branch where the
    # open was refused (no tab, therefore no child) may check once.
    if opened:
        pid = sessionlib.wait_for_pidfile(pidfile, timeout=5.0)
        # `None` is a legitimate outcome, not a miss: the stop's sweep can
        # reach the child between `fork` and `exec`, so it dies before it
        # can publish anything.
        if pid is not None:
            env.wait_pid_gone(pid, timeout=20.0)
    else:
        assert sessionlib.read_pidfile(pidfile) is None, (
            "a refused tab.open must never have spawned a child, but one "
            f"published a pid to {pidfile}"
        )


# ---------------------------------------------------------------------------
# 14. The push stream, read from Python
# ---------------------------------------------------------------------------


def test_events_push_reaches_a_python_subscriber(env):
    started(env)

    with env.client() as client:
        # The stream is lease-gated: a client that never connected is
        # told which step it skipped rather than handed a stream.
        with EventStream(env.socket) as leaseless:
            with pytest.raises(RoostError) as refused:
                leaseless.subscribe()
            assert refused.value.code == "connect-required"
            assert "session.connect" in refused.value.message

        lease = client.call("session.connect", {"takeover": False})["lease"]
        # Bind the length before asserting so a failure dump prints the
        # number, never the bearer token itself.
        lease_len = len(lease)
        assert lease_len == 32

        with EventStream(env.socket, lease=lease) as stream:
            fence = stream.subscribe()
            # The snapshot's revision is the same fence the ack names,
            # which is what makes "discard everything <= this" a usable
            # rule.
            snapshot = client.call("tab.list")
            assert snapshot["revision"] >= fence

            project = int(snapshot["projects"][0]["id"])
            tab = client.open_tab(project, cwd=str(env.launch_cwd), title="pushed")

            batches, envelope = stream.recv_until("tab.opened", timeout=20.0)
            # No holes: every commit pushes a batch, empty ones included.
            stream.expect_contiguous(batches, fence)
            assert int(envelope["data"]["tab"]["id"]) == tab

            # And the stop is announced, not just enacted: the last frame
            # before the close names the reason.
            env.stop_over_the_wire()
            assert stream.recv_stopping(timeout=30.0) == "stop"


# ---------------------------------------------------------------------------
# 15. The CLI's own verbs, end to end
# ---------------------------------------------------------------------------


def test_roostctl_session_start_status_stop(env):
    status = env.roostctl("session", "status")
    assert status.returncode == 3, status.stdout + status.stderr
    assert "not running" in status.stdout

    start = env.roostctl("session", "start")
    assert start.returncode == 0, start.stdout + start.stderr
    assert start.stdout.splitlines()[0] == "started", start.stdout
    assert "session_id=" in start.stdout
    identity = env.wait_answering()
    for line in start.stdout.splitlines():
        if line.startswith("session_id="):
            assert line.removeprefix("session_id=") == identity["session_id"]

    again = env.roostctl("session", "start")
    assert again.returncode == 0, again.stdout + again.stderr
    assert again.stdout.splitlines()[0] == "already-running", again.stdout

    status = env.roostctl("session", "status")
    assert status.returncode == 0, status.stdout + status.stderr
    assert "projects=1" in status.stdout
    assert "tabs=" in status.stdout

    stop = env.roostctl("session", "stop")
    assert stop.returncode == 0, stop.stdout + stop.stderr
    assert "stopped" in stop.stdout
    env.wait_socket_gone()

    # Stopping a stopped session is a success, `systemctl stop` style.
    stop_again = env.roostctl("session", "stop")
    assert stop_again.returncode == 0, stop_again.stdout + stop_again.stderr
    assert "not running" in stop_again.stdout

    status = env.roostctl("session", "status")
    assert status.returncode == 3, status.stdout + status.stderr
