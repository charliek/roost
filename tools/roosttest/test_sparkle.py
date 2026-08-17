"""Sparkle updater E2E — plan 028 C5 (`app.update_status` /
`app.update_check`).

Two explicit classes, one per lane, so neither can pass by silently
skipping itself (plan 028 § q):

* [`TestSparkleBundle`] runs only in the **test-keyed bundle lane**
  (`make e2e-iced-sparkle`): a `Roost-Iced.app` assembled with the
  fixture's TEST-ONLY `SUPublicEDKey` and a placeholder `SUFeedURL`,
  which the seam's test-mode delegate override then replaces with this
  module's loopback appcast. It proves the whole 6c machinery — dlopen,
  updater start, feed fetch, appcast parse, version comparison — up to
  but not including the download/install flow, which is not
  overnight-verifiable and lives on the morning eyeball checklist.
* [`TestSparkleBareBinary`] runs in every ordinary iced lane and pins
  the other half of the contract: a bare `cargo build` binary has no
  `Contents/Frameworks` above it, so the framework never loads, the
  updater is `unavailable`, and "Check for Updates…" is greyed.

**The loopback server starts at import time**, before pytest's
session-scoped `_ui_session` fixture launches the UI, because the feed
URL has to be in the launched process's environment (`open --env`, via
`ui.py`'s enumerated allowlist). A module fixture would run far too
late. In every non-bundle lane the import is inert — no server, no env
var, nothing to leak.
"""

from __future__ import annotations

import functools
import http.server
import os
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

import pytest

from client import RoostError
from test_menu_bar import APP, UPDATES_ITEM, _items_by_title, _menus_by_title

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = Path(__file__).parent / "fixtures" / "sparkle"
# Only the private half is read here; the public key is the *bundle's*
# input, stamped into Info.plist by `make e2e-iced-sparkle` / CI.
PRIVATE_KEY_FILE = FIXTURES / "TEST-ONLY-private-ed-key.txt"
SIGN_UPDATE = REPO_ROOT / "third_party" / "sparkle" / "out" / "bin" / "sign_update"

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# Far above any version this repo will ever ship, so the check's outcome
# is decided by the fixture and not by whatever `workspace.package.version`
# happens to be on the day the suite runs.
OFFERED_VERSION = "9999.0.0"


def _is_bundle_lane() -> bool:
    """The Sparkle lane: a macOS run driving a launched `Roost-Iced.app`.

    `ROOST_ICED_APP` is the same signal `ui.py` uses to pick its bundle
    launch path, so the two can never disagree about which lane this is.
    """
    return sys.platform == "darwin" and bool(os.environ.get("ROOST_ICED_APP"))


class _FeedServer:
    """A loopback http.server hosting the rendered appcast.

    Bound before the appcast is rendered, because the enclosure and
    channel URLs embed the ephemeral port. Requests are recorded so a
    passing run can show the appcast was actually fetched over the wire
    (plan 028 § 8) rather than inferred from the check's outcome.
    """

    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory(prefix="roost-sparkle-feed-")
        self.root = Path(self._dir.name)
        self.requests: list[str] = []
        recorder = self.requests

        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, fmt: str, *args) -> None:  # noqa: A002
                recorder.append(fmt % args)

        self._server = http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0),
            functools.partial(Handler, directory=str(self.root)),
        )
        self.port = self._server.server_address[1]
        self._render()
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def feed_url(self) -> str:
        return f"http://127.0.0.1:{self.port}/appcast.xml"

    def _render(self) -> None:
        # The enclosure is a real (tiny) file so the signing arm below has
        # something to sign and to measure; `checkForUpdateInformation`
        # never downloads it.
        enclosure = self.root / f"Roost-Iced-{OFFERED_VERSION}.zip"
        enclosure.write_bytes(b"roost-iced test enclosure, never downloaded\n")
        template = (FIXTURES / "appcast.xml.template").read_text()
        appcast = (
            template.replace("@VERSION@", OFFERED_VERSION)
            .replace("@PORT@", str(self.port))
            .replace("@SIGNATURE@", _signature_attributes(enclosure))
        )
        (self.root / "appcast.xml").write_text(appcast)

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5.0)
        self._dir.cleanup()


