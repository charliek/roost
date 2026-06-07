// Tests for `pathDisplay`, the chrome-subtitle path formatter:
// home-directory collapse to `~`, character-counted truncation that
// keeps the right end, and the `max <= 0` guards. The Linux UI shares
// the same rules so the subtitle string matches across both UIs for
// the same state.

import Foundation
import Testing
@testable import Roost

@Test
func pathDisplay_homeItself() {
    #expect(pathDisplay("/home/charliek", home: "/home/charliek", max: 48) == "~")
}

@Test
func pathDisplay_homeChild() {
    #expect(
        pathDisplay("/home/charliek/projects/roost", home: "/home/charliek", max: 48)
            == "~/projects/roost"
    )
}

@Test
func pathDisplay_homePrefixNotBoundary() {
    // "/home/charliek" is a prefix of "/home/charlieknudsen" but not a
    // path boundary — must not collapse.
    #expect(
        pathDisplay("/home/charlieknudsen/x", home: "/home/charliek", max: 48)
            == "/home/charlieknudsen/x"
    )
}

@Test
func pathDisplay_emptyHomeNoOp() {
    #expect(pathDisplay("/var/log", home: "", max: 48) == "/var/log")
}

@Test
func pathDisplay_unrelatedPath() {
    #expect(pathDisplay("/var/log", home: "/home/charliek", max: 48) == "/var/log")
}

@Test
func pathDisplay_truncateKeepsRight() {
    #expect(pathDisplay("/a/b/c/d/e/f", home: "", max: 7) == "…/d/e/f")
}

@Test
func pathDisplay_noTruncateWhenFits() {
    #expect(pathDisplay("/a/b/c", home: "", max: 10) == "/a/b/c")
}

@Test
func pathDisplay_truncateRespectsRunes() {
    // 🐓 is a multi-byte Character; the truncator counts characters,
    // not bytes, so it must not slice mid-codepoint.
    #expect(pathDisplay("/aaaa/🐓🐓🐓", home: "", max: 6) == "…a/🐓🐓🐓")
}

@Test
func pathDisplay_homeThenTruncate() {
    #expect(
        pathDisplay(
            "/home/charliek/very/deep/tree/leaf",
            home: "/home/charliek",
            max: 12
        ) == "…p/tree/leaf"
    )
}

@Test
func pathDisplay_emptyPathIsEmpty() {
    // The window subtitle path passes "" when there's no project; the
    // helper should be transparent for that — no crash, returns "".
    #expect(pathDisplay("", home: "/home/charliek", max: 48) == "")
}

@Test
func pathDisplay_zeroMaxReturnsEmpty() {
    // CodeRabbit-flagged guard (PR #67): `max <= 0` must not trap.
    // Returning "" is the documented behavior — "render zero
    // characters."
    #expect(pathDisplay("/a/b/c", home: "", max: 0) == "")
    #expect(pathDisplay("/home/charliek/foo", home: "/home/charliek", max: 0) == "")
}

@Test
func pathDisplay_negativeMaxReturnsEmpty() {
    // Same guard, but with a negative value — used to drive a runtime
    // trap via `Collection.suffix(max - 1)` on the truncate branch.
    #expect(pathDisplay("/a/b/c", home: "", max: -5) == "")
    #expect(pathDisplay("/long/path", home: "", max: Int.min) == "")
}
