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
"""

from __future__ import annotations

import json
import os
import socket
import threading
import time
from dataclasses import dataclass

from client import scaled_timeout


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
        self._changed = threading.Condition()
        self._next_conn = 0
        self._listener: socket.socket | None = None
        self._open: list[socket.socket] = []

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
        for sock in [self._listener, *self._open]:
            try:
                sock.close()
            except OSError:
                pass
        try:
            os.unlink(self.path)
        except OSError:
            pass

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
            threading.Thread(
                target=self._requests, args=(conn, client, upstream, pending), daemon=True
            ).start()
            threading.Thread(
                target=self._answers, args=(conn, upstream, client, pending), daemon=True
            ).start()

    def _requests(self, conn: int, client, upstream, pending: dict) -> None:
        for line in _lines(client):
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

    def _answers(self, conn: int, upstream, client, pending: dict) -> None:
        for line in _lines(upstream):
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
        _shutdown(client, socket.SHUT_RDWR)

    def _rewrite_identify(self, result: dict) -> dict:
        result = dict(result)
        if self.forced_poll:
            result["ops"] = [op for op in result.get("ops", []) if op != "events.subscribe"]
            result.pop("local_session_socket", None)
        elif self.session_socket is not None and result.get("local_session_socket"):
            result["local_session_socket"] = self.session_socket
        return result


def _lines(sock):
    buffered = b""
    while True:
        try:
            chunk = sock.recv(1 << 16)
        except OSError:
            return
        if not chunk:
            return
        buffered += chunk
        while b"\n" in buffered:
            line, buffered = buffered.split(b"\n", 1)
            yield line


def _shutdown(sock, how) -> None:
    try:
        sock.shutdown(how)
    except OSError:
        pass

