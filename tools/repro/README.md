# `tools/repro/` — flake & bug reproduction drivers

Scripts that make an *intermittent* failure happen on demand. Not a test
tier (see [`tools/README.md`](../README.md) for those) — nothing here runs
in CI. Reach for one when a test fails on CI but not locally, and you need
a failure rate you can measure before and after a fix.

A script lands here when the bug is timing- or environment-dependent
enough that "run the test again" isn't a reproduction. Once the fix is in,
the script stays: it is how the next person confirms a regression is the
same bug.

## `single-instance-flake.sh` — issue #324

`single_instance::tests::drop_releases_so_next_acquire_succeeds` panics
with `AlreadyHeld(<our own pid>)` in roughly 1-in-10 `cargo test
--workspace` runs, on both ubuntu-latest and macos-latest.

`flock(2)` locks live on the **open file description**, not on the fd or
the process. A `fork()` inherits a duplicate of the lock fd and keeps the
lock alive until that fd closes at `exec` (CLOEXEC). Rust's `File` drop
calls only `close(2)`, never `flock(LOCK_UN)`. So when a sibling test
forks during the window in which this test holds the lock, `drop(first)`
does not release the flock and the immediately following `acquire()` gets
`WouldBlock`.

**"Sibling test" means the same test *binary*.** Other crates' test
binaries are separate processes that never inherited our fd, so they are
irrelevant; the forks that matter are the subprocess-spawning tests inside
`roost-engine`'s own lib test binary (`git_metrics`, `process`, ...).
That is also why a filtered run (`cargo test -p roost-engine
single_instance`) essentially never fails — it has no forking siblings.

```sh
tools/repro/single-instance-flake.sh                     # 200 engine runs, ~3 min
tools/repro/single-instance-flake.sh -n 300              # tighter rate estimate
tools/repro/single-instance-flake.sh --scope workspace   # what CI runs, ~40s/iteration
```

`--scope engine` (the default) loops `cargo test -p roost-engine --lib`,
which is where the race actually lives and runs in well under a second per
iteration; `--scope workspace` loops the full `cargo test --workspace` for
a like-for-like comparison with CI, at ~60x the cost per iteration.
`-j/--test-threads` and `--load N` (background CPU hogs) both widen the
fork→exec window. The script splits failures into "reproduced the #324
lock flake" and "unrelated" so an incidental red can't be misread as a
reproduction, prints the first #324 failure, and exits non-zero if any
iteration failed.

Observed on an M-series Mac: **6/300** at the default settings.

The deterministic counterpart lives in the test suite itself:
`single_instance::tests::drop_releases_even_when_a_forked_child_inherited_the_fd`
clears `FD_CLOEXEC` on the lock fd before spawning a child, so the
inherited description provably outlives the drop. Run it with
`cargo test -p roost-engine single_instance -- --include-ignored`.

One gotcha when reading a failing log: `crash::tests` swaps the
process-global panic hook while it runs, so a concurrent panic in another
test can lose its message and show up as a bare `FAILED` with an empty
`stdout` block. The test name is still the signal.
