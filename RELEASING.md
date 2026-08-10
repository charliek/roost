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
   - Runs `scripts/release/update-version.sh X.Y.Z` → bumps Cargo.toml +
     Cargo.lock
   - Commits as `chore(version): bump to X.Y.Z`
   - Tags `vX.Y.Z` (annotated) on the version commit
   - `git push --follow-tags`

2. **`release.yml`** (CI, on tag):
   - `version-check` — tag matches `[workspace.package].version`
   - `ci-gate` — `ci-success` green on tagged commit
   - `create-release` — extract CHANGELOG section → `gh release create --draft`
   - `linux` (amd64 + arm64 matrix) — build + upload `roost_X.Y.Z_<arch>.deb`
   - `mac` — build + sign + notarize + upload `Roost-X.Y.Z.dmg`, then
     EdDSA-sign it and hand `sign.txt` forward as a build artifact. The
     "Append macOS first-launch note" step keeps the Gatekeeper bypass
     instructions on the Release body while the DMG is not notarized.
   - **`publish-release`** — asserts the artifact set, then flips the draft
     public. **This is the only irreversible step in the pipeline.**
   - `appcast` — append the signed Sparkle entry to `docs/appcast.xml` and
     bot-push it to main. Runs after publish because the enclosure URL it
     writes is a public `/releases/download/` link.
   - `dispatch-apt-charliek` — fire a `repository_dispatch` at
     `charliek/apt-charliek` so the .debs land on `apt.stridelabs.ai`.
     Uses a release-bot App token scoped to `apt-charliek` (no
     per-pipeline PAT); the App must be installed on apt-charliek too.

## Draft-until-complete, and how to recover

The Release is created as a **draft** and stays one until `publish-release`
has seen an amd64 `.deb`, an arm64 `.deb` and a `.dmg` on it — each present
exactly once, non-empty, and named for the tag.

A draft is invisible on the public releases page and API to anyone without
push access. That is not cosmetic: apt-charliek authenticates with a
repo-scoped `GITHUB_TOKEN` that has no push access here, so it cannot see a
draft at all. Before this, `dispatch-apt-charliek` fired on `needs: linux`
and apt-charliek's `collect-debs.sh` — which walks releases newest-first and
only *warns* past one carrying no matching `.deb` — would **silently
republish the previous version**.

### A build job failed

`linux` or `mac` red → the draft stays a draft. Nothing is public, no apt
dispatch, no appcast entry. `publish-release` has a plain `needs:` and no
`if: always()`, so it simply never runs.

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

### `appcast` failed — the one case that happens after the point of no return

The release is already live and correct: users can download it. `appcast` and
`dispatch-apt-charliek` are **parallel siblings** of `publish-release`, so the
apt dispatch may have fired, may be running, or may itself have failed —
check it separately rather than assuming. Only `docs/appcast.xml` is stale, so
existing macOS installs will not be offered the update until it is fixed.
**Nothing is broken for new users; in-app updates are simply not offered
yet.**

Re-run just the `appcast` job. It re-downloads `sparkle-sign` from the same
workflow run, and `update-appcast.py` dedupes by version and preserves the
prior `pubDate`, so re-runs are safe and idempotent. Two failure modes worth
telling apart:

- **the DMG URL check failed** — the asset is not actually on the published
  release, so contrary to the paragraph above **this release is not fine**:
  macOS users have nothing to download. `publish-release` asserts the DMG is
  present, so reaching this state means it was removed afterwards, or the CDN
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

NOT bumped:

- `pyproject.toml` — for the `tools/roosttest/` pytest harness; has its own
  version cadence
- `mac/Resources/Info.plist.template`'s `SUPublicEDKey` — bumped only when
  the Sparkle EdDSA key is rotated, not per release

## Snapshot / dev versioning

Not used. Main between releases shows the last released version. If a
build identity beyond "last released" is needed (e.g. for `roostctl
--version` diagnostics), derive it at build time from
`git describe --tags --dirty` rather than snapshotting the source tree.

## Secrets

