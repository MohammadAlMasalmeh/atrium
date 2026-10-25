# Releasing Aside

Details and test evidence are in [docs/chromium.md](docs/chromium.md#releases-signing-and-updates).

## One-time setup

1. **Update signing key.** Build the tool, then generate a key pair on a trusted machine:

   ```sh
   cargo build --release --bin aside_sign_update
   target/release/aside_sign_update keygen --secret ~/aside-update.pk8 --public aside-update.pub
   ```

   Put the public key line into `assets/update-public-key.txt` and commit it (or set the
   repository variable `ASIDE_UPDATE_PUBLIC_KEY`). Keep the secret key out of the repository;
   store it as the `ASIDE_UPDATE_SECRET_KEY` Actions secret (the file's contents). Builds
   without a public key have the updater disabled.
2. **Developer ID certificate** (needs an Apple Developer account). Export the
   "Developer ID Application" certificate with its private key as a `.p12` and add Actions
   secrets `MACOS_CERTIFICATE_P12` (base64 of the file), `MACOS_CERTIFICATE_PASSWORD`, and
   `MACOS_SIGN_IDENTITY` (e.g. `Developer ID Application: Name (TEAMID)`).
3. **Notarization.** Create an App Store Connect API key and add `NOTARY_API_KEY_P8`
   (file contents), `NOTARY_API_KEY_ID`, and `NOTARY_API_ISSUER`.

Every secret is optional: without the certificate the release is ad hoc signed, without
notary credentials notarization is skipped, and without the update key the archive has no
`.sig` (installed apps will refuse it).

## Cutting a release

1. Bump `version` in `Cargo.toml`, run `cargo build` to refresh `Cargo.lock`, and commit.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.
3. The **Release** workflow checks the tag matches the crate version, runs the tests, and runs
   `tools/release.sh`, which builds, signs (hardened runtime, per-helper entitlements),
   notarizes and staples the app, writes `Aside-<version>-macos-<arch>.zip` plus its `.sig`,
   builds, signs, notarizes, and staples the DMG, mounts it invisibly to check it, and writes
   `SHA256SUMS`. It then publishes everything as a GitHub release, which is the default
   update feed (`ASIDE_UPDATE_REPO` is set to the repository at build time).

## Local release build

```sh
ASIDE_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
ASIDE_NOTARY_PROFILE=aside-notary \
ASIDE_UPDATE_SECRET_KEY=~/aside-update.pk8 \
  ./tools/release.sh
```

## Testing the updater and crash reports

Both tests start background-only copies under `/private/tmp` and never touch
`/Applications`. They still start the app, so run them when that is acceptable.

```sh
target/release/aside_sign_update keygen --secret /private/tmp/test.pk8 --public /private/tmp/test.pub
ASIDE_UPDATE_PUBLIC_KEY=$(cat /private/tmp/test.pub) ./tools/build-chromium.sh --release
python3 tools/fixture_server.py --port 7801 &
python3 tools/test_crash_reports.py --fixture http://127.0.0.1:7801
python3 tools/test_updater.py --secret /private/tmp/test.pk8
```

Rebuild without `ASIDE_UPDATE_PUBLIC_KEY` afterwards.

`ASIDE_NOTARY_PROFILE` is a profile saved with `xcrun notarytool store-credentials`
(this stores the credentials in your login keychain). Leave all variables unset for an ad
hoc, unnotarized test build in `dist/`.
