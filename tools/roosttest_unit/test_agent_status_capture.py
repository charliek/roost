"""tools/agent-status/capture.py: decoder, probe reply, environment scrub.

The directory name has a dash, so the module is loaded by path.
"""
import base64
import contextlib
import io
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest

CAPTURE = os.path.join(os.path.dirname(__file__), "..", "agent-status", "capture.py")
spec = importlib.util.spec_from_file_location("agent_status_capture", CAPTURE)
capture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture)

REPLY = b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth"

FAKE_AGENT = r"""
import os, sys, time
sys.stdout.write("\x1b]7501;?\x07")
sys.stdout.flush()
time.sleep(0.2)
sys.stdout.write("\x1b]7501;?\x1b\\")
sys.stdout.flush()
open(sys.argv[1], "w").write("\n".join(sorted(os.environ)))
time.sleep(0.8)
"""


def b64(s):
    return base64.b64encode(s.encode()).decode()


class Decoder(unittest.TestCase):
    def test_fields_and_base64_msg_and_title(self):
        got = capture.decode7501(
            f"7501;state=blocked:kind=permission:app=claude-code:msg={b64('approve Bash: ls')}:title={b64('Claude')}"
        )
        self.assertEqual(got["state"], "blocked")
        self.assertEqual(got["kind"], "permission")
        self.assertEqual(got["app"], "claude-code")
        self.assertEqual(got["msg_decoded"], "approve Bash: ls")
        self.assertEqual(got["title_decoded"], "Claude")

    def test_unpadded_base64(self):
        full = base64.b64encode(b"hi").decode()
        raw = full.rstrip("=")
        self.assertNotEqual(full, raw)
        self.assertEqual(capture.decode7501(f"7501;state=done:msg={raw}")["msg_decoded"], "hi")

    def test_probe_query(self):
        self.assertEqual(capture.decode7501("7501;?"), {"query": True})

    def test_clear_without_fields(self):
        self.assertEqual(capture.decode7501("7501;state=clear:id="), {"state": "clear", "id": ""})


class ProbeReply(unittest.TestCase):
    def test_bel_query_gets_bel(self):
        self.assertEqual(capture.probe_reply(b"\x07"), REPLY + b"\x07")

    def test_st_query_gets_st(self):
        self.assertEqual(capture.probe_reply(b"\x1b\\"), REPLY + b"\x1b\\")


class EnvScrub(unittest.TestCase):
    def test_strips_roost_agent_and_multiplexer_variables(self):
        dirty = {
            "ROOST_SOCKET": "x", "ROOST_TAB_ID": "1", "CLAUDE_CODE_CHILD_SESSION": "1", "CLAUDECODE": "1",
            "CODEX_COMPANION_X": "1", "HERDR_ENV": "1", "TMUX": "t", "TMUX_PANE": "%1", "STY": "s",
            "ZELLIJ": "0", "ZELLIJ_SESSION_NAME": "z", "WEZTERM_PANE": "1", "KITTY_WINDOW_ID": "1",
            "TERM_PROGRAM": "WezTerm", "TERM_PROGRAM_VERSION": "1",
            "PATH": "/bin", "HOME": "/h", "TERM": "dumb",
        }
        env = capture.scrub_env(dirty)
        self.assertEqual(sorted(env), ["COLORTERM", "HOME", "PATH", "TERM"])
        self.assertEqual(env["TERM"], "xterm-256color")
        self.assertEqual(env["COLORTERM"], "truecolor")

    def test_extra_vars_survive_the_scrub(self):
        env = capture.scrub_env({"CLAUDE_X": "1"}, ["CLAUDE_AX_SCREEN_READER=1"])
        self.assertEqual(env["CLAUDE_AX_SCREEN_READER"], "1")
        self.assertNotIn("CLAUDE_X", env)


