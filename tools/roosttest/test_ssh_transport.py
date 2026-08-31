"""The SSH transport's far side, end to end against a real session.

`roost-session client-bridge` is what a client execs on the host —
`ssh -T host 'exec roost-session client-bridge'`, one process per
accepted connection — and this lane proves that the whole control
plane, the whole data plane, and both half-closes survive the trip
through it.

# Why the ssh hop is absent, and why that is honest

The local half of the transport is [`roost_ipc::ssh::SshTunnel`], which
is Rust and not importable from Python. Rather than approximate it, this
lane builds the same chain out of its real parts:

    client (this test)  →  a UDS this test binds
                        →  `roost-session client-bridge` (the REAL binary,
                           one child per connection, stdio-pumped)
                        →  the REAL session's socket

[`BridgeListener`] is the local half — accept, spawn, pump both
directions, propagate each half-close — in the same spawn-per-connection
shape `SshTunnel` pins. What is missing is exactly one link: the `ssh`
process in the middle, which moves bytes and nothing else. That link is
covered where it can be covered honestly — `crates/roost-ipc/tests/
ssh_transport_test.rs` drives the real tunnel against
`fixtures/fake-ssh.sh`, and `test_host_ssh.py` drives the real UI
through a real tunnel over that same fake — so nothing here is a
stand-in for something untested elsewhere.

What this lane owns, and nothing else does, is the pairing: the real
far-side binary in front of the real session, speaking the real
protocol, with a client at the other end of two pumps.

Everything is condition-waited; there are no sleeps.
"""

from __future__ import annotations

import contextlib
import json
import socket
import subprocess
import threading
import uuid

import dataplane
import pytest
import session as sessionlib
from client import Roost, scaled_timeout
from dataplane import DataPlane
from eventstream import EventStream

# The prologue is the session lane's, unchanged: same daemon, same test
# mode, same lease + ticket vocabulary. Only the socket a client dials
# is different here, which is the whole point of the module.
from test_session_attach import (
    COLS,
    ROWS,
    attach_ticket,
    connect_lease,
    first_project,
    pty_payload,
    quiet_tab,
    started,
)

pytestmark = pytest.mark.session_daemon


# One read's worth of wire bytes. Matches the bridge's own `CHUNK`
# (`crates/roost-session/src/bridge.rs`), so neither side of a pump is
# the one that chops a snapshot frame into syscalls.
CHUNK = 64 * 1024


# ---------------------------------------------------------------------------
# The tunnel's local half
# ---------------------------------------------------------------------------


