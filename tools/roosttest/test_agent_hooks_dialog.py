"""The agent-integration card, end to end — plan 064 §7 W5, reshaped by
the move off config writes.

One dialog, two modes: the **first-run** card a launch raises by itself
only when it has something to ask (neither agent key answered, and Cursor,
the one agent Roost still connects by writing its file, installed), and
the **preferences** card `Agent Integration…` opens from the command
palette, with a switch per agent. Both are driven
through `app.dialog_dump` / `app.dialog_answer`, which is
`tools/roosttest/`'s only view of a modal — see
`docs/reference/ipc.md` for the shapes.

Every case here launches its **own** jailed Roost (`agent_jail.py`),
never the harness's shared session UI, for three reasons that all point
the same way:

* The thing under test is what a UI does at *launch*, and the session UI
  launched before this module existed.
* The card is only raised when the key is unanswered, and the harness's
  own `fixtures/launcher.conf` answers it `off` — which is exactly the
  fence that keeps every other lane away from a real dotfile.
* Confirming the card **writes agent config files**. The jailed launch is
  the one place in the tree that lifts the install engine's test-mode
  refusal (`ROOST_AGENT_HOOKS_FORCE=1`), and everything it can reach is
  inside the jail. `Jail.assert_jailed` runs on the merged environment
  immediately before every spawn.
"""

from __future__ import annotations

from agent_jail import (
    INSTALLABLE_AGENTS,
    Jail,
    jailed_socket,
    jailed_ui,
    wait_for_jailed_window,
    wait_for_log_line,
)
from client import Roost, scaled_timeout
from util import wait_for_config_line


# ---------------------------------------------------------------------------
# Driving one jailed UI
# ---------------------------------------------------------------------------


def client(jail: Jail) -> Roost:
    return Roost(str(jailed_socket(jail)), timeout=scaled_timeout(30))


def dump(roost: Roost) -> dict:
    return roost.call("app.dialog_dump", {})


def answer(roost: Roost, action: str) -> dict:
    return roost.call("app.dialog_answer", {"action": action})


def wait_for_card(roost: Roost, mode: str) -> dict:
    """Block until the consent card is up in `mode`, and return its dump.

    A condition wait rather than a settle: the card is raised from the
    engine feed, one `spawn_blocking` round trip after `window_opened`
    returned, so it is never up the instant the window is."""
    seen: list[dict] = []

    def carded() -> bool:
        card = dump(roost)
        if card.get("dialog") != "agent_hooks":
            return False
        seen.append(card)
        return True

    Roost._wait(carded, 30.0, f"the agent-hooks card to open in {mode} mode")
    card = seen[0]
    assert card["mode"] == mode, card
    return card


def assert_no_card(roost: Roost) -> None:
    card = dump(roost)
    assert card.get("dialog") is None, f"a dialog was raised: {card}"


def assert_untouched(jail: Jail) -> None:
    """Nothing was written: no key, no state record, no agent file.

    Read after the jailed process has exited (`jailed_ui`'s teardown
    waits for it), so an absence is an absence rather than a race."""
    assert jail.read_key() is None, "the key was answered"
    assert not jail.record.exists(), "the state record was written"
    for agent in INSTALLABLE_AGENTS:
        directory = jail.agent_dirs[agent]
        if not directory.exists():
            continue
        contents = sorted(p.name for p in directory.iterdir())
        assert contents == [], f"{agent}'s config dir was written: {contents}"


# ---------------------------------------------------------------------------
# First run
# ---------------------------------------------------------------------------


