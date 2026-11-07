# Roost — common dev tasks. Run `make` (or `make help`) to list them.
#
# One desktop UI around libghostty-vt: Rust + iced (crates/roost-iced), the
# Linux package and the macOS Roost.app (packaged by mac/scripts), plus the
# roostctl CLI and the roost-session daemon. See docs/development/vision.md
# for the architecture + north star.

.DEFAULT_GOAL := help

MAC_DIR     := mac
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
# `make build` / `make bundle-iced` just works.
$(GHOSTTY_LIB):
	third_party/ghostty/build.sh

# ---- build ------------------------------------------------------------

.PHONY: build build-iced bundle-iced build-all
build: $(GHOSTTY_LIB)  ## cargo build the workspace (Iced UI + roostctl)
	cargo build

build-iced: $(GHOSTTY_LIB)  ## Build the isolated Iced UI binary
	cargo build -p roost-iced

bundle-iced: $(GHOSTTY_LIB)  ## Build + assemble the dev Roost-Iced.app (debug; macOS)
	cd $(MAC_DIR) && ./scripts/bundle-iced.sh debug

build-all: build bundle-iced  ## Build the workspace + the dev macOS bundle

# ---- run --------------------------------------------------------------

.PHONY: run-iced
run-iced: build-iced  ## Launch the Iced UI (Roost-iced profile)
	ROOST_BUNDLE_PROFILE=iced ./target/debug/roost-iced

# ---- test -------------------------------------------------------------

.PHONY: test test-rust which-runner test-iced test-harness test-linux-scripts test-mac-scripts e2e e2e-iced e2e-iced-exit e2e-iced-renderer e2e-iced-menu-quit e2e-iced-clipboard e2e-session e2e-host-client e2e-host-client-ci e2e-host-ssh e2e-host-ssh-ci e2e-host-missing-daemon e2e-host-missing-daemon-ci e2e-host-local-spawn e2e-host-local-spawn-ci e2e-host-localhost e2e-host-localhost-ci e2e-local-backend e2e-local-backend-ci e2e-old-session e2e-host-bootstrap e2e-host-bootstrap-ci e2e-iced-ci e2e-iced-release-ci e2e-iced-bundle e2e-iced-sparkle e2e-iced-real-input-mac mac-real-input-check smoke-iced test-iced-real-input test-iced-wayland-input check-iced perf-refresh perf-render-stats

