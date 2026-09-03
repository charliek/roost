#!/usr/bin/env bash
# Dev loop for a matching roost-session daemon on a real remote — build it
# on a shed of the target's own architecture, fetch it back proving the
# version pin, check it against a real ssh target proving the arch, then
# launch a local Roost-Iced.app pointed at it via the product's own
# bootstrap (ROOST_SESSION_INSTALL_BIN). No cross-compile, no roostctl
# verb, no scp of a daemon into place — see tools/session/README.md and
# docs/development/host-sessions.md's "Dev loop" section for the why.
#
# Usage:
#   tools/session/dev-session.sh build [-s <server>] <shed>
#   tools/session/dev-session.sh fetch [-s <server>] <shed>
#   tools/session/dev-session.sh check <artifact> <ssh-target>
#   tools/session/dev-session.sh launch <artifact> [--target <ssh-target>] [--app <Roost-Iced.app>]
#   tools/session/dev-session.sh all [-s <server>] <shed> <ssh-target>
#
# <shed> is a shed name known to `shed list` (or `shed -s <server> list`
# for one on a remote shed server). <ssh-target> is anything `ssh` accepts
# as a destination with an explicit user@host — `ssh://user@host:port` or
# plain `user@host:port` (port defaults to 22).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CACHE_DIR="${HOME}/.cache/roost/dev-session"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10)
FETCHED_ARTIFACT=""  # set by cmd_fetch; read by cmd_all
FETCH_TMP=""  # cmd_fetch's in-progress temp file; cleaned up by the EXIT trap below
trap 'rm -f "$FETCH_TMP"' EXIT

log() { printf '[dev-session] %s\n' "$*"; }
die() { printf '[dev-session] error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage:
  tools/session/dev-session.sh build [-s <server>] <shed>
  tools/session/dev-session.sh fetch [-s <server>] <shed>
  tools/session/dev-session.sh check <artifact> <ssh-target>
  tools/session/dev-session.sh launch <artifact> [--target <ssh-target>] [--app <Roost-Iced.app>]
  tools/session/dev-session.sh all [-s <server>] <shed> <ssh-target>

<shed> is a shed name known to `shed list` (or `shed -s <server> list` for
one on a remote shed server). <ssh-target> is anything ssh accepts as a
destination with an explicit user@host: ssh://user@host:port or plain
user@host:port (port defaults to 22).

See tools/session/README.md.
EOF
}

for t in ssh jq tar cargo; do
  command -v "$t" >/dev/null 2>&1 || die "required tool '$t' is not on PATH"
done

# ---- ssh-target and shed-json plumbing -------------------------------

# Splits a `[ssh://]user@host[:port]` string into SSH_USERHOST/SSH_PORT.
parse_ssh_target() {
  local t="${1#ssh://}"
  [[ "$t" == *@* ]] || die "ssh target '$1' needs an explicit user@host (got no '@')"
  if [[ "$t" == *:* ]]; then
    SSH_USERHOST="${t%:*}"
    SSH_PORT="${t##*:}"
  else
    SSH_USERHOST="$t"
    SSH_PORT=22
  fi
}

ssh_run() {
  # ssh_run <userhost> <port> <remote-command-string>
  ssh "${SSH_OPTS[@]}" -p "$2" "$1" "$3"
}

map_uname_arch() {
  case "$1" in
    x86_64|amd64) echo amd64 ;;
    aarch64|arm64) echo arm64 ;;
    *) die "unrecognized remote arch '$1' (uname -m) — expected x86_64 or aarch64" ;;
  esac
}

# Looks up a shed by name (optionally on a remote server) and fills
# SHED_SSH_TARGET (user@host:port) + SHED_LOCAL_MOUNT (1 if VirtioFS-mounted).
# Refuses if the shed is not running.
shed_lookup() {
  local server="$1" name="$2" json obj status
  if [ -n "$server" ]; then
    json="$(shed -s "$server" list --json)" || die "'shed -s $server list --json' failed"
  else
    json="$(shed list --json)" || die "'shed list --json' failed"
  fi
  obj="$(printf '%s' "$json" | jq -c --arg n "$name" '[.[] | select(.name == $n)] | first // empty')"
  [ -n "$obj" ] || die "no shed named $name${server:+ on server $server} — check: shed${server:+ -s $server} list"
  status="$(printf '%s' "$obj" | jq -r '.status')"
  [ "$status" = "running" ] || die "shed '$name' is not running (status: $status) — start it: shed${server:+ -s $server} start $name"
  SHED_SSH_TARGET="$(printf '%s' "$obj" | jq -r '.ssh // empty')"
  [ -n "$SHED_SSH_TARGET" ] || die "shed '$name' has no ssh endpoint in 'shed list --json' — is sshd running in it?"
  SHED_LOCAL_MOUNT="$(printf '%s' "$obj" | jq -r 'if (.project_mounts // []) | length > 0 then "1" else "" end')"
}

