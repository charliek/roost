#!/usr/bin/env python3
"""Assert a draft release carries exactly the assets a release ships.

    gh release view <tag> --json assets \\
        -q '.assets[] | [.name, (.size|tostring)] | @tsv' \\
      | python3 tools/release/check_assets.py <tag>

Reads one `name<TAB>size` line per asset on stdin. Exits 0 when the draft
holds exactly one of each expected asset, each plausibly sized, and nothing
else; otherwise prints one `::error::` line per problem and exits 1.
`release.yml`'s `publish-release` runs it before the irreversible publish.

The nine assets: the two `.deb`s, `Roost-<ver>.dmg`, and a standalone
`roost-session` binary per published platform with its `.sha256` sidecar
(the bootstrap install rung fetches those two directly —
`crates/roost-ipc/src/bootstrap.rs`, `asset_name`/`checksum_name`).

Two size floors. The six binaries and packages must be at least 1 MiB:
every one is multi-megabyte, and a truncated upload lands above zero, so
"non-empty" would wave through exactly the corruption worth catching. A
sidecar's floor is the smallest record its reader accepts — 64 hex digits,
a separator, and the asset's file name (`parse_checksum_file` rejects a
bare hash) — so a sidecar the feature cannot use is never published.

GitHub rewrites an uploaded file name, replacing anything outside
`[A-Za-z0-9._-]` with a dot, so the `~` nfpm puts in a prerelease Debian
version arrives as `.` (`roost_0.1.0~rc1_arm64.deb` is stored as
`roost_0.1.0.rc1_arm64.deb`). Every expected name is compared after that
rewrite. The session assets keep the tag's own spelling, as the linux job
names them.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Iterable

MIN_ARTIFACT_BYTES = 1024 * 1024
SESSION_PLATFORMS = ("linux-amd64", "linux-arm64", "darwin-arm64")


def sanitize(name: str) -> str:
    """GitHub's rewrite of an uploaded asset's file name."""
    return re.sub(r"[^A-Za-z0-9._-]", ".", name)


def expected_assets(tag: str) -> dict[str, int]:
    """Each expected asset's stored name, mapped to its size floor."""
    ver = tag[1:] if tag.startswith("v") else tag
    # nfpm splits the version at the FIRST hyphen and rejoins with `~`
    # (0.1.0-rc-2 -> 0.1.0~rc-2), the same substitution the linux job makes.
    deb_ver = ver.replace("-", "~", 1)
    artifacts = [
        f"roost_{deb_ver}_amd64.deb",
        f"roost_{deb_ver}_arm64.deb",
        f"Roost-{ver}.dmg",
        *(f"roost-session-{ver}-{platform}" for platform in SESSION_PLATFORMS),
    ]
    floors = {sanitize(name): MIN_ARTIFACT_BYTES for name in artifacts}
    for platform in SESSION_PLATFORMS:
        asset = f"roost-session-{ver}-{platform}"
        floors[sanitize(f"{asset}.sha256")] = 64 + 1 + len(asset)
    return floors


def parse_assets(lines: Iterable[str]) -> list[tuple[str, int]]:
    assets = []
    for number, line in enumerate(lines, 1):
        line = line.rstrip("\n")
        if not line.strip():
            continue
        name, sep, size = line.partition("\t")
        if not sep or not size.isdigit():
            raise ValueError(f"line {number} is not `name<TAB>size`: {line!r}")
        assets.append((name, int(size)))
    return assets


def problems(tag: str, assets: list[tuple[str, int]]) -> list[str]:
    expected = expected_assets(tag)
    found: dict[str, list[int]] = {}
    for name, size in assets:
        found.setdefault(name, []).append(size)
    out = []
    for name, floor in expected.items():
        sizes = found.get(name, [])
        if len(sizes) != 1:
            out.append(f"expected exactly one asset named {name} on {tag}, found {len(sizes)}.")
        elif sizes[0] < floor:
            out.append(
                f"asset {name} on {tag} is only {sizes[0]} bytes (expected at least"
                f" {floor}) — it looks truncated."
            )
    for name in sorted(set(found) - set(expected)):
        out.append(f"unexpected asset {name} on {tag} (a stale upload from an earlier attempt?).")
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("tag", help="the release tag, e.g. v0.1.0 or v0.1.0-rc1")
    args = ap.parse_args(argv)
    try:
        assets = parse_assets(sys.stdin)
    except ValueError as error:
        print(f"::error::{error}")
        return 1
    found = problems(args.tag, assets)
    for problem in found:
        print(f"::error::{problem}")
    if found:
        return 1
    print(f"{args.tag}: all {len(expected_assets(args.tag))} expected assets present and sized.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
