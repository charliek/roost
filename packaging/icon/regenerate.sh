#!/usr/bin/env bash
# Regenerate all Roost icon assets from the owl SVG.
#
# Runs generate_icons.py in an ephemeral uv env (cairosvg + Pillow), so no
# global Python deps are needed. Outputs:
#   packaging/icons/hicolor/{256x256,512x512}/apps/roost.png   (Linux .deb)
#   mac/Resources/AppIcon.icns                                 (macOS — needs iconutil)
#
# Change the brand color (then commit the regenerated assets):
#   ./packaging/icon/regenerate.sh --color '#1F6FEB'
#
# Defaults to Roost Violet (#6C4FD6). Run on macOS to refresh the .icns
# (iconutil is macOS-only; on Linux it writes the PNGs and skips the .icns).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# cairosvg loads native libcairo via cairocffi/ctypes, which doesn't search
# Homebrew's prefix on macOS. Add it to the dyld fallback path so the render
# works on a Mac dev box. (Linux finds libcairo via the normal loader cache.)
if [ "$(uname -s)" = "Darwin" ] && command -v brew >/dev/null 2>&1; then
  export DYLD_FALLBACK_LIBRARY_PATH="$(brew --prefix)/lib:${DYLD_FALLBACK_LIBRARY_PATH:-/usr/local/lib:/usr/lib}"
fi

exec uv run --with cairosvg --with pillow python "${SCRIPT_DIR}/generate_icons.py" "$@"
