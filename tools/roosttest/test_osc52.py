"""End-to-end test for OSC 52 program-initiated clipboard writes.

Each test runs a `printf '\\033]52;c;<base64>\\007'` command in the
shell so the OSC 52 bytes flow out the PTY's *output* side, where the
terminal's scanner reads. `tab.write(raw_osc_bytes)` would write to
the PTY's *input* side — bash would see them as keystrokes and no
OSC parse would happen.

Uses `clipboard.write` + `clipboard.dump` (PR #151) to first seed a
known-different baseline value, then verify the OSC 52 payload
actually replaced it — a prior matching clipboard value would
otherwise produce a false pass.

Run against either UI:

    pytest -q tools/roosttest/test_osc52.py --roost-target mac
    pytest -q tools/roosttest/test_osc52.py --roost-target gtk
"""

from __future__ import annotations

import base64
import sys
import uuid

import pytest


def _emit_osc52_command(target: str, text: str) -> str:
    """Build a shell command that PRINTS an OSC 52 sequence on stdout
    (where the terminal's scanner will see it). The bytes have to come
    from a program's output, not from user-typed input — `tab.write`
    feeds the PTY's input side, which bash interprets as keystrokes."""
    payload = base64.b64encode(text.encode()).decode()
    # printf '%b' so the \x1b and \x07 escapes are interpreted.
    return f"printf '\\033]52;{target};{payload}\\007'"


def _seed_baseline(roost, target: str) -> str:
    """Write a unique baseline string to the clipboard. The OSC 52
    test must then OVERWRITE this — without the baseline, an existing
    matching clipboard value could pass the assertion without OSC 52
    actually doing anything."""
    baseline = f"baseline-{uuid.uuid4().hex[:8]}"
    roost.clipboard_write(target, baseline)
    assert roost.clipboard_dump(target) == baseline, \
        "baseline write didn't take — clipboard.write may be broken"
    return baseline


def _wait_clipboard(roost, target: str, expected: str, timeout: float = 5.0) -> None:
    """OSC parse on the UI side is async (bytes → libghostty +
    scanner → idle-dispatched event → clipboard write). Poll
    `clipboard.dump` until the value updates or we timeout."""
    roost._wait(
        lambda: roost.clipboard_dump(target) == expected,
        timeout,
        f"clipboard[{target}] == {expected!r}",
    )


def test_osc52_writes_system_clipboard(roost, project):
    tab = roost.open_tab(project, cwd="/tmp", title="osc52")
    baseline = _seed_baseline(roost, "system")
    payload = f"osc52-{uuid.uuid4().hex[:8]}"
    roost.run(tab, _emit_osc52_command("c", payload))
    _wait_clipboard(roost, "system", payload)
    assert roost.clipboard_dump("system") != baseline


def test_osc52_writes_selection_clipboard(roost, project, target):
    # The GTK selection clipboard maps to the X11/Wayland PRIMARY
    # selection (`clipboard::Target::Primary` in roost-linux is
    # `#[cfg(target_os = "linux")]`-gated to no-op off Linux). The
    # macOS GTK dev build therefore has no PRIMARY backing: write is
    # a no-op, dump returns None, and the test fails before the OSC
    # 52 path even runs. Skip on that profile only; on real Linux
    # CI (e2e-gtk) and on `--roost-target mac` (named NSPasteboard)
    # the test runs and exercises the real PRIMARY path.
    if target == "gtk" and sys.platform == "darwin":
        pytest.skip(
            "GTK selection clipboard (X11/Wayland PRIMARY) is Linux-only; "
            "macOS GTK dev build has no PRIMARY. System clipboard covered "
            "by test_osc52_writes_system_clipboard. Real GTK on Linux runs "
            "this in e2e-gtk CI."
        )
    tab = roost.open_tab(project, cwd="/tmp", title="osc52-sel")
    baseline = _seed_baseline(roost, "selection")
    payload = f"osc52-sel-{uuid.uuid4().hex[:8]}"
    roost.run(tab, _emit_osc52_command("p", payload))
    _wait_clipboard(roost, "selection", payload)
    assert roost.clipboard_dump("selection") != baseline


def test_osc52_read_request_does_not_clobber_clipboard(roost, project):
    """`Pc == "?"` is a read request — phase 1 drops it silently.
    The clipboard must NOT change as a side effect of the parse."""
    tab = roost.open_tab(project, cwd="/tmp", title="osc52-read")
    baseline = _seed_baseline(roost, "system")
    # Print a read-request OSC 52 — the program output side, not user
    # input — so the terminal's OSC scanner sees it.
    roost.run(tab, "printf '\\033]52;c;?\\007'")
    # Give the UI a chance to process the bytes; a `tab.dump` round-trip
    # forces a main-loop tick.
    roost.dump(tab)
    assert roost.clipboard_dump("system") == baseline
