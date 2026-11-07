"""The retired Roost-Iced.app's frozen feed ends in a notice, not an update.

Roost-Iced.app has its own bundle id, so Sparkle cannot turn it into
Roost.app. Its feed's last item is an informational update: Sparkle shows the
notes and a Learn More button that opens the switch steps, and never
downloads anything.
"""

from __future__ import annotations

import re
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
FEED = REPO_ROOT / "docs" / "appcast-iced.xml"
INSTALLATION = REPO_ROOT / "docs" / "getting-started" / "installation.md"
SITE = "https://charliek.github.io/roost/"

NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"


def sparkle(tag: str) -> str:
    return f"{{{NS}}}{tag}"


def version_key(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("."))


def heading_anchors(markdown: str) -> set[str]:
    anchors = set()
    for line in markdown.splitlines():
        if line.startswith("#"):
            text = line.lstrip("#").strip().replace("`", "").lower()
            anchors.add(re.sub(r"[^a-z0-9 -]", "", text).replace(" ", "-"))
    return anchors


class IcedFeedSunsetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.items = ET.parse(FEED).getroot().find("channel").findall("item")
        self.notice = self.items[-1]

    def test_the_last_item_is_informational_with_nothing_to_install(self) -> None:
        self.assertIsNotNone(self.notice.find(sparkle("informationalUpdate")))
        self.assertIsNone(self.notice.find("enclosure"))

    def test_the_notice_outranks_every_shipped_roost_iced_version(self) -> None:
        shipped = [version_key(item.find(sparkle("version")).text) for item in self.items[:-1]]
        self.assertGreater(version_key(self.notice.find(sparkle("version")).text), max(shipped))

    def test_every_earlier_item_is_a_real_signed_update(self) -> None:
        for item in self.items[:-1]:
            enclosure = item.find("enclosure")
            self.assertIsNotNone(enclosure)
            self.assertTrue(enclosure.get(sparkle("edSignature")))

    def test_learn_more_opens_the_switch_steps(self) -> None:
        link = self.notice.find("link").text
        page, _, anchor = link.partition("#")
        self.assertEqual(page, SITE + "getting-started/installation/")
        self.assertIn(anchor, heading_anchors(INSTALLATION.read_text()))


if __name__ == "__main__":
    unittest.main()