ICED_E2E_TESTS := tools/roosttest/test_smoke.py tools/roosttest/test_iced_walking_skeleton.py tools/roosttest/test_notifications.py tools/roosttest/test_notification_raise.py tools/roosttest/test_agent_lifecycle.py tools/roosttest/test_agent_hooks.py tools/roosttest/test_agent_hooks_dialog.py tools/roosttest/test_agent_palette.py tools/roosttest/test_agent_panel.py tools/roosttest/test_doctor.py tools/roosttest/test_provider.py tools/roosttest/test_sidebar_pixels.py tools/roosttest/test_tab_strip_pixels.py tools/roosttest/test_sidebar_agents.py tools/roosttest/test_focus.py tools/roosttest/test_palette.py tools/roosttest/test_z_typography.py tools/roosttest/test_project_lifecycle.py tools/roosttest/test_sidebar_resize.py tools/roosttest/test_window_geometry_persistence.py tools/roosttest/test_osc_pipeline.py tools/roosttest/test_palette_256.py tools/roosttest/test_sprite_pixels.py tools/roosttest/test_cursor_pixels.py tools/roosttest/test_kitty_pixels.py tools/roosttest/test_kitty_images.py tools/roosttest/test_ime.py tools/roosttest/test_selection.py tools/roosttest/test_mouse_tracking.py tools/roosttest/test_tab_dump_scrollback.py tools/roosttest/test_dock_badge.py tools/roosttest/test_menu_bar.py tools/roosttest/test_sparkle.py tools/roosttest/test_view_perf.py tools/roosttest/test_shell_integration.py tools/roosttest/test_newtab_cwd.py tools/roosttest/test_boot_failure.py tools/roosttest/test_quit_with_foreground_job.py tools/roosttest/test_open_external.py tools/roosttest/test_context_menu.py tools/roosttest/test_password_input.py tools/roosttest/test_secure_input.py tools/roosttest/test_device_queries.py tools/roosttest/test_selection_autoscroll.py tools/roosttest/test_program_status.py tools/roosttest/test_memory_usage.py tools/roosttest/test_tab_env.py
# Curated: a new UI-socket module runs nowhere until it is listed here AND in
# ci.yml's three functional steps (X11, Wayland, macOS) — which
# tools/roosttest_unit/test_e2e_lists.py enforces, along with every module on
# disk being in some lane.
# `test_sparkle.py`'s two classes split by lane: the bare-binary class
# runs here (no framework beside a cargo binary ⇒ updater unavailable)
# and its bundle class self-skips — `make e2e-iced-sparkle` is where the
# latter runs, against a bundle assembled with the fixture's TEST-ONLY
# public key.
# `test_dock_badge.py` and `test_menu_bar.py` self-skip unless the host is
# macOS AND the target is iced (the native menu bar + Dock badge are both
# macOS-iced-only seams — plan 027 § 6b / plan 028 § 6d), so they cost a
# skip line on the Linux lanes and run for real on the macOS ones.
# `test_boot_failure.py` and `test_quit_with_foreground_job.py` are the
# reverse: Linux-only, they self-skip on macOS. Each launches its own UI
# (one whose boot must fail, one it SIGTERMs) beside the session's, so they
# share the list without touching the shared instance.
# `selection.*` reads UI state over IPC and never touches the host
# pasteboard, so `test_selection.py` belongs in the list above and runs
# under headless Wayland too. Only files that read/write the real
# clipboard go here.
# The tiny-skia cells' whole lane (D8): every module of ICED_E2E_TESTS that
# reads ICED_BACKEND or checks pixels. The full list runs on wgpu only.
ICED_RENDERER_E2E_TESTS := tools/roosttest/test_smoke.py tools/roosttest/test_iced_walking_skeleton.py tools/roosttest/test_sidebar_pixels.py tools/roosttest/test_tab_strip_pixels.py tools/roosttest/test_agent_panel.py tools/roosttest/test_sprite_pixels.py tools/roosttest/test_cursor_pixels.py tools/roosttest/test_kitty_pixels.py tools/roosttest/test_kitty_images.py tools/roosttest/test_ime.py
ICED_CLIPBOARD_TESTS := tools/roosttest/test_osc52.py tools/roosttest/test_paste_files.py tools/roosttest/test_context_menu_clipboard.py
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
# The headless host-session lane. Deliberately NOT in ICED_E2E_TESTS: it
# drives no UI at all — it spawns `roost-session` daemons against
# throwaway profiles — so it needs neither a display nor the shared
# UI-session fixture, and it gets its own target + CI job.
#
# `test_ssh_transport.py` is a member for the same reasons: it drives a
# daemon plus the far side of the SSH transport (`roost-session
# client-bridge`, spawned by the test itself), and no UI at all.
#
# `test_password_input.py`, `test_program_status.py` and `test_memory_usage.py`
# are members of BOTH this list and ICED_E2E_TESTS: their `session_daemon`
# cases spawn a daemon and their unmarked ones drive the harness UI. The
# marker splits them — `e2e-session` selects `-m session_daemon`, and the
# iced lanes deselect the daemon markers (DAEMON_E2E_DESELECT).
SESSION_E2E_TESTS := tools/roosttest/test_session.py tools/roosttest/test_session_env.py tools/roosttest/test_session_attach.py tools/roosttest/test_session_ownership.py tools/roosttest/test_session_effects.py tools/roosttest/test_ssh_transport.py tools/roosttest/test_password_input.py tools/roosttest/test_program_status.py tools/roosttest/test_memory_usage.py
# The HS-2 host-client lane (plan 037 C9). It drives BOTH a `roost-session`
# daemon and the harness UI, so it is neither a member of SESSION_E2E_TESTS
# (which runs headless and would stand the UI down) nor of ICED_E2E_TESTS
# (whose targets build no `roost-session`, so the first case would
# `cargo build` mid-test). Own list, own target, own CI steps — the same
# arrangement `e2e-session` has, and for the same reason: a daemon lifecycle
# the shared UI lane knows nothing about.
HOST_CLIENT_E2E_TESTS := tools/roosttest/test_host_client.py tools/roosttest/test_host_ownership.py
# HS-3's UI-side ssh lane (plan 038 C5): the same two processes as
# HOST_CLIENT_E2E_TESTS plus a real `SshTunnel` between them, over the
# `fake-ssh.sh` fixture. Marked `host_client` (it needs both processes,
# and the same deselects apply) but kept in its own list and its own
# target: it points the UI's `$ROOST_SSH_BIN` at a wrapper of its own,
# and the incarnation probe both lanes use cannot tell two connected
# hosts' tabs apart — so these two must never run at the same time.
SSH_HOST_E2E_TESTS := tools/roosttest/test_host_ssh.py
# Plan 042 W2's local-transport twin of SSH_HOST_E2E_TESTS: no tunnel, no
# fake `ssh` — it sets `$ROOST_SESSION_BIN` to a path that does not exist
# at import, so `localhost`'s own spawn attempt settles disconnected
# instead of ever dialing a socket. Marked `host_client` for the same
# deselect reasons and kept in its own list/target for the same one: the
# override poisons every `roost-session` launch the harness UI would
# otherwise attempt, so it must never run beside SSH_HOST_E2E_TESTS,
# HOST_CLIENT_E2E_TESTS, or BOOTSTRAP_E2E_TESTS.
MISSING_DAEMON_E2E_TESTS := tools/roosttest/test_host_local_missing_daemon.py
# Plan 044 W2's positive twin of MISSING_DAEMON_E2E_TESTS (#397): the
# `localhost` spawn that *works*, from a UI running under the harness's
# throwaway `ROOST_STATE_DIR`. Marked `host_client` for the same deselect
# reasons, and kept in its own list/target for a stronger one than the
# other host lanes have: it puts a real daemon on this build's DEFAULT
# session socket — the socket every other host lane's UI would dial
# instead of spawning or settling, and the one `roostctl session
# status|stop` address — so it demands that socket free before it runs
# and stops what it started afterwards. Never beside HOST_CLIENT_E2E_TESTS,
# SSH_HOST_E2E_TESTS, MISSING_DAEMON_E2E_TESTS or BOOTSTRAP_E2E_TESTS.
LOCAL_SPAWN_E2E_TESTS := tools/roosttest/test_host_local_spawn.py
# Plan 058 §3.2's `localhost` **sentinel** lane (#460): the one host lane
# whose target is the literal string `localhost` rather than a socket
# path, so it is the only cover for anything gated on
# `HostTransportKind::Localhost` — the ↻ Restart verb and the restart
# behind it. Marked `host_client` for the same deselect reasons, and kept
# in its own list/target for two: it points `XDG_RUNTIME_DIR` at a private
# root **at import**, which poisons every module imported after it in the
# same pytest process (MISSING_DAEMON_E2E_TESTS' own reason), and it
# shares the incarnation probe that cannot tell two connected hosts' tabs
# apart. The redirect is also what makes it the one host lane that owns no
# machine-wide path: its sentinel, and the UI that dials it, are both
# inside that root.
LOCALHOST_E2E_TESTS := tools/roosttest/test_host_localhost.py
# Plan 063 §D8's local-backend switch lane (#426). Marked `host_client`
# for the same deselect reasons, and kept in its own list/target for
# LOCALHOST_E2E_TESTS' two plus one of its own: it redirects
# `XDG_RUNTIME_DIR` at import (poisoning every module imported after it),
# it owns the sentinel socket the switch's destination is, and — the new
# one — it **relaunches the UI under a different `local-backend`** for
# most of its cases. Every other lane is pinned to `in-process` by
# `fixtures/launcher.conf`; a lane sharing a pytest process with this one
# would find the UI on a backend it never asked for.
LOCAL_BACKEND_E2E_TESTS := tools/roosttest/test_local_backend.py
# Plan 072 D13's old-session lane (#537): this tree's UI against a released
# `roost-session` that is already running when it starts. Marked
# `host_client` for the same deselect reasons, and its own invocation for
# LOCAL_BACKEND_E2E_TESTS' first: it redirects `XDG_RUNTIME_DIR` at import.
# The releases come from `tools/roosttest/old_session/fetch.sh`.
OLD_SESSION_E2E_TESTS := tools/roosttest/test_old_session.py
# HS-3 slice 2's UI-side bootstrap lane (plan 039 C6): the same shape as
# SSH_HOST_E2E_TESTS — `fake-ssh.sh`, this time in `run-remote` mode, so
# the generated probe/install/start scripts really run — plus a loopback
# HTTP server standing in for the release asset. Marked `host_client` for
# the same reason and kept in its own list/target for the same one:
# never beside SSH_HOST_E2E_TESTS or HOST_CLIENT_E2E_TESTS, since all
# three share the incarnation probe that cannot tell two connected hosts'
# tabs apart.
BOOTSTRAP_E2E_TESTS := tools/roosttest/test_host_bootstrap.py
# Every shared-UI pytest run must deselect BOTH daemon lanes: those runs
# drive a UI, nothing in them builds `roost-session`, and the first session
# or host-client case would therefore `cargo build` mid-test on a shared
# runner.
#
# Expressed as MARKERS, not a path list, so the exclusion has one source of
# truth — the same `session_daemon` marker `conftest.py` keys its "does this
# invocation need a UI" decision off, plus `host_client`. A module that
# lands in a curated list by mistake is still kept out of the shared lane.
#
# The curated iced lists (`e2e-iced`, `e2e-iced-ci`) carry it for the one
# module with cases in both lanes (`test_password_input.py`).
#
# MIRRORS (keep the expression identical, move together):
#   * .github/workflows/ci.yml       — the three iced functional cells
#                                      (X11, Wayland, macOS), and the macOS
#                                      bundle / sparkle cells (which
#                                      deselect only `host_client`)
#   * .github/workflows/release.yml  — "E2E against the release bundle"
# Neither can reference a Makefile variable, so they restate the flag —
# the same hand-mirrored-constant arrangement `smoke-deb`'s `expect_exec`
# list lives with.
DAEMON_E2E_DESELECT := -m 'not session_daemon and not host_client'
# The Sparkle lane's bundle inputs. The feed URL is a deliberate dead
# placeholder: the seam's test-mode delegate override replaces it with
# the live loopback port at check time, and a plist URL that could never
# resolve means a stray launch of this bundle can't reach anything.
SPARKLE_TEST_PUBLIC_KEY := tools/roosttest/fixtures/sparkle/TEST-ONLY-public-ed-key.txt
SPARKLE_TEST_PLACEHOLDER_FEED := http://127.0.0.1:1/placeholder
test: test-rust test-harness test-linux-scripts test-mac-scripts  ## All unit/integration tests (Rust + harness + release scripts)

