"""`ci.yml`'s required-check contract (plan 089 D4).

`ci-success` derives the jobs it gates from `toJSON(needs)`, so a job that
is missing from its `needs` is silently not gated — the check stays green
over less. This pins, statically:

* every job but `ci-success` is in its `needs`, or on `NON_BLOCKING`;
* every `needs` entry anywhere names a defined job;
* `ci-success` runs always, and still fails a `changes` that did not
  succeed (its `skipped` allowance is only safe behind that check);
* every `needs.changes.outputs.<name>` a job reads is an output `changes`
  declares, and every output is a filter that exists;
* every path pattern in the `changes` filters matches a file in the tree,
  so a renamed or deleted input cannot leave its filter silently dead.

Parsed as text, like `tools/release/dry_run.py` reads `release.yml`: the
harness-unit job runs a bare `python3` with no PyYAML. Every shape the
parser does not know raises rather than guessing.
"""

from __future__ import annotations

import os
import re
import subprocess
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CI = REPO / ".github" / "workflows" / "ci.yml"

AGGREGATE = "ci-success"
# Jobs deliberately outside the required check. The reason for each lives in
# its job comment in ci.yml.
NON_BLOCKING = {"e2e-iced-wayland-drag"}

JOB_KEY = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")
NEEDS = re.compile(r"^    needs:(.*)$")
OUTPUT_REF = re.compile(r"needs\.changes\.outputs\.([A-Za-z0-9_-]+)")


def _strip_comment(line: str) -> str:
    return re.split(r"\s#", line, maxsplit=1)[0].rstrip()


def jobs(text: str) -> dict[str, list[str]]:
    """Each job's body lines, keyed by job id, in file order."""
    lines = text.splitlines()
    try:
        start = lines.index("jobs:")
    except ValueError as error:
        raise AssertionError("ci.yml has no top-level `jobs:`") from error
    found: dict[str, list[str]] = {}
    current: str | None = None
    for line in lines[start + 1 :]:
        if line and not line.startswith(" ") and not line.startswith("#"):
            break
        if match := JOB_KEY.match(line):
            current = match.group(1)
            if current in found:
                raise AssertionError(f"job {current!r} is defined twice")
            found[current] = []
        elif current is not None:
            found[current].append(line)
    if not found:
        raise AssertionError("ci.yml defines no jobs")
    return found


def job_needs(body: list[str]) -> list[str]:
    """A job's `needs`, as a scalar, a flow list or a block list."""
    for i, line in enumerate(body):
        match = NEEDS.match(line)
        if not match:
            continue
        value = _strip_comment(match.group(1)).strip()
        if value.startswith("["):
            if not value.endswith("]"):
                raise AssertionError(f"multi-line flow `needs` is not supported: {line!r}")
            return [item.strip() for item in value[1:-1].split(",") if item.strip()]
        if value:
            return [value]
        items: list[str] = []
        for item in body[i + 1 :]:
            stripped = item.strip()
            if not stripped or stripped.startswith("#"):
                continue
            if not item.startswith("      - "):
                break
            items.append(_strip_comment(stripped[2:]).strip())
        if not items:
            raise AssertionError("an empty `needs:` block")
        return items
    return []


def changes_outputs(body: list[str]) -> dict[str, str]:
    """`changes`' declared outputs: name -> the filter output it forwards."""
    outputs: dict[str, str] = {}
    inside = False
    for line in body:
        if line == "    outputs:":
            inside = True
            continue
        if inside:
            if not line.startswith("      "):
                break
            match = re.match(
                r"^      ([A-Za-z0-9_-]+): \$\{\{ steps\.filter\.outputs\.([A-Za-z0-9_-]+) \}\}$",
                line,
            )
            if not match:
                raise AssertionError(f"unrecognised `changes` output: {line!r}")
            outputs[match.group(1)] = match.group(2)
    if not outputs:
        raise AssertionError("the `changes` job declares no outputs")
    return outputs


