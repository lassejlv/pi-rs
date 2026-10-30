# Embedded substrate API (chunk A)

The contract chunks B–E build on. Crate: `crates/pi-durable-jsrt`. Plan: `EMBEDDED-PLAN.md`.
Build with your own target dir: `CARGO_TARGET_DIR=<worktree>/target-<you> cargo …`.

## 1. Layout

| Path | What |
| --- | --- |
| `src/lib.rs` | re-exports: `install`, `HostOptions`, `DurableHost`, `Dispatch`, `Methods`, `Args`, `Call`, `HostValue`, `NativeRef`, `HostError`, `ErrorClass`, `HostResult`, `call_sync`, `call_async`, `from_js`, `to_js`, `strict_from_js`, `strict_to_js`, `modules`, `virtual_modules`, `specifier_for`, `hoist_vitest_mocks`, `BRIDGE`, `GLOBAL` |
| `src/host.rs` | `install`, the `__piDurable` global, method registry, module table, JS call helpers |
| `src/value.rs` | `HostValue` ⇄ Boa `JsValue` and ⇄ `pi_json::strict::Value` |
| `src/errors.rs` | `ErrorClass` table, `HostError` |
| `src/hoist.rs` | vitest `vi.mock` / `vi.hoisted` hoisting transform (runner only) |
| `src/dispatch/mod.rs` | `defaults()`: the method groups installed by default — **add one line per group here** |
| `src/dispatch/chord.rs` | Chord methods; seeded by A with `output.*` (owned by B from now on) |
| `src/dispatch/vitest.rs` | `vitest.mock` (installed only with `HostOptions::vitest`) |
| `build.rs` | embeds every `js/**/*.js` (no table to edit) |
| `js/bridge.js` | `pi-durable:bridge`: `call`, `callAsync`, `ref`, `method`, all TS reference error classes |
| `js/vitest.js` | the `vitest` shim |
| `js/chord/output.js` | `@earendil-works/chord/output` (seeded by A, owned by B) |
| `examples/embedded_stock.rs` | the runner |
| `tests/substrate.rs` | substrate tests |

Keep the crate compiling at every step: a new glue module only needs its file under `js/`; a new
dispatch module needs its file plus its line in `dispatch/mod.rs` (`pub mod x;` and an entry in
`defaults()`), added together.

## 2. Modules

Every file under `js/` is served as a `pi_jsrt::VirtualModule::EsmJs` (plain JS, no TS stripping)
under the specifier `host::specifier_for(path)` gives:

