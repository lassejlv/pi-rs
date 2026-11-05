// @earendil-works/pi-codemode over the native pi-codemode crate (src/dispatch/codemode.rs,
// codemode/sdk.rs and codemode/live.rs, `codemode.sdk.*`). Upstream runs QuickJS compiled to wasm,
// in a worker (`CodemodeSandbox`), on the calling thread (`InlineCodemodeSandbox`) or behind a
// remote (`RemoteCodemodeSandbox`, `serveCodemodeRemote`). Every VM is pi-codemode's native
// QuickJS VM on its own thread (not QuickJS in wasm); the inline sandbox keeps its
// interrupt budget, the remote one its protocol and the caller's remote. The host logic (tool
// table, call records, deadline, abort, output and store limits, deterministic runs) is native;
// tools, globals, admission callbacks and remotes stay the caller's JS objects.
//
// Not QuickJS: `loadQuickJSWasm()` resolves to an inert stand-in that every sandbox accepts and
// ignores (a `wasm` promise that rejects still fails executions as upstream does), and a custom
// `workerUrl` script is not run (a missing file still fails as a sandbox error).

import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { call, callAsync, ref } from "pi-durable:bridge";

export {
  DEFAULT_INPUT_SCHEMA_MAX_CHARS,
  renderDeclarations,
  renderToolOutputType,
  renderToolSample,
  renderToolSignature,
  schemaToType,
  toCodemodeIdentifier,
} from "@earendil-works/pi-codemode/declarations";
export {
  CODEMODE_OPTIONS_PREFIX,
  CODEMODE_SOURCE_GRAMMAR,
  CodemodeSourceError,
  parseCodemodeSource,
} from "@earendil-works/pi-codemode/source";

import { toCodemodeIdentifier } from "@earendil-works/pi-codemode/declarations";

const constants = call("codemode.sdk.constants");

export const MAX_OUTPUT_CHARS = constants.MAX_OUTPUT_CHARS;
export const MAX_OUTPUT_ITEMS = constants.MAX_OUTPUT_ITEMS;
export const MAX_STORE_TOTAL_CHARS = constants.MAX_STORE_TOTAL_CHARS;
export const MAX_STORE_VALUE_CHARS = constants.MAX_STORE_VALUE_CHARS;

/** What `loadQuickJSWasm()` resolves to: the VM is native, so there is no module to load. */
const QUICKJS_STANDIN = Object.freeze({ embedded: "pi-codemode QuickJS VM" });

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function abortMessage(signal) {
  const reason = signal?.reason;
  return reason instanceof Error ? reason.message : undefined;
}

/** Upstream's `serializeStore`: every value as JSON, `undefined` values left out. */
function serializeStore(store) {
  const serialized = {};
  for (const [key, value] of Object.entries(store ?? {})) {
    const json = JSON.stringify(value);
    if (json !== undefined) serialized[key] = json;
  }
  return serialized;
}

/** Why a custom worker script cannot start (`undefined`: it exists; the VM is native either way). */
function missingWorker(url) {
  let path;
  try {
    path = url instanceof URL || String(url).includes(":") ? fileURLToPath(url) : String(url);
  } catch (error) {
    return errorMessage(error);
  }
  return existsSync(path) ? undefined : `Cannot find module '${path}'`;
}

/** Upstream results always carry `value` when `ok`, `undefined` included. */
function withValue(result) {
  return result.ok && !Object.hasOwn(result, "value") ? { ok: true, value: undefined, ...result } : result;
}

/** A failed execution that never reached a VM (upstream: a launcher that throws). */
function sandboxFailure(message) {
  return { ok: false, error: { kind: "sandbox", message }, output: [], calls: [] };
}

/**
 * Tool calls still waiting for their tool. Upstream's host keeps a call that never settles pending
 * until the execution ends; here the native side waits on the promise, which nothing else would
 * keep alive, so it is held until it settles or the call is cancelled.
 */
const inflight = new Set();

/** Runs one tool call as upstream's host does: JSON arguments in, JSON value or error message out. */
function invoke(tool, argsJson, abort) {
  const controller = new AbortController();
  const result = (async () => {
    try {
      const args = argsJson === undefined ? undefined : JSON.parse(argsJson);
      const value = await tool.execute(args, { signal: controller.signal });
      return { ok: true, json: value === undefined ? undefined : JSON.stringify(value) };
    } catch (error) {
      return { ok: false, message: errorMessage(error) };
    }
  })();
  inflight.add(result);
  const onAbort = () => {
    controller.abort();
    inflight.delete(result);
  };
  if (call("codemode.sdk.onAbort", abort, onAbort)) onAbort();
  return result.finally(() => inflight.delete(result));
}