def test_an_unanswered_key_with_cursor_raises_the_card_once(short_root):
    """The card, its one row, and the latch that keeps it to one.

    Claude Code, Codex, Grok and OpenCode connect without a file, so they
    are on by default and have no row; the card asks only about Cursor,
    whose hooks Roost would write into its own `hooks.json`, and starts it
    off.

    The focus cycle is the half that would pass without the fix.
    `window_opened` runs on every focus change, not just the open, so
    without its own latch a user alt-tabbing back would get a second card
    over the one they were reading — or, having dismissed it, a fresh one.
    The latch itself is pinned deterministically in `agent_hooks.rs`
    (`the_first_run_card_is_raised_once_per_process`); this is the same
    claim against a real UI, driven through `app.set_window_focus`,
    which takes the whole production focus route."""
    jail = Jail(short_root, agent_hooks=None, present=("claude", "cursor"))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            card = wait_for_card(roost, "first_run")
            assert card["title"] == "Agent integration"
            assert "Claude Code is already connected" in card["body"], card
            assert card["buttons"] == ["Decide later", "Continue"], card

            rows = card["rows"]
            assert [row["agent"] for row in rows] == ["cursor"], rows
            cursor = rows[0]
            assert cursor["on"] is False, cursor
            assert cursor["found"] is True, cursor
            assert cursor["delivery"] == "file", cursor
            # The first-run card renders no status line, so the dump
            # reports none: it says what the user is being told.
            assert cursor["status"] is None, cursor
            assert len(cursor["files"]) == 1, cursor
            assert cursor["files"][0].startswith(str(jail.root)), cursor

            # Dismissed, and then a focus cycle: no second card.
            answer(roost, "cancel")
            assert_no_card(roost)
            roost.app_set_window_focus(focus=False)
            roost.app_set_window_focus(focus=True)
            assert_no_card(roost)
        # The deterministic half. `app.set_window_focus` runs
        # `window_opened` to completion before it answers, and the latch
        # is claimed (and logged) inside that call — so a reply in hand
        # means the decision is on disk, with no status walk to race.
        raised = log.read_text(errors="replace").count(
            "agent-hooks consent card requested"
        )
        assert raised == 1, f"the card was requested {raised} times, not once"

    # "Decide later" wrote nothing at all — not the key, which is what
    # makes the next launch ask again rather than treating silence as an
    # answer.
    assert_untouched(jail)
    assert jail.read_integration() is None

    # And it does ask again.
    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            wait_for_card(roost, "first_run")
            answer(roost, "cancel")
    assert_untouched(jail)


def continue_card(jail: Jail, *, cursor: bool) -> None:
    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            wait_for_card(roost, "first_run")
            if cursor:
                answer(roost, "toggle:cursor")
                assert [row["on"] for row in dump(roost)["rows"]] == [True]
            answer(roost, "confirm")
            wait_for_log_line(
                log,
                "agent.set_hooks applied",
                "the jailed UI to apply the card's answer",
            )
            assert_no_card(roost)


def test_continue_records_the_defaults_and_cursors_answer(short_root):
    """Continue answers the question for good: the defaults go into
    `agent-integration` with Cursor's answer, and `agent-hooks` becomes
    the shadow of what Roost writes by file — Cursor's entries only if
    its switch was on."""
    unchecked = Jail(short_root / "no", agent_hooks=None, present=("cursor",))
    continue_card(unchecked, cursor=False)
    assert unchecked.read_integration() == "claude, codex, grok, opencode"
    assert unchecked.read_key() == "off"
    hooks = unchecked.agent_dirs["cursor"] / "hooks.json"
    assert not hooks.exists() or "ROOST_AGENT_HOOK" not in hooks.read_text()

    checked = Jail(short_root / "yes", agent_hooks=None, present=("cursor",))
    continue_card(checked, cursor=True)
    assert checked.read_integration() == "claude, codex, grok, cursor, opencode"
    assert checked.read_key() == "cursor"
    hooks = checked.agent_dirs["cursor"] / "hooks.json"
    assert "ROOST_AGENT_HOOK" in hooks.read_text(), hooks.read_text()


def test_no_file_agent_here_asks_nothing(short_root):
    """Without Cursor there is nothing to ask — every other agent
    connects without touching a file and is on by default — so no card
    is raised and both keys stay absent."""
    jail = Jail(
        short_root,
        agent_hooks=None,
        present=("claude", "codex", "grok", "opencode"),
    )

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        # The decision is made on the engine feed, one status walk after
        # the window opened, so wait for the line that records it rather
        # than for an absence.
        wait_for_log_line(
            log,
            "no agent here needs a file opt-in",
            "the jailed UI to decline to ask about agent integration",
        )
        with client(jail) as roost:
            assert_no_card(roost)

    assert_untouched(jail)
    assert jail.read_integration() is None


