# Sparkle signing key (Roost.app)

`Roost.app` updates itself through Sparkle from one feed,
`https://charliek.github.io/roost/appcast.xml` (`docs/appcast.xml`). Every
update is signed with one EdDSA keypair, and every installed `Roost.app`
refuses an update its `SUPublicEDKey` does not verify.

| Half | Where it lives |
|------|----------------|
| Public | `mac/keys/roost-sparkle-ed-public-key.txt`, **committed**. `mac/scripts/bundle-iced.sh` stamps it into the production bundle's `Info.plist` as `SUPublicEDKey` (release.yml passes it as `ROOST_BUNDLE_SPARKLE_ED_PUBLIC_KEY`). |
| Private | The `SPARKLE_ED_PRIVATE_KEY` repo secret. Only `release.yml`'s `mac` job reads it: first to check the pair, then to sign the DMG for the appcast entry. |

This is the key the Swift `Roost.app` shipped with (v0.0.27 and earlier), not
a new one. The iced `Roost.app` that replaces it keeps the same bundle id and
the same key, so an installed Swift app takes the iced build as an ordinary
update. The public key is pinned in two more places that must change together
with this file: `PINNED_PUBLIC_KEY` in `mac/scripts/verify-sparkle-key.sh` and
`tools/roosttest_unit/test_mac_entitlements.py`.

Dev builds (`Roost-Iced.app`, `ai.stridelabs.Roost.iced`) are feedless unless
both Sparkle values are given at bundle time, and the e2e lanes give them the
TEST-ONLY fixture key (`tools/roosttest/fixtures/sparkle/`), never this one.

## The release check

`mac/scripts/verify-sparkle-key.sh mac/keys/roost-sparkle-ed-public-key.txt`
runs before the `mac` job builds anything. It refuses the TEST-ONLY fixture
key and the throwaway spike key, requires the committed public key to equal
the pinned literal, derives the public half of `SPARKLE_ED_PRIVATE_KEY` and
compares it, and signs a probe file with `sign_update` and verifies the
signature against the committed key. `mac/scripts/verify-sparkle-key_test.sh`
covers it with generated keys (`make test-mac-scripts`). It never prints key
material.

## Encoding convention

The secret's value is base64 of the key **file** `generate_keys -x` writes
(that file's own contents are already base64 of the key). The release job
decodes one layer and hands the result to `sign_update --ed-key-file`:

```bash
printf '%s' "$SPARKLE_ED_PRIVATE_KEY" | base64 --decode > "$W/key"
```

The key's own format was never recorded, so the check accepts both formats
`sign_update` reads: a 32-byte ed25519 seed, or the legacy 96-byte
`sk(64)||pk(32)` layout.

## Rotation

Releases do **not** rotate this key. Every `Roost.app` ever shipped trusts the
committed `SUPublicEDKey`, so rotating it strands every installed copy: they
refuse the new signatures and never auto-update again. If a rotation is ever
unavoidable, ship the new public key in a build users install **by hand**
first, and only then start signing with the new private half.
