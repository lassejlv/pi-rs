#!/usr/bin/env bash
# Embedded-package, native-suite, example/bench, wasm32 and optional browser/workerd gate parts.
# Run from scripts/check-durable-port.sh, or alone: scripts/check-durable-embedded.sh [../pi-mono]
#
# Everything runs ONE thing at a time. All builds finish before any timing-sensitive file runs, and no
# cargo command ever overlaps another command, so deadlines are not shared with compilation.
#
# Optional parts (off unless asked; they need local tooling):
#   DURABLE_GATE_BROWSER=1  tests/indexeddb_browser/run.sh   needs PLAYWRIGHT_CORE and CHROME
#   DURABLE_GATE_WORKERD=1  tests/workerd/run.sh             needs node/npm and network for the first install
set -euo pipefail
export RUST_TEST_THREADS=${RUST_TEST_THREADS:-4}
export CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-0}
cd "$(dirname "${BASH_SOURCE[0]}")/.."
reference=${1:-../pi-mono}
target=${CARGO_TARGET_DIR:-target}
work=$(mktemp -d "${TMPDIR:-/tmp}/pi-durable-embedded.XXXXXX")
trap 'rm -rf "$work"' EXIT

echo "== embedded: lint and native suites"
cargo fmt -p pi-durable-jsrt --check
cargo clippy --locked -p pi-durable-jsrt -p pi-jsrt --all-targets -- -D warnings
[ -d crates/pi-durable-jsrt/tests/indexeddb/node_modules/fake-indexeddb ] ||
  (cd crates/pi-durable-jsrt/tests/indexeddb && npm ci --silent)
PI_DURABLE_REQUIRE_INDEXEDDB=1 cargo test --locked -p pi-durable-jsrt -p pi-jsrt
if [ -f crates/pi-coding-agent/tests/extensions_durable.rs ]; then
  cargo test --locked -p pi-coding-agent --test extensions_durable
fi

echo "== embedded: build the stock runner (release, plain and with the transaction probes)"
cargo build --locked --release -p pi-durable-jsrt --example embedded_stock
cp "$target/release/examples/embedded_stock" "$work/embedded_stock"
cargo build --locked --release -p pi-durable-jsrt --features stock-test-probes --example embedded_stock
cp "$target/release/examples/embedded_stock" "$work/embedded_stock_probes"

echo "== embedded: examples and benches built before anything timing-sensitive runs"
cargo build --locked -p pi-durable --examples
cargo bench --locked -p pi-durable --no-run

echo "== embedded: acceptance matrix (unchanged pinned bytes, both retention modes, native calls > 0)"
python3 scripts/embedded-matrix.py excluded | while IFS=$'\t' read -r suite reason; do
  printf '   n/a %s: %s\n' "$suite" "$reason"
done
reports="$work/reports"
mkdir -p "$reports"
python3 scripts/embedded-matrix.py list | while IFS=$'\t' read -r suite build report; do
  runner="$work/embedded_stock"
  [ "$build" = probes ] && runner="$work/embedded_stock_probes"
  "$runner" "$reference" --suite="$suite" --report="$reports/$report"
done
python3 scripts/embedded-matrix.py verify "$reports"

echo "== examples (offline) and benches (quick mode)"
for source in crates/pi-durable/examples/ex_*.rs; do
  name=$(basename "$source" .rs)
  # ex_15 talks to a real model; the stock bridges are driven by their own gates.
  [ "$name" = ex_15_real_model ] && continue
  "$target/debug/examples/$name" > /dev/null
done
for source in crates/pi-durable/benches/*.rs; do
  PI_DURABLE_BENCH_QUICK=1 cargo bench --locked -p pi-durable --bench "$(basename "$source" .rs)"
done

echo "== wasm32"
cargo check --locked -p pi-durable --target wasm32-unknown-unknown --no-default-features --features indexeddb,cloudflare
cargo check --locked -p pi-chord --target wasm32-unknown-unknown

if [ "${DURABLE_GATE_BROWSER:-}" = 1 ]; then
  echo "== browser (IndexedDB in headless Chrome)"
  : "${PLAYWRIGHT_CORE:?set PLAYWRIGHT_CORE}" "${CHROME:?set CHROME}"
  crates/pi-durable/tests/indexeddb_browser/run.sh
fi
if [ "${DURABLE_GATE_WORKERD:-}" = 1 ]; then
  echo "== workerd (Rust Durable Object)"
  crates/pi-durable/tests/workerd/run.sh
fi
printf 'Embedded durable checks passed.\n'
