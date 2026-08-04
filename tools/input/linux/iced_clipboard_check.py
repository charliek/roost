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
* physical wheel input reaches local history, snaps on the next terminal key,
  emits terminal mouse reports, and becomes arrows on an untracked alt screen.

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
sys.path.insert(0, str(REPO / "tools" / "screenshot"))

import pngtool  # noqa: E402
import parity  # noqa: E402
from client import RoostError  # noqa: E402

ICED_BIN = Path(
    os.environ.get("ROOST_ICED_BIN") or REPO / "target" / "debug" / "roost-iced"
)
SCALE = float(os.environ.get("ROOST_TEST_TIMEOUT_SCALE", "1") or "1")

# Sidebar project rows: chrome::ROW_HEIGHT, the sidebar body column's spacing,
# and its `padding([4, 0])` top inset (crates/roost-iced/src/app.rs). The band
# above them is read from the product instead of copied.
SIDEBAR_ROW_HEIGHT = 28
SIDEBAR_ROW_SPACING = 2
SIDEBAR_BODY_TOP_PADDING = 4


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


def _assert_stays(
    predicate: Callable[[], bool], description: str, duration: float = 0.75
) -> None:
    deadline = time.monotonic() + duration * SCALE
    while time.monotonic() < deadline:
        if not predicate():
            raise AssertionError(f"state changed while verifying {description}")
        time.sleep(0.05)


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
            path.mkdir(parents=True, exist_ok=True)
        self.runtime_dir.chmod(0o700)
        self.config.write_text(
            "\n".join(
                [
                    f"copy-on-select = {copy_on_select}",
                    # Replace the former command-palette trigger. If the old
                    # hard-coded check runs before the effective table, the
                    # explicit-Copy assertion below fails.
                    "keybind = alt+shift+p = copy",
                    "keybind = ctrl+shift+p = command_palette",
                    "show-sidebar-agents = false",
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
            self.cell_width, self.cell_height = _measure_terminal_cell(self)
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

    def type_text(self, value: str) -> None:
        subprocess.run(
            [
                "xdotool",
                "windowfocus",
                "--sync",
                self.window,
                "type",
                "--clearmodifiers",
                "--delay",
                "18",
                value,
            ],
            env=self.xenv,
            check=True,
        )
        time.sleep(0.2)

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


def _measure_terminal_cell(launch: Launch) -> tuple[int, int]:
    """Measure the live renderer grid through one explicit-background cell."""
    marker = (17, 201, 93)
    path = launch.root / "terminal-cell-metrics.png"
    measured: list[tuple[int, int]] = []

    def capture() -> bool:
        launch.client.tab_feed_pty_bytes(
            launch.tab, b"\x1b[2J\x1b[H\x1b[48;2;17;201;93m \x1b[0m"
        )
        png, _width, _height = launch.client.screenshot(scale=1)
        path.write_bytes(png)
        image = pngtool.load(str(path))
        width, height, bpp, pixels = image
        metrics = launch.client.window_metrics()
        x0 = round(float(metrics["sidebar_width"])) + 12
        y0 = round(launch.client.terminal_top(metrics)) + 12

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
    launch.client.tab_feed_pty_bytes(launch.tab, b"\x1b[0m\x1b[2J\x1b[H")
    return measured[-1]


def _set_row(launch: Launch, text: str) -> None:
    payload = b"\x1b[2J\x1b[H" + text.encode("utf-8")
    timeout = 10.0 * SCALE
    deadline = time.monotonic() + timeout
    retry_interval = timeout / 4
    next_feed = 0.0
    attempts = 0
    last_dump: dict | None = None
    last_error: Exception | None = None

    # Paste and middle-click checks still send their bytes to the real PTY, so
    # delayed shell echo can overwrite row zero after this test fixture lands.
    # Reapply the complete clear/home/text fixture at a few spaced intervals;
    # every retry is idempotent and all attempts share the original deadline.
    while time.monotonic() < deadline:
        now = time.monotonic()
        if attempts < 4 and now >= next_feed:
            # Transport/protocol errors are real failures and intentionally
            # propagate instead of being mistaken for renderer scheduling.
            launch.client.tab_feed_pty_bytes(launch.tab, payload)
            attempts += 1
            next_feed = now + retry_interval
        try:
            last_dump = launch.client.dump(launch.tab)
            rows = last_dump.get("rows_text", [])
            if rows and rows[0].startswith(text):
                return
        except (ConnectionError, FileNotFoundError, OSError) as error:
            last_error = error
        time.sleep(0.1)

    suffix = f"; last error: {last_error}" if last_error else ""
    raise AssertionError(
        f"timed out waiting for terminal row {text!r} after {attempts} feeds; "
        f"last dump: {last_dump!r}{suffix}"
    )


def _capture_contains(launch: Launch, expected: bytes) -> bool:
    return expected in launch.client.tab_capture_pty_input(launch.tab, drain=True)


def _drain_tabs(launch: Launch, tab_ids: list[int]) -> None:
    for tab_id in tab_ids:
        launch.client.tab_capture_pty_input(tab_id, drain=True)


def _assert_no_pty_input(
    launch: Launch, tab_ids: list[int], description: str, duration: float = 0.5
) -> None:
    deadline = time.monotonic() + duration * SCALE
    captured = bytearray()
    while time.monotonic() < deadline:
        for tab_id in tab_ids:
            captured.extend(launch.client.tab_capture_pty_input(tab_id, drain=True))
        time.sleep(0.05)
    if captured:
        raise AssertionError(f"{description} leaked PTY bytes: {bytes(captured)!r}")


def _hold_key(launch: Launch, modifier: str, key: str, seconds: float = 1.2) -> None:
    subprocess.run(
        [
            "xdotool",
            "windowfocus",
            "--sync",
            launch.window,
            "keydown",
            "--clearmodifiers",
            modifier,
            "keydown",
            key,
            "sleep",
            str(seconds * SCALE),
            "keyup",
            key,
            "keyup",
            modifier,
        ],
        env=launch.xenv,
        check=True,
    )
    time.sleep(0.2)


def _hold_plain_key(launch: Launch, key: str, seconds: float = 1.2) -> None:
    subprocess.run(
        [
            "xdotool",
            "windowfocus",
            "--sync",
            launch.window,
            "keydown",
            "--clearmodifiers",
            key,
            "sleep",
            str(seconds * SCALE),
            "keyup",
            key,
        ],
        env=launch.xenv,
        check=True,
    )
    time.sleep(0.2)


def _screenshot_color_count(
    launch: Launch,
    color: tuple[int, int, int],
    name: str,
    region: tuple[float, float, float, float] | None = None,
) -> int:
    """Count exact-color pixels, optionally within a fractional
    (x0, y0, x1, y1) window region so a fence can't be satisfied by
    ambient content elsewhere in the frame."""
    png, _width, _height = launch.client.screenshot(scale=1)
    path = launch.root / f"{name}.png"
    path.write_bytes(png)
    width, height, bpp, data = pngtool.load(str(path))
    x0, y0, x1, y1 = (0, 0, width, height)
    if region is not None:
        x0 = int(region[0] * width)
        y0 = int(region[1] * height)
        x1 = int(region[2] * width)
        y1 = int(region[3] * height)
    return sum(
        tuple(data[(y * width + x) * bpp : (y * width + x) * bpp + 3]) == color
        for y in range(y0, y1)
        for x in range(x0, x1)
    )


def _wait_for_inline_editor_focus(
    launch: Launch, baseline: int, name: str
) -> None:
    """Wait for the renderer-owned focused border before sending replacement text."""
    _wait_until(
        lambda: _screenshot_color_count(
            launch, (0x4E, 0x9A, 0xF1), f"{name}-focused"
        )
        >= baseline + 40,
        f"{name} focused inline editor",
    )


def _wait_for_inline_editor_closed(
    launch: Launch, baseline: int, name: str
) -> None:
    _wait_until(
        lambda: _screenshot_color_count(
            launch, (0x4E, 0x9A, 0xF1), f"{name}-closed"
        )
        <= baseline + 10,
        f"{name} inline editor closed",
    )


def _open_inline_editor_with_key(launch: Launch, combo: str, name: str) -> None:
    baseline = _screenshot_color_count(
        launch, (0x4E, 0x9A, 0xF1), f"{name}-baseline"
    )
    launch.key(combo)
    _wait_for_inline_editor_focus(launch, baseline, name)


def _tab_is_attached(launch: Launch, tab_id: int) -> bool:
    try:
        return bool(launch.client.dump(tab_id))
    except RoostError as error:
        if error.code == "not-found" and "has no live terminal" in error.message:
            return False
        raise


def _wait_product_extent(
    launch: Launch, requested_width: int, requested_height: int
) -> tuple[int, int]:
    """Wait for two product captures at the compositor-settled extent."""
    state: dict[str, object] = {"extent": None, "stable": 0}
    path = launch.root / "chrome-resize.png"

    def settled() -> bool:
        png, width, height = launch.client.screenshot(scale=1)
        path.write_bytes(png)
        extent = (width, height)
        if extent == (requested_width, requested_height) and extent == state["extent"]:
            state["stable"] = int(state["stable"]) + 1
        else:
            state["stable"] = 1 if extent == (requested_width, requested_height) else 0
        state["extent"] = extent
        return int(state["stable"]) >= 2

    _wait_until(
        settled,
        f"two stable {requested_width}x{requested_height} product captures",
    )
    return requested_width, requested_height


def _active_pill_bounds(
    launch: Launch, expected_extent: tuple[int, int] | None = None
) -> tuple[int, int, int, int]:
    """Locate stable exact-color bounds for the rendered active tab pill."""
    metrics = launch.client.window_metrics()
    sidebar = round(float(metrics["sidebar_width"]))
    terminal_top = round(launch.client.terminal_top(metrics))
    path = launch.root / "chrome-close.png"
    previous: tuple[int, int, int, int] | None = None
    for _attempt in range(6):
        png, screenshot_width, screenshot_height = launch.client.screenshot(scale=1)
        if expected_extent is not None and (
            screenshot_width,
            screenshot_height,
        ) != expected_extent:
            previous = None
            time.sleep(0.1)
            continue
        path.write_bytes(png)
        width, height, bpp, data = pngtool.load(str(path))
        active = (0x24, 0x37, 0x51)
        points = []
        for y in range(min(terminal_top, height)):
            for x in range(max(0, sidebar), width):
                offset = (y * width + x) * bpp
                if tuple(data[offset : offset + 3]) == active:
                    points.append((x, y))
        if not points:
            previous = None
            time.sleep(0.1)
            continue
        xs, ys = zip(*points)
        bounds = min(xs), min(ys), max(xs), max(ys)
        left, top, right, bottom = bounds
        if right - left < 35 or bottom - top < 10:
            raise AssertionError(f"active tab pill has implausible bounds {bounds!r}")
        if bounds == previous:
            return bounds
        previous = bounds
        time.sleep(0.1)
    raise AssertionError(f"active tab pill never settled; last bounds: {previous!r}")


def _active_pill_close_point(
    launch: Launch, expected_extent: tuple[int, int] | None = None
) -> tuple[int, int]:
    """Locate the rendered active pill, then target its trailing close sibling."""
    _left, _top, right, _bottom = _active_pill_bounds(launch, expected_extent)
    return right - 11, round(launch.client.terminal_top()) // 2


def _stable_rollup_stripe_point(launch: Launch) -> tuple[int, int]:
    """Locate one stable waiting-agent project stripe after focus settles."""
    path = launch.root / "chrome-project-rollup-stripe.png"
    previous: tuple[int, int, int, int] | None = None
    for _attempt in range(50):
        png, _width, _height = launch.client.screenshot(scale=1)
        path.write_bytes(png)
        image = pngtool.load(str(path))
        _width, height, _bpp, _data = image
        metrics = launch.client.window_metrics()
        sidebar = round(float(metrics["sidebar_width"]))
        band = round(launch.client.terminal_top(metrics))
        bounds = parity.unique_rollup_stripe_bounds(
            image,
            parity.LIFECYCLE_COLORS["waiting"],
            sidebar,
            band,
            height - band,
        )
        if bounds is not None and bounds == previous:
            left, top, right, bottom = bounds
            return (left + right) // 2, (top + bottom) // 2
        previous = bounds
        time.sleep(0.1)
    raise AssertionError(
        f"one stable waiting-agent rollup stripe never settled; last bounds "
        f"{previous!r}, capture {path}"
    )


def _click_window_control(launch: Launch, x: int, y: int) -> None:
    """Fence move/press/release so tiny-skia cannot batch away a button edge."""
    launch.terminal_pointer(
        ["mousemove", "--window", launch.window, str(x), str(y)]
    )
    launch.terminal_pointer(["mousedown", "1"])
    launch.terminal_pointer(["mouseup", "1"])


def _double_click_window_control(launch: Launch, x: int, y: int) -> None:
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


def _palette_color_bounds(
    launch: Launch, color: tuple[int, int, int], minimum_pixels: int
) -> tuple[int, int, int, int]:
    """Return stable exact-color bounds from the in-product renderer capture."""
    path = launch.root / f"palette-{color[0]:02x}{color[1]:02x}{color[2]:02x}.png"
    previous: tuple[int, int, int, int] | None = None
    for _attempt in range(12):
        png, _width, _height = launch.client.screenshot(scale=1)
        path.write_bytes(png)
        width, height, bpp, data = pngtool.load(str(path))
        points: list[tuple[int, int]] = []
        for y in range(height):
            for x in range(width):
                offset = (y * width + x) * bpp
                if tuple(data[offset : offset + 3]) == color:
                    points.append((x, y))
        if len(points) >= minimum_pixels:
            xs, ys = zip(*points)
            bounds = min(xs), min(ys), max(xs), max(ys)
            if bounds == previous:
                return bounds
            previous = bounds
        else:
            previous = None
        time.sleep(0.1)
    raise AssertionError(
        f"palette color {color!r} never settled with {minimum_pixels} pixels; "
        f"last bounds: {previous!r}; capture: {path}"
    )


def _palette_pointer_routing(launch: Launch) -> None:
    """Prove the transparent catcher and semantic rows own their click regions."""
    launch.client.window_resize(640, 360)
    width, _height = _wait_product_extent(launch, 640, 360)
    project = int(launch.client.identify()["active_project_id"])

    state = launch.client.palette_open("commands")
    assert state["frame"] == "commands"
    panel = _palette_color_bounds(launch, (0x2D, 0x2D, 0x33), 5_000)
    assert panel[0] >= 15 and panel[2] <= width - 15, panel
    assert panel[1] >= 59, panel

    # Blank padding belongs to the card. It must neither dismiss the palette
    # nor leak into the terminal beneath it.
    _click_window_control(launch, panel[2] - 4, panel[3] - 4)
    _assert_stays(
        lambda: launch.client.palette_state().get("frame") == "commands",
        "blank card click remains inside the palette",
    )

    state = launch.client.palette_query("theme")
    assert state["selection"] == 0
    selected = _palette_color_bounds(launch, (0x48, 0x48, 0x4E), 1_000)
    _click_window_control(
        launch,
        (selected[0] + selected[2]) // 2,
        (selected[1] + selected[3]) // 2,
    )
    _wait_until(
        lambda: launch.client.palette_state().get("frame") == "themes",
        "exact selected palette row activation",
    )
    # A captured Escape pops the nested frame and must explicitly return
    # keyboard ownership to the restored root TextInput.
    launch.key("Escape")
    _wait_until(
        lambda: launch.client.palette_state().get("frame") == "commands",
        "nested palette Escape returns to root",
    )
    launch.key("ctrl+a")
    launch.type_text("toggle sidebar")
    _wait_until(
        lambda: launch.client.palette_state().get("query") == "toggle sidebar",
        "nested palette Escape restores text input focus",
    )
    launch.client.palette_dismiss()

    # The fixed add-tab control is deliberately outside the card. The first
    # click dismisses exactly once and cannot also activate that control.
    before = len(launch.client.project_tab_ids(project))
    launch.client.palette_open("commands")
    terminal_top = round(launch.client.terminal_top())
    _click_window_control(launch, width - 49, terminal_top // 2)
    _wait_until(
        lambda: launch.client.palette_state().get("open") is False,
        "outside click palette dismissal",
    )
    _assert_stays(
        lambda: len(launch.client.project_tab_ids(project)) == before,
        "outside dismissal does not click through to add-tab",
    )

    # The Enter that confirms a palette rename command is still physically
    # held while the inline TextInput appears. Its repeats must not immediately
    # submit and close the newly opened editor.
    active_tab = int(launch.client.identify()["active_tab_id"])
    title_before = launch.client.tab(active_tab)["title"]
    launch.client.palette_open("commands")
    state = launch.client.palette_query("rename tab")
    assert state["items"][state["selection"]]["id"] == "rename_tab", state
    _hold_plain_key(launch, "Return")
    _wait_until(
        lambda: launch.client.palette_state().get("open") is False,
        "held palette Enter opens inline rename",
    )
    _assert_stays(
        lambda: not launch.client.app_active_terminal_focused(),
        "held palette Enter leaves inline rename open",
    )
    assert launch.client.tab(active_tab)["title"] == title_before
    launch.key("Escape")
    _wait_until(
        launch.client.app_active_terminal_focused,
        "Escape closes held-Enter palette rename editor",
    )


def _keybind_dispatch(launch: Launch) -> tuple[int, int]:
    """Exercise the shared configured action table through physical XTEST keys."""
    identity = launch.client.identify()
    home_project = int(identity["active_project_id"])
    home_tab = int(identity["active_tab_id"])
    sibling = launch.client.open_tab(home_project, title="SHORTCUT-SIBLING")
    _wait_until(lambda: _tab_is_attached(launch, sibling), "shortcut sibling PTY")
    launch.client.reorder_tabs(home_project, [sibling, home_tab])
    _wait_until(
        lambda: launch.client.project_tab_ids(home_project) == [sibling, home_tab],
        "authoritative reordered tab snapshot",
    )

    other_project = launch.client.create_project("SHORTCUT-OTHER", "/tmp")
    other_first = launch.client.open_tab(other_project, "/tmp", "OTHER-FIRST")
    other_tab = launch.client.open_tab(other_project, "/tmp", "OTHER-PREFERRED")
    _wait_until(lambda: _tab_is_attached(launch, other_first), "other first PTY")
    _wait_until(lambda: _tab_is_attached(launch, other_tab), "other preferred PTY")
    # The reorder response means the engine mutation committed. Dispatch
    # immediately: adapter-local caches are deliberately not a prerequisite.
    launch.client.reorder_projects([other_project, home_project])

    launch.client.focus(home_tab)
    _drain_tabs(launch, [home_tab, sibling, other_first, other_tab])
    launch.key("ctrl+1")
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == sibling,
        "Ctrl+1 reordered tab selection",
    )
    _assert_no_pty_input(
        launch, [home_tab, sibling], "supported numeric tab shortcut"
    )

    launch.key("ctrl+9")
    _assert_stays(
        lambda: launch.client.identify()["active_tab_id"] == sibling,
        "out-of-range tab shortcut no-op",
    )
    _assert_no_pty_input(launch, [sibling], "out-of-range tab shortcut")

    launch.key("alt+1")
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == other_tab,
        "Alt+1 reordered project selection",
    )
    _assert_no_pty_input(
        launch, [sibling, other_tab], "supported numeric project shortcut"
    )
    launch.key("alt+9")
    _assert_stays(
        lambda: launch.client.identify()["active_tab_id"] == other_tab,
        "out-of-range project shortcut no-op",
    )
    _assert_no_pty_input(launch, [other_tab], "out-of-range project shortcut")

    # Jump-to-unread must clear the engine notification and reveal its target
    # even when the sidebar was hidden at dispatch time.
    launch.client.notify(home_tab, "Shortcut unread")
    launch.client.wait_notification(home_tab, True)
    if not launch.client.window_metrics()["sidebar_collapsed"]:
        launch.key("alt+b")
    _wait_until(
        lambda: launch.client.window_metrics()["sidebar_collapsed"],
        "sidebar collapse before jump-to-unread",
    )
    _drain_tabs(launch, [other_tab, home_tab])
    launch.key("alt+shift+u")
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == home_tab,
        "jump-to-unread focus",
    )
    _wait_until(lambda: not launch.client.has_notification(home_tab), "unread clear")
    _wait_until(
        lambda: not launch.client.window_metrics()["sidebar_collapsed"],
        "jump-to-unread sidebar reveal",
    )
    _assert_no_pty_input(launch, [other_tab, home_tab], "jump-to-unread shortcut")

    launch.client.focus(sibling)
    launch.key("alt+shift+bracketleft")
    _assert_stays(
        lambda: launch.client.identify()["active_tab_id"] == sibling,
        "previous-tab clamp at first tab",
    )
    launch.key("alt+shift+bracketright")
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == home_tab,
        "next-tab navigation",
    )
    launch.key("alt+shift+bracketright")
    _assert_stays(
        lambda: launch.client.identify()["active_tab_id"] == home_tab,
        "next-tab clamp at last tab",
    )
    _assert_no_pty_input(launch, [sibling, home_tab], "tab cycle shortcuts")

    # This real-seat hold supplements the deterministic Rust repeat-path unit
    # test. When Xvfb emits auto-repeat, New/Close must still execute once.
    before = len(launch.client.project_tab_ids(home_project))
    _drain_tabs(launch, [home_tab])
    _hold_key(launch, "alt", "t")
    _wait_until(
        lambda: len(launch.client.project_tab_ids(home_project)) == before + 1,
        "held New Tab one-shot",
    )
    _assert_stays(
        lambda: len(launch.client.project_tab_ids(home_project)) == before + 1,
        "held New Tab repeat suppression",
    )
    _assert_no_pty_input(launch, [home_tab], "held New Tab shortcut")

    active_before_close = int(launch.client.identify()["active_tab_id"])
    count_before_close = len(launch.client.project_tab_ids(home_project))
    _drain_tabs(launch, [active_before_close, home_tab])
    _hold_key(launch, "alt", "w")
    _wait_until(
        lambda: len(launch.client.project_tab_ids(home_project)) == count_before_close - 1,
        "held Close Tab one-shot",
    )
    _assert_stays(
        lambda: len(launch.client.project_tab_ids(home_project)) == count_before_close - 1,
        "held Close Tab repeat suppression",
    )

    # Inline project/tab rename owns physical text input, never the PTY. A
    # project shortcut reveals the collapsed sidebar so its editor cannot trap
    # keys while hidden.
    launch.client.focus(home_tab)
    if not launch.client.window_metrics()["sidebar_collapsed"]:
        launch.key("alt+b")
    _wait_until(
        lambda: launch.client.window_metrics()["sidebar_collapsed"],
        "sidebar collapsed before Rename Project",
    )
    _drain_tabs(launch, [home_tab])
    project_editor_baseline = _screenshot_color_count(
        launch, (0x4E, 0x9A, 0xF1), "rename-project-baseline"
    )
    launch.key("alt+shift+r")
    _wait_until(
        lambda: not launch.client.window_metrics()["sidebar_collapsed"],
        "Rename Project reveals its inline editor",
    )
    _wait_for_inline_editor_focus(
        launch, project_editor_baseline, "rename-project"
    )
    launch.type_text("RENAMED-PROJECT")
    launch.key("Return")
    _wait_until(
        lambda: launch.client.project(home_project)["name"] == "RENAMED-PROJECT",
        "Rename Project shortcut commit",
    )
    _wait_for_inline_editor_closed(
        launch, project_editor_baseline, "rename-project"
    )
    _assert_no_pty_input(launch, [home_tab], "Rename Project shortcut")

    rename_tab_baseline = _screenshot_color_count(
        launch, (0x4E, 0x9A, 0xF1), "rename-tab-baseline"
    )
    launch.key("alt+r")
    _wait_for_inline_editor_focus(launch, rename_tab_baseline, "rename-tab")
    launch.type_text("RENAMED-TAB")
    _hold_plain_key(launch, "Return")
    _wait_until(
        lambda: launch.client.tab(home_tab)["title"] == "RENAMED-TAB",
        "Rename Tab shortcut commit",
    )
    _wait_for_inline_editor_closed(launch, rename_tab_baseline, "rename-tab")
    _assert_no_pty_input(launch, [home_tab], "Rename Tab shortcut")

    # The inactive first tab selector is owned by the stable-ID strip gesture,
    # so its double-click must select that ID before opening the same editor.
    double_click_baseline = _screenshot_color_count(
        launch, (0x4E, 0x9A, 0xF1), "double-click-tab-baseline"
    )
    metrics = launch.client.window_metrics()
    _double_click_window_control(
        launch,
        round(float(metrics["sidebar_width"])) + 45,
        17,
    )
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == sibling,
        "inactive tab double-click selection",
    )
    _wait_for_inline_editor_focus(
        launch, double_click_baseline, "double-click-tab"
    )
    launch.type_text("DOUBLE-TAB")
    launch.key("Return")
    _wait_until(
        lambda: launch.client.tab(sibling)["title"] == "DOUBLE-TAB",
        "tab double-click inline rename",
    )
    _wait_for_inline_editor_closed(
        launch, double_click_baseline, "double-click-tab"
    )
    _assert_no_pty_input(launch, [home_tab, sibling], "tab double-click rename")

    # The first project is inactive and was authoritatively reordered to the
    # first sidebar row above. Its double-click selects and renames that exact
    # stable project ID.
    launch.client.focus(home_tab)
    project_double_baseline = _screenshot_color_count(
        launch, (0x4E, 0x9A, 0xF1), "double-click-project-baseline"
    )
    _double_click_window_control(launch, 100, 52)
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == other_project,
        "inactive project double-click selection",
    )
    _wait_for_inline_editor_focus(
        launch, project_double_baseline, "double-click-project"
    )
    launch.type_text("DOUBLE-PROJECT")
    launch.key("Return")
    _wait_until(
        lambda: launch.client.project(other_project)["name"] == "DOUBLE-PROJECT",
        "project double-click inline rename",
    )
    _wait_for_inline_editor_closed(
        launch, project_double_baseline, "double-click-project"
    )
    launch.client.focus(home_tab)

    # Escape and terminal click-away both discard the draft and clear pending
    # editor focus. The next ordinary key must route to the terminal.
    cancel_project_baseline = _screenshot_color_count(
        launch, (0x4E, 0x9A, 0xF1), "cancel-project-baseline"
    )
    launch.key("alt+shift+r")
    _wait_for_inline_editor_focus(
        launch, cancel_project_baseline, "cancel-project"
    )
    launch.type_text("CANCELLED-PROJECT")
    _hold_plain_key(launch, "Escape")
    _assert_stays(
        lambda: launch.client.project(home_project)["name"] == "RENAMED-PROJECT",
        "Escape cancels project rename",
    )
    _wait_for_inline_editor_closed(
        launch, cancel_project_baseline, "cancel-project"
    )
    _assert_no_pty_input(launch, [home_tab], "Escape project rename")

    _open_inline_editor_with_key(launch, "alt+shift+r", "clickaway-project")
    launch.type_text("CLICKAWAY-PROJECT")
    metrics = launch.client.window_metrics()
    _click_window_control(
        launch,
        round(float(metrics["sidebar_width"])) + 80,
        round(launch.client.terminal_top(metrics)) + 80,
    )
    _assert_stays(
        lambda: launch.client.project(home_project)["name"] == "RENAMED-PROJECT",
        "terminal click-away cancels project rename",
    )
    _drain_tabs(launch, [home_tab])
    launch.key("x")
    _wait_until(
        lambda: _capture_contains(launch, b"x"),
        "terminal keyboard route resumes after rename click-away",
    )

    # Blank sidebar chrome has no child action to emit a cancellation. The
    # root pointer catcher must still close the editor instead of leaving a
    # defocused, invisible keyboard trap behind.
    _open_inline_editor_with_key(launch, "alt+shift+r", "blank-click-project")
    launch.type_text("BLANK-CLICK-PROJECT")
    _click_window_control(launch, 100, 330)
    _wait_until(
        launch.client.app_active_terminal_focused,
        "blank chrome click cancels rename keyboard ownership",
    )
    _assert_stays(
        lambda: launch.client.project(home_project)["name"] == "RENAMED-PROJECT",
        "blank chrome click discards project rename draft",
    )
    _drain_tabs(launch, [home_tab])
    launch.key("y")
    _wait_until(
        lambda: _capture_contains(launch, b"y"),
        "terminal keyboard route resumes after blank chrome click",
    )

    # Restore the launch fixture for the independent close/overflow checks
    # that follow. Those scenarios deliberately assume one baseline tab.
    launch.client.focus(home_tab)
    for tab_id in launch.client.project_tab_ids(home_project):
        if tab_id != home_tab:
            launch.client.close_tab(tab_id)
    if launch.client.project(other_project) is not None:
        launch.client.delete_project(other_project)
    _wait_until(
        lambda: launch.client.project_tab_ids(home_project) == [home_tab]
        and launch.client.identify()["active_tab_id"] == home_tab,
        "shortcut fixture cleanup",
    )
    return home_project, home_tab


