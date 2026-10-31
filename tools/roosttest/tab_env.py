"""Read a tab's environment from its process, never through a shell
(plan 085 D9).

A shell adds `PWD`, `SHLVL` and `_`, and may rewrite what it was handed;
the block a process was exec'd with is exactly what Roost gave it. Linux
reads `/proc/<pid>/environ`. macOS reads `sysctl KERN_PROCARGS2` — what
`ps eww` prints from, but `ps` joins the strings with spaces, and
`SSH_CONNECTION` always contains some. macOS withholds a platform
binary's environment (`/bin/sleep`, `/bin/sh`, `/bin/zsh`) from every
other process, its parent included, so a process whose environment a test
reads there must run something else: `sleeper` and `sleep_argv`.

Only processes a test provably started are read, and that is settled
from metadata — executable, parent pid, start time — before any
environment is. A process descends from this one, or runs a private copy
of a binary that only this test has (`private_copy`); running the same
binary as this tree's build proves nothing, since a developer may run it
too. On Linux the metadata and the environment are read through one held
`/proc/<pid>` directory, which stops answering once that process is gone,
so a pid reused in between can never be read. macOS has no such handle:
its metadata is checked before and after a read by pid, which leaves a
reuse inside one second of `ps`'s start time (with the same parent and
executable) undetected — a known limit.
"""
from __future__ import annotations

import contextlib
import ctypes
import ctypes.util
import os
import shutil
import struct
import subprocess
import sys
import time
from collections.abc import Callable, Iterable, Iterator
from pathlib import Path

from client import scaled_timeout
from ui import OPENER_SET

#: MIRRORS `roost_ipc::opener::LOCAL_SET`: what a local opener brings.
LOCAL_SET = OPENER_SET[:4]

_CTL_KERN = 1
_KERN_ARGMAX = 8
_KERN_PROCARGS2 = 49
_PROC_PIDPATHINFO_MAXSIZE = 4096


def exe_of(pid: int) -> Path | None:
    """The executable `pid` runs, or None when it is gone or not ours to
    see."""
    if sys.platform == "darwin":
        libproc = ctypes.CDLL(ctypes.util.find_library("proc") or "/usr/lib/libproc.dylib")
        buf = ctypes.create_string_buffer(_PROC_PIDPATHINFO_MAXSIZE)
        if libproc.proc_pidpath(ctypes.c_int(pid), buf, ctypes.c_uint32(len(buf))) <= 0:
            return None
        return Path(os.fsdecode(buf.value))
    try:
        target = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        return None
    return Path(target.removesuffix(" (deleted)"))


def private_copy(binary: Path | str, dest: Path) -> Path:
    """A copy of `binary` at `dest`, in a directory only this test has, so
    a process running it is the test's by its executable alone."""
    shutil.copy2(os.path.realpath(binary), dest)
    return dest


_SLEEP_CODE = "import sys, time; time.sleep(float(sys.argv[1]))"


def sleeper() -> Path:
    """A binary that `sleep_argv` runs as a sleep, whose processes'
    environments this module can read: `sleep` on Linux; on macOS, which
    withholds `/bin/sleep`'s, the image this Python runs — not a launcher
    stub such as Homebrew's `python3`, which re-execs with variables of its
    own."""
    if sys.platform == "darwin":
        own = exe_of(os.getpid())
        assert own is not None, "this process's own executable is unreadable"
        return own
    return Path(shutil.which("sleep") or "/bin/sleep")


def sleep_argv(seconds: float, binary: Path | str | None = None) -> list[str]:
    """An argv that sleeps `seconds` running `binary` (default `sleeper()`),
    which is `sleeper()` or a `private_copy` of it."""
    binary = str(binary or sleeper())
    if sys.platform == "darwin":
        return [binary, "-c", _SLEEP_CODE, str(seconds)]
    return [binary, str(seconds)]


def readable_shell(directory: Path) -> Path:
    """A `SHELL` for a UI or daemon to give its restored and new tabs whose
    environment this module can read on macOS: a script `sleeper()` runs,
    so the kernel passes the block Roost gave it on unchanged. Written
    into `directory`, which the caller owns."""
    shell = directory / "readable-shell"
    shell.write_text(f"#!{sleeper()}\nimport time\ntime.sleep(600)\n")
    shell.chmod(0o755)
    return shell