function declaration(tool) {
  return {
    name: tool.name,
    description: tool.description,
    inputSchema: tool.inputSchema,
    outputSchema: tool.outputSchema,
    spread: tool.spread === true,
    signature: tool.signature,
  };
}

/**
 * Tool table, defaults, and in-flight executions; the behavior every sandbox shares. The second
 * argument picks where the VM runs (upstream: a `VmLauncher`).
 */
export class CodemodeSandboxBase {
  #toolsByName = new Map();
  #globalsByName = new Map();
  #native;
  #backend;
  #closed = false;

  constructor(options = {}, backend = { kind: "thread" }) {
    this.#backend = backend;
    this.#native = call("codemode.sdk.sandboxNew", {
      kind: backend.kind,
      timeoutMs: options.timeoutMs,
      memoryLimitBytes: options.memoryLimitBytes,
      interruptBudget: backend.interruptBudget,
      tools: (options.tools ?? []).map((tool) => ({ declaration: declaration(tool), tool: ref(tool) })),
      globals: (options.globals ?? []).map((tool) => ({ declaration: declaration(tool), tool: ref(tool) })),
      invoke,
      acquireWorker: backend.acquireWorker,
      onWorkerStarted: backend.onWorkerStarted,
      remote: backend.remote,
      remoteExecute: backend.remoteExecute,
    });
    for (const tool of options.tools ?? []) this.#toolsByName.set(tool.name, tool);
    for (const global of options.globals ?? []) this.#globalsByName.set(global.name, global);
  }

  /** Throws if a tool with the same name is already registered. */
  registerTool(tool) {
    if (this.#toolsByName.has(tool.name)) throw new Error(`Tool "${tool.name}" is already registered`);
    call("codemode.sdk.registerTool", this.#native, declaration(tool), ref(tool));
    this.#toolsByName.set(tool.name, tool);
  }

  unregisterTool(name) {
    call("codemode.sdk.unregisterTool", this.#native, name);
    return this.#toolsByName.delete(name);
  }

  get tools() {
    return [...this.#toolsByName.values()];
  }

  get globals() {
    return [...this.#globalsByName.values()];
  }

  /**
   * `code` is an async function body: `return` and top-level `await` work. Never rejects for
   * script failures; those come back as `{ ok: false }`.
   */
  async execute(code, options = {}) {
    if (this.#closed) throw new Error("Sandbox is closed");
    const backend = this.#backend;
    if (backend.wasm !== undefined) {
      try {
        await backend.wasm;
      } catch (error) {
        return sandboxFailure(`Failed to load QuickJS: ${errorMessage(error)}`);
      }
    }
    const workerError = backend.workerUrl === undefined ? undefined : missingWorker(backend.workerUrl);
    if (workerError !== undefined) {
      // As upstream: admission is reserved before the worker is constructed, then returned.
      let release;
      try {
        release = backend.acquireWorker?.();
      } catch (error) {
        return sandboxFailure(errorMessage(error));
      }
      release?.();
      return sandboxFailure(`Failed to start worker: ${workerError}`);
    }
    const signal = options.signal;
    let controller;
    let onAbort;
    if (signal !== undefined) {
      controller = call("codemode.sdk.controller");
      onAbort = () => call("codemode.sdk.controllerAbort", controller, abortMessage(signal));
      if (signal.aborted) onAbort();
      else signal.addEventListener("abort", onAbort, { once: true });
    }
    try {
      return withValue(await callAsync(
        "codemode.sdk.execute",
        this.#native,
        code,
        { timeoutMs: options.timeoutMs, store: serializeStore(options.store), timezone: backend.kind === "remote" ? undefined : process.env.TZ },
        controller,
      ));
    } finally {
      if (onAbort !== undefined) signal.removeEventListener("abort", onAbort);
    }
  }

  /** Run `code` deterministically, with the caller answering its calls through `options.onCall`. */
  start(code, options) {
    if (this.#closed) throw new Error("Sandbox is closed");
    if (!Number.isFinite(options.startedAt) || options.startedAt < 0) {
      throw new Error("startedAt must be a non-negative number of milliseconds since the epoch");
    }
    if (!Number.isInteger(options.seed) || options.seed < 0 || options.seed > 0xffffffff) {
      throw new Error("seed must be a 32-bit unsigned integer");
    }
    const tools = options.tools ?? [];
    if (new Set(tools.map((tool) => tool.name)).size !== tools.length) throw new Error("Tool names must be unique");
    const scriptNames = tools.map((tool) => tool.jsName ?? toCodemodeIdentifier(tool.name));
    if (new Set(scriptNames).size !== tools.length) throw new Error("Tool script names must be unique");
    const signal = options.signal;
    const events = (kind, payload) => {
      if (kind === "call") options.onCall(payload);
      else if (kind === "started") options.onStarted?.(payload);
      else options.onIdle?.(payload[0], payload[1]);
    };
    const run = call("codemode.sdk.start", this.#native, code, {
      startedAt: options.startedAt,
      seed: options.seed,
      tools: tools.map((tool) => ({ name: tool.name, jsName: tool.jsName, description: tool.description })),
      globals: (options.globals ?? []).map((global) => ({ name: global.name, spread: global.spread === true })),
      store: options.store,
      timeoutMs: options.timeoutMs,
      aborted: signal?.aborted === true,
      reason: abortMessage(signal),
      events,
    });
    let result = callAsync("codemode.sdk.result", run).then(withValue);
    if (signal !== undefined && !signal.aborted) {
      const onAbort = () => call("codemode.sdk.abort", run, abortMessage(signal));
      signal.addEventListener("abort", onAbort, { once: true });
      result = result.finally(() => signal.removeEventListener("abort", onAbort));
    }
    return {
      result,
      deliver: (seq, now, id, outcome) => {
        if (call("codemode.sdk.finished", run)) return;
        if (!Number.isFinite(now)) throw new Error(`The clock value of result ${seq} is not a finite number`);
        if (!Number.isInteger(seq) || seq < 1) throw new Error(`Result number ${seq} is not a positive integer used once`);
        if (!outcome.ok && outcome.properties !== undefined) {
          const { properties } = outcome;
          if (typeof properties !== "object" || properties === null || Array.isArray(properties)) {
            throw new Error(`The properties of result ${seq} are not an object`);
          }
        }
        call("codemode.sdk.deliver", run, seq, now, id, outcome);
      },
      get finished() {
        return call("codemode.sdk.finished", run);
      },
    };
  }

  /** Aborts in-flight runs (they resolve with `kind: "aborted"`) and rejects new ones. */
  async close() {
    this.#closed = true;
    await callAsync("codemode.sdk.sandboxClose", this.#native);
  }
}

/** Runs JavaScript in pi-codemode's native QuickJS VM on its own thread (upstream: QuickJS in a worker). */
export class CodemodeSandbox extends CodemodeSandboxBase {
  constructor(options = {}) {
    super(options, {
      kind: "thread",
      wasm: options.wasm ?? loadQuickJSWasm(),
      workerUrl: options.workerUrl,
      acquireWorker: options.acquireWorker === undefined ? undefined : () => options.acquireWorker(),
      onWorkerStarted:
        options.onWorkerStarted === undefined
          ? undefined
          : (exited) => options.onWorkerStarted(callAsync("codemode.sdk.workerExited", exited)),
    });
  }
}

/** Upstream `InlineCodemodeSandbox`'s default interrupt budget. */
export const DEFAULT_INTERRUPT_BUDGET = 100_000;

/** Runs the VM under an interrupt budget, the only limit on a script that computes without awaiting a tool. */
export class InlineCodemodeSandbox extends CodemodeSandboxBase {
  constructor(options) {
    super(options, {
      kind: "inline",
      wasm: options.wasm,
      interruptBudget: options.interruptBudget ?? DEFAULT_INTERRUPT_BUDGET,
    });
  }
}

/** Keeps the host role (tools, call records, deadline, abort) and runs the VM through `options.remote()`. */
export class RemoteCodemodeSandbox extends CodemodeSandboxBase {
  constructor(options) {
    super(options, {
      kind: "remote",
      interruptBudget: options.interruptBudget,
      remote: () => ref(options.remote()),
      remoteExecute: (remote, request, exchange) =>
        remote.execute(request, (messages) => callAsync("codemode.sdk.exchange", exchange, messages)),
    });
  }
}

/** The remote side: run one execution's VM, reporting through `exchange`. Never rejects. */
export async function serveCodemodeRemote(request, exchange, options = {}) {
  await callAsync("codemode.sdk.serve", request, exchange, {
    maxInterruptBudget: options.maxInterruptBudget,
    maxMemoryLimitBytes: options.maxMemoryLimitBytes,
    loopIterationLimit: options.loopIterationLimit,
    timezone: process.env.TZ,
  });
}

/** Resolves to an inert stand-in: the VM is native, so there is no QuickJS module to load. */
export function loadQuickJSWasm() {
  return Promise.resolve(QUICKJS_STANDIN);
}
