#!/usr/bin/env bash
# wasm32 gate: every feature combination of pi-durable (and the pi-ai, pi-chord,
# pi-telemetry code it pulls in) builds for wasm32-unknown-unknown with clippy
# -D warnings, and none of it calls a runtime/clock API that panics there
# (see clippy.toml).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
export CLIPPY_CONF_DIR="$here"
for features in "" indexeddb cloudflare indexeddb,cloudflare; do
	echo "wasm32 clippy: pi-durable features [$features]"
	cargo clippy --manifest-path "$here/../../Cargo.toml" -p pi-durable -p pi-chord -p pi-ai -p pi-telemetry \
		--no-deps --target wasm32-unknown-unknown --no-default-features --features "$features" \
		-- -D warnings -D clippy::disallowed_methods
done
