# Sparkle signing keys (Roost-Iced)

The experimental **Roost-Iced** macOS build ships its own Sparkle feed
(`docs/appcast-iced.xml`) signed with its **own** EdDSA keypair — never the
Swift app's. Two bundles, two feeds, two keypairs: a shared key would let
either app offer the other's updates, which is exactly the failure the
separate-feed requirement exists to prevent
(`docs/development/iced-migration-roadmap.md`, "separate keypair").

| Half | Where it lives |
|------|----------------|
| Public | `mac/keys/roost-iced-sparkle-ed-public-key.txt` — **committed** to the repo. `mac/scripts/bundle-iced.sh` stamps it into `Roost-Iced.app`'s `Info.plist` as `SUPublicEDKey` at bundle time (release.yml passes it via `ROOST_ICED_SPARKLE_ED_PUBLIC_KEY`). |
| Private | The `ROOST_ICED_SPARKLE_ED_PRIVATE_KEY` repo secret. Only `release.yml`'s `mac-iced` job reads it, to sign the DMG for the appcast entry. |

The public key file is **deliberately absent** from this directory until a
maintainer generates the real keypair — no placeholder, because a placeholder
could ship in a bundle. `release.yml`'s `mac-iced` job fails fast (before
building) when the file is missing, empty, identical to the TEST-ONLY test
fixture, or when the secret is unset or not a decodable ed25519 key.

The Swift app's counterpart is `SPARKLE_ED_PRIVATE_KEY` + the public key
committed in `mac/Resources/Info.plist.template`. Nothing here touches either.

## Encoding convention

`ROOST_ICED_SPARKLE_ED_PRIVATE_KEY` uses **exactly** the convention
`SPARKLE_ED_PRIVATE_KEY` uses: the secret's value is base64 of the key **file**
`generate_keys -x` wrote (that file's own contents are already base64 of the
32-byte ed25519 seed). The job decodes one layer —

```bash
printf '%s' "$KEY" | base64 --decode > "$W/key"
```

— and hands `$W/key` to `sign_update --ed-key-file`.

## Generating + installing the keypair (one time)

Run all of this from the repo root, on a trusted machine.

**1. Stage Sparkle's tools.**

```bash
./third_party/sparkle/fetch.sh
```

**2. Generate the keypair under its OWN keychain account.**

`generate_keys` stores the private half in the login Keychain, and *without*
`--account` it uses the default `ed25519` account — which already holds the
**Swift** app's key and which `generate_keys` would silently reuse rather than
override ("If a private key was already generated in your Keychain, that key
will be used and not overridden"). Always pass `--account roost-iced`.

```bash
./third_party/sparkle/out/bin/generate_keys --account roost-iced
```

**3. Commit the public half.**

```bash
./third_party/sparkle/out/bin/generate_keys --account roost-iced -p \
  > mac/keys/roost-iced-sparkle-ed-public-key.txt
git add mac/keys/roost-iced-sparkle-ed-public-key.txt
```

**4. Export the private half to a file.**

```bash
umask 077
./third_party/sparkle/out/bin/generate_keys --account roost-iced \
  -x "$TMPDIR/roost-iced-private.key"
```

**5. Set the repo secret** (base64 of that file — see the convention above).

```bash
gh secret set ROOST_ICED_SPARKLE_ED_PRIVATE_KEY \
  --body "$(base64 < "$TMPDIR/roost-iced-private.key")"
```

**6. Verify the two halves are a pair — do not skip.**

A public/private mismatch is silent client-side: Sparkle just reports "no
valid update" forever, and nothing in the release pipeline can notice. Prove
the pair with Sparkle's own tools (no OpenSSL — macOS's LibreSSL cannot
handle ed25519 keys):

```bash
# a) the committed public half is the one belonging to the keychain item
diff <(./third_party/sparkle/out/bin/generate_keys --account roost-iced -p) \
     mac/keys/roost-iced-sparkle-ed-public-key.txt \
  && echo "public half OK"

# b) the EXPORTED file is the same key as that keychain item: Ed25519 is
#    deterministic, so signing identical bytes with both must produce
#    byte-identical signatures.
scratch="$(mktemp)"; head -c 4096 /dev/urandom > "$scratch"
sig_from_keychain="$(./third_party/sparkle/out/bin/sign_update \
  --account roost-iced -p "$scratch")"
sig_from_file="$(./third_party/sparkle/out/bin/sign_update \
  --ed-key-file "$TMPDIR/roost-iced-private.key" -p "$scratch")"
[ "$sig_from_keychain" = "$sig_from_file" ] && echo "private half OK"

# c) the signature actually verifies
./third_party/sparkle/out/bin/sign_update --verify \
  --ed-key-file "$TMPDIR/roost-iced-private.key" "$scratch" "$sig_from_file" \
  && echo "signature verifies"

# d) the secret's encoding round-trips (what release.yml will decode)
base64 < "$TMPDIR/roost-iced-private.key" | base64 --decode \
  | diff - "$TMPDIR/roost-iced-private.key" && echo "encoding OK"

rm -f "$scratch"
```

(a) + (b) together are the pair proof: the committed public key belongs to the
keychain item, and the exported file *is* that keychain item.

**7. Delete the local private material.**

```bash
rm -f "$TMPDIR/roost-iced-private.key"
```

The Keychain copy stays (it is the recovery path if the secret is ever lost);
the exported file must not linger on disk.

## Rotation

Releases do **not** rotate this key. Every `Roost-Iced.app` we ship stamps the
same committed `SUPublicEDKey`, so rotating it strands every already-installed
build: they will refuse the new signatures and never auto-update again. If a
rotation is ever unavoidable, ship the new public key in a build users install
**by hand** first, and only then start signing with the new private half.
