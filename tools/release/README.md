# `tools/release/` — release tooling

## `dry_run.py`: a publish-free copy of `release.yml`

```
python3 tools/release/dry_run.py [--out .github/workflows/release-dryrun.yml]
```

Reads `.github/workflows/release.yml` and writes a workflow that builds every
release artifact and publishes nothing. It is stdlib only and edits the text
(a YAML round-trip would read `on:` as `True`, reformat every `run:` block and
need PyYAML). Whatever it expects to edit and cannot find stops the run with an
error naming it, so a `release.yml` change it doesn't understand is loud.
`tools/roosttest_unit/test_release_dry_run.py` runs it on the real file.

What the output does differently:

- Triggers on a push to `dryrun/**`, not on a tag.
- Drops `ci-gate`, `create-release`, `publish-release`, `appcast`,
  `appcast-iced` and `dispatch-apt-charliek`. The build jobs keep
  `needs: version-check` where they had `needs: create-release`.
- Replaces `GITHUB_REF_NAME` and `github.ref_name` with `DRYRUN_TAG`, a
  workflow-level `v<[workspace.package].version>`, so version stamping,
  `version-check`, `ROOST_RELEASE_BUILD` and the identity checks all agree.
- Puts a `gh` shim first on `PATH` in each build job. `gh release upload`
  copies each file to `$RUNNER_TEMP/dryrun/`; `gh release view` and `edit`
  fail the job; anything else goes to the real `gh`. The upload steps' own
  checks (the `.deb` architecture match, for one) still run.
- Deletes the release-body edit, every Sparkle signing step with its
  `sign*.txt` artifact, and **every step that references a secret other than
  `GITHUB_TOKEN`**: the Sparkle key checks, the certificate import and both
  notarization steps. No kept step reads a production secret, and the tool
  fails if any `secrets.*` reference is left in the output. The
  `ROOST_DEVELOPER_ID_IDENTITY` expressions on the bundle and DMG steps
  become `''`. A kept step that reads the output of a deleted one fails the
  run.
- Forces `CAN_NOTARIZE: 'false'` as well, so the remaining
  `env.CAN_NOTARIZE == 'true'` steps (signature assertions) skip.
- Fails if a kept job has an `if:` on `github.ref`, a tag or `ref_name`: on a
  `dryrun/**` branch it would skip the job and the run could still go green.
  The concurrency group keeps `github.ref`.
- Ends each build job with one `actions/upload-artifact` step
  (`dryrun-<job>-<arch>`, `retention-days: 1`, `if-no-files-found: error`).
- Runs with `contents: read` at the workflow and job level.

### What a dry run covers

Covered: the libghostty and Rust builds, the `.deb` and DMG packaging, the
packaged-artifact smoke, the release-identity and tag-version assertions, the
E2E against the release bundles and the `roost-session` staging.

Not covered: notarization and stapling, the Sparkle key checks, Sparkle signing, the draft release,
publishing, the appcast commit and the apt dispatch.

### Procedure

1. Run the tool. Push `dryrun/<topic>-<short sha>` as the commit under test
   plus one commit adding the generated `release-dryrun.yml`. The file is
   never committed anywhere else.
2. Watch the `Release dry run` run to green.
3. Check nothing was published: `gh release list` is unchanged, there is no
   `dispatch-apt-charliek` run, and no appcast commit landed on `main`.
4. Delete the `dryrun/**` branch.
5. Record the run URL and the coverage above in the PR body.

The release cut re-runs the same tool.
