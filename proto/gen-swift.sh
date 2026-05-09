#!/usr/bin/env bash
# Generate Swift bindings for Roost's proto schema.
#
# Output: mac/Sources/Roost/Generated/{Roost.pb.swift, Roost.grpc.swift}
# (canonical location, matching Package.swift's source path and the repo's
# .gitignore pattern that excludes generated bindings from VCS).
#
# Requirements:
#   - protoc (Homebrew: brew install protobuf)
#   - protoc-gen-swift (Homebrew: brew install swift-protobuf)
#   - protoc-gen-grpc-swift (built from grpc/grpc-swift; see notes)
#
# CI installs all three via Homebrew on macos-latest. Locally, run this
# before committing if you've edited roost.proto and want the Mac UI to
# pick up the changes.
#
# Rust bindings are generated at build time by `crates/roost-proto/build.rs`
# via prost-build; no separate step is needed for the Rust side.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

OUT_DIR="${REPO_ROOT}/mac/Sources/Roost/Generated"
mkdir -p "${OUT_DIR}"

# Verify the toolchain. Fail with a helpful message rather than a cryptic
# "command not found" if the user hasn't installed the plugins.
for cmd in protoc protoc-gen-swift protoc-gen-grpc-swift; do
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "error: ${cmd} not found on PATH" >&2
    echo "       install with: brew install protobuf swift-protobuf grpc-swift" >&2
    exit 1
  fi
done

protoc \
  --proto_path="${SCRIPT_DIR}" \
  --swift_out="${OUT_DIR}" \
  --swift_opt=Visibility=Public \
  --grpc-swift_out="${OUT_DIR}" \
  --grpc-swift_opt=Client=true,Server=false,Visibility=Public \
  "${SCRIPT_DIR}/roost.proto"

echo "Swift bindings generated under ${OUT_DIR}"
