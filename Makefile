# Roost — common dev tasks. Run `make` (or `make help`) to list them.
#
# Two native UIs around libghostty-vt: Swift + AppKit (mac/) and Rust +
# iced (crates/roost-iced, what the Linux package ships), plus the
# roostctl CLI. See docs/development/vision.md for the architecture +
# north star.

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

.PHONY: build build-iced build-mac bundle bundle-iced build-all
build: $(GHOSTTY_LIB)  ## cargo build the workspace (Iced UI + roostctl)
	cargo build

build-iced: $(GHOSTTY_LIB)  ## Build the isolated Iced UI binary
	cargo build -p roost-iced

build-mac: $(GHOSTTY_LIB)  ## swift build the Mac app
	cd $(MAC_DIR) && swift build

bundle: $(GHOSTTY_LIB)  ## Build + assemble Roost.app (debug)
	cd $(MAC_DIR) && ./scripts/bundle.sh debug

bundle-iced: $(GHOSTTY_LIB)  ## Build + assemble Roost-Iced.app (debug)
	cd $(MAC_DIR) && ./scripts/bundle-iced.sh debug

build-all: build bundle  ## Build both UIs + the Mac bundle

# ---- run --------------------------------------------------------------

.PHONY: run-iced run-mac
run-iced: build-iced  ## Launch the Iced UI (Roost-iced profile)
	ROOST_BUNDLE_PROFILE=iced ./target/debug/roost-iced

run-mac: bundle  ## Launch the bundled Mac app
	open $(APP)

# ---- test -------------------------------------------------------------

.PHONY: test test-rust test-iced test-mac test-harness e2e e2e-iced e2e-iced-exit e2e-iced-menu-quit e2e-iced-clipboard e2e-mac e2e-iced-ci e2e-iced-release-ci e2e-mac-ci e2e-iced-bundle e2e-iced-sparkle smoke-iced smoke-mac visual-parity smoke-mac-launch test-iced-real-input test-iced-wayland-input check-iced perf-refresh perf-render-stats

ICED_E2E_TESTS := tools/roosttest/test_smoke.py tools/roosttest/test_iced_walking_skeleton.py tools/roosttest/test_notifications.py tools/roosttest/test_provider.py tools/roosttest/test_sidebar_pixels.py tools/roosttest/test_tab_strip_pixels.py tools/roosttest/test_focus.py tools/roosttest/test_palette.py tools/roosttest/test_z_typography.py tools/roosttest/test_project_lifecycle.py tools/roosttest/test_sidebar_resize.py tools/roosttest/test_osc_pipeline.py tools/roosttest/test_sprite_pixels.py tools/roosttest/test_ime.py tools/roosttest/test_selection.py tools/roosttest/test_mouse_tracking.py tools/roosttest/test_dock_badge.py tools/roosttest/test_menu_bar.py tools/roosttest/test_sparkle.py tools/roosttest/test_view_perf.py
# `test_sparkle.py`'s two classes split by lane: the bare-binary class
# runs here (no framework beside a cargo binary ⇒ updater unavailable)
# and its bundle class self-skips — `make e2e-iced-sparkle` is where the
# latter runs, against a bundle assembled with the fixture's TEST-ONLY
# public key.
# `test_dock_badge.py` and `test_menu_bar.py` self-skip unless the host is
# macOS AND the target is iced (the native menu bar + Dock badge are both
# macOS-iced-only seams — plan 027 § 6b / plan 028 § 6d), so they cost a
# skip line on the Linux lanes and run for real on the macOS ones.
# `selection.*` reads UI state over IPC and never touches the host
# pasteboard, so `test_selection.py` belongs in the list above and runs
# under headless Wayland too. Only files that read/write the real
# clipboard go here.
ICED_CLIPBOARD_TESTS := tools/roosttest/test_osc52.py
# Its OWN invocation, never a member of ICED_E2E_TESTS: this module deletes
# the last project, which now ends the app (plan 026 D8) — inside the shared
# session it would strand every module that runs after it. Always fresh: it
# empties the workspace, so it must own the instance it drives.
ICED_EXIT_E2E_TESTS := tools/roosttest/test_exit_on_empty.py
# `test_menu_quit.py` (plan 028 C3) is ALSO app-ending (the menu's Quit
# item), so it needs the same "own invocation" isolation `ICED_EXIT_E2E_TESTS`
# gets — but not the SAME invocation as that list: the session-scoped
# harness fixture launches exactly one UI for the whole pytest run, so
# whichever exit-ending module ran first would strand the other. Kept as
# its own list/target/CI steps rather than folded into ICED_EXIT_E2E_TESTS.
ICED_MENU_QUIT_E2E_TESTS := tools/roosttest/test_menu_quit.py
# The release-profile lane's curated subset, not the full ICED_E2E_TESTS
# list: startup, the core op set, the VT pipeline, and font shaping/glyph
# rasterization — the last two because the one release-only bug this stack
# has produced was issue #299's swash shaping hang.
ICED_RELEASE_E2E_TESTS := tools/roosttest/test_smoke.py tools/roosttest/test_iced_walking_skeleton.py tools/roosttest/test_osc_pipeline.py tools/roosttest/test_z_typography.py tools/roosttest/test_sprite_pixels.py tools/roosttest/test_view_perf.py
# The Sparkle lane's bundle inputs. The feed URL is a deliberate dead
# placeholder: the seam's test-mode delegate override replaces it with
# the live loopback port at check time, and a plist URL that could never
# resolve means a stray launch of this bundle can't reach anything.
SPARKLE_TEST_PUBLIC_KEY := tools/roosttest/fixtures/sparkle/TEST-ONLY-public-ed-key.txt
SPARKLE_TEST_PLACEHOLDER_FEED := http://127.0.0.1:1/placeholder
test: test-rust test-mac test-harness  ## All unit/integration tests (Rust + Swift + harness)

