.PHONY: all libghostty build run test lint clean docs docs-serve

# Default: full build (libghostty-vt then Go binary). On a fresh
# clone the file rule below builds libghostty first; on subsequent
# runs it's a no-op because the sentinel header already exists.
# `make libghostty` (the .PHONY target) is the explicit way to
# force a rebuild — useful after bumping the pinned Ghostty SHA.
all: build/out/include/ghostty/vt.h build

# Build libghostty-vt against the pinned Ghostty SHA. Always runs.
# Output goes to build/out/ (header at build/out/include/ghostty/vt.h, lib at build/out/lib).
libghostty:
	./build/build.sh libghostty

# File rule that produces the libghostty header. Used as the
# fresh-clone bootstrap dep for `all`.
build/out/include/ghostty/vt.h:
	./build/build.sh libghostty

# Build the Go binary. Assumes libghostty has already been built
# (run `make libghostty` or `make` to bootstrap if not).
build:
	./build/build.sh build

run: build
	./roost

test:
	go test ./...

lint:
	golangci-lint run

clean:
	rm -rf build/out build/ghostty-src ./roost ./roost-cli site-build

# Build the static documentation site under site-build/.
docs:
	uv sync --group docs
	uv run mkdocs build

# Serve the documentation site locally on http://127.0.0.1:7070.
docs-serve:
	uv sync --group docs
	uv run mkdocs serve
