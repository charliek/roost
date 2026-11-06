# Releasing Roost

The general release framework is `cc-plugins:release-workflows`; this file
documents what's specific to this repo.

## TL;DR

    /release-workflows:release v<NEXT_VERSION>

That's it. Everything else is automatic.

## What happens

1. **`release-workflows:release`** (LLM, local):
   - Verifies branch (`main`) + clean tree + `ci-success` green on HEAD
   - Asks/confirms version
   - Drafts a CHANGELOG entry from `git log v<previous>..HEAD`, commits as
     `docs(changelog): vX.Y.Z entry`
   - Runs `scripts/release/update-version.sh X.Y.Z` → bumps Cargo.toml,
     Cargo.lock, and `.claude-plugin/plugin.json`
   - Commits as `chore(version): bump to X.Y.Z`
   - Tags `vX.Y.Z` (annotated) on the version commit
   - `git push --follow-tags`

2. **`release.yml`** (CI, on tag):
   - `version-check` — tag matches `[workspace.package].version`
   - `ci-gate` — `ci-success` green on tagged commit
   - `create-release` — extract CHANGELOG section → `gh release create --draft`
   - `linux` (amd64 + arm64 matrix) — build + upload `roost_X.Y.Z_<arch>.deb`,
     plus (unpacked from the same build, no second cargo invocation) a
     standalone `roost-session-X.Y.Z-linux-<arch>` binary and its
     `.sha256` checksum sidecar — the host-sessions bootstrap install rung
     fetches these directly (`crates/roost-ipc/src/bootstrap.rs`)
   - `mac` — the iced UI as `Roost.app`, in its **production identity**
     (`ROOST_BUNDLE_IDENTITY=production`: bundle id `ai.stridelabs.Roost`,
     the `docs/appcast.xml` feed, and the committed
     `mac/keys/roost-sparkle-ed-public-key.txt`), so every installed
     Roost.app updates to it in place. Before building it runs
     `mac/scripts/verify-sparkle-key.sh`, which fails fast unless the
     committed key is the pinned production key and `SPARKLE_ED_PRIVATE_KEY`
     is its private half (it signs and verifies a probe to prove it). Then:
     bundle, assert the bundle's id, feed and key, E2E (`test_smoke` +
     `test_sparkle`) against the bundle, notarize + staple the app itself,
     stage the darwin `roost-session` asset, package + notarize
     `Roost-X.Y.Z.dmg` and upload it, then EdDSA-sign that final DMG with
     the vendored `sign_update` and hand `sign.txt` forward as the
     `sparkle-sign` build artifact. A verify step mounts the DMG and
     confirms the app inside carries its own ticket. The "Append macOS
     first-launch note" step keeps the Gatekeeper bypass instructions on
     the Release body while the DMG is not notarized.
   - **`publish-release`** — runs `tools/release/check_assets.py` over the
     draft's assets (both `.deb`s, the DMG, the three `roost-session`
     standalone binaries and their three `.sha256` sidecars — nine assets,
     one each, correctly named and sized, nothing else), then flips the
     draft public. **This is the only irreversible step in the pipeline.**
   - `appcast` — append the signed Sparkle entry to `docs/appcast.xml` and
     bot-push it to main. Runs after publish because the enclosure URL it
     writes is a public `/releases/download/` link.
   - `dispatch-apt-charliek` — fire a `repository_dispatch` at
     `charliek/apt-charliek` so the .debs land on `apt.stridelabs.ai`.
     Uses a release-bot App token scoped to `apt-charliek` (no
     per-pipeline PAT); the App must be installed on apt-charliek too.

## Draft-until-complete, and how to recover

The Release is created as a **draft** and stays one until `publish-release`
has seen all **nine** assets on it — an amd64 `.deb`, an arm64 `.deb`,
`Roost-X.Y.Z.dmg`, a standalone `roost-session` binary per published
platform (linux-amd64, linux-arm64, darwin-arm64), and that binary's
`.sha256` checksum sidecar — each present exactly once, named for the tag,
and plausibly sized, with nothing else on the draft. The six
binaries/packages need at least 1 MiB (a floor that catches truncated
uploads plain non-emptiness would wave through); a sidecar's floor is the
smallest `sha256sum` record its reader accepts (64 hex digits, a separator
and the file name), so a real one is never mistaken for a truncated one.
`tools/release/check_assets.py` holds these rules, and
`tools/roosttest_unit/test_check_assets.py` tests them.

