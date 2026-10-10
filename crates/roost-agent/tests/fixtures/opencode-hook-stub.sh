#!/bin/sh
# The `$ROOST_AGENT_HOOK` that `crates/roost-agent/tests/opencode_plugin_test.rs`
# hands the opencode plugin.
#
# Records argv and stdin of one `agent-hook` invocation into its own
# file, named for the sequence number it takes *after* finishing — so
# the file names are completion order, which is the order Roost would
# have seen these reports arrive.
#
# Three behaviours ride on env vars, each off by default: wedge the
# first invocation, stall one named event, and nothing else.
#
# Committed rather than written per run: a script the test process
# writes races a sibling test's fork into ETXTBSY on `execve` — see
# `tools/roosttest/fixtures/fake-roost-session.sh`'s header.
if [ -n "$ROOST_TEST_HANG_DIR" ] && mkdir "$ROOST_TEST_HANG_DIR/hung" 2>/dev/null; then
  printf '%s\n' "$$" > "$ROOST_TEST_PID_FILE"
  exec sleep 30
fi
out=$(mktemp "$ROOST_TEST_STAGE_DIR/rec.XXXXXX")
printf '%s\n' "$*" > "$out"
cat >> "$out"
if [ -n "$ROOST_TEST_SLOW_EVENT" ] && grep -q "\"hook_event_name\":\"$ROOST_TEST_SLOW_EVENT\"" "$out"; then
  sleep "$ROOST_TEST_SLOW_SECONDS"
fi
i=0
while ! mkdir "$ROOST_TEST_SEQ_DIR/$i" 2>/dev/null; do i=$((i + 1)); done
mv "$out" "$ROOST_TEST_RECORD_DIR/$i"