def _direct_tab_reorder(
    launch: Launch, project: int, home_tab: int
) -> list[str]:
    """Drive live stable-ID tab reorders and return persisted title order."""
    second = launch.client.open_tab(project, title="DRAG-SECOND")
    third = launch.client.open_tab(project, title="DRAG-THIRD")
    # Lock the labels as user titles. Otherwise the live shell may derive a
    # cwd/process title before or after persistence, making title order an
    # invalid proxy for restored position even though the positions are right.
    launch.client.set_title(second, "DRAG-SECOND")
    launch.client.set_title(third, "DRAG-THIRD")
    tabs = [home_tab, second, third]
    for tab_id in tabs:
        _wait_until(lambda tab_id=tab_id: _tab_is_attached(launch, tab_id), f"drag tab {tab_id}")
    _wait_until(
        lambda: launch.client.project_tab_ids(project) == tabs,
        "initial direct-drag tab order",
    )
    _drain_tabs(launch, tabs)

    def drag_active(
        source: int,
        target_x: int,
        expected: list[int],
        label: str,
        *,
        focus_source: bool = True,
        release_outside: bool = False,
    ) -> None:
        if focus_source:
            launch.client.focus(source)
            _wait_until(
                lambda: int(launch.client.identify()["active_tab_id"]) == source,
                f"{label} source focus",
            )
        else:
            assert int(launch.client.identify()["active_tab_id"]) != source
        if focus_source:
            left, top, right, bottom = _active_pill_bounds(launch)
            x0 = left + min(22, max(8, (right - left) // 3))
            y = (top + bottom) // 2
        else:
            # This fixture's source is the first inactive pill. An active-pill
            # color scan would locate `third`, defeating the scenario.
            x0 = sidebar + 30
            y = round(launch.client.terminal_top()) // 2
        preview_path = launch.root / f"{label.lower().replace(' ', '-')}-preview.png"

        def preview_accent_capture() -> tuple[tuple[int, int] | None, bytes]:
            png, _width, _height = launch.client.screenshot(scale=1)
            preview_path.write_bytes(png)
            width, height, bpp, pixels = pngtool.load(str(preview_path))
            metrics = launch.client.window_metrics()
            band = min(round(launch.client.terminal_top(metrics)), height)
            left = max(0, round(float(metrics["sidebar_width"])))
            accent = (0x4E, 0x9A, 0xF1)
            runs: list[tuple[int, int]] = []
            for row in range(band):
                start: int | None = None
                for column in range(left, width):
                    offset = (row * width + column) * bpp
                    if tuple(pixels[offset : offset + 3]) == accent:
                        start = column if start is None else start
                    elif start is not None:
                        if column - start >= 40:
                            runs.append((start, column - 1))
                        start = None
                if start is not None and width - start >= 40:
                    runs.append((start, width - 1))
            longest = max(runs, key=lambda run: run[1] - run[0], default=None)
            return longest, png

        baseline_run, baseline_png = preview_accent_capture()
        assert baseline_run is None, (label, "unexpected pre-drag accent", baseline_run)
        capture_root = os.environ.get("ROOST_CAPTURE_DIR")
        if capture_root:
            capture_dir = Path(capture_root)
            capture_dir.mkdir(parents=True, exist_ok=True)
            capture_name = label.lower().replace(" ", "-")
            (capture_dir / f"{capture_name}-before.png").write_bytes(baseline_png)
        pending_error: BaseException | None = None
        try:
            launch.terminal_pointer(
                ["mousemove", "--window", launch.window, str(x0), str(y)]
            )
            launch.terminal_pointer(["mousedown", "1"])
            if not focus_source:
                _wait_until(
                    lambda: int(launch.client.identify()["active_tab_id"])
                    == source,
                    f"{label} press selects its stable ID",
                )
            pressed_capture: list[bytes] = []

            def source_press_rendered() -> bool:
                run, png = preview_accent_capture()
                if run is None:
                    return False
                left, right = run
                if not (left <= x0 <= right):
                    return False
                pressed_capture[:] = [png]
                return True

            # The held-source accent is a product-visible causal fence: Iced
            # has consumed the native press at the intended stable-ID pill
            # before the first trajectory sample can change cursor position.
            _wait_until(source_press_rendered, f"{label} source press render")
            if capture_root:
                (capture_dir / f"{capture_name}-pressed.png").write_bytes(
                    pressed_capture[-1]
                )
            # Tiny-skia on a loaded CI runner can process XTEST more slowly
            # than xdotool submits it. Separate each trajectory sample so the
            # existing terminal_pointer fence leaves the native event loop
            # time to observe press, threshold crossing, preview, and release.
            for step in range(1, 9):
                x = round(x0 + (target_x - x0) * step / 8)
                launch.terminal_pointer(
                    ["mousemove", "--window", launch.window, str(x), str(y)]
                )
            held_capture: list[bytes] = []

            def preview_rendered() -> bool:
                run, png = preview_accent_capture()
                if run is None:
                    return False
                left, right = run
                if target_x < x0:
                    at_target = left <= target_x
                else:
                    at_target = right >= target_x - 60
                if not at_target:
                    return False
                held_capture[:] = [png]
                return True

            _wait_until(preview_rendered, f"{label} held preview render")
            held_png = held_capture[-1]
            if capture_root:
                (capture_dir / f"{capture_name}-held.png").write_bytes(held_png)
            if release_outside:
                outside_y = round(launch.client.terminal_top()) + 20
                launch.terminal_pointer(
                    [
                        "mousemove",
                        "--window",
                        launch.window,
                        str(target_x),
                        str(outside_y),
                    ]
                )
        except BaseException as error:
            pending_error = error
            raise
        finally:
            try:
                launch.terminal_pointer(["mouseup", "1"])
            except Exception as release_error:
                if pending_error is None:
                    raise
                print(
                    f"failed to release {label} drag while handling {pending_error!r}: "
                    f"{release_error}",
                    file=sys.stderr,
                )
        _wait_until(
            lambda: launch.client.project_tab_ids(project) == expected,
            f"{label} authoritative reorder",
        )
        assert int(launch.client.identify()["active_tab_id"]) == source
        assert launch.process.poll() is None, f"Iced exited during {label}"

    metrics = launch.client.window_metrics()
    sidebar = round(float(metrics["sidebar_width"]))
    # The first source is inactive: the press must select its stable ID even
    # though adding the active close button changes that pill's width mid-drag.
    drag_active(
        home_tab,
        sidebar + 440,
        [second, third, home_tab],
        "forward inactive tab",
        focus_source=False,
    )
    # Stay comfortably inside the tab scroller while landing before the first
    # pill center. Outside the viewport, Iced correctly withholds live pointer
    # coordinates even though the eventual release still settles.
    drag_active(home_tab, sidebar + 30, [home_tab, second, third], "backward tab")
    # Move into the terminal before releasing. The strip no longer has cursor
    # coordinates, but it still owns and must settle the captured gesture.
    drag_active(
        second,
        sidebar + 440,
        [home_tab, third, second],
        "final outside-release tab",
        release_outside=True,
    )

    # A modal palette opened from the keyboard while the pointer is still held
    # must invalidate the preview. Its eventual release cannot commit behind
    # the overlay, even if the strip does not receive that release directly.
    launch.client.focus(home_tab)
    before_palette = launch.client.project_tab_ids(project)
    left, top, right, bottom = _active_pill_bounds(launch)
    x0 = left + min(22, max(8, (right - left) // 3))
    y = (top + bottom) // 2
    pending_error: BaseException | None = None
    try:
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(x0), str(y)]
        )
        launch.terminal_pointer(["mousedown", "1"])
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(sidebar + 300), str(y)]
        )
        # Keep the pointer button physically held while entering the shortcut.
        # Launch.key focuses the window and clears modifiers, which can synthesize
        # unrelated pointer settlement on some X11 servers.
        launch.terminal_pointer(
            [
                "keydown",
                "ctrl",
                "keydown",
                "shift",
                "key",
                "p",
                "keyup",
                "shift",
                "keyup",
                "ctrl",
            ]
        )
        _wait_until(
            lambda: launch.client.palette_state().get("open") is True,
            "palette opens over held tab drag",
        )
    except BaseException as error:
        pending_error = error
        raise
    finally:
        try:
            launch.terminal_pointer(["mouseup", "1"])
        except Exception as release_error:
            if pending_error is None:
                raise
            print(
                f"failed to release palette-cancel drag while handling {pending_error!r}: "
                f"{release_error}",
                file=sys.stderr,
            )
    after_palette = launch.client.project_tab_ids(project)
    assert after_palette == before_palette, (before_palette, after_palette)
    _assert_stays(
        lambda: launch.client.project_tab_ids(project) == before_palette,
        "palette cancels held tab drag",
    )
    launch.key("Escape")
    _wait_until(
        lambda: launch.client.palette_state().get("open") is False,
        "palette closes after held-drag cancellation",
    )

    launch.key("ctrl+1")
    _wait_until(
        lambda: int(launch.client.identify()["active_tab_id"]) == home_tab,
        "shortcut follows direct-drag order",
    )
    _assert_no_pty_input(launch, tabs, "direct tab drag and reordered shortcut")
    return ["RENAMED-TAB", "DRAG-THIRD", "DRAG-SECOND"]