# roost-vt's tests/*.rs all start with `#![cfg(feature = "ffi")]`, so the
# `--workspace` run compiles and then silently skips every one of them. The
# second line mirrors CI's separate `cargo test -p roost-vt --features ffi`
# step (.github/workflows/ci.yml, rust job) so `make test` runs them too.
test-rust:  ## cargo test --workspace (+ roost-vt ffi tests, cfg-gated out of the default run)
	cargo test --workspace
	cargo test -p roost-vt --features ffi

test-iced:  ## Iced unit tests (renderer + input + adapter)
	cargo test -p roost-iced

test-mac:  ## swift test (Mac)
	cd $(MAC_DIR) && swift test

test-harness:  ## Fast unit tests for target/path/capability harness wiring
	python3 -m unittest discover -s tools/roosttest_unit -v

# `e2e` dispatches rather than running a bare full-dir suite: the full
# `tools/roosttest` dir includes `test_exit_on_empty.py` / `test_menu_quit.py`,
# which end the UI they drive (plan 026 D8 / plan 028 C3) — under the iced
# default that would strand every module the session-scoped harness fixture
# runs after them. `e2e-iced`'s `ICED_E2E_TESTS` curated list (below) is the
# safe iced lane; `mac` still runs the full directory (nothing in it ends the
# Mac app).
e2e:  ## pytest E2E suite dispatch (ROOST_TARGET=mac|iced, default iced) -> e2e-mac or the curated e2e-iced lane
	@case "$${ROOST_TARGET:-iced}" in \
		mac) $(MAKE) e2e-mac ;; \
		iced) $(MAKE) e2e-iced ;; \
		*) echo "ROOST_TARGET=$${ROOST_TARGET} is not supported by 'make e2e' (want mac or iced)"; exit 1 ;; \
	esac

e2e-iced:  ## Required functional E2E against Iced
	@tests='$(ICED_E2E_TESTS)'; \
	if [ -z "$${WAYLAND_DISPLAY:-}" ]; then tests="$$tests $(ICED_CLIPBOARD_TESTS)"; \
	else echo "Iced/Wayland clipboard requires a focused seat/serial; running the documented non-clipboard renderer gate"; fi; \
	uv run --group test pytest $$tests --roost-target iced

