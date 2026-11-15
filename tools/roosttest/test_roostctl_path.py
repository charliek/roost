"""A bare `roostctl` in a tab is the Roost's own (#261), read back from the
tab's screen.

Every tab gets `ROOST_ROOSTCTL`; when that `roostctl` sits in the app
bundle, the bundle's `Contents/Resources/bin` goes first on `PATH`. The
shared-UI case runs in every lane: on Linux and a bare macOS binary the
`PATH` a tab gets is the UI's own, unchanged; in the bundle lane
(`ROOST_ICED_APP`) the bundle's `bin` is first.

Linux also relaunches its UI from a fake app bundle (a `.app` directory
with the binaries in their bundle places, under a name with a space), so
the prepend, the strip of an enclosing Roost's `bin`, and `app.cli_link`'s
link, card and toast are exercised where CI runs most. macOS keeps the
bundle check to the real bundle: a bare binary under a fake `.app` would
look bundled to AppKit too.
"""

from __future__ import annotations

import contextlib
import os
import re
import shutil
import sys
import tempfile
import uuid
from pathlib import Path

import pytest
import ui
from client import Roost, RoostError

# Each value in 50-character pieces, `@@name+piece@@` per row, so no row
# wraps however narrow the window is.
PROBE = (
    "emit() { printf '%s\\n' \"$2\" | fold -w 50 | sed \"s/^/@@$1+/; s/\\$/@@/\"; }; "
    "emit which \"$(command -v roostctl || echo none)\"; "
    "emit env \"${ROOST_ROOSTCTL:-unset}\"; "
    "emit path \"$PATH\"; "
    "printf '@@done@@\\n'; "
    "exec sleep 600"
)

PIECE = re.compile(r"@@(\w+)\+(.*)@@$")


def is_roost_bundle_bin(entry: str) -> bool:
    """MIRRORS `roost_ipc::cli_link::is_roost_bundle_bin_dir`."""
    parts = Path(entry).parts
    return (
        len(parts) >= 4
        and parts[-3:] == ("Contents", "Resources", "bin")
        and len(parts[-4]) > 4
        and parts[-4].endswith(".app")
        and os.path.isfile(os.path.join(entry, "roostctl"))
    )


def probe(roost: Roost, project: int) -> dict[str, str]:
    """`command -v roostctl`, `$ROOST_ROOSTCTL` and `$PATH` as a tab sees
    them, with no startup file in between."""
    tab = roost.open_tab(project, cwd="/tmp", argv=["/bin/sh", "-c", PROBE])
    roost.wait_text(tab, "@@done@@", timeout=15.0)
    dumped = roost.dump(tab, scrollback=500)
    found: dict[str, str] = {}
    for row in dumped.get("scrollback_text", []) + dumped["rows_text"]:
        piece = PIECE.match(row.strip())
        if piece:
            found[piece[1]] = found.get(piece[1], "") + piece[2]
    found["first"] = found.get("path", "").split(":")[0]
    return found


def test_a_bare_roostctl_in_a_tab_is_this_roosts(roost, project):
    found = probe(roost, project)
    app = ui.iced_bundle_app()
    if app is not None:
        bundle_bin = os.path.join(os.path.realpath(app), "Contents", "Resources", "bin")
        roostctl = os.path.join(bundle_bin, "roostctl")
        assert found["first"] == bundle_bin, found
        assert found["which"] == roostctl, found
        assert found["env"] == roostctl, found
        return
    if ui.session_state_dir() is None:
        pytest.skip("a reused UI's own PATH is unknown to the harness")
    # Unchanged: the UI's PATH, minus a bundle `bin` an enclosing Roost put
    # there (a developer running this inside Roost.app).
    path = ":".join(e for e in os.environ.get("PATH", "").split(":") if not is_roost_bundle_bin(e))
    assert found["path"] == path, found
    expected = shutil.which("roostctl", path=path) or "none"
    assert found["which"] == expected, found
    sibling = ui.rust_binary_path("iced")[0].parent / "roostctl"
    if os.access(sibling, os.X_OK):
        assert found["env"] == os.path.realpath(sibling), found