def _project_ids(launch: Launch) -> list[int]:
    """Authoritative sidebar order — the snapshot sorts by persisted position."""
    return [int(project["id"]) for project in launch.client.list()]


def _sidebar_row_center(launch: Launch, index: int) -> tuple[int, int]:
    """Window-relative center of the index-th sidebar project row.

    The band height and sidebar width come from the live product metrics the
    tab-strip checks already trust, so a chrome layout change fails as a
    verified wrong-row click below rather than as an unexplained drag result.
    """
    metrics = launch.client.window_metrics()
    x = round(float(metrics["sidebar_width"])) // 2
    top = (
        round(launch.client.terminal_top(metrics))
        + SIDEBAR_BODY_TOP_PADDING
        + index * (SIDEBAR_ROW_HEIGHT + SIDEBAR_ROW_SPACING)
    )
    return x, top + SIDEBAR_ROW_HEIGHT // 2


def _confirm_delete_fill_pixels(launch: Launch, name: str) -> int:
    """Count the destructive Delete button fill (chrome::danger_button).

    Scoped to the central window region: the confirm panel is a centered
    420px-max card, so terminal content near the edges can never satisfy
    the fence even if it paints the exact danger color."""
    return _screenshot_color_count(
        launch,
        (0x8A, 0x2A, 0x2A),
        f"{name}-confirm",
        region=(0.25, 0.25, 0.75, 0.75),
    )


