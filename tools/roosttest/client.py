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
import os
import socket
import time

# E2E timeouts are tuned for a fast dev box; shared CI runners (especially
# the macos-latest GUI session) are slower and variable. Scale every wait
# from one knob so CI can buy headroom without editing each call site.
# Default 1.0 = unchanged locally; CI sets ROOST_TEST_TIMEOUT_SCALE=3.
_TIMEOUT_SCALE = float(os.environ.get("ROOST_TEST_TIMEOUT_SCALE", "1.0"))


def scaled_timeout(timeout: float) -> float:
    return timeout * _TIMEOUT_SCALE


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
                 cols: int = 80, rows: int = 24,
                 argv: list[str] | None = None) -> int:
        params = {"project_id": str(project_id), "cwd": cwd,
                  "title": title, "cols": cols, "rows": rows}
        if argv:
            params["argv"] = argv
        r = self.call("tab.open", params)
        return int(r["tab"]["id"])

    def close_tab(self, tab_id: int) -> None:
        self.call("tab.close", {"tab_id": str(tab_id)})

    def focus(self, tab_id: int) -> None:
        self.call("tab.focus", {"tab_id": str(tab_id)})

    def set_state(self, tab_id: int, state: str) -> None:
        self.call("tab.set_state", {"tab_id": str(tab_id), "state": state})

    def set_hook_active(self, tab_id: int, active: bool) -> None:
        self.call("tab.set_hook_active", {"tab_id": str(tab_id), "active": active})

    def set_title(self, tab_id: int, title: str) -> None:
        self.call("tab.set_title", {"tab_id": str(tab_id), "title": title})

    def resize(self, tab_id: int, cols: int, rows: int) -> None:
        self.call("tab.resize", {"tab_id": str(tab_id), "cols": cols, "rows": rows})

    def rename_project(self, project_id: int, name: str) -> None:
        self.call("project.rename", {"project_id": str(project_id), "name": name})

    def reorder_tabs(self, project_id: int, tab_ids: list[int]) -> None:
        self.call("tab.reorder", {"project_id": str(project_id),
                                  "tab_ids": [str(t) for t in tab_ids]})

    def project(self, project_id: int) -> dict | None:
        return next((p for p in self.list() if int(p["id"]) == project_id), None)

    def project_tab_ids(self, project_id: int) -> list[int]:
        p = self.project(project_id)
        return [int(t["id"]) for t in (p["tabs"] if p else [])]

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

    def window_metrics(self) -> dict:
        """{window_width, window_height, sidebar_width, sidebar_collapsed}
        in logical points. Backs the sidebar-holds-width regression."""
        return self.call("app.window_metrics", {})

    def window_resize(self, width: float, height: float) -> None:
        """Test-mode only — set the window's logical size."""
        self.call("window.resize", {"width": float(width), "height": float(height)})

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

    def palette_present(self, items: list[dict], title: str = "", placeholder: str = "") -> dict:
        """Present a caller-supplied list and BLOCK until the user picks a
        row or dismisses. Returns ``{"selected_id"?, "dismissed"}``. Because
        it blocks, drive the selection from a *second* connection (or a
        thread) — see test_provider.py."""
        return self.call(
            "palette.present", {"title": title, "placeholder": placeholder, "items": items}
        )

    # -- selection + clipboard (test ops) --------------------------------
    # Mirror the user-driven drag flow (`selection.set` ~ mouseDown +
    # drag), reading back via `selection.dump`. The host clipboard is
    # accessible via `clipboard.dump` ("system" = ⌘V target / CLIPBOARD;
    # "selection" = the per-app selection pasteboard on Mac / PRIMARY on
    # Linux). `clipboard.write` seeds a known value for tests that need
    # to assert paste behavior.

    def selection_set(
        self,
        tab_id: int,
        anchor: tuple[int, int],
        cursor: tuple[int, int],
    ) -> None:
        """Anchor a selection at viewport (col, row) `anchor` and extend
        to viewport (col, row) `cursor`. Raises `RoostError('not-found')`
        if the tab has no live terminal."""
        self.call("selection.set", {
            "tab_id": str(tab_id),
            "anchor": {"col": anchor[0], "row": anchor[1]},
            "cursor": {"col": cursor[0], "row": cursor[1]},
        })

    def selection_clear(self, tab_id: int) -> None:
        self.call("selection.clear", {"tab_id": str(tab_id)})

    def selection_dump(self, tab_id: int) -> dict:
        """Return `{text: str|None, anchor_visible: bool, cursor_visible: bool}`."""
        return self.call("selection.dump", {"tab_id": str(tab_id)})

    def clipboard_dump(self, target: str = "system") -> str | None:
        return self.call("clipboard.dump", {"target": target}).get("text")

    def clipboard_write(self, target: str, text: str) -> None:
        self.call("clipboard.write", {"target": target, "text": text})

    # -- test-only PTY drain ops (ROOST_TEST_MODE=1) ---------------------
    # `tab.feed_pty_bytes` injects bytes into a tab's PTY-output drain;
    # `tab.capture_pty_input` reads the bytes the UI has queued back
    # onto the PTY input channel (keystrokes, paste, OSC reply replies).
    # Both require ROOST_TEST_MODE=1 at UI launch — without it the
    # server returns `not-enabled` (RoostError, code "not-enabled").
    # The companion `tab.dump_resolved` is ungated (richer read of the
    # same render state `tab.dump` exposes).
    #
    # Together these let the harness exercise OSC drains, reply
    # round-trips, and other byte-level wiring end-to-end — the
    # missing rung between Rust/Swift unit tests and the user-driven
    # IO injectors under `tools/input/`. See
    # `docs/development/test-automation.md` §5.4 for the full rationale.

    def tab_feed_pty_bytes(self, tab_id: int, data: bytes) -> None:
        """Inject raw bytes into a tab's PTY-output drain as if the
        supervisor had emitted them. Indistinguishable from real PTY
        output to the OSC scanner + libghostty. Raises
        `RoostError('not-enabled')` when ROOST_TEST_MODE=1 was not set
        at UI launch; `RoostError('not-found')` for unknown tab id."""
        self.call("tab.feed_pty_bytes", {
            "tab_id": str(tab_id),
            "data": base64.b64encode(data).decode("ascii"),
        })

    def tab_capture_pty_input(self, tab_id: int, drain: bool = True) -> bytes:
        """Return (and by default drain) the bytes the UI has queued
        onto this tab's PTY-input channel since the last drain.
        `drain=True` is the typical test pattern: feed → capture →
        assert; the second call will return empty. Same gating +
        error shape as `tab_feed_pty_bytes`."""
        res = self.call("tab.capture_pty_input", {
            "tab_id": str(tab_id),
            "drain": drain,
        })
        return base64.b64decode(res.get("data", "") or "")

    def tab_dump_resolved(self, tab_id: int) -> dict:
        """Snapshot the live viewport AFTER the production color
        resolver has run (including the theme's bold-color override).
        Returns `{cols, rows, cells: [{row, col, text, fg, bg,
        has_explicit_bg, bold, italic, inverse}, ...]}` with fg/bg as
        `#RRGGBB` hex strings. Ungated — safe outside test mode."""
        return self.call("tab.dump_resolved", {"tab_id": str(tab_id)})

    def tab_dispatch_mouse_event(
        self,
        tab_id: int,
        kind: str,
        button: str,
        cell_x: int,
        cell_y: int,
        mods: int = 0,
    ) -> None:
        """Drive a synthetic mouse event into the UI's mouse handler at
        cell-grid coordinates. Same `routeMouseEvent` path the real
        NSEvent / GestureClick takes, so the negotiated mouse-tracking
        mode + encoder format are honored exactly.

        `kind` ∈ {"press","release","motion"}; `button` ∈
        {"left","right","middle","wheel_up","wheel_down","none"}
        (use "none" for motion-no-button events under mode 1003).
        Gated by ROOST_TEST_MODE=1; raises `RoostError('not-enabled')`
        when the gate is off, `not-found` for an unknown tab id."""
        self.call("tab.dispatch_mouse_event", {
            "tab_id": str(tab_id),
            "kind": kind,
            "button": button,
            "cell_x": cell_x,
            "cell_y": cell_y,
            "mods": mods,
        })

    def app_set_window_focus(self, focus: bool) -> None:
        """Drive the focus-tracking emit path without taking real OS
        focus. Mode 1004 → `\\x1b[I` / `\\x1b[O` lands on the active
        tab's input channel. Gated by ROOST_TEST_MODE=1."""
        self.call("app.set_window_focus", {"focus": focus})

    def app_cursor_shape(self) -> str:
        """Return the active tab's currently-applied W3C cursor name
        (canonical: empty body and `"default"` both return
        `"default"`). Ungated."""
        res = self.call("app.cursor_shape", {})
        return res.get("shape", "")

    def app_active_terminal_focused(self) -> bool:
        """Return whether the active tab's terminal holds GTK *logical*
        keyboard focus (`window.focus_widget() == terminal`). Reads
        logical focus, so it is observable under the WM-less Xvfb e2e
        runner. Ungated (read-only); False when there is no active
        terminal."""
        res = self.call("app.active_terminal_focused", {})
        # Direct key access (not .get with a default): a missing field is
        # a protocol violation that should surface, not silently read as
        # unfocused. The op always sends a JSON bool.
        return bool(res["focused"])

    def app_selected_tab_id(self) -> int:
        """Return the tab id selected in the active project's AdwTabView —
        the on-screen tab (UI truth), independent of the core's active tab.
        Lets tests assert the UI selection and `identify().active_tab_id`
        agree. Ungated (read-only); 0 when there's no selection."""
        res = self.call("app.selected_tab_id", {})
        return int(res["tab_id"])

    def tab_expand_selection_at(
        self,
        tab_id: int,
        col: int,
        row: int,
        click_count: int,
    ) -> dict:
        """Drive the production double-/triple-click word/line
        expansion at `(col, row)` and commit the resulting span as the
        tab's selection. `click_count` must be >= 2 (the wire layer
        rejects anything smaller with `invalid-param`).

        Returns `{col0, col1, text}` mirroring the wire result. Both
        col fields are inclusive cell indices; `text` is the selected
        substring or `None` for a single-cell span that the renderer
        reports as empty. Gated like `tab.feed_pty_bytes`."""
        return self.call("tab.expand_selection_at", {
            "tab_id": str(tab_id),
            "col": col,
            "row": row,
            "click_count": click_count,
        })

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
        # Every wait_*/run helper funnels through here, so scaling the
        # budget once covers them all (see scaled_timeout / ROOST_TEST_TIMEOUT_SCALE).
        eff = scaled_timeout(timeout)
        deadline = time.monotonic() + eff
        while True:
            if pred():
                return
            if time.monotonic() >= deadline:
                raise Timeout(f"timed out after {eff}s waiting for {what}")
            time.sleep(interval)
