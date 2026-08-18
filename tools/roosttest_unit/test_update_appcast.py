"""Unit coverage for mac/scripts/update-appcast.py (plan 030 C3).

Runs the real script via subprocess against temp appcast files + a fake
sign_update output — not a Python port of its logic — the same convention
as test_sparkle_plist.py. No network, no cargo/swift build.

Covers the two invocation shapes the DMG-generalization work (C3) adds:
the unchanged Swift default (ROOST_DMG_NAME unset -> "Roost-{version}.dmg")
and the iced override (ROOST_DMG_NAME set), plus the fresh-channel shape a
committed docs/appcast-iced.xml seed will have (zero <item>s, C4), and the
existing dedupe-by-version behavior (re-running the same version replaces
rather than duplicates).
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "mac" / "scripts" / "update-appcast.py"

SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"

APPCAST_WITH_ONE_ITEM = """<?xml version='1.0' encoding='utf-8'?>
<rss xmlns:sparkle="{ns}" version="2.0">
  <channel>
    <title>Roost</title>
    <link>https://github.com/charliek/roost</link>
    <description>Appcast for Roost macOS auto-updates (Sparkle).</description>
    <language>en</language>
    <item>
      <title>0.0.1</title>
      <pubDate>Thu, 28 May 2026 02:38:39 +0000</pubDate>
      <sparkle:version>0.0.1</sparkle:version>
      <sparkle:shortVersionString>0.0.1</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>15.0.0</sparkle:minimumSystemVersion>
      <enclosure url="https://github.com/charliek/roost/releases/download/v0.0.1/Roost-0.0.1.dmg" type="application/octet-stream" sparkle:edSignature="OLDSIG" length="1234" />
    </item>
  </channel>
</rss>
""".format(ns=SPARKLE_NS)

EMPTY_CHANNEL_SKELETON = """<?xml version='1.0' encoding='utf-8'?>
<rss xmlns:sparkle="{ns}" version="2.0">
  <channel>
    <title>Roost Iced</title>
    <link>https://github.com/charliek/roost</link>
    <description>Appcast for the experimental Roost-Iced macOS build (Sparkle).</description>
    <language>en</language>
  </channel>