class FakeAgentRun(unittest.TestCase):
    def test_pty_run_replies_to_both_terminators_and_scrubs(self):
        with tempfile.TemporaryDirectory() as d:
            script = os.path.join(d, "steps.json")
            with open(script, "w") as f:
                json.dump([{"sleep": 0.5}], f)
            envfile = os.path.join(d, "env.txt")
            out = os.path.join(d, "out")
            os.makedirs(out)
            parent = {**os.environ, "ROOST_TAB_ID": "leak", "CLAUDE_CODE_CHILD_SESSION": "1"}
            proc = subprocess.run(
                [sys.executable, CAPTURE, "--name", "fake", "--cwd", d, "--out", out, "--script", script,
                 "--", sys.executable, "-c", FAKE_AGENT, envfile],
                env=parent, capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(out, "fake.events.jsonl")) as f:
                events = [json.loads(line) for line in f]
            probes = [e for e in events if e["kind"] == "probe7501"]
            self.assertEqual([e["terminator"] for e in probes], [repr(b"\x07"), repr(b"\x1b\\")])
            replies = [e["reply"].encode("latin1") for e in events if e["kind"] == "reply"]
            self.assertEqual(replies, [REPLY + b"\x07", REPLY + b"\x1b\\"])
            with open(envfile) as f:
                seen = f.read().split("\n")
            self.assertNotIn("ROOST_TAB_ID", seen)
            self.assertNotIn("CLAUDE_CODE_CHILD_SESSION", seen)

    def test_refuses_an_output_directory_inside_the_repo(self):
        proc = subprocess.run(
            [sys.executable, CAPTURE, "--name", "x", "--cwd", ".", "--out", os.path.join(capture.REPO_ROOT, "target", "x"),
             "--script", "none", "--", "true"],
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(proc.returncode, 2)


STUBBORN_AGENT = r"""
import signal, sys, time
signal.signal(signal.SIGHUP, signal.SIG_IGN)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
sys.stdout.write("READY\n")
sys.stdout.flush()
time.sleep(120)
"""


class Cleanup(unittest.TestCase):
    def test_exception_in_the_loop_still_kills_and_reaps_the_child(self):
        import re
        with tempfile.TemporaryDirectory() as d:
            script = os.path.join(d, "steps.json")
            with open(script, "w") as f:
                json.dump([{"wait": "READY", "timeout": 10}, {"wait": "(unclosed", "timeout": 10}], f)
            out = os.path.join(d, "out")
            a = capture.parse_args(["--name", "boom", "--cwd", d, "--out", out, "--script", script,
                                    "--", sys.executable, "-c", STUBBORN_AGENT])
            with contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(re.error):
                    capture.run(a)
            with open(os.path.join(out, "boom.events.jsonl")) as f:
                spawned = [json.loads(line) for line in f if '"spawned"' in line][0]
            with self.assertRaises(ProcessLookupError):
                os.kill(spawned["pid"], 0)
            with self.assertRaises(ProcessLookupError):
                os.killpg(spawned["pid"], 0)


class OutputPaths(unittest.TestCase):
    def test_name_cannot_escape_out(self):
        with tempfile.TemporaryDirectory() as d:
            for bad in (os.path.join(capture.REPO_ROOT, "capture"), "../x", "a/b", "..", ""):
                with self.assertRaises(ValueError, msg=bad):
                    capture.output_paths(d, bad)

    def test_symlinked_destination_into_the_repo_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            target = os.path.join(capture.REPO_ROOT, "target", "r082-symlink-target")
            os.symlink(target, os.path.join(d, "x.raw.bin"))
            with self.assertRaises(ValueError):
                capture.output_paths(d, "x")

    def test_symlinked_destination_outside_out_is_rejected(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as other:
            os.symlink(os.path.join(other, "f"), os.path.join(d, "x.text.log"))
            with self.assertRaises(ValueError):
                capture.output_paths(d, "x")

    def test_plain_name_is_accepted(self):
        with tempfile.TemporaryDirectory() as d:
            paths = capture.output_paths(d, "codex-permission")
            self.assertEqual([os.path.basename(p) for p in paths],
                             ["codex-permission.events.jsonl", "codex-permission.raw.bin", "codex-permission.text.log"])


if __name__ == "__main__":
    unittest.main()
