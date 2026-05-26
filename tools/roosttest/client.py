"""Thin JSON-IPC client for a running Roost UI (Mac or GTK).

Speaks the newline-delimited JSON protocol directly over the Unix socket
— the same contract `roostctl` uses (see docs/reference/ipc.md). Tests
drive the app through this and assert by reading back via `tab.list` /
`tab.dump`, so they exercise exactly the op set users drive (the north
star). No `roostctl` subprocess on the hot path; ids are string-wrapped
int64 on the wire and surfaced as plain ints here.
"""

from __future__ import annotations

import base64
import json
import socket
import time


class RoostError(Exception):
    """A server error envelope (`ok: false`) or a transport failure."""

    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class Timeout(RoostError):
    def __init__(self, message: str):
        super().__init__("timeout", message)


class Roost:
    def __init__(self, socket_path: str):
        self.path = str(socket_path)
        self._next_id = 0
        self._buf = b""
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.connect(self.path)

    # -- lifecycle --------------------------------------------------------
    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass

    def __enter__(self) -> "Roost":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    # -- transport --------------------------------------------------------
    def call(self, op: str, params: dict | None = None) -> dict:
        """Send one request, return its `result` dict, raise on error."""
        self._next_id += 1
        req = {"id": str(self._next_id), "op": op, "params": params or {}}
        self._sock.sendall((json.dumps(req) + "\n").encode())
        resp = json.loads(self._readline())
        if not resp.get("ok"):
            err = resp.get("error") or {}
            raise RoostError(err.get("code", "unknown"), err.get("message", ""))
        return resp.get("result") or {}

    def _readline(self) -> str:
        while b"\n" not in self._buf:
            chunk = self._sock.recv(1 << 16)
            if not chunk:
                raise RoostError("disconnected", "socket closed mid-response")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        return line.decode()

    # -- ops --------------------------------------------------------------
    def identify(self) -> dict:
        r = self.call("identify")
        return {**r, "active_project_id": int(r["active_project_id"]),
                "active_tab_id": int(r["active_tab_id"])}

    def list(self) -> list[dict]:
        """Projects (each with its `tabs`). There is no `project.list`
        op — `tab.list` is the workspace snapshot."""
        return self.call("tab.list")["projects"]

    def tabs(self) -> list[dict]:
        return [t for p in self.list() for t in p["tabs"]]

    def tab(self, tab_id: int) -> dict | None:
        return next((t for t in self.tabs() if int(t["id"]) == tab_id), None)

    def create_project(self, name: str = "", cwd: str = "") -> int:
        return int(self.call("project.create", {"name": name, "cwd": cwd})["project"]["id"])

    def delete_project(self, project_id: int) -> None:
        self.call("project.delete", {"project_id": str(project_id)})

    def open_tab(self, project_id: int, cwd: str = "", title: str = "",
                 cols: int = 80, rows: int = 24) -> int:
        r = self.call("tab.open", {"project_id": str(project_id), "cwd": cwd,
                                    "title": title, "cols": cols, "rows": rows})
        return int(r["tab"]["id"])

    def close_tab(self, tab_id: int) -> None:
        self.call("tab.close", {"tab_id": str(tab_id)})

    def focus(self, tab_id: int) -> None:
        self.call("tab.focus", {"tab_id": str(tab_id)})

    def set_state(self, tab_id: int, state: str) -> None:
        self.call("tab.set_state", {"tab_id": str(tab_id), "state": state})

    def set_title(self, tab_id: int, title: str) -> None:
        self.call("tab.set_title", {"tab_id": str(tab_id), "title": title})

    def notify(self, tab_id: int, title: str, body: str = "") -> None:
        self.call("notification.create", {"tab_id": str(tab_id), "title": title, "body": body})

    def clear_notification(self, tab_id: int) -> None:
        self.call("tab.clear_notification", {"tab_id": str(tab_id)})

    def send(self, tab_id: int, data: bytes | str) -> None:
        if isinstance(data, str):
            data = data.encode()
        self.call("tab.write", {"tab_id": str(tab_id),
                                "data": base64.b64encode(data).decode()})

    def run(self, tab_id: int, command: str, ready_timeout: float = 5.0) -> None:
        """Wait for the shell prompt, then send `command` + Enter.

        Sending immediately after `open_tab` races shell startup — bytes
        written before the first prompt get eaten by the shell's line
        editor. Waiting until the viewport is non-empty (a prompt has
        been drawn) makes content tests deterministic without a sleep.
        """
        self._wait(lambda: self._safe_dump_text(tab_id).strip() != "",
                   ready_timeout, f"tab {tab_id} shell prompt")
        self.send(tab_id, command + "\n")

    def dump(self, tab_id: int) -> dict:
        """Terminal viewport as text: {cols, rows, cursor?, rows_text}."""
        return self.call("tab.dump", {"tab_id": str(tab_id)})

    def dump_text(self, tab_id: int) -> str:
        return "\n".join(self.dump(tab_id)["rows_text"])

    def screenshot(self, scale: int = 1) -> tuple[bytes, int, int]:
        r = self.call("app.screenshot", {"scale": scale})
        return base64.b64decode(r["png"]), r["width"], r["height"]

    # -- command palette --------------------------------------------------
    # Each op returns the resulting palette state:
    #   {open: bool, frame?: str, query: str, selection: int,
    #    items: [{id, title, subtitle?}]}
    # Activating a row dispatches the same command its keybind would, so
    # these drive command dispatch, not just the overlay.
    def palette_open(self, kind: str = "commands") -> dict:
        return self.call("palette.open", {"kind": kind})

    def palette_state(self) -> dict:
        return self.call("palette.state")

    def palette_query(self, query: str) -> dict:
        return self.call("palette.query", {"query": query})

    def palette_activate(self, item_id: str) -> dict:
        """Confirm the row with `item_id`. Raises RoostError('not-found')
        if no palette is open or no visible row matches."""
        return self.call("palette.activate", {"id": item_id})

    def palette_dismiss(self) -> dict:
        return self.call("palette.dismiss")

    @staticmethod
    def palette_item_ids(state: dict) -> list[str]:
        return [it["id"] for it in state.get("items", [])]

    # -- waits (poll the op set; no sleeps in tests) ----------------------
    def wait_state(self, tab_id: int, state: str, timeout: float = 5.0) -> None:
        self._wait(lambda: (self.tab(tab_id) or {}).get("state") == state,
                   timeout, f"tab {tab_id} state == {state!r}")

    def wait_text(self, tab_id: int, needle: str, timeout: float = 5.0) -> None:
        self._wait(lambda: needle in self._safe_dump_text(tab_id),
                   timeout, f"tab {tab_id} viewport contains {needle!r}")

    def wait_gone(self, tab_id: int, timeout: float = 5.0) -> None:
        self._wait(lambda: self.tab(tab_id) is None, timeout, f"tab {tab_id} closed")

    def _safe_dump_text(self, tab_id: int) -> str:
        try:
            return self.dump_text(tab_id)
        except RoostError as e:
            if e.code == "not-found":  # tab not live yet / closed mid-poll
                return ""
            raise

    @staticmethod
    def _wait(pred, timeout: float, what: str, interval: float = 0.1) -> None:
        deadline = time.monotonic() + timeout
        while True:
            if pred():
                return
            if time.monotonic() >= deadline:
                raise Timeout(f"timed out after {timeout}s waiting for {what}")
            time.sleep(interval)
