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

Two things every subscriber has to know about, both HS-1b:

* the stream is lease-gated — `session.connect` first, its lease on the
  subscribe, or the server answers `connect-required`;
* every frame is a batch EXCEPT one: a terminal control envelope
  `{"event": "session.stopping", "data": {"reason": ...}}` that names why
  the stream is ending. It carries no `revision` and is exempt from the
  gap check, and the close that follows it is clean rather than a loss.
"""

from __future__ import annotations

import json
import socket
import time

from client import RoostError, scaled_timeout


STOPPING_EVENT = "session.stopping"


class EventStream:
    """One subscribed connection. Open it with the lease a
    `session.connect` handed out, [`subscribe`], then [`recv_frame`]
    until the test has what it needs."""

    def __init__(self, socket_path, lease: str = "", timeout: float = 15.0):
        self.path = str(socket_path)
        self.lease = lease
        self._buf = b""
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.settimeout(scaled_timeout(timeout))
        self._sock.connect(self.path)
        self.revision: int | None = None
        # Set when the terminal control envelope arrives; a close after
        # it is the session saying goodbye, not a dropped stream.
        self.stopping_reason: str | None = None

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
    def subscribe(self, tab_id_filter: int = 0, lease: str | None = None) -> int:
        """Send `events.subscribe` and return the ack's fence revision.

        The client already has everything at or below this revision (that
        is what a `tab.list` taken at the same moment means), so the first
        batch it should see is `revision + 1`.

        Raises `RoostError('connect-required')` without a live lease and
        `RoostError('taken-over')` with one another client has since
        taken.
        """
        request = {
            "id": "1",
            "op": "events.subscribe",
            "params": {
                "lease": self.lease if lease is None else lease,
                "tab_id_filter": str(tab_id_filter),
            },
        }
        self._sock.sendall((json.dumps(request) + "\n").encode())
        ack = json.loads(self._readline())
        if not ack.get("ok"):
            err = ack.get("error") or {}
            raise RoostError(err.get("code", "unknown"), err.get("message", ""))
        self.revision = int((ack.get("result") or {})["revision"])
        return self.revision

    def recv_frame(self, timeout: float = 10.0) -> dict:
        """One pushed frame: an `EventBatch` — `{"revision": int,
        "events": [...]}` — or the terminal `session.stopping` envelope.

        Raises `TimeoutError` when nothing arrives inside the (scaled)
        budget and `RoostError("disconnected", ...)` when the server
        closed the stream, which is itself a documented signal: a close
        is how the session says "resync" (`event_push.rs`).
        """
        return self._recv_within(scaled_timeout(timeout))

    def recv_stopping(self, timeout: float = 10.0) -> str:
        """Read to the close and return the reason the stream ended.

        The label is best-effort by contract (a peer that stopped reading
        makes the write impossible), but a session stopping a healthy
        connection must produce it, so its absence is a failure here.
        """
        deadline = time.monotonic() + scaled_timeout(timeout)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"the stream at {self.path} never ended within its budget")
            try:
                self._recv_within(remaining)
            except RoostError as error:
                if error.code != "disconnected":
                    raise
                if self.stopping_reason is None:
                    raise AssertionError(
                        f"the stream at {self.path} closed with no {STOPPING_EVENT} envelope"
                    ) from error
                return self.stopping_reason

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
            frame = self._recv_within(remaining)
            if "revision" not in frame:
                # The only non-batch frame is the terminal envelope, and
                # it means `event` is never coming.
                raise RoostError(
                    "disconnected",
                    f"the session stopped ({self.stopping_reason}) before {event!r} arrived",
                )
            batches.append(frame)
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
            if "revision" not in batch:
                # The terminal control envelope is not a batch and does
                # not participate in the revision sequence.
                continue
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
        frame = json.loads(self._readline())
        if frame.get("event") == STOPPING_EVENT:
            self.stopping_reason = (frame.get("data") or {}).get("reason", "")
        return frame

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
