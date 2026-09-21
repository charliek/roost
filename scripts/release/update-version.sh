#!/usr/bin/env bash
# Bump roost's release version across the manifests it owns.
#
# Single source of truth is [workspace.package].version in Cargo.toml.
# Cargo.lock is regenerated so the per-member entries match (every
# workspace member crate inherits via `version.workspace = true`).
# `.claude-plugin/plugin.json` carries the same version for the Claude
# Code plugin that ships `skills/roost`.
#
# `examples/ipc-consumer/` is its own workspace with its own lockfile,
# pinning roost-ipc by path; CI builds it `--locked`, so it has to move
# with the bump.
#
# `pyproject.toml` in this repo is for the test harness only — it has
# its own version cadence and is NOT touched by releases.
#
# Roost's Cargo.toml uses the column-aligned style (`version       = "X.Y.Z"`)
# throughout the [workspace.package] block. The sed replacement preserves
# the 7-space gap so we don't reformat the file's layout on every release.
# If the file is ever reformatted to vanilla single-space style, change
# `version       = "` below to `version = "`.
#
# Contract (see cc-plugins/plugins/release-workflows/references/update-version/README.md):
#   - one arg: semver string, no `v` prefix
#   - idempotent (same-version re-run leaves the tree unchanged)
#   - no network (--offline)
#   - verifies its own work
#   - does not `git add` (the release skill stages + commits)
#
# Adapted from the cc-plugins:release-workflows cargo-workspace.sh
# template. See that template's header for the generic shape.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <X.Y.Z>   e.g. $0 0.0.6" >&2
  exit 2
fi
V="$1"

if [[ ! "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]]; then
  echo "error: '$V' is not semver (X.Y.Z or X.Y.Z-suffix)" >&2
  exit 2
fi

# 1. Bump [workspace.package].version. Variable whitespace on the LHS
#    of the match (so a re-flow to vanilla style still matches) but a
#    fixed 7-space gap on the replacement (so roost's column alignment
#    survives).
sed -i.bak -E 's/^version[[:space:]]*=[[:space:]]*"[^"]+"/version       = "'"$V"'"/' Cargo.toml
rm -f Cargo.toml.bak

# 2. Verify Cargo.toml saw the bump. Match the aligned form because
#    that's what the replacement produces.
if ! grep -q "^version       = \"$V\"" Cargo.toml; then
  echo "error: Cargo.toml's [workspace.package].version did not update to $V." >&2
  echo "       Inspect by hand — the sed pattern may not match the current layout." >&2
  exit 1
fi

# 3. Regenerate Cargo.lock so workspace member entries match.
#    --workspace is the surface that actually moves.
#    --offline is safe: we're only changing internal version strings,
#    not touching the dep tree.
#
#    Resolve cargo via `mise exec` when the repo pins its toolchain
#    there (.mise.toml is the source of truth for the rust channel —
#    see rust-toolchain.toml's pin too), else fall back to whatever
#    cargo is on PATH (CI runners using actions-rs / rustup don't
#    need mise). Without this the script silently inherits the
#    caller's shell — non-interactive shells (`bash -c`, a release
#    skill subprocess, etc.) often don't have cargo on PATH even when
#    `mise install` already provisioned the pinned toolchain. Strix
#    v0.0.2 hit this exact failure mode; the fix landed in the
#    convention's cargo-workspace.sh template (cc-plugins#12) and is
#    cherry-picked here to keep roost aligned with that source of
#    truth.
if command -v mise >/dev/null 2>&1 && [[ -f .mise.toml ]]; then
  cargo=(mise exec -- cargo)
elif command -v cargo >/dev/null 2>&1; then
  cargo=(cargo)
else
  echo "error: cargo not found and 'mise exec' unavailable." >&2
  echo "       Install Rust (via rustup) or run inside a shell where" >&2
  echo "       \`mise exec -- cargo --version\` or \`cargo --version\` works." >&2
  exit 1
fi
"${cargo[@]}" update --workspace --offline >/dev/null

# 4. Verify the lockfile saw the bump. Cargo.lock uses single-space
#    style regardless of Cargo.toml's layout.
if ! grep -q "^version = \"$V\"" Cargo.lock; then
  echo "error: Cargo.lock did not update to $V — some member may override the version" >&2
  exit 1
fi

# 5. Refresh examples/ipc-consumer's own lockfile. That example is a
#    separate workspace pinning roost-ipc by path, and CI builds it with
#    --locked — so a bump that leaves its lockfile on the old version
#    fails the release commit's own CI after the tag is already pushed.
EXAMPLE_MANIFEST=examples/ipc-consumer/Cargo.toml
EXAMPLE_LOCK=examples/ipc-consumer/Cargo.lock
"${cargo[@]}" update -p roost-ipc --manifest-path "$EXAMPLE_MANIFEST" --offline >/dev/null

# 6. Verify it, for the reason step 2 verifies Cargo.toml.
if ! grep -q "^version = \"$V\"" "$EXAMPLE_LOCK"; then
  echo "error: $EXAMPLE_LOCK did not update to $V." >&2
  exit 1
fi

# 7. Bump the Claude Code plugin manifest, which Claude Code reads at
#    install/update time to decide whether the skill changed. Anchored to
#    the two-space indent so only the top-level `version` key can match —
#    a nested one would be indented deeper.
PLUGIN_JSON=.claude-plugin/plugin.json
sed -i.bak -E 's/^(  "version"[[:space:]]*:[[:space:]]*")[^"]*"/\1'"$V"'"/' "$PLUGIN_JSON"
rm -f "$PLUGIN_JSON.bak"

# 8. Verify it, for the reason step 2 verifies Cargo.toml: a sed that
#    matches nothing still exits 0.
if ! grep -q "^  \"version\": \"$V\",\$" "$PLUGIN_JSON"; then
  echo "error: $PLUGIN_JSON's top-level \"version\" did not update to $V." >&2
  exit 1
fi

echo "Bumped Cargo.toml + Cargo.lock + $EXAMPLE_LOCK + $PLUGIN_JSON to $V"