class BridgeListener:
    """A UDS that answers every connection with its own `client-bridge`.

    The pumps are deliberately dumb — no framing, no line buffering, one
    `read` to one `write` — because that is the property the bridge is
    specified on: a JSON handshake line and the binary residue behind it
    can land in a single read, and both ends must see the bytes exactly
    as they were written.

    Half-close is propagated rather than collapsed into a close, in both
    directions:

    * the client's write half ends → this closes the child's **stdin**
      only, and the child half-closes the session socket, so the session
      reads a clean EOF;
    * the child's stdout ends → this shuts down the connection's **write**
      half, so the client reads a clean EOF, and then the read half is
      shut down too (the real transport's `ssh` exec has exited by then;
      leaving the socket half-open would strand the pump on a client that
      holds its write half open for the life of an events connection).
    """

    def __init__(self, env: sessionlib.SessionEnv, name: str = "bridge.sock"):
        self.env = env
        # Under the profile root (`/tmp/roost-hs-…`), which `session.py`
        # already keeps short for `sun_path`'s sake.
        self.path = env.root / name
        self.stderr_log = env.root / "client-bridge.stderr"
        self._children: list[subprocess.Popen] = []
        self._conns: list[socket.socket] = []
        self._lock = threading.Lock()
        self._closing = threading.Event()
        self._server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._server.bind(str(self.path))
        self._server.listen(16)
        # Not a wait: the accept loop has to notice `close()` between
        # connections, and a blocking accept never would.
        self._server.settimeout(0.2)
        threading.Thread(target=self._accept_loop, daemon=True).start()

    # -- lifecycle --------------------------------------------------------
    def __enter__(self) -> "BridgeListener":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    def close(self) -> None:
        self._closing.set()
        with contextlib.suppress(OSError):
            self._server.close()
        with self._lock:
            conns, children = list(self._conns), list(self._children)
        for conn in conns:
            with contextlib.suppress(OSError):
                conn.close()
        for child in children:
            if child.poll() is None:
                with contextlib.suppress(OSError):
                    child.kill()
            with contextlib.suppress(subprocess.TimeoutExpired, OSError):
                child.wait(timeout=scaled_timeout(10.0))
        with contextlib.suppress(OSError):
            self.path.unlink()

    # -- the children -----------------------------------------------------
    @property
    def children(self) -> list[subprocess.Popen]:
        """Every `client-bridge` this listener has spawned, in accept
        order."""
        with self._lock:
            return list(self._children)

    def wait_children(self, count: int, timeout: float = 30.0) -> list[subprocess.Popen]:
        def enough() -> list[subprocess.Popen] | None:
            spawned = self.children
            return spawned if len(spawned) >= count else None

        return sessionlib.wait_until(
            enough, timeout, f"{count} client-bridge child(ren) at {self.path}"
        )

    def kill_children(self) -> list[subprocess.Popen]:
        """SIGKILL every live child — the transport dying under a client
        that is mid-stream. Returns the ones that were actually
        signalled."""
        killed = []
        for child in self.children:
            if child.poll() is None:
                with contextlib.suppress(OSError):
                    child.kill()
                killed.append(child)
        return killed

    def stderr_text(self) -> str:
        try:
            return self.stderr_log.read_text(errors="replace")
        except OSError:
            return ""

    # -- clients ----------------------------------------------------------
    def client(self, timeout: float = 30.0) -> Roost:
        """A control client that reaches the session *through* the
        bridge."""
        return Roost(self.path, timeout=scaled_timeout(timeout))

    # -- the pumps --------------------------------------------------------
    def _accept_loop(self) -> None:
        while not self._closing.is_set():
            try:
                conn, _ = self._server.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn: socket.socket) -> None:
        conn.settimeout(None)
        # Appended to, never truncated: one file holds every child's
        # diagnostics for the whole test, and a failure can read it.
        with open(self.stderr_log, "ab") as errors:
            child = subprocess.Popen(
                [str(self.env.binary), "client-bridge"],
                cwd=str(self.env.launch_cwd),
                env=self.env.command_env(),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=errors,
            )
        with self._lock:
            self._children.append(child)
            self._conns.append(conn)

        upstream = threading.Thread(target=self._upstream, args=(conn, child), daemon=True)
        downstream = threading.Thread(target=self._downstream, args=(conn, child), daemon=True)
        upstream.start()
        downstream.start()
        downstream.join()
        # The far side is gone, so nothing this connection could still
        # send has anywhere to go: releasing the read half is what lets
        # the upstream pump finish instead of parking forever on a client
        # that keeps its write half open.
        with contextlib.suppress(OSError):
            conn.shutdown(socket.SHUT_RD)
        upstream.join(scaled_timeout(10.0))
        with contextlib.suppress(OSError):
            conn.close()
        with contextlib.suppress(subprocess.TimeoutExpired):
            child.wait(timeout=scaled_timeout(10.0))

    def _upstream(self, conn: socket.socket, child: subprocess.Popen) -> None:
        try:
            while True:
                chunk = conn.recv(CHUNK)
                if not chunk:
                    break
                child.stdin.write(chunk)
                child.stdin.flush()
        except (OSError, ValueError):
            pass
        finally:
            # stdin EOF, not a kill: the child turns it into a half-close
            # of the session socket.
            with contextlib.suppress(OSError, ValueError):
                child.stdin.close()

    def _downstream(self, conn: socket.socket, child: subprocess.Popen) -> None:
        try:
            while True:
                # `read1`, never `read`: `read` would block for a full
                # buffer, and the wire has no framing this side can use
                # to know one is coming.
                chunk = child.stdout.read1(CHUNK)
                if not chunk:
                    break
                conn.sendall(chunk)
        except (OSError, ValueError):
            pass
        finally:
            with contextlib.suppress(OSError):
                conn.shutdown(socket.SHUT_WR)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def env():
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


@pytest.fixture
def bridge(env):
    """A bridge listener in front of `env`'s session. The session itself
    is started by the test — one case here needs a profile with nothing
    listening."""
    with BridgeListener(env) as listener:
        yield listener


