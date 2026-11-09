"""Unit coverage for bundle-lib.sh's bundle identity, plist stamping and
Sparkle feed insertion (plan 028 C4, plan 089 D2; AC9's no-cargo target).

Runs the real bash functions (`roost_resolve_bundle_identity`,
`roost_stamp_plist`, `roost_insert_sparkle_feed`) in a subprocess against a
temp bundle dir — not a Python port of their logic — the same convention as
test_socket_paths.py. No cargo build, no network. Pinned here: `dev` and
`production` are the only identities, and production requires both Sparkle
values; the stamp fills the id and name everywhere the template asks and
leaves no placeholder; both feed values set inserts SUFeedURL +
SUPublicEDKey into the stamped plist, exactly one set is a hard error,
neither set leaves the plist byte-identical (a feedless dev bundle).

The insertion-verification tests read the keys back with PlistBuddy and
therefore run only on macOS (harness-unit CI is ubuntu); the stamping
and both-or-error tests never reach PlistBuddy and run everywhere.
"""

from __future__ import annotations

import plistlib
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
BUNDLE_LIB = REPO_ROOT / "mac" / "scripts" / "bundle-lib.sh"
TEMPLATE_PLIST = REPO_ROOT / "mac" / "Resources" / "Info.plist.template"
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


DEV = ("ai.stridelabs.Roost.iced", "Roost-Iced")
PRODUCTION = ("ai.stridelabs.Roost", "Roost")


class _StampedBundle:
    """A temp Contents/ dir with the real iced template stamped into it."""

    def __init__(
        self,
        case: unittest.TestCase,
        version: str = "9.9.9",
        identity: tuple[str, str] = DEV,
        template: Path = TEMPLATE_PLIST,
    ) -> None:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-sparkle-plist-"))
        case.addCleanup(shutil.rmtree, root, True)
        self.app_dir = root / f"{identity[1]}.app"
        (self.app_dir / "Contents").mkdir(parents=True)
        self.result = _run_lib(
            'roost_stamp_plist "$1" "$2" "$3" "$4" "$5"',
            str(template),
            str(self.app_dir),
            version,
            *identity,
        )
        self.plist = self.app_dir / "Contents" / "Info.plist"

    def read(self) -> dict:
        with open(self.plist, "rb") as handle:
            return plistlib.load(handle)

    def insert_feed(self, feed_url: str, ed_public_key: str) -> subprocess.CompletedProcess[str]:
        return _run_lib(
            'roost_insert_sparkle_feed "$1" "$2" "$3"',
            str(self.app_dir),
            feed_url,
            ed_public_key,
        )


def _stamped(case: unittest.TestCase, **kwargs) -> _StampedBundle:
    bundle = _StampedBundle(case, **kwargs)
    case.assertEqual(bundle.result.returncode, 0, bundle.result.stderr)
    return bundle


