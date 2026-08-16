"""Unit coverage for bundle-lib.sh's plist stamping + Sparkle feed
insertion (plan 028 C4, AC9's no-cargo target).

Runs the real bash functions (`roost_stamp_plist` /
`roost_insert_sparkle_feed`) in a subprocess against a temp bundle dir —
not a Python port of their logic — the same convention as
test_socket_paths.py. No cargo build, no network. The feed-enablement
contract pinned here: both values set inserts SUFeedURL + SUPublicEDKey
into the stamped plist, exactly one set is a hard error, neither set
leaves the plist byte-identical (today's feedless posture).

The insertion-verification tests read the keys back with PlistBuddy and
therefore run only on macOS (harness-unit CI is ubuntu); the stamping
and both-or-error tests never reach PlistBuddy and run everywhere.
"""

from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
BUNDLE_LIB = REPO_ROOT / "mac" / "scripts" / "bundle-lib.sh"
TEMPLATE_PLIST = REPO_ROOT / "mac" / "Resources" / "Info-iced.plist.template"
PLISTBUDDY = Path("/usr/libexec/PlistBuddy")

FEED_URL = "https://example.invalid/roost-iced/appcast.xml"
ED_PUBLIC_KEY = "TESTONLYc29tZS1mYWtlLWtleQ=="


def _run_lib(script: str, *args: str) -> subprocess.CompletedProcess[str]:
    """Source bundle-lib.sh and run `script` with $1.. bound to args."""
    return subprocess.run(
        ["bash", "-c", f'set -euo pipefail; . "$1"; shift; {script}', "_", str(BUNDLE_LIB), *args],
        capture_output=True,
        text=True,
    )


def _plistbuddy_print(plist: Path, key: str) -> str:
    result = subprocess.run(
        [str(PLISTBUDDY), "-c", f"Print :{key}", str(plist)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise KeyError(f"{key} not in {plist}: {result.stderr}")
    return result.stdout.strip()


class _StampedBundle:
    """A temp Contents/ dir with the real iced template stamped into it."""

    def __init__(self, case: unittest.TestCase, version: str = "9.9.9") -> None:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-sparkle-plist-"))
        case.addCleanup(shutil.rmtree, root, True)
        self.app_dir = root / "Roost-Iced.app"
        (self.app_dir / "Contents").mkdir(parents=True)
        result = _run_lib(
            'roost_stamp_plist "$1" "$2" "$3"',
            str(TEMPLATE_PLIST),
            str(self.app_dir),
            version,
        )
        case.assertEqual(result.returncode, 0, result.stderr)
        self.plist = self.app_dir / "Contents" / "Info.plist"

    def insert_feed(self, feed_url: str, ed_public_key: str) -> subprocess.CompletedProcess[str]:
        return _run_lib(
            'roost_insert_sparkle_feed "$1" "$2" "$3"',
            str(self.app_dir),
            feed_url,
            ed_public_key,
        )


class StampPlistTests(unittest.TestCase):
    def test_stamp_substitutes_the_version(self) -> None:
        bundle = _StampedBundle(self, version="1.2.3-test")
        text = bundle.plist.read_text()
        self.assertIn("1.2.3-test", text)
        self.assertNotIn("@VERSION@", text)

    def test_template_ships_no_feed_and_an_explicit_auto_check_false(self) -> None:
        """The 6c posture baked into the template itself: no
        SUFeedURL/SUPublicEDKey keys (the words appear in the template's
        comment block, so match real <key> elements, not raw text), and
        SUEnableAutomaticChecks present WITH the value false — presence
        alone would still pass if someone flipped it to <true/>."""
        bundle = _StampedBundle(self)
        text = bundle.plist.read_text()
        self.assertNotIn("<key>SUFeedURL</key>", text)
        self.assertNotIn("<key>SUPublicEDKey</key>", text)
        self.assertRegex(
            text, r"<key>SUEnableAutomaticChecks</key>\s*<false/>"
        )


class InsertFeedContractTests(unittest.TestCase):
    """Both-or-error, and neither-is-a-no-op — no PlistBuddy needed
    (the validation happens before any plist edit), so these run on the
    ubuntu harness-unit lane too."""

    def test_neither_set_is_a_noop_leaving_the_plist_byte_identical(self) -> None:
        bundle = _StampedBundle(self)
        before = bundle.plist.read_bytes()
        result = bundle.insert_feed("", "")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(bundle.plist.read_bytes(), before)
        self.assertNotIn("<key>SUFeedURL</key>", bundle.plist.read_text())

    def test_feed_url_alone_is_a_hard_error(self) -> None:
        bundle = _StampedBundle(self)
        before = bundle.plist.read_bytes()
        result = bundle.insert_feed(FEED_URL, "")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ROOST_ICED_SPARKLE_FEED_URL", result.stderr)
        self.assertIn("ROOST_ICED_SPARKLE_ED_PUBLIC_KEY", result.stderr)
        self.assertEqual(bundle.plist.read_bytes(), before)

    def test_public_key_alone_is_a_hard_error(self) -> None:
        bundle = _StampedBundle(self)
        before = bundle.plist.read_bytes()
        result = bundle.insert_feed("", ED_PUBLIC_KEY)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be set together", result.stderr)
        self.assertEqual(bundle.plist.read_bytes(), before)


@unittest.skipUnless(
    PLISTBUDDY.exists(), "PlistBuddy is macOS-only (insertion readback needs it)"
)
class InsertFeedInsertionTests(unittest.TestCase):
    def test_both_set_inserts_both_keys(self) -> None:
        bundle = _StampedBundle(self)
        result = bundle.insert_feed(FEED_URL, ED_PUBLIC_KEY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(_plistbuddy_print(bundle.plist, "SUFeedURL"), FEED_URL)
        self.assertEqual(_plistbuddy_print(bundle.plist, "SUPublicEDKey"), ED_PUBLIC_KEY)

    def test_insertion_leaves_auto_checks_false_and_identity_untouched(self) -> None:
        bundle = _StampedBundle(self)
        result = bundle.insert_feed(FEED_URL, ED_PUBLIC_KEY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            _plistbuddy_print(bundle.plist, "SUEnableAutomaticChecks"), "false"
        )
        self.assertEqual(
            _plistbuddy_print(bundle.plist, "CFBundleIdentifier"),
            "ai.stridelabs.Roost.iced",
        )


if __name__ == "__main__":
    unittest.main()