#: A process as metadata tells it apart: its parent and its start time.
Identity = tuple[int, str]


def _parse_stat(stat: str) -> Identity:
    # `pid (comm) state ppid …`, the start time 19 fields past the state;
    # comm may hold spaces and parens.
    fields = stat[stat.rindex(")") + 2 :].split()
    return int(fields[1]), fields[19]


def _identity(pid: int) -> Identity | None:
    """`pid`'s parent pid and start time, or None once it is gone. The
    start time is what tells it from a later process that reuses the pid."""
    if sys.platform == "darwin":
        out = subprocess.run(
            ["ps", "-o", "ppid=,lstart=", "-p", str(pid)], capture_output=True, text=True
        ).stdout.split(None, 1)
        return (int(out[0]), out[1].strip()) if len(out) == 2 else None
    try:
        return _parse_stat(Path(f"/proc/{pid}/stat").read_text())
    except OSError:
        return None


def _processes() -> dict[int, Identity]:
    """Every live process's identity, from metadata alone."""
    if sys.platform == "darwin":
        listing = subprocess.run(
            ["ps", "-A", "-o", "pid=,ppid=,lstart="], capture_output=True, text=True, check=True
        ).stdout
        rows = (line.split(None, 2) for line in listing.splitlines())
        return {int(row[0]): (int(row[1]), row[2].strip()) for row in rows if len(row) == 3}
    table = {}
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit() and (found := _identity(int(entry.name))) is not None:
            table[int(entry.name)] = found
    return table


def _descendants(root: int, table: dict[int, Identity]) -> set[int]:
    """`root`'s descendants in `table`, by parent pid."""
    children: dict[int, list[int]] = {}
    for pid, (ppid, _) in table.items():
        children.setdefault(ppid, []).append(pid)
    found: set[int] = set()
    pending = [root]
    while pending:
        for child in children.get(pending.pop(), ()):
            if child not in found:
                found.add(child)
                pending.append(child)
    return found


@contextlib.contextmanager
def _pinned(pid: int) -> Iterator[int]:
    """`/proc/<pid>` held open (Linux): every read through it is of the
    process it named when opened, and fails once that process is gone,
    whoever has its pid by then."""
    fd = os.open(f"/proc/{pid}", os.O_RDONLY | os.O_DIRECTORY)
    try:
        yield fd
    finally:
        os.close(fd)


def _read_at(fd: int, name: str) -> bytes:
    with os.fdopen(os.open(name, os.O_RDONLY, dir_fd=fd), "rb") as handle:
        return handle.read()


def _facts_at(fd: int) -> tuple[Identity, Path]:
    """The identity and executable of the process `fd` pins."""
    exe = os.readlink("exe", dir_fd=fd).removesuffix(" (deleted)")
    return _parse_stat(_read_at(fd, "stat").decode()), Path(exe)


def _environ_at(pid: int, fd: int | None) -> dict[str, str]:
    """The environment of the process `fd` pins, or on macOS, which pins
    nothing, of `pid`."""
    if fd is None:
        return _environ(pid)
    return _pairs(_read_at(fd, "environ").split(b"\0"))


def _owned_environ(pid: int, owns: Callable[[Identity, Path], bool]) -> dict[str, str] | None:
    """`pid`'s environment when `owns` accepts its identity and executable,
    which are read first; None, with nothing read, when it does not or the
    process is gone."""
    if sys.platform == "darwin":
        identity, exe = _identity(pid), exe_of(pid)
        if identity is None or exe is None or not owns(identity, exe):
            return None
        try:
            env = _environ_at(pid, None)
        except OSError:
            return None
        return env if _identity(pid) == identity and exe_of(pid) == exe else None
    try:
        with _pinned(pid) as fd:
            identity, exe = _facts_at(fd)
            return _environ_at(pid, fd) if owns(identity, exe) else None
    except OSError:
        return None


