#!/usr/bin/env python3
"""Inject vertical wheel detents through a short-lived uinput mouse.

Usage: inject_wheel.py STEPS [DELAY_MS]

Positive STEPS scrolls up; negative scrolls down. Position the shared seat
cursor first (for example with inject_pointer.py). A separate relative mouse is
intentional: libinput can discard wheel axes exposed by an absolute-only test
pointer, while this conventional REL_X/REL_Y mouse follows the same path as a
physical wheel. Both legacy and high-resolution axes are emitted.
"""

import fcntl
import struct
import sys
import time

from _uinput import require_uinput, uinput_unavailable


def _IOW(nr, size):
    return (1 << 30) | (ord("U") << 8) | nr | (size << 16)


def _IO(nr):
    return (ord("U") << 8) | nr


UI_SET_EVBIT = _IOW(100, 4)
UI_SET_KEYBIT = _IOW(101, 4)
UI_SET_RELBIT = _IOW(102, 4)
UI_SET_PROPBIT = _IOW(110, 4)
UI_DEV_CREATE = _IO(1)
UI_DEV_DESTROY = _IO(2)

EV_SYN, EV_KEY, EV_REL = 0, 1, 2
SYN_REPORT = 0
BTN_LEFT = 0x110
REL_X, REL_Y, REL_WHEEL, REL_WHEEL_HI_RES = 0, 1, 8, 11
INPUT_PROP_POINTER = 0


def emit(fd, event_type, code, value):
    fd.write(struct.pack("llHHi", 0, 0, event_type, code, value))
    fd.flush()


def main():
    if len(sys.argv) not in (2, 3) or sys.argv[1] in ("-h", "--help"):
        sys.exit(__doc__)
    steps = int(sys.argv[1])
    delay_ms = int(sys.argv[2]) if len(sys.argv) == 3 else 0
    if steps == 0:
        return
    require_uinput()
    try:
        fd = open("/dev/uinput", "wb", buffering=0)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_SYN)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_REL)
        fcntl.ioctl(fd, UI_SET_KEYBIT, BTN_LEFT)
        fcntl.ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER)
        for axis in (REL_X, REL_Y, REL_WHEEL, REL_WHEEL_HI_RES):
            fcntl.ioctl(fd, UI_SET_RELBIT, axis)
        name = b"roost-test-wheel".ljust(80, b"\x00")
        device = name + struct.pack("HHHH", 0x03, 0x1, 0x2, 1) + struct.pack("I", 0)
        device += bytes(64 * 4 * 4)
        fd.write(device)
        fd.flush()
        fcntl.ioctl(fd, UI_DEV_CREATE)
    except OSError as error:
        sys.exit(uinput_unavailable(error))
    time.sleep(0.5 + delay_ms / 1000.0)
    # A newly-added relative device has no surface focus of its own, and some
    # wlroots/libinput combinations clear focus when a short-lived absolute
    # device disappears. Establish focus with this same device: clamp at the
    # fullscreen cage output's top-left, then move well into Roost's large
    # terminal pane before emitting the axis. Raw relative acceleration may
    # vary, but the destination has hundreds of points of safe margin.
    emit(fd, EV_REL, REL_X, -10_000)
    emit(fd, EV_REL, REL_Y, -10_000)
    emit(fd, EV_SYN, SYN_REPORT, 0)
    time.sleep(0.05)
    emit(fd, EV_REL, REL_X, 400)
    emit(fd, EV_REL, REL_Y, 120)
    emit(fd, EV_SYN, SYN_REPORT, 0)
    time.sleep(0.05)
    emit(fd, EV_REL, REL_WHEEL, steps)
    emit(fd, EV_REL, REL_WHEEL_HI_RES, steps * 120)
    emit(fd, EV_SYN, SYN_REPORT, 0)
    time.sleep(0.2)
    fcntl.ioctl(fd, UI_DEV_DESTROY)
    fd.close()
    print(f"wheel detents injected: {steps}")


if __name__ == "__main__":
    main()