# Refuses below 2G free on the shed's root fs; warns below 4G. Never deletes.
check_disk() {
  local userhost="$1" port="$2" avail_kb avail_g
  avail_kb="$(ssh_run "$userhost" "$port" "df -Pk / | awk 'NR==2{print \$4}'")" \
    || die "could not read free disk space on the shed"
  [[ "$avail_kb" =~ ^[0-9]+$ ]] || die "could not read free space on the shed's root fs"
  avail_g=$(( avail_kb / 1024 / 1024 ))
  log "shed root fs: ${avail_g}G free"
  if [ "$avail_kb" -lt $((2 * 1024 * 1024)) ]; then
    log "disk usage on the shed (never auto-deleted):"
    ssh_run "$userhost" "$port" 'du -sh ~/rt* ~/ghostty-src 2>/dev/null' || true
    die "shed root fs has < 2G free (${avail_g}G) — retired per-plan CARGO_TARGET_DIRs are the usual cause"
  elif [ "$avail_kb" -lt $((4 * 1024 * 1024)) ]; then
    log "WARNING: shed root fs has < 4G free (${avail_g}G) — a build may fail if it grows further"
  fi
}

# The mounted-repo path inside a local-mount shed for THIS checkout (main
# repo root -> ~/roost; a worktree -> ~/roost/<its path relative to the
# main repo root, unchanged>). Deliberately prints a literal, unexpanded
# "$HOME" — the caller embeds it in a remote command string so the SHED's
# $HOME expands it, not this Mac's. ROOST_SHED_REPO, if set, is trusted
# verbatim and skips all of this — the escape hatch for a checkout that
# isn't under the main repo root (an external worktree), which the shed's
# VirtioFS mount never sees at ~/roost.
# shellcheck disable=SC2016
in_vm_repo_path() {
  if [ -n "${ROOST_SHED_REPO:-}" ]; then
    printf '%s\n' "$ROOST_SHED_REPO"
    return
  fi
  local main_root
  main_root="$(cd "$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)"
  if [ "$REPO_ROOT" = "$main_root" ]; then
    printf '%s\n' '$HOME/roost'
  elif [ "${REPO_ROOT#"$main_root"/}" != "$REPO_ROOT" ]; then
    printf '%s/%s\n' '$HOME/roost' "${REPO_ROOT#"$main_root"/}"
  else
    die "this checkout ($REPO_ROOT) is not under the main checkout's root ($main_root), so it is not visible at ~/roost in the shed — set ROOST_SHED_REPO to the in-VM path for this tree"
  fi
}

# Reads e_machine (offset 18, header's own EI_DATA byte order) and returns
# amd64/arm64, or refuses non-ELF / unsupported-machine input outright —
# this script only ever handles a daemon it just fetched, so it is
# deliberately stricter than the product's own bootstrap guard.
elf_arch() {
  local f="$1" size magic ei_class ei_data min_size raw b0 b1 machine
  [ -f "$f" ] || die "$f: not a Linux binary (no such file)"
  size="$(wc -c < "$f" | tr -d ' ')"
  [ "$size" -ge 20 ] || die "$f: not a Linux binary (too short to be ELF)"
  magic="$(od -An -tx1 -N4 -- "$f" | tr -d ' \n')"
  [ "$magic" = "7f454c46" ] || die "$f: not a Linux binary (no ELF magic)"
  # EI_CLASS (byte 4) picks the header size a crafted short file must
  # actually have — checked before trusting anything past the magic,
  # so a truncated file can't be padded just enough to fake e_machine.
  ei_class="$(od -An -tx1 -j4 -N1 -- "$f" | tr -d ' \n')"
  case "$ei_class" in
    01) min_size=52 ;;  # ELFCLASS32
    02) min_size=64 ;;  # ELFCLASS64
    *) die "$f: not a Linux binary (unrecognized ELF EI_CLASS byte '$ei_class')" ;;
  esac
  [ "$size" -ge "$min_size" ] \
    || die "$f: not a Linux binary (truncated ELF header — need >= ${min_size} bytes for EI_CLASS $ei_class, got $size)"
  ei_data="$(od -An -tx1 -j5 -N1 -- "$f" | tr -d ' \n')"
  case "$ei_data" in
    01|02) ;;
    *) die "$f: not a Linux binary (unrecognized ELF EI_DATA byte '$ei_data')" ;;
  esac
  raw="$(od -An -tx1 -j18 -N2 -- "$f" | tr -d ' \n')"
  b0="${raw:0:2}"; b1="${raw:2:2}"
  case "$ei_data" in
    01) machine="0x${b1}${b0}" ;;  # little-endian: low byte first on disk
    02) machine="0x${b0}${b1}" ;;  # big-endian
  esac
  case "$machine" in
    0x003e) echo amd64 ;;
    0x00b7) echo arm64 ;;
    *) die "$f is an ELF for machine ${machine}, not amd64/arm64" ;;
  esac
}

