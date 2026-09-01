#!/bin/sh
# A fake `ssh` for roost's SSH-transport tests (host-sessions HS-3).
#
# Stands in for the real binary wherever a test needs to pin process
# choreography — which argv ran, in what order, with what still on disk —
# without a host on the other end. Every invocation is appended to
# $FAKE_SSH_LOG as one tab-separated line:
#
#   pid=<this process's pid><TAB><argv[1]><TAB>...<TAB><argv[n]>
#
# and, for a `-O exit`, one extra trailing field:
#
#   ctl-exists=<0|1>
#
# recording whether the `-S` path was still on disk when the exit ran.
# That is the whole point of the field: teardown ordering (exit the
# master *before* removing its control socket) is otherwise invisible.
#
# Configuration, all through the environment:
#
#   FAKE_SSH_LOG          required; the invocation log to append to.
#   FAKE_SSH_MODE         what this invocation does. Default `ok`.
#   FAKE_SSH_EXEC         shell source run (via `sh -c`) as the remote
#                         command in the modes that get that far.
#   FAKE_SSH_SESSION_ENV  optional file naming the variables the remote
#                         command runs with — the *whole* environment,
#                         not an addition to this process's. In
#                         `run-remote` it is how the far side gets its
#                         `HOME`, its `PATH` and its filesystem jail
#                         (see below).
#
# Modes:
#
#   ok                 run FAKE_SSH_EXEC (see the `true` rule below).
#   run-remote         run the *actual* remote command off the argv —
#                      see below.
#   auth-fail          `Permission denied (publickey).`, exit 255.
#   hostkey-fail       `Host key verification failed.`, exit 255.
#   hostkey-changed    the full REMOTE HOST IDENTIFICATION HAS CHANGED
#                      block, ending in `Host key verification failed.`,
#                      exit 255.
#   exit-127           `sh: roost-session: command not found`, exit 127.
#   unreachable        `No route to host`, exit 255 — a failure the
#                      classifier deliberately has no rule for, so it
#                      falls through to `Transport`. The only mode here
#                      that fails an establish *retryably*: every other
#                      one names a family the retry ladder refuses to
#                      spend an attempt on (a key to review, a password
#                      nobody can type under `BatchMode=yes`, a binary
#                      that is not there), which is exactly why a lane
#                      about giving up needed a mode of its own.
#   drop-after:<n>     run FAKE_SSH_EXEC but cut its output after <n>
#                      bytes, then exit 1.
#   slow-stderr-changed-key:<secs>
#                      the same block `hostkey-changed` writes and the
#                      same exit 255 — but first it backgrounds a
#                      `sleep <secs>` that inherits this stderr, so the
#                      pipe stays open for <secs> after the process that
#                      wrote to it is gone.
#   slow-stderr-hang:<secs>
#                      the same held-open stderr, and then this
#                      invocation hangs until it is killed. What a
#                      black-holed route looks like to a caller whose
#                      budget runs out.
#
# The two `slow-stderr-*` modes exist for one thing: reaping a child does
# not close a stderr pipe a *grandchild* inherited (a `ProxyCommand`
# helper, a remote `sh -c` that forked), so a drain that waits for EOF
# after the kill waits for the grandchild instead of for its own budget
# — issue #379. They are in this first dispatch block deliberately, so
# they fail the establish rather than a per-connection exec.
#
# `run-remote` in detail. `ok` runs one fixed FAKE_SSH_EXEC whatever it
# was asked to run, which is all a byte-pump transport test needs.
# The bootstrap tests (plan 039) need the opposite: the remote command
# *is* the thing under test — a generated `/bin/sh -s` script, a
# `tee -- <tmp>` the binary is streamed into, an exec of a discovered
# binary — so this mode runs the last argv element through `sh -c` with
# stdin and stdout wired straight through, exactly as sshd would.
#
# Which means the scripts run on **this** machine, and a fixture that
# leaks this machine's state is not a fixture. So `run-remote` runs the
# command under `env -i` with exactly the four variables below and
# nothing else — sourcing FAKE_SSH_SESSION_ENV only *adds* to what the
# test runner exported, and a developer's own ROOST_BOOTSTRAP_FS_ROOT
# reaching the far side would decide which binary the ladder resolves.
# Setting them is the caller's half of the contract, carried in
# FAKE_SSH_SESSION_ENV, and it is what the Rust suite writes there:
#
#   HOME=<tempdir>                 a fake home the install writes into.
#   PATH=<stub bin>                one directory, holding a fake `uname`
#                                  (which reports the OS and machine the
#                                  test wants) and symlinks to the
#                                  handful of coreutils the scripts
#                                  need — and no `roost-session`, so a
#                                  `command -v` finds nothing real.
#   USER=<name>                    what the ladder's per-user nix rung
#                                  interpolates.
#   ROOST_BOOTSTRAP_FS_ROOT=<dir>  the prefix a **test-mode** candidate
#                                  ladder puts in front of its absolute
#                                  rungs, so a `/usr/bin/roost-session`
#                                  probe lands inside the tempdir. A
#                                  shipped ladder never expands it at
#                                  all.
#
# Without all three the suite would pass or fail according to whether
# the developer's own box is a Mac and whether it has the deb
# installed. With them it is the same test everywhere.
#
# The mux flags a job passes (`-S <ctl>`, `-o ControlMaster=auto`,
# `-o ControlPersist=60s`) are accepted and ignored here, as they are in
# every other mode: they are logged, the `-S` path still gets the
# control-socket simulation below, and `-O exit` still removes it.
#
# Two rules that keep the fake faithful rather than merely configurable:
#
# * **`-O exit` is always a recorded no-op.** It never runs a remote
#   command, in any mode, and exits 0 — matching `ssh`, which only ever
#   talks to a local control socket for it. It also removes that socket,
#   as the real master does on its way out.
#
# * **A remote command of exactly `true` is honored literally.** That is
#   what the mux warm-up (`establish_argv`) runs, and running `true` on a
#   real host does nothing and exits 0 regardless of what any *other*
#   invocation would have done. Without this rule FAKE_SSH_EXEC would
#   have to be simultaneously "the connection's stdio partner" and "a
#   command that exits immediately", which no single value can be. The
#   failure modes still fail here, exactly as real ssh fails before it
#   ever reaches the remote command.
#
# Known limitation: `drop-after:<n>` cuts only the downstream
# (remote → client) direction. Cutting the upstream direction would need
# a second pipeline the shell cannot express portably, and no test needs
# it — a half-cut stream already produces the EOF-plus-nonzero-exit the
# client classifies on. So an *upload* cut mid-stream — a `tee` that
# stops half way through a binary — has no seam here either; the
# bootstrap suite reaches that case from the ends instead, with a remote
# `tee` that dies and with a local source that stops being readable.

