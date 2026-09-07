#!/bin/sh
# A stand-in `roost-session` for the launcher tests — the seven lifecycle
# cases in `crates/roost-cli/src/session.rs` and the state-dir seam in
# `crates/roost-ipc/tests/session_launch_state_dir_test.rs`.
#
# It has no behaviour of its own. A test symlinks this file to
# `<dir>/roost-session` and writes what the launcher should do — print a
# verdict, hang, close stdout, flood, say nothing — to
# `<dir>/roost-session.conf`, which is sourced here. The split is about
# how the tests run, not what they test: the Rust harness runs a binary's
# cases in parallel threads, and a script written by one thread while
# another thread is forking races `execve` — the forked child inherits
# the writer's still-open descriptor until its own exec closes it, and an
# `execve` of that file in the meantime answers ETXTBSY ("Text file
# busy") on Linux (#431, #408). Per-test paths do not help; another
# test's fork is what breaks a file that is private to this one.
# Sourcing has no such check — `.` reads the file, it does not exec it —
# so nothing the tests exec is ever written by the test process, and each
# body still sits beside the case that needs it.
#
# `PATH` is pinned because a spawned launcher inherits the test process's
# environment, and `roost-cli`'s `doctor` tests point the process-global
# `PATH` at an empty directory while they run. Without this a body using
# `sleep` or `head` would silently become one that exits at once.
PATH=/usr/bin:/bin
export PATH

if [ ! -f "$0.conf" ]; then
    printf '%s\n' "fake-roost-session: nothing to do — no $0.conf beside the symlink" >&2
    exit 64
fi
# shellcheck disable=SC1090
. "$0.conf"
