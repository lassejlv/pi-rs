# Embedded JavaScript package exports (milestone 8)

Reference: pi-mono `5e63241d0294ed3fcfc4388c737a8ea27e09c74e`, `@earendil-works/pi-durable` 1.4.0 and `@earendil-works/chord` 1.4.0.
The work lives on `dev` in this repository. This was read-only planning when written; the embedded crate (`crates/pi-durable-jsrt`) is now implemented. Current scope and test counts are in [STATUS.md](STATUS.md); treat the plan below as the design record, not the current state.

## 1. What "embedded exports" means here

**Recommendation: the embedded package is a set of `pi-jsrt` virtual modules (Boa), one per TS reference `exports` subpath of both packages, whose JS glue forwards to native `pi-durable`/`pi-chord` through in-process host functions. No N-API, no wasm for this milestone.**

Evidence:

- `crates/pi-jsrt` is the repo's only embedded JS runtime ("Embedded pure-Rust JavaScript runtime (Boa + oxc) used by pi extensions and code mode", `crates/pi-jsrt/src/lib.rs:1-5`). Every STATUS.md use of "embedded" (embedded-runtime group, "embedded draft bindings", "embedded-only" mapping class) refers to it. There is no `napi`, `neon` or Node addon anywhere in the workspace.
- The established pattern already exists for four packages: `crates/pi-coding-agent/src/core/extensions/js/mod.rs:442-491` registers `@earendil-works/pi-ai`, `/pi-tui`, `/pi-agent-core`, `/pi-coding-agent` and `typebox` as `VirtualModule::EsmJs` glue (`crates/pi-coding-agent/js/*.js`) over `__piHost.call`/`callAsync` (`js/bridge.js`), dispatched in Rust by `extensions/js/api.rs::dispatch{,_async}`; parity is tracked per runtime export in `crates/pi-coding-agent/js/EXPORTS.md`. PORTING.md ("Extensions in Rust") states the contract: existing extensions import these specifiers and load unchanged.
- Consumers. The TS reference, nothing outside `packages/durable` and `packages/env` depends on `pi-durable`; in pi-rs neither `pi-agent` nor `pi-coding-agent` link `pi-durable` (grep of both `src/` trees). The consumer of the embedded package is therefore JavaScript that runs inside pi-rs: extensions and embedders loaded by `pi-coding-agent`'s extension host, and durable's own code-mode/extension JS. That host is pi-jsrt, so a Node addon would not serve it, and a wasm build cannot host the native SQLite/JSONL/shell/watch/Photon services.
- wasm stays where it is: `crates/pi-durable-codec-stock` (test-only cdylib) and the `indexeddb` feature (`storage/indexeddb/web.rs`, wasm32 + wasm-bindgen) are the browser path and are not the "embedded exports".
- The process transport (`crates/pi-durable/examples/task_stock_bridge.rs` + `examples/support/*.rs`, 86 named methods such as `session.open`, `tx.task`, `doc.watch`, `registry.install`, `submission.submit`, `bound.submit`, `tool.*`, `codemode.*`, `graph.*`, `prompt.*`) is the already-proven RPC vocabulary. The embedded binding reuses that vocabulary in-process (pi-jsrt host functions instead of pipes, `JsHandle` callbacks instead of handle ids), so the Rust side is a thin re-host of `examples/support/*`, not a new design.

Shape:

