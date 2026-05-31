# PR 2 plan — issue #197: zsh + brew bash on CI runners

**Branch:** `test/ci-shell-provisioning`
**Closes:** [#197](https://github.com/charliek/roost/issues/197)

## Problem

The two auto-bootstrap shell-integration tests (`test_zsh_auto_bootstrap_tracks_cwd`, `test_bash_auto_bootstrap_tracks_cwd`) are skipped on CI because:
1. `e2e-gtk` doesn't `apt install zsh` (and a prior attempt to do so surfaced runner-image quirks — `compinit insecure directories` + first-keystroke garbling, see #197).
2. `e2e-mac` doesn't `brew install bash` (Apple `/bin/bash` is 3.2, no auto-bootstrap path).

Net: the **auto-bootstrap** path (`ZDOTDIR` shim for zsh, `--posix` + `ENV` for modern bash) — the "no rc edit needed" claim — is only exercised locally on developer boxes. A regression on `roost.zsh`/`roost.bash` parity-with-Ghostty could ship green.

## Two root causes (per #197 + code re-read)

1. **`compinit insecure directories` on GH runner zsh.** Stock `ubuntu:24.04` (Docker) doesn't repro; `actions/runner-images` ships a non-stock zsh config in `/etc/zsh/zshrc` that triggers `compinit` against world-writable completion dirs. Cosmetic, but widens the boot window.
2. **Input garble — first keystroke of `cd /usr` eaten.** The harness `run()` checks "viewport non-empty" (`client.py:163-173`) as its readiness signal. `compinit`'s "insecure directories…" line makes the viewport non-empty *before* zle is ready → `c` of `cd` dropped → `d /usr` reaches shell → `command not found: d`. The check is fine for `--norc bash` (prompt is the first content); breaks for any shell that prints pre-prompt content.

## Plan

Phase 0 discovery is collapsed into a draft-PR poke: the CI run **is** the discovery. Spinning up `catthehacker/ubuntu:act-full-24.04` locally is high-cost (~4 GB image pull + DOCKER_HOST setup) for a fidelity that the actual GH runner gives us directly in ~2 minutes.

### Phase 1 — harness hardening

After `/simplify`'s review surfaced four blocker-class findings (echoed-command false-positive, wrong exception type, stale-sentinel false-positive across retries, viewport-permanent-non-empty defeating subsequent `run()`), the helper design is revised below.

Add to `tools/roosttest/util.py` (and hoist `import uuid`, `from client import Timeout` to the module top):

```python
def wait_shell_ready(
    roost,
    tab_id: int,
    *,
    sentinel_attempts: int = 10,
    per_attempt_timeout: float = 2.0,
    total_timeout: float = 20.0,
) -> None:
    """Wait until the tab's shell can run a command and produce
    output — i.e., its line editor is initialized AND its prompt has
    redrawn past any startup banner (compinit, MOTD, /etc/zshrc
    motd-magic, `--posix` recreation, …).

    Robust against shells that emit pre-prompt output: the harness's
    default 'viewport non-empty' check (`roost.run`) races those,
    dropping the first keystroke into a half-initialized line editor.

    Each attempt sends `printf 'ROOST_READY_%s\\n' '<freshUuid>'`.
    The %s/$VAR pattern is load-bearing: the shell ECHOES typed
    commands verbatim onto the prompt line, so a literal sentinel
    inside single-quotes would match wait_text via the echo before
    the shell ever runs the command. With `%s` + a separate VALUE
    arg, the echo shows the literal `%s` while the printf OUTPUT
    contains the resolved value — only present when the command
    actually executes.

    Per attempt the sentinel suffix is fresh, so a partial echo from
    a prior attempt can't false-positive a later one.

    By the time the helper returns, the shell HAS executed printf
    and produced output — i.e. it was demonstrably interactable, so
    the race `run()`'s viewport-non-empty check defends against
    (writes-while-zle-uninitialized) is already past. The lingering
    sentinel echo is harmless to subsequent `roost.run` calls.

    Bounded by `sentinel_attempts` outer iterations; each per-attempt
    `wait_text` call is itself scaled by ROOST_TEST_TIMEOUT_SCALE
    inside `_wait`, so the outer total is a soft cap (the last
    iteration may overrun the outer deadline by one scaled
    `per_attempt_timeout`). On total exhaustion, raises `client.Timeout`
    with a viewport dump — never hangs.

    `suffix` must be shell-safe (no single quotes, no metachars) since
    it's passed as a positional printf arg inside single quotes. The
    default uuid4().hex is `[0-9a-f]` — safe.
    """
    deadline = time.monotonic() + scaled_timeout(total_timeout)
    last_sentinel = ""
    for _ in range(sentinel_attempts):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        # Fresh sentinel per attempt so a partial echo from a prior
        # iteration can't false-positive this one.
        suffix = uuid.uuid4().hex
        last_sentinel = f"ROOST_READY_{suffix}"
        # Output-only marker: echo shows `%s`, only stdout has the suffix.
        roost.send(tab_id, f"printf 'ROOST_READY_%s\\n' '{suffix}'\n")
        try:
            roost.wait_text(
                tab_id, last_sentinel,
                timeout=min(per_attempt_timeout, max(0.3, remaining)),
            )
            return
        except Timeout:
            continue
    try:
        tail = roost._safe_dump_text(tab_id)
    except Exception:
        tail = "<dump unavailable>"
    raise Timeout(
        f"shell never echoed printf output (last sentinel={last_sentinel!r}) "
        f"within {sentinel_attempts} attempts / {total_timeout}s (scaled). "
        f"Viewport tail:\n{tail}"
    )
```

Design notes:
1. **Output-only sentinel**: `printf 'ROOST_READY_%s\n' '<uuid>'` — echo shows `printf 'ROOST_READY_%s\n' '<uuid>'` (with the literal `%s`), so `wait_text` only matches `ROOST_READY_<uuid>` when the shell actually executes the printf. Mirrors the in-tree pattern documented in `test_shell_integration.py:13-18`.
2. **Fresh sentinel per attempt**: prevents a stale partial echo from a prior attempt false-positiving the next one.
3. **`except Timeout`**: imports from `client` so the actual exception (`client.Timeout`, a `RoostError`) is caught.
4. **Raises `Timeout`** (not bare `TimeoutError`): consistent with `roost.wait_text` and `_wait`.
5. **Per-attempt floor of 0.3s**: `_wait`'s default poll interval is 100ms, so 0.3s gives 2-3 cycles — meaningful budget.
6. **No double scaling**: `wait_text` → `_wait` already calls `scaled_timeout`. The helper scales only the outer deadline; it passes per-attempt unscaled, letting `_wait` apply scale once.

**Call-site changes**:

- `test_zsh_auto_bootstrap_tracks_cwd` (test_shell_integration.py:343): insert `wait_shell_ready(roost, tab)` right after the existing `wait_tab_attached(roost, tab)` on line 354.
- `test_bash_auto_bootstrap_tracks_cwd` (line 399): insert **both** `wait_tab_attached(roost, tab)` **and** `wait_shell_ready(roost, tab)` after `open_tab` (the test currently has neither; the zsh test has only attach — the parity fix is to add both). The attach-wait protects against the `roost.send` inside `wait_shell_ready` firing before the TerminalView is wired.
- `test_default_shell_login_matches_platform` (test_shell_integration.py:45): insert `wait_shell_ready(roost, tab)` after `open_tab` on Mac (where the default shell is integrated zsh against /etc/zshrc), since the same pre-prompt race applies. This addresses /simplify's finding #8 (Mac default-shell zsh race). On Linux the default is `/bin/bash` with the auto-bootstrap inject path, which also benefits — call it for both targets unconditionally; cheap.

**Subsequent `roost.run` interaction**: By design the helper does NOT clear the viewport before returning. Rationale: the shell has provably executed printf and emitted output — that's what `wait_text` matched — so the race the viewport-non-empty check defends against (writes into an uninitialized line editor) is already past. The lingering sentinel echo is harmless to subsequent `roost.run` calls. (Codex 2nd-pass review flagged an earlier draft of the docstring that contradicted this design.)

**`test_default_shell_login_matches_platform` insertion** (test_shell_integration.py:45): for parity with the other call sites, insert BOTH `wait_tab_attached(roost, tab)` AND `wait_shell_ready(roost, tab)` after `open_tab` (the test currently has neither). Skip on `ROOST_LOGIN_RESULT:skip` still applies for non-bash/zsh shells; `printf` is POSIX so the sentinel works there too. The Mac default `$SHELL` could be `/bin/sh` instead of zsh under some harness env setups (per Codex finding #7) — but the test already gates on `ROOST_LOGIN_RESULT:skip` for non-bash/zsh, and `printf` is POSIX-portable, so `wait_shell_ready` is safe to add unconditionally.

### Phase 2 — CI provisioning

In `.github/workflows/ci.yml`:

- e2e-gtk "Install GTK4 + libadwaita + Xvfb" step: **append ` zsh`** to the existing `sudo apt-get install -y` arg list (preserve the file's 12-space continuation indent). The step already runs `sudo apt-get update` so no separate update step is needed.
- e2e-mac: **new step** adjacent to setup-uv (right after the `Install uv` step at `ci.yml:389`, to parallel apt-get-install's slot on Linux):

```yaml
- name: Install modern bash + version probe
  run: |
    brew list bash >/dev/null 2>&1 || brew install bash
    # Print the resolved path + version so a future runner-image
    # regression (no preinstall + no install) is loud in CI logs
    # rather than silently re-triggering `pytest.skip("no modern bash")`.
    # Check both Homebrew prefixes — macos-latest is ARM today but
    # may flip back to Intel in the future, and _modern_bash()
    # probes both `/opt/homebrew/bin/bash` and `/usr/local/bin/bash`.
    which bash || true
    for p in /opt/homebrew/bin/bash /usr/local/bin/bash; do
      if [ -x "$p" ]; then "$p" --version; fi
    done
```

The `brew list || brew install` idiom is idempotent against runner-image preinstalls. The explicit version print catches the "Phase 4 coverage is hollow" risk: if a future runner image drops bash 5.x, the version probe will show that and the test's `_modern_bash()` probe will pytest.skip — loudly visible in CI logs.

No new jobs.

### Phase 3 — `compinit` quieting (only if Phase 0 shows it's still needed)

`.github/workflows/ci.yml` e2e-gtk, add a step before the test run:

```yaml
- name: Quiet zsh compinit (CI runner /usr/share/zsh perms)
  run: |
    sudo chmod -R go-w /usr/share/zsh /usr/local/share/zsh 2>/dev/null || true
```

Idempotent. Only added if the helper alone doesn't solve the race.

### Phase 4 — validate Mac brew-bash + tighten skip semantics + update README

- Replace the two test bodies' `pytest.skip("zsh not available" / "no modern bash")` with `precondition(...)` (the helper already in `util.py` from PR #194's WS4 — fails loudly in `--roost-fresh` / CI mode, skips on dev hosts). This addresses Codex's finding #8: a missed CI install today would re-trigger the skip and the job goes green; with `precondition` on CI, a missed install hard-fails. On dev hosts the skip still fires correctly.
- Update `tools/roosttest/README.md`'s skip-policy section (around line 109-113) — the existing text "zsh / modern bash are a CI-provisioning gap tracked in issues" needs to be reframed post-fix to "capability skips remain for dev hosts without the tool installed; CI runners are provisioned with both, and the tests use `precondition` (hard-fail) in fresh mode. The `wait_shell_ready` helper in `util.py` is the canonical pre-input pattern for any test that spawns a non-bare shell." Otherwise the stale doc will surface as a false bug in the next round.
- Document in the new tests' docstrings that the helper is the recommended pre-input pattern for any test spawning an integrated shell.

## Complexity gate (per user instruction)

The helper's bail signal is "10 attempts exhausted" — it doesn't distinguish 3-dropped-keystrokes from 10-dropped-keystrokes; it just retries until either success or the outer deadline. The loud-failure path raises `client.Timeout` with a viewport dump, so a CI failure is diagnosable without rerunning.

**Bail-and-discuss triggers** (any of):
- The helper exhausts all 10 attempts on the GH runner zsh — meaning zle is dropping bytes for longer than the budget. Expanding to OSC 133 A wait would add ~1 day and require touching `roost.zsh`'s deferred-load contract.
- The Mac brew-bash test fails for an orthogonal reason (HISTFILE path, `--posix` interaction, broken `_modern_bash` probe).
- A previously-passing CI test goes red because of the `test_default_shell_login_matches_platform` addition.

If the sentinel works but compinit still warns visibly, apply Phase 3.

## Verification

- Local: existing GTK e2e suite continues to pass (sanity).
- Draft / regular CI run: `test_zsh_auto_bootstrap_tracks_cwd` and `test_bash_auto_bootstrap_tracks_cwd` pass on CI without skips.
- The suite's `SKIPS:` summary no longer lists `"zsh not available"` / `"no modern bash"` after CI provisioning lands.

## Risks

- **Sentinel collision**: vanishingly unlikely (uuid suffix); the helper's failure mode is a false-positive readiness → loud failure in the subsequent test with viewport dump (a useful signal).
- **GH runner zsh might be more pathological than `wait_shell_ready` handles** — the complexity gate above catches this.

## PR checklist

- [ ] `wait_shell_ready` added to `tools/roosttest/util.py` with the revised design (output-only sentinel via `printf 'X:%s\n' VALUE`, fresh sentinel per attempt, `from client import Timeout`, raises `client.Timeout` not bare `TimeoutError`, 0.3s per-attempt floor, single-scaled timeout).
- [ ] `import uuid` hoisted to module top of util.py (matching `os`, `re`, `time` convention).
- [ ] `from client import Timeout` added to util.py imports.
- [ ] Inserted in `test_zsh_auto_bootstrap_tracks_cwd` (after `wait_tab_attached`).
- [ ] Inserted in `test_bash_auto_bootstrap_tracks_cwd` (BOTH `wait_tab_attached` AND `wait_shell_ready`, the test currently has neither).
- [ ] Inserted in `test_default_shell_login_matches_platform` (BOTH `wait_tab_attached` + `wait_shell_ready` after `open_tab`; covers Mac default-zsh /etc/zshrc race).
- [ ] e2e-gtk: append ` zsh` to the existing `sudo apt-get install` arg list (preserve 12-space continuation indent).
- [ ] e2e-mac: new step `brew list bash || brew install bash` + version probe, slotted adjacent to setup-uv.
- [ ] CI run confirms both `_auto_bootstrap_tracks_cwd` tests now run + pass (not skipped); SKIPS: summary no longer lists `"zsh not available"` / `"no modern bash"`.
- [ ] Existing `pytest.skip` calls for missing zsh / modern bash replaced with `util.precondition(...)` so a missed CI install hard-fails (Codex finding #8).
- [ ] `tools/roosttest/README.md` skip-policy section updated to reflect that the CI-provisioning gap is closed.
- [ ] `cargo fmt` not applicable (pure Python + YAML).
