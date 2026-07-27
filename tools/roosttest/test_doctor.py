"""`roostctl doctor` E2E — proves wiring, not logic (plan 003 §8).

pytest is **not** running inside a Roost tab, so most of doctor's
process-scoped checks legitimately report `info` here, and the overall
process exit code is **not** reliably 0 — `ui.target` / `ui.socket` fail
whenever nothing is reachable at the default profile paths, which is the
documented meaning of "no Roost UI is running" (plan §3.3 / AC 7). Every
assertion below is therefore on **section ids, check ids, and per-check
statuses** — never on prose, and never on a hard-coded whole-run exit
code. The exit *code* is still pinned, relationally: the process's
exit status must equal the report's own `exit_code` (§3.3 is what CI and
shell scripts read, so a report that says `1` while the process exits `0`
is a break even though every JSON assertion still passes).

`--socket` is a **global** flag on `Args`, not on `Doctor`'s own
subcommand args, so it must precede the subcommand:
`roostctl --socket PATH doctor`, not `roostctl doctor --socket PATH`.

Both target-override rungs are covered, deliberately split:
`test_ui_checks_ok_against_the_harness_ui` drives the `--socket` flag
positively, and `test_socket_env_override_is_honored_...` drives
`ROOST_SOCKET` at a dead path *while the harness UI is running*, so
auto-detect would have succeeded — the failure is only explicable by the
env var being honored. (`ROOST_SOCKET` is also the variable real Roost
tabs export, which is why it gets the load-bearing test.)
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
from pathlib import Path

import ui
from util import roostctl_path, wait_shell_ready, wait_tab_attached

# The fixed check-id inventory (plan §3.12): all 26 ids appear in every
# report, in every environment — a missing id is a bug, not a passing
# check.
EXPECTED_CHECK_IDS = {
    "env.tab_id",
    "env.socket",
    "ui.target",
    "ui.socket",
    "ui.identify",
    "ui.version",
    "ui.agent_model",
    "shell.login",
    "shell.current",
    "shell.integration",
    "shell.resources",
    "shell.marks_feature",
    "shell.marks_capability",
    "shell.marks_observed",
    "tab.selection",
    "tab.shell_state",
    "tab.agent_lifecycle",
    "tab.attention",
    "tab.ownership",
    "tab.derived",
    "tab.raw_osc",
    "claude.binary",
    "claude.settings",
    "claude.hook_events",
    "claude.hook_command",
    "claude.observed",
}

# The `tab` section's six axis checks (everything but `tab.selection`).
# When no tab could be resolved they are all replaced by one shared
# placeholder line, which is what `axes_are_resolved` discriminates on.
TAB_AXIS_IDS = (
    "tab.shell_state",
    "tab.agent_lifecycle",
    "tab.attention",
    "tab.ownership",
    "tab.derived",
    "tab.raw_osc",
)

# The four `tab.list` fields a mutation would land in — see
# `test_doctor_is_read_only`.
AGENT_AXES = ("agent_lifecycle", "ownership", "has_notification", "hook_active")

CLAUDE_EVENTS = (
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
)


def run_doctor(
    argv_prefix: list[str], env: dict[str, str], timeout: float = 30
) -> tuple[int, dict]:
    """Run `roostctl <argv_prefix...> doctor --json`; return
    `(returncode, report)`.

    The real return code is **returned, not swallowed**: doctor's
    exit-code policy (0 unless some check failed — plan §3.3 / AC 1) is
    the part scripts and CI consume, so every caller asserts it equals
    the report's own `exit_code`. Accepting "0 or 1" and then only
    checking the JSON would let a process that exits 0 while reporting
    `"exit_code": 1` pass the whole suite.

    Two guards run before stdout is parsed, so a regression fails with
    context instead of a bare JSON decode error:
      * the exit status is one of doctor's own two documented outcomes —
        a clap usage error (exit 2) or a panic must fail loudly;
      * stderr is empty — doctor's only output is the report, so a
        warning printed alongside otherwise-valid JSON is a break.
    """
    argv = [roostctl_path(), *argv_prefix, "doctor", "--json"]
    proc = subprocess.run(argv, capture_output=True, env=env, timeout=timeout)
    assert proc.returncode in (0, 1), (
        f"{' '.join(argv)} exited {proc.returncode}: "
        f"{proc.stderr.decode(errors='replace')}"
    )
    assert proc.stderr == b"", (
        f"{' '.join(argv)} wrote to stderr: {proc.stderr.decode(errors='replace')}"
    )
    return proc.returncode, json.loads(proc.stdout)


def checks_by_id(report: dict) -> dict[str, dict]:
    return {c["id"]: c for section in report["sections"] for c in section["checks"]}


def check_ids(report: dict) -> list[str]:
    return [c["id"] for s in report["sections"] for c in s["checks"]]


def axes_are_resolved(checks: dict[str, dict]) -> bool:
    """Whether the `tab` section is reporting a real tab rather than its
    "unavailable" placeholder.

    Structural, not textual: the placeholder branch gives all six axis
    checks one *identical* detail (they share a single reason string),
    while a resolved tab gives each its own. So "more than one distinct
    detail" is the discriminator, with no prose baked into the test.
    """
    details = {checks[cid]["detail"] for cid in TAB_AXIS_IDS}
    return len(details) > 1


def isolated_home(tmp_path: Path) -> tuple[Path, Path]:
    """A throwaway `$HOME`, plus an **empty** directory to point `$PATH` at.

    The empty PATH is load-bearing, not hygiene. `/usr/bin:/bin` does not
    guarantee `claude` is absent — a system-wide install lives there on
    some machines and would break the "nothing is configured yet" phase —
    and doctor *executes* `claude --version`, so a real binary running
    under a throwaway `$HOME` could initialize its own state inside the
    very tree the read-only proof snapshots, and doctor would take the
    blame. `doctor.rs`'s own `collect_leaves_home_byte_identical` points
    PATH at an empty directory for exactly this reason; this mirrors it.

    `$HOME` is a subdirectory rather than `tmp_path` itself so the empty
    PATH dir sits outside the snapshotted tree.
    """
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    empty_bin = tmp_path / "empty-bin"
    empty_bin.mkdir(exist_ok=True)
    return home, empty_bin


def write_claude_settings(home: Path, events: tuple[str, ...]) -> Path:
    """A minimal-but-real `claude-settings.json` under `home`, registering
    `events` with a command hook pointing back at this tree's `roostctl`
    — the same shape `claude install` writes (plan §2.4 / §3.7)."""
    path = home / ".config" / "roost" / "claude-settings.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    hooks = {
        event: [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": f"{roostctl_path()} claude-hook {event}",
                    }
                ]
            }
        ]
        for event in events
    }
    path.write_text(json.dumps({"hooks": hooks}) + "\n")
    return path


def snapshot_home(home: Path) -> dict[str, str]:
    """Recursive `{relative_path: sha256_hex}` over every regular file
    under `home` — the read-only proof's baseline (AC 12). A mutation of
    any kind (write, truncate, new file) shows up as a diff against a
    prior snapshot; an empty/missing tree snapshots to `{}`."""
    if not home.exists():
        return {}
    return {
        str(p.relative_to(home)): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in sorted(home.rglob("*"))
        if p.is_file()
    }


# ---------------------------------------------------------------------------
# a. --json parses; schema_version + exit_code present; the 26 ids, unique
# ---------------------------------------------------------------------------


def test_json_shape_and_full_check_inventory():
    """`doctor --json` parses, carries `schema_version` and `exit_code`,
    and its check ids are exactly the fixed 26-id inventory (plan §3.12),
    each appearing once — regardless of environment, since this process
    is not inside a Roost tab and no `--socket` is given."""
    env = dict(os.environ)
    code, report = run_doctor([], env)

    assert report["schema_version"] == 1
    assert isinstance(report["exit_code"], int)
    assert report["exit_code"] in (0, 1)
    assert code == report["exit_code"], (code, report["exit_code"])

    ids = check_ids(report)
    assert len(ids) == len(set(ids)), f"duplicate check ids: {ids}"
    assert set(ids) == EXPECTED_CHECK_IDS, (
        f"missing: {sorted(EXPECTED_CHECK_IDS - set(ids))}, "
        f"unexpected: {sorted(set(ids) - EXPECTED_CHECK_IDS)}"
    )


# ---------------------------------------------------------------------------
# b. --socket -> the harness UI: ui.socket / ui.identify / ui.agent_model ok
# ---------------------------------------------------------------------------


def test_ui_checks_ok_against_the_harness_ui(roost, project, target):
    """With `--socket` pointed at the harness UI, `ui.socket` and
    `ui.identify` are `ok`, and — the one assertion CI makes that a unit
    test structurally cannot — `ui.agent_model` is `ok` too, because it
    proves a real, *current* server actually emits the agent axes on the
    wire (plan §8 bullet 2).

    This is the `--socket` flag's positive coverage; `ROOST_SOCKET`'s
    lives in the two `$HOME`-isolated tests below, and the proof that the
    env var is actually consulted lives in the next test.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)

    env = dict(os.environ)
    code, report = run_doctor(["--socket", str(ui.socket_path(target))], env)
    checks = checks_by_id(report)

    assert code == report["exit_code"], (code, report["exit_code"])
    assert checks["ui.socket"]["status"] == "ok", checks["ui.socket"]
    assert checks["ui.identify"]["status"] == "ok", checks["ui.identify"]
    assert checks["ui.agent_model"]["status"] == "ok", checks["ui.agent_model"]


