# Durable port status

Native Rust port of the TypeScript reference's `packages/chord` and `packages/durable`
(`@earendil-works/chord` and `@earendil-works/pi-durable`, 1.4.0, pinned at
`5e63241d0294ed3fcfc4388c737a8ea27e09c74e` in `../pi-mono`). The plan is in [PLAN.md](PLAN.md)
and [EMBEDDED-PLAN.md](EMBEDDED-PLAN.md). This file is the current-state summary; it replaces
the day-by-day checkpoint log that used to live here.

Crates: `pi-chord`, `pi-durable`, `pi-durable-jsrt` (the embedded package, see
[crates/pi-durable-jsrt/README.md](../../crates/pi-durable-jsrt/README.md)).

## What's ported

- **Chord subset**: strict JSON ownership, UTF-16 strings, operation tuples, path
  validation/interning, immutable replay, revision diffs, the prepare/adopt lifecycle,
  invocation contexts, cancellation, telemetry parents, bounded watches, output buffering and
  progress pacing, and the SQLite executor/migrations/connection worker.
- **Durable storage**: records, documents, branded IDs, migration/checkpoint helpers, memory
  storage, ordered/fork-aware scans, task/submission indexes, JSONL (format-1 log, sidecars,
  fsync ordering, crash recovery) and SQLite (schema/migrations, native driver, Durable Object
  adapter).
- **Session/transaction kernel**: drafts, checkpoints, retirement, fork copies, task
  ownership/submissions, adoption/publication, close hooks, cancellation, caching and watches,
  with copy-on-write mutation capture over Arc containers.
- **Scheduler**: reservation, phase transitions, joins, sleep/wake, abort, restart limits,
  recovery, registry changes, cancellation ordering, task graph and inspection.
- **Agent runtime**: generation (prepare/request/retry/poll/tools/abort), tool call/execute/abort
  with nested-call admission and reuse, compaction (select/summarize/retry/abort), inbox/live/
  usage/provider documents, prompt rendering/replay, context views, events, and a public
  `Harness`/`Conversation` facade.
- **Environment and tools**: native filesystem (readers, pagination, temp paths), shell execution
  with spills, OS file watching (`notify` plus polling fallback), the coding-tools extension
  (read/write/edit/bash/PowerShell), deferred tool search, and native Photon image processing.
- **Code mode**: the full native replay/model-slot driver, deterministic VM execution, UTF-16
  source/declaration/metadata, and formatting.
- **wasm32 / browser / worker**: builds for `wasm32-unknown-unknown` (native SQLite, tools,
  images and process/fs/watch excluded by target). `IndexedDbStorage` matches the reference's
  on-disk format; a real workerd target runs storage conformance, alarms and a Harness turn over
  Durable Object SQLite.
- **Embedded package** (`pi-durable-jsrt`): both packages run as pi-jsrt virtual modules inside
  pi extensions, in process, no Node or wasm for this path. See its README for the module layout.

## How it's verified

Four checks, all against the pinned reference, never against modified test files:

1. **Native Rust tests** in each crate (`cargo test -p pi-chord -p pi-durable -p pi-durable-jsrt`
   plus `-p pi-ai -p pi-telemetry -p pi-json`).
2. **Stock pi-mono tests over a process transport**, run unmodified against the native crates
   (`scripts/check-durable-port.sh ../pi-mono`), both `PI_DURABLE_DOCUMENT_RETENTION_MS` modes.
3. **Stock pi-mono tests inside the embedded runtime** (`scripts/check-durable-embedded.sh
   ../pi-mono`), via `crates/pi-durable-jsrt/examples/embedded_stock.rs`.
4. `cargo test -p pi-coding-agent --test extensions_durable` proves an extension can import both
   packages through the real extension host.

Both gate scripts are slow; run them on a quiet machine.

### Test numbers

- `pi-chord`: 240 tests (up from 78 after the Chord test-case mapping below), all binaries,
  Clippy `-D warnings` and `cargo fmt --check` clean.
- `pi-durable` / `pi-json`: 432 tests passing alongside the Chord mapping work.
- Latest full native+stock combined gate: **858 Rust checks and 2530 stock executions across 63
  unchanged original files, both retention modes (126 file/mode groups)**. A later additive
  API/build commit kept all 858 Rust checks green but was not rerun against the full 2530-case
  stock matrix; no single later run claims that combined total.
- Embedded gate: 1372/1372 harness cases across 38 files, 700/700 session cases across 8 files,
  359/379 tool/env/image/testing cases (20 justified n/a, see below), and the 227 Chord cases
  including the 36 embedded-only ones, all in both retention modes. Full counts and per-file
  breakdowns are in `porting/durable/mappings/embedded-{harness,session,tools,env,chord}.json`.
- Chord test-case mapping: 227 reference cases across the 11 previously-pending Chord test files,
  185 native, 36 embedded-only (JS prototype/accessor/proxy/identity/GC semantics the embedded
  runtime now exercises), 6 not applicable.

### Parity map

`python3 scripts/merge-durable-mappings.py --write` and `python3 scripts/durable-inventory.py
../pi-mono --check` both pass as of this update. Current counts:

