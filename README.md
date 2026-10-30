# pi-rs

A Rust port of [pi](https://github.com/earendil-works/pi), the minimal, extensible coding agent
harness. It builds a single native `pi` binary with the same CLI, terminal UI, on-disk formats
(sessions, settings, `auth.json`, `models.json`), RPC protocol and extension API as the
TypeScript version. No Node.js or Bun is needed at runtime: TypeScript extensions run on an
embedded JavaScript engine.

The user documentation of pi applies unchanged; it ships with the crate in
[`crates/pi-coding-agent/README.md`](crates/pi-coding-agent/README.md) and
[`crates/pi-coding-agent/docs/`](crates/pi-coding-agent/docs/).

## Build and install

Requires Rust 1.88 or newer (edition 2024 with let chains; developed with 1.97).

```bash
cargo build --release          # builds target/release/pi
./target/release/pi --version
./target/release/pi --help

cargo install --path crates/pi-coding-agent   # installs `pi` into ~/.cargo/bin
```

Only the `pi` binary is built from `crates/pi-coding-agent`. Everything else is a library crate.
HTTP goes through `reqwest` with rustls, so there is no OpenSSL dependency.

The release profile (root `Cargo.toml`) links with fat LTO and one codegen unit, strips symbols,
and optimizes for size except for the hot crates (TUI renderer, JavaScript engine and runtime,
regex, JSON, crypto, compression), which keep `opt-level = 3`. The link step takes several minutes
and up to about 4 GB of memory. For a quicker build, or one whose panic backtraces show function
names, override it, e.g. `cargo build --release --config 'profile.release.lto="thin"' --config
profile.release.strip=false`.

## Workspace

| Crate | Ports | Contents |
|---|---|---|
| `crates/pi-ai` | `packages/ai` | Providers (Anthropic, OpenAI completions/responses/Codex, Azure, Google Gemini/Vertex, Bedrock, Mistral, Cloudflare, OpenRouter, ...), model catalog, streaming, OAuth logins, AWS/Google credential chains |
| `crates/pi-agent` | `packages/agent` | The agent loop, `Agent`, proxy stream |
| `crates/pi-tui` | `packages/tui` | Differential terminal renderer, components (editor, markdown, select lists, images, ...), keybindings |
| `crates/pi-mcp` | `packages/mcp` | Model Context Protocol client: stdio and streamable HTTP transports, OAuth |
| `crates/pi-codemode` | `packages/codemode` | Sandboxed JavaScript execution for code mode |
| `crates/pi-telemetry` | `packages/telemetry` | Telemetry contracts |
| `crates/pi-json` | (new) | `JSON.parse` with V8's error messages and `JSON.stringify` with JavaScript formatting, shared by the crates above |
| `crates/pi-jsrt` | (new) | Embedded JavaScript runtime for extensions: Boa engine, oxc TypeScript stripping, ES module loader, Node API shims (`fs`, `path`, `child_process`, `events`, streams, `fetch`, ...) |
| `crates/pi-coding-agent` | `packages/coding-agent` | The `pi` binary and library: sessions, tools, settings, resources, packages, interactive/print/JSON/RPC modes, built-in extensions |
| `crates/pi-chord` | `packages/chord` (subset) | JSON revisions, invocation context, cancellation, watches and SQLite contracts used by durable storage |
| `crates/pi-durable` | `packages/durable` | Durable sessions, transactions, documents, tasks, scheduler, Harness and storage (memory, JSONL, SQLite, IndexedDB, Cloudflare) |
| `crates/pi-durable-jsrt` | (new) | Serves `@earendil-works/pi-durable` and `@earendil-works/chord` to pi extensions as embedded pi-jsrt modules |

Modules mirror the TypeScript file layout (`core/agent-session.ts` is
`pi_coding_agent::core::agent_session`). [`PORTING.md`](PORTING.md) describes the conventions and
[`crates/pi-coding-agent/ARCHITECTURE.md`](crates/pi-coding-agent/ARCHITECTURE.md) the design of
the main crate.

## Durable

`pi-chord` and `pi-durable` are a native Rust port of the TypeScript `@earendil-works/chord` and
`@earendil-works/pi-durable` packages: sessions, transactions, documents, the task scheduler and a
public `Harness`/`Conversation` API, backed by memory, JSONL, SQLite, IndexedDB and Cloudflare
Durable Object storage. `pi-durable-jsrt` serves both packages to pi extensions as embedded
pi-jsrt modules, so existing extensions that import `@earendil-works/pi-durable` or
`@earendil-works/chord` run unchanged.

Try it with `/durable` from [`durable-demo.ts`](crates/pi-coding-agent/examples/extensions/durable-demo.ts),
or run an offline conversation directly:

```sh
cargo run -p pi-durable --example conversation -- "Hello"
```

Current scope, test counts and open items are in
[`porting/durable/STATUS.md`](porting/durable/STATUS.md).

## Parity with TypeScript pi

The port aims at behavioral parity, including error messages, defaults, environment variables and
byte-compatible JSON output:

- **CLI and modes**: interactive TUI, print mode (`-p`), JSON event mode, RPC mode
  (`--mode rpc`), session management, `pi install/remove/update/list/config`, login/logout flows.
- **Formats**: session JSONL files, `settings.json`, `auth.json` and `models.json` are read and
  written exactly like the TypeScript version (key order, number formatting, migrations), so both
  implementations can share an agent directory (`~/.pi/agent`).
- **Rendering**: UI components are checked line by line against renders of the TypeScript
  reference (golden tests, see below).
- **Providers**: requests carry the same bodies and headers as the official SDKs the TypeScript
  version uses (OpenAI, Anthropic, Google GenAI, AWS SDK user agents and retry behavior, ...).

The status of every area is tracked in [`PORTING-STATUS.md`](PORTING-STATUS.md); the differences
that remain are listed below.

## Known differences

Behavior that still differs from TypeScript pi, and why:

- **JavaScript engine.** Extensions run on Boa (`pi-jsrt`) and code mode scripts on Boa instead of
  V8/QuickJS: stack trace text, engine error messages and performance differ, a few language
  features (decorators) are missing, and code mode's memory limit is not enforced (Boa has no heap
  limit). Node API coverage of `pi-jsrt` is tracked in `PORTING-STATUS.md`. A reload of the
  extensions starts a new runtime, so `globalThis` does not carry values across `/reload` as
  Node's single global object does.