def shell_section(report: dict) -> list[dict]:
    return [c for s in report["sections"] if s["id"] == "shell" for c in s["checks"]]


# ---------------------------------------------------------------------------
# b2. ROOST_SOCKET alone must not be read as "inside a Roost tab"
# ---------------------------------------------------------------------------


def test_socket_env_alone_does_not_claim_a_roost_tab(roost, project, target):
    """`ROOST_SOCKET` and `ROOST_TAB_ID` are documented *user-settable*
    targeting variables — `docs/reference/cli.md` says verbatim to "set
    them by hand only when invoking the CLI from outside a Roost tab (e.g.
    a CI runner)". Keying doctor's process-scope applicability on them
    made doing exactly that flip the whole `shell` section into judge
    mode: `env.tab_id`, `shell.integration` and `shell.resources` all
    `fail` on a healthy machine, which is the outcome plan §3.3's
    applicability rule exists to prevent.

    So this asserts the rule directly: from outside a Roost tab (no
    `ROOST_SHELL_INTEGRATION`, no `ROOST_RESOURCES_DIR` — the two
    variables nothing but a Roost PTY sets), the `shell` section contains
    **no `fail`**, no matter which targeting variables are set. The suite
    previously could not see this class of regression at all: every other
    test sets `ROOST_SOCKET`/`ROOST_TAB_ID` and none asserts on `shell.*`.

    `ROOST_SOCKET` points at the *live* harness UI, so the run is healthy
    in every other respect and a failure here is unambiguous.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)

    base = {k: v for k, v in os.environ.items() if not k.startswith("ROOST_")}
    for extra in (
        {"ROOST_SOCKET": str(ui.socket_path(target))},
        {"ROOST_SOCKET": str(ui.socket_path(target)), "ROOST_TAB_ID": str(tab)},
    ):
        code, report = run_doctor([], {**base, **extra})
        checks = checks_by_id(report)

        assert code == report["exit_code"], (code, report["exit_code"])
        # The premise: the UI really was reachable, so nothing else can
        # explain a shell-section failure.
        assert checks["ui.socket"]["status"] == "ok", (extra, checks["ui.socket"])
        failed = [c for c in shell_section(report) if c["status"] == "fail"]
        assert failed == [], (extra, failed)
        assert checks["env.tab_id"]["status"] != "fail", (extra, checks["env.tab_id"])


# ---------------------------------------------------------------------------
# c. ROOST_SOCKET at a nonexistent path: ui.socket fails, exit 1, full report
# ---------------------------------------------------------------------------


def test_socket_env_override_is_honored_and_report_is_complete(tmp_path, roost):
    """`ROOST_SOCKET` pointed at a path nothing is listening on: `ui.socket`
    is `fail`, the process exits 1 (matching the report), and doctor still
    produces the full 26-check report — it never aborts partway through
    (plan §8 bullet 3 / AC 1).

    The failure is driven through the **env var**, not `--socket`, and the
    harness UI is deliberately left running: the control run below (same
    env, `ROOST_SOCKET` removed) reaches a UI and reports `ui.socket: ok`,
    so auto-detect demonstrably *would* have found one. That is what makes
    this a test of the override rather than of "nothing was running" —
    pointing the variable at the UI's own default path would pass
    identically if target resolution ignored the variable entirely.
    """
    # The premise: a UI really is up, so auto-detect has something to find.
    assert roost.identify()["pid"] > 0

    nonexistent = tmp_path / "does-not-exist" / "roost.sock"
    base = {k: v for k, v in os.environ.items() if not k.startswith("ROOST_")}

    control_code, control = run_doctor([], base)
    control_checks = checks_by_id(control)
    assert control_checks["ui.socket"]["status"] == "ok", (
        "control: auto-detect must reach the harness UI, otherwise the "
        f"override below proves nothing — {control_checks['ui.socket']}"
    )
    assert control_code == control["exit_code"], (control_code, control["exit_code"])

    code, report = run_doctor([], {**base, "ROOST_SOCKET": str(nonexistent)})
    checks = checks_by_id(report)

    assert checks["ui.socket"]["status"] == "fail", checks["ui.socket"]
    assert report["exit_code"] == 1
    assert code == report["exit_code"], (code, report["exit_code"])

    ids = check_ids(report)
    assert len(ids) == len(set(ids))
    assert set(ids) == EXPECTED_CHECK_IDS


# ---------------------------------------------------------------------------
# d. Claude-section isolation via HOME=<tmp_path>
# ---------------------------------------------------------------------------


def test_claude_section_isolated_by_home(tmp_path, roost, project, target):
    """`HOME` fully determines where doctor looks for
    `claude-settings.json` (plan §2.4), so a throwaway `HOME` isolates the
    `claude` section from whatever is really installed on this machine.
    `ROOST_SOCKET` is passed explicitly too, since `BundleProfile` also
    derives its default paths from `HOME` — without it, target resolution
    would try sockets under the tmp tree instead of the harness UI.

    Three phases, one throwaway `HOME` (plan §8 bullet 4):
      1. no settings file, no `claude` on PATH -> the whole section is `info`.
      2. a settings file missing `StopFailure` -> `claude.hook_events` fails.
      3. `roostctl claude install --force` into that `HOME` -> it passes.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)

    home, empty_bin = isolated_home(tmp_path)
    env = {
        "HOME": str(home),
        "ROOST_SOCKET": str(ui.socket_path(target)),
        "ROOST_TAB_ID": str(tab),
        # An EMPTY directory, not a minimal system PATH: phase 1 asserts
        # the whole claude section is `info`, which requires no `claude`
        # binary to be reachable — and `/usr/bin:/bin` cannot promise
        # that. See `isolated_home`.
        "PATH": str(empty_bin),
    }

    # 1. Nothing configured yet.
    code, report = run_doctor([], env)
    assert code == report["exit_code"], (code, report["exit_code"])
    checks = checks_by_id(report)
    claude_ids = [cid for cid in EXPECTED_CHECK_IDS if cid.startswith("claude.")]
    assert claude_ids, "sanity: the claude.* id set must be non-empty"
    for cid in claude_ids:
        assert checks[cid]["status"] == "info", (cid, checks[cid])

    # 2. A settings file that omits StopFailure.
    write_claude_settings(home, tuple(e for e in CLAUDE_EVENTS if e != "StopFailure"))
    code, report = run_doctor([], env)
    assert code == report["exit_code"], (code, report["exit_code"])
    checks = checks_by_id(report)
    assert checks["claude.hook_events"]["status"] == "fail", checks["claude.hook_events"]

    # 3. `claude install --force` fixes it.
    proc = subprocess.run(
        [roostctl_path(), "claude", "install", "--force"],
        capture_output=True,
        env=env,
        timeout=30,
    )
    assert proc.returncode == 0, (
        f"roostctl claude install --force exited {proc.returncode}: "
        f"{proc.stderr.decode(errors='replace')}"
    )
    code, report = run_doctor([], env)
    assert code == report["exit_code"], (code, report["exit_code"])
    checks = checks_by_id(report)
    assert checks["claude.hook_events"]["status"] == "ok", checks["claude.hook_events"]