# `cargo test` runs this workspace's 78 test binaries strictly one after
# another, and the suite is mostly *asleep* — PTY children, timeout budgets,
# bounded waits. Measured on a 32-core box with everything already built:
# 121s at 9% of one core. `cargo nextest` schedules across binaries and
# finishes the same 3029 tests in 31s, which is the floor set by the single
# slowest test rather than by CPU.
#
# Optional on purpose, so no machine is broken by its absence: nextest is not
# in mise's registry, so a checkout without it runs the identical binaries
# through `cargo test`. The two are equivalent in coverage here — the one
# thing nextest skips is doctests, and this workspace has zero of them
# (`cargo test --workspace --doc` reports 0 across every crate). Install with
# `cargo install cargo-nextest` or the binary from https://nexte.st.
# Serialisation that process isolation would otherwise lose is restored in
# `.config/nextest.toml`; read that before trusting a parallel run.
RUST_TEST := $(if $(shell command -v cargo-nextest 2>/dev/null),cargo nextest run,cargo test)
# Nextest cannot run doctests at all. There are none today, so this is a
# no-op — but "none today" is not an invariant anyone enforces, and without
# this line the first doctest anyone writes would pass locally by being
# skipped and then run for real in CI. `cargo test` already covers them, so
# the step is only added for the nextest path.
RUST_DOCTEST := $(if $(shell command -v cargo-nextest 2>/dev/null),cargo test --workspace --doc,true)

