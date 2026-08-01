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
sys.path.insert(0, str(REPO / "tools" / "screenshot"))

import pngtool  # noqa: E402
from client import RoostError  # noqa: E402

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


def _active_pill_close_point(
    launch: Launch, expected_extent: tuple[int, int] | None = None
) -> tuple[int, int]:
    """Locate the rendered active pill, then target its trailing close sibling."""
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
            return right - 11, terminal_top // 2
        previous = bounds
        time.sleep(0.1)
    raise AssertionError(f"active tab pill never settled; last bounds: {previous!r}")


def _active_project_leading_point(
    launch: Launch, excluded_y: int | None = None
) -> tuple[int, int]:
    """Locate the active project row, then target its one-pixel leading edge."""
    path = launch.root / "chrome-project-leading-edge.png"
    active = (0x13, 0x50, 0x9D)
    for _attempt in range(50):
        png, _width, _height = launch.client.screenshot(scale=1)
        path.write_bytes(png)
        width, height, bpp, data = pngtool.load(str(path))
        sidebar = round(float(launch.client.window_metrics()["sidebar_width"]))
        points: list[tuple[int, int]] = []
        for y in range(height):
            for x in range(min(sidebar, width)):
                offset = (y * width + x) * bpp
                if tuple(data[offset : offset + 3]) == active:
                    points.append((x, y))
        if points:
            xs, ys = zip(*points)
            bounds = min(xs), min(ys), max(xs), max(ys)
            if bounds[2] - bounds[0] >= 150 and bounds[3] - bounds[1] >= 20:
                center_y = (bounds[1] + bounds[3]) // 2
                if center_y != excluded_y:
                    # This is inside the stripe/gap region (x<14) that used
                    # to be outside the project row's hit target.
                    return 12, center_y
        time.sleep(0.1)
    raise AssertionError(f"active project selection never settled: {path}")


def _click_window_control(launch: Launch, x: int, y: int) -> None:
    """Fence move/press/release so tiny-skia cannot batch away a button edge."""
    launch.terminal_pointer(
        ["mousemove", "--window", launch.window, str(x), str(y)]
    )
    launch.terminal_pointer(["mousedown", "1"])
    launch.terminal_pointer(["mouseup", "1"])


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
    edge_tab = launch.client.open_tab(edge_project, "/tmp", "leading-edge")
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == edge_project,
        "edge project to become the rendered active row",
    )
    _wait_until(lambda: _tab_is_attached(launch, edge_tab), "edge project PTY attachment")
    x, y = _active_project_leading_point(launch)
    launch.client.focus(home_tab)
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == home_project,
        "home project before leading-edge project click",
    )
    _active_project_leading_point(launch, excluded_y=y)
    _click_window_control(launch, x, y)
    _wait_until(
        lambda: launch.client.identify()["active_project_id"] == edge_project,
        "project selection through the rollup stripe hit target",
    )
    launch.client.focus(home_tab)

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
        lambda: launch.client.window_metrics()["sidebar_collapsed"],
        "fixed sidebar footer after body scroll",
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


def _drag_copy_and_middle_paste(launch: Launch) -> None:
    marker = f"drag-{uuid.uuid4().hex[:8]}"
    _set_row(launch, marker)
    baseline = f"baseline-{uuid.uuid4().hex[:8]}"
    launch.client.clipboard_write("system", baseline)
    launch.client.clipboard_write("selection", baseline)

    # Window-relative client coordinates: sidebar + terminal padding and the
    # live application-owned terminal origin. End on the last marker cell because
    # TerminalSelection's committed range is inclusive at pointer release.
    x0 = 220 + 12 + 4
    x1 = 220 + 12 + int((len(marker) - 0.5) * 8.4)
    y = round(launch.client.terminal_top()) + 12 + 9
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
    x = 220 + 12 + int(2.5 * 8.4)
    y = round(launch.client.terminal_top()) + 12 + 9
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
        _direct_tab_close(off)
        _chrome_overflow_navigation(off)
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
        "link hover cursor composition, exact-ID tab close/fallback/cascade, "
        "and constrained chrome overflow navigation"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