set -u

# A caller that cannot set this process's environment — every caller,
# since the environment is process-global and the Rust suite runs its
# tests in parallel threads — configures us through a file named for the
# path it invoked us by. Symlink `<dir>/ssh` here, write `<dir>/ssh.conf`
# beside it, and each caller gets its own configuration with **no
# executable of its own to write**: writing a script while sibling
# threads are forking races `execve`, which answers ETXTBSY for a file
# any process still holds open for writing. Sourcing a plain data file
# has no such window. Callers that can write their wrapper once, before
# anything forks, do that instead and never create this file.
if [ -f "$0.conf" ]; then
    # shellcheck disable=SC1090
    . "$0.conf"
fi

: "${FAKE_SSH_LOG:?fake-ssh: FAKE_SSH_LOG must name an invocation log}"
FAKE_SSH_MODE="${FAKE_SSH_MODE:-ok}"
FAKE_SSH_EXEC="${FAKE_SSH_EXEC:-true}"

TAB=$(printf '\t')

line="pid=$$"
ctl=
want_ctl=0
prev=
is_exit=0
remote=
for arg in "$@"; do
    line="$line$TAB$arg"
    if [ "$want_ctl" -eq 1 ]; then
        ctl="$arg"
        want_ctl=0
    elif [ "$arg" = "-S" ]; then
        want_ctl=1
    fi
    if [ "$prev" = "-O" ] && [ "$arg" = "exit" ]; then
        is_exit=1
    fi
    prev="$arg"
    remote="$arg"
done

if [ "$is_exit" -eq 1 ]; then
    if [ -n "$ctl" ] && [ -e "$ctl" ]; then
        line="$line${TAB}ctl-exists=1"
        rm -f "$ctl"
    else
        line="$line${TAB}ctl-exists=0"
    fi
    printf '%s\n' "$line" >>"$FAKE_SSH_LOG"
    exit 0
