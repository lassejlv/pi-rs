#!/usr/bin/env bash
# Run pi-durable's IndexedDB storage, built from the crate for wasm32 with the
# `indexeddb` feature, against real IndexedDB in headless Chrome: the reference
# storage conformance traces (plain and across reopen), browser-vs-host-model
# differential scans, transaction semantics, and row interchange with the
# reference in both directions (fixtures/indexeddb-interchange.json).
#
#   PLAYWRIGHT_CORE=<playwright-core package dir> CHROME=<Chrome binary> [WASM_BINDGEN=<wasm-bindgen 0.2.129>] run.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
: "${PLAYWRIGHT_CORE:?set PLAYWRIGHT_CORE to a playwright-core package directory}"
: "${CHROME:?set CHROME to a Chrome or Chromium executable}"
target="${CARGO_TARGET_DIR:-$here/target}/indexeddb-browser"
"$here/../wasm32_gate/check.sh"
cargo build --manifest-path "$here/Cargo.toml" --target wasm32-unknown-unknown --target-dir "$target"
bindgen="${WASM_BINDGEN:-$target/wasm-bindgen-cli/bin/wasm-bindgen}"
# The CLI must match the wasm-bindgen library version exactly.
[ -x "$bindgen" ] || cargo install wasm-bindgen-cli --version 0.2.129 --locked --root "$target/wasm-bindgen-cli"
site="$(mktemp -d)"
trap 'rm -rf "$site"' EXIT
"$bindgen" --target web --out-dir "$site/pkg" "$target/wasm32-unknown-unknown/debug/pi_durable_indexeddb_browser.wasm"
cp "$here/index.html" "$here/driver.mjs" "$site/"
cp "$here/../fixtures/indexeddb-interchange.json" "$site/fixture.json"
cp "$here/../fixtures/storage-indexeddb.json" "$here/../fixtures/storage-indexeddb-reopen.json" "$site/"
mkdir "$site/node_modules"
ln -s "$PLAYWRIGHT_CORE" "$site/node_modules/playwright-core"
CHROME="$CHROME" node "$site/driver.mjs"