def _wait_confirm_delete_open(launch: Launch, name: str) -> None:
    _wait_until(
        lambda: _confirm_delete_fill_pixels(launch, name) >= 200,
        f"{name} delete-confirm overlay painted",
    )


def _wait_confirm_delete_closed(launch: Launch, name: str) -> None:
    _wait_until(
        lambda: _confirm_delete_fill_pixels(launch, name) <= 10,
        f"{name} delete-confirm overlay dismissed",
    )


def _project_lifecycle(launch: Launch, home_project: int, home_tab: int) -> None:
    """Drive real sidebar project drags and the keybind-opened delete confirm."""
    agents_visible = launch.client.sidebar_dump()["agents_visible"]
    assert not agents_visible, (
        "project row geometry assumes show-sidebar-agents = false; sidebar_dump "
        f"reports agents_visible={agents_visible!r}"
    )
    baseline = _project_ids(launch)
    assert baseline == [home_project], (
        "project lifecycle expects the single-project fixture left by the tab "
        f"checks; sidebar holds {baseline!r}"
    )

    alpha = launch.client.create_project("DRAG-PROJECT-ALPHA", "/tmp")
    alpha_tab = launch.client.open_tab(alpha, "/tmp", "ALPHA-SHELL")
    beta = launch.client.create_project("DRAG-PROJECT-BETA", "/tmp")
    beta_tab = launch.client.open_tab(beta, "/tmp", "BETA-SHELL")
    for tab_id in (alpha_tab, beta_tab):
        _wait_until(
            lambda tab_id=tab_id: _tab_is_attached(launch, tab_id),
            f"project lifecycle tab {tab_id} PTY",
        )
    ordered = [home_project, alpha, beta]
    launch.client.reorder_projects(ordered)
    _wait_until(
        lambda: _project_ids(launch) == ordered,
        "initial project lifecycle sidebar order",
    )

    # Verify every row hit target against the engine before dragging. A press
    # landing one row off would otherwise produce a plausible-looking but wrong
    # committed order with no diagnostic. The last project is active first, so
    # every row click below is an observable transition rather than a no-op.
    launch.client.focus(beta_tab)
    _wait_until(
        lambda: int(launch.client.identify()["active_project_id"]) == beta,
        "last project active before the row hit-target sweep",
    )
    for index, project_id in enumerate(ordered):
        x, y = _sidebar_row_center(launch, index)
        _click_window_control(launch, x, y)
        _wait_until(
            lambda project_id=project_id: int(
                launch.client.identify()["active_project_id"]
            )
            == project_id,
            f"sidebar project row {index} hit target selects project {project_id}",
        )
    _assert_stays(
        lambda: _project_ids(launch) == ordered,
        "sub-threshold row hit-target clicks leave the project order untouched",
    )

    tabs = launch.client.project_tab_ids(home_project) + [alpha_tab, beta_tab]
    _drain_tabs(launch, tabs)

    source_x, source_y = _sidebar_row_center(launch, 0)
    second_y = _sidebar_row_center(launch, 1)[1]
    third_y = _sidebar_row_center(launch, 2)[1]
    # The strip's target index counts row centers at or above the pointer, so
    # the midpoint between the second and third centers is the insertion
    # boundary directly below the second row with maximum margin either side.
    target_y = (second_y + third_y) // 2
    expected = [alpha, home_project, beta]
    pending_error: BaseException | None = None
    try:
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(source_x), str(source_y)]
        )
        launch.terminal_pointer(["mousedown", "1"])
        # The strip publishes its selection on press. That activation is the
        # IPC-observable causal fence proving Iced consumed the press at the
        # intended stable ID before the first trajectory sample moves on.
        _wait_until(
            lambda: int(launch.client.identify()["active_project_id"]) == home_project,
            "project drag press selects its stable ID",
        )
        # Sample the trajectory in separate XTEST submissions like the tab
        # drag: a single batched motion can be coalesced past the 8 px
        # threshold and the preview it is supposed to produce.
        for step in range(1, 9):
            y = round(source_y + (target_y - source_y) * step / 8)
            launch.terminal_pointer(
                ["mousemove", "--window", launch.window, str(source_x), str(y)]
            )
    except BaseException as error:
        pending_error = error
        raise
    finally:
        try:
            launch.terminal_pointer(["mouseup", "1"])
        except Exception as release_error:
            if pending_error is None:
                raise
            print(
                f"failed to release project drag while handling {pending_error!r}: "
                f"{release_error}",
                file=sys.stderr,
            )
    _wait_until(
        lambda: _project_ids(launch) == expected,
        "project drag authoritative reorder",
    )
    _assert_stays(
        lambda: _project_ids(launch) == expected,
        "committed project drag order",
    )
    assert launch.process.poll() is None, "Iced exited during the project drag"

    # A press/release below the 8 px threshold is an ordinary selection: it
    # must never publish a reorder.
    click_x, click_y = _sidebar_row_center(launch, 2)
    pending_error = None
    try:
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(click_x), str(click_y)]
        )
        launch.terminal_pointer(["mousedown", "1"])
        launch.terminal_pointer(
            [
                "mousemove",
                "--window",
                launch.window,
                str(click_x + 3),
                str(click_y + 2),
            ]
        )
    except BaseException as error:
        pending_error = error
        raise
    finally:
        try:
            launch.terminal_pointer(["mouseup", "1"])
        except Exception as release_error:
            if pending_error is None:
                raise
            print(
                f"failed to release sub-threshold project click while handling "
                f"{pending_error!r}: {release_error}",
                file=sys.stderr,
            )
    _wait_until(
        lambda: int(launch.client.identify()["active_project_id"]) == beta,
        "sub-threshold project click selection",
    )
    _assert_stays(
        lambda: _project_ids(launch) == expected,
        "sub-threshold project click does not reorder",
    )
    _assert_no_pty_input(launch, tabs, "project drag and sub-threshold click")

    # Close Project confirms first. Escape must cancel it, and the held key's
    # repeats and release must be latched instead of reaching any PTY, so the
    # drain deliberately happens before the shortcut that opens the dialog.
    _drain_tabs(launch, tabs)
    launch.key("alt+shift+w")
    _wait_confirm_delete_open(launch, "cancel-project-delete")
    _wait_until(
        lambda: not launch.client.app_active_terminal_focused(),
        "delete confirm owns the keyboard route",
    )
    _hold_plain_key(launch, "Escape")
    _wait_confirm_delete_closed(launch, "cancel-project-delete")
    _wait_until(
        launch.client.app_active_terminal_focused,
        "terminal keyboard route resumes after delete confirm cancel",
    )
    _assert_stays(
        lambda: launch.client.project(beta) is not None,
        "Escape cancels the project delete confirm",
    )
    _assert_no_pty_input(launch, tabs, "cancelled project delete confirm")

    survivors = [tab_id for tab_id in tabs if tab_id != beta_tab]
    _drain_tabs(launch, tabs)
    launch.key("alt+shift+w")
    _wait_confirm_delete_open(launch, "confirm-project-delete")
    _hold_plain_key(launch, "Return")
    _wait_until(
        lambda: launch.client.project(beta) is None,
        "held Enter confirms the project delete",
    )
    _wait_confirm_delete_closed(launch, "confirm-project-delete")
    # The engine's fallback is the lowest remaining project ID, which after the
    # drag above is deliberately not the first sidebar row.
    _wait_until(
        lambda: int(launch.client.identify()["active_project_id"]) == home_project,
        "lowest-remaining-ID project fallback after the confirmed delete",
    )
    _assert_no_pty_input(launch, survivors, "confirmed project delete")

    launch.client.delete_project(alpha)
    launch.client.focus(home_tab)
    _wait_until(
        lambda: _project_ids(launch) == baseline
        and int(launch.client.identify()["active_tab_id"]) == home_tab,
        "project lifecycle fixture cleanup",
    )