def test_an_answered_key_asks_nothing(short_root):
    """`off` and an allow-list are both answers, so neither is asked
    again.

    `wait_for_jailed_window` is the proof rather than a settle: the
    screenshot it waits on is answered on the same thread that ran
    `window_opened` to completion, and the decision not to ask is made
    inside that call — so if no card is up by then, none was ever going
    to be."""
    off = Jail(short_root / "off", agent_hooks="off", present=("claude",))
    with jailed_ui(off) as (proc, log):
        wait_for_jailed_window(off, proc, log)
        with client(off) as roost:
            assert_no_card(roost)
    assert off.read_key() == "off"
    assert not off.record.exists(), "`off` wired something at startup"

    allowed = Jail(short_root / "allow", agent_hooks="claude", present=("claude",))
    with jailed_ui(allowed) as (proc, log):
        wait_for_jailed_window(allowed, proc, log)
        with client(allowed) as roost:
            assert_no_card(roost)
        # The answer carries over into `agent-integration`, and the
        # `agent-hooks` shadow narrows to the file agents (none here), so
        # Claude is on through the tab environment and no file is written.
        wait_for_config_line(allowed.config, "agent-integration", "claude, -codex, -grok, -opencode")
    assert allowed.read_key() == "off"
    settings = allowed.agent_dirs["claude"] / "settings.json"
    assert not settings.exists() or "ROOST_AGENT_HOOK" not in settings.read_text()


def test_the_test_mode_fence_stops_the_card(short_root):
    """`ROOST_TEST_MODE=1` without `ROOST_AGENT_HOOKS_FORCE=1` asks
    nothing.

    This is the fence that protects every other lane in the whole suite:
    the harness's own UI runs under `ROOST_TEST_MODE` against the
    developer's real `$HOME`, and a consent card raised there would put a
    button that edits `~/.claude/settings.json` in front of a test run.
    Asserted inside the jail, so proving it costs nothing."""
    jail = Jail(short_root, agent_hooks=None, present=INSTALLABLE_AGENTS)

    with jailed_ui(jail, force=False) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            assert_no_card(roost)

    assert_untouched(jail)


# ---------------------------------------------------------------------------
# Preferences
# ---------------------------------------------------------------------------


def open_preferences(roost: Roost) -> dict:
    """`Agent Integration…` through the command palette, the way a user reaches
    it — the action is default-unbound, so the palette row is the one
    surface it has."""
    roost.palette_dismiss()
    roost.palette_open()
    roost.palette_activate("agent_hooks")
    roost.palette_dismiss()
    return wait_for_card(roost, "preferences")


def test_preferences_reads_the_switches_off_disk(short_root):
    """Opening the card re-reads the switches **from disk**, not from
    the snapshot this process launched with, and says how each agent is
    connected.

    This process is not the keys' only writer — `roostctl agent set
    --local` writes them with no UI running — so a card built from the
    launch-time value would show switches that disagree with the file it
    is about to overwrite. The key is changed underneath a *running* UI
    here, which is the only way to tell the two apart.

    Cancel is Esc's route (`App::agent_hooks_key` maps both to the same
    dismiss), and it writes nothing."""
    jail = Jail(short_root, agent_hooks=None, present=("claude", "codex", "cursor"))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            wait_for_card(roost, "first_run")
            answer(roost, "cancel")

            # Somebody else switches Claude off while this UI runs.
            jail.config.write_text("agent-integration = -claude\n")

            card = open_preferences(roost)
            assert card["buttons"] == ["Cancel", "Apply"], card
            rows = {row["agent"]: row for row in card["rows"]}
            assert list(rows) == list(INSTALLABLE_AGENTS), rows
            assert {agent: row["delivery"] for agent, row in rows.items()} == {
                "claude": "env",
                "codex": "7501",
                "grok": "7501",
                "cursor": "file",
                "opencode": "env",
            }, rows
            assert rows["claude"]["on"] is False, (
                "the card read its launch-time snapshot, not the key on disk"
            )
            assert rows["claude"]["status"] == "Off", rows["claude"]
            assert rows["codex"]["on"] is True, rows["codex"]
            assert rows["codex"]["status"] == "Built in", rows["codex"]
            assert rows["grok"]["status"] == "not found", rows["grok"]
            assert rows["cursor"]["on"] is False, rows["cursor"]
            assert rows["cursor"]["status"] == "Off", rows["cursor"]
            # Only the file agent names a file.
            assert [a for a, row in rows.items() if row["files"]] == ["cursor"], rows

            answer(roost, "cancel")
            assert_no_card(roost)

    assert jail.read_integration() == "-claude", "cancel rewrote the switches"
    assert not jail.record.exists(), "cancel wired something"
    for agent in ("claude", "codex", "cursor"):
        assert not any(jail.agent_dirs[agent].iterdir()), f"cancel wrote into {agent}"