- Parity map: **227 verified, 13 partial, 0 pending**.
- Reference inventory: Chord 103 files / 32,845 lines / 524 literal test cases; durable 210
  files / 69,111 lines / 1,039 literal test cases (dynamic and parameterized cases need the
  reference suites to run and aren't in this static count).

## Open items

- **Response-side U+FFFD in UTF-16 provider bodies.** Provider response text with lone
  surrogates is replaced with U+FFFD before it reaches durable assistant values
  (`pi_json`'s documented native contract). This is intentional and pinned by tests, not a bug,
  but it means a lone surrogate a model actually sent is not recoverable from the stored
  transcript.
- **pi-ai in the embedded test runner is test-host only.** The embedded runner resolves
  `@earendil-works/pi-ai` to the pinned TypeScript reference source to drive Chord/durable tests
  that need `/providers/faux`, `/providers/openai` and similar subpaths; the real extension host
  runs on pi-rs's native `pi-ai`. Extracting a shared `pi-ai-jsrt` crate so the embedded runner
  uses the same native provider is future work.
- **No real model provider on wasm32.** Harness turns on wasm32 are verified only with the faux
  provider; CORS/fetch behavior for real providers is unverified there.
- **Typed Journal cost.** Code mode's typed `pi.codemode.journal` document has a representation
  cost versus the reference's untyped journal; not yet measured against a target budget.
- **Load sensitivity.** At a machine load average of 200 or more, "forgets ended subagent chains" can exceed
  its 5-second deadline (2/40 in one batch, with the host-ack step gate described below in
  place); the embedded 20,000-deep retention case times out at that load with or without the
  gate.
- **Chord exports outside the durable subset are not embedded.** 19 exports in
  `@earendil-works/chord` root (host/contract/link/facet/replicated-state APIs), 5 in
  `@earendil-works/chord/node`, 1 in `@earendil-works/chord/telemetry-sqlite` and 1 in
  `@earendil-works/pi-telemetry/testing` have no native counterpart and are recorded as a
  deliberate scope decision, not a gap to close.
- **`revision-validator` is partial.** The JS Proxy draft port (`delta/draft.js`) reproduces the
  reference's `draft.ts`/`revision-validator.ts` behavior for the cases exercised by the 36
  embedded-only Chord tests, but the validator itself is not fully mapped case by case.
- **20 codemode cases are n/a in the embedded runner**, out of 379 in the embedded tools/codemode
  gate: 2 fingerprint-format cases (durable code mode refuses to resume across a VM fingerprint
  change, matching the reference's QuickJS-version behavior), 3 interrupt-budget-unit cases (QuickJS
  counts polls, pi-codemode counts loop iterations/promise jobs), 1 no-Boa-heap-limit case, 8
  custom raw-worker fixture scripts, and 6 Node `Worker.prototype` spies/process fixtures that
  run the TS reference's source directly.

## Performance fixes behind the current numbers

- **Scheduler retention.** The 20,000-deep retention case was latency-bound, not CPU-bound: one
  native commit per task plus quadratic costs in ownership-chain size (overlay link rebuilding,
  `IndexSet`/`IndexMap` shift-removal, a full `TaskTree::sizes()` per commit) and one SQLite
  worker round trip per statement. Fixing these brought the native 20k-chain case from 8.8-15.4s
  to 2.1-2.8s on MemoryStorage and from 21.8-34.6s to 6.8-8.1s on SQLite; the stock (process
  transport) 20k case went from a base 23-104s to 13-45s.
- **Host-ack step gate.** Under load, native scheduler steps could overtake the JS caller's own
  continuation, which the reference always runs first (microtask order). An optional
  `StepGate(TaskId)`, awaited before each step and acknowledged once the host's microtask queue
  drains, fixed two retention cases that failed under load in both the process bridge and the
  embedded runtime (process bridge: 0/40 failures, was 4/40 and 8/25; embedded: 0/32, was 7/16).
  Cost is about 5% on step-heavy cases under heavy load. This is the gate referenced in the load
  sensitivity item above: it narrows the failure window but doesn't close it at very high load.

## Design notes that must survive future work

- Chord's `json::Value`/`JsonString` preserve lone UTF-16 surrogates; `serde_json::Value` cannot
  represent them. Storage record decoders use raw-value dispatch, not serde's buffered
  tagged-enum representation, to keep this lossless end to end.
- JSON numeric equality and stringify follow JavaScript semantics (number formatting, index-key
  ordering), not Rust's.
- `MemoryStorage::prepare_commit` validates all global IDs/actions before mutation and is
  idempotent against stale competing preparations; `commit` serializes adoption while allowing
  reads inside the callback.
- Backend adoption runs synchronously after durability and before the commit future resolves;
  missing adoption, adoption failure, and backend rejection before/after adoption are handled
  separately.
- Session close seals admission synchronously, ends observers in registration order, then runs
  `before_close`, drains admitted commits/cleanup writes and closes storage.
- Task transitions, abort marks, reservations and decisive steps share one Session mutation line;
  a task cannot create new owned work in the commit that finishes it.
- No Node or TypeScript runtime is part of the production crates. Node is used only by fixture
  generators, the reference verifier, and the stock-test transports.

## Reference commands

```sh
# Native + stock process-transport gate
bash scripts/check-durable-port.sh ../pi-mono

# Embedded (pi-jsrt) gate
bash scripts/check-durable-embedded.sh ../pi-mono

# Parity bookkeeping
python3 scripts/merge-durable-mappings.py --write
python3 scripts/durable-inventory.py ../pi-mono --check
```
