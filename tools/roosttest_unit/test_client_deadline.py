"""`Roost`'s whole-call deadline (`tools/roosttest/client.py`): a call that
runs past it closes the client, so a reply that arrives late is never read
as a later call's — the client matches no reply to its request's id.
"""

from __future__ import annotations

import json
import shutil
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "roosttest"))

import client  # noqa: E402

LATE = {"late": True}


class LateServer:
    """Sends the first `head` bytes of its first reply at once and the rest
    only once the test says the client gave up; then answers whatever
    request comes next on the same connection."""

    def __init__(self, test: unittest.TestCase, head: int = 0):
        root = Path(tempfile.mkdtemp(prefix="rt-client-", dir="/tmp"))
        test.addCleanup(shutil.rmtree, root, True)
        self.path = root / "s"
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(str(self.path))
        self._listener.listen(1)
        test.addCleanup(self._listener.close)
        self._head = head
        self.gave_up = threading.Event()
        self.late_sent = threading.Event()
        self.ops: list[str] = []
        test.addCleanup(self.gave_up.set)
        threading.Thread(target=self._serve, daemon=True).start()

    @staticmethod
    def _reply(request: dict, result: dict) -> bytes:
        return json.dumps({"id": request["id"], "ok": True, "result": result}).encode() + b"\n"

    def _serve(self) -> None:
        connection, _ = self._listener.accept()
        with connection:
            connection.settimeout(10)
            lines = connection.makefile("rb")
            first = json.loads(lines.readline())
            self.ops.append(first["op"])
            reply = self._reply(first, LATE)
            assert b"\n" not in reply[: self._head]
            connection.sendall(reply[: self._head])
            self.gave_up.wait(10)
            try:
                connection.sendall(reply[self._head :])
            except OSError:
                pass
            self.late_sent.set()
            line = lines.readline()
            if line:
                second = json.loads(line)
                self.ops.append(second["op"])
                connection.sendall(self._reply(second, {"fresh": True}))


class JumpingClock:
    """`time` as the client sees it: still for its first `still` readings,
    then far past any deadline."""

    def __init__(self, still: int):
        self._still = still
        self._readings = 0

    def monotonic(self) -> float:
        self._readings += 1
        return 0.0 if self._readings <= self._still else 1e9


def outcome(roost: client.Roost, op: str):
    try:
        return roost.call(op)
    except Exception as error:
        return error


class DeadlineTests(unittest.TestCase):
    def assert_late_reply_is_not_read(self, server: LateServer, roost: client.Roost) -> None:
        first = outcome(roost, "first")
        server.gave_up.set()
        self.assertTrue(server.late_sent.wait(10), "the server never sent its late reply")
        second = outcome(roost, "second")
        self.assertNotEqual(second, LATE, "the first call's late reply read as the second's answer")
        self.assertIsInstance(first, client.Timeout)
        self.assertIsInstance(second, OSError, "a client past its deadline is closed")
        self.assertEqual(server.ops, ["first"])

    def test_a_reply_after_a_silent_deadline_is_not_the_next_calls(self) -> None:
        """Nothing arrives before the deadline, so the receive itself times
        out on it."""
        server = LateServer(self)
        roost = client.Roost(server.path, timeout=5, deadline=0.2)
        self.addCleanup(roost.close)
        self.assert_late_reply_is_not_read(server, roost)

    def test_the_rest_of_a_reply_cut_off_by_the_deadline_is_not_the_next_calls(self) -> None:
        """Part of the reply arrives in time and the deadline passes before
        the rest: the buffered part must go too."""
        server = LateServer(self, head=10)
        roost = client.Roost(server.path, deadline=5)
        self.addCleanup(roost.close)
        # The call reads the clock to set its deadline and once before its
        # first receive; the reading after that receive is past it.
        with patch.object(client, "time", JumpingClock(still=2)):
            self.assert_late_reply_is_not_read(server, roost)


if __name__ == "__main__":
    unittest.main()