class StampPlistTests(unittest.TestCase):
    def test_stamp_substitutes_the_version(self) -> None:
        bundle = _stamped(self, version="1.2.3-test")
        text = bundle.plist.read_text()
        self.assertIn("1.2.3-test", text)
        self.assertNotIn("@VERSION@", text)

    def test_each_identity_names_the_id_executable_and_usage_strings(self) -> None:
        for (bundle_id, name) in (PRODUCTION, DEV):
            with self.subTest(bundle_id=bundle_id):
                plist = _stamped(self, identity=(bundle_id, name)).read()
                self.assertEqual(plist["CFBundleIdentifier"], bundle_id)
                for key in ("CFBundleName", "CFBundleDisplayName", "CFBundleExecutable"):
                    self.assertEqual(plist[key], name, key)
                for key in (
                    "NSMicrophoneUsageDescription",
                    "NSCameraUsageDescription",
                    "NSAppleEventsUsageDescription",
                ):
                    self.assertTrue(
                        plist[key].startswith(f"A program running in {name} would like"), plist[key]
                    )

    def test_the_stamped_plist_carries_no_placeholder(self) -> None:
        text = _stamped(self, identity=PRODUCTION).plist.read_text()
        self.assertNotRegex(text, r"@[A-Z_]+@")

    def test_a_placeholder_the_stamp_does_not_know_fails_it(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-sparkle-plist-tpl-"))
        self.addCleanup(shutil.rmtree, root, True)
        template = root / "Info.plist.template"
        template.write_text(TEMPLATE_PLIST.read_text().replace("<string>APPL</string>", "<string>@KIND@</string>"))
        bundle = _StampedBundle(self, template=template)
        self.assertNotEqual(bundle.result.returncode, 0)
        self.assertIn("unsubstituted placeholder", bundle.result.stderr)

    def test_values_sed_would_read_as_syntax_are_refused(self) -> None:
        for identity in (("ai/stridelabs", "Roost"), ("ai.stridelabs.Roost", "Ro&st"), ("", "Roost")):
            with self.subTest(identity=identity):
                bundle = _StampedBundle(self, identity=identity)
                self.assertNotEqual(bundle.result.returncode, 0)

    def test_template_ships_no_feed_and_daily_prompting_checks(self) -> None:
        """The template's posture: no SUFeedURL/SUPublicEDKey keys (the
        words appear in the template's comment block, so match real <key>
        elements, not raw text), and background checks on, daily, never
        installing silently (#134). Each key is matched WITH its value."""
        bundle = _stamped(self)
        text = bundle.plist.read_text()
        self.assertNotIn("<key>SUFeedURL</key>", text)
        self.assertNotIn("<key>SUPublicEDKey</key>", text)
        self.assertRegex(text, r"<key>SUEnableAutomaticChecks</key>\s*<true/>")
        self.assertRegex(
            text, r"<key>SUScheduledCheckInterval</key>\s*<integer>86400</integer>"
        )
        self.assertRegex(text, r"<key>SUAutomaticallyUpdate</key>\s*<false/>")


class InsertFeedContractTests(unittest.TestCase):
    """Both-or-error, and neither-is-a-no-op — no PlistBuddy needed
    (the validation happens before any plist edit), so these run on the
    ubuntu harness-unit lane too."""

    def test_neither_set_is_a_noop_leaving_the_plist_byte_identical(self) -> None:
        bundle = _stamped(self)
        before = bundle.plist.read_bytes()
        result = bundle.insert_feed("", "")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(bundle.plist.read_bytes(), before)
        self.assertNotIn("<key>SUFeedURL</key>", bundle.plist.read_text())

    def test_feed_url_alone_is_a_hard_error(self) -> None:
        bundle = _stamped(self)
        before = bundle.plist.read_bytes()
        result = bundle.insert_feed(FEED_URL, "")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ROOST_BUNDLE_SPARKLE_FEED_URL", result.stderr)
        self.assertIn("ROOST_BUNDLE_SPARKLE_ED_PUBLIC_KEY", result.stderr)
        self.assertEqual(bundle.plist.read_bytes(), before)

    def test_public_key_alone_is_a_hard_error(self) -> None:
        bundle = _stamped(self)
        before = bundle.plist.read_bytes()
        result = bundle.insert_feed("", ED_PUBLIC_KEY)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be set together", result.stderr)
        self.assertEqual(bundle.plist.read_bytes(), before)


class ResolveBundleIdentityTests(unittest.TestCase):
    """`roost_resolve_bundle_identity`, which bundle-iced.sh runs before
    its cargo build (plan 089 D2)."""

    def _resolve(self, identity: str, feed: str = "", key: str = "") -> subprocess.CompletedProcess[str]:
        return _run_lib(
            'roost_resolve_bundle_identity "$1" "$2" "$3"; '
            'printf "%s %s\\n" "$ROOST_APP_NAME" "$ROOST_BUNDLE_ID"',
            identity,
            feed,
            key,
        )

    def test_dev_is_roost_iced_with_or_without_a_feed(self) -> None:
        for feed, key in (("", ""), (FEED_URL, ED_PUBLIC_KEY)):
            result = self._resolve("dev", feed, key)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "Roost-Iced ai.stridelabs.Roost.iced\n")

    def test_production_is_roost_and_needs_both_sparkle_values(self) -> None:
        result = self._resolve("production", FEED_URL, ED_PUBLIC_KEY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "Roost ai.stridelabs.Roost\n")
        for feed, key in (("", ""), (FEED_URL, ""), ("", ED_PUBLIC_KEY)):
            with self.subTest(feed=feed, key=key):
                result = self._resolve("production", feed, key)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("ROOST_BUNDLE_IDENTITY=production requires both", result.stderr)

    def test_any_other_identity_is_a_hard_error(self) -> None:
        for identity in ("", "Production", "release", "iced"):
            with self.subTest(identity=identity):
                result = self._resolve(identity, FEED_URL, ED_PUBLIC_KEY)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must be 'dev' or 'production'", result.stderr)

    def test_bundle_iced_resolves_the_identity_before_it_builds(self) -> None:
        script = (REPO_ROOT / "mac" / "scripts" / "bundle-iced.sh").read_text()
        resolve = script.index("roost_resolve_bundle_identity ")
        self.assertLess(resolve, script.index('"${CARGO_BIN}" build'))
        self.assertIn('"${ROOST_BUNDLE_IDENTITY:-dev}"', script)
        self.assertNotIn("ROOST_ICED_SPARKLE", script)


@unittest.skipUnless(
    PLISTBUDDY.exists(), "PlistBuddy is macOS-only (insertion readback needs it)"
)
class InsertFeedInsertionTests(unittest.TestCase):
    def test_both_set_inserts_both_keys(self) -> None:
        bundle = _stamped(self)
        result = bundle.insert_feed(FEED_URL, ED_PUBLIC_KEY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(_plistbuddy_print(bundle.plist, "SUFeedURL"), FEED_URL)
        self.assertEqual(_plistbuddy_print(bundle.plist, "SUPublicEDKey"), ED_PUBLIC_KEY)

    def test_insertion_leaves_auto_checks_and_identity_untouched(self) -> None:
        bundle = _stamped(self)
        result = bundle.insert_feed(FEED_URL, ED_PUBLIC_KEY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            _plistbuddy_print(bundle.plist, "SUEnableAutomaticChecks"), "true"
        )
        self.assertEqual(
            _plistbuddy_print(bundle.plist, "CFBundleIdentifier"),
            "ai.stridelabs.Roost.iced",
        )


if __name__ == "__main__":
    unittest.main()
