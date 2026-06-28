# Roost — common dev tasks. Run `make` (or `make help`) to list them.
#
# Two native UIs around libghostty-vt: Swift + AppKit (mac/) and
# Rust + gtk4-rs (crates/roost-linux), plus the roostctl CLI. See
# docs/development/vision.md for the architecture + north star.

.DEFAULT_GOAL := help

MAC_DIR     := mac
APP         := $(MAC_DIR)/build/Roost.app
GHOSTTY_LIB := third_party/ghostty/out/lib/libghostty-vt.a

# ---- help -------------------------------------------------------------

.PHONY: help
help:  ## List available targets
	@echo "Roost dev tasks:"
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | sort \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

# ---- setup ------------------------------------------------------------

.PHONY: setup ghostty ghostty-force
setup: $(GHOSTTY_LIB)  ## One-time bootstrap: toolchain (mise) + libghostty-vt
	mise install

ghostty: $(GHOSTTY_LIB)  ## Build/cache libghostty-vt (no-op on cache hit)

ghostty-force:  ## Rebuild libghostty-vt from scratch (after a Ghostty SHA bump)
	third_party/ghostty/build.sh --force

# File rule: bootstraps libghostty-vt on a fresh clone so the first
# `make build` / `make build-mac` just works.
$(GHOSTTY_LIB):
	third_party/ghostty/build.sh

# ---- build ------------------------------------------------------------

.PHONY: build build-mac bundle build-all
build: $(GHOSTTY_LIB)  ## cargo build the workspace (GTK UI + roostctl)
	cargo build

build-mac: $(GHOSTTY_LIB)  ## swift build the Mac app
	cd $(MAC_DIR) && swift build

bundle: $(GHOSTTY_LIB)  ## Build + assemble Roost.app (debug)
	cd $(MAC_DIR) && ./scripts/bundle.sh debug

build-all: build bundle  ## Build both UIs + the Mac bundle

# ---- run --------------------------------------------------------------

.PHONY: run-gtk run-mac
run-gtk: build  ## Launch the GTK UI (Roost-gtk profile)
	./target/debug/roost

run-mac: bundle  ## Launch the bundled Mac app
	open $(APP)

# ---- test -------------------------------------------------------------

.PHONY: test test-rust test-mac e2e e2e-gtk e2e-mac e2e-gtk-ci e2e-mac-ci smoke-gtk smoke-mac smoke-mac-launch test-real-input
test: test-rust test-mac  ## All unit/integration tests (Rust + Swift)

test-rust:  ## cargo test --workspace
	cargo test --workspace

test-mac:  ## swift test (Mac)
	cd $(MAC_DIR) && swift test

e2e:  ## pytest E2E suite (ROOST_TARGET=mac|gtk, default gtk; launches the UI)
	uv run --group test pytest tools/roosttest

e2e-gtk:  ## E2E against the GTK UI
	uv run --group test pytest tools/roosttest --roost-target gtk

e2e-mac:  ## E2E against the Mac app
	uv run --group test pytest tools/roosttest --roost-target mac

e2e-gtk-ci:  ## GTK E2E at CI parity (test-mode + fresh harness-owned UI, isolated state)
	ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest --roost-target gtk --roost-fresh

e2e-mac-ci:  ## Mac E2E at CI parity. DESTRUCTIVE: force-quits any running Roost.app
	ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest --roost-target mac --roost-fresh

smoke-gtk:  ## Screenshot-driven UI smoke against a running GTK UI
	tools/screenshot/smoke.sh gtk

smoke-mac:  ## Screenshot-driven UI smoke against a running Mac app
	tools/screenshot/smoke.sh mac

smoke-mac-launch:  ## Clean-install launch check (bundles Roost.app, hides build-tree resources, asserts it starts)
	./mac/scripts/bundle.sh debug
	./mac/scripts/smoke-launch.sh

test-real-input:  ## GTK real-input regressions: focus/core-sync + drag reorder (self-contained Xvfb+xdotool)
	uv run --group test python tools/input/linux/real_input_check.py

# ---- code quality -----------------------------------------------------

.PHONY: fmt fmt-check clippy themes-check check
fmt:  ## Format Rust (cargo fmt --all)
	cargo fmt --all

fmt-check:  ## Check formatting (what CI's rust-lint runs)
	cargo fmt --all -- --check

clippy:  ## Lint Rust (cargo clippy --workspace)
	cargo clippy --workspace --all-targets

themes-check:  ## Assert the Rust + Mac bundled-theme copies are byte-identical
	diff -r crates/roost-linux/src/resources/themes mac/Sources/Roost/Resources/themes

check: fmt-check clippy themes-check test  ## Pre-push gate: fmt-check + clippy + themes-check + tests

# ---- docs -------------------------------------------------------------

.PHONY: docs docs-serve
docs:  ## Build the mkdocs site into site-build/
	uv sync --group docs && uv run mkdocs build

docs-serve:  ## Serve the docs locally (mkdocs serve)
	uv sync --group docs && uv run mkdocs serve

# ---- clean ------------------------------------------------------------

.PHONY: clean
clean:  ## Remove build artifacts (cargo target, Roost.app, site-build)
	cargo clean
	rm -rf $(MAC_DIR)/build site-build
