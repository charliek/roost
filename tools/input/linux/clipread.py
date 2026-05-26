#!/usr/bin/env python3
"""Read the Wayland CLIPBOARD and PRIMARY selections via Gdk4, print them.

Useful for asserting what an app published to the system selections. NOTE: on
COSMIC/Wayland a separate Gdk process has been observed to read roost's
selections as empty even when in-roost copy/paste works — so a None here does
NOT necessarily mean roost's copy failed. Prefer verifying copy/paste with an
in-roost round-trip (copy, then paste, screenshot the prompt). See the README
and the "cross-process clipboard" note.
"""
import sys

try:
    import gi
    gi.require_version('Gdk', '4.0')
    gi.require_version('Gtk', '4.0')
    from gi.repository import Gdk, Gtk, GLib  # noqa: E402
except (ImportError, ValueError) as e:
    sys.exit(
        f"error: PyGObject (gi) with GTK4 typelibs not available ({e}).\n"
        "clipread.py needs a Linux desktop session with PyGObject + GTK4 "
        "(see tools/input/linux/README.md); it can't run on macOS or headless."
    )

Gtk.init()
disp = Gdk.Display.get_default() or Gdk.Display.open(None)
if disp is None:
    print("no display")
    raise SystemExit(1)
loop = GLib.MainLoop()
results = {}
pending = {"n": 0}


def make_cb(name, clip):
    def cb(c, res):
        try:
            results[name] = clip.read_text_finish(res)
        except Exception as e:
            results[name] = f"<err: {e}>"
        pending["n"] -= 1
        if pending["n"] == 0:
            loop.quit()
    return cb


cb_clip = disp.get_clipboard()
pr_clip = disp.get_primary_clipboard()
pending["n"] = 2
cb_clip.read_text_async(None, make_cb("CLIPBOARD", cb_clip))
pr_clip.read_text_async(None, make_cb("PRIMARY", pr_clip))
GLib.timeout_add(2500, lambda: loop.quit())
loop.run()
print("CLIPBOARD:", repr(results.get("CLIPBOARD")))
print("PRIMARY:  ", repr(results.get("PRIMARY")))