e2e-iced-exit:  ## Iced exit-on-empty E2E in its own lane (DESTRUCTIVE: force-quits a running Iced UI, and the UI it launches exits)
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_EXIT_E2E_TESTS) --roost-target iced --roost-fresh

e2e-iced-menu-quit:  ## Iced menu-Quit E2E in its own lane (DESTRUCTIVE: force-quits a running Iced UI, and the UI it launches exits via the menu). macOS-iced-only; self-skips elsewhere.
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_MENU_QUIT_E2E_TESTS) --roost-target iced --roost-fresh

e2e-iced-clipboard:  ## Native Iced clipboard/OSC E2E (macOS or Linux X11; not headless Wayland)
	uv run --group test pytest $(ICED_CLIPBOARD_TESTS) --roost-target iced

e2e-mac:  ## E2E against the Mac app
	uv run --group test pytest tools/roosttest --roost-target mac

e2e-iced-ci:  ## Required Iced functional E2E at CI parity (fresh + isolated state)
	@tests='$(ICED_E2E_TESTS)'; \
	if [ -z "$${WAYLAND_DISPLAY:-}" ]; then tests="$$tests $(ICED_CLIPBOARD_TESTS)"; \
	else echo "Iced/Wayland clipboard requires a focused seat/serial; running the documented non-clipboard renderer gate"; fi; \
	ROOST_TEST_MODE=1 uv run --group test pytest $$tests --roost-target iced --roost-fresh

e2e-iced-release-ci:  ## Release-profile Iced E2E gate: curated subset against a real release binary (ROOST_ICED_BIN required)
	@test -n "$$ROOST_ICED_BIN" || \
		( echo "ROOST_ICED_BIN is not set: tools/roosttest/ui.py falls back to target/debug/<binary> and will silently cargo build a debug binary, so this gate would test a debug build while claiming to test release. Set ROOST_ICED_BIN to the release binary."; exit 1 )
	@for f in $(ICED_RELEASE_E2E_TESTS); do \
		test -f "$$f" || { echo "missing release E2E test file: $$f (ICED_RELEASE_E2E_TESTS is stale)"; exit 1; }; \
	done
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_RELEASE_E2E_TESTS) --roost-target iced --roost-fresh

e2e-mac-ci:  ## Mac E2E at CI parity. DESTRUCTIVE: force-quits any running Roost.app
	ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest --roost-target mac --roost-fresh

e2e-iced-bundle:  ## macOS-only: assemble Roost-Iced.app + run the curated bundle smoke against it (ROOST_ICED_APP)
	@[ "$$(uname -s)" = "Darwin" ] || { echo "e2e-iced-bundle is macOS-only: it launches Roost-Iced.app via LaunchServices (open)"; exit 1; }
	$(MAKE) bundle-iced
	ROOST_ICED_APP=mac/build/Roost-Iced.app ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest/test_smoke.py tools/roosttest/test_iced_walking_skeleton.py tools/roosttest/test_menu_bar.py --roost-target iced --roost-fresh

e2e-iced-sparkle:  ## macOS-only: assemble a TEST-KEYED Roost-Iced.app + run the Sparkle E2E against a loopback appcast
	@[ "$$(uname -s)" = "Darwin" ] || { echo "e2e-iced-sparkle is macOS-only: it launches Roost-Iced.app via LaunchServices (open)"; exit 1; }
	@test -f $(SPARKLE_TEST_PUBLIC_KEY) || { echo "missing $(SPARKLE_TEST_PUBLIC_KEY)"; exit 1; }
	ROOST_ICED_SPARKLE_FEED_URL=$(SPARKLE_TEST_PLACEHOLDER_FEED) \
	ROOST_ICED_SPARKLE_ED_PUBLIC_KEY="$$(cat $(SPARKLE_TEST_PUBLIC_KEY))" \
		$(MAKE) bundle-iced
	ROOST_ICED_APP=mac/build/Roost-Iced.app ROOST_TEST_MODE=1 \
		uv run --group test pytest tools/roosttest/test_sparkle.py --roost-target iced --roost-fresh

