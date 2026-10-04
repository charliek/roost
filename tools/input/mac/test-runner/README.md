# Roost Test Runner — the Mac real-input harness's TCC anchor

macOS grants Accessibility, Input Monitoring and Screen Recording to an app's
**code identity**. An ad-hoc signature (all a local build gets) changes on
every build, so a helper that is rebuilt with the code it tests loses its
grants every time. Roost Test Runner is the fix: a tiny app that is built once,
granted once, and **never rebuilt**. The harness runs its freely rebuilt helper
(`../roost-input-mac`) as the runner's child, and TCC attributes a child to its
responsible app.

| File | What it is |
|---|---|
| `runner.c` | The app's executable: spawns `<cmd> [args…]`, writes its stdout, stderr and exit status into `<outdir>` |
| `Info.plist` | `ai.stridelabs.roost.test-runner`, `LSUIElement` (no Dock icon, never takes focus) |
| `tcc-probe.c` | Reports the four grants; with `request`, asks for them (the one-time setup below) |
| `build.sh` | Builds and installs both, and **refuses to replace an installed runner** |

## The committed sources are provenance only

`runner.c`, `tcc-probe.c` and `Info.plist` are byte-identical to the sources of
the runner installed on the harness Mac. They record what that binary is; they
cannot recreate it. Rebuilding — even from these exact bytes — produces a new
ad-hoc signature, a new CDHash, and an app TCC has never seen. **Rebuilding the
runner voids every grant**, which is why `build.sh` refuses, with no override,
to run over an installed copy. Replacing a copy that is already broken is a
deliberate manual step: move the old app away yourself, run `build.sh`, and
grant the new one again (below).

## One-time setup on a new Mac

1. `tools/input/mac/test-runner/build.sh` — installs
   `~/Applications/Roost Test Runner.app` and `~/roost-harness/tcc-probe`.
2. Ask for the grants once, through the runner, from a GUI login session:

   ```bash
   mkdir -p /tmp/rtr
   open -W -n "$HOME/Applications/Roost Test Runner.app" \
     --args /tmp/rtr "$HOME/roost-harness/tcc-probe" request
   ```

   Then in **System Settings → Privacy & Security** turn on *Roost Test
   Runner* under **Accessibility**, **Input Monitoring** and **Screen & System
   Audio Recording** (Accessibility also covers posting events).
3. Verify — every value must be `1`:

   ```bash
   open -g -W -n "$HOME/Applications/Roost Test Runner.app" \
     --args /tmp/rtr "$HOME/roost-harness/tcc-probe"
   cat /tmp/rtr/stdout   # accessibility=1 post_event=1 screen_capture=1 input_monitoring=1
   ```

The harness itself never asks for a grant: `roost-input-mac preflight` uses the
read-only checks only, so a missing grant is a clear skip (or a failure under
`ROOST_REQUIRE_REAL_INPUT=1`), never a permission dialog in the middle of a run.

## How the harness uses it

`tools/input/mac/runner.py` (runner mode, the default) launches

```bash
open -g -n "$HOME/Applications/Roost Test Runner.app" \
  --args <outdir> <abs path to roost-input-mac> --outdir <outdir> <command> …
```

- `-g` keeps the runner in the background, so it never takes focus from the
  Roost window under test; `-n` gives every invocation its own runner.
- The runner inherits the LaunchServices environment, not the caller's, so every
  path is absolute and nothing is passed through the environment.
- The helper writes `<outdir>/pid` first and journals what it holds in
  `<outdir>/held.json`; the runner writes `<outdir>/status` when the helper
  exits. On a timeout the wrapper stops that pid (SIGTERM, then SIGKILL,
  proving before each signal that the pid is still that helper); after any
  unsuccessful outcome a fresh `roost-input-mac release-held --file
  <outdir>/held.json` releases exactly what the journal still lists. A journal
  that cannot be updated after a release is removed rather than left stale, so
  a failing disk leaves the helper's own press down instead of releasing a key
  someone else may be holding by then. Nothing automated runs
  `key --release-all`, which would also release what the person at the desk is
  holding: it is for running by hand.

CI does not use the runner: GitHub's hosted macOS runners already grant those
capabilities to the job's own processes, so CI runs the helper directly
(`ROOST_REAL_INPUT_MODE=direct`).

## Known limits

The helper refuses whatever it can tell would go astray. These are the cases
it cannot tell, accepted as they stand:

- **A foreign key panel in an app that does not answer Accessibility.** A key
  goes to whichever window is key, and a non-activating panel can take the
  keyboard while Roost stays the front process. Before each key the helper asks
  every app with a window on screen whether its focused window is key, and
  refuses if any app but Roost says yes. An app with no Accessibility, an AX
  error, or no answer in time counts as saying no, so its panel is missed and
  the key reaches it. The answers are not one snapshot either: an app already
  asked can take the keyboard while later ones are asked. The 0.25 s
  `FOREIGN_TIMEOUT` is set on each app's application element only; the
  `AXFocused` read goes to that app's window element, which waits for the
  process-wide timeout instead (the system default, or 2 s once an earlier read
  in the same command has set it), so one app that stops answering delays the
  key by that much.
- **Secure Input is observed, not resolved.** Secure Input hides keystrokes from
  event taps; it never redirects them. The helper refuses to post while it is
  on unless the caller passes `allow_secure_input`, and that flag cannot tell
  Roost's own Secure Input from a foreign holder's: `kCGSSessionSecureInputPID`
  names the frontmost app, not the process that turned Secure Input on. Pass it
  only once Roost (`app.secure_input`) says it holds Secure Input, knowing that
  a background holder elsewhere passes too.
- **A filesystem stall between the deciding check and the post.** A press is
  journaled after its checks and before it is posted, so a recovery never
  misses it. That write is a synchronous local write and rename with no time
  bound: if it stalls, the checks are that much older when the press lands, and
  another window can take the point or the keyboard in between.
- **Recovery beside a helper in an unknown state.** When the wrapper cannot
  prove that a timed-out helper is still its own (`ps` gives no answer, or
  there is no pid file), it signals nothing and still releases what the journal
  lists; the helper may still be running and post after that release. The run
  fails loudly — `cleanup NOT confirmed: the helper may still be running` —
  rather than pass.