def marker(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def raw_connect(path, timeout: float = 30.0) -> socket.socket:
    """A bare socket on the bridge, for the cases whose subject is the
    half-close rather than the protocol."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(scaled_timeout(timeout))
    sock.connect(str(path))
    return sock


def request_line(sock: socket.socket, op: str) -> dict:
    """One request, one response line, on a socket the caller owns."""
    sock.sendall((json.dumps({"id": "1", "op": op, "params": {}}) + "\n").encode())
    buf = b""
    while b"\n" not in buf:
        chunk = sock.recv(CHUNK)
        assert chunk, f"the bridge closed before answering {op}"
        buf += chunk
    return json.loads(buf.split(b"\n", 1)[0].decode())


def read_to_eof(sock: socket.socket) -> bytes:
    """Read until the peer closes. A `TimeoutError` here is the failure
    the caller cares about — a connection that never ended — so it is
    deliberately not caught."""
    out = b""
    while True:
        chunk = sock.recv(CHUNK)
        if not chunk:
            return out
        out += chunk


# ---------------------------------------------------------------------------
# 1. The control plane is transparent through the bridge
# ---------------------------------------------------------------------------


def test_the_bridge_answers_exactly_what_the_socket_does(env, bridge):
    """Identify, connect and subscribe, compared against the same ops
    dialed directly.

    `session.identify` is the strongest available equality: every field
    is stable (a build string, a session id, a start timestamp), so a
    bridge that dropped, reordered or re-framed a byte cannot produce the
    same dict. The event stream is the other half — it is the one shape
    the request client cannot carry, because the server stops reading and
    starts pushing, and a transport that only worked for
    request/response would fail exactly there.
    """
    started(env)
    direct = env.identify()

    with bridge.client() as client:
        assert client.call("session.identify") == direct

        lease = connect_lease(client)
        with EventStream(bridge.path, lease=lease) as stream:
            fence = stream.subscribe()
            project = first_project(client)
            tab = client.open_tab(project, cwd=str(env.launch_cwd), title="through-the-bridge")

            batches, envelope = stream.recv_until("tab.opened", timeout=20.0)
            stream.expect_contiguous(batches, fence)
            assert int(envelope["data"]["tab"]["id"]) == tab

    # One child per connection is the pinned shape (`SshTunnel`'s
    # spawn-per-connection rule): the control client and the event stream
    # are two connections, so they are two bridges — a serialized
    # transport would have deadlocked on the stream that never ends.
    assert len(bridge.children) >= 2, bridge.children


# ---------------------------------------------------------------------------
# 2. The data plane, byte for byte, through two pumps
# ---------------------------------------------------------------------------


def test_a_full_attach_streams_and_echoes_through_the_bridge(env, bridge):
    """Snapshot in, input out, exit at the end — all of it bridged.

    The echo is what makes this a byte-fidelity assertion rather than a
    liveness one: the marker travels the client→session direction through
    the bridge's stdin pump, comes back through its stdout pump, and is
    compared to the literal bytes that were sent.
    """
    started(env)
    with bridge.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        line = marker("BRIDGED")
        tab = client.open_tab(
            project,
            cwd=str(env.launch_cwd),
            cols=COLS,
            rows=ROWS,
            argv=["/bin/sh", "-c", 'read line; printf "ECHO:%s\\r\\n" "$line"; exit 0'],
        )

        ticket = attach_ticket(client, lease, tab)
        with DataPlane(bridge.path) as conn:
            reply = conn.handshake(ticket["attach_token"])
            assert reply.ok, (reply.code, reply.message)
            assert reply.kind == dataplane.GHOSTTY_SNAPSHOT
            # SNAP → READY → FINISH, in that order, is the snapshot
            # contract; the scanner asserts the ordering as it reads.
            conn.read_until_finish()
            conn.snap.assert_ready_leads(expect_history=False)

            conn.send_input(f"{line}\n".encode())
            ending = conn.drain_to_close(timeout=30.0)
            assert ending.kind == "exit", ending
            assert ending.exit_code == 0, ending
            assert f"ECHO:{line}".encode() in pty_payload(ending.frames), (
                "the marker did not survive both pumps intact"
            )


# ---------------------------------------------------------------------------
# 3. A bridge that dies mid-attach strands nothing
# ---------------------------------------------------------------------------


def test_a_killed_bridge_child_leaves_no_wedged_state(env, bridge):
    """SIGKILL every `client-bridge`, then do the whole thing again.

    This is the transport failing in the one way a graceful close cannot
    cover: no half-close, no final frame, just a dead pipe. What must
    survive it is the *server* — the lease it had handed out, the attach
    it was serving and the tab behind them — so the proof is a second
    connection that connects, takes the lease back and attaches to the
    same tab.
    """
    started(env)
    with bridge.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        ticket = attach_ticket(client, lease, tab)
        conn = DataPlane(bridge.path)
        assert conn.handshake(ticket["attach_token"]).ok
        conn.read_until_ready()

        assert bridge.kill_children(), "nothing was live to kill"
        # The client's end of a killed transport: EOF, not a labelled
        # ending — the peer that would have written the label is gone.
        assert conn.drain_to_close(timeout=30.0).kind == "eof"
        conn.close()

    # A fresh connection over a fresh child. `takeover` because the old
    # lease outlives the transport that carried it — the session cannot
    # tell a killed bridge from a client that walked away, which is
    # exactly why the lease is reclaimed rather than assumed free.
    with bridge.client() as revived:
        again = connect_lease(revived, takeover=True)
        assert tab in {int(row["id"]) for row in revived.tabs()}
        ticket = attach_ticket(revived, again, tab)
        with DataPlane(bridge.path) as conn:
            assert conn.handshake(ticket["attach_token"]).ok
            conn.read_until_ready()
        assert revived.dump(tab)["rows_text"] is not None


# ---------------------------------------------------------------------------
# 4. Half-close, in both directions
# ---------------------------------------------------------------------------


def test_each_half_close_ends_the_chain_cleanly(env, bridge):
    """The client's write half first, then the session's whole side.

    Both are the same claim from opposite ends: a shutdown at one end of
    the chain has to arrive at the other end as a shutdown, not as a
    dropped connection or a process that lingers. The child's **exit
    code** is the strict half of it — a bridge that fell out of its pump
    on an error would report 1 and print a `client-bridge:` line, so a 0
    with empty stderr is the whole clean-EOF contract in two assertions.
    """
    started(env)

    # (a) the local end half-closes: stdin EOF → a clean EOF at the
    #     session, which answers by closing, which comes back as EOF here.
    local = raw_connect(bridge.path)
    assert request_line(local, "session.identify")["ok"]
    child = bridge.wait_children(1)[0]
    local.shutdown(socket.SHUT_WR)
    assert read_to_eof(local) == b"", "the session wrote something after the half-close"
    local.close()
    sessionlib.wait_until(
        lambda: child.poll() is not None, 30.0, "the bridge child to exit after stdin EOF"
    )
    assert child.returncode == 0, (child.returncode, bridge.stderr_text())

    # And the session is untouched by one client leaving: the next
    # connection is ordinary.
    with bridge.client() as after:
        lease = connect_lease(after)
        project = first_project(after)
        tab = quiet_tab(after, project, env.launch_cwd)

        # (b) the far end goes away: `session.stop` while a data
        #     connection is attached through the bridge.
        ticket = attach_ticket(after, lease, tab)
        conn = DataPlane(bridge.path)
        assert conn.handshake(ticket["attach_token"]).ok
        conn.read_until_ready()
        attached_child = bridge.children[-1]

        env.stop_over_the_wire()
        conn.drain_to_close(timeout=60.0)
        conn.close()

    sessionlib.wait_until(
        lambda: attached_child.poll() is not None,
        30.0,
        "the attached bridge child to exit when the session stopped",
    )
    assert attached_child.returncode == 0, (attached_child.returncode, bridge.stderr_text())
    assert "client-bridge:" not in bridge.stderr_text(), bridge.stderr_text()


# ---------------------------------------------------------------------------
# 5. No session: the failure the client classifies on
# ---------------------------------------------------------------------------


def test_client_bridge_without_a_session_fails_by_name(env):
    """The one bridge failure that has a *contract*.

    `roost_ipc::ssh::classify_ssh_failure` matches `client-bridge: no
    session` on stderr to tell "the host is fine, nothing is running
    there" from a transport failure, so this message is wire, not prose.
    The empty stdout is the other half of the same contract: stdout is
    the wire, and a byte of diagnostics on it would corrupt the first
    frame of a connection that had succeeded.
    """
    # Deliberately no `started(env)` — the profile is real, the socket is
    # not there.
    result = subprocess.run(
        [str(env.binary), "client-bridge"],
        cwd=str(env.launch_cwd),
        env=env.command_env(),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=scaled_timeout(60.0),
    )
    # 1, never ssh's own 255: the client has to tell "the bridge ran and
    # found nothing" from "the transport failed".
    assert result.returncode == 1, (result.returncode, result.stderr)
    assert result.stdout == "", result.stdout
    assert "client-bridge: no session is listening at" in result.stderr, result.stderr
    assert str(env.socket) in result.stderr, result.stderr
    assert "roostctl session start" in result.stderr, result.stderr