A draft is invisible on the public releases page and API to anyone without
push access. That is not cosmetic: apt-charliek authenticates with a
repo-scoped `GITHUB_TOKEN` that has no push access here, so it cannot see a
draft at all. Before this, `dispatch-apt-charliek` fired on `needs: linux`
and apt-charliek's `collect-debs.sh` — which walks releases newest-first and
only *warns* past one carrying no matching `.deb` — would **silently
republish the previous version**.

### A build job failed

`linux` or `mac` red → the draft stays a draft. Nothing is public, no apt
dispatch, no appcast entry. `publish-release` has a plain
`needs: [linux, mac]` and no `if: always()`, so it simply never runs — a
`mac` key-check failure, before it even builds, blocks the whole release
exactly like a build failure does.

    gh release view vX.Y.Z --json isDraft,assets   # inspect

**Re-run** the failed jobs from the Actions UI. `create-release` sees the
existing draft and reuses it, `gh release upload --clobber` overwrites any
partial asset, and the `sparkle-sign` artifact upload sets `overwrite: true`
so a mac re-run does not collide with its own earlier output. The workflow's concurrency group serializes a re-run
against a still-running original.

**Discard** with `gh release delete vX.Y.Z --yes`. That deletes the release
only, not the git tag; re-running the workflow recreates the draft from
scratch.

### `publish-release` failed its assertions

The draft is untouched and the error names the missing, duplicated or empty
asset. Fix the cause, re-run. The job is idempotent — against an
already-published release it emits a notice and exits 0.

### Re-running the whole workflow after a successful release

**It will fail at `create-release`, by design.** Reusing a *draft* is the
re-run case and is allowed; reusing a *published* release is refused, because
the build jobs would `--clobber` new assets into something users and
apt-charliek can already see, mid-run.

To re-dispatch apt or rebuild the appcast after a successful publish, re-run
**those individual jobs**. To genuinely rebuild a published version,
`gh release delete vX.Y.Z --yes` first, or cut a new tag.

### A post-publication job failed — the cases after the point of no return

The release is already live and correct: users can download it. `appcast` and
`dispatch-apt-charliek` are **parallel siblings** of `publish-release`, so the
apt dispatch may have fired, may be running, or may itself have failed —
check it separately rather than assuming. Only `docs/appcast.xml` is stale, so
existing macOS installs will not be offered the update until it is fixed.
**Nothing is broken for new users; in-app updates are simply not offered
yet.**

`dispatch-apt-charliek` can fail here too, and independently. Its own row in
the symptom table covers it; the short version is that apt-charliek re-scans
on its next scheduled run, so a missed dispatch self-heals where a missed
appcast does not.

For `appcast`: re-run just that job. It re-downloads `sparkle-sign` from the same
workflow run, and `update-appcast.py` dedupes by version and preserves the
prior `pubDate`, so re-runs are safe and idempotent. Two failure modes worth
telling apart:

- **the DMG URL check failed** — the asset is not actually on the published
  release, so contrary to the paragraph above **this release is not fine**:
  macOS users have nothing to download. `publish-release` asserts the DMG is
  present, so reaching this state means it was removed afterward, or the CDN
  has not caught up. Re-upload the DMG (`gh release upload <tag>
  Roost-X.Y.Z.dmg --clobber`), confirm the public URL resolves, then re-run.
- **the push loop exhausted its 3 attempts** — main moved faster than the
  retry. Just re-run.

Last resort: run `mac/scripts/update-appcast.py` locally and open a normal
PR. The bot exists only because of main's ruleset, not because the change is
special.

## Version files this repo owns

`scripts/release/update-version.sh` bumps:

- `Cargo.toml` — `[workspace.package].version`, the canonical roost version
- `Cargo.lock` — workspace member entries, regenerated via
  `cargo update --workspace --offline`
- `examples/ipc-consumer/Cargo.lock` — that example is its own workspace
  pinning `roost-ipc` by path, and CI builds it `--locked`
