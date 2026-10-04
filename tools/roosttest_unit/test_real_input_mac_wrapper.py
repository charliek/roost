"""The Mac real-input wrapper's lifecycle (`tools/input/mac/runner.py`, plan 074 §D7).

Driven against a fake `roost-input-mac` (a shell script that keeps the real
helper's contract: `<outdir>/pid` first, `<outdir>/held.json` for what it
holds, one JSON line, the exit code), so it runs on any OS: results map onto
skip / refusal / failure; after any unsuccessful outcome exactly the journaled
input is released, and nothing when nothing was pressed; a helper past its
timeout is stopped only while its identity is proven; and runner mode launches
through `open -g -n` with absolute paths and a binary staged off any external
volume.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "input" / "mac"))

import runner  # noqa: E402

# `runner.subprocess` is this same module, so the stand-in for `open` below
# keeps the real `run` for everything else.
REAL_RUN = subprocess.run

HELD = '{"modifiers":["alt-left"],"keys":[11],"buttons":[]}'

FAKE_HELPER = r"""#!/bin/sh
# argv: --outdir DIR --deadline-ms N <command> [args...]
outdir="$2"
echo $$ > "$outdir/pid"
shift 4
echo "$*" >> "$FAKE_CALLS"
# What this run "pressed" before it ended, as the real helper journals it.
[ -n "$FAKE_HELD" ] && printf '%s\n' "$FAKE_HELD" > "$outdir/held.json"
case "$1" in
  ok) echo '{"ok":true,"command":"ok","released":[]}' ;;
  key) echo '{"ok":true,"command":"key","released":[]}' ;;
  release-held)
    printf '{"modifiers":[],"keys":[],"buttons":[]}\n' > "$3"
    echo '{"ok":true,"command":"release-held","released_from_journal":[{"modifier":"alt-left","posted":true}]}' ;;
  unavailable) echo '{"ok":false,"kind":"unavailable","error":"no grant"}'; exit 3 ;;
  refused) echo '{"ok":false,"kind":"refused","error":"pid 9 is not frontmost"}'; exit 4 ;;
  garbage) echo 'not json'; exit 5 ;;
  failed) echo '{"ok":false,"kind":"failed","error":"AXError -25204"}'; exit 5 ;;
  panic) echo '{"ok":false,"kind":"panic","error":"boom"}'; exit 101 ;;
  deadline) echo '{"ok":false,"kind":"deadline","error":"late"}'; exit 124 ;;
  term) kill -TERM $$ ;;
  terminated) echo '{"ok":false,"kind":"terminated","error":"signal 15"}'; exit 143 ;;
  hang) sleep 30 ;;
  stubborn) trap '' TERM; sleep 30 ;;
  event-tap)
    : > "$outdir/ready"
    while [ ! -e "$outdir/stop" ]; do sleep 0.05; done
    echo '{"ok":true,"command":"event-tap","key_downs":[0]}' ;;
