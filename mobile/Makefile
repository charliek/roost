.PHONY: get fmt fmt-check analyze test check goldens run-linux build-linux build-apk

FLUTTER ?= flutter

# A Roost tab exports these; strip them so nothing run from here can reach
# the live session this shell may be a tab of.
CLEAN_ENV := env -u ROOST_SOCKET -u ROOST_TAB_ID -u ROOST_AGENT_HOOK -u HERDR_ENV

get:
	$(CLEAN_ENV) $(FLUTTER) pub get

fmt:
	dart format lib test

fmt-check:
	dart format --output=none --set-exit-if-changed lib test

analyze:
	$(CLEAN_ENV) $(FLUTTER) analyze

test:
	$(CLEAN_ENV) $(FLUTTER) test

check: fmt-check analyze test

# Rewrites test/terminal/goldens/*.png. Review the images before committing.
goldens:
	$(CLEAN_ENV) $(FLUTTER) test --update-goldens test/terminal/terminal_painter_test.dart

# The app on the Linux desktop against the scripted FakeCore (no Rust, no
# host). Resize the window across 600 and 840 dp to see the three layouts.
run-linux:
	$(CLEAN_ENV) $(FLUTTER) run -d linux --dart-define=ROOST_FAKE=1

build-linux:
	$(CLEAN_ENV) $(FLUTTER) build linux --debug

build-apk:
	$(CLEAN_ENV) $(FLUTTER) build apk --debug
