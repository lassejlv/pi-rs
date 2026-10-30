#!/usr/bin/env bash
# Run the Rust Durable Object in real workerd with upstream's test-workers scenarios
# (storage.test.ts, alarm host). wake.test.ts.parked waits for the Harness on wasm32:
# rename it back and build the worker with `--features wake` then.
#
#   run.sh            build the wasm, install npm packages, run vitest in workerd
#   run.sh --check    only the wasm32 build gate
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
target="${CARGO_TARGET_DIR:-$here/target}/workerd"

# The gate: the whole crate, not a stand-in, must build for wasm32.
cargo check --manifest-path "$here/../../Cargo.toml" --target wasm32-unknown-unknown --no-default-features --features indexeddb,cloudflare --target-dir "$target"
[ "${1:-}" = "--check" ] && exit 0

cargo build --manifest-path "$here/worker/Cargo.toml" --target wasm32-unknown-unknown --target-dir "$target"
bindgen="$target/wasm-bindgen-cli/bin/wasm-bindgen"
# The CLI must match the wasm-bindgen library version exactly.
[ -x "$bindgen" ] || cargo install wasm-bindgen-cli --version 0.2.129 --locked --root "$target/wasm-bindgen-cli"
rm -rf "$here/pkg"
"$bindgen" --target web --out-dir "$here/pkg" "$target/wasm32-unknown-unknown/debug/pi_durable_workerd.wasm"

cd "$here"
[ -d node_modules ] || npm install --no-audit --no-fund --legacy-peer-deps
npx vitest --run