| File under `js/` | Specifier |
| --- | --- |
| `bridge.js` | `pi-durable:bridge` |
| `vitest.js` | `vitest` (only with `HostOptions { vitest: true }`) |
| `index.js` | `@earendil-works/pi-durable` |
| `<p>.js`, `<p>/index.js` | `@earendil-works/pi-durable/<p>` (e.g. `env/index.js` → `/env`, `storage/sqlite/node.js` → `/storage/sqlite/node`) |
| `package.json.js` | `@earendil-works/pi-durable/package.json` |
| `chord/index.js` | `@earendil-works/chord` |
| `chord/<p>.js`, `chord/<p>/index.js` | `@earendil-works/chord/<p>` |
| `chord/package.json.js` | `@earendil-works/chord/package.json` |
| `internal/<p>.js` | `pi-durable:internal/<p>` (glue-private) |
| `internal/src/<pkg>/<p>.js` | `pi-durable:internal/src/<pkg>/<p>`: the runner serves it for TS reference `packages/<pkg>/src/<p>.ts` when that file is **not** an export entry (e.g. `internal/src/durable/session/session.js` for `test/setup-document-retention.ts`'s `../src/session/session.ts`) |

Do not put modules at non-TS reference public subpaths (e.g. `js/harness.js` would become
`@earendil-works/pi-durable/harness`, which TS reference does not export); use `internal/` for shared
glue such as D's harness module.

Glue imports the host through the bridge and re-exports the error classes from it, so every
module sees one class identity:

```js
import { call, callAsync, ref, ConversationBusy } from "pi-durable:bridge";
export { ConversationBusy };
```

`modules(include_vitest)` / `virtual_modules(include_vitest)` give the table for embedders that set
`RuntimeOptions::virtual_modules` themselves (chunk F, pi-coding-agent).

## 3. Installing

```rust
let rt = pi_jsrt::JsRuntime::new(pi_jsrt::RuntimeOptions { timer_control: true, expose_gc: true, ..Default::default() })?;
let host = pi_durable_jsrt::install(&rt, pi_durable_jsrt::HostOptions::default()).await?;
// HostOptions { vitest: bool (serve the shim + vitest.mock), dispatch: Vec<Arc<dyn Dispatch>> (default: dispatch::defaults()) }
host.counters();     // BTreeMap<method, calls>: proof execution reached Rust
host.total_calls();
host.methods();      // registered names
```

`install` registers the virtual modules (do it before the first import of any of them) and
defines the non-enumerable global `__piDurable` (`call`, `callAsync`, `method`, `ref`, plus
`makeError`, set by the bridge).

## 4. Dispatch (Rust methods)

```rust
use pi_durable_jsrt::{Args, Call, Dispatch, HostError, HostResult, HostValue, Methods, ErrorClass};

pub struct SessionDispatch;
impl Dispatch for SessionDispatch {
    fn register(&self, m: &mut Methods) {
        // Sync: runs on the JS thread inside pi_jsrt::ctx::host_call (re-entrant JS calls allowed).
        m.sync("session.version", |_call: &Call, args: Args| Ok(HostValue::from(1.0)));
        // Async: runs on tokio; JS gets a promise.
        m.asynchronous("session.open", |call: Call, args: Args| async move {
            let options = args.get(0).to_strict_omitting_undefined()?;
            Ok(HostValue::Undefined)
        });
    }
}
```

- JS side: `call("ns.name", ...args)` for sync methods, `callAsync("ns.name", ...args)` for async
  ones (a sync method called with `callAsync` returns a resolved promise; an async method called
  with `call` throws a `TypeError`). Unknown names throw `TypeError: unknown pi-durable host method`.
- Method names are unique across groups (duplicates panic at install). Use the plan's prefixes
  (`session.*`, `tx.*`, `doc.*`, `registry.*`, `fs.*`, …).
- Panics in a method become `Error: pi-durable host method <name> panicked`.
- **Never capture a `JsRuntime` (or a `JsHandle`) in a method closure**: the global function would
  keep its own runtime alive. Use `call.runtime()` per call; store handles in per-call state or in
  `NativeRef` state owned by a JS object.
- `Call`: `runtime() -> &JsRuntime`, `method() -> &str`.
- `Args` (a `Vec<HostValue>`): `len`, `is_empty`, `get(i)` (`Undefined` when missing),
  `take(i)`, `string(i) -> &JsonString`, `text(i) -> String` (lossy), `f64(i)`, `bool(i)`,
  `handle(i) -> JsHandle`, `native::<T>(i) -> Arc<T>`; wrong types fail with `TypeError: argument i must be …`.

Environment, tools, images and testing (`src/dispatch/{env,tools,images,testing}.rs`) dispatch
directly rather than through the in-process stock Hub (`dispatch/transport.rs`): they call the
same pi-durable functions as `examples/env_stock_bridge.rs`/`support/builtin_bridge.rs` (shared
error-code strings live in pi-durable: `FileErrorCode`/`ExecutionErrorCode::{as_str, from_name}`),
and the Hub's strict-JSON frames with base64 bytes would cost a serialization and a pump round
trip per file operation where Boa is already the bottleneck. A JS `ExecutionEnv` reaches native
code as `dispatch::env::js_env(&describeEnv(env))` (`pi-durable:internal/env/port`).

## 5. Values (`HostValue`)

No `serde_json` anywhere on this path; strings are UTF-16 code units (`pi_json::strict::JsonString`).

| JS | `HostValue` | back to JS |
| --- | --- | --- |
| `undefined` | `Undefined` (an object member set to `undefined` stays a member) | `undefined` |
| `null` | `Null` | `null` |
| boolean | `Bool` | boolean |
| number | `Number(f64)` bit-exact (`-0`, `NaN`, `±Infinity`) | number |
| string | `String(JsonString)` incl. lone surrogates | same code units |
| array | `Array(Vec)` (holes read as `undefined`) | new array |
| plain object (proto `Object.prototype` or `null`) | `Object(Vec<(JsonString, HostValue)>)`: own enumerable string keys in ES order, read with `[[Get]]` | new object (`Object.prototype`, data properties; `"__proto__"` is an ordinary key) |
| `Uint8Array` (incl. `Buffer`) | `Bytes(Vec<u8>)` | new `Uint8Array` |
| function, symbol, class instance, `Map`, `Promise`, any non-plain object, or `ref(x)` | `Handle(JsHandle)` | **the same value** (identity) |
| object made for a `NativeRef` | `Native(NativeRef)` | the same Rust state in an opaque null-proto object |
| `bigint`, cyclic structure | `TypeError` | |

Helpers: `HostValue::string(&str)`, `HostValue::object([(k, v), …])`, `HostValue::native(state)`,
`From<f64|usize|bool|&str|JsonString|Vec<HostValue>|Option<T>|strict::Value>`, accessors
`is_undefined`, `as_f64`, `as_bool`, `as_string`, `as_handle`, `as_bytes`, `as_native::<T>()`,
`get(key)` (object member).

Strict JSON (the durable payload type):
- `HostValue::from_strict(&strict::Value)` / `HostValue::from(strict::Value)`;
- `value.to_strict()` — fails (`TypeError`) on `undefined` anywhere, handles, native state, bytes,
  non-finite numbers;
- `value.to_strict_omitting_undefined()` — `JSON.stringify` semantics (members dropped, array
  items become `null`; a top-level `undefined` still fails).
- Direct: `strict_from_js(ctx, &JsValue)`, `strict_to_js(ctx, &strict::Value)`; and `from_js(ctx, &JsValue)`, `to_js(ctx, &HostValue)` for code running with a Boa `Context`.

Native state: `HostValue::native(Mutex::new(state))` returns an opaque JS object that keeps the
state alive until it is garbage collected; take it back with `args.native::<Mutex<State>>(i)`.
Glue keeps it in a private field (see `js/chord/output.js` `OutputBuffer`).

Plain JS objects are **copied**. Pass `ref(obj)` from glue when Rust must call methods on the
object or keep its identity (JS `Storage`/`SqliteDatabase` capabilities, `ExpectLike`, etc.).

## 6. Errors

```rust
HostError::new(ErrorClass::FileError, "message")        // UTF-8 message
HostError::typed(ErrorClass::Error, json_string)          // UTF-16 message
    .with_prop("code", "not_found")                       // constructor input / extra property
    .with_cause(other_host_error)                         // error.cause
HostError::type_error("…")
e.class() -> Option<ErrorClass>; e.name(); e.message() -> &JsonString
HostError::from_js(ctx, &thrown) / HostError::from_boa(ctx, js_error) / e.to_js(ctx) / e.to_boa(ctx)
```

- `HostError::Typed` is rebuilt in JS by `__piDurable.makeError` (bridge.js) as an instance of the
  TS reference class; `HostError::Thrown` (a JS callback's exception) rethrows **the same object**.
- Props the constructor does not consume are assigned onto the instance.

| `ErrorClass` | JS result (exported from) | props used |
| --- | --- | --- |
| `Error`, `TypeError`, `RangeError` | builtins | message, cause |
| `AbortError` | `new DOMException(message, "AbortError")` | |
| `ConversationBusy` | `@earendil-works/pi-durable` | `conversationId` (message built by the class) |
| `ReadAfterWrite` | `@earendil-works/pi-durable` | `method` |
| `ReadOnlySession` | `@earendil-works/pi-durable` | `what` |
| `SessionFailed` | `@earendil-works/pi-durable` | cause |
| `StorageRequestError` | `@earendil-works/pi-durable` | message |
| `FileError` | `@earendil-works/pi-durable/env` | `code`, `path`, message, cause |
| `ExecutionError` | `@earendil-works/pi-durable/env` | `code`, `spillPath`, message, cause |
| `JsonlCorruptionError` | `@earendil-works/pi-durable/storage/jsonl` | message, cause |
| `JsonlStoragePoisonedError` | `@earendil-works/pi-durable/storage/jsonl` | cause |
| `ChordError` | `@earendil-works/chord` | `code`, `reason`, `details`, cause; message **without** the `code/reason: ` prefix |
| `PathError` | `@earendil-works/chord/delta` | `path` |
| `UnsafePathError` | `@earendil-works/chord/delta` | `segment` |
| `WatchError` | `Error` with `name = "WatchError"` (no TS reference class) | message, cause |

All classes live in `js/bridge.js` (bodies copied from TS reference) and are exported there; the public
modules re-export them. `bridge.errorClassNames` must equal `ErrorClass::ALL` (tested). The
mapping from `pi_durable::Error` / `pi_chord` errors to `HostError` belongs in each chunk's
dispatch module (A does not depend on `pi-durable`).

## 7. Calling JS from Rust

```rust
// Sync: inside a sync method this runs re-entrantly on the JS thread (a returned promise must
// settle from pending jobs, else Error "…still pending"); from another thread it blocks until
// the result settles (promises awaited).
let r: HostResult<HostValue> = pi_durable_jsrt::call_sync(&handle, None, vec![arg]);          // handle(arg)
let r = pi_durable_jsrt::call_sync(&obj_handle, Some("render"), vec![HostValue::from(80.0)]); // obj.render(80)
// Async: queue on the JS thread and await the settled result (any thread / tokio).
let r = pi_durable_jsrt::call_async(&handle, None, vec![arg]).await;
```

- Results convert with the table above; a JS exception becomes `HostError::Thrown` and, when the
  method returns it, the original object is rethrown.
- This settles plan risk 2: sync JS → Rust → JS → Rust nesting works (tested 2 levels deep, plus a
  blocking call from a tokio thread while JS awaits).
- `pi_jsrt::JsHandle::call_blocking` also works inside sync methods but converts through
  `serde_json` (lossy for lone surrogates); prefer `call_sync`.

pi-jsrt additions available to glue/Rust (all additive):
- `pi_jsrt::ctx::{host_call, with_current, spawn_promise, when_settled, Settle}`;
- `JsRuntime::set_file_source(path, source)`: serve a source text instead of a file's contents;
- `RuntimeOptions::aliases`: a key that is an absolute file path redirects every import resolving to that file;
- `RuntimeOptions::expose_gc` (global `gc()`), `RuntimeOptions::timer_control` (global `__jsrtTimers`, below);
- `node:sqlite` (`DatabaseSync`, `StatementSync`; Node 22 value mapping; `node:`-prefixed only).

## 8. vitest shim (`js/vitest.js`)

Exports: `describe`/`suite`, `it`/`test` (each with `.skip`, `.only`, `.todo`, `.fails`,
`.concurrent` (sequential), `.sequential`, `.each` (arrays, objects with `$name`, tagged tables;
`%s %d %i %f %j %o %O %# %%`), `.for`, `.skipIf`, `.runIf`), `beforeAll`, `afterAll`,
`beforeEach`, `afterEach` (hooks may return a cleanup), `onTestFinished`, `onTestFailed`,
`expect`, `expectTypeOf`/`assertType` (no-ops), `vi`/`vitest`, `bench` (no-op), `AssertionError`.

- Runs like `vitest --run` on one file: sequential, `sequence.hooks: "stack"` (after-hooks
  reversed), `testTimeout` 5000 ms, `hookTimeout` 10000 ms unless a test/hook/describe passes its
  own; describe-level skip/todo/only; test context `{ task, expect, skip(), onTestFinished, onTestFailed, signal }`.
- `expect`: `toBe`, `toEqual`, `toStrictEqual`, `toMatchObject`, `toThrow`/`toThrowError`
  (class, string, RegExp, Error, asymmetric, object subset), `toHaveLength`, `toContain`,
  `toContainEqual`, `toBeUndefined`, `toBeDefined`, `toBeNull`, `toBeNaN`, `toBeTruthy`,
  `toBeFalsy`, `toBeTypeOf`, `toBeInstanceOf`, `toBeGreaterThan(OrEqual)`, `toBeLessThan(OrEqual)`,
  `toBeCloseTo`, `toBeOneOf`, `toSatisfy`, `toMatch`, `toHaveProperty`, `toHaveBeenCalled`,
  `toHaveBeenCalledOnce`, `toHaveBeenCalledTimes`, `toHaveBeenCalledWith`,
  `toHaveBeenLastCalledWith`, `toHaveBeenNthCalledWith`, `toHaveReturned(Times|With)`; `.not`,
  `.resolves`, `.rejects`; `expect.any`, `anything`, `objectContaining`, `arrayContaining`,
  `stringContaining`, `stringMatching`, `closeTo`, `expect.not.*`, `unreachable`, `assertions`,
  `hasAssertions`, `extend`, `getState`. Equality follows jest/vitest: `Object.is` for primitives
  (`-0 ≠ 0`, `NaN = NaN`), `toEqual` ignores `undefined` members and sparseness, `toStrictEqual`
  checks them and prototypes, Map/Set/Date/RegExp/typed arrays/Error (name, message, cause, own
  props)/iterables, cycles, asymmetric matchers anywhere.
- `vi`: `fn`, `spyOn` (methods and `get`/`set` accessors, prototypes; calls through by default),
  `isMockFunction`, `mocked`, `hoisted`, `mock` (see below), `unmock`, `importActual`,
  `clearAllMocks`, `resetAllMocks`, `restoreAllMocks` (spies only), `stubEnv`, `unstubAllEnvs`,
  `stubGlobal`, `unstubAllGlobals`, `waitFor`, `waitUntil` (advance fake time by `interval` when
  faking), and the timers below. Mock functions: `mock.calls/results/settledResults/instances/
  contexts/invocationCallOrder/lastCall`, `mockImplementation(Once)`, `mockReturnValue(Once)`,
  `mockResolvedValue(Once)`, `mockRejectedValue(Once)`, `mockReturnThis`, `withImplementation`,
  `mockName`, `getMockName`, `getMockImplementation`, `mockClear`, `mockReset`, `mockRestore`.
- **Fake timers drive pi-jsrt's real timer queue** (`__jsrtTimers`, requires
  `RuntimeOptions::timer_control`): `useFakeTimers({ now })`, `useRealTimers`,
  `setSystemTime` (also without fake timers: frozen `Date`), `getMockedSystemTime`,
  `getRealSystemTime`, `isFakeTimers`, `getTimerCount`, `clearAllTimers`, `advanceTimersByTime`,
  `advanceTimersByTimeAsync` (real event-loop yield before each timer, like `tickAsync`),
  `advanceTimersToNextTimer(Async)`, `runAllTimers(Async)` (10000-timer loop limit),
  `runOnlyPendingTimers(Async)`. Like vitest 4 defaults it fakes every timer API (`setTimeout`,
  `setInterval`, `setImmediate`, their clears) plus `Date`, `performance.now`, `process.hrtime`
  (not `nextTick`/`queueMicrotask`). Timers created before `useFakeTimers` keep real time; pending
  fake timers are dropped by `useRealTimers`. The runner's own timeouts/yields use real timers.
- `__jsrtTimers` (for glue that needs it): `freeze(nowMs)`, `thaw()`, `frozen()`, `now()`,
  `setNow(ms)`, `next()`, `count()`, `runNext(limitMs?)`, `advance(ms)`, `realTimeout(fn, ms)`,
  `unref(id)`. `gc()` (with `expose_gc`) clears kept WeakRef targets and forces a full collection.
- **`vi.mock(specifier, factory)`** works like vitest's: the runner hoists every top-level
  `vi.mock`/`vi.hoisted`/`vi.unmock` above the imports (`hoist_vitest_mocks`, served with
  `set_file_source`; the file on disk is untouched), turns the other imports into
  `const {…} = await import(…)`, and calls `__applyMocks(import.meta.url)` before them.
  `__applyMocks` runs each factory (with `importOriginal` resolving to the unmocked module) and
  installs the result: builtins under both `node:x` and `x` (`node:fs/promises` is not affected),
  bare packages under the exact specifier, relative/absolute paths by file path (every importer of
  that file). A `default` member becomes the default export. Mocks registered but never applied
  (non-top-level `vi.mock`) are reported as `unappliedMocks` and fail the run.
  Verified on the pinned `packages/durable/test/env-node-spill.test.ts`: its `node:fs` mock is
  applied before its other imports load (`tests/substrate.rs::TS reference_env_node_spill_mock_is_applied`).
- Runner entry points (not vitest API): `__runner.run({ file })` → report object;
  `__runner.mocks()` → `{ appliedMocks, unappliedMocks }`; `__applyMocks(baseUrl)`.

Limits (fail loudly): automocking (`vi.mock` without a factory), `vi.doMock`, `test.extend`
fixtures, `expect.soft`, snapshots, real concurrency, benchmarks. Converted imports are `const`
snapshots (vitest keeps them live).

## 9. Runner

```text
CARGO_TARGET_DIR=… cargo run -p pi-durable-jsrt --example embedded_stock -- <pi-mono> \
    --suite=packages/<durable|chord>/test/<file>.test.ts [--retention=<ms>] [--report=<path>]
```

- Asserts the reference `HEAD` is `5e63241d0294ed3fcfc4388c737a8ea27e09c74e` and that the test
  file and the package's setup files (`packages/durable/test/setup-document-retention.ts`) are
  byte-identical to that commit, before and after the run.
- Fresh `JsRuntime` per mode: `cwd` = the package dir, `timer_control` + `expose_gc`, process env
  with `PI_DURABLE_DOCUMENT_RETENTION_MS` removed (default mode) or set. Durable files run both
  modes (unset, `0`) like `npm test`; chord files run once; `--retention=<ms>` runs one mode.
- Routing (no TS reference implementation ever runs):
  - each `exports` subpath of `packages/{durable,chord}/package.json` maps to its embedded module;
    a missing one is a virtual module that throws `not embedded: <spec> …`;
  - every `packages/<pkg>/src/**/*.ts` file is aliased by absolute path to: its export's embedded
    module, else `pi-durable:internal/src/<pkg>/<path>` if the glue has it, else a throwing stub.
    Note a static named import from a stub fails at link time as
    `SyntaxError: could not find export \`x\`` (e.g. the durable setup file until C provides
    `internal/src/durable/session/session.js`).
  - `@earendil-works/pi-ai[/…]` resolves to its pinned sources like the stock gate's resolver
    plugin (`scripts/run-stock-durable-session.mjs`): `<name>` → `src/index.ts`, `<name>/<sub>` →
    `src/<sub>.ts`, else `src/<sub>/index.ts`, for any subpath (e.g.
    `@earendil-works/pi-ai/utils/identity`). Its built `dist/` is never used (it is not part of the
    pinned tree and may be stale). `git status` of `packages/ai/src` must be empty before and after
    the run (byte-identical to the pin); the report records the rule under `sourceAliases`.
    pi-codemode, pi-telemetry (`js/pi-telemetry/`, over the native pi-telemetry crate) and chord use
    their embedded modules. Other packages resolve through `<pi-mono>/node_modules` as usual.
- Imports `vitest`, the setup files, then the test file (with mock hoisting applied), then calls
  `__runner.run`. Uncaught exceptions / unhandled rejections are collected.
- Node mode / workers: this binary is the tests' `process.execPath`. Invoked as
  `<exe> [node flags such as --expose-gc] <file.ts|js> args…` (e.g. retention.test.ts's
  `spawnSync(process.execPath, ["--expose-gc", "retention.worker.ts", scenario])`), it runs the file
  in pi-jsrt like `node` with the same routing (from `PI_DURABLE_EMBEDDED_REFERENCE`, set by the
  parent) and the host installed, `gc` exposed; exit status 0 on success, non-zero on a throw or
  failed assertion. Each child writes its native counters to `PI_DURABLE_EMBEDDED_COUNTERS_DIR`
  (a per-mode temp dir); the parent sums them into the mode's `native.calls`
  (`native.children`, `native.childMethods` in the report).
- A mode passes only if: it loaded, `success`, no uncaught errors, ≥1 test, passed + skipped/todo
  = total, ≥1 native host call (except files listed in `NO_NATIVE` with a reason, e.g. the
  type-level `harness-telemetry-options.test.ts`), no unapplied mocks. Exit 0 iff every mode passes.
- Timing-heavy suites (`harness-ownership`, `harness-scheduler-retention`, the 150-step
  `harness-context` case) need an optimized build, like their stock gates (`--release`):
  `CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release -p pi-durable-jsrt --example embedded_stock`.
- Prints failures, then one line per mode:
  `Embedded <suite> retention=<default|ms>: P/T passed, F failed, S skipped, U uncaught; native calls=N (M methods)[ FAILED]`.
- Report (default `/tmp/pi-durable-embedded-<pkg>-<file>-report.json`, e.g.
  `…-chord-output-report.json`, `…-chord-delta-tracker-tracker-report.json` for `chord/test/delta-tracker/tracker.test.ts`):

```json
{
  "reference": "5e63241…", "test": "packages/chord/test/output.test.ts", "sha256": "…",
  "selection": "complete file without name filter",
  "runtime": "pi-jsrt (Boa) with pi-durable-jsrt embedded modules and the vitest shim",
  "success": true,
  "modes": [{
    "retention": null, "passed": 21, "failed": 0, "skipped": 0, "total": 21,
    "uncaught": [], "loadError": null, "appliedMocks": [], "unappliedMocks": [],
    "native": { "calls": 55182, "methods": { "output.bound": 46709, "…": 0 } },
    "tests": [{ "name": "tool output bounds …", "status": "passed", "durationMs": 1, "failureMessages": [] }]
  }]
}
```

## 10. Verified state (chunk A)

- `cargo test -p pi-durable-jsrt --test substrate`: 9 passed (values, strict, handles, every
  error class, re-entrancy, module table, vitest shim self-test, `vi.mock` hoisting, TS reference
  env-node-spill mock).
- Runner on `packages/chord/test/output.test.ts`: 21/21 passed, 0 uncaught, 55182 native calls.
- `cargo test -p pi-jsrt --test node_sqlite` (3), `--test test_controls` (4); full `cargo test -p pi-jsrt` green.
- `cargo clippy -p pi-durable-jsrt -p pi-jsrt --all-targets -- -D warnings` clean; `cargo fmt -p pi-durable-jsrt --check` clean.

## 11. The shared durable service (`transport.*`, chunk D1)

Harness, Session, storages, tools and codemode all run on **one** in-process service: the stock
gate's `Hub` (`crates/pi-durable/examples/task_stock_bridge.rs` + `examples/support/*`), included by
`src/dispatch/transport.rs` via `#[path]`, so the process gate and the embedded package exercise the
same Rust. Do not re-host its methods in another dispatch group.

- Rust: `task_stock_bridge.rs` exposes `new_hub(output)`, `spawn_lane`, `receive`, `close_input`;
  its `main` keeps the stdio loop. Hub options added for the embedded binding are opt-in request
  fields (`aliased: true` on `runtimeContext`/`conversationContext` returns views with message
  aliasing), so the stock adapters are unaffected.
- JS: `pi-durable:internal/transport` exports `nativeTransport` and `failure` with the API of
  `scripts/stock/durable-native-transport.mjs` (`request`, `requestAdmitted`, `sync`, `register`,
  `error`, `afterCallback`, `event`, `notify`, `crashActor`, `actorRuntime`, `contextAdapter`, …).
  Frames are strict-JSON text both ways (lossless). The service's frames reach JS through a queue:
  `JsRuntime::post_task` pumps it when the JS thread is idle (never via tokio, whose workers may all
  wait on synchronous callbacks), and `transport.wait` drains it while a `sync` request blocks.
  `PI_DURABLE_NATIVE_TRACE=1` (`…_DETAIL=1`) traces every frame to stderr with a timestamp.
- Shared glue: `pi-durable:internal/handles` (`handle`, `valueHandle`, `objectFor`,
  `temporaryHandle`, `describeTask`, `describeDoc`, `mark*`), `pi-durable:internal/settings`,
  `pi-durable:internal/telemetry` (`describeTelemetry`; installs the context adapter),
  `pi-durable:internal/env-signals` (`environmentContext`), `pi-durable:internal/native-storage`
  (`registerNativeStorage(kind, lookup)`, `nativeStorageHandle(kind, storage)`: native storages
  register so a Harness/Session hands them to the service directly instead of through JS callbacks
  — required, or a sync request waiting behind a commit that needs a JS storage callback deadlocks).
- Harness glue: `pi-durable:internal/harness` (the public names `js/index.js` re-exports),
  `pi-durable:internal/harness-service` (port of `durable-task-adapter.mjs`: `Harness`,
  `watchEvents`, `conversationViews`, `transport`, `runtime`, `invokeCodemodeGlobal`, the Session
  binding it installs into `session.js`, and every host callback), `pi-durable:internal/registry`
  (`createRegistry`, `resolveAgent`, `resolveSettings`, `agentHooks`, `sectionExtension`,
  `toolExtension`, `hydrateAgent`, `snapshot`), TS reference-path modules under
  `internal/src/durable/harness/*` (agent, compaction, context, define, events, generation, harness,
  inbox, live, native-task, prompt, provider, registry, telemetry, tool, usage, view) and
  `internal/src/durable/timers.js` (`nativeTimer`, `hostTimers`).
- Semantics kept from TS reference: built-in generation/tool/compaction tasks always run natively (JS
  shells keep name/version/initial/migrate/phase names); without `HarnessOptions.setTimer`, timers
  are armed with JS `setTimeout` (so fake timers see them); without `HarnessOptions.now` the native
  clock is used while `Date.now` is the builtin and `Date.now` is called through when it has been
  replaced at open (fake timers); context views come back with entries, contributions and messages
  frozen and `messages` aliasing `contributions`.
- pi-ai: the glue imports `mergeClientIdentity`, `withClientIdentity`, `validateToolArguments` and
  `uuidv7` from `@earendil-works/pi-ai` (root). The runner routes `@earendil-works/pi-ai[/…]` and
  `@earendil-works/pi-telemetry` to their pinned `src/` (their `dist/` is stale); an embedder
  provides `@earendil-works/pi-ai` (pi-coding-agent's glue).