# ---- build -------------------------------------------------------------

cmd_build() {
  local server="" shed=""
  while [ $# -gt 0 ]; do
    case "$1" in
      -s) [ $# -ge 2 ] || die "-s requires a value"; server="$2"; shift 2 ;;
      -*) die "build: unknown flag '$1'" ;;
      *) [ -z "$shed" ] || die "build: unexpected extra argument '$1'"; shed="$1"; shift ;;
    esac
  done
  [ -n "$shed" ] || die "usage: dev-session.sh build [-s <server>] <shed>"

  shed_lookup "$server" "$shed"
  parse_ssh_target "$SHED_SSH_TARGET"
  local remote_uname remote_arch
  remote_uname="$(ssh_run "$SSH_USERHOST" "$SSH_PORT" 'uname -m')" || die "ssh to $SHED_SSH_TARGET failed"
  remote_arch="$(map_uname_arch "$remote_uname")"
  log "shed '$shed' is $remote_arch ($remote_uname)"
  check_disk "$SSH_USERHOST" "$SSH_PORT"

  # Both branches end at the same build-in-shed.sh invocation; they differ only
  # in where the tree lives. remote_repo carries a literal, unexpanded "$HOME"
  # for the SHED to expand — see in_vm_repo_path.
  local remote_repo
  if [ -n "$SHED_LOCAL_MOUNT" ]; then
    remote_repo="$(in_vm_repo_path)"
    log "local-mount shed — building roost-session in place at $remote_repo"
  else
    log "remote shed — pushing the working tree over ssh (no rsync on the shed image; tar instead)"
    # --no-xattrs keeps macOS's com.apple.* xattr PAX records out of the
    # archive entirely, so the remote GNU tar extracting it never hits its
    # "Ignoring unknown extended header keyword" warning — nothing here
    # redirects stderr, so a real error (e.g. remote disk full) stays visible.
    tar -C "$REPO_ROOT" --no-xattrs \
      --exclude=./.git --exclude=./target \
      --exclude=./third_party/ghostty/src --exclude=./third_party/ghostty/out \
      --exclude=./.claude/worktrees --exclude=./site-build \
      --exclude=./mac/.build --exclude=./mac/build \
      -cf - . \
      | ssh "${SSH_OPTS[@]}" -p "$SSH_PORT" "$SSH_USERHOST" \
          'mkdir -p "$HOME/roost" && tar -C "$HOME/roost" -xf -' \
      || die "pushing the tree to $SHED_SSH_TARGET failed"
    # shellcheck disable=SC2016
    remote_repo='$HOME/roost'
    log "remote shed — building roost-session in $remote_repo"
  fi
  ssh_run "$SSH_USERHOST" "$SSH_PORT" \
    "bash -lc 'chmod +x \"$remote_repo/tools/shed/build-in-shed.sh\" && ROOST_REPO=\"$remote_repo\" ROOST_SHED_PACKAGES=roost-session \"$remote_repo/tools/shed/build-in-shed.sh\"'"
}

# ---- fetch ---------------------------------------------------------------

