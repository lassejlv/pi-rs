# djs engine migration

`pi-codemode` now embeds `djs-core` and `djs-prelude`, pinned to
`23b4548c4dfe40c833ca271d0b008cd08acb2d01` in Cargo.toml and Cargo.lock.
No sibling checkout is required to build. This is the native codemode milestone.
`pi-jsrt`, `pi-quickjs`, and extension execution still use QuickJS.

## Codemode embedding

The public `VmLauncher`/`VmChannel` API, thread lifecycle, and remote protocol are
unchanged. The existing prelude implements tools, globals, store helpers, output
limits and lockdown. Its bridge is passed directly and captured in a closure.
No filesystem, network, process, timer, module loader or general host dispatcher
is installed in script globals.

The Rust adapter services synchronous djs host-call suspensions. Tool calls return
prelude promises, resolved through `settle` when the host delivers results. djs
then runs their promise jobs. Persistent values use moving-GC handles. Temporary
argument/result handles are released after calls. The GC regression collects both
before script start and while a tool is pending.

The memory cap applies to the djs arena at creation and throughout execution.
Allocation failures may be caught by scripts; failure to create the VM or host
allocation failure reports a sandbox error. Cancellation and fatal exhaustion
abort the interpreter and cannot be caught by scripts. Interrupts are checked
at host calls and between 10,000-fuel interpreter slices. Terminal reports prevent
further host effects.

Deterministic runs use UTC, disable WeakRef/FinalizationRegistry, set Date from
start/delivery timestamps, buffer out-of-order results and keep time monotonic.
The random seed advances xorshift once from `seed + 1`, preserving existing pinned
random values. Fingerprints name the djs build hash, prelude hash, adapter version
6, stack allowance, heap cap, budget, fuel slice size, invocation cap and UTF-16 mode. QuickJS fingerprints do not
match. Call names, output and delivered strings retain lone surrogates through
djs UTF-16 APIs. There is no new snapshot persistence API.

## Intentional semantic differences

- djs is always strict. Undeclared assignment throws ReferenceError; frozen
  built-in mutations throw TypeError. The tampering test requires each mutation
  to throw, then retains its original tool/JSON/store protection assertions.
  `eval` is indirect and cannot access caller locals.
- `interrupt_budget` counts instrumented loop iterations plus each 10,000 fuel
  consumed, cumulatively across host calls and deliveries. Promise jobs and
  generated code consume fuel. Trusted prelude setup is excluded.
- `loop_iteration_limit` retains its API name but now caps interpreter fuel per
  invocation, including promise jobs. It is not a JavaScript loop count.
- `recursion_limit` is an approximate allowance translated to value-stack slots,
  with space reserved for the prelude, rather than an exact call-depth cap.
- Non-deterministic Date uses the requested/system timezone's offset at the
  execution clock. djs has a fixed-offset hook, so historical dates spanning
  different DST offsets lack full IANA timezone semantics.
- `Date.now()` and `new Date()` see integer milliseconds. djs returns the
  supplied clock unchanged, so a deterministic virtual clock is rounded and
  wall time is truncated before it is installed.
- Exact error messages and stack columns depend on the engine. SharedArrayBuffer
  is unavailable. The interpreter is optimized in development builds to make
  JavaScript execution and large output-limit tests practical.

## Remaining pi-jsrt migration

The pin is one commit past `65ab48705d07ab42f58f81ba42084ab4c5100175`: the
experimental source-module revision from
[earendil-works/djs#1](https://github.com/earendil-works/djs/pull/1). That pull
request is closed and unmerged. It executes acyclic source-module graphs with
live bindings and synthetic host exports. It is not complete ESM. Cycles,
namespace objects, export-star resolution, dynamic import, `import.meta`,
top-level await and TypeScript module grammar are still missing. Codemode does
not load modules. `pi-jsrt` stays on QuickJS until those contracts exist.

Required integration work:

1. Replace raw `JSModuleDef` pointers and loader callbacks in
   `crates/pi-jsrt/src/loader.rs` with upstream source/synthetic module APIs.
   Preserve cycles, live imports/exports, namespace objects, dynamic import,
   import.meta, top-level await, CJS/JSON interop, aliases, virtual modules,
   TypeScript and package resolution.
2. Replace `pi-quickjs` value/object/string/error wrappers with owned djs GC
   handles. Define cloning, host-held callback and shutdown ownership. Preserve
   `pi-jsrt/src/convert.rs` conversion semantics and handle-marker adoption.
3. Adapt `pi-jsrt/src/engine.rs` native callbacks to a suspended host-call
   dispatcher integrated with messages, timers and async operations. Existing
   raw QuickJS job-context hooks provide async-local state and rejection tracking;
   those need actual djs equivalents or a redesigned integration.
4. Run pi-jsrt module/runtime/Node-shim and extension integration suites,
   including callback cleanup, async hooks, cycles, dynamic imports and example
   extensions. Codemode tests cannot establish extension compatibility.

No dummy APIs, stub loader, renamed QuickJS crate or partial optional extension
backend is introduced. Keeping the existing extension runtime working is
necessary until these contracts exist. Parent owns review and publication of
both upstream and pi PRs; this worktree does not push or publish.