fi

printf '%s\n' "$line" >>"$FAKE_SSH_LOG"

changed_key_block() {
    cat >&2 <<'EOF'
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!
Someone could be eavesdropping on you right now (man-in-the-middle attack)!
It is also possible that a host key has just been changed.
Host key verification failed.
EOF
}

# Background a process that inherits this stderr and outlives us, so the
# write end of the pipe stays open after this invocation is gone or
# killed. `sh -c` rather than a bare `sleep` because the point is the
# *grandchild*: it is what a `ProxyCommand` helper leaves on the pipe.
#
# Only stderr: holding the *other* two open would stall a caller's
# stdout pump on a process this is not asking anything of.
hold_stderr_open() {
    sh -c "sleep $1" </dev/null >/dev/null &
}

case "$FAKE_SSH_MODE" in
auth-fail)
    printf '%s\n' "$(id -un)@host: Permission denied (publickey)." >&2
    exit 255
    ;;
hostkey-fail)
    printf '%s\n' "Host key verification failed." >&2
    exit 255
    ;;
hostkey-changed)
    changed_key_block
    exit 255
    ;;
slow-stderr-changed-key:*)
    hold_stderr_open "${FAKE_SSH_MODE#slow-stderr-changed-key:}"
    changed_key_block
    exit 255
    ;;
slow-stderr-hang:*)
    hold_stderr_open "${FAKE_SSH_MODE#slow-stderr-hang:}"
    # `exec`, so the caller's kill lands on the thing that is hanging
    # rather than orphaning it behind a shell.
    exec sleep 3600
    ;;
exit-127)
    printf '%s\n' "sh: roost-session: command not found" >&2
    exit 127
    ;;
unreachable)
    # Nothing here may match a `classify_ssh_failure` rule: no changed-key
    # banner, no "Host key verification failed", no "Permission denied",
    # no "client-bridge: no session", no "command not found" — and an
    # exit that is not 127. What is left is the fallthrough, `Transport`,
    # which is the one family a drop is allowed to retry.
    printf '%s\n' "ssh: connect to host workbox port 22: No route to host" >&2
    exit 255
    ;;
esac

# Past here the invocation "connects", so the control socket the real
# master would have created exists for as long as this connection does.
if [ -n "$ctl" ] && [ ! -e "$ctl" ]; then
    : >"$ctl"
fi

# The warm-up's literal remote command. See the `true` rule above.
if [ "$remote" = "true" ]; then
    exit 0
fi

# Resolved before the session env is sourced, because sourcing replaces
# PATH with the far side's — which is a jail holding a handful of
# coreutils and deliberately not this.
fake_env=$(command -v env 2>/dev/null || true)

if [ -n "${FAKE_SSH_SESSION_ENV:-}" ]; then
    # shellcheck disable=SC1090
    . "$FAKE_SSH_SESSION_ENV"
fi

case "$FAKE_SSH_MODE" in
run-remote)
    # The real remote command, with this process's stdin and stdout as
    # its own — which is what carries a `/bin/sh -s` script in and a
    # streamed binary through `tee`.
    #
    # `env -i` and not the inherited environment: sourcing the session
    # env *adds* to what cargo exported, so without this the far side
    # would still see the whole of a developer's shell — including
    # their own ROOST_BOOTSTRAP_FS_ROOT, which decides which binary the
    # ladder resolves. The four names below are exactly the ones the
    # hermeticity contract above enumerates; anything else the remote
    # command needs, it does not get, which is the point.
    if [ -z "$fake_env" ]; then
        printf '%s\n' "fake-ssh: run-remote needs env(1) and could not find it" >&2
        exit 1
    fi
    exec "$fake_env" -i \
        HOME="${HOME:-}" \
        PATH="${PATH:-}" \
        USER="${USER:-}" \
        ROOST_BOOTSTRAP_FS_ROOT="${ROOST_BOOTSTRAP_FS_ROOT:-}" \
        sh -c "$remote"
    ;;
drop-after:*)
    bytes="${FAKE_SSH_MODE#drop-after:}"
    sh -c "$FAKE_SSH_EXEC" | head -c "$bytes"
    exit 1
    ;;
esac

exec sh -c "$FAKE_SSH_EXEC"
