#!/bin/sh
# libroost_mobile.a carries libghostty-vt.a's bundled zig compiler_rt.o next
# to Rust's compiler_builtins. The Runner links the archive with -force_load,
# which loads every member, so the two define the same symbols (ld: duplicate
# symbol ___ucmpti2, ...). Dropping zig's object loses symbols only it has
# (___extendxftf2), so localize just the colliding ones and let Rust's copies
# serve them. Apple's toolchain has no objcopy; every Rust toolchain ships one.
#
# usage: strip_zig_compiler_rt.sh <libroost_mobile.a>
set -eu
lib="$1"
export PATH="$HOME/.cargo/bin:$PATH"
# rustc from the repo, so rust-toolchain.toml picks the pinned toolchain.
cd -P "$(dirname "$0")/../../.."
host="$(rustc -vV | sed -n 's/^host: //p')"
objcopy="$(rustc --print sysroot)/lib/rustlib/$host/bin/rust-objcopy"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
thin=""
for arch in $(lipo -archs "$lib"); do
  a="$tmp/$arch.a"
  lipo "$lib" -thin "$arch" -output "$a" 2>/dev/null || cp "$lib" "$a"
  if ar -t "$a" | grep -qx compiler_rt.o; then
    (cd "$tmp" && ar -x "$a" compiler_rt.o && mv compiler_rt.o "rt-$arch.o")
    ar -d "$a" compiler_rt.o
    nm -gjU "$tmp/rt-$arch.o" | sort -u > "$tmp/rt.syms"
    nm -gjU "$a" 2>/dev/null | grep -v ':$' | sort -u > "$tmp/rest.syms"
    comm -12 "$tmp/rt.syms" "$tmp/rest.syms" > "$tmp/dups.syms"
    "$objcopy" --localize-symbols="$tmp/dups.syms" "$tmp/rt-$arch.o" "$tmp/compiler_rt.o"
    (cd "$tmp" && ar -q "$a" compiler_rt.o)
    ranlib "$a"
  fi
  thin="$thin $a"
done
# shellcheck disable=SC2086
lipo -create $thin -output "$lib"
