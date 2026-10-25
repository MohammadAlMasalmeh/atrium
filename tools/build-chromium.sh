#!/bin/sh
# Build the Chromium runtime and the correctly structured macOS app bundle.
set -eu
cd "$(dirname "$0")/.."
if [ -x "$PWD/target/build-tools/bin/ninja" ]; then
    PATH="$PWD/target/build-tools/bin:$PATH"
    export PATH
fi
if ! command -v ninja >/dev/null 2>&1; then
    echo 'CEF requires Ninja. Install ninja, or run: python3 -m pip install --target target/build-tools ninja' >&2
    exit 1
fi
# A stable runtime cache avoids downloading a new CEF copy for each Cargo profile.
CEF_PATH="${CEF_PATH:-$PWD/target/cef}"
export CEF_PATH
cargo build --locked --bin atrium --bin atrium_helper "$@"
# Build the bundler with the same profile; otherwise it recompiles CEF and the app in debug.
cargo run --locked "$@" --bin atrium_bundle -- "$@"
