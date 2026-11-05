// packages/durable/src/codemode/codemode.ts: port of scripts/stock/durable-codemode-adapter.mjs.
// The hosted stock service owns durable code mode execution (the replay driver, journal, store,
// admission, model slots and output formatting); only the caller's sandbox and globals are
// called back through the transport. With the embedded @earendil-works/pi-codemode the sandbox's
// VM is native too.
import { defineDoc } from "pi-durable:internal/src/durable/documents";
import { nativeTransport } from "pi-durable:internal/transport";
import { handle, objectFor } from "pi-durable:internal/handles";
import { invokeCodemodeGlobal } from "pi-durable:internal/harness-service";
import { createModelTools, CODEMODE_INTERNAL_PREFIX } from "pi-durable:internal/src/durable/codemode/models";
import { codemodeSection, DEFAULT_CODEMODE_INLINE_BUDGET } from "pi-durable:internal/src/durable/codemode/describe";

const runs = new Map();

export const CODEMODE_MODULE_VERSION = 2;
export const CODEMODE_TOOL_NAME = "codemode";

/** The native checkpoint policy of a built-in document. */
const checkpoint = (kind, info) => nativeTransport.sync("codemode.docCheckpoint", { kind, deltas: info.deltasSinceBase });

export const CodemodeRunDoc = defineDoc({
  kind: "pi.codemode.run",
  version: 1,
  scope: "task",
  initial: () => ({}),
  checkpointWhen: (_value, _ops, info) => checkpoint("pi.codemode.run", info),
});
export const CodemodeJournalDoc = defineDoc({
  kind: "pi.codemode.journal",
  version: 1,
  scope: "task",
  initial: () => ({ entries: [] }),
  checkpointWhen: (_value, _ops, info) => checkpoint("pi.codemode.journal", info),
});
export const CodemodeStoreDoc = defineDoc({
  kind: "pi.codemode.store",
  version: 1,
  scope: "conversation",
  history: "rewindable",
  fork: "asOf",
  initial: () => ({ values: {} }),
  checkpointWhen: (_value, _ops, info) => checkpoint("pi.codemode.store", info),
});

function optionsFor(options) {
  return {
    sandbox: handle(options.sandbox),
    namespaces: options.namespaces,
    inlineBudget: options.inlineBudget,
    models: options.models,
    globals: (options.globals ?? []).map((global) => ({
      name: global.name,
      spread: global.spread === true,
      callback: handle(global),
    })),
  };
}

/** The code mode tool. Scripts call the conversation's callable tools as `tools.<name>(args)`. */
export function createCodemodeTool(options) {
  const wire = optionsFor(options);
  return {
    ...nativeTransport.sync("codemode.declaration", { options: wire }),
    __piNativeBuiltin: "codemode",
    __piCodemodeOptions: wire,
    callers: ["model"],
    replay: "safe",
    outputLimits: { maxBytes: 4194304, maxLines: 100000 },
    execute() {
      throw new Error("Source code-mode replay driver was bypassed incorrectly");
    },
  };
}

/** The `codemode` extension: the code mode tool, and the `codemode` prompt section listing the tools scripts can call. */
export function createCodemodeExtension(options) {
  return {
    name: "codemode",
    tools: [createCodemodeTool(options), ...(options.models === false ? [] : createModelTools())],
    sections: [
      {
        key: "codemode",
        render: (input) =>
          codemodeSection(
            // Deferred tools are found with `searchTools()`, as the coding agent leaves them out too.
            input.agent.callable.filter(
              (tool) => !tool.name.startsWith(CODEMODE_INTERNAL_PREFIX) && tool.deferred !== true,
            ),
            options.namespaces ?? [],
            options.inlineBudget ?? DEFAULT_CODEMODE_INLINE_BUDGET,
          ),
      },
    ],
  };
}

nativeTransport.register("codemode.vm.start", (args) => {
  const controller = new AbortController();
  if (args.aborted) controller.abort(new Error("Aborted"));
  const notify = (event, fields) => nativeTransport.notify({ type: "codemode.vmEvent", run: args.run, event, ...fields });
  const start = args.start;
  const run = objectFor(args.sandbox).start(args.code, {
    startedAt: start.startedAt,
    seed: start.seed,
    tools: start.tools.filter((tool) => tool.hidden !== true),
    globals: start.globals,
    store: start.store,
    signal: controller.signal,
    ...(args.timeoutMs == null ? {} : { timeoutMs: args.timeoutMs }),
    onStarted: (fingerprint) => notify("started", { fingerprint }),
    onCall: (call) => notify("call", { call }),
    onIdle: (seq, issued) => notify("idle", { seq, issued }),
  });
  runs.set(args.run, { run, controller });
  return null;
});
nativeTransport.register("codemode.vm.result", (id) => runs.get(id).run.result);
nativeTransport.register("codemode.vm.finished", (id) => runs.get(id)?.run.finished ?? true);
nativeTransport.register("codemode.vm.deliver", (args) => {
  runs.get(args.run)?.run.deliver(args.seq, args.at, args.call, args.outcome);
  return null;
});
nativeTransport.event("codemode.vm.abort", (args) => runs.get(args.run)?.controller.abort(new Error("Code-mode invocation ended")));
nativeTransport.event("codemode.vm.release", (args) => runs.delete(args.run));
nativeTransport.register("codemode.global", invokeCodemodeGlobal);