</rss>
""".format(ns=SPARKLE_NS)

SIGN_LINE = 'sparkle:edSignature="{sig}" length="{length}"'


def qname(local: str) -> str:
    return f"{{{SPARKLE_NS}}}{local}"


class UpdateAppcastTests(unittest.TestCase):
    def setUp(self) -> None:
        tmpdir = tempfile.TemporaryDirectory(prefix="roost-unit-appcast-")
        self.addCleanup(tmpdir.cleanup)
        self.tmp = Path(tmpdir.name)

    def _write_appcast(self, content: str, name: str = "appcast.xml") -> Path:
        path = self.tmp / name
        path.write_text(content, encoding="utf-8")
        return path

    def _write_sign_file(self, sig: str = "SIG-BASE64==", length: str = "5551212") -> Path:
        path = self.tmp / "sign_update.txt"
        path.write_text(SIGN_LINE.format(sig=sig, length=length) + "\n", encoding="utf-8")
        return path

    def _run(self, env_overrides: dict[str, str]) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT)],
            capture_output=True,
            text=True,
            env=env_overrides,
        )

    def _channel(self, path: Path) -> ET.Element:
        tree = ET.parse(path)
        channel = tree.getroot().find("channel")
        assert channel is not None
        return channel

    def test_default_swift_shape_enclosure_name_and_signature(self) -> None:
        appcast = self._write_appcast(APPCAST_WITH_ONE_ITEM)
        sign_file = self._write_sign_file(sig="NEWSIG-abc123", length="9999")
        result = self._run(
            {
                "PATH": "/usr/bin:/bin",
                "ROOST_VERSION": "0.0.2",
                "ROOST_APPCAST": str(appcast),
                "ROOST_SIGN_FILE": str(sign_file),
            }
        )
        self.assertEqual(result.returncode, 0, result.stderr)

        channel = self._channel(appcast)
        items = channel.findall("item")
        versions = {item.find(qname("version")).text for item in items}
        self.assertEqual(versions, {"0.0.1", "0.0.2"})

        new_item = next(
            item for item in items if item.find(qname("version")).text == "0.0.2"
        )
        enclosure = new_item.find("enclosure")
        self.assertTrue(enclosure.get("url").endswith("Roost-0.0.2.dmg"))
        self.assertEqual(enclosure.get(qname("edSignature")), "NEWSIG-abc123")
        self.assertEqual(enclosure.get("length"), "9999")

    def test_iced_dmg_name_override_changes_only_the_enclosure_name(self) -> None:
        appcast = self._write_appcast(APPCAST_WITH_ONE_ITEM)
        sign_file = self._write_sign_file()
        result = self._run(
            {
                "PATH": "/usr/bin:/bin",
                "ROOST_VERSION": "0.0.2",
                "ROOST_APPCAST": str(appcast),
                "ROOST_SIGN_FILE": str(sign_file),
                "ROOST_DMG_NAME": "Roost-Iced-0.0.2.dmg",
            }
        )
        self.assertEqual(result.returncode, 0, result.stderr)

        channel = self._channel(appcast)
        new_item = next(
            item
            for item in channel.findall("item")
            if item.find(qname("version")).text == "0.0.2"
        )
        enclosure = new_item.find("enclosure")
        self.assertTrue(enclosure.get("url").endswith("Roost-Iced-0.0.2.dmg"))
        self.assertNotIn("Roost-0.0.2.dmg", enclosure.get("url"))

    def test_fresh_empty_channel_appends_cleanly(self) -> None:
        appcast = self._write_appcast(EMPTY_CHANNEL_SKELETON, name="appcast-iced.xml")
        sign_file = self._write_sign_file(sig="FIRSTSIG", length="42")
        result = self._run(
            {
                "PATH": "/usr/bin:/bin",
                "ROOST_VERSION": "0.0.1",
                "ROOST_APPCAST": str(appcast),
                "ROOST_SIGN_FILE": str(sign_file),
                "ROOST_DMG_NAME": "Roost-Iced-0.0.1.dmg",
            }
        )
        self.assertEqual(result.returncode, 0, result.stderr)

        channel = self._channel(appcast)
        items = channel.findall("item")
        self.assertEqual(len(items), 1)
        enclosure = items[0].find("enclosure")
        self.assertTrue(enclosure.get("url").endswith("Roost-Iced-0.0.1.dmg"))
        self.assertEqual(enclosure.get(qname("edSignature")), "FIRSTSIG")

    def test_rerunning_the_same_version_replaces_not_duplicates(self) -> None:
        appcast = self._write_appcast(APPCAST_WITH_ONE_ITEM)
        sign_file = self._write_sign_file(sig="FIRST-RUN-SIG", length="111")
        first = self._run(
            {
                "PATH": "/usr/bin:/bin",
                "ROOST_VERSION": "0.0.2",
                "ROOST_APPCAST": str(appcast),
                "ROOST_SIGN_FILE": str(sign_file),
            }
        )
        self.assertEqual(first.returncode, 0, first.stderr)

        sign_file.write_text(SIGN_LINE.format(sig="SECOND-RUN-SIG", length="222") + "\n", encoding="utf-8")
        second = self._run(
            {
                "PATH": "/usr/bin:/bin",
                "ROOST_VERSION": "0.0.2",
                "ROOST_APPCAST": str(appcast),
                "ROOST_SIGN_FILE": str(sign_file),
            }
        )
        self.assertEqual(second.returncode, 0, second.stderr)

        channel = self._channel(appcast)
        items = channel.findall("item")
        matching = [
            item for item in items if item.find(qname("version")).text == "0.0.2"
        ]
        self.assertEqual(len(matching), 1)
        enclosure = matching[0].find("enclosure")
        self.assertEqual(enclosure.get(qname("edSignature")), "SECOND-RUN-SIG")
        # 0.0.1 (unrelated version) is untouched.
        self.assertEqual(
            {item.find(qname("version")).text for item in items}, {"0.0.1", "0.0.2"}
        )


if __name__ == "__main__":
    unittest.main()