def owned_processes(
    *, private: Iterable[Path | str] = (), shared: Iterable[Path | str] = ()
) -> list[tuple[int, dict[str, str]]]:
    """Every live process running one of `private` or `shared` that this
    test provably started, with the environment it was exec'd with (a
    zombie's reads empty).

    A `private` binary is a `private_copy`: any process running it is this
    test's, wherever it was reparented. A `shared` one counts only in a
    process that descended from this one, and still has the start time it
    had then."""
    private_set = {os.path.realpath(binary) for binary in private}
    shared_set = {os.path.realpath(binary) for binary in shared}
    table = _processes()
    mine = {pid: table[pid][1] for pid in _descendants(os.getpid(), table)}
    found = []
    for pid in table:

        def owns(identity: Identity, exe: Path, pid: int = pid) -> bool:
            return str(exe) in private_set or (str(exe) in shared_set and mine.get(pid) == identity[1])

        env = _owned_environ(pid, owns)
        if env is not None:
            found.append((pid, env))
    return found


def _environ(pid: int) -> dict[str, str]:
    """The environment `pid` was exec'd with, read by pid with no check of
    whose it is. Raises `OSError` when it is gone or not readable."""
    if sys.platform == "darwin":
        return parse_procargs2(_procargs2(pid))
    return _pairs(Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"))


class HiddenEnvironment(RuntimeError):
    """The kernel gave a process's arguments but withheld its environment.
    Not an `OSError`: a reader that treats those as "gone" must not take
    an unreadable environment for an absent process."""


#: The names of the strings the kernel puts after a process's environment
#: (`exec_add_apple_strings`). The NUL run that usually ends the
#: environment is word-alignment padding, which can be no bytes at all,
#: plus the first of these once the process's own start-up has zeroed
#: them — so a read right after `execve` can find them unseparated.
_APPLE_KEYS = frozenset(
    {
        b"pfz",
        b"stack_guard",
        b"malloc_entropy",
        b"ptr_munge",
        b"main_stack",
        b"executable_path",
        b"executable_file",
        b"dyld_file",
        b"executable_cdhash",
        b"executable_boothash",
        b"arm64e_abi",
        b"th_port",
        b"security_config",
    }
)


def parse_procargs2(buf: bytes) -> dict[str, str]:
    """A `KERN_PROCARGS2` buffer's environment: after `argc` (a native
    int), the exec path and its NUL padding, and `argc` argument strings
    come the environment strings, up to an empty string or the first of
    Apple's (`_APPLE_KEYS`), neither of which is read. A buffer that ends
    with the arguments is a withheld environment, which raises
    `HiddenEnvironment` rather than reading as an empty one."""
    (argc,) = struct.unpack_from("=i", buf)
    pos = buf.find(b"\0", 4)
    while 0 <= pos < len(buf) and buf[pos] == 0:
        pos += 1
    # The last piece is what follows the final NUL: never a whole string.
    strings = buf[pos:].split(b"\0")[:-1][argc:]
    if not strings:
        raise HiddenEnvironment("the buffer ends with the arguments: macOS withholds a platform binary's environment")
    entries = []
    for entry in strings:
        if not entry or entry.partition(b"=")[0] in _APPLE_KEYS:
            break
        entries.append(entry)
    return _pairs(entries)


def _procargs2(pid: int) -> bytes:
    libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
    argmax = ctypes.c_int()
    size = ctypes.c_size_t(ctypes.sizeof(argmax))
    mib = (ctypes.c_int * 2)(_CTL_KERN, _KERN_ARGMAX)
    if libc.sysctl(mib, 2, ctypes.byref(argmax), ctypes.byref(size), None, ctypes.c_size_t(0)):
        raise OSError(ctypes.get_errno(), "sysctl KERN_ARGMAX")
    buf = ctypes.create_string_buffer(argmax.value)
    size = ctypes.c_size_t(argmax.value)
    mib = (ctypes.c_int * 3)(_CTL_KERN, _KERN_PROCARGS2, pid)
    if libc.sysctl(mib, 3, buf, ctypes.byref(size), None, ctypes.c_size_t(0)):
        raise OSError(ctypes.get_errno(), f"sysctl KERN_PROCARGS2 for pid {pid}")
    return buf.raw[: size.value]


def _pairs(entries: Iterable[bytes]) -> dict[str, str]:
    env: dict[str, str] = {}
    for entry in entries:
        name, sep, value = entry.partition(b"=")
        if sep:
            env.setdefault(os.fsdecode(name), os.fsdecode(value))
    return env


def opener_vars(env: dict[str, str]) -> dict[str, str]:
    """The opener-set part of `env`: what a tab's opener decides."""
    return {name: value for name, value in env.items() if name in OPENER_SET}


def local_set_from(env: dict[str, str]) -> dict[str, str]:
    """MIRRORS `roost_ipc::opener::local_set_from`: the local-set values
    `env` brings as an opener, a relative `WAYLAND_DISPLAY` made absolute
    against `env`'s own `XDG_RUNTIME_DIR`."""
    found = {name: env[name] for name in LOCAL_SET if name in env}
    display, runtime = found.get("WAYLAND_DISPLAY"), env.get("XDG_RUNTIME_DIR", "")
    if display and not os.path.isabs(display) and os.path.isabs(runtime):
        found["WAYLAND_DISPLAY"] = os.path.join(runtime, display)
    return found


def _require_child(pid: int, allowed: set[Path]) -> None:
    """Refuse `pid` unless it is a child of this process — so its pid
    cannot be reused before this process reaps it — running one of
    `allowed`, the binaries this test launched."""
    identity = _identity(pid)
    if identity is None or identity[0] != os.getpid():
        raise AssertionError(f"pid {pid} is not a child of this test: refusing to read it")
    exe = exe_of(pid)
    if exe is None or Path(os.path.realpath(exe)) not in allowed:
        raise AssertionError(
            f"pid {pid} runs {exe}, not one of this test's binaries "
            f"{sorted(map(str, allowed))}: refusing to read it"
        )


def child_environ(pid: int, *, binaries: Iterable[Path | str]) -> dict[str, str]:
    """The environment `pid` was exec'd with, when it is a direct child of
    this test running one of `binaries` — a UI or daemon the test started
    — or nothing is read at all."""
    allowed = {Path(os.path.realpath(binary)) for binary in binaries}
    _require_child(pid, allowed)
    env = _owned_environ(
        pid,
        lambda identity, exe: identity[0] == os.getpid()
        and Path(os.path.realpath(exe)) in allowed,
    )
    if env is None:
        raise AssertionError(f"pid {pid} went away before its environment could be read")
    return env


def tab_process(
    server_pid: int, tab_id: int, *, binaries: Iterable[Path | str], timeout: float = 30.0
) -> tuple[int, dict[str, str]]:
    """`tab_id`'s process and its environment: the one direct child of
    `server_pid` whose environment carries `ROOST_TAB_ID=<tab_id>`.

    `server_pid` must be a child of this process — so its pid cannot be
    reused before this process reaps it — running one of `binaries`, the
    ones this test launched, or nothing is read at all. Polled, bounded: a
    child that has forked but not yet exec'd still has the server's
    environment, which has no `ROOST_TAB_ID`, and one inside `execve` reads
    empty. More than one match is an error.
    """
    _require_child(server_pid, {Path(os.path.realpath(binary)) for binary in binaries})
    wanted = str(tab_id)
    deadline = time.monotonic() + scaled_timeout(timeout)
    while True:
        found = []
        for child, (ppid, _) in _processes().items():
            if ppid != server_pid:
                continue
            env = _owned_environ(child, lambda identity, _exe: identity[0] == server_pid)
            if env is not None and env.get("ROOST_TAB_ID") == wanted:
                found.append((child, env))
        if len(found) > 1:
            pids = [child for child, _ in found]
            raise AssertionError(f"children {pids} of {server_pid} all claim tab {tab_id}")
        if found:
            return found[0]
        if time.monotonic() >= deadline:
            raise AssertionError(f"no child of {server_pid} is tab {tab_id} after {timeout}s")
        time.sleep(0.05)
