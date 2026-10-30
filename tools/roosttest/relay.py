"""A recording relay in front of a Roost socket.

`roostctl --socket <relay>` reaches the real socket through it, and the
relay writes down every op each connection asked for and every answer it
passed back, so a test can say what `roostctl` *did* — which ops, in which
order, on which connection — rather than only what it printed.

Two rewrites of `identify`'s answer, and nothing else is ever touched:

* `forced_poll` strips `events.subscribe` from `ops` and drops
  `local_session_socket`, which is exactly the answer the Mac app or an
  older Roost gives: `roostctl wait` then takes its poll loop against the
  very UI a stream lane would have streamed from.
* `session_socket` replaces a present `local_session_socket`, so the
  session legs of a UI under `local-backend = session` can be routed
  through a second relay and recorded too.

Pushed frames pass through verbatim, and each side's close is passed on,
so a stream's `session.stopping` and its EOF reach the client as the
server sent them.

Each connection is one of three kinds, by the first line its client
sends ([`classify`]): an event stream (`events.subscribe`), a data plane
(an attach handshake — binary after that line, so it is pumped as bytes
in both directions) or a control connection (anything else). [`hold`]
holds what the server sends back on every connection of one kind until
it is released — which is how a test makes one leg of a client late
against the others (plan 084 A17, A19).
"""

from __future__ import annotations

import json
import os
import socket
import threading
import time
from dataclasses import dataclass

from client import scaled_timeout


#: The connection kinds [`Relay.hold`] takes.
CONTROL, EVENTS, DATA = "control", "events", "data"


def classify(first_line: bytes) -> str:
    """Which kind of connection opens with `first_line`."""
    try:
        frame = json.loads(first_line)
    except ValueError:
        return CONTROL
    if isinstance(frame, dict):
        if "attach" in frame:
            return DATA
        if frame.get("op") == "events.subscribe":
            return EVENTS
    return CONTROL


@dataclass(frozen=True)
class Seen:
    conn: int
    #: `"request"` or `"response"`.
    kind: str
    op: str


