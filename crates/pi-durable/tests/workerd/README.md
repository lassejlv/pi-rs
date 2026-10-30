# Rust Durable Object in workerd

Runs upstream `packages/durable/test-workers` (`storage.test.ts`, `wake.test.ts`) against the Rust
`pi-durable` Durable Object inside real workerd, through `@cloudflare/vitest-plugin` (the plugin
upstream's `test-workers/vitest.config.ts` uses).

- `worker/` is the Rust object (wasm-bindgen `Core`, built on its own: `[workspace]` is declared).
- `worker.mjs` is the thin JS Durable Object class: lifecycle only.
- `run.sh` checks that the whole crate builds for wasm32 with `--features cloudflare`, builds the worker,
  generates the wasm-bindgen glue into `pkg/`, and runs vitest. `run.sh --check` runs only the gate.

Differences from upstream: the storage conformance cases and the migration scenario run in Rust (the
object's SQLite is opened by the Rust adapter), and `instance.harness.*` accessors are replaced by the
methods of `worker.mjs`.
