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
   - `create-release` — extract CHANGELOG section → `gh release create`
   - `linux` (amd64 + arm64 matrix) — build + upload `roost_X.Y.Z_<arch>.deb`
   - `mac` — build + sign + notarize + upload `Roost-X.Y.Z.dmg`, then
     EdDSA-sign the DMG and bot-push the appcast entry to main (Sparkle
     auto-update). The mac job's "Append macOS first-launch note" step
     keeps the Gatekeeper bypass instructions on the Release body while
     the DMG is not notarized.
   - `dispatch-apt-charliek` — fire a `repository_dispatch` at
     `charliek/apt-charliek` so the .debs land on `apt.stridelabs.ai`.
     Uses a release-bot App token scoped to `apt-charliek` (no
     per-pipeline PAT); the App must be installed on apt-charliek too.

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
| `SPARKLE_ED_PRIVATE_KEY` | EdDSA signing key for Sparkle appcast, base64-encoded | required for stable releases (a `*-beta`/`*-rc` build skips signing) |
| `APT_DISPATCH_TOKEN` | Legacy PAT — superseded by the release-bot App; can be removed once you're sure the App-based dispatch is working | optional / deprecated |
| `MACOS_CERTIFICATE_P12_BASE64` + `MACOS_CERTIFICATE_PASSWORD` + `APPLE_ID` + `APPLE_TEAM_ID` + `APPLE_APP_SPECIFIC_PASSWORD` + `ROOST_DEVELOPER_ID_IDENTITY` | Mac code-signing + notarization | **set** (2026-06-28; #83 closed) — DMG is Developer ID signed + notarized. `MACOS_CERTIFICATE_P12_BASE64` is the `HAS_CERT` gate; unset all six → ad-hoc-signed DMG with the Gatekeeper-bypass note |

The cert + Apple creds are kept locally — git-ignored, synced across machines
via envsecrets (the `# envsecrets` marker in `.gitignore`) — at
`.secrets/cert.p12` + `.secrets/apple.env`. `envsecrets pull` restores them on a
new machine; source `apple.env` for a local notarized build.

## Branch protection

`main` is protected by ruleset `main-protection` (id `17018841`) with
`required_status_checks=['ci-success']`. Two bypass actors:

- `charliek-release-bot` (App id `3902108`, type `Integration`) — lets the
  bot push the appcast commit after the mac job builds + signs the DMG
- Admin role (id `5`, type `RepositoryRole`) — lets `/release-workflows:release`'s
  push of the changelog + version commits + tag land before `ci-success`
  exists on those new commits

Inspect or edit at https://github.com/charliek/roost/rules.

## The appcast lives where

The Sparkle appcast is at `docs/appcast.xml`, served by GitHub Pages from
`https://charliek.github.io/roost/appcast.xml` via `docs.yml`'s mkdocs
deploy. The mac job's appcast steps mutate that file in place, commit it
as the release-bot, and push to main; `docs.yml` redeploys Pages
shortly after.

The appcast updater script is `mac/scripts/update-appcast.py`. It reads
`ROOST_VERSION`, `ROOST_TAG`, and `ROOST_SIGN_FILE` from the environment
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
| `mac` job fails at "Sign DMG + append appcast entry" with `SPARKLE_ED_PRIVATE_KEY secret is unset` | Stable release without the signing secret | Set the secret; re-run the mac job, OR cut the release as `vX.Y.Z-beta1` (the throwaway-key guard at the top of the mac job only enforces the real key for stable tags) |
| `mac` job fails at "Push signed appcast" with `protected branch hook declined` | App removed from ruleset bypass | Re-add `{ actor_id: 3902108, actor_type: "Integration" }` to `main-protection`'s `bypass_actors` |
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
- The Sparkle appcast steps live INSIDE the mac job (not as a separate
  `appcast` job). The framework's job-sparkle-appcast template assumes a
  cross-job sign_update binary; roost keeps it inline because the mac job
  has the SwiftPM artifacts already and a separate job would need to
  rebuild or cross-job-cache them. Trade-off: a failure in just the
  appcast step requires re-running the whole mac job (~5 min).
