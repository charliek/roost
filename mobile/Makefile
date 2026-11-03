.PHONY: get fmt fmt-check analyze test check goldens frb-gen frb-check rust-check \
	session-bin test-integration-linux test-integration-android run-linux run-linux-fake \
	build-linux build-apk

FLUTTER ?= flutter

# A Roost tab exports these; strip them so nothing run from here can reach
# the live session this shell may be a tab of.
CLEAN_ENV := env -u ROOST_SOCKET -u ROOST_TAB_ID -u ROOST_AGENT_HOOK -u HERDR_ENV

get:
	$(CLEAN_ENV) $(FLUTTER) pub get

fmt:
	dart format lib test integration_test

fmt-check:
	dart format --output=none --set-exit-if-changed lib test integration_test

analyze:
	$(CLEAN_ENV) $(FLUTTER) analyze

test:
	$(CLEAN_ENV) $(FLUTTER) test

check: fmt-check analyze test

# Regenerate the Rust bridge after any change to crates/roost-mobile/src/api/:
# lib/src/rust/ and crates/roost-mobile/src/frb_generated.rs, both committed.
frb-gen:
	$(CLEAN_ENV) flutter_rust_bridge_codegen generate

# The drift guard: regenerating from a clean tree changes nothing.
frb-check: frb-gen
	@git diff --exit-code -- lib/src/rust ../crates/roost-mobile/src/frb_generated.rs
	@test -z "$$(git status --porcelain --untracked-files=all -- lib/src/rust)" || \
	  { echo "frb-check: untracked generated files in lib/src/rust" >&2; exit 1; }

# The Rust half: the bridge crate's lints and tests.
rust-check:
	cd .. && $(CLEAN_ENV) cargo clippy -p roost-mobile --all-targets -- -D warnings
	cd .. && $(CLEAN_ENV) sh -c 'if command -v cargo-nextest >/dev/null; then \
	  cargo nextest run -p roost-mobile; else cargo test -p roost-mobile; fi'

session-bin:
	cd .. && $(CLEAN_ENV) cargo build -p roost-session

# Rewrites test/terminal/goldens/*.png. Review the images before committing.
goldens:
	$(CLEAN_ENV) $(FLUTTER) test --update-goldens test/terminal/terminal_painter_test.dart

# The whole stack under Xvfb: this app and the Rust core through the bridge,
# against a roost-session of its own (private /tmp/rm-* dirs, ROOST_TEST_MODE,
# stopped by PID). One `flutter test` per file: the Linux device runner
# cannot launch the app twice in one invocation. It builds its own test
# entrypoint into build/linux/…/bundle, so rebuild before running the bundle.
test-integration-linux: session-bin
	@command -v xvfb-run >/dev/null || { echo "needs xvfb-run (apt install xvfb)" >&2; exit 1; }
	@set -e; for f in integration_test/*_test.dart; do \
	  echo "=== $$f"; \
	  $(CLEAN_ENV) xvfb-run -a -s "-screen 0 1280x800x24" $(FLUTTER) test "$$f" -d linux; \
	done

# The same test on an Android device or emulator, over SSH: dev-host.sh runs a
# private roost-session behind dev_sshd on this machine (stopped by its PID
# afterwards), and the app adds it as dev@10.0.2.2 (the emulator's name for
# this host), trusts its key and drives it. Boot an x86_64 emulator first.
ANDROID_DEVICE ?= emulator-5554
E2E_PORT ?= 2299
E2E_HOST ?= dev@10.0.2.2:$(E2E_PORT)
test-integration-android:
	@set -u; mkdir -p build; log=build/dev-host.log; \
	$(CLEAN_ENV) ../tools/mobile/dev-host.sh --port $(E2E_PORT) >$$log 2>&1 & pid=$$!; \
	trap 'kill $$pid 2>/dev/null; wait $$pid 2>/dev/null' EXIT INT TERM; \
	for _ in $$(seq 1 1500); do \
	  grep -q '^In the app add host' $$log && break; \
	  kill -0 $$pid 2>/dev/null || { cat $$log >&2; exit 1; }; \
	  sleep 0.2; \
	done; \
	grep -q '^In the app add host' $$log || { echo "dev-host did not start" >&2; cat $$log >&2; exit 1; }; \
	$(CLEAN_ENV) $(FLUTTER) test integration_test/real_core_test.dart -d $(ANDROID_DEVICE) \
	  --dart-define=ROOST_E2E_TARGET=$(E2E_HOST)

# The app on the Linux desktop with the Rust core. Add a host as
# unix:/path/to/roost.sock (a dev session) or user@machine (Tailscale SSH).
run-linux:
	$(CLEAN_ENV) $(FLUTTER) run -d linux

# The same against the scripted FakeCore: no Rust, no host. Resize the window
# across 600 and 840 dp to see the three layouts.
run-linux-fake:
	$(CLEAN_ENV) $(FLUTTER) run -d linux --dart-define=ROOST_FAKE=1

build-linux:
	$(CLEAN_ENV) $(FLUTTER) build linux --debug

build-apk:
	$(CLEAN_ENV) $(FLUTTER) build apk --debug
