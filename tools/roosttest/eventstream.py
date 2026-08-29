"""Read a host session's server-push event stream.

`client.IpcClient`'s request path is strictly one-request-one-response,
and `events.subscribe` breaks that shape on purpose: the ack is the last
request/response frame on the connection, after which the server writes
`EventBatch` frames and never reads again
(`roost-ipc/src/server.rs::serve_push`). Teaching the request client to
also be a stream reader would put an event-shaped frame in the way of
every ordinary `call`, so this is its own connection and its own module.

One batch per workspace commit, empty commits included, so a gap in
`revision` always means loss — [`EventStream.expect_contiguous`] is the
client half of that contract.
"""

from __future__ import annotations

import json
import socket
import time

from client import RoostError, scaled_timeout


class EventStream:
    """One subscribed connection. Open it, [`subscribe`], then
    [`recv_frame`] until the test has what it needs."""

    def __init__(self, socket_path, timeout: float = 15.0):
        self.path = str(socket_path)
        self._buf = b""
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.settimeout(scaled_timeout(timeout))
        self._sock.connect(self.path)
        self.revision: int | None = None

    # -- lifecycle --------------------------------------------------------
    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass

    def __enter__(self) -> "EventStream":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    # -- protocol ---------------------------------------------------------
    def subscribe(self, tab_id_filter: int = 0) -> int:
        """Send `events.subscribe` and return the ack's fence revision.

        The client already has everything at or below this revision (that
        is what a `tab.list` taken at the same moment means), so the first
        batch it should see is `revision + 1`.
        """
        request = {
            "id": "1",
            "op": "events.subscribe",
            "params": {"tab_id_filter": str(tab_id_filter)},
        }
        self._sock.sendall((json.dumps(request) + "\n").encode())
        ack = json.loads(self._readline())
        if not ack.get("ok"):
            err = ack.get("error") or {}
            raise RoostError(err.get("code", "unknown"), err.get("message", ""))
        self.revision = int((ack.get("result") or {})["revision"])
        return self.revision

    def recv_frame(self, timeout: float = 10.0) -> dict:
        """One pushed `EventBatch` — `{"revision": int, "events": [...]}`.

        Raises `TimeoutError` when nothing arrives inside the (scaled)
        budget and `RoostError("disconnected", ...)` when the server
        closed the stream, which is itself a documented signal: a close
        is how the session says "resync" (`event_push.rs`).
        """
        return self._recv_within(scaled_timeout(timeout))

    def recv_until(
        self, event: str, timeout: float = 10.0, max_batches: int = 512
    ) -> tuple[list[dict], dict]:
        """Read batches until one carries `event`.

        Returns `(batches, envelope)` — every batch consumed, plus the
        matching event envelope — so a caller can assert on the fence
        discipline (see [`expect_contiguous`]) as well as the payload.

        `timeout` bounds the WHOLE call, not each frame: a session that
        keeps committing while never producing `event` would otherwise
        refresh the per-frame budget forever and grow `batches` without
        limit. `max_batches` is the second bound, for the case where the
        frames arrive fast enough to fill memory inside one deadline —
        hitting either is a failure, not something to keep waiting on.
        """
        deadline = time.monotonic() + scaled_timeout(timeout)
        batches: list[dict] = []
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"never saw {event!r} on {self.path} within its budget "
                    f"(read {len(batches)} batches)"
                )
            batches.append(self._recv_within(remaining))
            if len(batches) > max_batches:
                raise AssertionError(
                    f"read {len(batches)} batches from {self.path} without seeing "
                    f"{event!r}; the stream is producing events this test never expects"
                )
            for envelope in batches[-1].get("events", []):
                if envelope.get("event") == event:
                    return batches, envelope

    def expect_contiguous(self, batches: list[dict], start: int) -> None:
        """Assert the batches run `start+1, start+2, …` with no holes.

        A hole is the only loss signal the protocol offers; skipping the
        check here would make this reader unable to notice the very thing
        the empty-commit batches exist to make visible.
        """
        want = start + 1
        for batch in batches:
            got = int(batch["revision"])
            if got != want:
                raise AssertionError(
                    f"event stream skipped a revision: expected {want}, got {got} "
                    f"(batches={[b['revision'] for b in batches]})"
                )
            want += 1

    # -- transport --------------------------------------------------------
    def _recv_within(self, seconds: float) -> dict:
        """Read one frame under an ALREADY-SCALED budget. The scaling
        happens once, at the public entry point, so a caller spending a
        shared deadline down doesn't re-scale what it has left."""
        self._sock.settimeout(seconds)
        return json.loads(self._readline())

    def _readline(self) -> str:
        while b"\n" not in self._buf:
            try:
                chunk = self._sock.recv(1 << 16)
            except TimeoutError as error:
                raise TimeoutError(f"no push frame from {self.path} within its budget") from error
            if not chunk:
                raise RoostError("disconnected", "the session closed the event stream")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        return line.decode()