smoke-iced:  ## Screenshot-driven UI smoke against a running Iced UI
	tools/screenshot/smoke.sh iced

smoke-mac:  ## Screenshot-driven UI smoke against a running Mac app
	tools/screenshot/smoke.sh mac

visual-parity:  ## DESTRUCTIVE: close live target UIs, then capture a hermetic comparison fixture
	python3 tools/screenshot/parity.py

smoke-mac-launch:  ## Clean-install launch check (bundles Roost.app, hides build-tree resources, asserts it starts)
	./mac/scripts/bundle.sh debug
	./mac/scripts/smoke-launch.sh

test-iced-real-input: build-iced  ## Iced real clipboard input (self-contained Linux Xvfb+xdotool)
	ROOST_REQUIRE_REAL_INPUT=1 uv run --group test python tools/input/linux/iced_clipboard_check.py

test-iced-wayland-input: build-iced  ## Iced system clipboard with cage + a real uinput seat
	ROOST_REQUIRE_REAL_INPUT=1 uv run --group test python tools/input/linux/iced_wayland_clipboard_check.py

perf-refresh:  ## In-crate refresh_snapshot perf harness (release, --ignored; see tools/perf/README.md)
	cargo test -p roost-iced --release -- --ignored --nocapture

perf-render-stats:  ## Render-path counters for a running Iced UI (tools/perf/render-stats.sh iced <duration>; args for other targets)
	tools/perf/render-stats.sh iced

# ---- code quality -----------------------------------------------------

.PHONY: fmt fmt-check clippy themes-check check
fmt:  ## Format Rust (cargo fmt --all)
	cargo fmt --all

fmt-check:  ## Check formatting (what CI's rust-lint runs)
	cargo fmt --all -- --check

clippy:  ## Lint Rust at CI parity (warnings are errors)
	# `-D warnings` matches the `rust-lint` CI job. Without it `make check`
	# passed while CI failed, which is worse than no local gate at all.
	cargo clippy --workspace --all-targets -- -D warnings

# `linux-package` is off in every dev build, so without the second test +
# clippy pair the packaging configuration would compile for the first time
# during a release build.
check-iced: fmt-check test-iced  ## Iced formatting, lint, tests, and dependency boundaries
	cargo clippy -p roost-iced --all-targets -- -D warnings
	cargo test -p roost-iced --features linux-package
	cargo clippy -p roost-iced --features linux-package --all-targets -- -D warnings
	@! cargo tree -p roost-iced | grep -E '(^| )(gtk4|libadwaita|pango|cairo-rs) v' || \
		( echo "roost-iced has a forbidden GTK dependency"; exit 1 )
	@! cargo tree -p roost-engine | grep -E '(^| )(gtk4|libadwaita|iced|notify-rust|zbus|arboard) v' || \
		( echo "roost-engine has a UI toolkit dependency"; exit 1 )
	@cargo tree -p roost-iced | grep -q 'swash v0.2.10 (.*third_party/swash)' || \
		( echo "swash [patch.crates-io] not applied"; exit 1 )

themes-check:  ## Assert the Rust + Mac bundled-theme copies are byte-identical
	diff -r crates/roost-ui-model/src/resources/themes mac/Sources/Roost/Resources/themes

check: fmt-check clippy themes-check test  ## Pre-push gate: fmt-check + clippy + themes-check + tests

# ---- docs -------------------------------------------------------------

.PHONY: docs docs-serve
docs:  ## Build the docs site into site-build/ (same as CI)
	uv sync --locked --group docs && uv run --locked zensical build --strict

docs-serve:  ## Serve the docs locally with live reload
	uv sync --locked --group docs && uv run --locked zensical serve

# ---- clean ------------------------------------------------------------

.PHONY: clean
clean:  ## Remove build artifacts (cargo target, Roost.app, site-build)
	cargo clean
	rm -rf $(MAC_DIR)/build site-build
