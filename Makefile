.PHONY: all libghostty build run test lint clean

# Default: full build (libghostty-vt then Go binary).
all: build

# Build libghostty-vt against the pinned Ghostty SHA.
# Output goes to build/out/ (header at build/out/include/ghostty/vt.h, lib at build/out/lib).
libghostty:
	./build/build.sh libghostty

# Build the Go binary. Assumes libghostty has already been built (run `make libghostty` first).
build:
	./build/build.sh build

run: build
	./roost

test:
	go test ./...

lint:
	golangci-lint run

clean:
	rm -rf build/out build/ghostty-src ./roost ./roost-cli
