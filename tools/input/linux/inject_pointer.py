#!/usr/bin/env python3
"""Minimal /dev/uinput absolute-pointer injector (stdlib only).

Creates a virtual absolute pointing device mapped 1:1 to a WIDTHxHEIGHT
output, runs a scripted op sequence in ONE device session (so a
press+move+release drag stays coherent), then destroys it.

Usage:
  inject_pointer.py WIDTH HEIGHT "move X Y" "down MIDDLE" "up MIDDLE" ...
Ops: move X Y | down BTN | up BTN | sleep MS   (BTN: LEFT|MIDDLE|RIGHT)

IMPORTANT (multi-monitor): a Wayland compositor binds an absolute device to
a single output, so WIDTH/HEIGHT must be that ONE output's logical size and
X/Y are in its local coordinate space. With two monitors enabled the device
typically binds to the *primary*, so clicks aimed at a window on the other
output silently miss. Run on a single enabled output — see single_monitor.sh.
Absolute devices bypass libinput pointer acceleration, so X/Y land exactly.
"""
import fcntl
import struct
import sys
import time

from _uinput import require_uinput, uinput_unavailable


def _IOW(nr, size):
    return (1 << 30) | (ord('U') << 8) | nr | (size << 16)


def _IO(nr):
    return (ord('U') << 8) | nr


UI_SET_EVBIT = _IOW(100, 4)
UI_SET_KEYBIT = _IOW(101, 4)
UI_SET_ABSBIT = _IOW(103, 4)
UI_DEV_CREATE = _IO(1)
UI_DEV_DESTROY = _IO(2)

EV_SYN, EV_KEY, EV_ABS = 0, 1, 3
SYN_REPORT = 0
ABS_X, ABS_Y = 0, 1
BTN = {"LEFT": 0x110, "MIDDLE": 0x112, "RIGHT": 0x111}


def emit(fd, t, c, v):
    fd.write(struct.pack("llHHi", 0, 0, t, c, v))
    fd.flush()


def syn(fd):
    emit(fd, EV_SYN, SYN_REPORT, 0)


def main():
    if len(sys.argv) < 4 or sys.argv[1] in ("-h", "--help"):
        sys.exit(__doc__)
    W, H = int(sys.argv[1]), int(sys.argv[2])
    ops = sys.argv[3:]
    require_uinput()
    try:
        fd = open("/dev/uinput", "wb", buffering=0)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_SYN)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_ABS)
        for b in BTN.values():
            fcntl.ioctl(fd, UI_SET_KEYBIT, b)
        fcntl.ioctl(fd, UI_SET_ABSBIT, ABS_X)
        fcntl.ioctl(fd, UI_SET_ABSBIT, ABS_Y)
        name = b"roost-test-ptr".ljust(80, b"\x00")
        dev = name + struct.pack("HHHH", 0x03, 0x1, 0x1, 1) + struct.pack("I", 0)
        # absmax[64], absmin[64], absfuzz[64], absflat[64]  (s32 each)
        absmax = [0] * 64
        absmin = [0] * 64
        absfuzz = [0] * 64
        absflat = [0] * 64
        absmax[ABS_X] = W - 1
        absmax[ABS_Y] = H - 1
        dev += struct.pack("64i", *absmax) + struct.pack("64i", *absmin)
        dev += struct.pack("64i", *absfuzz) + struct.pack("64i", *absflat)
        fd.write(dev)
        fd.flush()
        fcntl.ioctl(fd, UI_DEV_CREATE)
    except OSError as e:
        sys.exit(uinput_unavailable(e))
    time.sleep(0.5)
    cur = [W // 2, H // 2]

    def moveto(x, y):
        cur[0], cur[1] = x, y
        emit(fd, EV_ABS, ABS_X, x)
        emit(fd, EV_ABS, ABS_Y, y)
        syn(fd)

    moveto(*cur)
    for op in ops:
        p = op.split()
        if p[0] == "move":
            x, y = int(p[1]), int(p[2])
            # glide in steps so the compositor sees motion (drag updates)
            sx, sy = cur[0], cur[1]
            for i in range(1, 11):
                moveto(sx + (x - sx) * i // 10, sy + (y - sy) * i // 10)
                time.sleep(0.01)
        elif p[0] == "down":
            emit(fd, EV_KEY, BTN[p[1]], 1)
            syn(fd)
            time.sleep(0.05)
        elif p[0] == "up":
            emit(fd, EV_KEY, BTN[p[1]], 0)
            syn(fd)
            time.sleep(0.05)
        elif p[0] == "sleep":
            time.sleep(int(p[1]) / 1000.0)
    time.sleep(0.2)
    fcntl.ioctl(fd, UI_DEV_DESTROY)
    fd.close()
    print("pointer ops done:", ops)


if __name__ == "__main__":
    main()
