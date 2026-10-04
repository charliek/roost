#!/bin/bash
# Build + install Roost Test Runner once, on a Mac that has none. Refuses,
# unconditionally, to replace an installed copy: a rebuild changes the ad-hoc
# signature and silently voids its TCC grants. Replacing a broken one is a
# deliberate manual step (README.md), never this script's.
set -euo pipefail
[ "$(uname -s)" = "Darwin" ] || { echo "Roost Test Runner is a macOS app" >&2; exit 1; }
cd "$(dirname "$0")"
APP="$HOME/Applications/Roost Test Runner.app"
PROBE="$HOME/roost-harness/tcc-probe"
if [ "$#" -ne 0 ]; then
  echo "usage: $0 (no arguments)" >&2
  exit 2
fi
if [ -e "$APP" ]; then
  echo "already installed: $APP (rebuilding would void its grants; refusing)" >&2
  exit 1
fi
BUILD="$(mktemp -d)"
trap 'rm -rf "$BUILD"' EXIT
mkdir -p "$BUILD/Roost Test Runner.app/Contents/MacOS"
clang -O2 -Wall -Werror -o "$BUILD/Roost Test Runner.app/Contents/MacOS/roost-test-runner" runner.c
cp Info.plist "$BUILD/Roost Test Runner.app/Contents/Info.plist"
codesign --force --sign - --identifier ai.stridelabs.roost.test-runner "$BUILD/Roost Test Runner.app"
clang -O2 -Wall -Werror -o "$BUILD/tcc-probe" tcc-probe.c -framework ApplicationServices -framework IOKit -framework CoreFoundation
mkdir -p "$HOME/Applications"
cp -R "$BUILD/Roost Test Runner.app" "$APP"
if [ -e "$PROBE" ]; then
  echo "kept the existing $PROBE"
else
  mkdir -p "$(dirname "$PROBE")"
  cp "$BUILD/tcc-probe" "$PROBE"
fi
codesign -dv "$APP" 2>&1 | grep -E "Identifier|CDHash"
