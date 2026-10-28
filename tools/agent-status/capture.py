#!/usr/bin/env python3
"""Run an agent TUI in a pty, answer the common terminal queries, and log
OSC 7501 (plus OSC 133 / titles / notifications) with timestamps.

usage: capture.py --name N --cwd DIR --out OUTDIR --script scripts/N.json -- cmd args...

OUTDIR is required and must be outside this repository. Each run writes
N.events.jsonl (decoded events), N.raw.bin (raw PTY bytes) and N.text.log.

steps: [{"wait": "<regex on visible text>", "timeout": 60},
        {"wait7501": "<regex on raw 7501 payload>", "timeout": 120},
        {"send": "text with \\r etc"}, {"sleep": 2}, {"quiet": 3, "timeout": 30},
        {"optional_wait": "<regex>", "timeout": 10, "then_send": "\\r"}]
"""
import argparse
import base64
import codecs
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

# A nested Claude Code inherits CLAUDE_CODE_CHILD_SESSION and runs with the
# parent's permission mode, which hides every permission prompt. Roost, herdr
# and multiplexer variables change what an agent detects about its terminal.
SCRUB_PREFIXES = (
    "ROOST_", "CLAUDE", "CODEX_COMPANION_", "HERDR_",
    "TMUX", "ZELLIJ", "WEZTERM", "KITTY_", "TERM_PROGRAM",
)
SCRUB_NAMES = ("STY",)

PROBE_REPLY_BODY = b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth"

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def scrub_env(environ, extra=()):
    env = {k: v for k, v in environ.items()
           if k not in SCRUB_NAMES and not k.startswith(SCRUB_PREFIXES)}
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    for kv in extra:
        k, v = kv.split("=", 1)
        env[k] = v
    return env


def probe_reply(term):
    """libghostty's reply once GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS is set,
    terminated like the query."""
    return PROBE_REPLY_BODY + term


def decode7501(payload):
    body = payload[len("7501;"):]
    if body == "?":
        return {"query": True}
    fields = {}
    for part in body.split(":"):
        if "=" in part:
            k, v = part.split("=", 1)
            fields[k] = v
            if k in ("msg", "title"):
                try:
                    fields[k + "_decoded"] = base64.b64decode(v + "=" * (-len(v) % 4)).decode("utf-8", "replace")
                except Exception as e:  # noqa: BLE001
                    fields[k + "_decoded"] = f"<b64 error {e}>"
    return fields


def inside(path, root):
    return path == root or path.startswith(root + os.sep)


def output_paths(out, name):
    """The three output files, each checked after symlinks are followed:
    inside --out and outside the repository."""
    if not name or "/" in name or os.sep in name or (os.altsep and os.altsep in name) or ".." in name:
        raise ValueError(f"--name {name!r} must be a plain file name without path separators or '..'")
    out_real = os.path.realpath(out)
    if inside(out_real, REPO_ROOT):
        raise ValueError(f"--out {out} is inside the repository; pass a directory outside it")
    os.makedirs(out_real, exist_ok=True)
    paths = [os.path.join(out_real, f"{name}{ext}") for ext in (".events.jsonl", ".raw.bin", ".text.log")]
    for p in paths:
        real = os.path.realpath(p)
        if inside(real, REPO_ROOT) or os.path.dirname(real) != out_real:
            raise ValueError(f"{p} resolves to {real}, outside --out or inside the repository")
    return paths


def reaped(pid, fd, seconds, on_data):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            p, _ = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return True
        if p == pid:
            return True
        try:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                on_data(os.read(fd, 65536))
        except (OSError, ValueError):
            pass
    return False


