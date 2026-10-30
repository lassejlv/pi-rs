// pi-durable:internal/registry — the native registry (examples/support/registry_bridge.rs) behind
// `createRegistry`, and agents resolved from its snapshots. Port of
// scripts/stock/durable-registry-adapter.mjs with the built-in tasks always native.
import { nativeTransport, failure } from "pi-durable:internal/transport";
import { captureSettings, settingsRequest, hydrateSettings } from "pi-durable:internal/settings";
import { handle, objectFor, describeTask } from "pi-durable:internal/handles";
import { GenerationTask } from "pi-durable:internal/src/durable/harness/generation";
import { ToolTask } from "pi-durable:internal/src/durable/harness/tool";
import { CompactionTask } from "pi-durable:internal/src/durable/harness/compaction";

const ids = new WeakMap();
const snapshots = new Map();
const agents = new WeakMap();
const resolvedAgents = new Map();
const registries = new WeakMap();
const call = (method, args = {}) => nativeTransport.sync(`registry.${method}`, args);

function describeTool(tool) {
  return {
    id: handle(tool),
    name: tool.name,
    nativeBuiltin: tool.__piNativeBuiltin,
    codemodeOptions: tool.__piCodemodeOptions,
    callers: tool.callers,
    deferred: tool.deferred === true,
    declaration: { name: tool.name, description: tool.description, parameters: tool.parameters, constrainedSampling: tool.constrainedSampling },
    replay: tool.replay,
    executionMode: tool.executionMode,
    limits: { maxBytes: tool.outputLimits?.maxBytes ?? 51200, maxLines: tool.outputLimits?.maxLines ?? 2000, retain: tool.outputLimits?.retain ?? "head" },
    schema: tool.structuredOutputSchema,
  };
}

function describeSection(section) {
  return { id: handle(section), key: section.key, untagged: section.tag === false };
}

function describeExtension(extension) {
  return {
    id: handle(extension),
    name: extension.name,
    tools: (extension.tools ?? []).map(describeTool),
    sections: (extension.sections ?? []).map(describeSection),
    tasks: (extension.tasks ?? []).map(describeTask),
    hooks: (extension.hooks ?? []).map((hook) => ({
      task: hook.task,
      handlers: handle(hook.handlers),
      nativeTool: hook.task === "pi.tool",
      nativeGeneration: hook.task === "pi.generation",
      nativeCompaction: hook.task === "pi.compaction",
    })),
    wraps: (extension.wraps ?? []).map((wrap) => ({
      ...(Object.hasOwn(wrap, "tool") ? { tool: wrap.tool } : { section: wrap.section }),
      callback: handle(wrap.wrap),
    })),
  };
}

nativeTransport.register("registry.listener", (args) => {
  objectFor(args)();
  return null;
});
nativeTransport.register("registry.wrap", (args) => {
  const result = objectFor(args.callback)(objectFor(args.item));
  return args.kind === "tool" ? describeTool(result) : describeSection(result);
});
nativeTransport.register("registry.report", (args) => {
  objectFor(args.callback)(failure(args.error), args.extension);
  return null;
});

/** A registry of extensions; `snapshot()` gives an immutable view of what is installed. */
export function createRegistry() {
  const registry = call("create", { builtins: [GenerationTask, ToolTask, CompactionTask].map(describeTask) });
  const result = {
    snapshot: () => snapshot(call("snapshot", { registry })),
    install: (extension) => {
      call("install", { registry, extension: describeExtension(extension) });
    },
    uninstall: (extension) => {
      call("uninstall", { registry, name: extension.name });
    },
    subscribe: (listener) => {
      const subscription = call("subscribe", { registry, callback: handle(listener) });
      return () => {
        call("unsubscribe", { subscription });
      };
    },
  };
  registries.set(result, registry);
  return result;
}

/** The native id of a registry made by `createRegistry` (`undefined` for a custom reader). */
export function nativeRegistryId(registry) {
  return registries.get(registry);
}

/** The native id of a snapshot made by a native registry. */
export function nativeSnapshotId(value) {
  const id = ids.get(value);
  if (id === undefined) throw new Error("Custom reader must return a native registry snapshot");
  return id;
}

export function lookupNativeSnapshot(value) {
  return ids.get(value);
}

/** The snapshot object for a native snapshot id (one object per id). */
export function snapshot(id) {
  let held = snapshots.get(id);
  if (held) return held;
  const request = (method, args = {}) => call(method, { snapshot: id, ...args });
  held = {
    installed: () => request("installed").map((item) => objectFor(item)),
    extension: (name) => objectFor(request("extension", { name })),
    tools: () => request("tools").map((row) => ({ extension: objectFor(row.extension), tool: objectFor(row.tool) })),
    sections: () => request("sections").map((row) => ({ extension: objectFor(row.extension), section: objectFor(row.section) })),
    tasks: () => request("tasks").map((item) => objectFor(item)),
    task: (name) => objectFor(request("task", { name })),
  };
  snapshots.set(id, held);
  ids.set(held, id);
  return held;
}

/** `harness/agent.ts` `resolveSettings`: every field over its built-in default. */
export function resolveSettings(settings) {
  const { request, extensions } = captureSettings(settings);
  const result = hydrateSettings(call("settings", request));
  if (extensions !== undefined) result.extensions = extensions;
  return result;
}

/** `harness/agent.ts` `resolveAgent`: the agent of `state` over a registry snapshot. */
export function resolveAgent(state, snapshotValue, settings, report) {
  return hydrateAgent(call("resolve", { snapshot: ids.get(snapshotValue), state: state ?? null, ...settingsRequest(settings), report: handle(report) }));
}

/** The agent object for a resolved native agent (one object per resolution). */
export function hydrateAgent(result) {
  const { id, ...fields } = result;
  const held = resolvedAgents.get(id);
  if (held) return held;
  const agent = {
    ...fields,
    extensions: fields.extensions.map((item) => objectFor(item)),
    tools: fields.tools.map((item) => objectFor(item)),
    callable: fields.callable.map((item) => objectFor(item)),
    deferred: fields.deferred.map((item) => objectFor(item)),
    sections: fields.sections.map((section) => (Object.hasOwn(section, "id") ? objectFor(section.id) : { key: "instructions", render: () => section.instructions })),
  };
  agents.set(agent, id);
  resolvedAgents.set(id, agent);
  return agent;
}

/** `harness/agent.ts` `agentHooks`. */
export function agentHooks(agent, task) {
  return call("hooks", { agent: agents.get(agent), task }).map((hook) => ({ ...hook, handlers: objectFor(hook.handlers) }));
}

/** `harness/agent.ts` `sectionExtension`. */
export function sectionExtension(agent, key) {
  return call("sectionExtension", { agent: agents.get(agent), key });
}

/** `harness/agent.ts` `toolExtension`. */
export function toolExtension(agent, name) {
  return call("toolExtension", { agent: agents.get(agent), name });
}
