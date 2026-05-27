#!/usr/bin/env python3
"""Append a release to Roost's Sparkle appcast (issue #122).

Adapted from Ghostty's src/dist/macos/update_appcast_tag.py. Parses the
appcast in place, drops any existing <item> for the same version (we reuse
the semver as the Sparkle version, and duplicate versions make Sparkle pick
a signature nondeterministically), appends a new <item> for this release,
and writes the file back.

We append-in-repo rather than regenerating with Sparkle's `generate_appcast`,
which would need every historical DMG present on disk.

Inputs (environment):
  ROOST_VERSION    required   e.g. "0.0.4" or "0.0.4-beta1"
  ROOST_TAG        optional   git tag; default "v$ROOST_VERSION"
  ROOST_APPCAST    optional   appcast path; default "docs/appcast.xml"
  ROOST_SIGN_FILE  optional   sign_update output file; default "sign_update.txt"
  ROOST_REPO       optional   "owner/repo"; default "charliek/roost"
  ROOST_MIN_MACOS  optional   minimum system version; default "15.0.0"

`sign_update.txt` must hold the line Sparkle's `sign_update` prints for the
released DMG:

    sparkle:edSignature="<base64>" length="<bytes>"

A prerelease tag (one containing "-", e.g. v0.0.4-beta1 — mirroring
release.yml's `case "$tag" in *-*)`) gets a <sparkle:channel>beta</sparkle:channel>
so it reaches only beta-channel subscribers, not stable users.
"""

import os
import sys
import xml.etree.ElementTree as ET
from datetime import datetime, timezone

SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"
# RFC-822, the format Sparkle expects for <pubDate>.
PUBDATE_FMT = "%a, %d %b %Y %H:%M:%S %z"


def parse_sign_update(path):
    """Parse the `key="value"` pairs from a `sign_update` output line.

    Values are a base64 signature and an integer length — neither contains a
    space, so splitting on whitespace is safe.
    """
    with open(path, encoding="utf-8") as f:
        text = f.read().strip()
    if not text:
        sys.exit(f"error: {path} is empty (did sign_update run?)")
    attrs = {}
    for pair in text.split():
        if "=" not in pair:
            continue
        key, value = pair.split("=", 1)
        attrs[key] = value.strip().strip('"')
    return attrs


def qname(local):
    """Namespaced ElementTree tag/attr name in the sparkle namespace."""
    return f"{{{SPARKLE_NS}}}{local}"


def main():
    try:
        version = os.environ["ROOST_VERSION"]
    except KeyError:
        sys.exit("error: ROOST_VERSION is required")
    tag = os.environ.get("ROOST_TAG", f"v{version}")
    appcast_path = os.environ.get("ROOST_APPCAST", "docs/appcast.xml")
    sign_file = os.environ.get("ROOST_SIGN_FILE", "sign_update.txt")
    repo = os.environ.get("ROOST_REPO", "charliek/roost")
    min_macos = os.environ.get("ROOST_MIN_MACOS", "15.0.0")
    is_prerelease = "-" in tag

    attrs = parse_sign_update(sign_file)
    sig = attrs.get("sparkle:edSignature")
    length = attrs.get("length")
    if not sig or not length:
        sys.exit(
            f"error: {sign_file} missing sparkle:edSignature/length (got {attrs})"
        )

    # Preserve in-tree XML comments across the rewrite (Python 3.8+). Comments
    # outside the root element can't be represented by ElementTree and are
    # dropped — keep maintainer docs in this script / release.yml, not there.
    ET.register_namespace("sparkle", SPARKLE_NS)
    parser = ET.XMLParser(target=ET.TreeBuilder(insert_comments=True))
    tree = ET.parse(appcast_path, parser)
    channel = tree.getroot().find("channel")
    if channel is None:
        sys.exit(f"error: {appcast_path} has no <channel>")

    # Dedupe by version so a re-run (or re-tag) replaces rather than duplicates.
    for item in channel.findall("item"):
        existing = item.find(qname("version"))
        if existing is not None and existing.text == version:
            channel.remove(item)

    now = datetime.now(timezone.utc)
    dmg = f"Roost-{version}.dmg"
    url = f"https://github.com/{repo}/releases/download/{tag}/{dmg}"

    item = ET.SubElement(channel, "item")
    ET.SubElement(item, "title").text = version
    ET.SubElement(item, "pubDate").text = now.strftime(PUBDATE_FMT)
    ET.SubElement(item, qname("version")).text = version
    ET.SubElement(item, qname("shortVersionString")).text = version
    ET.SubElement(item, qname("minimumSystemVersion")).text = min_macos
    if is_prerelease:
        ET.SubElement(item, qname("channel")).text = "beta"
    enclosure = ET.SubElement(item, "enclosure")
    enclosure.set("url", url)
    enclosure.set("type", "application/octet-stream")
    enclosure.set(qname("edSignature"), sig)
    enclosure.set("length", length)

    ET.indent(tree, space="  ")
    tree.write(appcast_path, xml_declaration=True, encoding="utf-8")
    # ElementTree omits the trailing newline; add one for a clean diff.
    with open(appcast_path, "a", encoding="utf-8") as f:
        f.write("\n")

    channel_kind = "beta" if is_prerelease else "stable"
    print(f"appended {version} ({channel_kind}) -> {appcast_path}")
    print(f"  enclosure: {url}")
    print(f"  length={length} edSignature={sig[:16]}...")


if __name__ == "__main__":
    main()