def test_a_survey_does_not_steal_the_screen_from_another_dialog(short_root):
    """A survey finishes in the background. If the user has opened
    something else in the meantime it stands down — a card that appears
    over a question somebody is already answering takes the screen away
    from them, and preferences is one palette row away."""
    jail = Jail(short_root, agent_hooks="claude", present=("claude",))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            # Any other member of the `HostDialog` family will do; the
            # local-backend switch is the one reachable from a palette
            # row with no host set up.
            roost.palette_dismiss()
            roost.palette_open()
            roost.palette_activate("local:use_session")
            roost.palette_dismiss()
            assert dump(roost).get("dialog") == "confirm_switch", dump(roost)

            # Now ask for the agent-hooks card: its survey completes and
            # finds the screen taken.
            roost.palette_open()
            roost.palette_activate("agent_hooks")
            roost.palette_dismiss()
            wait_for_log_line(
                log,
                "another dialog is open; not raising the agent-hooks card",
                "the survey to stand down",
            )
            assert dump(roost).get("dialog") == "confirm_switch", (
                "the survey replaced a dialog the user was already answering"
            )
            answer(roost, "cancel")


def test_preferences_reports_what_is_wired_and_can_turn_it_all_off(
    short_root
):
    """The status line after a real wiring, and the zero case. Cursor,
    the one agent still wired by file, is the one with a status to show.

    Every switch off is `off`, not an empty value — an empty value parses
    back as "nobody has answered" and would bring the first-run card
    round again on the next launch — which is why the button says so
    rather than reading a bare "Apply"."""
    jail = Jail(short_root, agent_hooks="cursor", present=("cursor", "grok"))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        wait_for_log_line(
            log,
            "agent hooks toast shown",
            "the jailed UI to wire the agent its key names",
        )
        with client(jail) as roost:
            card = open_preferences(roost)
            rows = {row["agent"]: row for row in card["rows"]}
            assert rows["cursor"]["status"].startswith("wired v"), rows["cursor"]
            assert rows["cursor"]["on"] is True, rows["cursor"]
            # Installed, and the carried-over answer did not name it.
            assert rows["grok"]["status"] == "Off", rows["grok"]
            assert rows["grok"]["on"] is False, rows["grok"]

            answer(roost, "toggle:cursor")
            card = dump(roost)
            assert all(not row["on"] for row in card["rows"]), card
            assert card["buttons"] == ["Cancel", "Apply: turn off"], card

            answer(roost, "confirm")
            Roost._wait(
                lambda: jail.read_key() == "off",
                30.0,
                "the apply to write `agent-hooks = off`",
            )
            assert_no_card(roost)

    assert jail.read_key() == "off"
    assert jail.read_integration() == "off"
    # Roost created this file, so an uninstall deletes it outright; a
    # file the user already had would be written back without Roost's
    # entry instead. Both are "the entry is gone", which is the claim.
    hooks = jail.agent_dirs["cursor"] / "hooks.json"
    assert not hooks.exists() or "ROOST_AGENT_HOOK" not in hooks.read_text(), (
        f"turning cursor off left its entry behind: {hooks.read_text()}"
    )