| Secret | Purpose | Required? |
|---|---|---|
| `RELEASE_BOT_APP_ID` | `charliek-release-bot` GitHub App ID (3902108) | required — bot push of signed appcast + apt-charliek dispatch |
| `RELEASE_BOT_APP_KEY` | App private key (.pem) | required — same |
| `SPARKLE_ED_PRIVATE_KEY` | EdDSA signing key for Sparkle appcast, base64-encoded | **required for every release, prereleases included** — the mac job's signing step fails hard without it. Only the separate *throwaway-key guard* is prerelease-exempt |
| `APT_DISPATCH_TOKEN` | Legacy PAT — superseded by the release-bot App; can be removed once you're sure the App-based dispatch is working | optional / deprecated |
| `MACOS_CERTIFICATE_P12_BASE64` + `MACOS_CERTIFICATE_PASSWORD` + `APPLE_ID` + `APPLE_TEAM_ID` + `APPLE_APP_SPECIFIC_PASSWORD` + `ROOST_DEVELOPER_ID_IDENTITY` | Mac code-signing + notarization | **set** (2026-06-28; #83 closed) — DMG is Developer ID signed + notarized. All six are gated together as `CAN_NOTARIZE` (all-or-nothing); any one unset → ad-hoc-signed DMG with the Gatekeeper-bypass note |

The cert + Apple creds are kept locally — git-ignored, synced across machines
via envsecrets (the `# envsecrets` marker in `.gitignore`) — at
`.secrets/cert.p12` + `.secrets/apple.env`. `envsecrets pull` restores them on a
new machine; source `apple.env` for a local notarized build.

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
`https://charliek.github.io/roost/appcast.xml` via `docs.yml`'s mkdocs
deploy. The `appcast` job mutates that file in place, commits it as the
release-bot, and pushes to main; `docs.yml` redeploys Pages shortly after.

The appcast updater script is `mac/scripts/update-appcast.py`. It reads
`ROOST_VERSION`, `ROOST_TAG`, `ROOST_REPO`, and `ROOST_SIGN_FILE` from the
environment
(the sign output of Sparkle's `sign_update`), dedupes by version, and
preserves the existing `pubDate` if re-running against an unchanged version
(so workflow re-runs produce a byte-empty diff and the "nothing to push"
guard fires correctly).

## When things break

| Symptom | Cause | Fix |
|---|---|---|
| `git push` rejected: `Required status check "ci-success"` | Pusher not in ruleset bypass | Confirm both the App (3902108, Integration) and the admin role (5, RepositoryRole) are in `main-protection`'s `bypass_actors` — see [`cc-plugins/plugins/release-workflows/references/github-app.md`](https://github.com/charliek/cc-plugins/blob/main/plugins/release-workflows/references/github-app.md) |
| `scripts/release/update-version.sh` not found | Convention not adopted | Run `/release-workflows:setup` |
| `update-version.sh` aborts: "Cargo.toml's version did not update" | Someone reformatted `[workspace.package]` away from the column-aligned style this script expects | Either restore the alignment, or change the sed replacement in `scripts/release/update-version.sh` to vanilla single-space style |
| Tag pushed, `version-check` fails | Tagged a commit that didn't run `update-version.sh` | Re-bump locally + cut a fresh patch tag (don't force-update an existing tag) |
| `mac` job fails at "Sign the DMG for Sparkle" with `SPARKLE_ED_PRIVATE_KEY secret is unset` | The signing secret is not set | Set the secret and re-run the mac job. Cutting a prerelease does **not** help — signing is unconditional; only the separate throwaway-*key* guard is prerelease-exempt |
| `appcast` job fails at "Push signed appcast" with `protected branch hook declined` | App removed from ruleset bypass | Re-add `{ actor_id: 3902108, actor_type: "Integration" }` to `main-protection`'s `bypass_actors` |
| `create-release` fails: "already exists and is PUBLISHED" | Re-running the whole workflow for a tag that already shipped | Deliberate (fail-closed). Re-run the individual job you need, or `gh release delete vX.Y.Z --yes` to rebuild from scratch |
| `publish-release` fails: "expected exactly one asset named …" | A build job uploaded nothing, or uploaded the same name twice | The draft is untouched. Inspect with `gh release view vX.Y.Z --json assets`, fix, re-run |
| `publish-release` fails: "unexpected assets on vX.Y.Z" | A reused draft still carries an asset from an earlier attempt or an older version | Delete the stale asset (`gh release delete-asset vX.Y.Z <name>`) and re-run. This matters: apt-charliek globs `roost_*.deb`, so a stale one would ship |
| `publish-release` fails: "… looks truncated" | An upload was interrupted | Re-run the job that produced it; `--clobber` overwrites |
| `appcast` fails: "does not resolve — refusing to publish an appcast entry that points at a 404" | The DMG is not on the published release | Confirm the asset, then re-run just the `appcast` job. The release itself is fine |
| Release is stuck as a draft | `publish-release` never ran or never passed | See [Draft-until-complete](#draft-until-complete-and-how-to-recover) |
| Appcast not visible at `https://charliek.github.io/roost/appcast.xml` after a release | `docs.yml` didn't redeploy | Check `docs.yml`'s most recent run; re-trigger via Actions UI if needed |
| `dispatch-apt-charliek` shows a warning about missing token | `RELEASE_BOT_APP_ID` unset OR the App is not installed on `charliek/apt-charliek` | Confirm via `sanity-check-app.yml`'s "Token can reach charliek/apt-charliek" block; if missing, install the App on apt-charliek. Otherwise wait for apt-charliek's next scheduled re-scan (it picks up new .debs automatically) |
| v0.0.5 incident: mac job failed at appcast step because `Cargo.lock` drifted during the build | `/release:release` didn't bump `Cargo.lock` (legacy plugin); the staged-set assertion in the bot push step caught the drift | Now solved: `/release-workflows:release` runs `update-version.sh` which always regenerates `Cargo.lock`. |

## Adopting the convention (for new contributors)

Read [`cc-plugins/plugins/release-workflows/references/convention.md`](https://github.com/charliek/cc-plugins/blob/main/plugins/release-workflows/references/convention.md)
in the framework repo. It defines the contract every file in this repo's
`scripts/release/` and `.github/workflows/release.yml` is written against.

## Notes for this repo

- The `mac` job's "Guard against the throwaway Sparkle key on stable
  releases" step is a transitional safety net from the Sparkle 2 spike
  (issue #122). It only fires on stable tags; prereleases bypass it
  intentionally so the throwaway-key path can be tested.
- Sparkle appcast publishing is **split across two jobs**. Signing stays in
  `mac` — it is the only job holding the SwiftPM artifacts that carry
  `sign_update`, and a separate job would have to rebuild or cross-job-cache
  them. Writing the feed lives in `appcast`, which cannot run until the
  Release is published, because the enclosure URL is a public
  `/releases/download/` link that a draft's assets do not answer. The whole
  `sign.txt` travels between them as a build artifact: it carries both
  `sparkle:edSignature` and `length`, and the updater needs both. Upside over
  the old inline arrangement: an appcast failure now costs a ~1-minute job
  re-run instead of the whole ~5-minute mac job.