# roost-vt's tests/*.rs all start with `#![cfg(feature = "ffi")]`, so the
# `--workspace` run compiles and then silently skips every one of them. The
# second line mirrors CI's separate `cargo test -p roost-vt --features ffi`
# step (.github/workflows/ci.yml, rust job) so `make test` runs them too.
test-rust:  ## Workspace tests (+ roost-vt ffi and roost-engine server-vt, cfg-gated out of the default run). Uses cargo-nextest when installed.
	$(RUST_TEST) --workspace
	$(RUST_DOCTEST)
	$(RUST_TEST) -p roost-vt --features ffi
	$(RUST_TEST) -p roost-engine --features server-vt

which-runner:  ## Say which test runner `make test-rust` will use, and why it matters
	@echo "$(RUST_TEST)"
	@command -v cargo-nextest >/dev/null 2>&1 || \
		echo "  (cargo-nextest not installed — the suite runs ~4x slower; see https://nexte.st)"

test-iced:  ## Iced unit tests (renderer + input + adapter)
	cargo test -p roost-iced

test-harness:  ## Fast unit tests for target/path/capability harness wiring
	python3 -m unittest discover -s tools/roosttest_unit -v

test-linux-scripts:  ## Shell tests for linux/scripts/*.sh release-artifact + CI helpers (no cargo/deb build needed)
	./linux/scripts/stage-session-artifact_test.sh
	./linux/scripts/ci-apt-install_test.sh

test-mac-scripts:  ## Shell tests for mac/scripts release helpers: notarize.sh and verify-sparkle-key.sh against fakes (no macOS needed; the key test needs uv)
	./mac/scripts/notarize_test.sh
	./mac/scripts/verify-sparkle-key_test.sh

# `e2e` runs the curated lane rather than a bare full-dir suite: the full
# `tools/roosttest` dir includes `test_exit_on_empty.py` / `test_menu_quit.py`,
# which end the UI they drive (plan 026 D8 / plan 028 C3), and would strand
# every module the session-scoped harness fixture runs after them.
e2e: e2e-iced  ## pytest E2E suite: the curated e2e-iced lane

# `roostctl` is built first because `tools/roosttest/util.py` only builds
# one when the binary is *missing*: without this the lane either charges
# the first test's timeout for a cold cargo build, or — worse — silently
# runs whatever stale binary is already in target/debug.
e2e-iced:  ## Required functional E2E against Iced
	cargo build -p roost-cli
	@tests='$(ICED_E2E_TESTS)'; \
	if [ -z "$${WAYLAND_DISPLAY:-}" ]; then tests="$$tests $(ICED_CLIPBOARD_TESTS)"; \
	else echo "Iced/Wayland clipboard requires a focused seat/serial; running the documented non-clipboard renderer gate"; fi; \
	ROOST_TEST_MODE=1 uv run --group test pytest $$tests $(DAEMON_E2E_DESELECT) --roost-target iced

e2e-iced-renderer:  ## Renderer-sensitive subset of the Iced E2E (fresh + isolated state); set ICED_BACKEND=tiny-skia|wgpu
	cargo build -p roost-cli
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_RENDERER_E2E_TESTS) $(DAEMON_E2E_DESELECT) --roost-target iced --roost-fresh