- **Strings.** Rust strings cannot hold unpaired UTF-16 surrogates; where JavaScript would keep one
  (`JSON.parse("\"\\uD800\"")`, `sanitizeSurrogates`), the port has U+FFFD.
- **`JSON.parse` limits.** Numbers beyond the `f64` range (`1e400`, `Infinity` in JavaScript) and
  nesting deeper than 128 levels are rejected (serde_json); a JSON value cannot hold `Infinity`.
- **Distribution.** No Bun-specific plumbing or `rpc-entry.ts` (use `pi --mode rpc`), no
  npm/pnpm/yarn self-update (the binary updates by download). Without an `npm` executable, npm
  packages are installed by a built-in client that cannot run lifecycle scripts or resolve
  git/file/workspace dependency specs.
- **Native code.** Images are processed with the `image` crate instead of Photon (WASM), so encoded
  bytes and the resize candidate picked can differ. Word segmentation uses ICU4X dictionaries,
  which split about 5% of Chinese/Japanese test sentences differently from V8's ICU4C. Codex
  request bodies are zstd-compressed at the fastest level (pure-Rust `ruzstd`), so bytes differ
  while the content is the same. `localeCompare` is ICU's root collation, which is what Node uses
  for English locales; a full-ICU Node under a tailored locale (`de`, `sv`, ...) would sort by that
  locale.
- **Windows.** The Windows terminal backend (VT input, console modes) is not ported, and the AWS
  user agent omits `os.release()` there; neither could be built and tested in the Linux environment
  the port was developed in.
- **YAML frontmatter.** The `yaml` package is ported in full for parsing, but keys whose value is a
  collection, date or buffer are stringified approximately (the package runs its YAML
  stringifier), and nesting deeper than 800 levels reports `Maximum call stack size exceeded` at a
  different depth than V8's stack limit.
