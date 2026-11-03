"""The macOS bundle's TCC surface, read from its committed sources.

Programs hosted in a Roost tab (Claude Code /voice, ffmpeg, sox, osascript)
reach the microphone, camera and Apple events *through Roost*: macOS makes
Roost the responsible app. Under the hardened runtime those requests are
silently denied (no prompt, no Privacy pane entry) unless the app carries
the matching entitlement AND the paired Info.plist purpose string. Both are
one edit away from being dropped in a merge and the failure is invisible at
runtime, so they are pinned here; CI's "Assert bundle contents" step checks
the signed bundle. The negatives are pinned too: the broad keys stay out of
the app (`mac/Resources/Roost.entitlements` says why), `roostctl` does not
inherit the capture set, and `roost-session` carries none at all.

Also pins the production Sparkle public key: every installed Roost.app
verifies updates with it (`mac/keys/README.md`).
"""

from __future__ import annotations

import plistlib
import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
RESOURCES = REPO / "mac" / "Resources"
TEMPLATE = RESOURCES / "Info.plist.template"
SPARKLE_PUBLIC_KEY = REPO / "mac" / "keys" / "roost-sparkle-ed-public-key.txt"

# The SUPublicEDKey the Swift Roost.app shipped with. Changing it strands
# every installed copy: they refuse updates signed for any other key.
PRODUCTION_SPARKLE_PUBLIC_KEY = "P2VexGJQtk6cptdgKi3skMV9+WcmcZMAKjSPrKhylJA="

CAPTURE_KEYS = (
    "com.apple.security.device.audio-input",
    "com.apple.security.device.camera",
    "com.apple.security.automation.apple-events",
)
# Required by the ad-hoc-signed embedded Sparkle.framework.
LIBRARY_VALIDATION_KEY = "com.apple.security.cs.disable-library-validation"
BROAD_KEYS_EXCLUDED = (
    "com.apple.security.personal-information.addressbook",
    "com.apple.security.personal-information.calendars",
    "com.apple.security.personal-information.location",
    "com.apple.security.personal-information.photos-library",
    "com.apple.security.cs.allow-jit",
    "com.apple.security.cs.allow-unsigned-executable-memory",
    "com.apple.security.app-sandbox",
    "com.apple.security.network.client",
    "com.apple.security.network.server",
)
USAGE_STRING_KEYS = (
    "NSMicrophoneUsageDescription",
    "NSCameraUsageDescription",
    "NSAppleEventsUsageDescription",
)
# What `roost_stamp_plist` (mac/scripts/bundle-lib.sh) fills in.
KNOWN_PLACEHOLDERS = {"@VERSION@", "@BUNDLE_ID@", "@APP_NAME@"}


def load(name: str) -> dict:
    with open(RESOURCES / name, "rb") as handle:
        value = plistlib.load(handle)
    if not isinstance(value, dict):
        raise AssertionError(f"{name} did not parse as a plist dictionary")
    return value


def app_entitlement_problems(entitlements: dict) -> list[str]:
    found = []
    for key in (*CAPTURE_KEYS, LIBRARY_VALIDATION_KEY):
        if entitlements.get(key) is not True:
            found.append(f"Roost.entitlements must declare {key}=true")
    for key in BROAD_KEYS_EXCLUDED:
        if key in entitlements:
            found.append(f"Roost.entitlements unexpectedly declares {key}")
    return found


class EntitlementsTests(unittest.TestCase):
    def test_the_app_declares_the_capture_set_and_no_broad_key(self) -> None:
        self.assertEqual(app_entitlement_problems(load("Roost.entitlements")), [])

    def test_a_dropped_capture_key_is_caught(self) -> None:
        entitlements = load("Roost.entitlements")
        for key in (*CAPTURE_KEYS, LIBRARY_VALIDATION_KEY):
            with self.subTest(key=key):
                for broken in ({k: v for k, v in entitlements.items() if k != key}, {**entitlements, key: False}):
                    self.assertIn(f"Roost.entitlements must declare {key}=true", app_entitlement_problems(broken))

    def test_an_added_broad_key_is_caught(self) -> None:
        broken = {**load("Roost.entitlements"), "com.apple.security.network.client": True}
        self.assertIn(
            "Roost.entitlements unexpectedly declares com.apple.security.network.client",
            app_entitlement_problems(broken),
        )

    def test_roostctl_does_not_inherit_the_capture_set(self) -> None:
        entitlements = load("roostctl.entitlements")
        for key in CAPTURE_KEYS:
            with self.subTest(key=key):
                self.assertNotIn(key, entitlements, "the CLI never captures (least privilege)")

    def test_roost_session_carries_no_entitlements(self) -> None:
        self.assertEqual(load("roost-session.entitlements"), {})

    def test_no_entitlements_comment_holds_a_double_hyphen(self) -> None:
        """AMFI's XML parser, which codesign hands an entitlements file to,
        rejects `--` even inside a comment, and the bundle build dies."""
        for path in sorted(RESOURCES.glob("*.entitlements")):
            with self.subTest(file=path.name):
                body = path.read_text().replace("<!--", "", 1).replace("-->", "", 1)
                self.assertNotIn("--", body)


class InfoPlistTemplateTests(unittest.TestCase):
    def test_the_template_carries_every_usage_string(self) -> None:
        template = load(TEMPLATE.name)
        for key in USAGE_STRING_KEYS:
            with self.subTest(key=key):
                value = template.get(key)
                self.assertIsInstance(value, str)
                self.assertTrue(value.strip(), f"{key} is empty (it pairs with a capture entitlement)")
                self.assertIn("@APP_NAME@", value, f"{key} must name the app its identity stamps")

    def test_the_template_uses_only_placeholders_the_stamp_fills(self) -> None:
        text = TEMPLATE.read_text()
        found = set(re.findall(r"@[A-Z_]+@", text))
        self.assertEqual(found - KNOWN_PLACEHOLDERS, set())
        template = load(TEMPLATE.name)
        self.assertEqual(template["CFBundleIdentifier"], "@BUNDLE_ID@")
        self.assertEqual(template["CFBundleExecutable"], "@APP_NAME@")
        self.assertEqual(template["CFBundleShortVersionString"], "@VERSION@")


class SparkleKeyTests(unittest.TestCase):
    def test_the_committed_public_key_is_the_production_key(self) -> None:
        self.assertEqual(SPARKLE_PUBLIC_KEY.read_text().strip(), PRODUCTION_SPARKLE_PUBLIC_KEY)

    def test_the_release_check_pins_the_same_key(self) -> None:
        script = (REPO / "mac" / "scripts" / "verify-sparkle-key.sh").read_text()
        self.assertIn(f'PINNED_PUBLIC_KEY="{PRODUCTION_SPARKLE_PUBLIC_KEY}"', script)


if __name__ == "__main__":
    unittest.main()
