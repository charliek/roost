"""The agent-hooks consent card, end to end — plan 064 §7 W5.

One dialog, two modes: the **first-run** card a launch with an
unanswered `agent-hooks` key raises by itself, and the **preferences**
card `Agent Hooks…` opens from the command palette. Both are driven
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

`iced_only`: `make e2e-mac` collects this whole directory, and the Mac
app has no dialog test ops at all (C8 mirrors this card as an `NSAlert`
sheet and brings its own coverage).
"""

from __future__ import annotations

import pytest
from agent_jail import (
    INSTALLABLE_AGENTS,
    Jail,
    jailed_socket,
    jailed_ui,
    wait_for_jailed_window,
    wait_for_log_line,
)
from client import Roost, scaled_timeout


@pytest.fixture
def iced_only(target):
    if target != "iced":
        pytest.skip("the Mac app has no app.dialog_* ops; C8 covers its sheet")


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


def test_an_unanswered_key_raises_the_card_once(short_root, iced_only):
    """The card, its five rows, and the latch that keeps it to one.

    All five agents get a row whether or not they are installed — an
    agent the user only has on a host is still something to consent to —
    and the rows arrive in the install engine's `ALL_AGENTS` order, which
    is the order the card draws them in. Only the *installed* ones start
    on (plan 064 D2: conservative consent).

    The focus cycle is the half that would pass without the fix. `window_opened`
    runs on every focus change, not just the open, so without its own
    latch a user alt-tabbing back would get a second card over the one
    they were reading — or, having dismissed it, a fresh one. The latch
    itself is pinned deterministically in `agent_hooks.rs`
    (`the_first_run_card_is_raised_once_per_process`); this is the same
    claim against a real UI, driven through `app.set_window_focus`,
    which takes the whole production focus route."""
    jail = Jail(short_root, agent_hooks=None, present=("claude", "cursor"))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            card = wait_for_card(roost, "first_run")
            assert card["title"] == "Agent hooks"
            assert card["body"].startswith(
                "Roost adds a hook to each agent you switch on"
            ), card
            assert "roostctl agent uninstall --all" in card["body"], card
            assert card["buttons"] == ["Decide later", "Instrument 2"], card

            rows = card["rows"]
            assert [row["agent"] for row in rows] == list(INSTALLABLE_AGENTS), rows
            assert [row["on"] for row in rows] == [True, False, False, True, False], rows
            assert [row["found"] for row in rows] == [True, False, False, True, False], rows
            # The first-run card renders no status line, so the dump
            # reports none: it says what the user is being told.
            assert all(row["status"] is None for row in rows), rows
            # Every row names the files an Apply would touch — two for
            # codex — and all of them are inside the jail.
            assert [len(row["files"]) for row in rows] == [1, 2, 1, 1, 1], rows
            for row in rows:
                for path in row["files"]:
                    assert path.startswith(str(jail.root)), row

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

    # And it does ask again.
    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            wait_for_card(roost, "first_run")
            answer(roost, "cancel")
    assert_untouched(jail)


def test_confirming_writes_the_key_and_only_the_agents_that_are_here(
    short_root, iced_only
):
    """Apply: the key, the files, the receipt — and the one agent that is
    named but not installed.

    `toggle:codex` switches on an agent this jail does not have. That is
    the case D2 exists for: a user whose codex lives on a host says so
    once, here, and the key carries it to every host this Roost connects
    to. Locally it must be a *skip* — `NotPresent` — and not a mkdir:
    Roost creating `~/.codex` for a codex nobody installed would be a
    directory the user never asked for, in a tool they may not use.
    """
    jail = Jail(short_root, agent_hooks=None, present=("claude",))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            card = wait_for_card(roost, "first_run")
            assert card["buttons"] == ["Decide later", "Instrument 1"], card

            answer(roost, "toggle:codex")
            card = dump(roost)
            assert [row["on"] for row in card["rows"]] == [
                True,
                True,
                False,
                False,
                False,
            ], card
            assert card["buttons"] == ["Decide later", "Instrument 2"], card

            answer(roost, "confirm")
            # The receipt, read off the log: no IPC op carries the status
            # banner (see `wait_for_log_line`).
            toast = wait_for_log_line(
                log,
                "agent hooks toast shown",
                "the jailed UI to put the agent-hooks receipt on the banner",
            )
            # claude alone: the toast names what was *wired*, and codex
            # is not here to wire.
            assert "for claude —" in toast, toast
            assert "Agent Hooks…" in toast, toast
            assert_no_card(roost)

    assert jail.read_key() == "claude, codex", "the key must carry the absent agent too"
    settings = (jail.agent_dirs["claude"] / "settings.json").read_text()
    assert "ROOST_AGENT_HOOK" in settings and "agent-hook claude" in settings
    record = jail.read_record()
    assert sorted(record) == ["claude"], record
    assert not jail.agent_dirs["codex"].exists(), (
        "a codex nobody installed got a config directory"
    )