def terminate(pid, fd, on_data):
    """SIGHUP, SIGTERM, then SIGKILL the child's process group (pty.fork makes
    the child a session leader), reap it and close the pty."""
    try:
        pgid = os.getpgid(pid)
    except ProcessLookupError:
        pgid = pid
    try:
        done = False
        for sig, wait in ((signal.SIGHUP, 1.5), (signal.SIGTERM, 1.5), (signal.SIGKILL, 1.0)):
            try:
                os.killpg(pgid, sig)
            except ProcessLookupError:
                done = True
                break
            if reaped(pid, fd, wait, on_data):
                done = True
                break
        try:
            os.killpg(pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if not done:
            reaped(pid, fd, 2.0, on_data)
    finally:
        try:
            os.close(fd)
        except OSError:
            pass


def parse_args(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--name", required=True)
    ap.add_argument("--cwd", required=True)
    ap.add_argument("--out", required=True, help="output directory, outside the repository")
    ap.add_argument("--script", required=True)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--max-seconds", type=float, default=600)
    ap.add_argument("--no-7501-reply", action="store_true")
    ap.add_argument("--env", action="append", default=[], help="KEY=VALUE added to the child environment after the scrub")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    return ap.parse_args(argv)


def run(a):
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    with open(a.script) as f:
        steps = json.load(f)
    ev_path, raw_path, text_path = output_paths(a.out, a.name)
    evlog = open(ev_path, "w")
    rawlog = open(raw_path, "wb")
    textlog = open(text_path, "w")
    T0 = time.monotonic()
    seen7501 = []


    def now():
        return round(time.monotonic() - T0, 3)


    def ev(kind, **kw):
        if kind == "osc7501":
            seen7501.append(kw["raw"])
        rec = {"t": now(), "kind": kind, **kw}
        evlog.write(json.dumps(rec) + "\n")
        evlog.flush()
        if kind in ("osc7501", "probe7501", "step", "osc133", "note", "exit"):
            print(json.dumps(rec), flush=True)


    env = scrub_env(os.environ, a.env)
    result = "aborted"
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(a.cwd)
        os.execvpe(cmd[0], cmd, env)
    try:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, 0, 0))
        ev("note", msg="spawned", cmd=cmd, pid=pid, cwd=a.cwd)

        buf = b""          # unparsed tail
        text = ""          # visible text accumulated since last step mark
        alltext = ""
        titles_seen = set()
        dec = codecs.getincrementaldecoder("utf-8")("replace")


        def write(b):
            try:
                os.write(fd, b)
            except OSError:
                pass


        def reply(desc, b):
            ev("reply", query=desc, reply=b.decode("latin1"))
            write(b)


        def handle_osc(payload, term):
            st = term
            if payload.startswith("7501;"):
                if payload == "7501;?":
                    ev("probe7501", raw=payload, terminator=repr(term))
                    if not a.no_7501_reply:
                        reply("osc7501?", probe_reply(term))
                else:
                    ev("osc7501", raw=payload, fields=decode7501(payload), terminator=repr(term))
                return
            num = payload.split(";", 1)[0]
            if num in ("10", "11", "12") and payload.endswith(";?"):
                col = {"10": "ffff/ffff/ffff", "11": "0000/0000/0000", "12": "ffff/ffff/ffff"}[num]
                reply(f"osc{num}?", f"\x1b]{num};rgb:{col}".encode() + st)
                return
            if num == "4" and payload.endswith("?"):
                m = re.match(r"4;(\d+);\?", payload)
                if m:
                    reply("osc4?", f"\x1b]4;{m.group(1)};rgb:8080/8080/8080".encode() + st)
                return
            if num == "133":
                ev("osc133", raw=payload)
            elif num in ("0", "2"):
                t = payload.split(";", 1)[1] if ";" in payload else ""
                if t not in titles_seen:
                    titles_seen.add(t)
                    ev("title", raw=payload)
            elif num in ("9", "777", "99", "1337", "21337"):
                ev("osc_other", raw=payload)
            else:
                ev("osc_misc", raw=payload[:200])


        def handle_csi(params, final):
            seq = params + final
            if final == "c" and params in ("", "0"):
                reply("DA1", b"\x1b[?62;22c")
            elif seq == "6n":
                reply("DSR6", b"\x1b[1;1R")
            elif seq == "5n":
                reply("DSR5", b"\x1b[0n")
            elif seq == "?u":
                reply("kittykbd?", b"\x1b[?0u")
            elif seq == "18t":
                reply("18t", f"\x1b[8;{a.rows};{a.cols}t".encode())
            elif seq == "14t":
                reply("14t", f"\x1b[4;{a.rows*20};{a.cols*10}t".encode())
            elif seq == "16t":
                reply("16t", b"\x1b[6;20;10t")
            elif final in "cnqpu" and (params.startswith("?") or params.startswith(">") or "$" in params):
                ev("query_unanswered", seq=seq)


        def feed(data):
            """Parse escape sequences out of data; return visible text."""
            nonlocal buf
            buf += data
            out = []
            i = 0
            n = len(buf)
            while i < n:
                b = buf[i]
                if b != 0x1b:
                    j = buf.find(b"\x1b", i)
                    j = n if j < 0 else j
                    out.append(buf[i:j])
                    i = j
                    continue
                if i + 1 >= n:
                    break
                c = buf[i + 1]
                if c == ord("]") or c in (ord("P"), ord("_"), ord("^"), ord("X")):
                    bel = buf.find(b"\x07", i + 2) if c == ord("]") else -1
                    st = buf.find(b"\x1b\\", i + 2)
                    cands = [x for x in (bel, st) if x >= 0]
                    if not cands:
                        if n - i > 4_000_000:
                            i = n
                        break
                    end = min(cands)
                    term = b"\x07" if end == bel else b"\x1b\\"
                    if c == ord("]"):
                        handle_osc(buf[i + 2:end].decode("utf-8", "replace"), term)
                    i = end + len(term)
                    continue
                if c == ord("["):
                    m = re.compile(rb"\x1b\[([\x30-\x3f]*[\x20-\x2f]*)([\x40-\x7e])").match(buf, i)
                    if not m:
                        if n - i > 64:
                            i += 2
                            continue
                        break
                    params, final = m.group(1).decode(), chr(m.group(2)[0])
                    handle_csi(params, final)
                    if final in "HfABCDGdEF":
                        out.append(b" ")
                    if final in "HfdEF" or (final == "J"):
                        out.append(b"\n")
                    i = m.end()
                    continue
                # two/three byte escape
                if c in (ord("("), ord(")"), ord("#"), ord("%")):
                    if i + 2 >= n:
                        break
                    i += 3
                else:
                    i += 2
            buf = buf[i:]
            return dec.decode(b"".join(out))


        ANSI_JUNK = re.compile(r"[\x00-\x08\x0b-\x1f\x7f]")

        pending = list(steps)
        step = None
        step_deadline = None
        last_output = time.monotonic()
        child_alive = True
        hard_deadline = time.monotonic() + a.max_seconds

        def start_next():
            nonlocal step, step_deadline, text
            if not pending:
                step = None
                return
            step = pending.pop(0)
            step_deadline = time.monotonic() + float(step.get("timeout", 60))
            ev("step", step=step)
            text = ""
            seen7501.clear()


        start_next()
        result = "ok"
        while True:
            if time.monotonic() > hard_deadline:
                result = "hard-timeout"
                break
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    data = b""
                if not data:
                    child_alive = False
                    ev("exit", msg="pty EOF")
                    break
                rawlog.write(data)
                last_output = time.monotonic()
                t = ANSI_JUNK.sub("", feed(data))
                text += t
                alltext_piece = t
                textlog.write(alltext_piece)
                if len(text) > 400_000:
                    text = text[-200_000:]
            # step progress
            if step is None:
                break
            done = False
            if "send" in step:
                s = step["send"].encode().decode("unicode_escape").encode("latin1")
                for ch in [s] if not step.get("slow") else [bytes([x]) for x in s]:
                    write(ch)
                    if step.get("slow"):
                        time.sleep(0.03)
                done = True
            elif "sleep" in step:
                if time.monotonic() >= step_deadline - float(step.get("timeout", 60)) + step["sleep"]:
                    done = True
            elif "wait" in step or "optional_wait" in step:
                pat = step.get("wait") or step.get("optional_wait")
                flat = re.sub(r"\s+", " ", text)
                if re.search(pat, flat, re.I):
                    ev("note", msg=f"matched {pat!r}")
                    if step.get("then_send"):
                        write(step["then_send"].encode().decode("unicode_escape").encode("latin1"))
                    done = True
            elif "wait7501" in step:
                if any(re.search(step["wait7501"], p) for p in seen7501):
                    done = True
            elif "quiet" in step:
                if time.monotonic() - last_output >= step["quiet"]:
                    done = True
            if not done and time.monotonic() > step_deadline:
                if "optional_wait" in step or step.get("optional"):
                    ev("note", msg=f"optional step timed out: {step}")
                    done = True
                else:
                    ev("note", msg=f"step TIMEOUT: {step}")
                    result = "step-timeout"
                    break
            if done:
                start_next()
                if step is None:
                    # drain a little more output after the last step
                    end = time.monotonic() + 2
                    while time.monotonic() < end:
                        r, _, _ = select.select([fd], [], [], 0.05)
                        if r:
                            try:
                                d = os.read(fd, 65536)
                            except OSError:
                                d = b""
                            if not d:
                                child_alive = False
                                ev("exit", msg="pty EOF")
                                break
                            rawlog.write(d)
                            textlog.write(ANSI_JUNK.sub("", feed(d)))
                    break

    finally:
        terminate(pid, fd, lambda d: (rawlog.write(d), feed(d)))
        ev("note", msg=f"done result={result}")
        for f in (evlog, rawlog, textlog):
            f.close()
    print("RESULT", result)
    return 0 if result == "ok" else 1


def main(argv=None):
    a = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        output_paths(a.out, a.name)
    except ValueError as e:
        print(e, file=sys.stderr)
        return 2
    return run(a)


if __name__ == "__main__":
    sys.exit(main())
