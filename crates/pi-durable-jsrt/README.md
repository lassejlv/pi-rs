# pi-durable-jsrt

Serves `@earendil-works/pi-durable` and `@earendil-works/chord` to pi extensions as embedded
modules inside [pi-jsrt](../pi-jsrt), the QuickJS-based JavaScript runtime pi extensions run on.
It's thin JS glue (`js/`) over the native `pi-durable` and `pi-chord` crates, no `node_modules`,
no N-API, no wasm. The import names match the TypeScript packages exactly, so an extension
written against `@earendil-works/pi-durable` runs unchanged, in process, on the native port.

`pi-coding-agent` mounts it next to `pi-ai` in the extension host, so any extension can `import`
these packages. See
[`durable-demo.ts`](../pi-coding-agent/examples/extensions/durable-demo.ts) for a worked example
(a SQLite-backed counter and a `Harness` turn against the faux provider), and
[the port status](../../porting/durable/STATUS.md) for current scope and test counts.

Install the host in a runtime and call a module function:

```rust
let rt = pi_jsrt::JsRuntime::new(pi_jsrt::RuntimeOptions::default())?;
let host = pi_durable_jsrt::install(&rt, pi_durable_jsrt::HostOptions::default()).await?;
let ns = rt
    .eval_module(
        "export { boundOutput } from '@earendil-works/chord/output';",
        "main.mjs",
    )
    .await?;
```

Values cross the boundary as `pi_json::strict::Value` (UTF-16 lossless, explicit `undefined`
preserved), not `serde_json::Value`, because the latter can't represent the lone UTF-16
surrogates the reference's conformance suite relies on. JS functions and objects cross as
`JsHandle`s. The TS reference's error classes (`ConversationBusy`, `ReadOnlySession`, `ChordError`, and so
on) are reconstructed in the glue from a typed native discriminator.

## Testing

```sh
cargo test -p pi-durable-jsrt
```

`examples/embedded_stock.rs` runs one pinned, unmodified TS reference test file inside pi-jsrt
against these modules:

```sh
cargo run -p pi-durable-jsrt --example embedded_stock -- ../pi-mono \
  --suite=packages/chord/test/output.test.ts
```

The full embedded gate across every mapped file is `scripts/check-durable-embedded.sh
../pi-mono` (slow; run on a quiet machine). `cargo test -p pi-coding-agent --test
extensions_durable` proves an extension can import both packages through the real extension
host.
