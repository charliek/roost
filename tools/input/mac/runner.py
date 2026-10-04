"""Run the Mac real-input helper (`roost-input-mac`) for the harness (plan 074 §D7).

Two launch modes behind one lifecycle:

* ``runner`` (default; the harness Mac): through the never-rebuilt TCC anchor,
  ``open -g -n "~/Applications/Roost Test Runner.app" --args <outdir> <helper>
  …``, so the helper runs with the runner's grants. ``-g`` keeps the runner from
  taking focus off the window under test.
* ``direct`` (``ROOST_REAL_INPUT_MODE=direct``; CI): the helper itself, whose
  grants come from the hosted runner's own responsible process.

Either way the helper writes ``<outdir>/pid`` first and has its own deadline,
``<outdir>/held.json`` journals what it holds, and ``<outdir>/status`` is its
exit code. A helper that outlives the wrapper's timeout is stopped (SIGTERM,
then SIGKILL, its identity proven before each), and after any unsuccessful
outcome a fresh ``release-held`` releases exactly what that journal still
lists — the helper's own presses, never anything else held down.

Results map onto three exceptions: :class:`RealInputUnavailable` (a missing
grant or runner, a locked console — a skip, or a failure under
``ROOST_REQUIRE_REAL_INPUT=1``), :class:`RealInputRefused` (the target was not
frontmost, or not topmost at the point — always a failure) and
:class:`RealInputError` (anything else). This module is importable without
pytest, so the lifecycle is unit-tested on any OS.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATE = HERE / "roost-input-mac"
BINARY_NAME = "roost-input-mac"
RUNNER_APP = Path.home() / "Applications" / "Roost Test Runner.app"
MODES = ("runner", "direct")

# How long past the helper's own deadline the wrapper waits before it kills:
# a LaunchServices launch of the runner is not instant on a loaded machine.
_LAUNCH_SLACK_S = 10.0
_POLL_S = 0.05
_RELEASE_DEADLINE_MS = 5000
_READY_S = 10.0
# Every subprocess on the recovery path is bounded: a hung LaunchServices
# call or `ps` must not hold up the release that follows it.
_OPEN_TIMEOUT_S = 15.0
_PS_TIMEOUT_S = 5.0
_TERM_GRACE_S = 2.0
_PS_ATTEMPTS = 3
US_INPUT_SOURCE = "com.apple.keylayout.US"


class RealInputUnavailable(Exception):
    """This machine cannot do real input now: a skip, or a failure when required."""


class RealInputRefused(AssertionError):
    """The helper would not post: the target was not frontmost or not topmost."""


class RealInputError(RuntimeError):
    """The helper failed, timed out, or did not say what happened."""


HELPER_ERRORS = (RealInputUnavailable, RealInputRefused, RealInputError)


def readiness(report: dict) -> str | None:
    """Why a `preflight` report says real input cannot run here now, or None:
    a grant missing (read-only checks; nothing here ever asks for one), the
    helper outside the GUI login session, a session not on the console, a
    locked console, Secure Input already on (someone else's: it blinds the
    event tap and Roost's own toggle can't be told apart from it), or an input
    source other than US, which the key scenarios' expected bytes assume."""
    missing = sorted(name for name, granted in report["capabilities"].items() if not granted)
    if missing:
        return f"not granted to the helper: {', '.join(missing)}"
    session, console = report["session"], report["console"]
    if not session["available"]:
        return "the helper is outside the GUI login session"
    if session.get("on_console") is False or console.get("on_console") is False:
        return "the login session is not on the console"
    if session["locked"] or console.get("locked"):
        return "the console is locked"
    if (holder := report.get("secure_input_pid")) is not None:
        return f"Secure Input is already on (ioreg names pid {holder}, the frontmost app)"
    if report["input_source"] != US_INPUT_SOURCE:
        return f"the input source is {report['input_source']!r}, not {US_INPUT_SOURCE}"
    return None


def required() -> bool:
    return os.environ.get("ROOST_REQUIRE_REAL_INPUT") == "1"


def mode() -> str:
    value = os.environ.get("ROOST_REAL_INPUT_MODE") or "runner"
    if value not in MODES:
        raise ValueError(f"ROOST_REAL_INPUT_MODE={value!r} (want {' or '.join(MODES)})")
    return value


def _timeout_scale() -> float:
    return float(os.environ.get("ROOST_TEST_TIMEOUT_SCALE", "1") or "1")


