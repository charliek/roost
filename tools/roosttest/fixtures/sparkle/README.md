# Sparkle test fixtures (TEST-ONLY)

Backing data for [`tools/roosttest/test_sparkle.py`](../../test_sparkle.py)
— the plan 028 § 3.11 verification of the 6c update machinery against a
loopback appcast. Nothing here is used by, or reachable from, a shipped
build.

## `TEST-ONLY-public-ed-key.txt`

The EdDSA public key stamped into the **test-keyed** bundle as
`SUPublicEDKey` by `make e2e-iced-sparkle` (which reads this file and
passes it through `bundle-iced.sh`'s `ROOST_ICED_SPARKLE_ED_PUBLIC_KEY`
env pair). Sparkle 2 will not report a valid update for an ad-hoc-signed
bundle with no public key, so the key is what makes AC6's `found`
outcome reachable at all.

CI's **default** `make bundle-iced` assemble stays keyless and feedless —
the shipped posture. Only the Sparkle lane re-assembles with these
values.

The real feed, whenever Roost-Iced grows one, gets its own separate
keypair and its own separate feed URL (plan 028 § 3.9). This key must
never appear in a shipped build.

### How it was generated

```sh
third_party/sparkle/fetch.sh                     # stages out/bin/generate_keys
third_party/sparkle/out/bin/generate_keys --account roost-test-only-ed25519
# → prints the SUPublicEDKey value committed here
third_party/sparkle/out/bin/generate_keys --account roost-test-only-ed25519 \
    -x /tmp/roost-test-only-private-ed-key.txt   # export, for the signing arm
security delete-generic-password -a roost-test-only-ed25519 \
    -s "https://sparkle-project.org"             # remove it from the keychain
```

`generate_keys` only ever writes the private half to the login keychain,
so the export + delete pair is how a throwaway test key gets out of it
without leaving a stray signing key on the machine.

## `TEST-ONLY-private-ed-key.txt` — absent by design

The plan's recorded fallback arm (§ 3.11): an information-only check
(`checkForUpdateInformation`) does **not** verify enclosure signatures,
so the public key alone is enough and no private key needs to live in
the repo. `test_sparkle.py`'s `_signature_attributes` turns signing on
automatically if this file ever appears — it signs the fixture enclosure
with the staged `sign_update` and fills the appcast's `@SIGNATURE@`
placeholder — so committing the exported private half here (under this
exact name) is the whole change if a future Sparkle starts filtering
unsigned items at appcast-parse time.

Verified on Sparkle 2.9.x (2026-08-16): unsigned items parse fine and
the check reports `found`, so the private key stays out of the repo.

## `appcast.xml.template`

Rendered at test time with the offered version, the loopback server's
ephemeral port, and (only under the arm above) the enclosure signature.
The offered version is deliberately far above any real Roost version so
the check's outcome never drifts with the workspace version. The
enclosure is written but never downloaded — the check stops after the
version comparison.
