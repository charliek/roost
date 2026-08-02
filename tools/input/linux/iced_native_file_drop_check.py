#!/usr/bin/env python3
"""Exercise the native X11 file-drop path into Iced.

The source window is a separate GTK application solely because GTK exposes a
small, reliable XDND source in the shed image. GTK is not loaded by
``roost-iced`` and this check does not weaken the Cargo dependency-boundary
gate. Wayland is intentionally not covered: Iced 0.14 does not currently emit
native file-drop events on that backend.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
os.environ.setdefault("ROOST_ICED_BIN", "/home/shed/rt/debug/roost-iced")
sys.path.insert(0, str(REPO / "tools" / "input" / "linux"))

from iced_clipboard_check import Launch, _free_display, _wait_until  # noqa: E402


def _run_source(path: Path) -> int:
    import gi

    gi.require_version("Gtk", "4.0")
    from gi.repository import Gdk, Gio, Gtk

    app = Gtk.Application(application_id="com.github.charliek.roost.drop-source")

    def activate(application: Gtk.Application) -> None:
        window = Gtk.ApplicationWindow(application=application)
        window.set_title("Roost Native Drop Source")
        window.set_default_size(360, 180)

        label = Gtk.Label(label=f"Drag this file into Roost\n{path.name}")
        source = Gtk.DragSource()
        source.set_actions(Gdk.DragAction.COPY)

        def prepare(
            _source: Gtk.DragSource, _x: float, _y: float
        ) -> Gdk.ContentProvider:
            files = Gdk.FileList.new_from_list([Gio.File.new_for_path(str(path))])
            return Gdk.ContentProvider.new_for_value(files)

        source.connect("prepare", prepare)
        label.add_controller(source)
        window.set_child(label)
        window.present()

    app.connect("activate", activate)
    return app.run([])


def _window(display: str, title: str) -> str:
    env = {**os.environ, "DISPLAY": display}
    holder: list[str] = []

    def find() -> bool:
        result = subprocess.run(
            ["xdotool", "search", "--name", title],
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

    _wait_until(find, f"window titled {title!r}", timeout=15)
    return holder[-1]


def _capture_drop(launch: Launch, expected: bytes) -> bytes:
    captured = bytearray()

    def received() -> bool:
        captured.extend(
            launch.client.tab_capture_pty_input(launch.tab, drain=True)
        )
        return expected in captured

    _wait_until(received, f"native file-drop bytes {expected!r}", timeout=10)
    # Catch a delayed duplicate or trailing malformed frame rather than
    # declaring success as soon as the expected bytes first appear.
    time.sleep(0.5)
    captured.extend(launch.client.tab_capture_pty_input(launch.tab, drain=True))
    if bytes(captured) != expected:
        raise AssertionError(
            f"native file drop was not one exact bracketed frame: {bytes(captured)!r}"
        )
    return bytes(captured)


def _main() -> int:
    if len(sys.argv) == 3 and sys.argv[1] == "--source":
        return _run_source(Path(sys.argv[2]))

    display = _free_display()
    xenv = {**os.environ, "DISPLAY": display}
    xvfb = subprocess.Popen(
        ["Xvfb", display, "-screen", "0", "1280x800x24", "-nolisten", "tcp"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    source: subprocess.Popen[bytes] | None = None
    launch: Launch | None = None

    try:
        _wait_until(
            lambda: subprocess.run(
                ["xdpyinfo"], env=xenv, capture_output=True, check=False
            ).returncode
            == 0,
            f"X display {display}",
        )
        with tempfile.TemporaryDirectory(prefix="roost-iced-native-drop-") as raw:
            root = Path(raw)
            dropped = root / "Roost iced native drop.txt"
            dropped.touch()
            launch = Launch(root, display, "off", "iced")
            launch.client.tab_feed_pty_bytes(launch.tab, b"\x1b[?2004h")
            time.sleep(0.2)
            launch.client.tab_capture_pty_input(launch.tab, drain=True)

            source = subprocess.Popen(
                [sys.executable, str(Path(__file__)), "--source", str(dropped)],
                env=xenv,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
            source_window = _window(display, "Roost Native Drop Source")

            subprocess.run(
                [
                    "xdotool",
                    "windowmove",
                    source_window,
                    "20",
                    "250",
                    "windowmove",
                    launch.window,
                    "430",
                    "80",
                    "windowsize",
                    launch.window,
                    "800",
                    "680",
                    "mousemove",
                    "200",
                    "340",
                    "mousedown",
                    "1",
                    "sleep",
                    "0.5",
                    "mousemove",
                    "330",
                    "340",
                    "sleep",
                    "0.15",
                    "mousemove",
                    "470",
                    "360",
                    "sleep",
                    "0.15",
                    "mousemove",
                    "650",
                    "420",
                    "sleep",
                    "0.15",
                    "mousemove",
                    "900",
                    "560",
                    "sleep",
                    "0.5",
                    "mouseup",
                    "1",
                ],
                env=xenv,
                check=True,
            )

            escaped = str(dropped).replace(" ", "\\ ").encode()
            expected = b"\x1b[200~" + escaped + b"\x1b[201~"
            captured = _capture_drop(launch, expected)
            print(f"PASS: native X11 file drop captured {captured!r}")
        return 0
    finally:
        if launch is not None:
            launch.close()
        if source is not None:
            try:
                os.killpg(os.getpgid(source.pid), signal.SIGTERM)
                source.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(source.pid), signal.SIGKILL)
                source.wait()
        try:
            os.killpg(os.getpgid(xvfb.pid), signal.SIGTERM)
            xvfb.wait(timeout=5)
        except ProcessLookupError:
            pass
        except subprocess.TimeoutExpired:
            os.killpg(os.getpgid(xvfb.pid), signal.SIGKILL)
            xvfb.wait()


if __name__ == "__main__":
    raise SystemExit(_main())