def helper_binary() -> Path:
    """`ROOST_INPUT_MAC_BIN`, else the debug build where cargo puts it for
    `--manifest-path tools/input/mac/roost-input-mac/Cargo.toml`."""
    if explicit := os.environ.get("ROOST_INPUT_MAC_BIN"):
        return Path(explicit).expanduser().resolve()
    if target := os.environ.get("CARGO_TARGET_DIR"):
        root = Path(target).expanduser()
        if not root.is_absolute():
            # cargo resolves a relative CARGO_TARGET_DIR from its own cwd: the
            # repo root, where the make target and CI run it.
            root = HERE.parents[2] / root
        return (root / "debug" / BINARY_NAME).resolve()
    return CRATE / "target" / "debug" / BINARY_NAME


def skip_or_fail(reason: str):
    """Real input is unavailable: skip the test, or fail it when required."""
    import pytest  # imported here so this module stays importable without pytest

    if required():
        pytest.fail(f"real input is required (ROOST_REQUIRE_REAL_INPUT=1): {reason}")
    pytest.skip(reason)


@contextmanager
def unavailable_skips():
    """Turn a :class:`RealInputUnavailable` raised inside into `skip_or_fail`."""
    try:
        yield
    except RealInputUnavailable as error:
        skip_or_fail(str(error))


def _classify(command: str, status: int | None, stdout: str, stderr: str) -> dict:
    """The helper's JSON result, or the exception its output means."""
    line = next((line for line in reversed(stdout.splitlines()) if line.strip()), "")
    try:
        result = json.loads(line) if line else None
    except json.JSONDecodeError:
        result = None
    if not isinstance(result, dict):
        raise RealInputError(
            f"helper `{command}` exited {status} without a JSON result; "
            f"stdout={stdout[-2000:]!r} stderr={stderr[-2000:]!r}"
        )
    if result.get("ok") is True and status == 0:
        return result
    kind = result.get("kind")
    message = f"helper `{command}`: {result.get('error') or result}"
    if kind == "unavailable":
        raise RealInputUnavailable(message)
    if kind == "refused":
        raise RealInputRefused(message)
    raise RealInputError(f"{message} (kind={kind}, status={status})")


def _text(path: Path) -> str:
    return path.read_text(errors="replace") if path.exists() else ""


