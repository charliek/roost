"""The Mac real-input suite's own Roost-Iced (plan 074 §D7), in the fixed
`Roost-linux` namespace.

The branch build shares the installed app's bundle id, and anyone can launch
it, so nothing here acts on a process it has not proven is this run's: a
process is ours only when its executable is inside the launched bundle **and**
its environment carries this launch's unique `ROOST_STATE_DIR` (read with
`ps -E`). Every signal goes through `ui._terminate_owned_pid`, which re-checks
that proof before each one; a process that cannot be proven either way is
neither signalled nor taken for dead, and its state dir is kept. Only what
this run created is cleaned up — each directory holds this run's marker — and
only when it owned the namespace throughout and holds the namespace's socket
lock while it cleans: the logs directory is removed, and the caches directory
keeps the app's `roost.lock`, which is never deleted.

Importable without pytest, so these rules are unit-tested on any OS.
"""

from __future__ import annotations

import fcntl
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

import session
import ui
import util
from client import Roost, RoostError, scaled_timeout

PROFILE = "linux"
NAMESPACE = "Roost-linux"
CACHES = Path.home() / "Library" / "Caches" / NAMESPACE
LOGS = Path.home() / "Library" / "Logs" / NAMESPACE
SOCKET = CACHES / "roost.sock"
SOCKET_LOCK = CACHES / "roost.lock"
LOCK = Path("/tmp/roost-real-input.lock")
MARKER_PREFIX = ".roost-real-input-run-"
SAFETY_KEYS = ("agent-hooks", "local-backend")
_PS_TIMEOUT_S = 5.0
# A socket that accepts and never answers must not stall a poll.
_SOCKET_TIMEOUT_S = 2.0


def platform_mismatch(system: str, target: str) -> str | None:
    """Why this run cannot drive real input at all, or None."""
    if system != "Darwin":
        return "real CGEvent input is macOS-only"
    if target != "iced":
        return "drives the iced bundle; the Swift lane collects the whole directory"
    return None


def answering_pid() -> int | None:
    return ui._answering_pid_at(SOCKET, timeout=_SOCKET_TIMEOUT_S)


def _ps(*args: str) -> str | None:
    try:
        out = subprocess.run(
            ["ps", *args], capture_output=True, text=True, check=False, timeout=_PS_TIMEOUT_S
        )
    except subprocess.TimeoutExpired:
        return None
    return out.stdout if out.returncode == 0 else None


def bundle_pids(app: Path) -> set[int]:
    """Running processes whose executable is inside `app`."""
    prefix = f"{app}/Contents/MacOS/"
    pids = set()
    for line in (_ps("-axo", "pid=,comm=") or "").splitlines():
        pid, _, command = line.strip().partition(" ")
        if command.strip().startswith(prefix):
            pids.add(int(pid))
    return pids


def seed_config(keys: dict[str, str]) -> str:
    """launcher.conf's safety lines, then this launch's own keys."""
    safety = {key: util.config_value(ui.SEED_CONFIG, key) for key in SAFETY_KEYS}
    if None in safety.values():
        raise AssertionError(f"{ui.SEED_CONFIG} no longer sets each of {SAFETY_KEYS}: {safety}")
    if overridden := set(SAFETY_KEYS) & keys.keys():
        raise ValueError(f"a real-input launch may not override {sorted(overridden)}")
    return "".join(f"{key} = {value}\n" for key, value in {**safety, **keys}.items())


def removable(
    created: list[Path],
    *,
    verified_owner: bool,
    foreign_owner: bool,
    answers_now: bool,
    lock_held: bool,
    marked: bool = False,
) -> tuple[list[Path], str | None]:
    """The namespace directories this run may clean (`Namespace.close`), and
    why it keeps them when it keeps any: only if this run created them, each still holds
    this run's marker (`marked`), a launch of this run proved it owned the
    socket, nobody else answered the socket during the run, nothing answers it
    now, and this run holds its lock (`lock_held` means someone else holds
    it, or this run could not take it)."""
    if not created:
        return [], None
    if not verified_owner:
        return [], "no launch of this run proved it owned the namespace"
    if foreign_owner:
        return [], "another process answered the namespace's socket during the run"
    if lock_held:
        return [], "the namespace's socket lock is held"
    if answers_now:
        return [], "the namespace's socket still answers"
    if not marked:
        return [], "a directory this run created no longer holds its marker"
    return created, None


