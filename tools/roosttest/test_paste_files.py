"""Pasting copied files (plan 073 D2, #576).

A paste that finds files on the system clipboard does what dropping them
on the tab does: a local tab gets their shell-escaped paths, one per
line. `clipboard.write_files` seeds the clipboard the way a file
manager's copy leaves it — on macOS with each file's *name* as the text,
which is why a paste there reads files before text — and the platform
paste key, pressed through `app.key_event`, runs the real paste.

Two oracles: what `tab.send_file` (the drop route as an op) types for the
same paths into a fresh tab, so nothing here re-implements the escaping;
and, for one simple path, the literal bytes.

Real files, compared by canonical path: the Linux writer canonicalizes
and drops a path that does not exist, and macOS's temp dirs live under
`/private`.

iced only, and only on the clipboard lanes (X11 and the macOS cell):
`clipboard.write_files` is an iced test seam, and headless Wayland has no
clipboard the harness can own.
"""

from __future__ import annotations

import os
import re
import sys
import uuid

import pytest

from util import drain, drain_until_match, wait_tab_attached

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

#: The paste shortcut as `app.key_event` spells it: ⌘V on macOS, and the
#: native-modifier default Alt+V everywhere else.
PASTE_KEY = ("v", ["super"] if sys.platform == "darwin" else ["alt"])

#: A child that never writes, so every captured byte is the paste's.
QUIET_ARGV = ["/bin/sh", "-c", "exec sleep 300"]

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="clipboard.write_files and app.key_event require ROOST_TEST_MODE=1",
)


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("clipboard.write_files is an iced test seam; Roost.app answers unknown-op")


def _quiet_tab(roost, project) -> int:
    tab = roost.open_tab(project, cwd="/tmp", title="paste-files", argv=QUIET_ARGV)
    wait_tab_attached(roost, tab)
    drain(roost, tab)
    return tab


def _send_file_bytes(roost, project, paths) -> bytes:
    """Oracle 1: what the drop route types for `paths`, in a fresh tab."""
    tab = _quiet_tab(roost, project)
    pasted = roost.tab_send_file(tab, paths)["pasted"]
    return drain_until_match(roost, tab, re.escape(pasted.encode()))


def _paste(roost, project, expected: bytes) -> bytes:
    """Press the paste key in a fresh, focused tab; what it was typed."""
    tab = _quiet_tab(roost, project)
    roost.focus(tab)
    roost._wait(
        lambda: roost.app_selected_tab_id() == tab,
        5.0,
        f"tab {tab} on screen, where the paste key lands",
    )
    roost.key_event(*PASTE_KEY)
    return drain_until_match(roost, tab, re.escape(expected))


def test_two_copied_files_paste_what_dropping_them_types(roost, project, tmp_path):
    base = tmp_path.resolve()
    spaced = base / "a b.txt"
    plain = base / "c.txt"
    spaced.write_text("a\n")
    plain.write_text("c\n")
    expected = _send_file_bytes(roost, project, [spaced, plain])

    roost.clipboard_write_files([spaced, plain])

    assert _paste(roost, project, expected) == expected


def test_a_copied_non_image_file_pastes_its_path(roost, project, tmp_path):
    """Any file type, not only images (Swift's paste filters to those)."""
    notes = tmp_path.resolve() / "notes.md"
    notes.write_text("# notes\n")
    literal = f"{notes.parent}/notes.md".encode()
    assert _send_file_bytes(roost, project, [notes]) == literal

    roost.clipboard_write_files([notes])

    assert _paste(roost, project, literal) == literal


def test_a_clipboard_with_no_files_pastes_its_text(roost, project):
    """`write_files([])` writes nothing, so the paste reads the text the
    clipboard already carried — on macOS, through the file step that
    found nothing."""
    text = f"plain-{uuid.uuid4().hex[:8]}"
    roost.clipboard_write("system", text)
    roost._wait(
        lambda: roost.clipboard_dump("system") == text,
        5.0,
        f"clipboard[system] == {text!r}",
    )

    roost.clipboard_write_files([])

    assert _paste(roost, project, text.encode()) == text.encode()