def _sidebar_resize_grip(launch: Launch) -> None:
    """Drag the sidebar/terminal seam through SidebarResizeGrip's real
    pointer path (crates/roost-iced/src/sidebar_resize.rs).

    The grip's hit zone straddles the seam at `sidebar_width` +/- 3px and
    claims a press inside it outright (`Ownership::Own`) before the content
    ever sees the event, so a press landing there must never start a
    terminal selection. `dragged_width` anchors the new width on the
    press-time width plus the pointer's travel from the press x
    (`start_width + (x - start_x)`), so pressing exactly on the seam and
    moving +60px lands at seed_width + 60.
    """
    tolerance = 2.0

    def width_within(expected: float) -> bool:
        current = float(launch.client.window_metrics()["sidebar_width"])
        return abs(current - expected) <= tolerance

    metrics = launch.client.window_metrics()
    starting_collapsed = bool(metrics["sidebar_collapsed"])
    if starting_collapsed:
        # Collapsing drops the grip from the widget tree (sidebar_resize.rs
        # module doc); expand via the same keybind the chrome-overflow
        # segment already uses so the grip exists to drag.
        launch.key("alt+b")
        _wait_until(
            lambda: not launch.client.window_metrics()["sidebar_collapsed"],
            "sidebar expand before resize-grip drag",
        )
        metrics = launch.client.window_metrics()
    starting_width = float(metrics["sidebar_width"])
    assert starting_width > 0, (
        "resize-grip drag expects a laid-out sidebar; "
        f"metrics={metrics!r}"
    )

    seed_width = 220.0
    launch.client.sidebar_set_width(seed_width)
    _wait_until(
        lambda: width_within(seed_width), "seeded sidebar width before resize-grip drag"
    )

    launch.client.selection_clear(launch.tab)
    baseline_selection = launch.client.selection_dump(launch.tab)
    assert not baseline_selection.get("text"), baseline_selection
    _drain_tabs(launch, [launch.tab])

    delta = 60.0
    x0 = round(seed_width)
    target_x = round(seed_width + delta)
    expected_width = seed_width + delta
    y = round(float(launch.client.window_metrics()["window_height"]) / 2)

    pending_error: BaseException | None = None
    try:
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(x0), str(y)]
        )
        launch.terminal_pointer(["mousedown", "1"])
        # `mouse::Event::ButtonPressed` carries no position, so iced hit-tests
        # a press against the newest cursor position in the event batch it is
        # drained with — not the position the button actually went down at.
        # A UI running a frame behind (routine on a loaded CI box) drains the
        # press together with the first drag move and evaluates it 8px away,
        # outside the grip's 6px zone, so no drag ever starts and the wait
        # below times out with the sidebar untouched. Let the press drain on
        # its own before the pointer moves again.
        time.sleep(0.5 * SCALE)
        # Separate XTEST submissions per sample, like the tab/project drags
        # above: a single batched xdotool motion can be coalesced past the
        # grip's move handling.
        for step in range(1, 9):
            x = round(x0 + (target_x - x0) * step / 8)
            launch.terminal_pointer(
                ["mousemove", "--window", launch.window, str(x), str(y)]
            )
        # The live drag overlay (`sidebar_drag_width` in app.rs) is
        # IPC-observable before release; fence on it the same way the tab
        # drags fence on their rendered preview.
        _wait_until(
            lambda: width_within(expected_width),
            "held resize-grip drag reaches the target width",
        )
    except BaseException as error:
        pending_error = error
        raise
    finally:
        try:
            launch.terminal_pointer(["mouseup", "1"])
        except Exception as release_error:
            if pending_error is None:
                raise
            print(
                f"failed to release resize-grip drag while handling {pending_error!r}: "
                f"{release_error}",
                file=sys.stderr,
            )

    _wait_until(lambda: width_within(expected_width), "resize-grip drag committed width")
    _assert_stays(
        lambda: width_within(expected_width), "committed resize-grip drag width"
    )
    assert launch.process.poll() is None, "Iced exited during the resize-grip drag"

    # The hit-zone claim must beat TerminalWidget: no selection may exist
    # after a drag that started and ended inside the grip's zone.
    selection = launch.client.selection_dump(launch.tab)
    assert not selection.get("text"), selection
    _assert_no_pty_input(launch, [launch.tab], "sidebar resize-grip drag")

    # Relaunch persistence of the committed width is covered by
    # tools/roosttest/test_sidebar_resize.py; this segment only proves the
    # real pointer path, so it stays lean and restores the fixture.
    # Never drive sidebar.set_width concurrent with a live drag (the seam
    # drag re-anchors on its press-time width and its release wins,
    # docs/reference/ipc.md sidebar.set_width) — the drag above has already
    # released and settled by this point.
    launch.client.sidebar_set_width(starting_width)
    _wait_until(
        lambda: width_within(starting_width), "resize-grip fixture width restored"
    )
    if starting_collapsed:
        launch.key("alt+b")
        _wait_until(
            lambda: launch.client.window_metrics()["sidebar_collapsed"],
            "resize-grip fixture collapse restored",
        )