e2e-iced-exit:  ## Iced exit-on-empty E2E in its own lane (DESTRUCTIVE: force-quits a running Iced UI, and the UI it launches exits)
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_EXIT_E2E_TESTS) --roost-target iced --roost-fresh

e2e-iced-menu-quit:  ## Iced menu-Quit E2E in its own lane (DESTRUCTIVE: force-quits a running Iced UI, and the UI it launches exits via the menu). macOS-iced-only; self-skips elsewhere.
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_MENU_QUIT_E2E_TESTS) --roost-target iced --roost-fresh

e2e-iced-clipboard:  ## Native Iced clipboard/OSC E2E (macOS or Linux X11; not headless Wayland)
	uv run --group test pytest $(ICED_CLIPBOARD_TESTS) --roost-target iced

# No display — the lane spawns daemons rather than windows — but it does
# need libghostty-vt: `roost-session` enables `roost-engine/server-vt`,
# which pulls `roost-vt/ffi`, so the archive is a build input. The
# build.sh call is idempotent and cached (it no-ops once
# third_party/ghostty/out exists). Both binaries are built first because
# the harness would otherwise `cargo build` mid-test and charge the first
# case for it.
e2e-session:  ## Headless host-session E2E (spawns roost-session daemons; no UI)
	./third_party/ghostty/build.sh
	cargo build -p roost-session -p roost-cli
	uv run --group test pytest -m session_daemon $(SESSION_E2E_TESTS)

# Both processes, and the wire between them. Needs everything `e2e-session`
# needs (the daemon + the archive it links) AND a UI, so it builds all three
# binaries up front — the harness would otherwise cargo-build one mid-test.
# ROOST_TEST_MODE is not optional here: the lane reads what the UI queued
# toward a host (`tab.capture_pty_input`), and without it every case skips.
e2e-host-client: $(GHOSTTY_LIB)  ## HS-2 host-client E2E (a roost-session daemon beside the Iced UI)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(HOST_CLIENT_E2E_TESTS) --roost-target iced

e2e-host-client-ci: $(GHOSTTY_LIB)  ## HS-2 host-client E2E at CI parity. DESTRUCTIVE: force-quits a running Iced UI
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(HOST_CLIENT_E2E_TESTS) --roost-target iced --roost-fresh

# The same three binaries `e2e-host-client` needs — the UI dials the
# session through a tunnel it runs itself, and the far side of that
# tunnel is the `roost-session` binary. Never run beside
# `e2e-host-client`: see SSH_HOST_E2E_TESTS.
e2e-host-ssh: $(GHOSTTY_LIB)  ## HS-3 SSH-transport E2E (the Iced UI reaching a session over a fake ssh)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(SSH_HOST_E2E_TESTS) --roost-target iced

e2e-host-ssh-ci: $(GHOSTTY_LIB)  ## HS-3 SSH-transport E2E at CI parity. DESTRUCTIVE: force-quits a running Iced UI
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(SSH_HOST_E2E_TESTS) --roost-target iced --roost-fresh

# The same three binaries `e2e-host-ssh` needs — a stale build passes this
# lane vacuously (an old binary that never gets a fresh `ROOST_SESSION_BIN`
# override is not what settled the last time it ran). Never beside
# e2e-host-client, e2e-host-ssh, or e2e-host-bootstrap: see
# MISSING_DAEMON_E2E_TESTS.
e2e-host-missing-daemon: $(GHOSTTY_LIB)  ## Plan 042 W2 local-daemon-missing E2E (an explicit Connect settles once, no retry)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(MISSING_DAEMON_E2E_TESTS) --roost-target iced

e2e-host-missing-daemon-ci: $(GHOSTTY_LIB)  ## Plan 042 W2 local-daemon-missing E2E at CI parity. DESTRUCTIVE: force-quits a running Iced UI
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(MISSING_DAEMON_E2E_TESTS) --roost-target iced --roost-fresh

# The same three binaries the missing-daemon lane needs — here the UI
# really does spawn the sibling `roost-session`, so a stale one is the
# daemon under test. Never beside another host lane, and it needs this
# build's default session socket free: see LOCAL_SPAWN_E2E_TESTS. Either
# target asserts for real as long as the harness owns the UI — which,
# without `--roost-fresh`, means only that no UI is already running; a
# developer's live instance is reused instead and the lane then skips by
# name, because the state dir of a UI the harness did not launch is not
# knowable from outside. `-ci` is the shape that always asserts.
e2e-host-local-spawn: $(GHOSTTY_LIB)  ## Plan 044 W2 local-daemon-spawn E2E (#397: a spawned session gets <ROOST_STATE_DIR>/session)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(LOCAL_SPAWN_E2E_TESTS) --roost-target iced

e2e-host-local-spawn-ci: $(GHOSTTY_LIB)  ## Plan 044 W2 local-daemon-spawn E2E at CI parity. DESTRUCTIVE: force-quits a running Iced UI
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(LOCAL_SPAWN_E2E_TESTS) --roost-target iced --roost-fresh