# ---------------------------------------------------------------------------
# Linux: the UI relaunched from a fake app bundle
# ---------------------------------------------------------------------------


def place(source: Path, dest: Path) -> None:
    """`source` at `dest`, hard-linked when the filesystem allows: the
    binary has to run from the bundle path, and a symlink would resolve
    back out of it."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.link(os.path.realpath(source), dest)
    except OSError:
        shutil.copy2(os.path.realpath(source), dest)


class FakeBundle:
    def __init__(self, root: Path):
        self.root = root
        self.app = root / "Fake Roost.app"
        self.bin = self.app / "Contents" / "Resources" / "bin"
        self.roostctl = self.bin / "roostctl"
        self.exe = self.app / "Contents" / "MacOS" / "roost-iced"
        # An enclosing Roost's `bin`, and another app's that holds no
        # `roostctl`.
        self.outer_bin = root / "Outer.app" / "Contents" / "Resources" / "bin"
        self.docker_bin = root / "Docker.app" / "Contents" / "Resources" / "bin"
        self.links = root / "links"


@pytest.fixture(scope="module")
def fake_bundle(target):
    if sys.platform != "linux":
        pytest.skip("macOS checks the prepend against its real bundle")
    if ui.iced_bundle_app() is not None or ui.session_state_dir() is None:
        pytest.skip("relaunching needs a harness-owned bare-binary UI")
    if not ui.test_mode_active():
        pytest.skip("the link directories are redirected only under ROOST_TEST_MODE=1")
    binary, _ = ui.rust_binary_path(target)
    roostctl = binary.parent / "roostctl"
    if not os.access(roostctl, os.X_OK):
        pytest.skip(f"no roostctl beside {binary}")
    root = Path(tempfile.mkdtemp(prefix="roost-rp-", dir="/tmp"))
    world = FakeBundle(root)
    place(binary, world.exe)
    place(roostctl, world.roostctl)
    world.outer_bin.mkdir(parents=True)
    shutil.copy2(roostctl, world.outer_bin / "roostctl")
    world.docker_bin.mkdir(parents=True)
    path = f"{world.outer_bin}:/usr/bin:{world.docker_bin}:/bin"
    extra = {
        "PATH": path,
        "ROOST_ROOSTCTL": str(world.outer_bin / "roostctl"),
        "ROOST_TEST_CLI_LINK_DIRS": f"{world.links}/system:{world.links}/user",
    }
    saved = os.environ.get("ROOST_ICED_BIN")
    os.environ["ROOST_ICED_BIN"] = str(world.exe)
    try:
        ui.quit(target)
        ui.launch(target, force=True, extra_env=extra)
        yield world
    finally:
        if saved is None:
            os.environ.pop("ROOST_ICED_BIN", None)
        else:
            os.environ["ROOST_ICED_BIN"] = saved
        with contextlib.suppress(Exception):
            ui.quit(target)
        ui.launch(target, force=True)
        shutil.rmtree(root, ignore_errors=True)


@pytest.fixture
def bundled(target, fake_bundle):
    client = Roost(ui.socket_path(target))
    pid = client.create_project(name=f"rp-{uuid.uuid4().hex[:6]}", cwd="/tmp")
    try:
        yield client, pid
    finally:
        with contextlib.suppress(Exception):
            client.delete_project(pid)
        client.close()


def test_a_bundled_roostctls_bin_goes_first_and_an_outer_roosts_comes_out(fake_bundle, bundled):
    roost, project = bundled
    found = probe(roost, project)
    assert found["which"] == str(fake_bundle.roostctl), found
    assert found["env"] == str(fake_bundle.roostctl), found
    assert found["path"] == f"{fake_bundle.bin}:/usr/bin:{fake_bundle.docker_bin}:/bin", found


def dialog(roost: Roost) -> dict:
    return roost.call("app.dialog_dump", {})


def bottom_line(roost: Roost) -> str | None:
    line = roost.notice_dump()["bottom_line"]
    return line["text"] if line else None


def test_the_link_op_installs_shows_and_uninstalls(fake_bundle, bundled):
    roost, _ = bundled
    status = roost.call("app.cli_link", {"action": "status"})
    assert status["state"] == "absent", status
    assert status["label"] == "Install 'roostctl' Command", status
    assert status["bundled"] is True, status
    assert status["source"] == str(fake_bundle.roostctl), status

    # The system dir does not exist, so the link lands in the user dir,
    # which is not on the UI's PATH: a card with the line to add.
    user_link = fake_bundle.links / "user" / "roostctl"
    made = roost.call("app.cli_link", {"action": "install"})
    assert made["outcome"] == "created", made
    assert made["link"] == str(user_link), made
    assert os.readlink(user_link) == str(fake_bundle.roostctl)
    assert made["path_line"] == f'export PATH="{fake_bundle.links}/user:$PATH"', made
    roost._wait(lambda: dialog(roost).get("dialog") == "cli_link", 5.0, "the cli_link card")
    card = dialog(roost)
    assert card["buttons"] == ["Close", "Copy Line"], card
    assert card["body"].endswith(made["path_line"]), card
    roost.call("app.dialog_answer", {"action": "cancel"})
    roost._wait(lambda: dialog(roost).get("dialog") is None, 5.0, "the card closed")

    # A link pointing at an app that moved is broken, and install repairs
    # it in place.
    os.remove(user_link)
    gone = fake_bundle.root / "Gone.app" / "Contents" / "Resources" / "bin" / "roostctl"
    os.symlink(gone, user_link)
    broken = roost.call("app.cli_link", {"action": "status"})
    assert (broken["state"], broken["target"]) == ("broken", str(gone)), broken
    assert broken["label"] == "Repair 'roostctl' Command…", broken
    repaired = roost.call("app.cli_link", {"action": "install"})
    assert repaired["outcome"] == "repaired", repaired
    assert os.readlink(user_link) == str(fake_bundle.roostctl)
    roost.call("app.dialog_answer", {"action": "cancel"})

    removed = roost.call("app.cli_link", {"action": "uninstall"})
    assert (removed["outcome"], removed["state"]) == ("removed", "absent"), removed
    assert not os.path.lexists(user_link)
    roost._wait(
        lambda: (bottom_line(roost) or "").startswith("Uninstalled 'roostctl'"),
        5.0,
        "the uninstall toast",
    )


def test_a_file_that_is_not_roosts_is_never_overwritten(fake_bundle, bundled):
    roost, _ = bundled
    for name in ("system", "user"):
        (fake_bundle.links / name).mkdir(parents=True, exist_ok=True)
        (fake_bundle.links / name / "roostctl").write_text("mine")
    try:
        refused = roost.call("app.cli_link", {"action": "install"})
        assert refused["outcome"] == "refused", refused
        assert refused["command"].startswith("ln -sf "), refused
        for name in ("system", "user"):
            assert (fake_bundle.links / name / "roostctl").read_text() == "mine"
        roost._wait(lambda: dialog(roost).get("dialog") == "cli_link", 5.0, "the refusal card")
        assert dialog(roost)["buttons"] == ["Close", "Copy Command"]
        roost.call("app.dialog_answer", {"action": "cancel"})
    finally:
        shutil.rmtree(fake_bundle.links, ignore_errors=True)


def test_an_unknown_action_is_an_invalid_param(fake_bundle, bundled):
    roost, _ = bundled
    with pytest.raises(RoostError) as refused:
        roost.call("app.cli_link", {"action": "repair"})
    assert refused.value.code == "invalid-param"


def test_the_skill_fallback_names_a_variable_every_tab_has(fake_bundle, bundled):
    """The skill's fallback line is only true while every tab carries
    `ROOST_ROOSTCTL`; a tab opened with `PATH` reset by its own command
    still reaches roostctl through it."""
    roost, project = bundled
    tab = roost.open_tab(
        project,
        cwd="/tmp",
        cols=400,
        argv=["/bin/sh", "-c", 'PATH=/usr/bin:/bin; "$ROOST_ROOSTCTL" --version; exec sleep 600'],
    )
    roost.wait_text(tab, "roostctl ", timeout=15.0)