cmd_fetch() {
  local server="" shed=""
  while [ $# -gt 0 ]; do
    case "$1" in
      -s) [ $# -ge 2 ] || die "-s requires a value"; server="$2"; shift 2 ;;
      -*) die "fetch: unknown flag '$1'" ;;
      *) [ -z "$shed" ] || die "fetch: unexpected extra argument '$1'"; shed="$1"; shift ;;
    esac
  done
  [ -n "$shed" ] || die "usage: dev-session.sh fetch [-s <server>] <shed>"

  shed_lookup "$server" "$shed"
  parse_ssh_target "$SHED_SSH_TARGET"
  local remote_uname remote_arch
  remote_uname="$(ssh_run "$SSH_USERHOST" "$SSH_PORT" 'uname -m')" || die "ssh to $SHED_SSH_TARGET failed"
  remote_arch="$(map_uname_arch "$remote_uname")"

  # A literal, unexpanded "$HOME" — the SHED expands it, not this Mac.
  # shellcheck disable=SC2016
  local remote_bin='$HOME/rt/debug/roost-session'
  ssh_run "$SSH_USERHOST" "$SSH_PORT" "test -f \"$remote_bin\"" \
    || die "no $remote_bin in shed '$shed' — run: dev-session.sh build${server:+ -s $server} $shed"

  mkdir -p "$CACHE_DIR"
  FETCH_TMP="$(mktemp "${CACHE_DIR}/.fetch.XXXXXX")"
  log "copying $remote_bin out of '$shed' over ssh"
  ssh_run "$SSH_USERHOST" "$SSH_PORT" "cat \"$remote_bin\"" > "$FETCH_TMP" \
    || die "copying the binary out of '$shed' failed"

  local artifact_arch
  artifact_arch="$(elf_arch "$FETCH_TMP")"
  if [ "$artifact_arch" != "$remote_arch" ]; then
    die "fetched binary is ELF arch '$artifact_arch' but shed '$shed' reports uname -m '$remote_uname' ($remote_arch) — refusing a mismatched fetch"
  fi

  log "fetch proves the version pin; run 'check' next to prove the arch against a real target"
  # ROOST_DEV_SESSION_IDENTIFY_ENV, if set, is prefixed onto the shed-side
  # `identify` invocation verbatim — e.g.
  # "ROOST_TEST_MODE=1 ROOST_SESSION_FAKE_BUILD=bogus" is the seam that
  # proves the identity-mismatch refusal below deterministically, without
  # needing a genuinely differently-pinned build.
  local identify_prefix="${ROOST_DEV_SESSION_IDENTIFY_ENV:+$ROOST_DEV_SESSION_IDENTIFY_ENV }"
  log "running identify in the shed${ROOST_DEV_SESSION_IDENTIFY_ENV:+ (with $ROOST_DEV_SESSION_IDENTIFY_ENV)}"
  local remote_identity remote_version remote_build
  remote_identity="$(ssh_run "$SSH_USERHOST" "$SSH_PORT" "${identify_prefix}\"$remote_bin\" identify")" \
    || die "'roost-session identify' failed in shed '$shed'"
  remote_version="$(printf '%s' "$remote_identity" | jq -r '.app_version // empty')"
  remote_build="$(printf '%s' "$remote_identity" | jq -r '.libghostty_build // empty')"
  [ -n "$remote_version" ] && [ -n "$remote_build" ] \
    || die "unexpected 'identify' output from shed '$shed': $remote_identity"

  local local_bin="${REPO_ROOT}/target/debug/roost-session"
  if [ ! -x "$local_bin" ]; then
    log "no local target/debug/roost-session — building it: cargo build -p roost-session"
    ( cd "$REPO_ROOT" && cargo build -p roost-session ) || die "local 'cargo build -p roost-session' failed"
  fi
  local local_identity local_version local_build
  local_identity="$("$local_bin" identify)" || die "local 'roost-session identify' failed"
  local_version="$(printf '%s' "$local_identity" | jq -r '.app_version // empty')"
  local_build="$(printf '%s' "$local_identity" | jq -r '.libghostty_build // empty')"

  if [ "$local_version" != "$remote_version" ] || [ "$local_build" != "$remote_build" ]; then
    die "identity mismatch — local: app_version=$local_version libghostty_build=$local_build; shed '$shed': app_version=$remote_version libghostty_build=$remote_build. Rebuild (dev-session.sh build${server:+ -s $server} $shed) and fetch again."
  fi

  local name="roost-session-${remote_version}-linux-${artifact_arch}"
  local dest="${CACHE_DIR}/${name}"
  mv "$FETCH_TMP" "$dest"
  FETCH_TMP=""
  chmod 755 "$dest"
  printf '%s\n' "$remote_identity" > "${dest}.identity.json"
  ( cd "$REPO_ROOT" && printf 'HEAD=%s\ndirty_files=%s\n' \
      "$(git rev-parse HEAD)" \
      "$(git status --porcelain | wc -l | tr -d ' ')" \
  ) > "${dest}.tree.txt"

  log "identity matches local build: app_version=$local_version libghostty_build=$local_build"
  log "fetched: $dest"
  log "identity: ${dest}.identity.json"
  log "tree:     ${dest}.tree.txt"
  FETCHED_ARTIFACT="$dest"
}