esac
"""


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class FakeHelperCase(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="ri-wrapper-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.binary = self.root / "bin" / runner.BINARY_NAME
        self.binary.parent.mkdir()
        self.binary.write_text(FAKE_HELPER)
        self.binary.chmod(self.binary.stat().st_mode | stat.S_IXUSR)
        self.calls = self.root / "calls"
        self.calls.touch()
        env = patch.dict(os.environ, {"FAKE_CALLS": str(self.calls)})
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("FAKE_HELD", None)
        self.artifacts = self.root / "artifacts"

    def helper(self, **kwargs) -> runner.Helper:
        helper = runner.Helper(self.artifacts, launch_mode="direct", binary=self.binary, **kwargs)
        self.addCleanup(helper.close)
        return helper

    def called(self) -> list[str]:
        return self.calls.read_text().splitlines()

    def pressing(self) -> None:
        """Every helper run from here on journals `HELD` before it ends."""
        os.environ["FAKE_HELD"] = HELD

    def assert_released_from(self, call: str, outdir: str) -> None:
        self.assertTrue(call.startswith("release-held --file "), call)
        self.assertTrue(call.endswith(f"/{outdir}/held.json"), call)


class ResultTests(FakeHelperCase):
    def test_a_result_is_the_json_line_and_its_outdir_is_kept(self) -> None:
        result = self.helper().run("ok")
        self.assertEqual(result["command"], "ok")
        kept = self.artifacts / "helper" / "001-ok"
        self.assertEqual((kept / "status").read_text().strip(), "0")
        self.assertTrue((kept / "pid").read_text().strip().isdigit())
        self.assertIn('"ok":true', (kept / "stdout").read_text())

    def test_unavailable_refused_and_garbage_map_onto_their_exceptions(self) -> None:
        helper = self.helper()
        with self.assertRaisesRegex(runner.RealInputUnavailable, "no grant"):
            helper.run("unavailable")
        with self.assertRaisesRegex(runner.RealInputRefused, "not frontmost"):
            helper.run("refused")
        with self.assertRaisesRegex(runner.RealInputError, "without a JSON result"):
            helper.run("garbage")

    CASES = (
        ("term", runner.RealInputError, "exited 143 without a JSON result"),
        ("terminated", runner.RealInputError, "kind=terminated, status=143"),
        ("panic", runner.RealInputError, "kind=panic, status=101"),
        ("deadline", runner.RealInputError, "status=124"),
        ("failed", runner.RealInputError, "status=5"),
        ("garbage", runner.RealInputError, "without a JSON result"),
        ("refused", runner.RealInputRefused, "not frontmost"),
        ("unavailable", runner.RealInputUnavailable, "no grant"),
    )

    def test_an_unsuccessful_outcome_releases_exactly_the_journaled_input(self) -> None:
        self.pressing()
        helper = self.helper()
        for index, (command, error, reason) in enumerate(self.CASES):
            with self.subTest(command=command):
                self.calls.write_text("")
                with self.assertRaisesRegex(error, f"{reason}.*tracked release: .*alt-left"):
                    helper.run(command)
                called = self.called()
                self.assertEqual(called[0], command)
                self.assertEqual(len(called), 2, called)
                self.assert_released_from(called[1], f"{2 * index + 1:03d}-{command}")

    def test_a_failure_with_nothing_pressed_releases_nothing(self) -> None:
        helper = self.helper()
        for command, error, reason in self.CASES:
            with self.subTest(command=command):
                self.calls.write_text("")
                with self.assertRaisesRegex(error, f"{reason}.*tracked release: nothing held"):
                    helper.run(command)
                self.assertEqual(self.called(), [command])

    def test_an_outcome_that_cannot_be_read_still_releases_the_journaled_input(self) -> None:
        """The helper ended holding input, then reading its status or its
        output fails: the tracked release still runs."""
        real_exited, real_text = runner.Helper._exited, runner._text
        cases = (
            ("status", "_exited", OSError(28, "No space left on device")),
            ("status", "_exited", ValueError("invalid literal for int()")),
            ("stdout", "_text", OSError(5, "Input/output error")),
        )
        for index, (what, name, failure) in enumerate(cases):
            with self.subTest(what=what, failure=failure):
                self.pressing()
                self.calls.write_text("")
                failed = []

                def exited(helper, outdir, proc):
                    if outdir.name.endswith("-failed") and not failed:
                        failed.append(outdir)
                        raise failure
                    return real_exited(helper, outdir, proc)

                def text(path):
                    if path.name == "stdout" and path.parent.name.endswith("-failed") and not failed:
                        failed.append(path)
                        raise failure
                    return real_text(path)

                target, stand_in = {"_exited": (runner.Helper, exited), "_text": (runner, text)}[name]
                helper = self.helper()
                with patch.object(target, name, stand_in):
                    try:
                        helper.run("failed")
                    except runner.RealInputError as error:
                        message = str(error)
                    except Exception as error:  # noqa: BLE001 — the defect: it escapes
                        self.fail(f"{error!r} escaped without the tracked release")
                    else:
                        self.fail("an unreadable outcome returned a result")
                self.assertTrue(failed, "the stand-in never failed")
                self.assertRegex(message, "tracked release: .*alt-left")
                called = self.called()
                self.assertEqual(len(called), 2, called)
                self.assert_released_from(called[1], "001-failed")

    def test_no_automated_path_releases_system_wide(self) -> None:
        self.pressing()
        helper = self.helper()
        for command, error, _ in self.CASES:
            with self.assertRaises(error):
                helper.run(command)
        self.assertFalse([call for call in self.called() if "--release-all" in call])

    def test_success_releases_nothing_extra(self) -> None:
        self.pressing()
        self.helper().run("ok")
        self.assertEqual(self.called(), ["ok"])

    def test_a_refusal_is_a_failure_not_a_skip(self) -> None:
        self.assertTrue(issubclass(runner.RealInputRefused, AssertionError))
        self.assertFalse(issubclass(runner.RealInputRefused, runner.RealInputUnavailable))

    def test_an_ok_result_with_a_failing_status_is_an_error(self) -> None:
        with self.assertRaisesRegex(runner.RealInputError, "status=5"):
            runner._classify("x", 5, '{"ok":true}\n', "")


class TimeoutTests(FakeHelperCase):
    def setUp(self) -> None:
        super().setUp()
        # A short, unscaled slack: the wait is the test's whole cost.
        for name, value in (
            ("_LAUNCH_SLACK_S", 0.3),
            ("_TERM_GRACE_S", 0.3),
            ("_timeout_scale", lambda: 1.0),
        ):
            patcher = patch.object(runner, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def assert_gone(self, outdir: str) -> None:
        pid = int((self.artifacts / "helper" / outdir / "pid").read_text())
        deadline = time.monotonic() + 5
        while _alive(pid) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(_alive(pid), "the hung helper survived")

    def test_a_helper_past_its_timeout_is_terminated_then_its_input_released(self) -> None:
        self.pressing()
        helper = self.helper()
        with self.assertRaisesRegex(
            runner.RealInputError, "did not finish.*stopped pid [0-9]+ with SIGTERM.*tracked release: "
        ):
            helper.run("hang", deadline_ms=100)
        self.assert_gone("001-hang")
        called = self.called()
        self.assertEqual(called[0], "hang")
        self.assert_released_from(called[1], "001-hang")

    def test_a_helper_that_ignores_sigterm_is_killed(self) -> None:
        helper = self.helper()
        with self.assertRaisesRegex(runner.RealInputError, "stopped pid [0-9]+ with SIGKILL"):
            helper.run("stubborn", deadline_ms=100)
        self.assert_gone("001-stubborn")

    def test_the_release_after_a_timeout_never_releases_again(self) -> None:
        self.pressing()
        helper = self.helper()
        with self.assertRaises(runner.RealInputError):
            helper.run("hang", deadline_ms=100, _release=False)
        self.assertEqual(self.called(), ["hang"])

    def test_a_helper_whose_status_cannot_be_read_is_stopped_before_the_release(self) -> None:
        self.pressing()
        helper = self.helper()
        real_exited = runner.Helper._exited

        def exited(helper, outdir, proc):
            if outdir.name.endswith("-hang"):
                raise OSError(5, "Input/output error")
            return real_exited(helper, outdir, proc)

        try:
            with patch.object(runner.Helper, "_exited", exited):
                with self.assertRaisesRegex(
                    runner.RealInputError,
                    "could not be read.*stopped pid [0-9]+ with SIGTERM.*tracked release: .*alt-left",
                ):
                    helper.run("hang", deadline_ms=100)
            self.assert_gone("001-hang")
        finally:
            pid = int((self.artifacts / "helper" / "001-hang" / "pid").read_text())
            if _alive(pid):
                os.killpg(pid, signal.SIGKILL)
        self.assertIs(runner.Helper._exited, real_exited)

    def test_a_crash_while_stopping_the_helper_still_releases_its_input(self) -> None:
        self.pressing()
        helper = self.helper()
        try:
            with patch.object(runner.Helper, "_kill", side_effect=ProcessLookupError(3, "gone")):
                try:
                    helper.run("hang", deadline_ms=100)
                except runner.RealInputError as error:
                    message = str(error)
                except ProcessLookupError as error:
                    self.fail(f"the race escaped as {error!r}, skipping the tracked release")
                else:
                    self.fail("a hung helper returned a result")
            self.assertRegex(message, "cleanup NOT confirmed.*tracked release: .*alt-left")
            called = self.called()
            self.assert_released_from(called[1], "001-hang")
        finally:
            pid = int((self.artifacts / "helper" / "001-hang" / "pid").read_text())
            if _alive(pid):
                os.killpg(pid, signal.SIGKILL)


class IdentityTests(unittest.TestCase):
    """Runner mode: the helper is not this process's child, so its identity
    is proven again immediately before each signal."""

    def setUp(self) -> None:
        self.outdir = Path(tempfile.mkdtemp(prefix="ri-identity-"))
        self.addCleanup(shutil.rmtree, self.outdir, True)
        # A stand-in helper: `--outdir <outdir>` on its command line, and it
        # ignores SIGTERM so SIGKILL is the only way it goes — once its trap is
        # set, which it says by creating `trapped`.
        trapped = self.outdir / "trapped"
        self.proc = subprocess.Popen(
            ["/bin/sh", "-c", "trap '' TERM; : > \"$1\"; while :; do sleep 0.05; done", "sh",
             str(trapped), "--outdir", str(self.outdir)],
            start_new_session=True,
        )
        deadline = time.monotonic() + 10
        while not trapped.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        # Reaped as soon as it dies, as the runner reaps a real helper, so a
        # dead stand-in does not linger as a zombie that still "exists".
        threading.Thread(target=self.proc.wait, daemon=True).start()
        self.addCleanup(self.reap)
        (self.outdir / "pid").write_text(f"{self.proc.pid}\n")
        self.ours = f"/bin/sh -c ... --outdir {self.outdir}"
        for name, value in (("_TERM_GRACE_S", 0.2), ("_PS_ATTEMPTS", 2)):
            patcher = patch.object(runner, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.helper = object.__new__(runner.Helper)

    def reap(self) -> None:
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGKILL)
        self.proc.wait(5)

    def test_an_unprovable_identity_is_never_signalled_nor_taken_as_gone(self) -> None:
        with patch.object(runner, "_command_line", return_value=None):
            state, message = self.helper._kill(self.outdir, None)
        self.assertEqual(state, "unknown", message)
        self.assertIsNone(self.proc.poll(), "the process was signalled")

    def test_ownership_is_proven_again_before_sigkill(self) -> None:
        # Ours for the SIGTERM, someone else's by the SIGKILL (a reused pid).
        with patch.object(runner, "_command_line", side_effect=[self.ours, "/Applications/Roost.app/Contents/MacOS/Roost"]):
            state, message = self.helper._kill(self.outdir, None)
        self.assertEqual(state, "gone", message)
        time.sleep(0.2)
        self.assertIsNone(self.proc.poll(), "SIGKILL reached a process that was not proven ours")

    def test_a_proven_helper_is_killed(self) -> None:
        with patch.object(runner, "_command_line", return_value=self.ours):
            state, message = self.helper._kill(self.outdir, None)
        self.assertEqual(state, "stopped", message)
        self.assertIn("SIGKILL", message)

    def test_a_helper_dying_between_checks_is_not_an_error(self) -> None:
        with (
            patch.object(runner, "_command_line", return_value=self.ours),
            patch.object(runner.os, "getpgid", side_effect=ProcessLookupError(3, "gone")),
        ):
            try:
                state, message = self.helper._kill(self.outdir, None)
            except ProcessLookupError as error:
                self.fail(f"a helper that died between checks escaped as {error!r}")
        self.assertEqual(state, "gone", message)


READY = {
    "capabilities": {
        "accessibility": True,
        "post_event": True,
        "listen_event": True,
        "screen_capture": True,
    },
    "session": {"available": True, "locked": False, "on_console": True},
    "console": {"available": True, "locked": False, "on_console": True},
    "input_source": "com.apple.keylayout.US",
}


class ReadinessTests(unittest.TestCase):
    def test_a_ready_machine_has_no_reason(self) -> None:
        self.assertIsNone(runner.readiness(READY))

    def test_each_unready_state_names_its_reason(self) -> None:
        cases = (
            ({"capabilities": {**READY["capabilities"], "listen_event": False}}, "listen_event"),
            ({"session": {"available": False}}, "outside the GUI login session"),
            ({"session": {**READY["session"], "locked": True}}, "locked"),
            ({"console": {**READY["console"], "locked": True}}, "locked"),
            ({"session": {**READY["session"], "on_console": False}}, "not on the console"),
            ({"console": {**READY["console"], "on_console": False}}, "not on the console"),
            ({"secure_input_pid": 449}, "Secure Input is already on (ioreg names pid 449"),
            ({"input_source": "com.apple.keylayout.German"}, "German"),
        )
        for change, reason in cases:
            with self.subTest(change=change):
                self.assertIn(reason, runner.readiness({**READY, **change}) or "")


class CommandLineTests(FakeHelperCase):
    def test_secure_input_is_allowed_only_when_asked_for(self) -> None:
        helper = self.helper()
        helper.key(5, 0)
        helper.key(5, [0, 1], ("alt-left",), allow_secure_input=True)
        self.assertEqual(
            self.called(),
            ["key --pid 5 --code 0", "key --pid 5 --code 0,1 --flags alt-left --allow-secure-input"],
        )


class BackgroundTests(FakeHelperCase):
    def test_a_background_helper_is_live_at_ready_and_reports_on_stop(self) -> None:
        with self.helper().event_tap(2) as tap:
            self.assertTrue((tap.outdir / "ready").exists())
            self.assertIsNone(tap.result)
            result = tap.stop()
        self.assertEqual(result["key_downs"], [0])
        self.assertEqual(self.called(), ["event-tap --seconds 2.0"])


class RunnerModeTests(FakeHelperCase):
    def setUp(self) -> None:
        super().setUp()
        self.runner_app = self.root / "Roost Test Runner.app"
        (self.runner_app / "Contents").mkdir(parents=True)
        (self.runner_app / "Contents" / "Info.plist").write_text("<plist/>")
        self.opened: list[list[str]] = []

    def fake_open(self, argv, **kwargs):
        """Stand in for `open` + the runner: run the command with its output
        in <outdir>, then write <outdir>/status, as runner.c does."""
        if argv[0] != "open":
            return REAL_RUN(argv, **kwargs)
        self.opened.append(argv)
        args = argv[argv.index("--args") + 1:]
        outdir, command = Path(args[0]), args[1:]
        with open(outdir / "stdout", "wb") as out, open(outdir / "stderr", "wb") as err:
            status = REAL_RUN(command, stdout=out, stderr=err, check=False).returncode
        (outdir / "status").write_text(f"{status}\n")
        return subprocess.CompletedProcess(argv, 0, "", "")

    def test_the_runner_is_opened_in_the_background_with_absolute_staged_paths(self) -> None:
        helper = runner.Helper(
            self.artifacts, launch_mode="runner", binary=self.binary, runner_app=self.runner_app
        )
        self.addCleanup(helper.close)
        with patch.object(runner.subprocess, "run", side_effect=self.fake_open):
            self.assertEqual(helper.run("ok")["command"], "ok")
        (argv,) = self.opened
        self.assertEqual(argv[:4], ["open", "-g", "-n", str(self.runner_app)])
        outdir, binary = Path(argv[5]), Path(argv[6])
        self.assertEqual(argv[7:10], ["--outdir", str(outdir), "--deadline-ms"])
        for path in (outdir, binary):
            self.assertTrue(path.is_absolute(), path)
            self.assertTrue(str(path).startswith("/tmp/"), path)
        self.assertNotEqual(binary, self.binary, "the runner must run a staged copy")
        self.assertEqual(binary.read_bytes(), self.binary.read_bytes())

    def test_a_hung_open_stays_bounded_and_claims_no_cleanup(self) -> None:
        helper = runner.Helper(
            self.artifacts, launch_mode="runner", binary=self.binary, runner_app=self.runner_app
        )
        self.addCleanup(helper.close)
        timeouts = []

        def open_hangs_once(argv, **kwargs):
            if argv[0] == "open" and not timeouts:
                timeouts.append(kwargs.get("timeout"))
                raise subprocess.TimeoutExpired(argv, kwargs.get("timeout"))
            return self.fake_open(argv, **kwargs)

        with (
            patch.object(runner, "_LAUNCH_SLACK_S", 0.2),
            patch.object(runner, "_timeout_scale", lambda: 1.0),
            patch.object(runner.subprocess, "run", side_effect=open_hangs_once),
        ):
            with self.assertRaisesRegex(
                runner.RealInputError,
                "did not finish.*no pid file.*cleanup NOT confirmed.*tracked release: nothing held",
            ):
                helper.run("ok", deadline_ms=100)
        self.assertEqual(timeouts, [runner._OPEN_TIMEOUT_S])
        self.assertEqual(self.called(), [])

    def test_no_runner_or_no_binary_is_unavailable(self) -> None:
        with self.assertRaisesRegex(runner.RealInputUnavailable, "no TCC runner"):
            runner.Helper(
                self.artifacts, launch_mode="runner", binary=self.binary,
                runner_app=self.root / "missing.app",
            )
        with self.assertRaisesRegex(runner.RealInputUnavailable, "no helper binary"):
            runner.Helper(self.artifacts, launch_mode="direct", binary=self.root / "nope")


class ProcessQueryTests(unittest.TestCase):
    def test_a_hung_ps_reads_as_no_answer(self) -> None:
        def hangs(argv, **kwargs):
            raise subprocess.TimeoutExpired(argv, kwargs.get("timeout"))

        with patch.object(runner.subprocess, "run", side_effect=hangs) as run:
            self.assertIsNone(runner._command_line(1))
        self.assertEqual(run.call_args.kwargs.get("timeout"), runner._PS_TIMEOUT_S)


class EnvironmentTests(unittest.TestCase):
    def test_the_mode_defaults_to_the_runner_and_rejects_unknowns(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(runner.mode(), "runner")
            self.assertFalse(runner.required())
        with patch.dict(os.environ, {"ROOST_REAL_INPUT_MODE": "direct", "ROOST_REQUIRE_REAL_INPUT": "1"}):
            self.assertEqual(runner.mode(), "direct")
            self.assertTrue(runner.required())
        with patch.dict(os.environ, {"ROOST_REAL_INPUT_MODE": "ssh"}):
            with self.assertRaises(ValueError):
                runner.mode()

    def test_the_helper_binary_follows_cargo(self) -> None:
        repo = runner.HERE.parents[2]
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(
                runner.helper_binary(), runner.CRATE / "target" / "debug" / runner.BINARY_NAME
            )
        with patch.dict(os.environ, {"CARGO_TARGET_DIR": "/v/target-c10"}, clear=True):
            self.assertEqual(runner.helper_binary(), Path("/v/target-c10/debug/roost-input-mac"))
        with patch.dict(os.environ, {"CARGO_TARGET_DIR": "target/x"}, clear=True):
            self.assertEqual(runner.helper_binary(), repo / "target/x/debug/roost-input-mac")
        with patch.dict(os.environ, {"ROOST_INPUT_MAC_BIN": "/opt/h", "CARGO_TARGET_DIR": "/v"}):
            self.assertEqual(runner.helper_binary(), Path("/opt/h"))


if __name__ == "__main__":
    unittest.main()
