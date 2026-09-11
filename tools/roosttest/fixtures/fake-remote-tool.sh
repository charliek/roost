#!/bin/sh
# A stand-in for whatever the far side runs in `bootstrap_test.rs` — the
# fake `uname`, the tool overrides (`tee`, `chmod`) that replace a
# coreutil in the jail's `PATH`, and the planted `roost-session` the
# probe's ladder finds.
#
# It has no behaviour of its own. A test symlinks this file to the name
# the far side will exec — `<stub-bin>/uname`, `$HOME/.local/bin/
# roost-session` — and writes what that name should do to `<that
# path>.conf`, which is sourced here. The split is about how the tests
# run, not what they test: the Rust harness runs a binary's cases in
# parallel threads, and a script written by one thread while another
# thread is forking races `execve` — the forked child inherits the
# writer's still-open descriptor until its own exec closes it, and an
# `execve` of that file in the meantime answers ETXTBSY ("Text file
# busy") on Linux (#434, same class as #431 and #408). Per-test paths do
# not help; another test's fork is what breaks a file that is private to
# this one. Sourcing has no such check — `.` reads the file, it does not
# exec it — so nothing the tests exec is ever written by the test
# process, and each body still sits beside the case that needs it.
#
# **Unlike `fake-roost-session.sh`, this fixture does not pin `PATH`.**
# That sibling pins `PATH=/usr/bin:/bin` because `roost-cli`'s doctor
# tests point the process-global `PATH` at an empty directory. Here the
# `PATH` in force *is* the thing under test: the far side runs under
# `env -i` with `PATH` set to the harness's jail, a directory holding a
# handful of symlinked coreutils and deliberately not the developer's
# environment. Widening it would break the hermeticity contract these
# tests exist to enforce.
#
# `$0.conf` is sourced before anything else, and this file never `cd`s:
# macOS `/bin/sh` is bash 3.2 in POSIX mode, whose `.` PATH-searches a
# *relative* operand. `$0` is absolute here only because every planted
# path and the jail's `PATH` are absolute — keep it that way.

if [ ! -f "$0.conf" ]; then
    printf '%s\n' "fake-remote-tool: nothing to do — no $0.conf beside the symlink" >&2
    exit 64
fi
# shellcheck disable=SC1090
. "$0.conf"
