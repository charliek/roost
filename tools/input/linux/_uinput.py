"""Shared `/dev/uinput` guards for the injectors (stdlib only).

`inject_key.py` and `inject_pointer.py` both open `/dev/uinput` and set it
up via ioctls; this centralizes the "is uinput actually usable here?" check
+ the friendly error, so running on macOS or in a container (no uinput)
fails with one clear line instead of a stack trace.
"""

import os
import sys

UINPUT = "/dev/uinput"

_HELP = (
    "Needs Linux with the uinput module loaded + write access "
    "(see tools/input/linux/README.md). Not available on macOS, or in a "
    "container without `--device /dev/uinput`."
)


def require_uinput():
    """Exit cleanly if `/dev/uinput` isn't present. Important: opening it
    `"wb"` would otherwise *create a regular file* when it's absent, and the
    failure would then surface later as a confusing `ioctl` ENOTTY."""
    if not os.path.exists(UINPUT):
        sys.exit(f"error: {UINPUT} not present.\n{_HELP}")


def uinput_unavailable(err):
    """The message for an OSError during open/ioctl/write (e.g. EACCES on a
    real box without the udev rule)."""
    return f"error: {UINPUT} not usable ({err}).\n{_HELP}"
