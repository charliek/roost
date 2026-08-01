#!/usr/bin/env python3
"""Real-seat Wayland clipboard proof for the Iced UI.

Runs Iced fullscreen under cage with headless + libinput backends, then uses
the repository's /dev/uinput pointer and keyboard injectors. The hard gates
avoid programmatic Wayland clipboard seeding (which lacks an input serial):

* select through IPC, inject the configured Copy chord, then inject Paste and
  require the copied bytes on the initiating PTY;
* perform a real pointer drag with copy-on-select=clipboard, inject Paste, and
  require the dragged bytes on the PTY;
* drive native double/triple clicks and a combined Alt+pointer URL hover.

Those prove ordinary system clipboard ownership/read under real keyboard and
pointer serials. PRIMARY/middle-click remains compositor-protocol-dependent and
is covered as a required X11 path by iced_clipboard_check.py.
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

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.path.insert(0, str(REPO / "tools" / "roosttest"))

ICED_BIN = Path(
    os.environ.get("ROOST_ICED_BIN") or REPO / "target" / "debug" / "roost-iced"
)
INJECT_KEY = HERE / "inject_key.py"
INJECT_POINTER = HERE / "inject_pointer.py"
SCALE = float(os.environ.get("ROOST_TEST_TIMEOUT_SCALE", "1") or "1")


def _skip(message: str) -> NoReturn:
    if os.environ.get("ROOST_REQUIRE_REAL_INPUT") == "1":
        raise SystemExit(f"FAIL (Iced Wayland input required): {message}")
    raise SystemExit(f"SKIP: {message}")


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


def _inject_key(*names: str) -> None:
    subprocess.run([sys.executable, str(INJECT_KEY), *names], check=True)
    time.sleep(0.3)


def _inject_drag(width: int, height: int, x0: int, y: int, x1: int) -> None:
    operations = [f"move {x0} {y}", "down LEFT"]
    for step in range(1, 9):
        x = int(x0 + (x1 - x0) * step / 8)
        operations.append(f"move {x} {y}")
    operations.append("up LEFT")
    subprocess.run(
        [
            sys.executable,
            str(INJECT_POINTER),
            str(width),
            str(height),
            *operations,
        ],
        check=True,
    )
    time.sleep(0.5)


def _inject_clicks(width: int, height: int, x: int, y: int, count: int) -> None:
    operations = [f"move {x} {y}"]
    for _ in range(count):
        operations.extend(["down LEFT", "up LEFT", "sleep 80"])
    subprocess.run(
        [
            sys.executable,
            str(INJECT_POINTER),
            str(width),
            str(height),
            *operations,
        ],
        check=True,
    )
    time.sleep(0.3)


def _inject_link_hover(client, width: int, height: int, x: int, y: int) -> None:
    process = subprocess.Popen(
        [
            sys.executable,
            str(INJECT_POINTER),
            str(width),
            str(height),
            "keydown ALT",
            f"move {x} {y}",
            "sleep 1000",
            f"move 20 {y}",
            "sleep 200",
            "keyup ALT",
        ],
    )
    _wait_until(
        lambda: client.app_cursor_shape() == "pointer",
        "real-seat Wayland Alt-hover link pointer",
    )
    assert process.wait(timeout=5 * SCALE) == 0, "combined Wayland hover injector failed"
    time.sleep(0.3)


def _set_row(client, tab: int, text: str) -> None:
    client.tab_feed_pty_bytes(tab, b"\x1b[2J\x1b[H" + text.encode())
    _wait_until(
        lambda: client.dump(tab)["rows_text"][0].startswith(text),
        f"terminal row {text!r}",
    )


def _capture_contains(client, tab: int, expected: bytes) -> bool:
    return expected in client.tab_capture_pty_input(tab, drain=True)


def _wait_for_selection(client, tab: int, expected: str, description: str) -> None:
    observed: dict[str, object] = {}

    def matches() -> bool:
        nonlocal observed
        observed = client.selection_dump(tab)
        return observed.get("text") == expected

    try:
        _wait_until(matches, description)
    except AssertionError as error:
        raise AssertionError(
            f"{error}; expected {expected!r}, last selection {observed!r}"
        ) from error


def main() -> int:
    if not ICED_BIN.is_file():
        _skip(f"Iced binary not found: {ICED_BIN}")
    cage = shutil.which("cage")
    if cage is None:
        _skip("cage not installed")
    if not Path("/dev/uinput").exists():
        _skip("/dev/uinput is unavailable")

    root = Path(tempfile.mkdtemp(prefix="roost-iced-wayland-"))
    runtime = root / "runtime"
    data = root / "data"
    state = root / "state"
    for path in (runtime, data, state):
        path.mkdir(parents=True)
    runtime.chmod(0o700)
    config = root / "config.conf"
    config.write_text(
        "copy-on-select = clipboard\nkeybind = alt+shift+p = copy\n",
        encoding="utf-8",
    )
    socket_path = runtime / "roost-iced" / "roost.sock"
    cage_log = root / "cage.log"
    app_log = state / "roost-iced" / "roost.log"
    env = {
        **os.environ,
        "XDG_RUNTIME_DIR": str(runtime),
        "XDG_DATA_HOME": str(data),
        "XDG_STATE_HOME": str(state),
        "ROOST_STATE_DIR": str(data / "workspace"),
        "ROOST_CONFIG": str(config),
        "ROOST_BUNDLE_PROFILE": "iced",
        "ROOST_TEST_MODE": "1",
        "ICED_BACKEND": os.environ.get("ICED_BACKEND", "tiny-skia"),
        "WINIT_UNIX_BACKEND": "wayland",
        "WLR_BACKENDS": os.environ.get("WLR_BACKENDS", "headless,libinput"),
        "WLR_RENDERER": os.environ.get("WLR_RENDERER", "pixman"),
        "RUST_LOG": os.environ.get("RUST_LOG", "warn"),
    }
    env.pop("DISPLAY", None)
    env.pop("DBUS_SESSION_BUS_ADDRESS", None)

    process = None
    client = None
    try:
        with cage_log.open("wb") as log_handle:
            process = subprocess.Popen(
                [cage, "--", str(ICED_BIN)],
                cwd=REPO,
                env=env,
                stdout=log_handle,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        client = _connect(socket_path)
        time.sleep(2 * SCALE)
        assert process.poll() is None, "cage/Iced exited before input"
        tab = int(client.identify()["active_tab_id"])
        _wait_until(lambda: bool(client.dump(tab)), "live Iced terminal")
        metrics = client.window_metrics()
        logical_width = int(metrics.get("window_width") or 0)
        logical_height = int(metrics.get("window_height") or 0)
        sidebar = int(metrics.get("sidebar_width") or 220)
        if not logical_width or not logical_height:
            _skip(f"window_metrics returned invalid size: {metrics!r}")
        # The virtual absolute device is normalized against the compositor
        # output, not the client's logical/decorated window extent. Cage's
        # headless output can be wider than `window_metrics` (for example,
        # 2048 output pixels for a 1920-wide client); using the client extent
        # shifts every injected point proportionally. A scale-1 product
        # capture reports the renderer/output extent we must advertise to
        # uinput while the target coordinates remain logical layout points.
        _png, width, height = client.screenshot(scale=1)
        if not width or not height:
            _skip(f"screenshot returned invalid output size: {(width, height)!r}")

        explicit = f"wayland-copy-{uuid.uuid4().hex[:8]}"
        _set_row(client, tab, explicit)
        client.selection_set(tab, anchor=(0, 0), cursor=(len(explicit) - 1, 0))
        client.tab_capture_pty_input(tab, drain=True)
        _inject_key("ALT", "SHIFT", "P")
        _inject_key("ALT", "V")
        _wait_until(
            lambda: _capture_contains(client, tab, explicit.encode()),
            "real-seat Wayland explicit Copy-to-Paste system round trip",
        )

        dragged = f"wayland-drag-{uuid.uuid4().hex[:8]}"
        _set_row(client, tab, dragged)
        client.selection_clear(tab)
        client.tab_capture_pty_input(tab, drain=True)
        x0 = sidebar + 12 + 4
        x1 = sidebar + 12 + int((len(dragged) - 0.5) * 8.4)
        y = round(client.terminal_top(metrics)) + 12 + 9
        _inject_drag(width, height, x0, y, x1)
        _wait_for_selection(
            client, tab, dragged, "real-seat Wayland drag selection"
        )
        _inject_key("ALT", "V")
        _wait_until(
            lambda: _capture_contains(client, tab, dragged.encode()),
            "real-seat Wayland drag copy-on-select to system Paste round trip",
        )

        multi = "alpha/beta tail"
        _set_row(client, tab, multi)
        click_x = sidebar + 12 + int(2.5 * 8.4)
        _inject_clicks(width, height, click_x, y, 2)
        _wait_until(
            lambda: client.selection_dump(tab).get("text") == "alpha/beta",
            "real-seat Wayland native double-click",
        )
        time.sleep(0.7)
        _inject_clicks(width, height, click_x, y, 3)
        _wait_until(
            lambda: client.selection_dump(tab).get("text") == multi,
            "real-seat Wayland native triple-click",
        )

        url = "https://hover.test/path"
        _set_row(client, tab, url)
        client.tab_feed_pty_bytes(tab, b"\x1b]22;crosshair\x1b\\")
        _wait_until(
            lambda: client.app_cursor_shape() == "crosshair",
            "real-seat Wayland OSC cursor baseline",
        )
        hover_x = sidebar + 12 + int(8.5 * 8.4)
        _inject_link_hover(client, width, height, hover_x, y)
        _wait_until(
            lambda: client.app_cursor_shape() == "crosshair",
            "real-seat Wayland link hover restores OSC cursor",
        )
        assert process.poll() is None, "cage/Iced exited during clipboard checks"
    except Exception:
        for log_path in (cage_log, app_log):
            if log_path.exists():
                print(f"--- {log_path} ---", file=sys.stderr)
                print(log_path.read_text(errors="replace")[-8000:], file=sys.stderr)
        raise
    finally:
        if client is not None:
            client.close()
        if process is not None:
            try:
                os.killpg(os.getpgid(process.pid), signal.SIGTERM)
                process.wait(timeout=5)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                try:
                    os.killpg(os.getpgid(process.pid), signal.SIGKILL)
                    process.wait()
                except ProcessLookupError:
                    pass
        shutil.rmtree(root, ignore_errors=True)

    print(
        "PASS: Iced real-seat Wayland system clipboard — explicit Copy/Paste "
        "drag copy-on-select/Paste, native multi-click, and link hover"
    )
    print("ACCEPTED LIMITATION: cage does not advertise PRIMARY; middle-click is X11-gated")
    return 0


if __name__ == "__main__":
    sys.exit(main())
