# Linux UI test harness

Tooling to drive and verify the `roost-linux` GTK app in an automated way —
launch it, send input as a real user would, and read back what's on screen —
without Pillow/ImageMagick/`wtype`/`ydotool` installed. Built up while
verifying clipboard/reflow changes; intended to grow as UI testing becomes a
routine task.

Two input paths, two very different properties:

- **Keyboard** (`inject_key.py`) follows **focus**, not coordinates — reliable
  on any monitor layout. Use it for everything you can (keybinds, typing).
- **Pointer** (`inject_pointer.py`) is an **absolute** device bound to a single
  output, so it needs a known on-screen geometry and a single enabled monitor
  (see [Single monitor](#single-monitor-required-for-the-pointer)).

Everything is stdlib Python 3 + bash. No build step.

## Prerequisites

- COSMIC desktop on Wayland (uses `cosmic-screenshot` / `cosmic-randr`). The
  injectors are compositor-agnostic; the screenshot + display helpers are not.
- `/dev/uinput` writable by your user (`ls -l /dev/uinput`). If not, add a udev
  rule or run the injectors via a wrapper with access.
- `roostctl` built (`cargo build -p roost-cli --bin roostctl`).
- `clipread.py` only: PyGObject (`gi`) with GTK4 typelibs.

## The tools

| Tool | What it does |
|------|--------------|
| `inject_key.py` | Press a key chord (`CTRL SHIFT C`) or type a string (`--type "ls\n"`). Follows keyboard focus. |
| `inject_pointer.py` | Absolute pointer: `move X Y`, `down/up LEFT\|MIDDLE\|RIGHT`, drags. Needs single monitor. |
| `pngtool.py` | Decode/inspect a PNG with no image libs: `info`, `pixel`, `hscan`/`vscan`, `textscan` (find text rows), `findcolor` (locate a UI element), `crop` (write a focused region). |
| `clipread.py` | Print the CLIPBOARD + PRIMARY selections via Gdk4 (see the caveat below). |
| `single_monitor.sh` | `solo <OUTPUT>` / `restore` — collapse to one enabled output for pointer work, then put the others back. |

Run any Python tool with `python3 tools/linux/<tool>.py ...`; `--help`/no-args
prints usage.

## Seeing the UI: in-process screenshot

`roostctl screenshot --out /tmp/x.png` renders the window **in-process**, so it
works even when the window is occluded or unfocused — this is the source of
truth for "what does roost think is on screen," independent of stacking/focus.
Then inspect it with `pngtool.py`. (`cosmic-screenshot` captures the whole
*physical* screen and is only needed to find a window's on-screen position for
pointer injection.)

## Single monitor (required for the pointer)

A Wayland compositor binds a uinput absolute device to **one** output (usually
the primary). With two monitors enabled, pointer events aimed at a window on
the *other* output silently miss (clicks land on the bound output instead).
Symptom: drags produce no selection and the target window loses focus.

```sh
tools/linux/single_monitor.sh status          # see what's enabled
tools/linux/single_monitor.sh solo eDP-1       # disable the others
# ... run pointer-based tests ...
tools/linux/single_monitor.sh restore          # bring them back
```

## Mapping screen ↔ window coordinates

`roostctl screenshot` gives window-local pixels; `inject_pointer.py` needs
*screen* pixels. With roost maximized on a single output the window sits just
below the compositor's top panel, so:

```
screen_x = window_x
screen_y = window_y + PANEL_H     # PANEL_H ≈ screen_height − window_height
```

Find `PANEL_H` from `pngtool.py info` on both a full-screen `cosmic-screenshot`
and a `roostctl screenshot` (e.g. 1050 − 1018 = 32). Pass the **output's**
logical width/height to `inject_pointer.py` (the `cosmic-screenshot` PNG size).

To locate a target cell, screenshot the window and use `pngtool.py textscan` to
find a text row's `y` and `x`-range; cell width ≈ row-width ÷ char-count, cell
height ≈ spacing between rows.

## Gotchas learned the hard way

- **Sidebar GtkPaned handle.** It sits at the terminal's left edge. A
  mouse-*down* there grabs the divider and resizes the sidebar instead of
  selecting. To select text starting at column 0, drag **right-to-left**:
  mouse-down in the terminal interior, drag left toward the edge.
- **IPC input doesn't clear a selection.** `roostctl tab send` writes straight
  to the PTY; the selection highlight is a UI overlay cleared only by a real
  keypress or a new drag. Don't expect a sent command to drop the highlight.
- **Cursor as a focus probe.** A filled cursor = focused; hollow/absent =
  unfocused. Handy to confirm a click landed in the window.

## Worked example: clipboard copy round-trip

```sh
AT=$(roostctl identify | sed -n 's/active_tab=//p')
roostctl tab send --tab "$AT" --bytes 'clear\n'
roostctl tab send --tab "$AT" --bytes 'echo MARKER_0123456789_END\n'

# locate the printed MARKER row in window pixels, add PANEL_H for screen y,
# then drag right-to-left across it (single monitor, output is 1680x1050):
python3 tools/linux/inject_pointer.py 1680 1050 "move 560 159" "down LEFT" "move 224 159" "up LEFT"

python3 tools/linux/inject_key.py ALT C     # copy   (Linux: clipboard mod = Alt)
python3 tools/linux/inject_key.py ALT V     # paste
roostctl screenshot --out /tmp/after.png    # MARKER appears at the prompt
python3 tools/linux/pngtool.py crop /tmp/after.png /tmp/after_top.png 0 0 1680 205
```

## Caveat: cross-process clipboard on COSMIC

On COSMIC/Wayland, a separate Gdk reader (`clipread.py`) has been observed to
read roost's CLIPBOARD/PRIMARY as empty even when in-roost copy/paste works.
So a `None` from `clipread.py` does **not** prove roost's copy failed — verify
with an in-roost round-trip (copy, then paste, screenshot the prompt) instead.
This is an open question about GTK clipboard propagation on COSMIC, not a
harness bug.
