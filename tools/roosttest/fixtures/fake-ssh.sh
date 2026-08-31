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
#   FAKE_SSH_SESSION_ENV  optional file sourced before FAKE_SSH_EXEC, so
#                         a test can hand the remote command variables of
#                         its own.
#
# Modes:
#
#   ok                 run FAKE_SSH_EXEC (see the `true` rule below).
#   auth-fail          `Permission denied (publickey).`, exit 255.
#   hostkey-fail       `Host key verification failed.`, exit 255.
#   hostkey-changed    the full REMOTE HOST IDENTIFICATION HAS CHANGED
#                      block, ending in `Host key verification failed.`,
#                      exit 255.
#   exit-127           `sh: roost-session: command not found`, exit 127.
#   drop-after:<n>     run FAKE_SSH_EXEC but cut its output after <n>
#                      bytes, then exit 1.
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
# client classifies on.

set -u

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
    cat >&2 <<'EOF'
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!
Someone could be eavesdropping on you right now (man-in-the-middle attack)!
It is also possible that a host key has just been changed.
Host key verification failed.
EOF
    exit 255
    ;;
exit-127)
    printf '%s\n' "sh: roost-session: command not found" >&2
    exit 127
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

if [ -n "${FAKE_SSH_SESSION_ENV:-}" ]; then
    # shellcheck disable=SC1090
    . "$FAKE_SSH_SESSION_ENV"
fi

case "$FAKE_SSH_MODE" in
drop-after:*)
    bytes="${FAKE_SSH_MODE#drop-after:}"
    sh -c "$FAKE_SSH_EXEC" | head -c "$bytes"
    exit 1
    ;;
esac

exec sh -c "$FAKE_SSH_EXEC"