# The same three binaries the local-spawn lane needs — this one connects
# to the daemon by the `localhost` sentinel rather than by a path, and the
# restart it drives relaunches that daemon out of the UI's environment, so
# a stale `roost-session` is the binary under test twice over. Never
# beside another host lane: see LOCALHOST_E2E_TESTS.
e2e-host-localhost: $(GHOSTTY_LIB)  ## Plan 058 localhost-sentinel E2E (#460: `host.add localhost`, its ↻ Restart verb, and a killed session restarted on it)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(LOCALHOST_E2E_TESTS) --roost-target iced

e2e-host-localhost-ci: $(GHOSTTY_LIB)  ## Plan 058 localhost-sentinel E2E at CI parity (nothing to force-quit: its XDG_RUNTIME_DIR is private)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(LOCALHOST_E2E_TESTS) --roost-target iced --roost-fresh

# The same three binaries the localhost lane needs, for the same reason:
# the switch's destination is a real `roost-session` on this build's
# session socket, spawned by the UI under test. Never beside another host
# lane — see LOCAL_BACKEND_E2E_TESTS.
e2e-local-backend: $(GHOSTTY_LIB)  ## Plan 063 §D8 local-backend switch E2E (#426: both directions, the journal, and the quiescence latch)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(LOCAL_BACKEND_E2E_TESTS) --roost-target iced

e2e-local-backend-ci: $(GHOSTTY_LIB)  ## Plan 063 §D8 local-backend switch E2E at CI parity (nothing to force-quit: its XDG_RUNTIME_DIR is private)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(LOCAL_BACKEND_E2E_TESTS) --roost-target iced --roost-fresh

# The UI and `roostctl` under test, and this tree's `roost-session`: the
# forced-update check restarts a released session into it.
e2e-old-session: $(GHOSTTY_LIB)  ## Plan 072 D13 old-session E2E (#537: today's UI against released sessions; nothing to force-quit: its XDG_RUNTIME_DIR is private)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(OLD_SESSION_E2E_TESTS) --roost-target iced --roost-fresh

# The same three binaries `e2e-host-ssh` needs — the bootstrap job runs
# the same fake-`ssh`-fronted transport, this time in `run-remote` mode.
# Never run beside e2e-host-client or e2e-host-ssh: see BOOTSTRAP_E2E_TESTS.
e2e-host-bootstrap: $(GHOSTTY_LIB)  ## HS-3 slice-2 bootstrap E2E (the Iced UI installing/starting roost-session over a fake ssh)
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(BOOTSTRAP_E2E_TESTS) --roost-target iced

e2e-host-bootstrap-ci: $(GHOSTTY_LIB)  ## HS-3 slice-2 bootstrap E2E at CI parity. DESTRUCTIVE: force-quits a running Iced UI
	cargo build -p roost-iced -p roost-cli -p roost-session
	ROOST_TEST_MODE=1 uv run --group test pytest $(BOOTSTRAP_E2E_TESTS) --roost-target iced --roost-fresh

e2e-iced-ci:  ## Required Iced functional E2E at CI parity (fresh + isolated state)
	cargo build -p roost-cli
	@tests='$(ICED_E2E_TESTS)'; \
	if [ -z "$${WAYLAND_DISPLAY:-}" ]; then tests="$$tests $(ICED_CLIPBOARD_TESTS)"; \
	else echo "Iced/Wayland clipboard requires a focused seat/serial; running the documented non-clipboard renderer gate"; fi; \
	ROOST_TEST_MODE=1 uv run --group test pytest $$tests $(DAEMON_E2E_DESELECT) --roost-target iced --roost-fresh

e2e-iced-release-ci:  ## Release-profile Iced E2E gate: curated subset against a real release binary (ROOST_ICED_BIN required)
	@test -n "$$ROOST_ICED_BIN" || \
		( echo "ROOST_ICED_BIN is not set: tools/roosttest/ui.py falls back to target/debug/<binary> and will silently cargo build a debug binary, so this gate would test a debug build while claiming to test release. Set ROOST_ICED_BIN to the release binary."; exit 1 )
	@for f in $(ICED_RELEASE_E2E_TESTS); do \
		test -f "$$f" || { echo "missing release E2E test file: $$f (ICED_RELEASE_E2E_TESTS is stale)"; exit 1; }; \
	done
	ROOST_TEST_MODE=1 uv run --group test pytest $(ICED_RELEASE_E2E_TESTS) --roost-target iced --roost-fresh

# ICED_APP names the bundle under test. The default dev Roost-Iced.app is
# assembled first; any other bundle is driven as already built, e.g. a
# production one: `make e2e-iced-bundle ICED_APP=mac/build/Roost.app
# ROOST_TEST_ALLOW_PRODUCTION_IDENTITY=1` (outside CI the harness refuses
# that identity without the opt-in — tools/roosttest/README.md).
ICED_APP ?= mac/build/Roost-Iced.app
e2e-iced-bundle:  ## macOS-only: run the curated bundle smoke against $(ICED_APP) (default: assemble the dev Roost-Iced.app first)
	@[ "$$(uname -s)" = "Darwin" ] || { echo "e2e-iced-bundle is macOS-only: it launches the bundle via LaunchServices (open)"; exit 1; }
	@if [ "$(ICED_APP)" = "mac/build/Roost-Iced.app" ]; then $(MAKE) bundle-iced; fi
	ROOST_ICED_APP="$(ICED_APP)" ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest/test_smoke.py tools/roosttest/test_iced_walking_skeleton.py tools/roosttest/test_menu_bar.py --roost-target iced --roost-fresh