def path_filters(body: list[str]) -> dict[str, list[str]]:
    """The paths-filter's `filters: |` block: filter name -> its literal path
    patterns. A `*anchor` alias contributes the anchored filter's patterns,
    so an aliasing filter is never empty."""
    try:
        start = next(i for i, line in enumerate(body) if line.strip() == "filters: |")
    except StopIteration as error:
        raise AssertionError("the `changes` job has no `filters: |` block") from error
    indent = len(body[start]) - len(body[start].lstrip())
    filters: dict[str, list[str]] = {}
    anchors: dict[str, str] = {}
    current: str | None = None
    for line in body[start + 1 :]:
        if line.strip() and len(line) - len(line.lstrip()) <= indent:
            break
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if match := re.match(r"^([A-Za-z0-9_-]+):(?:\s+&([A-Za-z0-9_-]+))?$", stripped):
            current = match.group(1)
            filters[current] = []
            if match.group(2):
                anchors[match.group(2)] = current
        elif match := re.match(r"^- '([^']+)'$", stripped):
            if current is None:
                raise AssertionError(f"a pattern before any filter: {line!r}")
            filters[current].append(match.group(1))
        elif match := re.match(r"^- \*([A-Za-z0-9_-]+)$", stripped):
            if current is None or match.group(1) not in anchors:
                raise AssertionError(f"an alias of an unknown anchor: {line!r}")
            filters[current].extend(filters[anchors[match.group(1)]])
        else:
            raise AssertionError(f"unrecognised filter line: {line!r}")
    if not filters:
        raise AssertionError("the paths-filter defines no filters")
    return filters


def glob_regex(pattern: str) -> re.Pattern[str]:
    """picomatch's reading of a paths-filter pattern (`dot: true`): `**`
    spans directories, `*` and `?` stay inside one."""
    out = ""
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out += "(?:.*/)?"
            i += 3
        elif pattern.startswith("**", i):
            out += ".*"
            i += 2
        elif pattern[i] == "*":
            out += "[^/]*"
            i += 1
        elif pattern[i] == "?":
            out += "[^/]"
            i += 1
        elif pattern[i] in "[{!":
            raise AssertionError(f"unsupported glob syntax in {pattern!r}")
        else:
            out += re.escape(pattern[i])
            i += 1
    return re.compile(out + r"\Z")


def tree_files() -> list[str]:
    """Every file a checkout carries: tracked or new, minus deleted and
    ignored ones."""
    listed = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=REPO, capture_output=True, check=True,
    ).stdout.decode().split("\0")
    return [path for path in listed if path and os.path.lexists(REPO / path)]


def problems(text: str, files: list[str]) -> list[str]:
    found: list[str] = []
    defined = jobs(text)
    if AGGREGATE not in defined:
        return [f"no {AGGREGATE} job"]
    for name, body in defined.items():
        for need in job_needs(body):
            if need not in defined:
                found.append(f"{name} needs undefined job {need!r}")
    gated = set(job_needs(defined[AGGREGATE]))
    for name in defined:
        if name == AGGREGATE or name in NON_BLOCKING:
            continue
        if name not in gated:
            found.append(f"{name} is neither in {AGGREGATE}.needs nor NON_BLOCKING")
    for name in sorted(NON_BLOCKING):
        if name not in defined:
            found.append(f"NON_BLOCKING names {name!r}, which is not a job")
        elif name in gated:
            found.append(f"{name} is in {AGGREGATE}.needs and NON_BLOCKING")
    aggregate = "\n".join(defined[AGGREGATE])
    if not re.search(r"^    if: always\(\)$", aggregate, re.M):
        found.append(f"{AGGREGATE} must run `if: always()`")
    if "toJSON(needs)" not in aggregate:
        found.append(f"{AGGREGATE} no longer derives its job set from toJSON(needs)")
    if not re.search(r"\.changes\.result'\)\"\n\s+if \[ \"\$\{changes_result\}\" != \"success\" \]", aggregate):
        found.append(f"{AGGREGATE} no longer fails a `changes` that did not succeed")
    if "changes" not in defined:
        return found + ["no `changes` job"]
    outputs = changes_outputs(defined["changes"])
    filters = path_filters(defined["changes"])
    for output, source in outputs.items():
        if source not in filters:
            found.append(f"changes output {output!r} forwards a filter that does not exist: {source!r}")
    for name, body in defined.items():
        for ref in sorted(set(OUTPUT_REF.findall("\n".join(body)))):
            if ref not in outputs:
                found.append(f"{name} reads needs.changes.outputs.{ref}, which `changes` does not declare")
    for name, patterns in filters.items():
        if not patterns:
            found.append(f"filter {name!r} has no patterns")
        for pattern in patterns:
            regex = glob_regex(pattern)
            if not any(regex.match(path) for path in files):
                found.append(f"filter {name!r}: {pattern!r} matches no file in the tree")
    return found


class CiContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.text = CI.read_text()
        cls.files = tree_files()

    def test_the_workflow_keeps_the_contract(self) -> None:
        self.assertEqual(problems(self.text, self.files), [])

    def test_the_non_blocking_lane_stays_out_of_the_required_check(self) -> None:
        gated = set(job_needs(jobs(self.text)[AGGREGATE]))
        self.assertFalse(gated & NON_BLOCKING)

    def test_a_job_dropped_from_needs_is_caught(self) -> None:
        needs = next(line for line in self.text.splitlines() if line.startswith("    needs: [changes, "))
        mutated = self.text.replace(needs, needs.replace(", session-e2e", ""), 1)
        self.assertNotEqual(mutated, self.text)
        self.assertIn(
            f"session-e2e is neither in {AGGREGATE}.needs nor NON_BLOCKING",
            problems(mutated, self.files),
        )

    def test_a_new_job_outside_needs_is_caught(self) -> None:
        mutated = self.text.replace(
            f"\n  {AGGREGATE}:\n",
            "\n  mobile:\n    needs: changes\n    runs-on: ubuntu-latest\n\n"
            f"  {AGGREGATE}:\n",
            1,
        )
        self.assertIn(f"mobile is neither in {AGGREGATE}.needs nor NON_BLOCKING", problems(mutated, self.files))

    def test_a_filter_path_that_matches_nothing_is_caught(self) -> None:
        mutated = self.text.replace(
            "              - 'tools/release/**'\n",
            "              - 'tools/release/**'\n              - 'mac/Sources/**'\n",
            1,
        )
        self.assertNotEqual(mutated, self.text)
        self.assertIn(
            "filter 'harness': 'mac/Sources/**' matches no file in the tree",
            problems(mutated, self.files),
        )

    def test_a_read_of_an_undeclared_output_is_caught(self) -> None:
        mutated = self.text.replace(
            "needs.changes.outputs.scripts == 'true'",
            "needs.changes.outputs.mac == 'true'",
            1,
        )
        self.assertNotEqual(mutated, self.text)
        self.assertIn(
            "scripts reads needs.changes.outputs.mac, which `changes` does not declare",
            problems(mutated, self.files),
        )

    def test_dropping_the_changes_check_is_caught(self) -> None:
        mutated = self.text.replace('if [ "${changes_result}" != "success" ]', "if false", 1)
        self.assertNotEqual(mutated, self.text)
        self.assertIn(
            f"{AGGREGATE} no longer fails a `changes` that did not succeed",
            problems(mutated, self.files),
        )


class ParserTests(unittest.TestCase):
    def test_needs_forms(self) -> None:
        self.assertEqual(job_needs(["    needs: changes"]), ["changes"])
        self.assertEqual(job_needs(["    needs: [a, b ,c]"]), ["a", "b", "c"])
        self.assertEqual(job_needs(["    needs:", "      - a", "      # x", "      - b", "    if: x"]), ["a", "b"])
        self.assertEqual(job_needs(["    runs-on: x"]), [])
        with self.assertRaises(AssertionError):
            job_needs(["    needs: [a,", "      b]"])

    def test_globs_follow_picomatch(self) -> None:
        self.assertTrue(glob_regex("crates/**").match("crates/a/b.rs"))
        self.assertTrue(glob_regex("crates/*/Cargo.toml").match("crates/roost-ipc/Cargo.toml"))
        self.assertFalse(glob_regex("crates/*/Cargo.toml").match("crates/a/b/Cargo.toml"))
        self.assertTrue(glob_regex("mac/scripts/notarize*.sh").match("mac/scripts/notarize_test.sh"))
        self.assertTrue(glob_regex(".mise.toml").match(".mise.toml"))
        self.assertFalse(glob_regex(".mise.toml").match("x.mise.toml"))
        self.assertFalse(glob_regex("Makefile").match("tools/Makefile"))
        with self.assertRaises(AssertionError):
            glob_regex("a/{b,c}")


if __name__ == "__main__":
    unittest.main()