- New crate `crates/pi-durable-jsrt` (library + examples): `js/` glue tree laid out exactly like the TS reference `src/` entry points (`index.js`, `env/index.js`, `env/node.js`, `codemode/index.js`, `tool-search/index.js`, `tools/index.js`, `images/{index,node,cloudflare}.js`, `storage/{memory,indexeddb}.js`, `storage/jsonl/{index,node}.js`, `storage/sqlite/{index,node,cloudflare}.js`, `testing/index.js`, and `chord/{index,context/index,output,delta/index,sqlite/index,sqlite/node,sqlite/cloudflare}.js`); `src/host.rs` with `install(rt: &JsRuntime, options) -> DurableHost` registering `__piDurable.call`/`callAsync` host functions, the virtual modules and the `@earendil-works/pi-durable[/…]`, `@earendil-works/chord[/…]` aliases; `src/dispatch/*.rs` grouped by the method prefixes above.
- `pi-coding-agent` mounts it next to pi-ai (`extensions/js/mod.rs`), so `import { Harness } from "@earendil-works/pi-durable"` works in extensions; `pi-agent` is untouched.
- Values: strict JSON crosses as `pi_json::strict::Value` (UTF-16 lossless, STATUS "Design details"), not `serde_json::Value`; JS functions/objects cross as `JsHandle` markers (`pi_jsrt::convert`); `undefined` is carried explicitly where the TS reference API distinguishes it (copyJson options, policy members). Error classes (`ConversationBusy`, `ReadOnlySession`, `ReadAfterWrite`, `SessionFailed`, `StorageRequestError`, `FileError`, `ExecutionError`, `JsonlCorruptionError`, `JsonlStoragePoisonedError`, `ChordError`, `PathError`, `UnsafePathError`, `WatchError`) are JS classes in the glue reconstructed from the typed native discriminator, as `scripts/stock/durable-native-transport.mjs::failure` does today.
- Acceptance runs the unchanged TS reference vitest files *inside* pi-jsrt: a `vitest` virtual module (describe/it/test/expect/vi/afterEach/afterAll/beforeAll/onTestFinished/expectTypeOf, the exact import set used by the 110 test files) plus `aliases` that redirect each `../src/<entry>.ts` and `@earendil-works/*` specifier to the glue module, driven by a Rust runner example. The existing Node stock gate stays as the process-transport proof; the embedded gate is additive.

## 2. Export surface

Type-only exports are erased by TS stripping and are not listed (same convention as `js/EXPORTS.md`). "glue" = implemented in the JS module itself over native primitives. "missing" = no native item today.

### `@earendil-works/pi-durable`