class Relay:
    def __init__(
        self,
        upstream,
        path,
        *,
        forced_poll: bool = False,
        session_socket=None,
    ):
        self.upstream = str(upstream)
        self.path = str(path)
        self.forced_poll = forced_poll
        self.session_socket = None if session_socket is None else str(session_socket)
        self.log: list[Seen] = []
        #: Every line the server pushed back on an event stream, in order.
        self.pushed: list[bytes] = []
        self._changed = threading.Condition()
        self._next_conn = 0
        self._listener: socket.socket | None = None
        self._open: list[socket.socket] = []
        # One gate per kind, open (set) unless a hold is on.
        self._gates = {kind: threading.Event() for kind in (CONTROL, EVENTS, DATA)}
        for gate in self._gates.values():
            gate.set()

    # -- lifecycle --------------------------------------------------------
    def __enter__(self) -> "Relay":
        if os.path.exists(self.path):
            os.unlink(self.path)
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(self.path)
        self._listener.listen(16)
        threading.Thread(target=self._accept, daemon=True).start()
        return self

    def __exit__(self, *_exc) -> None:
        for gate in self._gates.values():
            gate.set()
        for sock in [self._listener, *self._open]:
            try:
                sock.close()
            except OSError:
                pass
        try:
            os.unlink(self.path)
        except OSError:
            pass

    # -- holding a kind --------------------------------------------------
    def hold(self, kind: str) -> threading.Event:
        """Hold everything the server sends back on every connection of
        `kind` — open now or opened later — until the returned event is
        set. Requests still reach the server; only what it sends waits."""
        gate = self._gates[kind]
        gate.clear()
        return _Release(gate)

    # -- what was seen ----------------------------------------------------
    def requests(self) -> list[str]:
        with self._changed:
            return [seen.op for seen in self.log if seen.kind == "request"]

    def by_connection(self) -> dict[int, list[str]]:
        """Each connection's requests, in order."""
        with self._changed:
            conns: dict[int, list[str]] = {}
            for seen in self.log:
                if seen.kind == "request":
                    conns.setdefault(seen.conn, []).append(seen.op)
            return conns

    def wait_for_response(self, op: str, count: int = 1, timeout: float = 30.0) -> None:
        """Block until `count` answers to `op` have been passed back to a
        client — the moment that client can act on them."""
        deadline = time.monotonic() + scaled_timeout(timeout)
        with self._changed:
            while self._answered(op) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"{self.path} passed back {self._answered(op)} {op} answers, "
                        f"not {count}; requests so far: {self.requests()}"
                    )
                self._changed.wait(remaining)

    def wait_pushed(self, *needles: bytes, since: int = 0, timeout: float = 30.0) -> None:
        """Block until an event stream has passed back a line containing
        every one of `needles` — the moment its client can act on it.
        Only lines from position `since` of `pushed` on count: a test that
        marks `len(relay.pushed)` before acting waits for news, not for
        an earlier line that happens to match."""
        deadline = time.monotonic() + scaled_timeout(timeout)
        with self._changed:
            while not any(all(n in line for n in needles) for line in self.pushed[since:]):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"{self.path} passed back no event naming {needles!r}")
                self._changed.wait(remaining)

    def _answered(self, op: str) -> int:
        return sum(1 for seen in self.log if seen.kind == "response" and seen.op == op)

    # -- plumbing ---------------------------------------------------------
    def _record(self, conn: int, kind: str, op: str) -> None:
        with self._changed:
            self.log.append(Seen(conn, kind, op))
            self._changed.notify_all()

    def _accept(self) -> None:
        while True:
            try:
                client, _ = self._listener.accept()
            except OSError:
                return
            upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                upstream.connect(self.upstream)
            except OSError:
                client.close()
                continue
            self._next_conn += 1
            conn = self._next_conn
            self._open += [client, upstream]
            pending: dict = {}
            kind: list[str] = []
            classified = threading.Event()
            threading.Thread(
                target=self._requests,
                args=(conn, client, upstream, pending, kind, classified),
                daemon=True,
            ).start()
            threading.Thread(
                target=self._answers,
                args=(conn, upstream, client, pending, kind, classified),
                daemon=True,
            ).start()

    def _requests(self, conn: int, client, upstream, pending: dict, kind, classified) -> None:
        lines = _Lines(client)
        first = next(lines, None)
        kind.append(DATA if first is None else classify(first))
        classified.set()
        if first is None:
            _shutdown(upstream, socket.SHUT_WR)
            return
        if kind[0] == DATA:
            try:
                upstream.sendall(first + b"\n")
            except OSError:
                return
            _pump_bytes(client, upstream, residue=lines.buffered)
            _shutdown(upstream, socket.SHUT_WR)
            return
        for line in _chain(first, lines):
            try:
                request = json.loads(line)
            except ValueError:
                request = {}
            if isinstance(request, dict) and "op" in request:
                pending[request.get("id")] = request["op"]
                self._record(conn, "request", request["op"])
            try:
                upstream.sendall(line + b"\n")
            except OSError:
                return
        _shutdown(upstream, socket.SHUT_WR)

    def _answers(self, conn: int, upstream, client, pending: dict, kind, classified) -> None:
        classified.wait()
        gate = self._gates[kind[0]]
        if kind[0] == DATA:
            _pump_bytes(upstream, client, gate=gate)
            _shutdown(client, socket.SHUT_RDWR)
            return
        for line in _Lines(upstream):
            gate.wait()
            try:
                frame = json.loads(line)
            except ValueError:
                frame = {}
            op = None
            if isinstance(frame, dict) and "ok" in frame and frame.get("id") in pending:
                op = pending.pop(frame["id"])
                if op == "identify" and frame.get("ok"):
                    frame["result"] = self._rewrite_identify(frame["result"])
                    line = json.dumps(frame).encode()
            try:
                client.sendall(line + b"\n")
            except OSError:
                return
            if op is not None:
                self._record(conn, "response", op)
            elif kind[0] == EVENTS:
                with self._changed:
                    self.pushed.append(line)
                    self._changed.notify_all()
        _shutdown(client, socket.SHUT_RDWR)

    def _rewrite_identify(self, result: dict) -> dict:
        result = dict(result)
        if self.forced_poll:
            result["ops"] = [op for op in result.get("ops", []) if op != "events.subscribe"]
            result.pop("local_session_socket", None)
        elif self.session_socket is not None and result.get("local_session_socket"):
            result["local_session_socket"] = self.session_socket
        return result


class _Release:
    """What [`Relay.hold`] hands back: `set()` lets the held kind flow."""

    def __init__(self, gate: threading.Event) -> None:
        self._gate = gate

    def set(self) -> None:
        self._gate.set()


class _Lines:
    """The lines a socket sends, keeping what follows the last one read
    so a connection that turns binary can be handed on whole."""

    def __init__(self, sock) -> None:
        self._sock = sock
        self.buffered = b""

    def __iter__(self):
        return self

    def __next__(self) -> bytes:
        while b"\n" not in self.buffered:
            try:
                chunk = self._sock.recv(1 << 16)
            except OSError:
                raise StopIteration
            if not chunk:
                raise StopIteration
            self.buffered += chunk
        line, self.buffered = self.buffered.split(b"\n", 1)
        return line


def _chain(first: bytes, rest):
    yield first
    yield from rest


def _pump_bytes(src, dst, *, residue: bytes = b"", gate: threading.Event | None = None) -> None:
    """Copy `src` to `dst` byte for byte until either side ends, waiting
    on `gate` before each forward."""
    try:
        if residue:
            dst.sendall(residue)
        while True:
            chunk = src.recv(1 << 16)
            if not chunk:
                return
            if gate is not None:
                gate.wait()
            dst.sendall(chunk)
    except OSError:
        return


def _shutdown(sock, how) -> None:
    try:
        sock.shutdown(how)
    except OSError:
        pass