def _command_line(pid: int) -> str | None:
    try:
        out = subprocess.run(
            ["ps", "-p", str(pid), "-o", "args="],
            capture_output=True,
            text=True,
            check=False,
            timeout=_PS_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return None
    return out.stdout.strip() or None if out.returncode == 0 else None


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


class Background:
    """A running helper (`event-tap`): live once it wrote `<outdir>/ready`."""

    def __init__(self, helper: "Helper", outdir: Path, command: str, proc, timeout: float):
        self._helper = helper
        self.outdir = outdir
        self._command = command
        self._proc = proc
        self._timeout = timeout
        self._started = time.monotonic()
        self.result: dict | None = None

    def events(self) -> list[dict]:
        """The events logged so far (`<outdir>/events.jsonl`)."""
        log = self.outdir / "events.jsonl"
        if not log.exists():
            return []
        return [json.loads(line) for line in log.read_text().splitlines() if line.strip()]

    def stop(self) -> dict:
        """Ask the helper to stop, and return its JSON result."""
        if self.result is None:
            (self.outdir / "stop").touch()
            remaining = self._timeout - (time.monotonic() - self._started)
            self.result = self._helper._finish(
                self.outdir, self._command, self._proc, max(remaining, _LAUNCH_SLACK_S)
            )
        return self.result

    def __enter__(self) -> "Background":
        return self

    def __exit__(self, exc_type, _exc, _tb) -> None:
        if self.result is None:
            try:
                self.stop()
            except HELPER_ERRORS:
                # Never mask the error that is already unwinding.
                if exc_type is None:
                    raise


class Helper:
    """One harness run's handle on the helper.

    Every invocation gets its own outdir in a private staging directory on the
    boot volume, copied into `artifacts/helper/` once it ends. In runner mode
    the helper binary is staged there too: the runner's grants do not cover
    files on another volume (an external disk), and a child of it that runs
    from one sends the runner a removable-volume permission prompt instead of
    running.
    """

    def __init__(
        self,
        artifacts: Path,
        *,
        launch_mode: str | None = None,
        binary: Path | None = None,
        runner_app: Path = RUNNER_APP,
    ):
        self.mode = launch_mode or mode()
        if self.mode not in MODES:
            raise ValueError(f"unknown real-input mode {self.mode!r}")
        self.artifacts = Path(artifacts).resolve()
        self.runner_app = runner_app
        self._seq = 0
        source = Path(binary) if binary is not None else helper_binary()
        if not source.is_file():
            raise RealInputUnavailable(
                f"no helper binary at {source}: build it with `cargo build --manifest-path "
                f"{CRATE.relative_to(HERE.parents[2])}/Cargo.toml --locked`"
            )
        if self.mode == "runner" and not (runner_app / "Contents" / "Info.plist").is_file():
            raise RealInputUnavailable(
                f"no TCC runner at {runner_app} (tools/input/mac/test-runner/README.md)"
            )
        self._stage = Path(tempfile.mkdtemp(prefix="roost-ri-", dir="/tmp"))
        if self.mode == "runner":
            self.binary = self._stage / BINARY_NAME
            shutil.copy2(source, self.binary)
        else:
            self.binary = source.resolve()

    def close(self) -> None:
        shutil.rmtree(self._stage, ignore_errors=True)

    def __enter__(self) -> "Helper":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    # -- lifecycle -------------------------------------------------------

    def _outdir(self, command: str) -> Path:
        self._seq += 1
        outdir = self._stage / f"{self._seq:03d}-{command}"
        outdir.mkdir()
        return outdir

    def _launch(self, outdir: Path, argv: list[str]):
        """Start the helper; the direct child's `Popen`, or None for runner mode."""
        full = [str(self.binary), "--outdir", str(outdir), *argv]
        if self.mode == "runner":
            try:
                launched = subprocess.run(
                    ["open", "-g", "-n", str(self.runner_app), "--args", str(outdir), *full],
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=_OPEN_TIMEOUT_S,
                )
            except subprocess.TimeoutExpired:
                # The helper may or may not have started: `_finish` finds out
                # through its pid file, within its own timeout.
                return None
            if launched.returncode != 0:
                raise RealInputUnavailable(
                    f"`open` could not launch {self.runner_app}: {launched.stderr.strip()}"
                )
            return None
        with open(outdir / "stdout", "wb") as out, open(outdir / "stderr", "wb") as err:
            return subprocess.Popen(
                full,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=err,
                start_new_session=True,
            )

    def _exited(self, outdir: Path, proc) -> int | None:
        """`<outdir>/status`, which a direct-mode exit writes here the way
        runner.c writes it (128 + signal for a killed helper)."""
        status_file = outdir / "status"
        if proc is not None and not status_file.exists():
            code = proc.poll()
            if code is None:
                return None
            status_file.write_text(f"{code if code >= 0 else 128 - code}\n")
        text = _text(status_file).strip()
        return int(text) if text else None

    def _identity(self, pid: int, outdir: Path) -> str:
        """Whether `pid` is still this invocation's helper: "ours", "gone",
        "other" (the number now belongs to another process) or "unknown"
        (`ps` could not say, even after retries). Its own outdir on its
        command line is what makes a process this invocation's helper; an
        answer that cannot be had is never taken as "gone"."""
        for _ in range(_PS_ATTEMPTS):
            if not _alive(pid):
                return "gone"
            command = _command_line(pid)
            if command is not None:
                return "ours" if f"--outdir {outdir}" in command else "other"
        return "gone" if not _alive(pid) else "unknown"

    def _kill(self, outdir: Path, proc) -> tuple[str, str]:
        """Stop a helper that outlived its timeout: SIGTERM first, which the
        helper answers by releasing what it holds, then SIGKILL. Returns a
        state ("stopped", "gone", "unknown", "survived") and a description.

        A direct-mode child is signalled through its own unreaped `Popen`
        (its pid cannot be reused until it is reaped). A runner-mode helper is
        not this process's child, so its identity is proven again immediately
        before each signal, and nothing is signalled on an identity that
        cannot be proven."""
        if proc is not None:
            return self._kill_child(proc)
        pid_file = outdir / "pid"
        if not pid_file.exists():
            return "unknown", "no pid file: the helper may not have started, or may be starting"
        pid = int(pid_file.read_text())
        for sig, grace in ((signal.SIGTERM, _TERM_GRACE_S), (signal.SIGKILL, 5.0)):
            identity = self._identity(pid, outdir)
            if identity in ("gone", "other"):
                return "gone", f"pid {pid} is gone"
            if identity == "unknown":
                return "unknown", f"cannot tell whether pid {pid} is still the helper; not signalled"
            try:
                if os.getpgid(pid) == pid:
                    os.killpg(pid, sig)
                else:
                    os.kill(pid, sig)
            except ProcessLookupError:
                return "gone", f"pid {pid} exited before {sig.name}"
            deadline = time.monotonic() + grace
            while _alive(pid) and time.monotonic() < deadline:
                time.sleep(_POLL_S)
            if not _alive(pid):
                return "stopped", f"stopped pid {pid} with {sig.name}"
        return "survived", f"pid {pid} survived SIGKILL"

    def _kill_child(self, proc) -> tuple[str, str]:
        for sig, grace in ((signal.SIGTERM, _TERM_GRACE_S), (signal.SIGKILL, 5.0)):
            if proc.poll() is not None:
                return "gone", f"pid {proc.pid} had exited"
            try:
                os.killpg(proc.pid, sig)
            except ProcessLookupError:
                return "gone", f"pid {proc.pid} exited before {sig.name}"
            try:
                proc.wait(timeout=grace)
                return "stopped", f"stopped pid {proc.pid} with {sig.name}"
            except subprocess.TimeoutExpired:
                continue
        return "survived", f"pid {proc.pid} survived SIGKILL"

    def _collect(self, outdir: Path) -> None:
        destination = self.artifacts / "helper" / outdir.name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(outdir, destination, dirs_exist_ok=True)

    def _finish(self, outdir: Path, command: str, proc, timeout: float, release=True) -> dict:
        try:
            return self._outcome(outdir, command, proc, timeout)
        except BaseException as error:
            # Every outcome but success — a refusal, a signal, a panic, the
            # deadline, no status, a status or output file that cannot be
            # read — releases what this helper itself still holds, as its
            # journal records it, and nothing else.
            if not release:
                raise
            if isinstance(error, HELPER_ERRORS):
                raise type(error)(f"{error}; tracked release: {self._recover(outdir)}") from error
            # The outcome could not be read, so the helper may still be
            # running: stop it, as a timeout does, before releasing.
            try:
                stopped = self._kill(outdir, proc)[1]
            except Exception as kill_error:
                stopped = f"stopping it failed: {kill_error!r}"
            report = self._recover(outdir)
            if isinstance(error, Exception):
                raise RealInputError(
                    f"helper `{command}`: its outcome could not be read ({error!r}); "
                    f"{stopped}; tracked release: {report}"
                ) from error
            raise
        finally:
            self._collect(outdir)

    def _outcome(self, outdir: Path, command: str, proc, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        if proc is not None:
            try:
                proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                pass
        status = self._exited(outdir, proc)
        while status is None and time.monotonic() < deadline:
            time.sleep(_POLL_S)
            status = self._exited(outdir, proc)
        if status is None:
            try:
                state, killed = self._kill(outdir, proc)
            except Exception as error:  # a race must not skip the release
                state, killed = "unknown", f"stopping it failed: {error!r}"
            if state in ("unknown", "survived"):
                killed += "; cleanup NOT confirmed: the helper may still be running"
            raise RealInputError(f"helper `{command}` did not finish within {timeout:.1f}s: {killed}")
        return _classify(command, status, _text(outdir / "stdout"), _text(outdir / "stderr"))

    def _recover(self, outdir: Path) -> str:
        """Release exactly what `<outdir>/held.json` says the helper still
        holds, from a fresh, bounded `release-held`. Nothing held (a refusal
        before any press, or a helper that released on its own way out) means
        nothing is posted. Never `key --release-all`, which would also release
        whatever the person at the desk is holding."""
        journal = outdir / "held.json"
        try:
            held = json.loads(journal.read_text()) if journal.exists() else {}
        except (OSError, ValueError) as error:
            return f"FAILED to read {journal}: {error}"
        if not any(held.get(name) for name in ("modifiers", "keys", "buttons")):
            return "nothing held"
        try:
            result = self.run(
                "release-held", "--file", str(journal),
                deadline_ms=_RELEASE_DEADLINE_MS, _release=False,
            )
        except Exception as error:
            return f"FAILED: {error}"
        return json.dumps(result.get("released_from_journal"))

    def _begin(self, argv: tuple[str, ...], deadline_ms: int):
        """Launch `argv`; its command name, outdir, direct-mode `Popen` and
        how long the wrapper gives it before killing it."""
        command = argv[0]
        outdir = self._outdir(command)
        proc = self._launch(outdir, ["--deadline-ms", str(deadline_ms), *argv])
        timeout = deadline_ms / 1000 + _LAUNCH_SLACK_S * _timeout_scale()
        return command, outdir, proc, timeout

    def run(self, *argv: str, deadline_ms: int = 10_000, _release: bool = True) -> dict:
        """Run one helper command to completion and return its JSON result."""
        command, outdir, proc, timeout = self._begin(argv, deadline_ms)
        return self._finish(outdir, command, proc, timeout, release=_release)

    def start(self, *argv: str, deadline_ms: int) -> Background:
        """Start a helper command that runs on (`event-tap`); return once it is live."""
        command, outdir, proc, timeout = self._begin(argv, deadline_ms)
        background = Background(self, outdir, command, proc, timeout)
        live_by = time.monotonic() + _READY_S * _timeout_scale()
        while not (outdir / "ready").exists():
            if self._exited(outdir, proc) is not None:
                background.result = self._finish(outdir, command, proc, _LAUNCH_SLACK_S)
                raise RealInputError(f"helper `{command}` exited before it was live: {background.result}")
            if time.monotonic() >= live_by:
                background.stop()
                raise RealInputError(f"helper `{command}` was not live within {_READY_S}s")
            time.sleep(_POLL_S)
        return background

    # -- commands --------------------------------------------------------

    def preflight(self, pid: int | None = None) -> dict:
        return self.run("preflight", *(["--pid", str(pid)] if pid is not None else []))

    def require_ready(self) -> dict:
        """A preflight that raises :class:`RealInputUnavailable` with the
        reason when this machine cannot do real input now."""
        report = self.preflight()
        if (reason := readiness(report)) is not None:
            raise RealInputUnavailable(reason)
        return report

    def window(self, pid: int) -> dict:
        return self.run("window", "--pid", str(pid))

    def window_set(self, pid: int, frame: tuple[float, float, float, float]) -> dict:
        return self.run("window-set", "--pid", str(pid), "--frame", _numbers(frame))

    def key(self, pid: int, codes, flags=(), *, allow_secure_input: bool = False) -> dict:
        """`allow_secure_input` only after Roost itself (`app.secure_input`)
        confirmed it holds Secure Input: the helper refuses otherwise, and
        cannot tell a foreign holder apart (test-runner/README.md, "Known
        limits")."""
        codes = [codes] if isinstance(codes, int) else list(codes)
        argv = ["key", "--pid", str(pid), "--code", ",".join(str(code) for code in codes)]
        if flags:
            argv += ["--flags", ",".join(flags)]
        if allow_secure_input:
            argv.append("--allow-secure-input")
        return self.run(*argv)

    def release_all(self, _release: bool = True) -> dict:
        """`key --release-all`: every modifier, and any key or button the
        system reports down, whoever pressed it. For a person at the desk to
        run by hand; no automated path calls it (recovery uses `held.json`)."""
        return self.run("key", "--release-all", deadline_ms=_RELEASE_DEADLINE_MS, _release=_release)

    def mouse(
        self,
        action: str,
        pid: int,
        at: tuple[float, float] | None = None,
        *,
        button: str | None = None,
        path=None,
        hold_ms: int | None = None,
        deadline_ms: int = 10_000,
        allow_secure_input: bool = False,
    ) -> dict:
        argv = ["mouse", action, "--pid", str(pid)]
        if allow_secure_input:
            argv.append("--allow-secure-input")
        if at is not None:
            argv += ["--at", _numbers(at)]
        if button is not None:
            argv += ["--button", button]
        if path is not None:
            argv += ["--path", ";".join(_numbers(point) for point in path)]
        if hold_ms is not None:
            argv += ["--hold-ms", str(hold_ms)]
        return self.run(*argv, deadline_ms=deadline_ms)

    def menu_bar(self, pid: int) -> dict:
        return self.run("menu-bar", "--pid", str(pid))

    def popup(self, pid: int, at: tuple[float, float], wait_ms: int | None = None) -> dict:
        argv = ["popup", "--pid", str(pid), "--at", _numbers(at)]
        if wait_ms is not None:
            argv += ["--wait-ms", str(wait_ms)]
        return self.run(*argv)

    def press(self, pid: int, path: list[str], at: tuple[float, float] | None = None) -> dict:
        argv = ["press", "--pid", str(pid), "--path", json.dumps(path)]
        if at is not None:
            argv += ["--at", _numbers(at)]
        return self.run(*argv)

    def event_tap(self, seconds: float) -> Background:
        return self.start(
            "event-tap", "--seconds", repr(float(seconds)),
            deadline_ms=int(seconds * 1000) + 5000,
        )

    def capture(self, rect: tuple[float, float, float, float]) -> dict:
        """A true screen capture of `rect`, kept under `artifacts/helper/` as
        `capture.png`; the result's `artifact` is that copy."""
        result = self.run("capture", "--rect", _numbers(rect))
        result["artifact"] = str(self.artifacts / "helper" / Path(result["path"]).parent.name / "capture.png")
        return result


def _numbers(values) -> str:
    return ",".join(repr(float(value)) for value in values)
