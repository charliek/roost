#!/usr/bin/env python3
"""Minimal /dev/uinput keyboard injector (stdlib only).

Usage:
  inject_key.py KEY [KEY ...]          # press KEYs together as a chord
  inject_key.py --type "some text"     # type a literal ASCII string

Emits a virtual keyboard, presses the named keys in order (releasing in
reverse) — kernel input -> libinput -> compositor -> *focused* window. Because
it follows keyboard focus rather than screen coordinates, it works correctly
on multi-monitor setups (unlike absolute-pointer injection; see README).

Key names are case-insensitive: letters A-Z, digits 0-9, and the symbolic
names in KEYS below. Modifier aliases: CTRL/ALT/SHIFT/SUPER (= the left key).
Chord example:  inject_key.py CTRL SHIFT C
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
UI_DEV_CREATE = _IO(1)
UI_DEV_DESTROY = _IO(2)

EV_SYN, EV_KEY = 0, 1
SYN_REPORT = 0

# Linux input-event-codes.h keycodes.
KEYS = {
    "A": 30, "B": 48, "C": 46, "D": 32, "E": 18, "F": 33, "G": 34, "H": 35,
    "I": 23, "J": 36, "K": 37, "L": 38, "M": 50, "N": 49, "O": 24, "P": 25,
    "Q": 16, "R": 19, "S": 31, "T": 20, "U": 22, "V": 47, "W": 17, "X": 45,
    "Y": 21, "Z": 44,
    "1": 2, "2": 3, "3": 4, "4": 5, "5": 6, "6": 7, "7": 8, "8": 9, "9": 10,
    "0": 11,
    "MINUS": 12, "EQUAL": 13, "LEFTBRACE": 26, "RIGHTBRACE": 27,
    "BACKSLASH": 43, "SEMICOLON": 39, "APOSTROPHE": 40, "GRAVE": 41,
    "COMMA": 51, "DOT": 52, "SLASH": 53, "SPACE": 57,
    "ENTER": 28, "ESC": 1, "BACKSPACE": 14, "TAB": 15,
    "UP": 103, "DOWN": 108, "LEFT": 105, "RIGHT": 106,
    "HOME": 102, "END": 107, "PAGEUP": 104, "PAGEDOWN": 109,
    "INSERT": 110, "DELETE": 111,
    "F1": 59, "F2": 60, "F3": 61, "F4": 62, "F5": 63, "F6": 64, "F7": 65,
    "F8": 66, "F9": 67, "F10": 68, "F11": 87, "F12": 88,
    "LEFTCTRL": 29, "CTRL": 29, "RIGHTCTRL": 97,
    "LEFTSHIFT": 42, "SHIFT": 42, "RIGHTSHIFT": 54,
    "LEFTALT": 56, "ALT": 56, "RIGHTALT": 100,
    "LEFTMETA": 125, "SUPER": 125, "META": 125, "RIGHTMETA": 126,
    "PLUS": 13, "EQUALS": 13,
}

# For --type: map each ASCII char to (needs_shift, key_name).
_UNSHIFTED = {
    "-": "MINUS", "=": "EQUAL", "[": "LEFTBRACE", "]": "RIGHTBRACE",
    "\\": "BACKSLASH", ";": "SEMICOLON", "'": "APOSTROPHE", "`": "GRAVE",
    ",": "COMMA", ".": "DOT", "/": "SLASH", " ": "SPACE", "\n": "ENTER",
    "\t": "TAB",
}
_SHIFTED = {
    "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7",
    "*": "8", "(": "9", ")": "0", "_": "MINUS", "+": "EQUAL", "{": "LEFTBRACE",
    "}": "RIGHTBRACE", "|": "BACKSLASH", ":": "SEMICOLON", '"': "APOSTROPHE",
    "~": "GRAVE", "<": "COMMA", ">": "DOT", "?": "SLASH",
}


def emit(fd, etype, code, value):
    # struct input_event { timeval{long,long}; u16 type; u16 code; s32 value; }
    fd.write(struct.pack("llHHi", 0, 0, etype, code, value))
    fd.flush()


def syn(fd):
    emit(fd, EV_SYN, SYN_REPORT, 0)


def char_to_chord(ch):
    """Return the list of key names to press for a single literal char."""
    if ch.isalpha():
        return (["LEFTSHIFT", ch.upper()] if ch.isupper() else [ch.upper()])
    if ch.isdigit():
        return [ch]
    if ch in _UNSHIFTED:
        return [_UNSHIFTED[ch]]
    if ch in _SHIFTED:
        return ["LEFTSHIFT", _SHIFTED[ch]]
    raise SystemExit(f"--type: unsupported character {ch!r}")


def open_device(needed_codes):
    """Create + register a uinput keyboard advertising `needed_codes`.

    Returns the open fd; the caller issues UI_DEV_DESTROY when done.
    """
    require_uinput()
    try:
        fd = open("/dev/uinput", "wb", buffering=0)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_SYN)
        for c in set(needed_codes):
            fcntl.ioctl(fd, UI_SET_KEYBIT, c)
        # legacy uinput_user_dev: char name[80]; input_id(4*u16); u32 ff; then
        # 4*64 s32 abs arrays. Pack name + bustype, zero-fill the rest.
        name = b"roost-test-kbd".ljust(80, b"\x00")
        dev = name + struct.pack("HHHH", 0x03, 0x1234, 0x5678, 1) + struct.pack("I", 0)
        dev += b"\x00" * (4 * 64 * 4)
        fd.write(dev)
        fd.flush()
        fcntl.ioctl(fd, UI_DEV_CREATE)
    except OSError as e:
        sys.exit(uinput_unavailable(e))
    time.sleep(0.4)  # let libinput/compositor register the device
    return fd


def press_chord(fd, codes):
    """Press `codes` together (in order), then release in reverse."""
    for c in codes:
        emit(fd, EV_KEY, c, 1)
        syn(fd)
        time.sleep(0.02)
    time.sleep(0.05)
    for c in reversed(codes):
        emit(fd, EV_KEY, c, 0)
        syn(fd)
        time.sleep(0.02)


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        sys.exit(__doc__)
    if args and args[0] == "--type":
        text = args[1]
        chords = [char_to_chord(ch) for ch in text]
        all_codes = [KEYS[n] for chord in chords for n in chord]
        fd = open_device(all_codes)
        for chord in chords:
            press_chord(fd, [KEYS[n] for n in chord])
            time.sleep(0.01)
        time.sleep(0.2)
        fcntl.ioctl(fd, UI_DEV_DESTROY)
        fd.close()
        print(f"typed: {text!r}")
        return

    names = [a.upper() for a in args]
    codes = [KEYS[n] for n in names]
    fd = open_device(codes)
    press_chord(fd, codes)
    time.sleep(0.2)
    fcntl.ioctl(fd, UI_DEV_DESTROY)
    fd.close()
    print("injected:", "+".join(names))


if __name__ == "__main__":
    try:
        main()
    except KeyError as e:
        sys.exit(f"error: unknown key/char {e}. Run with no args for the key list.")