def _signature_attributes(enclosure: Path) -> str:
    """The enclosure's `sparkle:edSignature` + `length` attributes.

    Empty unless the private-key fallback arm is in play (plan 028
    § 3.11): an information-only check does not verify enclosure
    signatures, so the committed public key alone is normally enough.
    Committing `TEST-ONLY-private-ed-key.txt` is what turns this on —
    see the fixtures README.
    """
    if not PRIVATE_KEY_FILE.exists():
        return ""
    signed = subprocess.run(
        [str(SIGN_UPDATE), "--ed-key-file", str(PRIVATE_KEY_FILE), str(enclosure)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    # `sign_update` prints the attribute pair ready to paste into an
    # enclosure element, e.g. `sparkle:edSignature="…" length="42"`.
    return f" {signed}"


# --- import-time bootstrap (see the module docstring) -----------------
FEED_SERVER: _FeedServer | None = None
if _is_bundle_lane() and TEST_MODE:
    FEED_SERVER = _FeedServer()
    os.environ["ROOST_SPARKLE_FEED_URL"] = FEED_SERVER.feed_url


@pytest.fixture(scope="module", autouse=True)
def _teardown_feed_server():
    yield
    if FEED_SERVER is not None:
        print("\nsparkle loopback feed access log:")
        for line in FEED_SERVER.requests:
            print(f"  {line}")
        FEED_SERVER.stop()


pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="app.update_status/app.update_check require ROOST_TEST_MODE=1 in the UI's launch env",
)


def _updates_item(roost) -> dict:
    return _items_by_title(_menus_by_title(roost.app_menu_dump())[APP])[UPDATES_ITEM]


def _run_check(roost) -> dict:
    """Drive one non-interactive check and return the status it produced.

    Waits on `check_id` advancing, never on `last_check` becoming
    non-null: the seam keeps the previous result until the new one
    lands, so the null test would pass instantly on a second call.
    """
    before = roost.app_update_status()["check_id"]
    roost.app_update_check()
    roost._wait(
        lambda: roost.app_update_status()["check_id"] > before,
        20.0,
        f"app.update_check to advance check_id past {before}",
    )
    status = roost.app_update_status()
    print(f"\napp.update_status after check: {status}")
    return status


def _ensure_completed_check(roost) -> dict:
    """A completed check, running one only if none has finished yet — so
    every test that needs check side effects (the server's access log,
    post-cycle canCheckForUpdates) stands alone under a focused `-k`
    run instead of depending on test order."""
    status = roost.app_update_status()
    if status["check_id"] > 0:
        return status
    return _run_check(roost)


class TestSparkleBundle:
    """§ AC6/AC7 in the test-keyed bundle lane (`make e2e-iced-sparkle`):
    the framework loads, the updater starts, and a check against the
    loopback appcast reports the fixture's version."""

    @pytest.fixture(autouse=True)
    def _bundle_only(self, target):
        if not _is_bundle_lane() or target != "iced":
            pytest.skip(
                "the Sparkle bundle lane needs a launched Roost-Iced.app "
                "(ROOST_ICED_APP) — `make e2e-iced-sparkle`"
            )

    def test_the_framework_loaded_and_the_updater_started(self, roost):
        status = roost.app_update_status()
        print(f"\napp.update_status at boot: {status}")
        assert status["framework_loaded"] is True, status
        assert status["updater"] == "started", status
        assert status["reason"] is None, status

    def test_a_check_finds_the_fixture_version(self, roost):
        status = _run_check(roost)
        assert status["last_check"]["outcome"] == "found", status
        assert status["last_check"]["version"] == OFFERED_VERSION, status

    def test_the_feed_was_fetched_over_loopback(self, roost):
        _ensure_completed_check(roost)
        # The found outcome could in principle come from a cached feed;
        # the server's own access log is what proves the wire hop.
        assert FEED_SERVER is not None
        assert any("appcast.xml" in line for line in FEED_SERVER.requests), (
            "the loopback server never served appcast.xml: "
            f"{FEED_SERVER.requests}"
        )

    def test_the_check_for_updates_item_is_enabled(self, roost):
        _ensure_completed_check(roost)
        assert _updates_item(roost)["action"] == "check_for_updates"
        # canCheckForUpdates only flips true once the probe session ends
        # (cycle-finish fires AFTER the check_id advance the previous test
        # waited on), and the menu item only reflects it on a later update
        # turn — condition-wait rather than one-shot to keep a slow runner
        # from reading the gap (review finding, plan 028 C5).
        roost._wait(
            lambda: _updates_item(roost)["enabled"] is True,
            5.0,
            "Check for Updates… item to become enabled",
        )


class TestSparkleBareBinary:
    """§ AC6/AC7 in the ordinary lanes: no bundle, so no framework, so no
    updater — and a permanently greyed menu item."""

    @pytest.fixture(autouse=True)
    def _bare_only(self, target):
        if sys.platform != "darwin" or target != "iced":
            pytest.skip("the Sparkle seam is macOS-iced-only (plan 028 § 3.8)")
        if _is_bundle_lane():
            pytest.skip(
                "this class pins the NO-framework posture; the bundle lane "
                "has one (TestSparkleBundle covers it)"
            )

    def test_the_updater_is_unavailable_with_a_no_framework_reason(self, roost):
        status = roost.app_update_status()
        assert status["framework_loaded"] is False, status
        assert status["updater"] == "unavailable", status
        # The reason names the path that was not there, which is what
        # makes a genuinely broken bundle distinguishable from this.
        assert "Sparkle.framework" in (status["reason"] or ""), status
        assert status["last_check"] is None, status

    def test_a_check_errors_rather_than_reporting_a_result(self, roost):
        with pytest.raises(RoostError) as caught:
            roost.app_update_check()
        assert caught.value.code == "internal", caught.value
        assert roost.app_update_status()["check_id"] == 0

    def test_the_check_for_updates_item_is_disabled(self, roost):
        item = _updates_item(roost)
        assert item["action"] == "check_for_updates"
        assert item["enabled"] is False
