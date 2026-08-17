#!/usr/bin/env python3
"""Re-runnable echo-latency probe for the iced UI's `App::view()` path.

Reads `app.render_stats` counter deltas (`view_calls`/`view_nanos`/
`elide_calls`/`elide_nanos` — plan 029 C1) around a scripted burst of
single-character `tab.write` echoes into a scratch `/bin/cat` tab, instead
of scraping log lines from an instrumented build the way the diagnosing
session's `probe_real.py` did. No instrumented build, no log scraping —
just the counters every iced build already carries.

    tools/perf/echo-latency.py                              # attach (default)
    tools/perf/echo-latency.py --socket /path/to/roost.sock  # attach elsewhere
    tools/perf/echo-latency.py --launch target/debug/roost-iced

Prints one JSON object to stdout:
    {"view_calls": ..., "view_avg_us": ..., "elide_calls": ...,
     "elide_avg_us_per_view": ..., "keystrokes": ...}

Absolute numbers are machine- and profile-state-dependent (see
tools/perf/README.md) — the A/B delta on one machine, one run to the
next, is the meaningful signal, not the raw number in isolation.
"""
import argparse
import base64
import json
import os
import pathlib
import socket
import subprocess
import sys
import time

SCRATCH_TITLE = "echo-latency-probe"
ECHO_CHARS = "abcdefghij"


def default_socket(launching: bool) -> pathlib.Path:
    """The iced UI's socket. Attach mode matches CLAUDE.md's canonical
    resolution for `--target iced`: the isolated dev profile on macOS
    (`Roost-iced`, distinct from the Swift app's `Roost` namespace so
    both can run side by side), the shared production namespace on
    Linux (`roost/`, the same socket the packaged `/usr/bin/roost`
    binary uses). `--launch` forces `ROOST_BUNDLE_PROFILE=iced`, which
    on Linux resolves to a *separate* `roost-iced/` runtime dir
    (`crates/roost-ipc/src/paths.rs`) — so launch mode defaults to that
    path instead of the production one.
    """
    if sys.platform == "darwin":
        return pathlib.Path.home() / "Library/Caches/Roost-iced/roost.sock"
    namespace = "roost-iced" if launching else "roost"
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg and xdg.startswith("/"):
        return pathlib.Path(xdg) / namespace / "roost.sock"
    return pathlib.Path(f"/tmp/{namespace}-{os.getuid()}/roost.sock")


