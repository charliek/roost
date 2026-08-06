#!/usr/bin/env bash
# Read a running Iced UI's render-path counters (`roostctl render-stats`)
# around a measurement window, so the printed numbers are a clean delta
# instead of a running total since process start.
#
#   tools/perf/render-stats.sh iced           # interactive: prompt before reading
#   tools/perf/render-stats.sh iced 10        # reset, sleep 10s, read
#
# Only the *running-app* half of the perf harness: `draw_calls` /
# `draw_nanos` / `fill_text_calls` need a live iced `Renderer`, which only
# a running UI has (see tools/perf/README.md). The `refresh_*` /
# `rows_rebuilt` / `cells_walked` counters this also prints are the same
# ones the in-crate `cargo test -p roost-iced --release -- --ignored
# --nocapture` harness measures without a UI — reach for that instead
# when you don't need the draw-path numbers.
#
# --target follows the rest of tools/: mac|gtk|iced (see ../screenshot/).
# The GTK UI has no render-path instrumentation yet, so it reports all
# zeros — that is expected, not a bug in this script.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../screenshot" && pwd)/lib.sh"

TARGET="${1:?usage: render-stats.sh <mac|gtk|iced> [duration_seconds]}"
DURATION="${2:-}"

ut_init "${TARGET}"
ut_alive || {
  echo "error: ${TARGET} UI is not running — launch it first (tools/screenshot/launch.sh ${TARGET})" >&2
  exit 1
}

echo "==> resetting ${TARGET} render-stats counters" >&2
rc render-stats --reset >/dev/null

if [[ -n "${DURATION}" ]]; then
  echo "==> waiting ${DURATION}s — exercise the app now (scroll, type, resize...)" >&2
  sleep "${DURATION}"
else
  read -rp "Exercise the app now, then press Enter to read the counters... " _unused
fi

echo "==> ${TARGET} render-stats since reset:" >&2
rc render-stats