- `.claude-plugin/plugin.json` — the Claude Code plugin's `version`, which
  Claude Code compares to decide whether an installed skill is stale

NOT bumped:

- `pyproject.toml` — for the `tools/roosttest/` pytest harness; has its own
  version cadence
- `mac/keys/roost-sparkle-ed-public-key.txt` — the Sparkle EdDSA public
  key, changed only when the key is rotated, never per release. Rotating it
  strands every installed Roost.app (they verify updates with the old key),
  which is why `verify-sparkle-key.sh` pins its literal too

## Snapshot / dev versioning

Not used. Main between releases shows the last released version. If a
build identity beyond "last released" is needed (e.g. for `roostctl
--version` diagnostics), derive it at build time from
`git describe --tags --dirty` rather than snapshotting the source tree.

## Secrets

| Secret | Purpose | Required? |
|---|---|---|
| `RELEASE_BOT_CLIENT_ID` | `charliek-release-bot` GitHub App client ID (`create-github-app-token`'s `client-id` input; the numeric App ID also works) | required — bot push of signed appcast + apt-charliek dispatch |
| `RELEASE_BOT_APP_KEY` | App private key (.pem) | required — same |
| `SPARKLE_ED_PRIVATE_KEY` | EdDSA signing key for the `docs/appcast.xml` feed: base64 of the key file `generate_keys -x` wrote (32-byte seed, or the legacy 96-byte layout). Its public half is `mac/keys/roost-sparkle-ed-public-key.txt`, the key the Swift Roost.app shipped with | **required for every release, prereleases included** — the `mac` job's "Verify the Sparkle key material" step fails fast, before building, without it or with any other key |
| `ROOST_ICED_SPARKLE_ED_PRIVATE_KEY` | The retired Roost-Iced.app feed's key. No job reads it since v0.1.0 | unused — safe to delete |
| `APT_DISPATCH_TOKEN` | Legacy PAT — superseded by the release-bot App; can be removed once you're sure the App-based dispatch is working | optional / deprecated |
| `MACOS_CERTIFICATE_P12_BASE64` + `MACOS_CERTIFICATE_PASSWORD` + `APPLE_ID` + `APPLE_TEAM_ID` + `APPLE_APP_SPECIFIC_PASSWORD` + `ROOST_DEVELOPER_ID_IDENTITY` | Mac code-signing + notarization | **set** (2026-06-28; #83 closed) — DMG is Developer ID signed + notarized. All six are gated together as `CAN_NOTARIZE` (all-or-nothing); any one unset → ad-hoc-signed DMG with the Gatekeeper-bypass note |

The cert + Apple creds are kept locally — git-ignored, synced across
machines via envsecrets (the `# envsecrets` marker in `.gitignore`) — at
`.secrets/cert.p12` + `.secrets/apple.env`. `envsecrets pull` restores them
on a new machine. For a local notarized build, source `apple.env`
AND import `cert.p12` into a keychain first — the login keychain carries
no signing identity, so without the import the bundle scripts fail
per-file with "no identity found" (mirror release.yml's temp-keychain
import step).

## Branch protection

`main` is protected by ruleset `main-protection` (id `17018841`) with
`required_status_checks=['ci-success']`. Two bypass actors:

- `charliek-release-bot` (App id `3902108`, type `Integration`) — lets the
  bot push the appcast commit from the `appcast` job
- Admin role (id `5`, type `RepositoryRole`) — lets `/release-workflows:release`'s
  push of the changelog + version commits + tag land before `ci-success`
  exists on those new commits

Inspect or edit at https://github.com/charliek/roost/rules.

## The appcast lives where

The Sparkle appcast is at `docs/appcast.xml`, served by GitHub Pages from
`https://charliek.github.io/roost/appcast.xml` via `docs.yml`'s Zensical
deploy. The `appcast` job mutates that file in place, commits it as the
release-bot, and pushes to main; `docs.yml` redeploys Pages shortly after.

The appcast updater script is `mac/scripts/update-appcast.py`. It reads
`ROOST_VERSION`, `ROOST_TAG`, `ROOST_REPO`, and `ROOST_SIGN_FILE` from the
environment
(the sign output of Sparkle's `sign_update`), dedupes by version, and
preserves the existing `pubDate` if re-running against an unchanged version
(so workflow re-runs produce a byte-empty diff and the "nothing to push"
guard fires correctly).

`docs/appcast-iced.xml`, the feed of the retired Roost-Iced.app, is
**frozen**: no job writes it any more, and it is never deleted, so a
Roost-Iced.app still installed keeps a valid feed. Its last item is a
Sparkle *informational* update (`<sparkle:informationalUpdate />`, no
enclosure) at 0.1.0, so **Check for Updates…** shows a notice whose Learn
More opens the switch steps in the installation docs instead of installing
anything. It went live with v0.1.0, not before, because Pages redeploys on
any push to `docs/`.

## When things break

| Symptom | Cause | Fix |
|---|---|---|
| `git push` rejected: `Required status check "ci-success"` | Pusher not in ruleset bypass | Confirm both the App (3902108, Integration) and the admin role (5, RepositoryRole) are in `main-protection`'s `bypass_actors` — see [`cc-plugins/plugins/release-workflows/references/github-app.md`](https://github.com/charliek/cc-plugins/blob/main/plugins/release-workflows/references/github-app.md) |
| `scripts/release/update-version.sh` not found | Convention not adopted | Run `/release-workflows:setup` |
| `update-version.sh` aborts: "Cargo.toml's version did not update" | Someone reformatted `[workspace.package]` away from the column-aligned style this script expects | Either restore the alignment, or change the sed replacement in `scripts/release/update-version.sh` to vanilla single-space style |
| Tag pushed, `version-check` fails | The tag's base version and `[workspace.package].version` disagree — either the tagged commit didn't run `update-version.sh`, or the tag itself was mistyped | Compare the two first. Re-bump only if the commit is the wrong one; if the tag is, just re-tag. See the next row for whether the tag may move |
| Tag pushed, the run failed before the release went public | `ci-gate` refused a red commit, or a build job failed | Nothing is external yet: `create-release` only makes a draft. Fix forward on `main`, wait for CI, then move the tag: `git push origin :refs/tags/vX.Y.Z`, re-tag at the fix commit, push the tag. The cutoff is the release going **public**, not `publish-release` starting: a `publish-release` that fails its assertions leaves the draft unpublished, and the tag may still move. **Once the release is public, never move the tag** — cut a fresh patch version instead. v0.0.20 moved its tag twice this way |
| `ci-gate` fails right after the bump: `cannot update the lock file … --locked` | `update-version.sh` missed a lockfile some CI job builds `--locked` | Add the file to `update-version.sh` as a verified step (as `examples/ipc-consumer/Cargo.lock` was), fix forward, re-tag |
| `mac` fails at notarize with `status: Invalid` | Apple refused the submission; `notarize.sh` prints Apple's per-binary reasons (`notarytool log`) before it stops | Read the printed `issues`. *Not signed with a valid Developer ID certificate* together with *no secure timestamp* on every binary means the bundle was **ad-hoc** signed — something after the bundle step rebuilt or re-signed it (v0.0.20: the E2E harness ran `bundle.sh debug` over it; now `ROOST_MAC_NO_BUNDLE=1` plus a refusal in the harness). To read an old submission's log by hand: source `.secrets/apple.env` and run `xcrun notarytool log <submission-id> --apple-id … --team-id … --password …` on a Mac |
| `mac` fails at "Assert the app still carries its Developer ID signature" | A step between bundling and notarizing replaced or re-signed the bundle | Find the step that writes to `mac/build/*.app` and stop it; the same cause as the row above, caught locally |
| `mac` job fails at "Sign the DMG for Sparkle" with `SPARKLE_ED_PRIVATE_KEY secret is unset` | The signing secret is not set (the key check before the build would normally have caught it) | Set the secret and re-run the mac job. Cutting a prerelease does **not** help — signing is unconditional |
| `appcast` job fails at "Push signed appcast" with `protected branch hook declined` | App removed from ruleset bypass | Re-add `{ actor_id: 3902108, actor_type: "Integration" }` to `main-protection`'s `bypass_actors` |
| `create-release` fails: "already exists and is PUBLISHED" | Re-running the whole workflow for a tag that already shipped | Deliberate (fail-closed). Re-run the individual job you need, or `gh release delete vX.Y.Z --yes` to rebuild from scratch |
| `publish-release` fails: "expected exactly one asset named …" | A build job uploaded nothing, or uploaded the same name twice | The draft is untouched. Inspect with `gh release view vX.Y.Z --json assets`, fix, re-run |
| `publish-release` fails: "unexpected assets on vX.Y.Z" | A reused draft still carries an asset from an earlier attempt or an older version | Delete the stale asset (`gh release delete-asset vX.Y.Z <name>`) and re-run. This matters: apt-charliek globs `roost_*.deb`, so a stale one would ship |
| `publish-release` fails: "… looks truncated" | An upload was interrupted | Re-run the job that produced it; `--clobber` overwrites |
| `appcast` fails: "does not resolve — refusing to publish an appcast entry that points at a 404" | The DMG is not on the published release | Re-upload it (`gh release upload vX.Y.Z Roost-X.Y.Z.dmg --clobber`), confirm the public URL resolves, then re-run just the `appcast` job. Re-running `appcast` alone cannot fix a missing asset — and until it is back, macOS users have nothing to download |
| Release is stuck as a draft | `publish-release` never ran or never passed | See [Draft-until-complete](#draft-until-complete-and-how-to-recover) |
| Appcast not visible at `https://charliek.github.io/roost/appcast.xml` after a release | `docs.yml` didn't redeploy | Check `docs.yml`'s most recent run; re-trigger via Actions UI if needed |
| `dispatch-apt-charliek` shows a warning about missing token | `RELEASE_BOT_CLIENT_ID` unset OR the App is not installed on `charliek/apt-charliek` | Confirm via `sanity-check-app.yml`'s "Token can reach charliek/apt-charliek" block; if missing, install the App on apt-charliek. Otherwise wait for apt-charliek's next scheduled re-scan (it picks up new .debs automatically) |
| `mac` fails at "Verify the Sparkle key material" | `verify-sparkle-key.sh` names which: the committed `mac/keys/roost-sparkle-ed-public-key.txt` is missing, the TEST-ONLY fixture, the throwaway key, or not the pinned production key; or `SPARKLE_ED_PRIVATE_KEY` is unset, doesn't decode to a 32- or 96-byte ed25519 key, isn't the committed key's private half, or `sign_update`'s probe signature doesn't verify against it | Restore the committed key or the secret — never rotate the key to make the check pass: installed apps only accept updates signed by it. This blocks `publish-release` (needs `[linux, mac]`) |
| v0.0.5 incident: mac job failed at appcast step because `Cargo.lock` drifted during the build | `/release:release` didn't bump `Cargo.lock` (legacy plugin); the staged-set assertion in the bot push step caught the drift | Now solved: `/release-workflows:release` runs `update-version.sh` which always regenerates `Cargo.lock`. |

## Adopting the convention (for new contributors)

Read [`cc-plugins/plugins/release-workflows/references/convention.md`](https://github.com/charliek/cc-plugins/blob/main/plugins/release-workflows/references/convention.md)
in the framework repo. It defines the contract every file in this repo's
`scripts/release/` and `.github/workflows/release.yml` is written against.

## Notes for this repo

- `mac/scripts/verify-sparkle-key.sh` refuses the TEST-ONLY fixture key
  and the throwaway key from the Sparkle 2 spike (issue #122) on every tag,
  prereleases included; its pinned production key leaves no prerelease
  path for testing another key. `mac/scripts/verify-sparkle-key_test.sh`
  (the `scripts` CI job) tests it with generated keys.
- Sparkle appcast publishing is **split across two jobs**. Signing stays in
  `mac` — it holds the final DMG and the `sign_update` its Stage Sparkle
  step fetched. Writing the feed lives in `appcast`, which cannot run until
  the Release is published, because the enclosure URL is a public
  `/releases/download/` link that a draft's assets do not answer. The whole
  `sign.txt` travels between them as the `sparkle-sign` build artifact: it
  carries both `sparkle:edSignature` and `length`, and the updater needs
  both. An appcast failure costs a ~1-minute job re-run instead of the
  whole mac job.
