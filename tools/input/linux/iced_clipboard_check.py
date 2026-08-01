#!/usr/bin/env python3
"""Real-key / real-pointer clipboard regressions for the Iced Linux UI.

The IPC suite proves native clipboard tasks, selection geometry, and PTY
capture independently. This check closes the user-input gap by driving Iced
through XTEST under its own Xvfb server. Two isolated launches prevent
copy-on-select from making the explicit-Copy assertion pass accidentally:

* copy-on-select=off: a configured Copy replacement, plain Paste, and
  bracketed Paste;
* copy-on-select=clipboard: a real terminal drag publishes selection text to
  CLIPBOARD + PRIMARY, ordinary Paste reads CLIPBOARD, and middle-click reads
  PRIMARY;
* native double/triple clicks expand a word/line, and Alt-hover composes the
  link pointer over an OSC 22 cursor without launching a browser.

Set ROOST_REQUIRE_REAL_INPUT=1 in CI/shed so a missing dependency is a failure.
ROOST_ICED_BIN and ROOSTCTL may point at shed-local Linux artifacts.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Callable, NoReturn

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "tools" / "roosttest"))

ICED_BIN = Path(
    os.environ.get("ROOST_ICED_BIN") or REPO / "target" / "debug" / "roost-iced"
)
SCALE = float(os.environ.get("ROOST_TEST_TIMEOUT_SCALE", "1") or "1")


def _skip(message: str) -> NoReturn:
    if os.environ.get("ROOST_REQUIRE_REAL_INPUT") == "1":
        raise SystemExit(f"FAIL (Iced real input required): {message}")
    raise SystemExit(f"SKIP: {message}")


def _free_display() -> str:
    for number in range(130, 160):
        if not Path(f"/tmp/.X{number}-lock").exists():
            return f":{number}"
    _skip("no free X display in :130..:159")


def _wait_until(
    predicate: Callable[[], bool], description: str, timeout: float = 10.0
) -> None:
    deadline = time.monotonic() + timeout * SCALE
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (ConnectionError, FileNotFoundError, OSError) as error:
            last_error = error
        time.sleep(0.1)
    suffix = f"; last error: {last_error}" if last_error else ""
    raise AssertionError(f"timed out waiting for {description}{suffix}")


def _connect(socket_path: Path):
    from client import Roost

    holder = []

    def connect() -> bool:
        client = None
        try:
            client = Roost(str(socket_path))
            client.identify()
            holder.append(client)
            return True
        except (ConnectionError, FileNotFoundError, OSError):
            if client is not None:
                client.close()
            return False

    _wait_until(connect, f"Iced socket {socket_path}", timeout=15)
    return holder[-1]


def _wait_window(display: str) -> str:
    env = {**os.environ, "DISPLAY": display}
    holder: list[str] = []

    def find() -> bool:
        result = subprocess.run(
            ["xdotool", "search", "--name", "Roost"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        windows = result.stdout.split()
        if windows:
            holder.append(windows[-1])
            return True
        return False

    _wait_until(find, "mapped Iced window", timeout=15)
    return holder[-1]


class Launch:
    def __init__(self, root: Path, display: str, copy_on_select: str, name: str):
        self.root = root / name
        self.runtime_dir = self.root / "runtime"
        self.data_dir = self.root / "data"
        self.state_dir = self.root / "state"
        self.config = self.root / "config.conf"
        for path in (self.runtime_dir, self.data_dir, self.state_dir):
            path.mkdir(parents=True)
        self.runtime_dir.chmod(0o700)
        self.config.write_text(
            "\n".join(
                [
                    f"copy-on-select = {copy_on_select}",
                    # Replace the former command-palette trigger. If the old
                    # hard-coded check runs before the effective table, the
                    # explicit-Copy assertion below fails.
                    "keybind = alt+shift+p = copy",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        self.socket = self.runtime_dir / "roost-iced" / "roost.sock"
        self.log = self.root / "roost-iced.log"
        env = {
            **os.environ,
            "DISPLAY": display,
            "XDG_RUNTIME_DIR": str(self.runtime_dir),
            "XDG_DATA_HOME": str(self.data_dir),
            "XDG_STATE_HOME": str(self.state_dir),
            "ROOST_STATE_DIR": str(self.data_dir / "workspace"),
            "ROOST_CONFIG": str(self.config),
            "ROOST_BUNDLE_PROFILE": "iced",
            "ROOST_TEST_MODE": "1",
            "RUST_LOG": os.environ.get("RUST_LOG", "warn"),
        }
        self.log_handle = self.log.open("wb")
        self.process = subprocess.Popen(
            [str(ICED_BIN)],
            cwd=REPO,
            env=env,
            stdout=self.log_handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        self.client = None
        try:
            self.client = _connect(self.socket)
            self.window = _wait_window(display)
            self.xenv = {**os.environ, "DISPLAY": display}
            subprocess.run(
                ["xdotool", "windowfocus", "--sync", self.window],
                env=self.xenv,
                check=False,
            )
            self.tab = int(self.client.identify()["active_tab_id"])
            _wait_until(lambda: bool(self.client.dump(self.tab)), "live Iced terminal")
        except Exception:
            self.close()
            raise

    def key(self, combo: str) -> None:
        subprocess.run(
            ["xdotool", "windowfocus", "--sync", self.window],
            env=self.xenv,
            check=False,
        )
        subprocess.run(
            ["xdotool", "key", "--clearmodifiers", combo],
            env=self.xenv,
            check=True,
        )
        time.sleep(0.15)

    def terminal_pointer(self, commands: list[str]) -> None:
        subprocess.run(
            ["xdotool", *commands], env=self.xenv, check=True
        )
        time.sleep(0.2)

    def close(self) -> None:
        try:
            if self.client is not None:
                self.client.close()
                self.client = None
        finally:
            try:
                os.killpg(os.getpgid(self.process.pid), signal.SIGTERM)
                self.process.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(self.process.pid), signal.SIGKILL)
                    self.process.wait()
                except ProcessLookupError:
                    pass
            self.log_handle.close()


def _set_row(launch: Launch, text: str) -> None:
    launch.client.tab_feed_pty_bytes(
        launch.tab, b"\x1b[2J\x1b[H" + text.encode("utf-8")
    )
    _wait_until(
        lambda: launch.client.dump(launch.tab)["rows_text"][0].startswith(text),
        f"terminal row {text!r}",
    )


def _capture_contains(launch: Launch, expected: bytes) -> bool:
    return expected in launch.client.tab_capture_pty_input(launch.tab, drain=True)


def _preserve_failure(launches: list[Launch]) -> None:
    log_dir = os.environ.get("ROOST_E2E_LOG_DIR")
    artifact_dir = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    if log_dir:
        destination = Path(log_dir)
        destination.mkdir(parents=True, exist_ok=True)
        for launch in launches:
            if launch.log.exists():
                shutil.copyfile(launch.log, destination / f"{launch.root.name}.log")
    if artifact_dir:
        destination = Path(artifact_dir)
        destination.mkdir(parents=True, exist_ok=True)
        for launch in launches:
            if launch.client is None:
                continue
            try:
                png, _, _ = launch.client.screenshot(scale=1)
                (destination / f"{launch.root.name}.png").write_bytes(png)
            except Exception as error:  # diagnostics must not mask the failure
                print(f"failed to capture {launch.root.name} screenshot: {error}", file=sys.stderr)


def _explicit_copy_and_paste(launch: Launch) -> None:
    marker = f"iced-copy-{uuid.uuid4().hex[:8]}"
    _set_row(launch, marker)
    launch.client.selection_set(
        launch.tab, anchor=(0, 0), cursor=(len(marker) - 1, 0)
    )
    baseline = f"baseline-{uuid.uuid4().hex[:8]}"
    launch.client.clipboard_write("system", baseline)
    assert launch.client.clipboard_dump("system") == baseline
    launch.key("alt+shift+p")
    _wait_until(
        lambda: launch.client.clipboard_dump("system") == marker,
        "configured explicit Copy write",
    )

    plain = f"plain-paste-{uuid.uuid4().hex[:8]}"
    launch.client.clipboard_write("system", plain)
    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    launch.key("alt+v")
    _wait_until(lambda: _capture_contains(launch, plain.encode()), "plain Paste PTY bytes")

    bracketed = f"bracketed-{uuid.uuid4().hex[:8]}"
    launch.client.tab_feed_pty_bytes(launch.tab, b"\x1b[?2004h")
    launch.client.clipboard_write("system", bracketed)
    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    launch.key("alt+v")
    expected = b"\x1b[200~" + bracketed.encode() + b"\x1b[201~"
    _wait_until(lambda: _capture_contains(launch, expected), "bracketed Paste PTY bytes")


def _drag_copy_and_middle_paste(launch: Launch) -> None:
    marker = f"drag-{uuid.uuid4().hex[:8]}"
    _set_row(launch, marker)
    baseline = f"baseline-{uuid.uuid4().hex[:8]}"
    launch.client.clipboard_write("system", baseline)
    launch.client.clipboard_write("selection", baseline)

    # Window-relative client coordinates: 220px sidebar + Canvas padding;
    # 44px tab band + Canvas padding. End on the last marker cell because
    # TerminalSelection's committed range is inclusive at pointer release.
    x0 = 220 + 12 + 4
    x1 = 220 + 12 + int((len(marker) - 0.5) * 8.4)
    y = 44 + 12 + 9
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(x0),
            str(y),
            "sleep",
            "0.15",
            "mousedown",
            "1",
            "sleep",
            "0.15",
            "mousemove",
            "--window",
            launch.window,
            str(x1),
            str(y),
            "sleep",
            "0.15",
            "mouseup",
            "1",
        ]
    )
    deadline = time.monotonic() + 5 * SCALE
    selection = {}
    while time.monotonic() < deadline:
        selection = launch.client.selection_dump(launch.tab)
        if selection.get("text") == marker:
            break
        time.sleep(0.1)
    else:
        raise AssertionError(
            f"real drag did not select {marker!r}; selection={selection!r}"
        )
    _wait_until(
        lambda: launch.client.clipboard_dump("system") == marker,
        "copy-on-select system write after real drag",
    )
    _wait_until(
        lambda: launch.client.clipboard_dump("selection") == marker,
        "copy-on-select PRIMARY write after real drag",
    )

    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    launch.key("alt+v")
    _wait_until(
        lambda: _capture_contains(launch, marker.encode()),
        "drag selection round-trip through system Paste",
    )

    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(x0),
            str(y),
            "click",
            "2",
        ]
    )
    _wait_until(
        lambda: _capture_contains(launch, marker.encode()),
        "middle-click PRIMARY Paste",
    )


def _multi_click_and_link_hover(launch: Launch) -> None:
    row = "alpha/beta tail"
    _set_row(launch, row)
    x = 220 + 12 + int(2.5 * 8.4)
    y = 44 + 12 + 9
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(x),
            str(y),
            "click",
            "--repeat",
            "2",
            "--delay",
            "100",
            "1",
        ]
    )
    _wait_until(
        lambda: launch.client.selection_dump(launch.tab).get("text") == "alpha/beta",
        "native double-click word selection",
    )
    _wait_until(
        lambda: launch.client.clipboard_dump("system") == "alpha/beta",
        "double-click copy-on-select system write",
    )

    time.sleep(0.7)
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(x),
            str(y),
            "click",
            "--repeat",
            "3",
            "--delay",
            "100",
            "1",
        ]
    )
    _wait_until(
        lambda: launch.client.selection_dump(launch.tab).get("text") == row,
        "native triple-click line selection",
    )

    url = "https://hover.test/path"
    _set_row(launch, url)
    launch.client.tab_feed_pty_bytes(launch.tab, b"\x1b]22;crosshair\x1b\\")
    _wait_until(
        lambda: launch.client.app_cursor_shape() == "crosshair",
        "OSC 22 baseline cursor",
    )
    url_x = 220 + 12 + int(8.5 * 8.4)
    launch.terminal_pointer(
        [
            "keydown",
            "alt",
            "mousemove",
            "--window",
            launch.window,
            str(url_x),
            str(y),
        ]
    )
    _wait_until(
        lambda: launch.client.app_cursor_shape() == "pointer",
        "Alt-hover link pointer over OSC shape",
    )
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            "20",
            str(y),
            "keyup",
            "alt",
        ]
    )
    _wait_until(
        lambda: launch.client.app_cursor_shape() == "crosshair",
        "OSC shape restored after terminal leave",
    )


def main() -> int:
    for tool in ("Xvfb", "xdotool"):
        if shutil.which(tool) is None:
            _skip(f"{tool} not installed")
    if not ICED_BIN.is_file():
        _skip(f"Iced binary not found: {ICED_BIN}")

    display = _free_display()
    root = Path(tempfile.mkdtemp(prefix="roost-iced-realinput-"))
    xvfb = subprocess.Popen(
        ["Xvfb", display, "-screen", "0", "1400x1000x24"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    launches: list[Launch] = []
    try:
        time.sleep(0.8)
        off = Launch(root, display, "off", "explicit")
        launches.append(off)
        _explicit_copy_and_paste(off)
        off.close()
        launches.pop()

        clipboard = Launch(root, display, "clipboard", "selection")
        launches.append(clipboard)
        _drag_copy_and_middle_paste(clipboard)
        _multi_click_and_link_hover(clipboard)
    except Exception:
        _preserve_failure(launches)
        for launch in launches:
            if launch.log.exists():
                print(f"--- {launch.log} ---", file=sys.stderr)
                print(launch.log.read_text(errors="replace")[-8000:], file=sys.stderr)
        raise
    finally:
        for launch in launches:
            launch.close()
        xvfb.terminate()
        try:
            xvfb.wait(timeout=5)
        except subprocess.TimeoutExpired:
            xvfb.kill()
            xvfb.wait()
        shutil.rmtree(root, ignore_errors=True)

    print(
        "PASS: configured explicit Copy, plain/bracketed Paste, real-drag "
        "copy-on-select, middle-click PRIMARY Paste, native multi-click, "
        "and link hover cursor composition"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