def test_no_agent_here_asks_nothing(short_root, iced_only):
    """A machine with none of the five installed is asked nothing, and
    the key stays absent.

    There is nothing to consent about — every switch would be off and
    every row "not found here" — so the card is not raised and the
    question is left open for a launch that can answer it usefully."""
    jail = Jail(short_root, agent_hooks=None, present=())

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        # The decision is made on the engine feed, one status walk after
        # the window opened, so wait for the line that records it rather
        # than for an absence.
        wait_for_log_line(
            log,
            "no coding agent is installed here",
            "the jailed UI to decline to ask about agent hooks",
        )
        with client(jail) as roost:
            assert_no_card(roost)

    assert_untouched(jail)


def test_an_answered_key_asks_nothing(short_root, iced_only):
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
        # The allow-list DOES wire, which is the other half of "already
        # answered": the startup ensure runs and the card does not.
        wait_for_log_line(
            log,
            "agent hooks toast shown",
            "the jailed UI to wire the agents its key already names",
        )
    assert allowed.read_key() == "claude"
    assert "ROOST_AGENT_HOOK" in (allowed.agent_dirs["claude"] / "settings.json").read_text()


def test_the_test_mode_fence_stops_the_card(short_root, iced_only):
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
    """`Agent Hooks…` through the command palette, the way a user reaches
    it — the action is default-unbound, so the palette row is the one
    surface it has."""
    roost.palette_dismiss()
    roost.palette_open()
    roost.palette_activate("agent_hooks")
    roost.palette_dismiss()
    return wait_for_card(roost, "preferences")


def test_preferences_reads_the_key_and_the_agents_off_disk(short_root, iced_only):
    """Opening the card re-reads `agent-hooks` **from disk**, not from
    the snapshot this process launched with.

    That is not a nicety: since plan 064 this process is not the key's
    only writer — `roostctl agent set --local` writes it with no UI
    running, and a client raises it through this machine's own session —
    so a card built from the launch-time value would show switches that
    disagree with the file it is about to overwrite. The key is changed
    underneath a *running* UI here, which is the only way to tell the two
    apart.

    Cancel is Esc's route (`App::agent_hooks_key` maps both to the same
    dismiss), and it writes nothing."""
    jail = Jail(short_root, agent_hooks=None, present=("claude", "codex"))

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with client(jail) as roost:
            card = wait_for_card(roost, "first_run")
            # Launched unanswered: both installed agents start on.
            assert [row["on"] for row in card["rows"]] == [
                True,
                True,
                False,
                False,
                False,
            ], card
            answer(roost, "cancel")

            # Somebody else answers the key while this UI runs.
            jail.write_config(agent_hooks="codex")

            card = open_preferences(roost)
            assert card["buttons"] == ["Cancel", "Apply"], card
            rows = {row["agent"]: row for row in card["rows"]}
            assert rows["claude"]["on"] is False, (
                "the card read its launch-time snapshot, not the key on disk"
            )
            assert rows["codex"]["on"] is True, rows["codex"]
            # Preferences mode shows where each agent stands. Nothing has
            # been wired yet, so the two installed ones read the same.
            assert rows["claude"]["status"] == "found, not wired", rows["claude"]
            assert rows["codex"]["status"] == "found, not wired", rows["codex"]
            assert rows["grok"]["status"] == "not found", rows["grok"]

            answer(roost, "cancel")
            assert_no_card(roost)

    assert jail.read_key() == "codex", "cancel rewrote the key"
    assert not jail.record.exists(), "cancel wired something"
    for agent in ("claude", "codex"):
        assert not any(jail.agent_dirs[agent].iterdir()), f"cancel wrote into {agent}"


def test_a_survey_does_not_steal_the_screen_from_another_dialog(short_root, iced_only):
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
    short_root, iced_only
):
    """The status line after a real wiring, and the zero case.

    Every switch off is `off`, not an empty value — an empty value parses
    back as "nobody has answered" and would bring the first-run card
    round again on the next launch — which is why the button says so
    rather than reading a bare "Apply"."""
    jail = Jail(short_root, agent_hooks="claude", present=("claude", "grok"))

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
            assert rows["claude"]["status"].startswith("wired v"), rows["claude"]
            assert rows["claude"]["on"] is True, rows["claude"]
            # Installed, and the key does not name it.
            assert rows["grok"]["status"] == "found, not wired", rows["grok"]
            assert rows["grok"]["on"] is False, rows["grok"]

            answer(roost, "toggle:claude")
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
    # Roost created this file, so an uninstall deletes it outright; a
    # file the user already had would be written back without Roost's
    # entry instead. Both are "the entry is gone", which is the claim.
    settings = jail.agent_dirs["claude"] / "settings.json"
    assert not settings.exists() or "ROOST_AGENT_HOOK" not in settings.read_text(), (
        f"turning claude off left its entry behind: {settings.read_text()}"
    )