class Client:
    def __init__(self, path: pathlib.Path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(30)
        self.sock.connect(str(path))
        self.file = self.sock.makefile("rwb")
        self.next_id = 0

    def call(self, op: str, params: dict | None = None) -> dict:
        self.next_id += 1
        request = {"id": str(self.next_id), "op": op, "params": params or {}}
        self.file.write((json.dumps(request) + "\n").encode())
        self.file.flush()
        reply = json.loads(self.file.readline())
        if reply.get("error"):
            raise RuntimeError(f"{op}: {reply['error']}")
        return reply.get("result", {})

    def close(self):
        try:
            self.file.close()
        finally:
            self.sock.close()


def quit_running_roost_iced():
    for _ in range(20):
        pids = subprocess.run(
            ["pgrep", "-x", "roost-iced"], capture_output=True, text=True
        ).stdout.split()
        if not pids:
            return
        for pid in pids:
            subprocess.run(["kill", pid], check=False)
        time.sleep(0.5)


def connect_with_retry(sock_path: pathlib.Path, timeout_s: float) -> Client:
    deadline = time.time() + timeout_s
    last_error: Exception | None = None
    while time.time() < deadline:
        if sock_path.exists():
            try:
                client = Client(sock_path)
                client.call("identify")
                return client
            except Exception as exc:
                last_error = exc
        time.sleep(0.2)
    raise TimeoutError(
        f"no iced UI answered at {sock_path} within {timeout_s}s"
        + (f" (last error: {last_error})" if last_error else "")
    )


def read_render_stats(client: Client, reset: bool) -> dict:
    result = client.call("app.render_stats", {"reset": reset})
    return {key: int(value) for key, value in result.items()}


def run_probe(client: Client, keystrokes: int, rate_hz: float) -> dict:
    projects = client.call("tab.list").get("projects") or []
    if not projects:
        raise RuntimeError("no projects to open a scratch tab in")
    project_id = projects[0]["id"]

    scratch_id = None
    try:
        opened = client.call(
            "tab.open",
            {
                "project_id": str(project_id),
                "cwd": "/tmp",
                "argv": ["/bin/cat"],
                "cols": 100,
                "rows": 30,
                "title": SCRATCH_TITLE,
            },
        )
        scratch_id = opened["tab"]["id"]
        time.sleep(2.0)  # let the tab settle before sampling

        read_render_stats(client, reset=True)

        interval_s = 1.0 / rate_hz
        for i in range(keystrokes):
            char = ECHO_CHARS[i % len(ECHO_CHARS)]
            data = base64.b64encode(char.encode()).decode()
            client.call("tab.write", {"tab_id": str(scratch_id), "data": data})
            time.sleep(interval_s)

        stats = read_render_stats(client, reset=False)
        view_calls = stats["view_calls"]
        view_nanos = stats["view_nanos"]
        elide_calls = stats["elide_calls"]
        elide_nanos = stats["elide_nanos"]
        return {
            "view_calls": view_calls,
            "view_avg_us": (view_nanos / view_calls / 1000.0) if view_calls else None,
            "elide_calls": elide_calls,
            "elide_avg_us_per_view": (
                (elide_nanos / 1000.0 / view_calls) if view_calls else 0.0
            ),
            "keystrokes": keystrokes,
        }
    finally:
        if scratch_id is not None:
            try:
                client.call("tab.close", {"tab_id": str(scratch_id)})
            except Exception as exc:
                print(f"warning: failed to close scratch tab: {exc}", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--attach",
        action="store_true",
        help="attach to an already-running iced UI (default when --launch is absent)",
    )
    parser.add_argument(
        "--launch",
        metavar="BINARY",
        help="quit any running roost-iced, launch BINARY, and probe it",
    )
    parser.add_argument(
        "--socket",
        metavar="PATH",
        help="override the socket path (defaults per platform and mode — see default_socket())",
    )
    parser.add_argument(
        "--keystrokes", type=int, default=240, help="echo keystrokes to drive (default: 240)"
    )
    parser.add_argument(
        "--rate-hz", type=float, default=30.0, help="keystroke rate in Hz (default: 30)"
    )
    args = parser.parse_args()

    sock_path = (
        pathlib.Path(args.socket) if args.socket else default_socket(bool(args.launch))
    )

    child = None
    client = None
    try:
        if args.launch:
            print("==> quitting any running roost-iced", file=sys.stderr)
            quit_running_roost_iced()
            env = dict(os.environ)
            env["ROOST_BUNDLE_PROFILE"] = "iced"
            env["ROOST_TEST_MODE"] = "1"
            print(f"==> launching {args.launch}", file=sys.stderr)
            child = subprocess.Popen(
                [args.launch],
                env=env,
                stdout=sys.stderr,
                stderr=subprocess.STDOUT,
            )
            print(f"==> waiting for socket at {sock_path}", file=sys.stderr)
            client = connect_with_retry(sock_path, timeout_s=60.0)
        else:
            print(f"==> attaching to {sock_path}", file=sys.stderr)
            client = connect_with_retry(sock_path, timeout_s=10.0)

        result = run_probe(client, args.keystrokes, args.rate_hz)
        print(json.dumps(result))
        return 0
    finally:
        if client is not None:
            client.close()
        if child is not None:
            child.terminate()
            try:
                child.wait(10)
            except subprocess.TimeoutExpired:
                child.kill()


if __name__ == "__main__":
    sys.exit(main())