- **Emoji width.** RGI emoji ZWJ sequences are recognized structurally; a ZWJ sequence of RGI
  emoji that is not itself in the RGI list counts as one wide emoji.
- **Error stacks.** After a settings warning, package commands print the error's
  `Name: message` line but not the V8 stack frames that follow in TypeScript (they name the
  installation's JavaScript files and line numbers).
- **Timing-only output.** When stdin ends at the `--resume` picker, TypeScript may draw an
  intermediate "Loading ..." frame if its asynchronous session load is slower than the first
  render; the final screen and the terminal restore are identical.
- **Deliberately not reproduced.** The HTML export inserts its templates literally: TypeScript's
  `String.replace` expands `$&`/`$$` in the inserted code, which breaks a highlight.js regex and
  drops the `$` of the cost display. `theme()` initializes the system theme on first use instead
  of throwing "Theme not initialized". An invalid `file://` URL in a path argument is kept
  literally instead of failing in `fileURLToPath`.
- **Extension API details.** `ctx.getSystemPromptOptions()` returns a copy, so mutating it outside
  `before_agent_start` does not change later turns as mutating TypeScript's live object would.
- **Unverified.** The Bedrock error text when a stream is aborted mid-response (HTTP/2 close) has
  not been compared with the AWS SDK.

## Extensions

pi extensions are TypeScript or JavaScript modules, and the port loads them unchanged:

- `.ts`/`.js`/`.mjs` extensions run inside `pi-jsrt`, an embedded [Boa](https://boajs.dev) engine
  on a dedicated thread. TypeScript is stripped with [oxc](https://oxc.rs) when loaded.
- Imports of `@earendil-works/pi-coding-agent`, `@earendil-works/pi-tui`,
  `@earendil-works/pi-ai`, `@earendil-works/pi-agent-core`, `typebox` (the real library, vendored)
  and `node:*` built-ins resolve to modules provided by the runtime, backed by the Rust
  implementations. npm dependencies of extension packages are loaded from their `node_modules`.
- The full extension API is available: events, tools, commands, shortcuts, flags, providers, tool
  renderers, custom UI components, and session control.
- The built-in extensions (MCP, code mode, tool search, llama.cpp) are implemented in Rust against
  the same API.

[`crates/pi-coding-agent/js/EXPORTS.md`](crates/pi-coding-agent/js/EXPORTS.md) lists every export
of the extension-facing packages and how it is provided.

## Tests

```bash
cargo test --workspace                    # all tests
cargo test -p pi-coding-agent --test rpc_mode   # one test target
cargo clippy --workspace --all-targets
```

Most tests are ports of the TypeScript test suites (see
[`crates/pi-coding-agent/TEST-MAP.md`](crates/pi-coding-agent/TEST-MAP.md)) and run offline against
mock servers and the faux provider. A few suites use the TypeScript reference checkout when it is
available and skip otherwise:

- the example-extension tests (`extensions_examples`, `extensions_interactive`) load the examples
  from `PI_TS_SOURCE` (a checkout of the TypeScript repository);
- live provider tests in `crates/pi-ai/tests/ts_ported/live` need real API keys and are ignored by
  default.

### Golden fixtures

Rendering and configuration tests compare against fixtures generated from the TypeScript
reference (`crates/pi-coding-agent/tests/fixtures/golden/`). To regenerate them, install the
reference's dependencies (`npm ci --ignore-scripts` in the checkout; Node 22.6 or newer) and run:

```bash
PI_TS_REF=/path/to/pi scripts/golden/run.sh                 # all generators
PI_TS_REF=/path/to/pi scripts/golden/run.sh scripts/golden/gen-tui.mjs   # one generator
```

The generators render with a fixed home directory, clock, terminal and theme setup that the Rust
side reproduces. The vendored JavaScript libraries for extensions are regenerated with
`scripts/vendor-typebox.mjs` and `scripts/vendor-marked.mjs`.

## License

MIT, like pi.