class Namespace:
    """This run's hold on `Roost-linux`: the cross-run lock, what it found,
    and who answered the socket while it held it."""

    def __init__(self, artifacts: Path):
        self.artifacts = artifacts
        self.created: list[Path] = []
        self.marker = f"{MARKER_PREFIX}{uuid.uuid4().hex}"
        self.verified_owner = False
        self.foreign_owner = False
        self._fd: int | None = None

    def acquire(self) -> str | None:
        """Take the namespace; the reason it is not available, or None."""
        fd = os.open(LOCK, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            return f"{LOCK} is held: another real-input run owns {NAMESPACE}"
        self._fd = fd
        if answering_pid() is not None:
            self.release_lock()
            return f"{SOCKET} answers: a {NAMESPACE} UI is already running; refusing to share it"
        self.created = []
        for directory in (CACHES, LOGS):
            try:
                directory.mkdir(mode=0o700)
            except FileExistsError:
                continue
            self.created.append(directory)
            (directory / self.marker).touch()
        return None

    def marked(self) -> bool:
        """Every directory this run created still holds this run's marker."""
        return all((directory / self.marker).is_file() for directory in self.created)

    def take_socket_lock(self) -> int | None:
        """The namespace's socket lock (`roost.lock`, the UI's single-instance
        lock), taken by this run, or None when someone else holds it or it
        cannot be opened. While this run holds it no UI can start in the
        namespace."""
        try:
            fd = os.open(SOCKET_LOCK, os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
        except OSError:
            return None
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(fd)
            return None
        return fd

    def socket_lock_held(self) -> bool:
        """Whether a UI holds the namespace's socket lock now (a probe; it
        creates nothing and holds nothing)."""
        try:
            fd = os.open(SOCKET_LOCK, os.O_RDWR | getattr(os, "O_NOFOLLOW", 0))
        except FileNotFoundError:
            return False
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        finally:
            os.close(fd)
        return False

    def close(self) -> str | None:
        """Copy the logs into the artifacts, clean what this run may clean —
        the logs directory it created, and its marker in the caches directory,
        which stays with the lock — and let go of the namespace; the reason
        anything was kept."""
        try:
            if LOGS.is_dir():
                shutil.copytree(LOGS, self.artifacts / f"{NAMESPACE}-logs", dirs_exist_ok=True)
            # Held from before the checks until after the cleanup, so no UI
            # can take the namespace between "nobody is using it" and rmtree.
            socket_lock = self.take_socket_lock() if self.created else None
            try:
                dirs, kept = removable(
                    self.created,
                    verified_owner=self.verified_owner,
                    foreign_owner=self.foreign_owner,
                    answers_now=answering_pid() is not None,
                    lock_held=socket_lock is None,
                    marked=self.marked(),
                )
                for directory in dirs:
                    if directory == CACHES:
                        # Never `roost.lock` or the directory holding it: the
                        # app opens the lock and then locks it, two steps, so
                        # no removal of that pathname can be made safe.
                        (directory / self.marker).unlink(missing_ok=True)
                    else:
                        shutil.rmtree(directory, ignore_errors=True)
            finally:
                if socket_lock is not None:
                    os.close(socket_lock)
            if kept is not None:
                note = f"kept {', '.join(map(str, self.created))}: {kept}"
                (self.artifacts / "namespace-cleanup.txt").write_text(note + "\n")
                print(f"real-input namespace: {note}", file=sys.stderr)
            return kept
        finally:
            self.release_lock()

    def release_lock(self) -> None:
        if self._fd is not None:
            os.close(self._fd)
            self._fd = None


class RealInputUI:
    """One harness-launched Roost-Iced, handled by pid."""

    def __init__(self, app: Path, config: dict[str, str], namespace: Namespace):
        self.app = app
        self.config = dict(config)
        self.namespace = namespace
        self.pid: int | None = None
        self.state_dir: Path | None = None
        self._client: Roost | None = None
        self._strays: set[int] = set()

    @property
    def client(self) -> Roost:
        if self._client is None:
            self._client = Roost(
                str(SOCKET), timeout=scaled_timeout(30), deadline=scaled_timeout(30)
            )
        return self._client

    def owns(self, pid: int) -> bool | None:
        """Whether `pid` is this launch's app: its executable is inside the
        bundle and its environment carries this launch's state dir. False only
        when that is provably not so — the pid is gone, its executable is
        elsewhere, or its environment names another state dir — and None when
        it cannot be told (`ps` failed or timed out, or showed no
        `ROOST_STATE_DIR` at all, as for an environment it cannot read)."""
        if self.state_dir is None or not ui._pid_alive(pid):
            return False
        executable = _ps("-p", str(pid), "-o", "comm=")
        if executable is None:
            return None if ui._pid_alive(pid) else False
        if not executable.strip().startswith(f"{self.app}/Contents/MacOS/"):
            return False
        command = _ps("-wwE", "-p", str(pid), "-o", "command=")
        if command is None:
            return None if ui._pid_alive(pid) else False
        tokens = command.split()
        if f"ROOST_STATE_DIR={self.state_dir}" in tokens:
            return True
        if any(token.startswith("ROOST_STATE_DIR=") for token in tokens):
            return False
        return None if ui._pid_alive(pid) else False

    def launched_pids(self) -> set[int]:
        """Every running process this launch may have started: those proven
        its own, and those that cannot be told apart from it (`owns` None),
        which `stop` then keeps rather than forgets."""
        return {pid for pid in bundle_pids(self.app) if self.owns(pid) is not False}

    def launch(self) -> "RealInputUI":
        """Start the app: on a new state dir the first time, on the same one
        after `stop` (so `relaunch` sees what the last run persisted)."""
        if answering_pid() is not None:
            self.namespace.foreign_owner = True
            raise AssertionError(f"{SOCKET} answers before this launch: the namespace is occupied")
        if self.state_dir is None:
            config_text = seed_config(self.config)
            self.state_dir = Path(tempfile.mkdtemp(prefix="roost-ri-state-", dir="/tmp"))
            (self.state_dir / "config.conf").write_text(config_text, encoding="utf-8")
        argv = [
            "open", "-n",
            "--env", f"ROOST_BUNDLE_PROFILE={PROFILE}",
            "--env", f"ROOST_STATE_DIR={self.state_dir}",
            "--env", f"ROOST_CONFIG={self.state_dir / 'config.conf'}",
            "--env", "ROOST_TEST_MODE=1",
        ]
        for name in ("ICED_BACKEND", "ROOST_TEST_TIMEOUT_SCALE"):
            ui._forward_env(argv, name)
        if (rust_log := os.environ.get("RUST_LOG")) is not None:
            argv += ["--env", f"RUST_LOG={ui._floor_roost_iced_info(rust_log)}"]
        try:
            subprocess.run([*argv, str(self.app)], check=True, timeout=scaled_timeout(30))
            self.pid = self._adopt()
            self._wait_booted()
        except BaseException:
            self._strays = self.launched_pids()
            self.quit()
            raise
        return self

    def _adopt(self) -> int:
        """The pid answering the namespace's socket, once it is proven to be
        the process this launch started."""
        pid = session.wait_until(answering_pid, 30, f"{self.app} to answer {SOCKET}", 0.25)
        if self.owns(pid) is not True:
            self.namespace.foreign_owner = True
            raise AssertionError(
                f"{SOCKET} is answered by pid {pid}, which is not the app this launch started"
            )
        self.namespace.verified_owner = True
        return pid

    def _wait_booted(self) -> None:
        identify = self.client.identify()
        assert identify.get("local_backend", "in-process") == "in-process", identify
        tabs = session.wait_until(self.client.tabs, 30, "the launched UI's first tab", 0.1)
        try:
            self.client.tab_feed_pty_bytes(int(tabs[0]["id"]), b"")
        except RoostError as error:
            if error.code == "not-enabled":
                raise AssertionError("ROOST_TEST_MODE=1 did not reach the launched app") from error
            raise

    def open_tab(self, argv: list[str]) -> int:
        """A new tab running `argv`, selected in the window, with the
        keyboard; `/bin/cat` keeps a shell's own terminal queries out of the
        captured input."""
        project = int(self.client.list()[0]["id"])
        tab = self.client.open_tab(project, cwd="/tmp", argv=argv, activate=True)
        Roost._wait(lambda: self.client.app_selected_tab_id() == tab, 10, "the new tab to be selected")
        return tab

    def bring_to_front(self, helper) -> None:
        """Activate the app and wait until the helper sees its pid frontmost;
        each poll's report is kept under `artifacts/helper/`."""
        self.client.call("app.activate")
        session.wait_until(
            lambda: helper.preflight(pid=self.pid)["target"]["frontmost"],
            10,
            f"pid {self.pid} to become frontmost",
            0.2,
        )
        Roost._wait(self.client.app_active_terminal_focused, 5, "the terminal to take the keyboard")

    def relaunch(self) -> "RealInputUI":
        """Quit and start again on the same state dir and config."""
        self.stop()
        return self.launch()

    def quit(self) -> None:
        """Stop the app, then remove its state dir — never while `stop`
        cannot confirm the app is gone."""
        self.stop()
        if self.state_dir is not None:
            shutil.rmtree(self.state_dir, ignore_errors=True)
            self.state_dir = None

    def stop(self) -> None:
        """Stop every process this launch started, by pid, and return only
        once each is gone: the state dir it leaves holds the state lock. A
        process whose end cannot be confirmed (its ownership unknown, or it
        survived SIGKILL) stays tracked and this raises, so nothing removes
        the state dir from under it."""
        if self._client is not None:
            self._client.close()
            self._client = None
        unconfirmed: dict[int, str] = {}
        for pid in sorted({self.pid, *self._strays} - {None}):
            try:
                ui._terminate_owned_pid(pid, owned=self.owns, graceful=scaled_timeout(10))
            except RuntimeError as error:
                unconfirmed[pid] = str(error)
        if self.pid not in unconfirmed:
            self.pid = None
        self._strays = set(unconfirmed) - {self.pid}
        if unconfirmed:
            raise RuntimeError(
                f"cannot confirm this launch's app stopped ({unconfirmed}); "
                f"keeping its state dir {self.state_dir}"
            )