def _direct_tab_close(launch: Launch) -> None:
    identity = launch.client.identify()
    project = int(identity["active_project_id"])
    sibling = launch.tab
    launch.client.set_title(sibling, "SIBLING")
    doomed = launch.client.open_tab(project, title="DOOMED")
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == doomed,
        "new tab to become the rendered active pill",
    )
    _wait_until(lambda: _tab_is_attached(launch, doomed), "new tab PTY attachment")

    x, y = _active_pill_close_point(launch)
    _click_window_control(launch, x, y)
    _wait_until(lambda: launch.client.tab(doomed) is None, "exact clicked tab removal")
    assert launch.client.identify()["active_tab_id"] == sibling

    # Earlier paste and scrollback checks deliberately leave bytes in the
    # shell's editable line. Cancel that line and observe a survivor repaint
    # before issuing the marker; otherwise the command is appended to fixture
    # residue and its visibility depends on the renderer's live column count.
    before_interrupt = launch.client.dump(sibling).get("rows_text", [])
    launch.client.send(sibling, b"\x03")
    _wait_until(
        lambda: launch.client.dump(sibling).get("rows_text", []) != before_interrupt,
        "fallback sibling PTY to repaint after line cancellation",
    )
    marker = f"close-survivor-{uuid.uuid4().hex[:8]}"
    launch.client.send(sibling, f"printf '%s\\n' '{marker}'\n")
    _wait_until(
        lambda: marker in "\n".join(launch.client.dump(sibling).get("rows_text", [])),
        "fallback sibling PTY to remain writable",
    )

    last_project = launch.client.create_project("LAST-CASCADE", "/tmp")
    last = launch.client.open_tab(last_project, "/tmp", "LAST")
    launch.client.focus(last)
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == last,
        "last-tab project to become active",
    )
    _wait_until(lambda: _tab_is_attached(launch, last), "last-tab PTY attachment")
    x, y = _active_pill_close_point(launch)
    _click_window_control(launch, x, y)
    _wait_until(
        lambda: launch.client.project(last_project) is None,
        "last-tab close to cascade-delete its project",
    )
    assert launch.client.identify()["active_tab_id"] == sibling