e2e-iced-sparkle:  ## macOS-only: assemble a TEST-KEYED Roost-Iced.app + run the Sparkle E2E against a loopback appcast
	@[ "$$(uname -s)" = "Darwin" ] || { echo "e2e-iced-sparkle is macOS-only: it launches Roost-Iced.app via LaunchServices (open)"; exit 1; }
	@test -f $(SPARKLE_TEST_PUBLIC_KEY) || { echo "missing $(SPARKLE_TEST_PUBLIC_KEY)"; exit 1; }
	ROOST_BUNDLE_SPARKLE_FEED_URL=$(SPARKLE_TEST_PLACEHOLDER_FEED) \
	ROOST_BUNDLE_SPARKLE_ED_PUBLIC_KEY="$$(cat $(SPARKLE_TEST_PUBLIC_KEY))" \
		$(MAKE) bundle-iced
	ROOST_ICED_APP=mac/build/Roost-Iced.app ROOST_TEST_MODE=1 \
		uv run --group test pytest tools/roosttest/test_sparkle.py --roost-target iced --roost-fresh

# The Mac real-input harness (plan 074 §D7): real CGEvents and Accessibility
# through tools/input/mac/roost-input-mac, against the branch's own
# Roost-Iced.app in the Roost-linux namespace. Never in ICED_E2E_TESTS: the
# module launches and quits its own UI (`owns_ui`). ROOST_REAL_INPUT_MODE unset
# means runner mode, through ~/Applications/Roost Test Runner.app and its grants
# (tools/input/mac/test-runner/README.md); CI runs the module in `direct` mode.
e2e-iced-real-input-mac:  ## macOS-only: real CGEvent/AX input against the branch Roost-Iced.app (ROOST_REQUIRE_REAL_INPUT=1)
	@[ "$$(uname -s)" = "Darwin" ] || { echo "e2e-iced-real-input-mac is macOS-only: it posts real CGEvents into a macOS app"; exit 1; }
	cargo build --manifest-path tools/input/mac/roost-input-mac/Cargo.toml --locked
	$(MAKE) bundle-iced
	ROOST_REQUIRE_REAL_INPUT=1 ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest/test_real_input_mac.py --roost-target iced -v

mac-real-input-check:  ## macOS-only: can this Mac take real input now? (grants, lock, Secure Input, layout; claimants shown for information)
	@[ "$$(uname -s)" = "Darwin" ] || { echo "mac-real-input-check is macOS-only"; exit 1; }
	cargo build --manifest-path tools/input/mac/roost-input-mac/Cargo.toml --locked
	python3 tools/input/mac/runner.py check

smoke-iced:  ## Screenshot-driven UI smoke against a running Iced UI
	tools/screenshot/smoke.sh iced

test-iced-real-input: build-iced  ## Iced real clipboard input (self-contained Linux Xvfb+xdotool)
	ROOST_REQUIRE_REAL_INPUT=1 uv run --group test python tools/input/linux/iced_clipboard_check.py

test-iced-wayland-input: build-iced  ## Iced system clipboard with cage + a real uinput seat
	ROOST_REQUIRE_REAL_INPUT=1 uv run --group test python tools/input/linux/iced_wayland_clipboard_check.py

perf-refresh:  ## In-crate refresh_snapshot perf harness (release, --ignored; see tools/perf/README.md)
	cargo test -p roost-iced --release -- --ignored --nocapture

perf-render-stats:  ## Render-path counters for a running Iced UI (tools/perf/render-stats.sh iced <duration>; args for other targets)
	tools/perf/render-stats.sh iced

# ---- code quality -----------------------------------------------------

.PHONY: fmt fmt-check clippy check
fmt:  ## Format Rust (cargo fmt --all)
	cargo fmt --all

fmt-check:  ## Check formatting (what CI's rust-lint runs)
	cargo fmt --all -- --check

