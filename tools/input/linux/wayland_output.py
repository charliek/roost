"""The current mode of a compositor's one output, read over the Wayland wire
protocol with the standard library alone: CI's cage runner has no
`wayland-info`, and the extent a uinput absolute device spans is the
output's, which no Roost op reports."""

from __future__ import annotations

import socket
import struct
from pathlib import Path

_DISPLAY = 1
_REGISTRY = 2


def _message(obj: int, opcode: int, payload: bytes) -> bytes:
    return struct.pack("=II", obj, (8 + len(payload)) << 16 | opcode) + payload


def _string(value: str) -> bytes:
    raw = value.encode() + b"\0"
    return struct.pack("=I", len(raw)) + raw + b"\0" * (-len(raw) % 4)


class _Wire:
    def __init__(self, path: Path) -> None:
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(5)
        self.sock.connect(str(path))
        self.buffer = b""
        self.next_id = _REGISTRY

    def send(self, obj: int, opcode: int, payload: bytes = b"") -> None:
        self.sock.sendall(_message(obj, opcode, payload))

    def new_id(self) -> int:
        self.next_id += 1
        return self.next_id

    def events_until_sync(self):
        """Every event the compositor sends before a fresh `wl_display.sync`
        answers, as (object, opcode, payload)."""
        callback = self.new_id()
        self.send(_DISPLAY, 0, struct.pack("=I", callback))
        while True:
            while len(self.buffer) < 8:
                chunk = self.sock.recv(4096)
                if not chunk:
                    raise ConnectionError("the compositor closed the connection")
                self.buffer += chunk
            obj, word = struct.unpack_from("=II", self.buffer)
            size, opcode = word >> 16, word & 0xFFFF
            while len(self.buffer) < size:
                self.buffer += self.sock.recv(4096)
            payload, self.buffer = self.buffer[8:size], self.buffer[size:]
            if obj == _DISPLAY and opcode == 0:
                _, code, length = struct.unpack_from("=III", payload)
                message = payload[12 : 12 + length - 1].decode(errors="replace")
                raise RuntimeError(f"wl_display error {code}: {message}")
            if obj == callback:
                return
            yield obj, opcode, payload


def _socket_in(runtime_dir: Path) -> Path:
    sockets = [p for p in runtime_dir.glob("wayland-*") if not p.name.endswith(".lock")]
    if len(sockets) != 1:
        raise RuntimeError(f"expected one Wayland socket in {runtime_dir}, found {sockets}")
    return sockets[0]


def current_mode(runtime_dir: Path) -> tuple[int, int]:
    """(width, height) in pixels of the current mode of the single output
    the compositor listening in `runtime_dir` advertises."""
    wire = _Wire(_socket_in(runtime_dir))
    try:
        wire.send(_DISPLAY, 1, struct.pack("=I", _REGISTRY))
        outputs = []
        for obj, opcode, payload in wire.events_until_sync():
            if obj == _REGISTRY and opcode == 0:
                (name, length) = struct.unpack_from("=II", payload)
                interface = payload[8 : 8 + length - 1].decode()
                if interface == "wl_output":
                    outputs.append(name)
        if len(outputs) != 1:
            raise RuntimeError(f"expected one wl_output, the compositor lists {len(outputs)}")
        output = wire.new_id()
        wire.send(
            _REGISTRY,
            0,
            struct.pack("=I", outputs[0]) + _string("wl_output") + struct.pack("=II", 1, output),
        )
        modes = [
            struct.unpack_from("=Iii", payload)
            for obj, opcode, payload in wire.events_until_sync()
            if obj == output and opcode == 1
        ]
    finally:
        wire.sock.close()
    current = [(width, height) for flags, width, height in modes if flags & 0x1]
    if len(current) != 1:
        raise RuntimeError(f"wl_output reported no single current mode: {modes}")
    return current[0]