def _chrome_overflow_navigation(launch: Launch) -> None:
    """Constrain both scroll regions while fixed controls remain reachable."""
    launch.client.window_resize(640, 360)
    width, height = _wait_product_extent(launch, 640, 360)
    identity = launch.client.identify()
    home_project = int(identity["active_project_id"])
    home_tab = int(identity["active_tab_id"])

    edge_project = launch.client.create_project("LEADING-EDGE", "/tmp")
    edge_tab = launch.client.open_tab(
        edge_project,
        "/tmp",
        "leading-edge",
        argv=["/bin/cat"],
    )
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == edge_project,
        "edge project to become the rendered active row",
    )
    _wait_until(lambda: _tab_is_attached(launch, edge_tab), "edge project PTY attachment")
    report = launch.client.agent_report(
        edge_tab,
        "claude",
        "claim",
        session_id="x11-rollup-stripe",
        lifecycle="waiting",
    )
    assert report.get("accepted"), report
    _wait_until(
        lambda: launch.client.agent_lifecycle(edge_tab) == "waiting",
        "edge project waiting-agent rollup",
    )
    launch.client.focus(home_tab)
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == home_project,
        "home project before leading-edge project click",
    )
    x, y = _stable_rollup_stripe_point(launch)
    _click_window_control(launch, x, y)
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == edge_project,
        "project selection through the rollup stripe hit target",
    )
    launch.client.focus(home_tab)
    launch.client.delete_project(edge_project)
    _wait_until(
        lambda: launch.client.project(edge_project) is None
        and launch.client.identify()["active_tab_id"] == home_tab,
        "rollup stripe fixture cleanup",
    )

    overflow_tabs = [
        launch.client.open_tab(home_project, title=f"LONG-TAB-{index:02d}-OVERFLOW")
        for index in range(12)
    ]
    last_tab = overflow_tabs[-1]
    launch.client.focus(last_tab)
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == last_tab,
        "offscreen active tab selection",
    )
    metrics = launch.client.window_metrics()
    sidebar = round(float(metrics["sidebar_width"]))
    terminal_top = round(launch.client.terminal_top(metrics))
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(sidebar + 120),
            str(terminal_top // 2),
            "click",
            "--repeat",
            "35",
            "--delay",
            "15",
            # X11 buttons 6/7 are horizontal wheel left/right. A vertical
            # wheel event is intentionally owned by the terminal/sidebar,
            # while the tab strip consumes horizontal deltas only.
            "7",
        ]
    )
    x, y = _active_pill_close_point(launch, (width, height))
    _click_window_control(launch, x, y)
    _wait_until(lambda: launch.client.tab(last_tab) is None, "scrolled active tab close")

    before = len(launch.client.project_tab_ids(home_project))
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(width - 49),
            str(terminal_top // 2),
            "click",
            "1",
        ]
    )
    _wait_until(
        lambda: len(launch.client.project_tab_ids(home_project)) == before + 1,
        "fixed add-tab control outside horizontal overflow",
    )
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            str(width - 20),
            str(terminal_top // 2),
            "click",
            "1",
        ]
    )
    _wait_until(
        lambda: launch.client.palette_state().get("frame") == "notifications",
        "fixed notification control outside horizontal overflow",
    )
    launch.client.palette_dismiss()

    last_project = 0
    last_project_tab = 0
    for index in range(14):
        last_project = launch.client.create_project(f"SCROLL-{index:02d}", "/tmp")
        last_project_tab = launch.client.open_tab(
            last_project, "/tmp", f"scroll-{index:02d}"
        )
    launch.client.focus(home_tab)
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == home_project,
        "home project before sidebar scroll",
    )
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            "110",
            str(height // 2),
            "click",
            "--repeat",
            "40",
            "--delay",
            "15",
            "5",
        ]
    )
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            "110",
            str(height - 52),
            "click",
            "1",
        ]
    )
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == last_project_tab,
        "last sidebar row activation after vertical scroll",
    )

    # The fixed footer is now "+ New Project" (plan 010): clicking it after
    # a body scroll must still hit the footer, not a scrolled row.
    before_footer_click = set(_project_ids(launch))
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            "110",
            str(height - 17),
            "click",
            "1",
        ]
    )
    _wait_until(
        lambda: set(_project_ids(launch)) - before_footer_click,
        "fixed sidebar footer creates a project after body scroll",
    )
    footer_created = set(_project_ids(launch)) - before_footer_click
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] in footer_created,
        "footer-created project activation",
    )
    for project_id in footer_created:
        launch.client.delete_project(project_id)
    launch.client.focus(last_project_tab)
    _wait_until(
        lambda: launch.client.identify()["active_tab_id"] == last_project_tab,
        "focus restored after footer-created project cleanup",
    )

    # The sidebar has no pointer collapse control (the header « was removed
    # after user testing; parity with Mac, where collapse is keybind/menu
    # only) — collapse via the ToggleSidebar default so the collapsed-state
    # ☰ restore control below is still exercised by a real click.
    launch.key("alt+b")
    _wait_until(
        lambda: launch.client.window_metrics()["sidebar_collapsed"],
        "keybind sidebar collapse after body scroll",
    )
    assert launch.client.identify()["active_tab_id"] == last_project_tab
    launch.terminal_pointer(
        [
            "mousemove",
            "--window",
            launch.window,
            "20",
            str(terminal_top // 2),
            "click",
            "1",
        ]
    )
    _wait_until(
        lambda: not launch.client.window_metrics()["sidebar_collapsed"],
        "fixed collapsed-sidebar control",
    )


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


def _terminal_scrollback_routing(launch: Launch) -> None:
    """Drive all three shared terminal-wheel routes through native XTEST."""
    rows = "".join(f"wheel-history-{index:02}\r\n" for index in range(72))
    launch.client.tab_feed_pty_bytes(
        launch.tab, b"\x1b[?1000l\x1b[?1006l\x1b[?1049l\x1b[2J\x1b[H" + rows.encode()
    )
    _wait_until(
        lambda: "wheel-history-71"
        in "\n".join(launch.client.dump(launch.tab).get("rows_text", [])),
        "terminal history fixture at live bottom",
    )
    terminal_x = 220 + 12 + 4
    terminal_y = round(launch.client.terminal_top()) + 12 + launch.cell_height // 2

    def wheel(button: int) -> None:
        launch.terminal_pointer(
            [
                "mousemove",
                "--window",
                launch.window,
                str(terminal_x),
                str(terminal_y),
                "click",
                str(button),
            ]
        )

    wheel(4)
    _wait_until(
        lambda: "wheel-history-71"
        not in "\n".join(launch.client.dump(launch.tab).get("rows_text", [])),
        "physical wheel reveals local terminal history",
    )
    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    launch.key("x")
    _wait_until(
        lambda: "wheel-history-71"
        in "\n".join(launch.client.dump(launch.tab).get("rows_text", [])),
        "terminal key snaps scrolled viewport to live bottom",
    )
    _wait_until(lambda: _capture_contains(launch, b"x"), "post-scroll terminal key bytes")

    launch.client.tab_feed_pty_bytes(launch.tab, b"\x1b[?1000h\x1b[?1006h")
    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    wheel(4)
    _wait_until(
        lambda: _capture_contains(launch, b"\x1b[<64;"),
        "physical wheel-up terminal mouse report",
    )
    wheel(5)
    _wait_until(
        lambda: _capture_contains(launch, b"\x1b[<65;"),
        "physical wheel-down terminal mouse report",
    )

    launch.client.tab_feed_pty_bytes(
        launch.tab, b"\x1b[?1000l\x1b[?1006l\x1b[?1049h"
    )
    launch.client.tab_capture_pty_input(launch.tab, drain=True)
    wheel(4)
    _wait_until(lambda: _capture_contains(launch, b"\x1b[A"), "alt-screen wheel-up key")
    wheel(5)
    _wait_until(lambda: _capture_contains(launch, b"\x1b[B"), "alt-screen wheel-down key")
    launch.client.tab_feed_pty_bytes(
        launch.tab, b"\x1b[?1049l\x1b[?1000l\x1b[?1006l\x1b[2J\x1b[H"
    )


def _drag_copy_and_middle_paste(launch: Launch) -> None:
    marker = f"drag-{uuid.uuid4().hex[:8]}"
    _set_row(launch, marker)
    baseline = f"baseline-{uuid.uuid4().hex[:8]}"
    launch.client.clipboard_write("system", baseline)
    launch.client.clipboard_write("selection", baseline)

    # Window-relative client coordinates: sidebar + terminal padding and the
    # live application-owned terminal origin. End on the last marker cell because
    # TerminalSelection's committed range is inclusive at pointer release.
    x0 = 220 + 12 + launch.cell_width // 2
    x1 = 220 + 12 + int((len(marker) - 0.5) * launch.cell_width)
    y = round(launch.client.terminal_top()) + 12 + launch.cell_height // 2
    # Keep the press, motion, and release as separate XTEST submissions. The
    # tiny-skia event loop can process a single batched xdotool sequence only
    # after its release, coalescing away the drag motion. IPC observation while
    # the button is still held is the synchronization fence; sleeps in
    # terminal_pointer are not the correctness mechanism.
    launch.terminal_pointer(
        ["mousemove", "--window", launch.window, str(x0), str(y)]
    )
    selection: dict = {}

    def held_drag_is_observed() -> bool:
        nonlocal selection
        selection = launch.client.selection_dump(launch.tab)
        return (
            selection.get("text") == marker
            and launch.client.clipboard_dump("system") == baseline
            and launch.client.clipboard_dump("selection") == baseline
        )

    pending_error: BaseException | None = None
    try:
        # Guard the injection itself: xdotool can press successfully and then
        # fail or be interrupted before terminal_pointer's trailing delay.
        # An unmatched XTEST mouseup is harmless if the press never landed.
        launch.terminal_pointer(["mousedown", "1"])
        midpoint = (x0 + x1) // 2
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(midpoint), str(y)]
        )
        launch.terminal_pointer(
            ["mousemove", "--window", launch.window, str(x1), str(y)]
        )
        try:
            _wait_until(
                held_drag_is_observed,
                f"held drag selection {marker!r}",
                timeout=5,
            )
        except AssertionError as error:
            raise AssertionError(
                f"held drag did not select {marker!r}; selection={selection!r}"
            ) from error
    except BaseException as error:
        pending_error = error
        raise
    finally:
        try:
            launch.terminal_pointer(["mouseup", "1"])
        except Exception as release_error:
            if pending_error is None:
                raise
            print(
                f"failed to release held drag while handling {pending_error!r}: "
                f"{release_error}",
                file=sys.stderr,
            )

    def committed_selection_is_exact() -> bool:
        nonlocal selection
        selection = launch.client.selection_dump(launch.tab)
        return selection.get("text") == marker

    try:
        _wait_until(
            committed_selection_is_exact,
            f"committed drag selection {marker!r}",
            timeout=5,
        )
    except AssertionError as error:
        raise AssertionError(
            f"released drag changed {marker!r}; selection={selection!r}"
        ) from error
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
    x = 220 + 12 + int(2.5 * launch.cell_width)
    y = round(launch.client.terminal_top()) + 12 + launch.cell_height // 2
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
    url_x = 220 + 12 + int(8.5 * launch.cell_width)
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
        _terminal_scrollback_routing(off)
        renamed_project, renamed_tab = _keybind_dispatch(off)
        dragged_titles = _direct_tab_reorder(off, renamed_project, renamed_tab)
        _project_lifecycle(off, renamed_project, renamed_tab)
        _sidebar_resize_grip(off)
        off.close()
        launches.pop()

        # The same harness-owned profile must restore the authoritative names,
        # not merely keep an adapter-local label alive until process exit.
        off = Launch(root, display, "off", "explicit")
        launches.append(off)
        restored = off.client.project(renamed_project)
        assert restored["name"] == "RENAMED-PROJECT"
        assert any(
            tab["title"] == "RENAMED-TAB" and tab["user_titled"]
            for tab in restored["tabs"]
        ), (renamed_tab, restored["tabs"])
        restored_titles = [tab["title"] for tab in restored["tabs"]]
        assert restored_titles == dragged_titles, (restored_titles, dragged_titles)
        restored_home = next(
            int(tab["id"]) for tab in restored["tabs"] if tab["title"] == "RENAMED-TAB"
        )
        for tab_id in off.client.project_tab_ids(renamed_project):
            if tab_id != restored_home:
                off.client.close_tab(tab_id)
        _wait_until(
            lambda: off.client.project_tab_ids(renamed_project) == [restored_home],
            "persisted direct-drag fixture cleanup",
        )
        _direct_tab_close(off)
        _chrome_overflow_navigation(off)
        _palette_pointer_routing(off)
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
        "local/tracked/alternate terminal wheel routing and key snap, "
        "exhaustive shortcut dispatch/repeat suppression, "
        "project/tab inline rename with double-click and click-away, "
        "stable-ID direct tab drag in both directions with persistence, "
        "sidebar project drag reorder with sub-threshold select, "
        "sidebar resize-grip drag with no selection leak or PTY leak, "
        "keybind-opened delete confirm cancelled and confirmed without PTY leaks, "
        "link hover cursor composition, exact-ID tab close/fallback/cascade, "
        "constrained chrome overflow navigation, and palette pointer routing"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
