# Replace native codemode engine with djs

This change replaces Boa in `pi-codemode` with `djs-core` and `djs-prelude`, pinned
to `23b4548c4dfe40c833ca271d0b008cd08acb2d01` from
[earendil-works/djs#1](https://github.com/earendil-works/djs/pull/1). Cargo.lock records canonical git
sources; there are no sibling path dependencies. It is based on published main
`9411c5b` and does not bring in the divergent local-main changes.

`pi-jsrt` and extension execution remain on Boa. The eventual goal is full engine
replacement, but this change delivers the native codemode milestone while
upstream executable ES modules and the broader embedding migration are pending.

## Preserved contracts

The public `VmLauncher`/`VmChannel` API, start/delivery messages, thread lifecycle,
remote transport and existing JavaScript prelude are unchanged. No UTF-16 protocol
extensions, pi-json updates or durable runtime changes are included.

The prelude retains its bridge in a closure. Tools and injected globals return
promises; the Rust adapter processes djs host-call suspensions, and delivered
results settle the prelude's promises. There are no filesystem, network, timer,
process, module-loading or general host-dispatch globals in the sandbox.

Persistent JavaScript values use djs GC handles; temporary handles are released
after calls. A regression explicitly collects before start and while awaiting a
tool. Strings continue crossing this base's existing String/JSON protocol. djs
uses UTF-16 internally; this PR does not claim a lossless UTF-16 host protocol.

Deterministic runs retain UTC, monotonic delivery timestamps, ordered result
buffering, disabled WeakRef/FinalizationRegistry and the original pinned
mulberry32 random sequence. Math.random uses a suspended native function with
the same algorithm as the previous Boa adapter. Fingerprints identify the djs
build, prelude, adapter version 7, stack allowance, arena cap, budget, fuel slice
size and invocation cap. Replay must reject a different fingerprint.

Cancellation is checked at host calls and between 10,000-fuel runs. Fatal stops
abort interpreter state, so user catch blocks cannot defeat them. Actual fuel
is charged across short host-call suspensions and deliveries; frequent host calls
cannot reset the budget. Terminal outcomes prevent further host effects. The
prelude's output/store limits are unchanged.

## Intentional differences

- djs is always strict. Undeclared assignment throws ReferenceError and attempted
  writes to frozen built-ins throw TypeError. The tampering test requires every
  mutation to throw, then retains the original tool/JSON/store integrity checks.
  `eval` is indirect and cannot read the caller's locals.
- The memory cap now applies to the whole arena, including ordinary JS objects.
  Boa only capped ArrayBuffer allocations. Failed script allocations throw
  InternalError rather than Boa's ArrayBuffer RangeError. Failure to create the
  VM or allocate host memory reports a sandbox error. The portable cap test
  retains its exact success/error-name assertion with the new engine's name.
- `interrupt_budget` counts instrumented loop iterations plus each 10,000 fuel
  consumed cumulatively. Jobs, built-ins and generated code consume fuel;
  trusted setup does not. Existing `loop_iteration_limit` retains its API name
  but now caps fuel per invocation, including its promise jobs.
- `recursion_limit` is an approximate allowance translated to djs value-stack
  slots with space reserved for the prelude, rather than exact call depth.
  Stack exhaustion throws a catchable RangeError instead of Boa's fatal runtime
  limit. Tests require both successful catching and the uncaught script error.
- Non-deterministic dates use the host's offset at the execution clock, honoring
  TZ on Unix. The offset lookup is cached for up to one second to avoid libc
  timezone-file work at every loop tick. djs's fixed-offset hook does not provide full historical/DST
  timezone behavior. Non-Unix uses UTC, as the previous adapter did.
- Error messages and exact stack columns depend on the engine. The cyclic JSON
  error is now `TypeError: circular reference`, asserted exactly in the test. SharedArrayBuffer
  is unavailable. No snapshot persistence functionality is added.

The Rust interpreter is optimized in development builds so JavaScript execution
and the large output-limit tests remain practical.

## Remaining full replacement

The pinned djs revision executes acyclic source-module graphs with live bindings
and synthetic host exports. It is an experimental upstream PR, not complete ESM.
Extensions still need cyclic graph instantiation/evaluation, namespace objects,
export-star resolution, dynamic import, import.meta, top-level await and TypeScript
module grammar before their engine can switch.

Then migrate `pi-jsrt/src/loader.rs` from Boa's ModuleLoader, Module parsing,
load/link/evaluate promises and synthetic initializers to the upstream API while
preserving TypeScript, package resolution, virtual modules and CJS/JSON interop.
Migrate Boa values and native callback captures to owned djs GC handles, including
host-held functions, conversion markers and shutdown ownership. Adapt
`pi-jsrt/src/engine.rs` JobExecutor, native sync/async functions, timers and host
operations to suspended dispatch. Verify module, runtime, Node shim and extension
suites before switching pi-jsrt. This PR introduces no dummy module APIs or
partial extension backend.

The upstream PR remains unmerged; the exact commit is pinned for reproducible
review and testing. Full pi-jsrt replacement remains a follow-up once the remaining
upstream module contracts are available.
