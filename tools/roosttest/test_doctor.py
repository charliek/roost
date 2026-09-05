"""`roostctl doctor` E2E — proves wiring, not logic (plan 003 §8).

pytest is **not** running inside a Roost tab, so most of doctor's
process-scoped checks legitimately report `skipped` here, and the overall
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
from agent_jail import Jail
from client import scaled_timeout
from util import roostctl_path, wait_shell_ready, wait_tab_attached

# The fixed check-id inventory (plan §3.12): all 39 ids appear in every
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
    "agent.hook_binary",
    "agent.claude.wired",
    "agent.claude.owning",
    "agent.claude.legacy_settings",
    "agent.codex.wired",
    "agent.codex.trust",
    "agent.codex.owning",
    "agent.grok.wired",
    "agent.grok.owning",
    "agent.cursor.wired",
    "agent.cursor.owning",
    "agent.opencode.wired",
    "agent.opencode.owning",
}

# The `tab` section's six axis checks (everything but `tab.selection`).
# They are observations: a resolved tab gives each a status-less fact,
# and when no tab could be resolved all six carry `skipped` plus one
# shared placeholder line. `axes_are_resolved` discriminates on both.
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

# Must stay `roost_agent::claude::CLAUDE_HOOK_EVENTS`: doctor's
# `claude.hook_events` check fails a settings file that is missing any
# of them, so a stale list here reads as a product bug.
CLAUDE_EVENTS = (
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
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

    Structural, not textual, on two independent signals. A resolved axis
    is a plain fact (`status: null`); an unobservable one is `skipped` —
    that split is the machine-readable half of schema 2. The placeholder
    branch also gives all six axes one *identical* detail (they share a
    single reason string), while a resolved tab gives each its own, so
    "more than one distinct detail" still holds with no prose baked in.
    """
    statuses = {checks[cid]["status"] for cid in TAB_AXIS_IDS}
    details = {checks[cid]["detail"] for cid in TAB_AXIS_IDS}
    return statuses == {None} and len(details) > 1


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
    """A minimal-but-real `~/.claude/settings.json` under `home`,
    registering `events` with a command hook — plan 046 re-points doctor's
    `claude.*` checks at Claude's own global settings file rather than the
    retired `~/.config/roost/claude-settings.json`. The command here is
    deliberately **not** one `agent install claude` would write (that
    shape has nothing left to parse an event out of — see
    `HookKind`/`resolve_hook_command` in `doctor.rs`), so this only
    exercises `claude.hook_events` (which event *keys* are registered);
    `claude.hook_command` needs a real install and is covered by
    `test_claude_install_alias_wires_settings_json_and_agent_uninstall_cleans_up_the_legacy_file`.
    """
    path = home / ".claude" / "settings.json"
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
# a. --json parses; schema_version + exit_code present; the 39 ids, unique
# ---------------------------------------------------------------------------


