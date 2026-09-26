# User messaging

The Linux UI has six places to tell the user something. Each one carries
one kind of message, so a new message has an obvious home and two surfaces
never say the same thing in different words.

| Surface | Carries | Lifetime | Tone | Read over IPC |
|---|---|---|---|---|
| **Toast** (bottom right) | The outcome of something you just did: a refusal, an error, a receipt | 5 s | `error` or `info` | [`app.notice_dump`](../reference/ipc.md#appnotice_dump) `bottom_line`, `source: "status"` |
| **Bottom line** (the same slot) | A standing condition of the workspace: the last save failed | Until it clears; a toast covers it while up | `error` | `app.notice_dump` `bottom_line`, `source: "durability"` |
| **Terminal notice** | A condition that takes over the terminal area and has actions: the session under this frame ended | While the condition holds | `info`, `warning` or `error` | `app.notice_dump` `terminal` |
| **Sidebar band** | A host's connection state, and a short reason | While the state holds | The band's dot | [`app.sidebar_dump`](../reference/ipc.md#appsidebar_dump) `sections`, and [`host.status`](../reference/ipc.md#host-registry-host) for the full reason |
| **Dialog** | A decision that must be made now | Until answered | — | [`app.dialog_dump`](../reference/ipc.md#host-bootstrap-test-ops-appdialog_dump-appdialog_answer-appkeybind_dispatch-test-only-gated) (test mode) |
| **Desktop notification** | An agent event: a tab wants attention | The desktop's | — | The [`notification.fired`](../reference/ipc.md#events) event |

A terminal notice draws in one of two places. **Over a frame**, it sits
at the top of a frame the window keeps on screen, under a scrim, because
nothing will update those pixels again. **In an empty area**, it sits at
the top of a terminal area with nothing selected, with no scrim.

## Three rules

1. **Content comes from a pure table.** Which message, which words and
   which buttons are decided by a pure function over plain inputs, with a
   table test over every state it can be given. The terminal notice is
   `roost_ui_model::notice::terminal_notice`; the band is
   `host_sidebar::SectionState`; the dialogs are `host_notice` and
   `host_dialog`. The widgets paint the answer and add nothing of their
   own. The toast is the exception today: its call sites compose their
   own strings.
2. **Every surface is dumpable over IPC.** The dump reports the rendered
   strings, the ones the widgets read, not the state behind them. A test
   then asserts what the user is told, rather than re-deriving the copy
   rule a second time.
3. **A click names what it was drawn on.** A button carries the identity
   of the thing it was drawn for, and the handler checks it against what
   is on screen **now** before acting. A press can land after the window
   moved on — a second click, or a click on pixels not yet repainted — and
   acting on whatever replaced it is how a stale click aborts a reconnect
   or starts a second session. Over IPC, `app.notice_answer` also names
   the notice's `generation`, so a notice that went away and came back is
   not mistaken for the one a test read.
