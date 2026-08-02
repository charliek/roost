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
sys.path.insert(0, str(REPO / "tools" / "screenshot"))

import pngtool  # noqa: E402

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
    # Keep the virtual device alive for the complete gesture, but leave a
    # scheduling fence after positioning and pressing. Without these pauses,
    # the Wayland event loop can observe the button press with a later,
    # coalesced cursor position and start the selection several cells into the
    # fixture. X11 uses separate XTEST submissions plus an IPC fence for the
    # same reason; a single uinput device cannot be split across processes.
    operations = [f"move {x0} {y}", "sleep 300", "down LEFT", "sleep 300"]
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


def _measure_terminal_cell(client, tab: int, root: Path) -> tuple[int, int]:
    """Measure the live renderer grid through one explicit-background cell."""
    marker = (17, 201, 93)
    path = root / "terminal-cell-metrics.png"
    measured: list[tuple[int, int]] = []

    def capture() -> bool:
        client.tab_feed_pty_bytes(
            tab, b"\x1b[2J\x1b[H\x1b[48;2;17;201;93m \x1b[0m"
        )
        png, _width, _height = client.screenshot(scale=1)
        path.write_bytes(png)
        width, height, bpp, pixels = pngtool.load(str(path))
        metrics = client.window_metrics()
        x0 = round(float(metrics["sidebar_width"])) + 12
        y0 = round(client.terminal_top(metrics)) + 12

        def pixel(x: int, y: int) -> tuple[int, int, int]:
            offset = (y * width + x) * bpp
            return tuple(pixels[offset : offset + 3])

        if not (0 <= x0 < width and 0 <= y0 < height) or pixel(x0, y0) != marker:
            return False
        cell_width = 0
        while x0 + cell_width < width and pixel(x0 + cell_width, y0) == marker:
            cell_width += 1
        cell_height = 0
        while y0 + cell_height < height and pixel(x0, y0 + cell_height) == marker:
            cell_height += 1
        if cell_width <= 0 or cell_height <= 0:
            return False
        measured.append((cell_width, cell_height))
        return True

    _wait_until(capture, "live Iced terminal cell metrics")
    client.tab_feed_pty_bytes(tab, b"\x1b[0m\x1b[2J\x1b[H")
    return measured[-1]


def _active_pill_bounds(client, root: Path) -> tuple[int, int, int, int]:
    """Locate stable active-tab chrome in the in-product Wayland capture."""
    path = root / "wayland-active-tab.png"
    previous: tuple[int, int, int, int] | None = None
    for _attempt in range(12):
        png, _width, _height = client.screenshot(scale=1)
        path.write_bytes(png)
        width, height, bpp, pixels = pngtool.load(str(path))
        metrics = client.window_metrics()
        sidebar = round(float(metrics["sidebar_width"]))
        band = round(client.terminal_top(metrics))
        points: list[tuple[int, int]] = []
        for y in range(min(band, height)):
            for x in range(max(0, sidebar), width):
                offset = (y * width + x) * bpp
                if tuple(pixels[offset : offset + 3]) == (0x24, 0x37, 0x51):
                    points.append((x, y))
        if points:
            xs, ys = zip(*points)
            bounds = min(xs), min(ys), max(xs), max(ys)
            if bounds == previous:
                return bounds
            previous = bounds
        else:
            previous = None
        time.sleep(0.1)
    raise AssertionError(
        f"active Wayland tab never settled; last bounds {previous!r}; capture {path}"
    )


def _wayland_tab_reorder(client, root: Path, width: int, height: int) -> None:
    """Prove the Iced strip consumes a real compositor-seat drag."""
    identity = client.identify()
    project = int(identity["active_project_id"])
    home = int(identity["active_tab_id"])
    second = client.open_tab(project, title="WAYLAND-DRAG-SECOND")
    third = client.open_tab(project, title="WAYLAND-DRAG-THIRD")
    client.set_title(second, "WAYLAND-DRAG-SECOND")
    client.set_title(third, "WAYLAND-DRAG-THIRD")
    tabs = [home, second, third]
    for tab_id in tabs:
        _wait_until(lambda tab_id=tab_id: bool(client.dump(tab_id)), f"Wayland drag tab {tab_id}")
        client.tab_capture_pty_input(tab_id, drain=True)
    _wait_until(
        lambda: client.project_tab_ids(project) == tabs,
        "initial Wayland direct-drag order",
    )

    def drag(source: int, target_x: int, expected: list[int], label: str) -> None:
        client.focus(source)
        _wait_until(
            lambda: int(client.identify()["active_tab_id"]) == source,
            f"{label} source focus",
        )
        left, top, right, bottom = _active_pill_bounds(client, root)
        x0 = left + min(22, max(8, (right - left) // 3))
        _inject_drag(width, height, x0, (top + bottom) // 2, target_x)
        _wait_until(
            lambda: client.project_tab_ids(project) == expected,
            f"{label} authoritative order",
        )

    sidebar = round(float(client.window_metrics()["sidebar_width"]))
    drag(home, sidebar + 440, [second, third, home], "forward Wayland tab drag")
    drag(home, sidebar + 30, [home, second, third], "backward Wayland tab drag")
    for tab_id in tabs:
        assert client.tab_capture_pty_input(tab_id, drain=True) == b"", (
            tab_id,
            "tab drag leaked pointer bytes into PTY",
        )
    client.close_tab(second)
    client.close_tab(third)
    client.focus(home)
    _wait_until(
        lambda: client.project_tab_ids(project) == [home],
        "Wayland direct-drag fixture cleanup",
    )


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
        print(
            "Wayland geometry:",
            {
                "window": (logical_width, logical_height),
                "capture": (width, height),
                "sidebar": sidebar,
                "terminal_top": client.terminal_top(metrics),
            },
        )
        cell_width, cell_height = _measure_terminal_cell(client, tab, root)
        print("Wayland terminal cell:", (cell_width, cell_height))

        _wayland_tab_reorder(client, root, width, height)

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
        x0 = sidebar + 12 + cell_width // 2
        x1 = sidebar + 12 + int((len(dragged) - 0.5) * cell_width)
        y = round(client.terminal_top(metrics)) + 12 + cell_height // 2
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
        click_x = sidebar + 12 + int(2.5 * cell_width)
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
        hover_x = sidebar + 12 + int(8.5 * cell_width)
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
        "PASS: Iced real-seat Wayland — stable-ID tab drag in both directions, "
        "explicit Copy/Paste, drag copy-on-select/Paste, native multi-click, "
        "and link hover"
    )
    print("ACCEPTED LIMITATION: cage does not advertise PRIMARY; middle-click is X11-gated")
    return 0


if __name__ == "__main__":
    sys.exit(main())