# ---- check ---------------------------------------------------------------

cmd_check() {
  [ $# -eq 2 ] || die "usage: dev-session.sh check <artifact> <ssh-target>"
  local artifact="$1" target="$2"
  [ -f "$artifact" ] || die "artifact not found: $artifact"
  local artifact_arch
  artifact_arch="$(elf_arch "$artifact")"

  parse_ssh_target "$target"
  local remote_uname remote_arch
  remote_uname="$(ssh_run "$SSH_USERHOST" "$SSH_PORT" 'uname -m')" || die "ssh to $target failed"
  remote_arch="$(map_uname_arch "$remote_uname")"

  if [ "$artifact_arch" != "$remote_arch" ]; then
    die "arch mismatch: $artifact is linux-${artifact_arch} but $target is linux-${remote_arch} (uname -m: $remote_uname) — build on an ${remote_arch} shed instead (roost-dev is arm64, a shed on mini3 is amd64) and fetch that artifact"
  fi
  log "check: OK — $artifact (${artifact_arch}) matches $target (${remote_arch})"
}

# ---- launch ---------------------------------------------------------------

cmd_launch() {
  local artifact="" target="" app=""
  [ $# -ge 1 ] || die "usage: dev-session.sh launch <artifact> [--target <ssh-target>] [--app <Roost-Iced.app>]"
  artifact="$1"; shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --target) [ $# -ge 2 ] || die "--target requires a value"; target="$2"; shift 2 ;;
      --app) [ $# -ge 2 ] || die "--app requires a value"; app="$2"; shift 2 ;;
      *) die "launch: unknown argument '$1'" ;;
    esac
  done
  [ -f "$artifact" ] || die "artifact not found: $artifact"

  if [ -n "$target" ]; then
    cmd_check "$artifact" "$target"
  fi

  app="${app:-${REPO_ROOT}/mac/build/Roost-Iced.app}"
  [ -d "$app" ] || die "no app bundle at $app — build one first: mac/scripts/bundle-iced.sh debug"

  # bundle-iced.sh always names both the .app dir and its executable
  # "Roost-Iced" (APP_NAME) — this is not derived from --app's basename.
  local exe="${app}/Contents/MacOS/Roost-Iced"
  [ -x "$exe" ] || die "$exe missing or not executable — rebuild with mac/scripts/bundle-iced.sh debug"

  log "running: ROOST_SESSION_INSTALL_BIN=$artifact $exe"
  ROOST_SESSION_INSTALL_BIN="$artifact" exec "$exe"
}

# ---- all ---------------------------------------------------------------

cmd_all() {
  local server="" shed="" target=""
  while [ $# -gt 0 ]; do
    case "$1" in
      -s) [ $# -ge 2 ] || die "-s requires a value"; server="$2"; shift 2 ;;
      -*) die "all: unknown flag '$1'" ;;
      *)
        if [ -z "$shed" ]; then shed="$1"
        elif [ -z "$target" ]; then target="$1"
        else die "all: unexpected extra argument '$1'"
        fi
        shift ;;
    esac
  done
  [ -n "$shed" ] && [ -n "$target" ] || die "usage: dev-session.sh all [-s <server>] <shed> <ssh-target>"

  local shed_args=("$shed")
  [ -z "$server" ] || shed_args=(-s "$server" "$shed")
  cmd_build "${shed_args[@]}"
  cmd_fetch "${shed_args[@]}"
  cmd_launch "$FETCHED_ARTIFACT" --target "$target"
}

# ---- dispatch ---------------------------------------------------------------

sub="${1:-}"
[ -n "$sub" ] || { usage; exit 1; }
shift || true
case "$sub" in
  build) cmd_build "$@" ;;
  fetch) cmd_fetch "$@" ;;
  check) cmd_check "$@" ;;
  launch) cmd_launch "$@" ;;
  all) cmd_all "$@" ;;
  -h|--help) usage; exit 0 ;;
  *) usage; die "unknown subcommand '$sub'" ;;
esac
