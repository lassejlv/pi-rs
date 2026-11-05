//! # pi-jsrt — an embedded JavaScript runtime
//!
//! `pi-jsrt` runs JavaScript and TypeScript for pi extensions without Node.js. It is built on:
//!
//! - [QuickJS](https://github.com/quickjs-ng/quickjs) (QuickJS-ng through
//!   [rquickjs](https://github.com/DelSkayn/rquickjs), with a small patch, see
//!   `vendor/rquickjs-sys/PATCH.md`) as the ECMAScript engine, reached through the
//!   [`js`] embedding API (the `pi-quickjs` crate),
//! - [oxc](https://oxc.rs) to strip TypeScript (types, `enum`, `namespace`,
//!   parameter properties) and to detect ES module syntax,
//! - tokio + reqwest for async host work (fs, child processes, `fetch`).
//!
//! ## Threading model
//!
//! QuickJS values are bound to their runtime's thread, so every runtime owns a dedicated OS thread that runs
//! the context, its promise job queue and a timer queue (`setTimeout`,
//! `setInterval`, `setImmediate`, `queueMicrotask`). [`JsRuntime`] is a cheap,
//! `Send + Sync + Clone` handle that talks to that thread with messages; the thread
//! sleeps until a message arrives or the next timer is due.
//!
//! ```no_run
//! # async fn demo() -> pi_jsrt::Result<()> {
//! use pi_jsrt::{JsRuntime, RuntimeOptions};
//! use serde_json::json;
//!
//! let rt = JsRuntime::new(RuntimeOptions::default())?;
//!
//! // Host functions: sync ones run on the JS thread, async ones on tokio and
//! // surface in JS as promises.
//! rt.register_async_function("host.double", |_rt, args| async move {
//!     Ok(json!(args[0].as_f64().unwrap_or(0.0) * 2.0))
//! })
//! .await?;
//!
//! // Evaluate a TypeScript module and call its export.
//! let ns = rt
//!     .eval_module("export async function run(x: number) { return await host.double(x); }", "main.ts")
//!     .await?;
//! assert_eq!(ns.call_method("run", vec![json!(21)]).await?, json!(42));
//! # Ok(()) }
//! ```
//!
//! ## Values and handles
//!
//! Values cross the boundary as `serde_json::Value` (see [`convert`] for the exact
//! mapping). JS functions inside converted values become handle markers
//! `{"$jsHandle": id}`; adopt them with [`JsRuntime::handle`] to get a [`JsHandle`]
//! (aka [`JsCallback`] / [`JsObjectHandle`]) that Rust can invoke later with
//! [`JsHandle::call`] (async) or [`JsHandle::call_blocking`] (from non-async threads,
//! e.g. a TUI render loop calling `component.render(width)`). Results that are
//! promises are awaited. A handle keeps its JS value alive until dropped.
//!
//! Rust closures can be passed into JS as values with [`JsRuntime::host_function`]
//! (marker `{"$hostFn": id}`).
//!
//! ## Errors
//!
//! JS exceptions and rejections become [`Error::Js`] with `name`, `message`, `stack`
//! (Node-style `Name: message\n    at ...`, mapped back to TypeScript positions) and
//! `code`. Host functions returning `Err` throw (or reject) a JS `Error` carrying the
//! same fields.
//!
//! ## Modules
//!
//! ES modules (with top-level await and dynamic `import()`), CommonJS (`require`,
//! `module.exports`, `__dirname`, `__filename`), JSON modules and TypeScript files are
//! supported with Node-style interop and `node_modules` resolution (`exports`,
//! `module`, `main`). Embedders add modules with [`VirtualModule`]s and
//! specifier aliases ([`RuntimeOptions::virtual_modules`], [`RuntimeOptions::aliases`]).
//! ES modules outside `node_modules` also get `__filename`, `__dirname` and
//! `require` like under jiti, which pi uses to load extensions.
//!
//! The builtin module `jsrt:host` exports `ref(value)` which returns a handle marker
//! for any JS value, so glue code can hand objects (e.g. TUI components) to Rust.
//!
//! ## Node compatibility
//!
//! With [`RuntimeOptions::node_compat`] (the default) the runtime provides `process`
//! (`process.version` is `v22.19.0`, pi's `engines` minimum), `Buffer`,
//! `fetch`/`Headers`/`Request`/`Response`, `crypto` (WebCrypto included), `WebAssembly`,
//! `MessageChannel`/`BroadcastChannel`, `CompressionStream`/`DecompressionStream`, `navigator`, and
//! the builtins `node:fs`, `node:fs/promises`, `node:path`, `node:os`, `node:child_process`,
//! `node:events`, `node:util`, `node:url`, `node:readline`, `node:assert`, `node:stream`,
//! `node:buffer`, `node:crypto`, `node:zlib`, `node:net`, `node:tls`, `node:http`, `node:https`,
//! `node:dns`, `node:module`, `node:timers`, `node:timers/promises`, `node:string_decoder`,
//! `node:tty`, `node:querystring`, `node:perf_hooks`, `node:async_hooks`, `node:worker_threads`
//! (no `Worker`) and `node:process` (unprefixed names work too). They are compiled on first use
//! (see `ops/mod.rs`), so they cost nothing at startup.
//!
//! Native parts are pure Rust: zlib/brotli/zstd on zlib-rs, brotli and ruzstd; ciphers, keys,
//! signatures and KDFs on RustCrypto (`ops/cipher.rs`, `ops/keys.rs`); TLS on rustls
//! (`ops/tls.rs`, sans I/O over any duplex); WebAssembly on the wasmi interpreter with linear
//! memory shared zero-copy with the JS `ArrayBuffer` (`ops/wasm.rs`).
//!
//! Without `node_compat`, only ECMAScript plus web-style globals (`console`, timers,
//! `TextEncoder`/`TextDecoder`, `URL`, `AbortController`, `structuredClone`, ...) exist,
//! which is the sandbox used by code mode.
//!
//! ## Limitations
//!
//! See `PORTING-STATUS.md`: no `Intl` beyond a small shim; synchronous infinite loops run until
//! [`RuntimeOptions::loop_iteration_limit`] (when set) stops them; no DNS record queries beyond
//! A/AAAA.

#![warn(missing_docs)]

pub mod convert;
pub mod ctx;
mod engine;
mod error;
mod loader;
pub mod node;
mod ops;
mod runtime;
pub mod transpile;

pub use error::{Error, JsException, Result};
/// The QuickJS embedding API the runtime is built on (see [`JsRuntime::with_context`]).
pub use pi_quickjs as js;
pub use runtime::{
    in_host_call, set_blocking_wait_hook, BlockingWaitHook, ExitHandler, JsCallback, JsHandle, JsObjectHandle, JsRuntime,
    OutputSink, RuntimeOptions, UncaughtHandler, VirtualModule,
};

