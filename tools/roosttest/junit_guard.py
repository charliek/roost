#!/usr/bin/env python3
"""Fail unless a JUnit report proves every real-input scenario ran and passed (plan 075 §D4).

    python3 tools/roosttest/junit_guard.py <junit.xml> --scenarios tools/roosttest/test_real_input_mac.py

The expected names are the module's `SCENARIOS`, read with `ast` because the
module imports pytest, which this script's interpreter may not have. A step
that exits green while a scenario was deselected, skipped, or errored in
setup or teardown is the failure this exists to catch.
"""

from __future__ import annotations

import argparse
import ast
import sys
import xml.etree.ElementTree as ET
from collections import Counter
from pathlib import Path


def scenarios_and_tests(module: Path) -> tuple[list[str], list[str]]:
    """`SCENARIOS` and the module's top-level test names, in source order."""
    tree = ast.parse(Path(module).read_text())
    listed: list[str] = []
    tests: list[str] = []
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
            isinstance(target, ast.Name) and target.id == "SCENARIOS" for target in node.targets
        ):
            listed = list(ast.literal_eval(node.value))
        elif isinstance(node, ast.FunctionDef) and node.name.startswith("test_"):
            if node.decorator_list:
                raise AssertionError(
                    f"{node.name} is decorated: a parametrized test's ids are not its name"
                )
            tests.append(node.name)
    return listed, tests


def problems(xml_text: str, expected: list[str]) -> list[str]:
    """Why this report does not prove the run, or an empty list when it does."""
    try:
        root = ET.fromstring(xml_text)
    except ET.ParseError as error:
        return [f"not parseable XML: {error}"]
    if root.tag not in ("testsuites", "testsuite"):
        return [f"unexpected root element <{root.tag}>"]
    cases = list(root.iter("testcase"))
    names = Counter(case.get("name", "") for case in cases)
    found: list[str] = []
    if not expected:
        found.append("the expected scenario list is empty")
    for name in sorted(set(expected) - set(names)):
        found.append(f"missing scenario: {name}")
    for name in sorted(set(names) - set(expected)):
        found.append(f"unexpected testcase: {name!r}")
    for name, count in sorted(names.items()):
        if count > 1:
            found.append(f"duplicate testcase: {name!r} x{count}")
    for case in cases:
        for child in case:
            if child.tag in ("skipped", "failure", "error"):
                detail = child.get("message") or child.get("type") or ""
                found.append(f"{case.get('name')}: <{child.tag}> {detail}".rstrip())
    return found


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("junit", type=Path)
    parser.add_argument("--scenarios", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        xml_text = args.junit.read_text()
    except OSError as error:
        print(f"junit_guard: cannot read {args.junit}: {error}", file=sys.stderr)
        return 1
    expected, _ = scenarios_and_tests(args.scenarios)
    found = problems(xml_text, expected)
    if found:
        print(f"junit_guard: {args.junit} does not prove the real-input run:", file=sys.stderr)
        for line in found:
            print(f"  - {line}", file=sys.stderr)
        return 1
    print(f"junit_guard: all {len(expected)} scenarios passed ({args.junit})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