| Subpath | Runtime export | Native item | Status |
| --- | --- | --- | --- |
| `.` | `defineDoc`, `defineDocFamily` | `pi_durable::define_doc`, `define_doc_family` (`src/documents.rs:418,446`) | glue: JS definitions keep `initial/migrate/checkpointWhen` callbacks as `JsHandle`s (method `docInitial/docMigrate/docCheckpoint`) |
| `.` | `UserEntry`, `AssistantEntry`, `SystemEntry`, `ToolResultEntry`, `ResetEntry`, `CompactionEntry`, `defineEntry` | `USER_ENTRY`…`COMPACTION_ENTRY`, `define_entry` (`src/entries.rs`) | native |
| `.` | `ConversationBusy`, `ReadAfterWrite`, `ReadOnlySession`, `SessionFailed`, `StorageRequestError` | `ErrorKind::{ConversationBusy, ReadAfterWrite, ReadOnly, SessionFailed, StorageRequest}` (`src/errors.rs`) | glue classes + discriminator |
| `.` | `AgentDoc`, `configure`, `DEFAULT_COMPACTION_POLICY`, `DEFAULT_PROGRESS_POLICY`, `DEFAULT_RESTART_LIMIT`, `DEFAULT_RETRY_POLICY` | `AGENT_DOC`, `configure`, `harness::settings::DEFAULT_*` | native |
| `.` | `CompactionTask`, `GenerationTask`, `ToolTask` | `COMPACTION_TASK`/`compaction_task`, `generation_task`, `TOOL_TASK`/`tool_task` | native (`registry.task` method) |
| `.` | `defineExtension`, `defineTool`, `hook`, `section`, `wrapSection`, `wrapTool` | `harness::define::*` | glue over `registry.*` methods; JS tool `execute`/section `render`/hook callbacks stay `JsHandle`s |
| `.` | `watchEvents` | `watch_events` (`harness/events.rs`) | native (`doc.watch` frame delivery) |
| `.` | `DEFAULT_HARNESS_IDENTITY`, `Harness` | `default_harness_identity()`, `Harness::open/root/create_conversation/conversation/submit/…` (`harness/harness.rs:314-830`) | native (`bound.*`, `submission.*`, `conversation.*`, `graph.*` methods) |
| `.` | `InboxDoc`, `LiveDoc`, `ProviderDoc`, `UsageDoc`, `NestedCallDoc` | `INBOX_DOC`, `LIVE_DOC`, `PROVIDER_DOC`, `USAGE_DOC`, `NESTED_CALL_DOC` | native |
| `.` | `createRegistry` | `create_registry` | native |
| `.` | `DURABLE_TELEMETRY_SCHEMA` | `harness::telemetry::DURABLE_TELEMETRY_SCHEMA` | native (JSON constant) |
| `.` | `createSession` | `Session::new(storage, SessionOptions)` (`session/mod.rs:179`) | native (`session.*`, `tx.*`, `doc.*`) |
| `.` | `MemoryStorage` | `storage::MemoryStorage::new` | native; JS `Storage` implementations (tests' controlled backends) cross as `JsHandle` capabilities (`examples/support/task_remote_storage.rs`) |
| `.` | `defineTask` | `define_task` (`src/tasks.rs:171`) | glue: JS phase handlers as `JsHandle`s (`phase.enter`/`phase.turn`) |
| `.` | `ROOT_CONVERSATION_ID` | `types::ROOT_CONVERSATION_ID` | native |
| `./env` | `ok`, `err`, `getOrThrow`, `getOrUndefined`, `toError` | none (pure JS helpers) | glue |
| `./env` | `FileError`, `ExecutionError` | `env::FileError`, `env::ExecutionError` | glue classes + discriminator |
| `./env` | `rangeDecoder`, `StreamDecoder`, `startsWithBom`, `LineScanner` | `env::decode::{RangeDecoder, StreamDecoder, starts_with_bom}`, `env::LineScanner` | native (same code `pi-durable-codec-stock` already binds for wasm) |
| `./env/node` | `NodeExecutionEnv` | `env::NativeExecutionEnv` (+ `NativeFileSystem`, `native_shell`, `native_watch`) | native (`examples/env_stock_bridge.rs` vocabulary) |
| `./codemode` | `CODEMODE_MODULE_VERSION`, `CODEMODE_TOOL_NAME`, `CodemodeStoreDoc`, `createCodemodeExtension`, `createCodemodeTool`, `DEFAULT_CODEMODE_INLINE_BUDGET` | `codemode::types::*`, `CODEMODE_STORE_DOC`, `codemode::{create_codemode_extension, create_codemode_tool}`, `codemode::describe::DEFAULT_CODEMODE_INLINE_BUDGET` | native; VM is `pi-codemode` (Boa), user globals/provider callbacks as `JsHandle`s (`examples/support/codemode_vm_bridge.rs`) |
| `./tool-search` | `createToolSearchExtension`, `createToolSearchTool`, `DEFAULT_TOOL_SEARCH_LIMIT`, `TOOL_SEARCH_TOOL_NAME` | `tool_search::*` | native |
| `./tools` | `createBashTool`, `createPowerShellTool`, `createEditTool`, `createReadTool`, `createWriteTool`, `createCodingTools`, `CodingTools`, `DEFAULT_IMAGE_LIMITS` | `tools::{create_bash_tool, create_powershell_tool, create_edit_tool, create_read_tool, create_write_tool, create_coding_tools, coding_tools}`, `tools::image_processor::DEFAULT_IMAGE_LIMITS` | native (`builtin.*` methods); `prepare` callbacks as `JsHandle`s |
| `./images` | `createPhotonImages(wasm)` | `images::create_photon_images()` | native Photon; the wasm module argument is accepted and ignored (document it) |
| `./images/node` | `createNodePhotonImages(options)` | `images::create_native_photon_images(NodePhotonImagesOptions)` | native |
| `./images/cloudflare` | `createCloudflarePhotonImages` | none (`parity-map` pending) | glue alias to native Photon; structural only |
| `./storage/memory` | `MemoryStorage` | as above | native |
| `./storage/indexeddb` | `IndexedDbStorage` | `storage::indexeddb::IndexedDbStorage<B: IdbBackend>`; only `web.rs` (wasm32) and `MemoryIdbFactory` backends exist | **missing**: a `JsHandle`-backed `IdbBackend` for jsrt (stretch, see risks) |
| `./storage/jsonl` | `JsonlStorage`, `JsonlCorruptionError`, `JsonlStoragePoisonedError` | `storage::JsonlStorage`, `ErrorKind::{JsonlCorruption, JsonlPoisoned}` | native |
| `./storage/jsonl/node` | `openNodeJsonlStorage`, `JsonlStorage` | `JsonlStorage::open_native` | native |
| `./storage/sqlite` | `applySqliteMigrations`, `CURRENT_SQLITE_SCHEMA_VERSION`, `SQLITE_MIGRATIONS`, `SqliteStorage` | `storage::sqlite::migrations::*`, `storage::SqliteStorage` | native; JS `SqliteDatabase` objects (TS reference driver in 3 stock files) cross as `JsHandle` executors |
| `./storage/sqlite/node` | `openNodeSqliteStorage`, `NodeSqliteDatabase`, `openNodeSqliteDatabase` | `open_native_sqlite_storage`, `pi_chord::sqlite::{NativeSqliteDatabase, open_native_sqlite_database}` | native |
| `./storage/sqlite/cloudflare` | `openDurableObjectSqliteStorage`, `DurableObjectSqliteDatabase` | `open_durable_object_sqlite_storage`, `pi_chord::sqlite::cloudflare` | native, structural (no workerd) |
| `./testing` | `createExpectAssertions`, `createEnvConformance`, `registerEnvConformance`, `registerStorageConformance`, `createStorageConformance`, `STORAGE_MEMORY_SCALES`, `STORAGE_READ_BENCHMARKS`, `STORAGE_WRITE_BENCHMARKS`, `seedStorageBenchmark`, `seedStorageWriteBenchmark`, `storageBenchmarkPrimaryRecordCount`, `TIMING_SCALE` | `pi_durable::testing::*` per `mappings/durable-testing.json` (`run_env_conformance`, `run_storage_conformance`, `create_storage_conformance_for`, `storage_read_benchmarks()`, …) | glue: the JS `register*` drivers (describe/it/expect registration) are re-implemented in JS over native case lists; `ExpectLike`/`StorageConformanceRunner` are the caller's objects |
| `./package.json` | JSON | none | glue (static JSON module with name/version/exports) |

### `@earendil-works/chord`

PLAN.md scopes the native Chord to the subset durable uses. Everything else is listed so the audit is explicit.

| Subpath | Runtime export | Native item | Status |
| --- | --- | --- | --- |
| `.` | `copyJson` | `pi_chord::copy_json` | glue: strict-JSON copy with `omitUndefinedProperties`, cycle/accessor/sparse rejection done in JS before crossing (the two `json.test.ts` embedded-only cases) |
| `.` | `isJsonValue` | `pi_json::strict::parse` + JS structural check | glue |
| `.` | `Watch`, `NativeWatch` | `pi_chord::{Watch, NativeWatch}` | native (`doc.watch`/`graph.*` frames) |
| `.` | `ChordError`, `forbidden`, `invalid`, `isChordError` | none | glue classes (needed by durable error paths) |
| `.` | `bind`, `coverage`, `hostFamily`, `hostNamed`, `local`, `method`, `opaque`, `remote`, `createHost`, `defineFacet`, `inflateMessage`, `ipcLink`, `memoryLinkPair`, `PACK_THRESHOLD`, `webSocketLink`, `defineRegistry`, `replicatedState`, `acceptWebSockets`, `PROTOCOL_VERSION` | none (host/contract/link/facet/replicated-state are outside the durable subset) | **missing**; not exported by the embedded module; their 38 chord test files are out of acceptance (decision to record in STATUS.md) |
| `./context` | `BACKGROUND_CONTEXT`, `TODO_CONTEXT`, `createContextKey`, `withContextValue`, `withTelemetry`, `withAbortSignal`, `withoutAbortSignal`, `withCancel`, `awaitWithContext` | `pi_chord::context::{BACKGROUND_CONTEXT, TODO_CONTEXT, create_context_key, with_context_value, with_telemetry, with_abort_signal, without_abort_signal, with_cancel, await_with_context}` | native (`context.*` methods; JS `AbortSignal` bridged to `AbortController`) |
| `./context` | `traceIn` | none | glue over `with_telemetry` + `pi_telemetry` span |
| `./output` | `sanitizeOutput`, `boundOutput`, `characterEnd`, `OutputBuffer`, `utf8ByteLength`, `PROGRESS_BYTES_PER_SECOND`, `Progress` | `pi_chord::output::{sanitize_output, bound_output, character_end, OutputBuffer, utf8_byte_length, PROGRESS_BYTES_PER_SECOND}`, `output::progress::Progress` | native |
| `./delta` | `isReplace`, `isBase`, `overlap`, `diffRevisions`, `RESERVED_SEGMENTS`, `assertValidWireOp`, `assertSafePath`, `apply`, `applyImmutable`, `applyImmutableBatches`, `encoder`, `decoder` | `pi_chord::delta::*` (`is_replace`, `is_base`, `overlap`, `diff_revisions`, `RESERVED_SEGMENTS`, `assert_valid_wire_op`, `assert_safe_path`, `apply`, `apply_immutable`, `apply_immutable_batches`, `Encoder`, `Decoder`) | native |
| `./delta` | `assertValidOp`, `UnsafePathError`, `PathError` | `Op::from_value`, `delta::Error` variants | glue classes + discriminator |
| `./delta` | `track` | `pi_chord::delta::{track, Tracker, Change, Prepared}` (explicit op batches) | **glue, the largest piece**: the JS `Proxy` draft (`packages/chord/src/delta/draft.ts`, `revision-validator.ts`, both `pending` in parity-map) ported to JS, capturing edits into explicit ops that native `Change::apply_ops` validates/prepares. This is what the 36 chord embedded-only cases exercise |
| `./node` | `nodeLoader`, `childControl`, `createHostManager`, `forkDriver`, `processLock` | none | **missing**; not exported (host manager is outside the subset) |
| `./telemetry-sqlite` | `createSqliteTelemetry` | none (`pi-telemetry` has memory/noop stores only) | **missing**; not exported this milestone |
| `./sqlite` | `applySqliteMigrations`, `validateSqliteSchemaPrefix` | `pi_chord::sqlite::{apply_sqlite_migrations, validate_sqlite_schema_prefix}` | native |
| `./sqlite/node` | `NodeSqliteDatabase`, `openNodeSqliteDatabase` | `NativeSqliteDatabase`, `open_native_sqlite_database` | native |
| `./sqlite/cloudflare` | `DurableObjectSqliteDatabase` | `pi_chord::sqlite::cloudflare` | native, structural |
| `./package.json` | JSON | none | glue |

## 3. Acceptance criteria

All runs: pinned reference check (`git rev-parse HEAD == 5e63241…`), the test file's bytes compared before/after (as `scripts/run-stock-durable-session.mjs` does), both `PI_DURABLE_DOCUMENT_RETENTION_MS` modes for durable files, no name filter, original timeouts, a JSON report under `/tmp/pi-durable-embedded-<file>-report.json` with native counters proving execution reached Rust.

Tier 1 (must pass unchanged in pi-jsrt against the embedded modules):

- The 40 cases classified `embedded-only` in `porting/durable/mappings/*.json`: `chord/test/json.test.ts` (2), `delta-clone.test.ts` (2), `delta.test.ts` (4), `delta-apply-immutable.test.ts` (1), `delta-tracker/tracker.test.ts` (19), `delta-tracker/retention.test.ts` (6), `watch.test.ts` (1), `durable/test/storage-runtime-boundary.test.ts` (4). They run as part of their whole files: the 11 mapped Chord files (`json`, `context`, `delta`, `delta-clone`, `delta-diff`, `delta-apply-immutable`, `delta-tracker/tracker`, `delta-tracker/retention`, `output`, `output-skip`, `watch`) must pass complete. The boundary file runs with `../src` resolving to the glue tree (`crates/pi-durable-jsrt/js/`), which must satisfy its no-`node:` import hygiene for `index`, `env/index`, `storage/jsonl/index`, `storage/sqlite/index`.
- The stock files whose runners still keep host-owned JS pieces (`nativeScope` strings in `scripts/run-stock-*.mjs` and `durable-task-adapter.mjs`). Against the embedded package those pieces are ordinary consumer JS, which is the point of the milestone:
  - Session (8 files `session-*.test.ts`): controlled storage, definition callbacks, strict input copying, mutable draft capture, callback result identities, table-record identities.
  - Harness/tasks (`harness-tasks`, `-tools`, `-tools-recovery`, `-nested-tools`, `-nested-tools-restart`, `-view`, `-events`, `-generation`, `-generation-recovery`, `-submissions`, `-inspect`, `-task-graph`, `-host-inputs`, `-structured`, `-structured-output`, `-read-only`, `-conversations`, `-reports`, `-lifecycle`, `-compaction`, `-telemetry`, plus the 13 audit files `-input-entries`, `-live-deltas`, `-scheduler-indexes`, `-callers`, `-failed-run-inbox`, `-inbox`, `-cancellation-barrier`, `-sleep`, `-storage-failure`, `-tasks-recovery`, `-blocked`, `-ownership`, `-scheduler-retention`): custom task callbacks, model collections, tool callbacks, schema validation, environments, controlled/persistent storage, host timers, original JS registrations and render callbacks.
  - `harness-context`, `harness-prompt`, `harness-registry`: today the outer Harness/Session/scheduler/backend are *reference* code; embedded must run them on native.
  - `codemode`, `codemode-models`, `codemode-describe`, `codemode-checkpoints`, `codemode/declarations`: controlled VM and user/provider/environment callbacks.
  - `sqlite-storage`, `sqlite-facade`, `sqlite-migrations`, `sqlite-cloudflare`, `jsonl-storage`: TS reference driver objects behind the Rust backend.
  - `env-node`, `env-node-spill`, `env-node-conformance`, `env-line-scan`: opened-file fault-injection callbacks, conformance registration factory (`createEnvConformance` export pending).
  - `tools`, `tools-read-differential`, `tools-images`, `tool-search`: environment subclasses, prepare callbacks, model collections.
- Files not in the stock gate at all today: `memory-storage.test.ts`, `env-truncate.test.ts`, `harness-telemetry-options.test.ts`, `images-wasm-module.test.ts` (passes by accepting the module argument).
- `cargo test -p pi-coding-agent --test extensions_durable` (new): an extension importing `@earendil-works/pi-durable`, `/storage/memory`, `@earendil-works/chord/context` opens a `Harness` on `MemoryStorage` with the faux provider and completes one turn.
- `crates/pi-coding-agent/js/EXPORTS.md` gains both packages with every runtime export above marked `[x]`, `[~]` or `[ ]`, and `scripts/check-durable-port.sh` runs the embedded gate.

Tier 2 (stretch, reported separately): `indexeddb-storage.test.ts` (fake-indexeddb loaded from `node_modules` by jsrt over a `JsHandle` `IdbBackend`).

Not embedded (record as n/a in parity-map): `test-workers/*` (workerd), `provider-session-cache-e2e`, `system-order-cache-e2e` (live APIs), `types.test.ts`/`spec-usage.test.ts` (compile-only), `*-bench.ts`, the 38 Chord host/contract/link/state/manager/telemetry-sqlite test files.

## 4. Work breakdown

Five chunks. A ships first (it is the contract the others compile against); B–E run in parallel; F closes. Each agent owns only the files listed and may add tests under its own `tests/` names.

### A. Substrate: crate, host dispatch, values, vitest shim, runner (1 agent, first)

- Owns: `crates/pi-durable-jsrt/Cargo.toml`, `src/lib.rs`, `src/host.rs` (install, host functions `__piDurable.call/callAsync`, method registry trait `Dispatch`), `src/value.rs` (`pi_json::strict::Value` ⇄ Boa `JsValue` lossless UTF-16 conversion, explicit-undefined marker, `JsHandle` adoption), `src/errors.rs` (typed discriminator ⇄ JS class table), `js/bridge.js`, `js/vitest.js` (describe/it/test/expect incl. `.resolves/.rejects/.not`, `vi.spyOn/mocked/restoreAllMocks/waitFor/useFakeTimers/useRealTimers/advanceTimersByTimeAsync/getTimerCount/setSystemTime/mock`, `afterEach/afterAll/beforeAll/onTestFinished`, `expectTypeOf` no-op), `examples/embedded_stock.rs` (runner: `--reference <pi-mono> --suite <file> [--retention]`, pins the commit, byte-checks the file, maps `../src/*.ts` and `@earendil-works/*` via `RuntimeOptions::aliases`, writes the report), `Cargo.toml` workspace member line.
- Also owns in `crates/pi-jsrt`: `src/ops/sqlite.rs` (+ `ops/mod.rs` entry) providing `node:sqlite` `DatabaseSync` over `rusqlite` (6 TS reference files import it); a `gc()` global and timer-queue control ops (`__jsrtTimers.freeze/advance/count`) that `js/vitest.js` fake timers use, behind `RuntimeOptions` flags.
- Depends on: nothing.
- Verify: `cargo test -p pi-durable-jsrt --test substrate` (round-trips lone surrogates, undefined, handles, every error class) and `cargo run -p pi-durable-jsrt --example embedded_stock -- ../pi-mono --suite=packages/chord/test/output.test.ts` (pure-function file, proves the shim/runner before B–E land); `cargo test -p pi-jsrt --test node_sqlite`.

### B. Chord embedded modules (1 agent)

- Owns: `crates/pi-durable-jsrt/js/chord/{index,context/index,output,delta/index,sqlite/index,sqlite/node,sqlite/cloudflare,package.json}.js`, `js/chord/delta/draft.js` (JS Proxy draft port of `packages/chord/src/delta/draft.ts` + `revision-validator.ts` over native `track`/`Change::apply_ops`), `src/dispatch/chord.rs` (`context.*`, `delta.*`, `output.*`, `watch.*`, `sqlite.*` methods), `tests/chord_embedded.rs`.
- Depends on: A.
- Verify: `for f in json context delta delta-clone delta-diff delta-apply-immutable delta-tracker/tracker delta-tracker/retention output output-skip watch; do cargo run -p pi-durable-jsrt --example embedded_stock -- ../pi-mono --suite=packages/chord/test/$f.test.ts; done` — all 227 cases, including the 36 embedded-only; `cargo test -p pi-durable-jsrt --test chord_embedded`.

### C. Durable Session/storage core (1 agent)

- Owns: `js/index.js` (the `.` entry: documents, entries, errors, `createSession`, `defineTask`, `MemoryStorage`, `ROOT_CONVERSATION_ID`; Harness symbols re-exported from D's module `js/harness.js`), `js/storage/{memory,jsonl/index,jsonl/node,sqlite/index,sqlite/node,sqlite/cloudflare,indexeddb}.js`, `js/package.json.js`, `src/dispatch/session.rs` (`session.*`, `tx.*`, `doc.*`, `transport.ready`), `src/dispatch/storage.rs` (JS `Storage`/`SqliteDatabase` capabilities as `JsHandle`s, re-hosting `examples/support/{session_bridge,document_bridge,strict_bridge,memory_bridge,task_remote_storage}.rs`), `tests/session_embedded.rs`.
- Depends on: A (B's `Watch`/context glue is consumed via alias; no file overlap).
- Verify: the 8 `session-*.test.ts`, `memory-storage`, `jsonl-storage`, `sqlite-storage`, `sqlite-facade`, `sqlite-migrations`, `sqlite-cloudflare`, `storage-runtime-boundary` through `embedded_stock` in both retention modes; `cargo test -p pi-durable-jsrt --test session_embedded`.

### D. Harness, registry, agent services (1–2 agents; split at `tool.*`/`codemode.*` if two)

- Owns: `js/harness.js` (Harness, createRegistry, defineExtension/defineTool/hook/section/wrap*, watchEvents, docs, tasks, DEFAULT_*), `js/codemode/index.js`, `js/tool-search/index.js`, `src/dispatch/{registry,harness,tool,codemode,telemetry}.rs` (`registry.*`, `submission.*`, `bound.*`, `conversation.*`, `graph.*`, `prompt.*`, `context.*`, `phase.*`, `change.*`, `host.turn`, `tool.*`, `progress.*`, `codemode.*`, `builtin.describe/modelExports`), re-hosting `examples/support/{registry,generation,compaction,model,tool,tool_environment,submission,task_graph,view,prompt,context,telemetry,bound_conversation,codemode,codemode_vm,builtin}_bridge.rs`, `tests/harness_embedded.rs`.
- Depends on: A; runs against C's session module through aliases only.
- Verify: every `harness-*.test.ts` (33 files), `codemode*.test.ts` (4), `codemode/declarations.test.ts`, `tool-search.test.ts`, `harness-telemetry-options.test.ts` through `embedded_stock` in both modes; `cargo test -p pi-durable-jsrt --test harness_embedded`.

### E. Environment, tools, images, testing (1 agent)

- Owns: `js/env/{index,node}.js`, `js/tools/index.js`, `js/images/{index,node,cloudflare}.js`, `js/testing/index.js` (JS `registerStorageConformance`/`registerEnvConformance`/`createStorageConformance`/`createEnvConformance` drivers over native case lists), `src/dispatch/{env,tools,images,testing}.rs` (`fs.*`, `shell.*`, `watch.*`, `spill.*`, `builtin.*`, `image.*` methods re-hosting `examples/env_stock_bridge.rs` and `examples/support/builtin_bridge.rs`), `tests/env_embedded.rs`.
- Depends on: A.
- Verify: `env-node`, `env-node-spill`, `env-node-conformance`, `env-line-scan`, `env-truncate`, `tools`, `tools-read-differential`, `tools-images`, `images-wasm-module` through `embedded_stock` in both modes (original platform skips retained); `cargo test -p pi-durable-jsrt --test env_embedded`.

### F. Integration, gate, ledger (1 agent, after B–E report green)

- Owns: `crates/pi-coding-agent/src/core/extensions/js/mod.rs` (mount `pi_durable_jsrt::install` beside pi-ai), `crates/pi-coding-agent/Cargo.toml`, `crates/pi-coding-agent/tests/extensions_durable.rs`, `crates/pi-coding-agent/js/EXPORTS.md` (two new package sections), `scripts/check-durable-port.sh` (embedded gate loop), `porting/durable/parity-map.json`, `porting/durable/mappings/*.json` (embedded-only → verified with evidence), `porting/durable/STATUS.md`, `porting/durable/VALIDATION.json`, `porting/durable/PLAN.md` current-state line.
- Depends on: A–E.
- Verify: `cargo test -p pi-coding-agent --test extensions_durable`, `cargo clippy -p pi-durable-jsrt -p pi-jsrt --all-targets -- -D warnings`, `cargo fmt -p pi-durable-jsrt --check`, full `scripts/check-durable-port.sh ../pi-mono` with terminal `Durable port checks passed.`, `python3 scripts/durable-inventory.py ../pi-mono --check`.

## 5. Risks and unknowns

1. **Lossless strings across the boundary.** `pi_jsrt::convert` moves values as `serde_json::Value`, which cannot hold lone UTF-16 surrogates (STATUS "Design details"). Boa strings are UTF-16, so chunk A must add a direct `pi_json::strict::Value` ⇄ `JsValue` path and never route durable payloads through `serde_json`. Every reference conformance suite distinguishes lone-surrogate identities; this is a correctness gate, not polish.
2. **Re-entrancy on the single JS thread.** Boa's `Context` is `!Send`; sync host functions run on the JS thread. The stock transport needed a worker + `Atomics` pump so synchronous registry getters could serve nested callbacks. In-process, a sync JS → Rust → JS callback is a Boa re-entry; whether `JsHandle::call_blocking` from inside a sync host call is allowed by `runtime.rs` (`in_host_call`, `wait_servicing`) must be settled in chunk A before D relies on it. Fallback: make those registry paths async in the glue where TS reference semantics permit, otherwise fail the milestone early.
3. **vitest shim fidelity.** Fake timers (`vi.useFakeTimers`/`advanceTimersByTimeAsync`, 21 uses) must drive jsrt's real timer queue, not a JS re-implementation; `vi.mock` (2 uses) is hoisted module mocking; `vi.spyOn` on class prototypes must see the glue's real methods. Matcher coverage is finite but must match vitest's deep-equality (`Map`, `-0`, `undefined` vs missing).
4. **GC/heap cases.** 4 of the 6 `retention.test.ts` embedded-only cases need `WeakRef` + `gc()`; Boa has `FinalizationRegistry` jobs (`engine.rs:212`) and a forced collection can be exposed, but the two `heapUsed` cases measure V8 and may remain n/a. Decide per case with evidence, do not force.
5. **JS proxy draft port.** `draft.ts`/`revision-validator.ts` are `pending` in parity-map; the glue must reproduce one-proxy-per-container identity, integer-key ordering, mutator return values and explicit `undefined` writes, and emit op streams native `Change::apply_ops` accepts unchanged. This is the biggest single piece and is on B alone.
6. **Performance under Boa.** `harness-scheduler-retention` 20,000-task chains ran 23–33 s under Node with a 60 s deadline and already has one unexplained 76 s incident. Boa is slower; `RuntimeOptions::loop_iteration_limit` and the sandbox fixture concurrency (`RUST_TEST_THREADS=4`) interact. Expect to need the same 4-way bound and to record, not hide, any deadline miss.
7. **`node:sqlite`, `node:v8`, `node:vm`, `node:child_process` in tests.** jsrt has `vm`, `child_process` and workers (`ops/worker.rs`), lacks `node:sqlite` (A adds it) and `node:v8` (only the heap cases).
8. **IndexedDB in jsrt.** No IDB in Boa; `fake-indexeddb` from `node_modules` over a `JsHandle` backend is unproven; Tier 2 only.
9. **Scope decision on Chord.** The embedded `@earendil-works/chord` is the durable subset; host/contract/link/replicated-state/node-manager/telemetry-sqlite are not exported. This must be written into STATUS.md and parity-map as an explicit decision, or the "full parity audit" in PLAN.md step 8 will read as incomplete.
10. **Build fragility.** Vendored Boa previously emitted an unhashed shared library; STATUS requires serialized Cargo builds for Boa-linked crates. Six agents building `pi-durable-jsrt` and `pi-jsrt` concurrently in one target dir will hit E0460 again; give each agent `CARGO_TARGET_DIR` or a worktree.
11. **Process-transport parity drift.** Re-hosting `examples/support/*.rs` duplicates ~5,400 lines of bridge logic. Prefer extracting the method handlers into `pi-durable-jsrt/src/dispatch/*` and having `task_stock_bridge.rs` call the same functions over its pipe, so both gates exercise one implementation; otherwise the two bindings diverge silently.