clippy: $(GHOSTTY_LIB)  ## Lint Rust at CI parity (warnings are errors)
	# `-D warnings` matches the `rust-lint` CI job. Without it `make check`
	# passed while CI failed, which is worse than no local gate at all.
	cargo clippy --workspace --all-targets -- -D warnings
	# roost-engine's `server-vt` module and its test target are cfg-gated,
	# so the workspace run above compiles neither. Same trap the roost-vt
	# comment on `test-rust` documents: a member's gated targets get
	# features only from an explicit `-p --features` run.
	cargo clippy -p roost-engine --features server-vt --all-targets -- -D warnings

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
	# roost-vt is an OPTIONAL roost-engine edge (`server-vt`). A
	# default-features build must not pull it in, or every UI build would
	# require the libghostty-vt archive — architecture §2's "no
	# default-build ghostty dependency", machine-enforced now that the
	# edge exists at all.
	@! cargo tree -p roost-engine | grep -E '(^| )roost-vt v' || \
		( echo "roost-engine pulls roost-vt with default features"; exit 1 )
	@t=$$(cargo tree -p roost-client -e normal,build --target all --prefix none) || exit 1; \
		! printf '%s\n' "$$t" | grep -E '^(roost-iced|iced|iced_[^ ]*|winit|gtk4|libadwaita|pango|cairo-rs|wgpu|zbus|arboard|notify-rust) ' || \
		( echo "roost-client depends on a UI crate"; exit 1 )
	@cargo tree -p roost-iced | grep -q 'swash v0.2.10 (.*third_party/swash)' || \
		( echo "swash [patch.crates-io] not applied"; exit 1 )

check: fmt-check clippy test  ## Pre-push gate: fmt-check + clippy + tests

# ---- mobile -----------------------------------------------------------
# Android cross-builds. Need the Android NDK (tools/mobile/android-env.sh
# finds it) and `rustup target add aarch64-linux-android x86_64-linux-android`;
# deliberately not part of `make check`.

ANDROID_TARGETS := aarch64-linux-android x86_64-linux-android

.PHONY: mobile-deps mobile-android-check mobile-apk mobile-apk-release mobile-dev-host mobile-icons
mobile-deps:  ## Build libghostty-vt for both Android ABIs into third_party/ghostty/out/<triple>/
	. tools/mobile/android-env.sh && for t in $(ANDROID_TARGETS); do \
		third_party/ghostty/build.sh --target $$t || exit 1; \
	done

# roost-client and roost-ssh are checked without their tests: their
# dev-dependencies (roost-engine -> portable-pty -> termios) do not build for
# Android.
mobile-android-check: mobile-deps  ## Cross-check roost-vt (ffi), roost-ipc, roost-ui-model, roost-client, roost-ssh for Android; link their test binaries
	. tools/mobile/android-env.sh && for t in $(ANDROID_TARGETS); do \
		RUSTFLAGS=-Dwarnings cargo check -p roost-client -p roost-ssh --target $$t && \
		RUSTFLAGS=-Dwarnings cargo test --no-run -p roost-vt -p roost-ipc -p roost-ui-model \
			--features roost-vt/ffi --target $$t || exit 1; \
	done

# Built for the two ABIs libghostty-vt exists for; Flutter's default would
# add android-arm too.
mobile-apk:  ## Debug APK (arm64-v8a + x86_64), then the 16 KB page-alignment check on it
	cd mobile && env -u ROOST_SOCKET -u ROOST_TAB_ID -u ROOST_AGENT_HOOK -u HERDR_ENV \
		flutter build apk --debug --target-platform android-arm64,android-x64
	tools/mobile/check-16kb-alignment.sh mobile/build/app/outputs/flutter-apk/app-debug.apk

mobile-apk-release:  ## Release APK for devices (arm64-v8a), signed with the .secrets key when linked (tools/mobile/link-signing.sh)
	cd mobile && env -u ROOST_SOCKET -u ROOST_TAB_ID -u ROOST_AGENT_HOOK -u HERDR_ENV \
		flutter build apk --release --target-platform android-arm64
	tools/mobile/check-16kb-alignment.sh mobile/build/app/outputs/flutter-apk/app-release.apk

mobile-dev-host:  ## A private dev roost-session behind dev_sshd, for the app in an emulator (Ctrl-C stops it)
	tools/mobile/dev-host.sh

# cairosvg loads libcairo through ctypes, which misses Homebrew's prefix on a Mac.
mobile-icons:  ## Re-render the app icons from Roost's owl and deploy them to android/ and macos/
	cd mobile && if [ "$$(uname -s)" = "Darwin" ] && command -v brew >/dev/null 2>&1; then \
		export DYLD_FALLBACK_LIBRARY_PATH="$$(brew --prefix)/lib:$${DYLD_FALLBACK_LIBRARY_PATH:-/usr/local/lib:/usr/lib}"; \
	fi; \
	uv run --with cairosvg --with pillow python scripts/generate_app_icons.py && \
	env -u ROOST_SOCKET -u ROOST_TAB_ID -u ROOST_AGENT_HOOK -u HERDR_ENV dart run flutter_launcher_icons

# ---- docs -------------------------------------------------------------

.PHONY: docs docs-serve
docs:  ## Build the docs site into site-build/ (same as CI)
	uv sync --locked --group docs && uv run --locked zensical build --strict

docs-serve:  ## Serve the docs locally with live reload
	uv sync --locked --group docs && uv run --locked zensical serve

# ---- clean ------------------------------------------------------------

.PHONY: clean
clean:  ## Remove build artifacts (cargo target, mac/build, site-build)
	cargo clean
	rm -rf $(MAC_DIR)/build site-build
