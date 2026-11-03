"""Every import in a `tools/` script names something that exists.

The real-input and screenshot scripts run only in their own CI lanes, so a
deleted helper module (plan 089 C2 removed `tools/screenshot/parity.py`)
breaks a lane at import time with nothing earlier to notice. This checks,
statically, that each absolute import is the standard library, a module or
package somewhere under `tools/`, or one of the third-party packages the
tools are known to use.
"""

from __future__ import annotations

import ast
import sys
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[2] / "tools"

THIRD_PARTY = {
    "gi",  # PyGObject, for iced_native_file_drop_check.py's GTK drag source
    "pytest",
}


def unresolved_imports(root: Path) -> dict[str, list[str]]:
    local = {p.stem for p in root.rglob("*.py")} | {p.name for p in root.rglob("*") if p.is_dir()}
    known = local | set(sys.stdlib_module_names) | THIRD_PARTY
    missing: dict[str, list[str]] = {}
    for path in sorted(root.rglob("*.py")):
        for node in ast.walk(ast.parse(path.read_text(), filename=str(path))):
            if isinstance(node, ast.Import):
                names = [alias.name for alias in node.names]
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                names = [node.module]
            else:
                continue
            for name in names:
                top = name.split(".")[0]
                if top not in known:
                    missing.setdefault(top, []).append(str(path.relative_to(root)))
    return missing


class ToolImportsTests(unittest.TestCase):
    def test_every_import_resolves(self) -> None:
        self.assertEqual(unresolved_imports(TOOLS), {})

    def test_a_deleted_module_is_reported(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "check.py").write_text("import json\nimport helper\nimport parity\n")
            (root / "helper.py").write_text("")
            self.assertEqual(unresolved_imports(root), {"parity": ["check.py"]})


if __name__ == "__main__":
    unittest.main()