# ---------------------------------------------------------------------------
# e. Read-only proof (AC 12)
# ---------------------------------------------------------------------------


def test_doctor_is_read_only(tmp_path, roost, project, target):
    """Doctor reports and links; it never mutates (plan §3.1). Two
    independent proofs, captured before and after one doctor run and
    required to be identical: the selected tab's agent axes over IPC, and
    the full contents of a throwaway `$HOME` tree doctor was pointed at."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    # `wait_tab_attached` only proves the TerminalView is live. A shell
    # that is still booting emits OSC 0/7/133 on its own schedule, so
    # without this the first title/cwd/mark could land BETWEEN the two
    # reads below and be blamed on doctor.
    wait_shell_ready(roost, tab)

    home, empty_bin = isolated_home(tmp_path)
    write_claude_settings(home, CLAUDE_EVENTS)

    env = {
        "HOME": str(home),
        "ROOST_SOCKET": str(ui.socket_path(target)),
        "ROOST_TAB_ID": str(tab),
        # Empty on purpose: doctor executes `claude --version`, and the
        # real binary running under this throwaway $HOME could write its
        # own state into the tree snapshotted below. See `isolated_home`.
        "PATH": str(empty_bin),
    }

    def agent_axes() -> dict:
        """Only the axes an *agent report* moves.

        `title`, `cwd`, `shell_state` and the `state` derived from it are
        shell-driven: a healthy shell keeps emitting OSC 0/7/133, so one
        landing between the two reads would fail this test for something
        doctor never touched. Doctor sends exactly two read-only ops
        (`identify` + `tab.list`), so a mutation by doctor would have to
        surface in these four — narrowing the snapshot removes the
        volatile fields without removing the thing being proven.
        """
        row = roost.tab(tab) or {}
        return {axis: row.get(axis) for axis in AGENT_AXES}

    before_tab = agent_axes()
    before_home = snapshot_home(home)
    assert before_home, "the fixture must have something to lose"

    code, report = run_doctor([], env)
    checks = checks_by_id(report)

    # Not vacuous: doctor really read the settings file and really
    # resolved this tab. `tab.selection != "fail"` alone would NOT show
    # that — the branch taken when `tab.list` was never read reports
    # `info` by design — so the proof is assembled from statuses that pin
    # each step:
    #   ui.agent_model == ok   -> tab.list answered, decoded, and its tab
    #                             objects carry the agent axes
    #   env.tab_id     == ok   -> $ROOST_TAB_ID parsed, so a tab IS selected
    #   tab.selection  == info -> with a selection and a decoded list, the
    #                             only non-`fail` outcome is "found in
    #                             tab.list"
    # Together those force the resolved branch of the `tab` section,
    # which `axes_are_resolved` re-confirms structurally.
    assert code == report["exit_code"], (code, report["exit_code"])
    assert checks["claude.hook_events"]["status"] == "ok", checks["claude.hook_events"]
    assert checks["ui.agent_model"]["status"] == "ok", checks["ui.agent_model"]
    assert checks["env.tab_id"]["status"] == "ok", checks["env.tab_id"]
    assert checks["tab.selection"]["status"] == "info", checks["tab.selection"]
    assert axes_are_resolved(checks), [checks[cid] for cid in TAB_AXIS_IDS]

    after_tab = agent_axes()
    after_home = snapshot_home(home)

    assert after_tab == before_tab
    assert after_home == before_home
