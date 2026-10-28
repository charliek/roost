"""tools/release/dry_run.py turns the real release.yml into a publish-free copy.

Runs the generator on the real `.github/workflows/release.yml` and checks the
output's structure with indentation-level parsing (the unit tests are
stdlib-only, so no PyYAML).
"""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
TOOL = REPO_ROOT / "tools" / "release" / "dry_run.py"
RELEASE_YML = REPO_ROOT / ".github" / "workflows" / "release.yml"

PUBLISHING_JOBS = {
    "ci-gate",
    "create-release",
    "publish-release",
    "appcast",
    "appcast-iced",
    "dispatch-apt-charliek",
}
BUILD_JOBS = {"linux", "mac", "mac-iced"}


def load_tool():
    spec = importlib.util.spec_from_file_location("release_dry_run", TOOL)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def split_jobs(text: str) -> dict[str, str]:
    lines = text.splitlines(keepends=True)
    at = lines.index("jobs:\n")
    jobs: dict[str, list[str]] = {}
    cur = None
    for ln in lines[at + 1 :]:
        m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", ln)
        if m:
            cur = m.group(1)
            jobs[cur] = []
        elif cur is not None:
            jobs[cur].append(ln)
    return {k: "".join(v) for k, v in jobs.items()}


def outside_shim(text: str) -> str:
    return re.sub(r"<<'SHIM'\n.*?\n\s*SHIM\n", "", text, flags=re.S)


class DryRunTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tool = load_tool()
        cls.tmp = tempfile.TemporaryDirectory()
        cls.out = Path(cls.tmp.name) / "release-dryrun.yml"
        cls.tool.main(["--out", str(cls.out)])
        cls.text = cls.out.read_text()
        cls.jobs = split_jobs(cls.text)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_no_publishing_job_remains(self):
        self.assertEqual(PUBLISHING_JOBS & set(self.jobs), set())

    def test_every_build_job_is_kept_and_needs_version_check(self):
        self.assertEqual(set(self.jobs), BUILD_JOBS | {"version-check"})
        for name in BUILD_JOBS:
            self.assertRegex(
                self.jobs[name], r"(?m)^    needs: version-check$", name
            )
            self.assertNotIn("create-release", re.sub(r"(?m)^\s*#.*$", "", self.jobs[name]))

    def test_trigger_is_the_dryrun_branch_glob_only(self):
        head = self.text.split("\njobs:\n")[0]
        self.assertIn("on:\n  push:\n    branches:\n      - 'dryrun/**'\n", head)
        self.assertNotIn("tags:", head)

    def test_ref_name_is_gone_and_dryrun_tag_matches_cargo(self):
        self.assertNotIn("GITHUB_REF_NAME", self.text)
        self.assertNotIn("github.ref_name", self.text)
        cargo = (REPO_ROOT / "Cargo.toml").read_text()
        section = cargo.split("[workspace.package]", 1)[1]
        version = re.search(r'(?m)^version\s*=\s*"([^"]+)"', section).group(1)
        self.assertIn(f"\nenv:\n  DRYRUN_TAG: v{version}\n", self.text)

    def test_no_gh_release_view_or_edit_outside_the_shim(self):
        body = outside_shim(self.text)
        self.assertNotRegex(body, r"gh release (view|edit)")

    def test_sparkle_signing_and_release_body_steps_are_gone(self):
        self.assertNotRegex(self.text, r"sign(-iced)?\.txt")
        self.assertNotIn("sparkle-sign", self.text)
        self.assertNotIn("Sign the DMG for Sparkle", self.text)
        self.assertNotIn("Append macOS first-launch note", self.text)

    def test_notarization_is_forced_off(self):
        for name in ("mac", "mac-iced"):
            self.assertIn("\n      CAN_NOTARIZE: 'false'\n", self.jobs[name])
        self.assertNotRegex(self.text, r"CAN_NOTARIZE: \$\{\{")

    def test_artifact_names_are_unique_per_cell(self):
        names = re.findall(r"(?m)^          name: (dryrun-.+)$", self.text)
        self.assertEqual(
            sorted(names),
            ["dryrun-linux-${{ matrix.arch }}", "dryrun-mac-arm64", "dryrun-mac-iced-arm64"],
        )
        self.assertEqual(len(set(names)), len(names))
        for name in BUILD_JOBS:
            job = self.jobs[name]
            self.assertEqual(job.count("uses: actions/upload-artifact"), 1, name)
            self.assertIn("          retention-days: 1\n", job)
            self.assertIn("          if-no-files-found: error\n", job)

    def test_permissions_are_read_only_everywhere(self):
        head = self.text.split("\njobs:\n")[0]
        self.assertIn("\npermissions:\n  contents: read\n", head)
        self.assertNotIn("contents: write", self.text)
        self.assertNotRegex(self.text, r"(?m)^\s+(id-token|pull-requests|actions): write")
        for name in BUILD_JOBS:
            self.assertIn("    permissions:\n      contents: read\n", self.jobs[name])

    def test_the_gh_shim_is_the_first_step_of_each_build_job(self):
        for name in BUILD_JOBS:
            steps = re.findall(r"(?m)^      - (?:name|uses): (.+)$", self.jobs[name])
            self.assertEqual(steps[0], "Install the dry-run gh shim", name)

    def test_no_production_secret_survives(self):
        code = "\n".join(
            ln for ln in self.text.splitlines() if not ln.lstrip().startswith("#")
        )
        self.assertNotRegex(code, r"\bsecrets\b(?!\.GITHUB_TOKEN\b)")
        for gone in (
            "Verify the Sparkle key material",
            "Verify the iced Sparkle key material",
            "Import Developer ID certificate",
            "Notarize + staple",
        ):
            self.assertNotIn(gone, self.text)

    def test_an_injected_secret_step_is_deleted_and_a_stray_secret_raises(self):
        src = RELEASE_YML.read_text()
        anchor = "      - name: Build .deb\n"
        self.assertIn(anchor, src)
        step = (
            "      - name: Injected\n        env:\n"
            "          K: ${{ secrets.SPARKLE_ED_PRIVATE_KEY }}\n        run: true\n\n"
        )
        out = self.tool.transform(src.replace(anchor, step + anchor, 1), "0.0.0")
        self.assertNotIn("Injected", out)
        self.assertNotIn("secrets.SPARKLE_ED_PRIVATE_KEY", out)

        stray = src.replace(
            "  version-check:\n",
            "  version-check:\n    env:\n      K: ${{ secrets.SPARKLE_ED_PRIVATE_KEY }}\n",
            1,
        )
        self.assertNotEqual(stray, src)
        with self.assertRaises(SystemExit):
            self.tool.transform(stray, "0.0.0")

    def test_a_tag_only_condition_in_a_kept_job_raises(self):
        src = RELEASE_YML.read_text()
        for cond in (
            "startsWith(github.ref, 'refs/tags/')",
            "github.ref_name == 'v1'",
        ):
            mutated = src.replace("  linux:\n", f"  linux:\n    if: {cond}\n", 1)
            self.assertNotEqual(mutated, src)
            with self.assertRaises(SystemExit, msg=cond):
                self.tool.transform(mutated, "0.0.0")
        step_level = src.replace(
            "      - name: Build .deb\n",
            "      - name: Build .deb\n        if: startsWith(github.ref, 'refs/tags/')\n",
            1,
        )
        self.assertNotEqual(step_level, src)
        with self.assertRaises(SystemExit):
            self.tool.transform(step_level, "0.0.0")

    def test_a_step_reading_a_deleted_steps_output_raises(self):
        src = RELEASE_YML.read_text()
        mutated = src.replace(
            "      - name: Sign the DMG for Sparkle\n",
            "      - name: Sign the DMG for Sparkle\n        id: signed\n",
            1,
        ).replace(
            "if: steps.dmg_notarized.outputs.value != 'true'\n        run: |\n          echo \"::notice::Roost-",
            "if: steps.signed.outputs.value != 'true'\n        run: |\n          echo \"::notice::Roost-",
            1,
        )
        self.assertNotEqual(mutated, src)
        with self.assertRaises(SystemExit):
            self.tool.transform(mutated, "0.0.0")

    def test_unrecognised_release_yml_fails_loudly(self):
        src = RELEASE_YML.read_text()
        for needle in (
            "  publish-release:\n",
            "    needs: create-release\n",
            "      - name: Sign the DMG for Sparkle\n",
        ):
            mutated = src.replace(needle, needle.replace("a", "@", 1), 1)
            self.assertNotEqual(mutated, src)
            with self.assertRaises(SystemExit):
                self.tool.transform(mutated, "0.0.0")
        with self.assertRaises(SystemExit):
            self.tool.transform(src + "\n  brand-new-job:\n    runs-on: x\n", "0.0.0")


class GhShimTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tool = load_tool()
        out = tool.transform(RELEASE_YML.read_text(), "0.0.0")
        m = re.search(r"<<'SHIM'\n(.*?)\n\s*SHIM\n", out, flags=re.S)
        if not m:
            raise AssertionError("no gh shim heredoc in the generated workflow")
        lines = m.group(1).splitlines()
        indent = min(len(ln) - len(ln.lstrip()) for ln in lines if ln.strip())
        cls.shim = "\n".join(ln[indent:] for ln in lines) + "\n"

    def run_shim(self, *args, files=()):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "bin").mkdir()
            shim = root / "bin" / "gh"
            shim.write_text(self.shim)
            shim.chmod(0o755)
            real = root / "real"
            real.mkdir()
            realgh = real / "gh"
            realgh.write_text("#!/bin/sh\necho REAL-GH \"$@\"\n")
            realgh.chmod(0o755)
            for f in files:
                (root / f).write_text("x")
            env = {
                "PATH": f"{root / 'bin'}:{real}:/usr/bin:/bin",
                "RUNNER_TEMP": str(root / "tmp"),
            }
            p = subprocess.run(
                [str(shim), *args], cwd=root, env=env, capture_output=True, text=True
            )
            staged = sorted(
                x.name for x in (root / "tmp" / "dryrun").glob("*")
            ) if (root / "tmp" / "dryrun").exists() else []
            return p, staged

    def test_upload_stages_files_and_never_reaches_real_gh(self):
        p, staged = self.run_shim(
            "release", "upload", "v1", "a.deb#Label one", "b.sha256#Label — two",
            "--clobber", files=("a.deb", "b.sha256"),
        )
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(staged, ["a.deb", "b.sha256"])
        self.assertNotIn("REAL-GH", p.stdout)

    def test_upload_of_a_missing_file_fails(self):
        p, _ = self.run_shim("release", "upload", "v1", "nope.deb#L", "--clobber")
        self.assertNotEqual(p.returncode, 0)

    def test_view_and_edit_fail(self):
        for sub in ("view", "edit"):
            p, _ = self.run_shim("release", sub, "v1")
            self.assertNotEqual(p.returncode, 0, sub)
            self.assertNotIn("REAL-GH", p.stdout)

    def test_other_subcommands_pass_through(self):
        p, _ = self.run_shim("api", "repos/x/y")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("REAL-GH api repos/x/y", p.stdout)


if __name__ == "__main__":
    unittest.main()