def test_json_shape_and_full_check_inventory():
    """`doctor --json` parses, carries `schema_version` and `exit_code`,
    and its check ids are exactly the fixed 39-id inventory (plan §3.12),
    each appearing once — regardless of environment, since this process
    is not inside a Roost tab and no `--socket` is given."""
    env = dict(os.environ)
    code, report = run_doctor([], env)

    assert report["schema_version"] == 2
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
    produces the full 39-check report — it never aborts partway through
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
      1. no settings file, no `claude` on PATH -> the whole section is `skipped`.
      2. a settings file missing `StopFailure` -> `claude.hook_events` fails.
      3. `roostctl claude install` (now a bare alias of `agent install
         claude` — plan 046 §3.5, no `--force`: explicit always wins) into
         that `HOME` -> it merges in the missing event and passes.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)

    home, empty_bin = isolated_home(tmp_path)
    env = {
        "HOME": str(home),
        "ROOST_SOCKET": str(ui.socket_path(target)),
        "ROOST_TAB_ID": str(tab),
        # An EMPTY directory, not a minimal system PATH: phase 1 asserts
        # the whole claude section is `skipped`, which requires no `claude`
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
        assert checks[cid]["status"] == "skipped", (cid, checks[cid])

    # 2. A settings file that omits StopFailure.
    write_claude_settings(home, tuple(e for e in CLAUDE_EVENTS if e != "StopFailure"))
    code, report = run_doctor([], env)
    assert code == report["exit_code"], (code, report["exit_code"])
    checks = checks_by_id(report)
    assert checks["claude.hook_events"]["status"] == "fail", checks["claude.hook_events"]

    # 3. `claude install` fixes it — no `--force`, and no `ROOST_TEST_MODE`
    # in `env` (it isn't inherited from `os.environ` here), so the install
    # engine's own harness-jail refusal never fires.
    proc = subprocess.run(
        [roostctl_path(), "claude", "install"],
        capture_output=True,
        env=env,
        timeout=30,
    )
    assert proc.returncode == 0, (
        f"roostctl claude install exited {proc.returncode}: "
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
    # `skipped` by design — so the proof is assembled from statuses that
    # pin each step:
    #   ui.agent_model == ok   -> tab.list answered, decoded, and its tab
    #                             objects carry the agent axes
    #   env.tab_id     == ok   -> $ROOST_TAB_ID parsed, so a tab IS selected
    #   tab.selection  == ok   -> the selected tab was found in tab.list;
    #                             the other arms are `skipped` (no
    #                             selection / no list) or `fail`
    # Together those force the resolved branch of the `tab` section,
    # which `axes_are_resolved` re-confirms structurally.
    assert code == report["exit_code"], (code, report["exit_code"])
    assert checks["claude.hook_events"]["status"] == "ok", checks["claude.hook_events"]
    assert checks["ui.agent_model"]["status"] == "ok", checks["ui.agent_model"]
    assert checks["env.tab_id"]["status"] == "ok", checks["env.tab_id"]
    assert checks["tab.selection"]["status"] == "ok", checks["tab.selection"]
    assert axes_are_resolved(checks), [checks[cid] for cid in TAB_AXIS_IDS]

    after_tab = agent_axes()
    after_home = snapshot_home(home)

    assert after_tab == before_tab
    assert after_home == before_home


# ---------------------------------------------------------------------------
# f. The `Agents` section (plan 046 C9) — driven against agents wired for
# real inside a jailed `$HOME`. `Jail` is `test_agent_hooks.py`'s shared
# harness helper (`agent_jail.py`); everything below reuses it rather than
# writing a second jail, and never touches the developer's own dotfiles.
# `_run_agent`/`_run_doctor_in_jail` are local copies of the same shape
# `test_agent_hooks.py` and `test_host_client.py` each keep — only `Jail`
# itself is meant to be shared (see that module's own docstring).
# ---------------------------------------------------------------------------


def _run_agent(jail: Jail, *args: str) -> subprocess.CompletedProcess:
    """`roostctl agent …` inside `jail`, forcing past the harness-jail
    refusal the same way `test_agent_hooks.py`'s helper of the same name
    does."""
    env = {**os.environ, **jail.env}
    env["ROOST_TEST_MODE"] = "1"
    env["ROOST_AGENT_HOOKS_FORCE"] = "1"
    for leaked in ("ROOST_TAB_ID", "ROOST_SOCKET"):
        env.pop(leaked, None)
    jail.assert_jailed(env)
    return subprocess.run(
        [roostctl_path(), "agent", *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=scaled_timeout(60),
    )


def _run_doctor_in_jail(jail: Jail) -> dict:
    """`roostctl doctor --json` against the same jailed config
    directories `_run_agent` wires. No `ROOST_SOCKET`/`ROOST_TAB_ID`, so
    every ui/tab-scoped check reports its usual unreachable state —
    only the `agent.*` ids are asserted on by the tests below."""
    env = {**os.environ, **jail.env}
    for leaked in ("ROOST_TAB_ID", "ROOST_SOCKET"):
        env.pop(leaked, None)
    jail.assert_jailed(env)
    proc = subprocess.run(
        [roostctl_path(), "doctor", "--json"],
        env=env,
        capture_output=True,
        timeout=30,
    )
    assert proc.returncode in (0, 1), proc.stderr.decode(errors="replace")
    return json.loads(proc.stdout)


def test_agents_section_reports_wiring_and_trust(tmp_path):
    """Every agent wired for real, then read back by doctor: `wired`
    reports the integration version the state record actually holds,
    `agent.codex.trust` matches the hash codex would compute, and there
    is nothing legacy to warn about. Covers the C9 test obligation
    ("`roostctl doctor -v` in the e2e asserts the Agents section")."""
    jail = Jail(tmp_path)
    wired = _run_agent(jail, "ensure", "--json")
    assert wired.returncode == 0, wired.stdout + wired.stderr
    record = jail.read_record()

    report = _run_doctor_in_jail(jail)
    checks = checks_by_id(report)

    for agent in ("claude", "codex", "grok", "cursor", "opencode"):
        wired_check = checks[f"agent.{agent}.wired"]
        assert wired_check["status"] == "ok", wired_check
        # The version, not just the shape of it: `wired@v` matched a
        # report that had lost track of which version was installed.
        version = record[agent]["integration_version"]
        assert wired_check["detail"] == f"wired@v{version}", wired_check
        owning_check = checks[f"agent.{agent}.owning"]
        assert owning_check["status"] == "skipped", owning_check

    assert checks["agent.codex.trust"]["status"] == "ok", checks["agent.codex.trust"]
    assert checks["agent.claude.legacy_settings"]["status"] == "ok", checks[
        "agent.claude.legacy_settings"
    ]


def test_agents_section_names_present_but_unwired_agents(tmp_path):
    """`agent-hooks = off`: the jail still creates each agent's config
    directory, so every one of them is `present` — but nothing is
    wired, and doctor says exactly that per agent, `warn` not `fail`
    (§3.6: a config switch, not a break)."""
    jail = Jail(tmp_path, agent_hooks="off")
    quiet = _run_agent(jail, "ensure", "--json")
    assert quiet.returncode == 0, quiet.stdout + quiet.stderr

    report = _run_doctor_in_jail(jail)
    checks = checks_by_id(report)
    for agent in ("claude", "codex", "grok", "cursor", "opencode"):
        wired_check = checks[f"agent.{agent}.wired"]
        assert wired_check["status"] == "warn", wired_check
        assert "not wired" in wired_check["detail"], wired_check
    assert checks["agent.codex.trust"]["status"] == "skipped", checks["agent.codex.trust"]


def test_wiring_is_read_off_the_agents_own_config_not_the_record(tmp_path):
    """Roost's state record is not the authority on whether an agent is
    wired — the agent's own config file is. Deleting
    `~/.config/roost/agent-hooks.json` leaves a fully wired machine, and
    a doctor that reported "present, not wired" there would send the
    user to reinstall something already installed."""
    jail = Jail(tmp_path)
    wired = _run_agent(jail, "ensure", "--json")
    assert wired.returncode == 0, wired.stdout + wired.stderr
    assert jail.record.exists()

    jail.record.unlink()

    checks = checks_by_id(_run_doctor_in_jail(jail))
    for agent in ("claude", "codex", "grok", "cursor", "opencode"):
        wired_check = checks[f"agent.{agent}.wired"]
        assert wired_check["status"] == "warn", wired_check
        assert "state record has no entry" in wired_check["detail"], wired_check
        assert "present, not wired" not in wired_check["detail"], wired_check
    # The entries really are still there, which is what makes the
    # "not wired" reading wrong rather than merely pessimistic.
    assert checks["agent.codex.trust"]["status"] == "ok", checks["agent.codex.trust"]


def test_a_hand_edited_codex_handler_is_reported_as_trust_drift(tmp_path):
    """`agent.codex.trust` has to hash the handler that is on disk. An
    edited `timeout` leaves the command byte-identical — codex hashes the
    new value and opens its review dialog — so a check that re-derived
    the expected hash from Roost's canonical command would report `ok`
    straight through that dialog."""
    jail = Jail(tmp_path)
    wired = _run_agent(jail, "ensure", "--json")
    assert wired.returncode == 0, wired.stdout + wired.stderr
    assert checks_by_id(_run_doctor_in_jail(jail))["agent.codex.trust"]["status"] == "ok"

    hooks_path = jail.agent_dirs["codex"] / "hooks.json"
    hooks = json.loads(hooks_path.read_text())
    hooks["hooks"]["Stop"][0]["hooks"][0]["timeout"] = 20
    hooks_path.write_text(json.dumps(hooks))

    trust = checks_by_id(_run_doctor_in_jail(jail))["agent.codex.trust"]
    assert trust["status"] == "fail", trust
    assert "Stop" in trust["detail"], trust

    # And `agent ensure` is the remedy the check names.
    again = _run_agent(jail, "ensure", "--json")
    assert again.returncode == 0, again.stdout + again.stderr
    assert checks_by_id(_run_doctor_in_jail(jail))["agent.codex.trust"]["status"] == "ok"


def _write_legacy_settings(jail: Jail, events) -> Path:
    """A `claude-settings.json` in the shape the retired writer produced,
    over exactly `events`."""
    legacy = jail.home / ".config" / "roost" / "claude-settings.json"
    legacy.parent.mkdir(parents=True, exist_ok=True)
    legacy.write_text(
        json.dumps(
            {
                "hooks": {
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
            }
        )
    )
    return legacy


# The complete event set `roostctl claude install` wrote in every
# release up to and including v0.0.19 — the file almost every machine
# that ran it is carrying. MIRRORS `roost-cli`'s
# `LEGACY_GENERATED_EVENT_SETS[1]`.
LEGACY_EVENTS = (
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
)


def test_legacy_claude_settings_warns_and_uninstall_cleans_it_up(tmp_path):
    """`agent.claude.legacy_settings` warns when the pre-046 file *and*
    its shell alias are both still present — that is the case where every
    Claude hook event really is delivered twice — names both removal
    steps, and `agent uninstall claude` retires the file, because it
    still holds exactly what `claude install` used to write (plan 046
    §3.5)."""
    jail = Jail(tmp_path)
    legacy = _write_legacy_settings(jail, LEGACY_EVENTS)
    bashrc = jail.home / ".bashrc"
    bashrc.write_text(
        "alias claude='claude --settings /somewhere/claude-settings.json'\n"
    )

    report = _run_doctor_in_jail(jail)
    checks = checks_by_id(report)
    legacy_check = checks["agent.claude.legacy_settings"]
    assert legacy_check["status"] == "warn", legacy_check
    assert "claude-settings.json" in legacy_check["detail"], legacy_check
    assert "shell rc" in legacy_check["detail"], legacy_check
    assert "delivered twice" in legacy_check["detail"], legacy_check

    removed = _run_agent(jail, "uninstall", "claude")
    assert removed.returncode == 0, removed.stdout + removed.stderr
    assert not legacy.exists(), "a shape-matching legacy file must be deleted"


def test_a_commented_out_alias_is_not_reported_as_an_active_one(tmp_path):
    """The scan asks for a live alias line, not for two words somewhere
    in the same file. A commented-out example — which is what the retired
    docs told people to paste — delivers nothing, and warning that every
    Claude event fires twice because of one is a diagnostic doing
    harm."""
    jail = Jail(tmp_path)
    (jail.home / ".bashrc").write_text(
        "# roostctl claude install used to print:\n"
        "#   alias claude='claude --settings '~/.config/roost/claude-settings.json\n"
        "export EDITOR=vi\n"
    )
    check = checks_by_id(_run_doctor_in_jail(jail))["agent.claude.legacy_settings"]
    assert check["status"] == "ok", check


def test_uninstall_leaves_a_hand_edited_legacy_file_alone(tmp_path):
    """The promise behind the delete: only a file that still matches what
    `claude install` wrote is Roost's to remove. A file trimmed to the
    events its owner cared about is a hand-edited file — it is reported
    and left exactly where it is."""
    jail = Jail(tmp_path)
    legacy = _write_legacy_settings(jail, ("Stop",))
    before = legacy.read_text()

    removed = _run_agent(jail, "uninstall", "claude")
    assert removed.returncode == 0, removed.stdout + removed.stderr
    assert legacy.exists(), "a trimmed legacy file is not Roost's to delete"
    assert legacy.read_text() == before
    assert "left in place" in removed.stderr, removed.stderr


def test_a_refused_uninstall_does_not_delete_the_legacy_file(tmp_path):
    """The legacy cleanup is a side effect of an uninstall that ran, not
    something that runs beside one. Under the harness jail
    (`ROOST_TEST_MODE=1` with no explicit force) the uninstall is refused
    and exits non-zero — and a cleanup that went ahead anyway would
    delete a real dotfile the install engine's own guard cannot see, then
    report it removed."""
    jail = Jail(tmp_path)
    legacy = _write_legacy_settings(jail, LEGACY_EVENTS)

    env = {**os.environ, **jail.env}
    env["ROOST_TEST_MODE"] = "1"
    env.pop("ROOST_AGENT_HOOKS_FORCE", None)
    for leaked in ("ROOST_TAB_ID", "ROOST_SOCKET"):
        env.pop(leaked, None)
    jail.assert_jailed(env)
    refused = subprocess.run(
        [roostctl_path(), "agent", "uninstall", "claude"],
        env=env,
        capture_output=True,
        text=True,
        timeout=scaled_timeout(60),
    )

    assert refused.returncode != 0, refused.stdout + refused.stderr
    assert legacy.exists(), "a refused uninstall deleted the legacy file anyway"
    assert "removed" not in refused.stderr, refused.stderr
