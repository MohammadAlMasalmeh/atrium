# Contributing

<!-- PRODUCT NAME: "Atrium" below is a placeholder; update it when the project is renamed. -->

Thanks for helping. This is an experimental macOS browser in Rust that embeds
Chromium through CEF. Read [docs/chromium.md](docs/chromium.md) first; it explains
the architecture, what is verified, and what is missing.

## Build

Requirements: macOS (Apple Silicon is what has been exercised), Rust stable, Xcode
command-line tools, CMake, and Ninja. The first build downloads the pinned CEF
runtime (about 135 MB compressed).

```sh
python3 -m pip install --target target/build-tools ninja   # if Ninja is missing
./tools/build-chromium.sh --release
```

The app is `target/bundle/atrium.app`. Always run the bundled executable
(`target/bundle/atrium.app/Contents/MacOS/atrium`); `target/release/atrium` cannot
find the Chromium framework. Set `CEF_PATH` to share one CEF download between
checkouts.

## Test

Rust unit tests:

```sh
CEF_PATH="$PWD/target/cef" PATH="$PWD/target/build-tools/bin:$PATH" cargo test --release --locked
```

End-to-end checks drive a running instance through the local API. **Run test
instances in background mode with a throwaway profile**, so they never take focus,
show a window, or prompt for the keychain:

```sh
ATRIUM_MOCK_KEYCHAIN=1 ATRIUM_BACKGROUND_WINDOW=1 \
  ./target/bundle/atrium.app/Contents/MacOS/atrium \
  --data-dir /private/tmp/atrium-test-1 run --port 7791 \
  --opencode-url http://127.0.0.1:1
```

- `ATRIUM_MOCK_KEYCHAIN=1` uses Chromium's mock keychain. Never use it with a real
  profile.
- `ATRIUM_BACKGROUND_WINDOW=1` makes the window invisible, click-through, and
  unable to activate. Keyboard focus and native dialogs cannot be tested this way.
- `--opencode-url http://127.0.0.1:1` keeps the assistant disconnected.
API scripts use `tools/atrium_api.py`, which sends the per-launch token from
`<data dir>/api-token`. Available checks (see `docs/chromium.md` for the full
commands): `smoke_chromium.py`, `test_tab_lifecycle.py`,
`test_profile_persistence.py`, and `test_about.py`. New features need a
repeatable `tools/test_<feature>.py` that exits non-zero on failure.

## Style

- Match the surrounding code: naming, density, and idiom. Keep the existing UI
  shape; add UI only where a feature needs it.
- Comment only for constraints the code can't show.
- Small commits with clear messages; the build and all Rust tests pass before each.
- Update `docs/chromium.md` honestly: what works, how it was tested, what remains.
- Prefer no new crates. If one is needed, regenerate the notices (below) and check
  its license.

## Dependency licenses

`THIRD-PARTY-NOTICES.txt` is generated from `Cargo.lock`:

```sh
python3 tools/gen_third_party_notices.py          # regenerate
python3 tools/gen_third_party_notices.py --check  # fail if out of date
python3 tools/gen_third_party_notices.py --report # print flagged licenses
```

Run it after any dependency change and commit the result; the About page embeds the
file at compile time. The script flags copyleft and uncommon licenses. Do not add
GPL, LGPL, or AGPL crates without the maintainer's agreement.

## Privacy

Any change that adds a network destination or stores new data must update
[PRIVACY.md](PRIVACY.md). Do not add telemetry or third-party services to the
shell; for example, favicons come from the local cache, not an icon service.

## Renaming the product

The product name and bundle identifier are set in `src/brand.rs`
(`PRODUCT_NAME`, `BUNDLE_ID`). They flow into the window title, app menu, About
page, shell title, user-facing errors, and the bundle's `Info.plist`. After
changing them, update the placeholders marked `PRODUCT NAME` in the Markdown files.
A new bundle identifier makes macOS treat the app as a different application, so
privacy permissions (camera, microphone, and so on) are requested again.

These identifiers are deliberately **not** tied to the product name, because
changing them breaks existing profiles, scripts, or integrations. Change them only
on purpose, together:

| Identifier | Where | Effect of changing |
| --- | --- | --- |
| `.atrium-rs` data directory | `brand::DATA_DIR` | Existing profiles are no longer found (migrate them) |
| `atrium` executable and `atrium.app` | `brand::EXECUTABLE`, `[[bin]]` in `Cargo.toml`, `tools/*.py` defaults, docs | Build and script paths change. The bundle itself is `<PRODUCT_NAME>.app`; `target/bundle/atrium.app` is a symlink to it |
| `atrium_helper` helper binary | `Cargo.toml`, `src/bin/atrium_bundle.rs` | Bundle layout |
| `ATRIUM_*` environment variables, `x-atrium-token` header | `src/main.rs`, `src/chromium/mac.rs`, `src/app_cef.rs`, `tools/atrium_api.py` | Test tooling and clients |
| `atrium_*` MCP tool names, `atrium` MCP server name | `src/agent.rs`, `src/main.rs` | OpenCode sessions and prompts |
| `atrium.` local-storage prefix, `__atrium` page globals | `assets/` | Saved shell settings and history |
| Cargo package name `atrium` | `Cargo.toml` | Crate name only |
