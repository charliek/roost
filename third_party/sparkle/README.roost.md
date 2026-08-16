# Sparkle (fetched, not vendored)

Provenance for the Sparkle.framework the Roost-Iced bundle embeds
(M6 6c, plan 028). Nothing here is committed — `fetch.sh` downloads the
official release artifact into the gitignored `out/` on demand:

* Version: **2.9.5** (latest stable at pin time, 2026-08-16)
* Artifact: `https://github.com/sparkle-project/Sparkle/releases/download/2.9.5/Sparkle-2.9.5.tar.xz`
* SHA256: `015336b601493e05c237964954bff6191370003d94edefe663724c88840d73cc`
  (computed from the official artifact; `fetch.sh` re-verifies with
  `shasum -a 256 -c` on every download)

Why 2.9.x and not 2.8.1 (the version the shed reference embedding
validated): the Swift Roost.app's SwiftPM resolution is already on
2.9.x, and later 2.9.x releases carry security fixes — for an *update*
framework, shipping a known-older line to match a reference repo would
be backwards. shed's signing-order lessons (the strict inner→outer
chain in `mac/scripts/bundle-lib.sh::codesign_sparkle_or_die`) are
version-stable and carry over; its version pin does not. The pin here
is independent of the Swift app's SwiftPM resolution — separate
products, drift is fine and recorded.

Staged layout (`fetch.sh`, idempotent via `out/.stamp`):

* `out/Sparkle.framework` — embedded by `mac/scripts/bundle-iced.sh`
  via `cp -R` (the `Versions/` symlink farm must stay intact; the
  runtime dlopen path resolves the top-level
  `Sparkle.framework/Sparkle` symlink, which `fetch.sh` validates).
* `out/bin/sign_update`, `out/bin/generate_keys` — EdDSA tooling for
  test fixtures and (later) real feed enablement.

Removal condition: delete this directory when Roost-Iced drops Sparkle
auto-update, or if the iced bundle ever switches to consuming the
framework from a package manager the cargo build can share.
